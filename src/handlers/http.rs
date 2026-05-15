// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

use crate::db;
use crate::embeddings::EmbedStatus;
use crate::models::{
    CreateMemory, ForgetQuery, LinkBody, ListQuery, Memory, MemoryLink, RecallBody, RecallQuery,
    RegisterAgentBody, SearchQuery, Tier, UpdateMemory,
};
use crate::profile::Family;
use crate::validate;

#[cfg(feature = "sal")]
use super::StorageBackend;
use super::fanout_or_503;
#[cfg(feature = "sal")]
use super::store_err_to_response;
use super::{AppState, JsonOrBadRequest};
use super::{BULK_FANOUT_CONCURRENCY, MAX_BULK_SIZE};

/// v0.7.0 L5 — minimum content length (chars) below which the HTTP
/// `create_memory` handler skips the `auto_tag` autonomy hook. Mirrors
/// the constant the MCP `handle_store` path uses (`AUTONOMY_MIN_CONTENT_LEN`
/// at `src/mcp.rs:1405`) so a memory that's too short to be meaningfully
/// tagged doesn't burn a 30s Ollama round-trip on each store.
const AUTO_TAG_MIN_CONTENT_LEN: usize = 50;
/// v0.7.0 L5 — maximum number of auto-generated tags merged into the
/// memory. Mirrors `mcp.rs:1827-1828` so postgres + sqlite + MCP all
/// converge on the same on-disk shape.
const AUTO_TAG_MAX_TAGS: usize = 8;

/// v0.7.0 fold-A2A1.6 (#700, S16/S49) — `app.store.get` with bounded
/// retry on [`crate::store::StoreError::NotFound`].
///
/// Why this exists: on a postgres-backed daemon a freshly-stored row
/// can briefly return NotFound from the SAL `get` while WAL flush
/// settles or the read query hits a still-replicating standby. The
/// 22-failure A2A triage (memory `9ffaa55d`) classified this as
/// Bucket-A: the row exists, the promote handler just races the
/// visibility window. Returning a one-shot 404 surfaces a flake to
/// the operator even though a 5 ms retry would have caught the
/// (eventually-consistent) row.
///
/// Retry budget: 5 + 10 + 15 + 20 ms = 50 ms wall clock, evenly
/// dwarfed by the 2 s daemon p99 SLO. Any other StoreError class
/// (e.g. backend down, integrity failure) returns immediately
/// without retry — those are not visibility-race symptoms.
#[cfg(feature = "sal")]
async fn get_with_visibility_retry(
    store: &dyn crate::store::MemoryStore,
    ctx: &crate::store::CallerContext,
    id: &str,
) -> crate::store::StoreResult<Memory> {
    let mut attempt: u32 = 0;
    loop {
        match store.get(ctx, id).await {
            Ok(m) => return Ok(m),
            Err(crate::store::StoreError::NotFound { .. }) if attempt < 4 => {
                let backoff_ms = u64::from(5 * (attempt + 1));
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// v0.7.0 L5 — fire the LLM `auto_tag` hook for a freshly-built memory.
///
/// Returns the list of LLM-generated tags (capped at
/// [`AUTO_TAG_MAX_TAGS`]) when every gate is satisfied:
///   - The daemon's configured [`crate::config::FeatureTier`] declares
///     an `llm_model` (the smart / autonomous tier capability —
///     `tier_config.llm_model.is_some()`).
///   - The operator did NOT pre-populate `tags` on the request
///     (auto-tag never overwrites operator-supplied tags).
///   - The content is at least [`AUTO_TAG_MIN_CONTENT_LEN`] chars
///     (too-short content has no useful taggable signal).
///   - The namespace is not internal / system (starts with `_`) —
///     matches MCP's `handle_store` skip at `src/mcp.rs:1818`.
///   - An LLM client is wired on `AppState` and the Ollama endpoint
///     is reachable.
///
/// On any LLM error the function returns `Vec::new()` and logs a
/// `tracing::warn!` — auto_tag is a soft hook and a failure must not
/// fail the store (mirrors MCP `handle_store` at `src/mcp.rs:1830`).
///
/// The blocking Ollama call is wrapped in `tokio::task::spawn_blocking`
/// to keep the async runtime healthy under load — matches the embedder
/// pattern at `src/daemon_runtime.rs:1182`.
pub(crate) async fn maybe_auto_tag(
    app: &AppState,
    title: &str,
    content: &str,
    operator_tags: &[String],
    namespace: &str,
) -> Vec<String> {
    if !operator_tags.is_empty() {
        return Vec::new();
    }
    if content.len() < AUTO_TAG_MIN_CONTENT_LEN {
        return Vec::new();
    }
    if namespace.starts_with('_') {
        return Vec::new();
    }
    if app.tier_config.llm_model.is_none() {
        return Vec::new();
    }
    let llm_arc = app.llm.clone();
    if llm_arc.is_none() {
        return Vec::new();
    }
    // v0.7.0 L15 — when the operator has configured a dedicated tag
    // model (`auto_tag_model = "..."` in config.toml), pass it through
    // so the call hits the fast structured-output model instead of the
    // reasoning-tier llm_model. Closes the NHI-D-autotag-empty finding
    // where Gemma 4 thinking-mode would generate 400+ tokens for a
    // 5-tag list and hit the 30s tail latency.
    let auto_tag_model = app.auto_tag_model.as_ref().clone();
    let title_owned = title.to_string();
    let content_owned = content.to_string();
    let llm_timeout = app.llm_call_timeout;
    // H8 (v0.7.0 round-2) — bound the Ollama call by the configured
    // per-LLM-call timeout (default 30s). On timeout we degrade to the
    // LLM-absent fallback (empty tags) — same shape the keyword /
    // semantic tiers already return when no LLM is wired (L5/L7).
    let join = tokio::time::timeout(
        llm_timeout,
        tokio::task::spawn_blocking(move || {
            let llm = match llm_arc.as_ref() {
                Some(c) => c,
                None => return Ok(Vec::new()),
            };
            llm.auto_tag(&title_owned, &content_owned, auto_tag_model.as_deref())
        }),
    )
    .await;
    match join {
        Ok(Ok(Ok(tags))) => tags.into_iter().take(AUTO_TAG_MAX_TAGS).collect(),
        Ok(Ok(Err(e))) => {
            tracing::warn!("L5: auto_tag hook failed: {e}");
            Vec::new()
        }
        Ok(Err(e)) => {
            tracing::warn!("L5: auto_tag spawn_blocking join failed: {e}");
            Vec::new()
        }
        Err(_) => {
            tracing::warn!(
                "H8: LLM call (auto_tag) exceeded {}s timeout — falling back to no tags",
                llm_timeout.as_secs()
            );
            Vec::new()
        }
    }
}

/// v0.7.0 (issue #519) — same-namespace conflict probe fired during
/// `create_memory`. Mirrors the MCP `handle_store` autonomy hook's
/// `detect_contradiction` loop (`src/mcp.rs:1830-1850`) but lives on the
/// HTTP path so a smart/autonomous-tier daemon surfaces conflicts in the
/// 201 response without requiring the caller to follow up with a manual
/// `memory_detect_contradiction`.
///
/// Gating layers (any false → returns empty):
///   1. `request_override`:
///       `Some(true)`  → force-on regardless of `autonomous_hooks`
///       `Some(false)` → force-off regardless of `autonomous_hooks`
///       `None`        → defer to `autonomous_hooks`
///   2. tier — only smart/autonomous (`tier_config.llm_model.is_some()`)
///   3. LLM client wired (`app.llm`)
///   4. content ≥ 50 chars (matches `AUTO_TAG_MIN_CONTENT_LEN`)
///   5. namespace not `_*` (internal)
///
/// The probe is best-effort: any LLM error or timeout returns an empty
/// vec — never fails the parent store. Bounded by the H8 per-LLM-call
/// timeout (default 30s) the same way `maybe_auto_tag` is.
//
// v0.7.0 (round-2) — call sites for this helper are still being
// wired in the create_memory hot path; the function is staged for
// the next round so we silence the dead-code warning rather than
// rip out the implementation. Tracked in issue #519.
#[allow(dead_code)]
async fn maybe_detect_conflicts(
    app: &AppState,
    title: &str,
    content: &str,
    namespace: &str,
    request_override: Option<bool>,
) -> Vec<ConflictReport> {
    let enabled = match request_override {
        Some(b) => b,
        None => app.autonomous_hooks,
    };
    if !enabled
        || content.len() < AUTO_TAG_MIN_CONTENT_LEN
        || namespace.starts_with('_')
        || app.tier_config.llm_model.is_none()
    {
        return Vec::new();
    }
    let llm_arc = app.llm.clone();
    if llm_arc.is_none() {
        return Vec::new();
    }

    // Pull same-namespace candidates that could contradict the new memory.
    // Cap at 8 to bound LLM cost (8 × 30s worst-case = 4 min if every probe
    // tail-times-out; in practice most return in 0.7s on gemma3:4b).
    let candidates: Vec<(String, String, String)> =
        match fetch_namespace_candidates(app, namespace, title, 8).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("L?: maybe_detect_conflicts candidate fetch failed: {e}");
                return Vec::new();
            }
        };

    let llm_timeout = app.llm_call_timeout;
    let new_content = content.to_string();
    let mut out: Vec<ConflictReport> = Vec::new();
    for (cand_id, cand_title, cand_content) in candidates {
        let llm_arc_cl = llm_arc.clone();
        let cand_content_cl = cand_content.clone();
        let new_content_cl = new_content.clone();
        let join = tokio::time::timeout(
            llm_timeout,
            tokio::task::spawn_blocking(move || {
                let llm = match llm_arc_cl.as_ref() {
                    Some(c) => c,
                    None => return Ok(false),
                };
                llm.detect_contradiction(&new_content_cl, &cand_content_cl)
            }),
        )
        .await;
        match join {
            Ok(Ok(Ok(true))) => out.push(ConflictReport {
                id: cand_id,
                title: cand_title,
                suggested_merge: None,
            }),
            Ok(Ok(Ok(false))) => {}
            Ok(Ok(Err(e))) => tracing::warn!("detect_contradiction LLM error for {cand_id}: {e}"),
            Ok(Err(e)) => tracing::warn!("detect_contradiction join error for {cand_id}: {e}"),
            Err(_) => tracing::warn!(
                "H8: LLM call (detect_contradiction) exceeded {}s timeout for {cand_id} — skipping",
                llm_timeout.as_secs()
            ),
        }
    }
    out
}

/// Fetch up to `limit` same-namespace memories whose title is NOT byte-equal
/// to the incoming title (we want potentially-contradictory siblings, not
/// the row that an UPSERT would target). Routes through the active storage
/// backend.
//
// v0.7.0 (round-2) — only used by the staged-in `maybe_detect_conflicts`
// helper above; silence dead_code under pedantic until #519 wires the
// call site through create_memory.
#[allow(dead_code)]
async fn fetch_namespace_candidates(
    app: &AppState,
    namespace: &str,
    new_title: &str,
    limit: usize,
) -> Result<Vec<(String, String, String)>, String> {
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("ai:http");
        let filter = crate::store::Filter {
            namespace: Some(namespace.to_string()),
            limit: limit + 1,
            ..crate::store::Filter::default()
        };
        let mems = app
            .store
            .list(&ctx, &filter)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(mems
            .into_iter()
            .filter(|m| m.title != new_title)
            .take(limit)
            .map(|m| (m.id, m.title, m.content))
            .collect());
    }
    let lock = app.db.lock().await;
    let mems = db::list(
        &lock.0,
        Some(namespace),
        None,
        limit + 1,
        0,
        None,
        None,
        None,
        None,
        None,
    )
    .map_err(|e| e.to_string())?;
    Ok(mems
        .into_iter()
        .filter(|m| m.title != new_title)
        .take(limit)
        .map(|m| (m.id, m.title, m.content))
        .collect())
}

/// v0.7.0 (issue #519) — a single same-namespace memory the LLM flagged as
/// contradictory with the incoming row. Surfaced in the create_memory
/// response under `conflicts: [...]` when proactive detection ran.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConflictReport {
    pub id: String,
    pub title: String,
    /// LLM-proposed merged content. Future expansion (#519 §"suggested
    /// merge"). For v0.7.0 ship-scope this is left `None`; the caller can
    /// follow up with `memory_consolidate` using the reported ids. The
    /// field reserves the wire shape so callers can branch on it now.
    pub suggested_merge: Option<String>,
}

#[allow(clippy::too_many_lines)]
pub async fn create_memory(
    State(app): State<AppState>,
    headers: HeaderMap,
    JsonOrBadRequest(body): JsonOrBadRequest<CreateMemory>,
) -> impl IntoResponse {
    let state = app.db.clone();
    if let Err(e) = validate::validate_create(&body) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    // Resolve agent_id via the HTTP precedence chain:
    //   1. top-level `body.agent_id`
    //   2. embedded `body.metadata.agent_id` (caller's NHI claim — load-bearing
    //      for federation receivers and clients that prefer the metadata-only
    //      shape; mirrors the MCP precedence at `src/mcp.rs:1514-1516` and the
    //      CLAUDE.md §Agent Identity (NHI) contract)
    //   3. `X-Agent-Id` request header
    //   4. per-request anonymous fallback
    //
    // L11 (NHI-D-fed-agentid-mutation): prior to this, step 2 was missing.
    // A federated peer that resent a memory through `POST /api/v1/memories`
    // (or a client that only stamped `metadata.agent_id`) would have its claim
    // silently rewritten to the per-request anonymous id by the
    // unconditional `obj.insert("agent_id", ...)` below, breaking the
    // immutable-provenance contract documented in CLAUDE.md and enforced at
    // the SQL layer by `db::insert_if_newer` / `apply_remote_memory`.
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let metadata_agent_id = body
        .metadata
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let explicit_agent_id = body.agent_id.as_deref().or(metadata_agent_id.as_deref());
    let agent_id = match crate::identity::resolve_http_agent_id(explicit_agent_id, header_agent_id)
    {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid agent_id: {e}")})),
            )
                .into_response();
        }
    };
    let mut metadata = body.metadata;
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String(agent_id.clone()),
        );
    }
    // #151 scope: validate + merge into metadata if supplied at the top level
    // (inline metadata.scope still works; top-level is a shortcut)
    if let Some(ref s) = body.scope {
        if let Err(e) = validate::validate_scope(s) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("scope".to_string(), serde_json::Value::String(s.clone()));
        }
    }

    // v0.7.0 Wave-3 — Postgres-backed daemons take the SAL trait
    // dispatch path. The trait's `store` accepts a fully-formed
    // `Memory` value; the legacy SQLite path below also assembles
    // a canonical `Memory` row but with substantially more
    // ceremony (federation fanout, embedder integration, conflict
    // policy enforcement, governance hooks). The Postgres branch
    // takes the simpler shape — the upstream layers (governance,
    // federation, audit) are still SQLite-bound today and lighting
    // them up on Postgres is a follow-on wave. Until then the
    // postgres-backed daemon ships a clean store-and-return path
    // that's portable across both adapters.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let now = Utc::now();
        // v0.7.0 L5 — fire the LLM `auto_tag` hook before assembling the
        // canonical `Memory` row so the postgres `tags` column lands
        // populated with LLM suggestions on the FIRST insert (no
        // post-insert metadata update needed, unlike the MCP path's
        // best-effort `db::update` at `src/mcp.rs:1864`). The hook is
        // gated to autonomous/smart tiers (`tier_config.llm_model.is_some()`),
        // skipped when operator supplied tags, and silently no-ops when
        // Ollama is unreachable. See `maybe_auto_tag` for the full gate list.
        let auto_tags = maybe_auto_tag(
            &app,
            &body.title,
            &body.content,
            &body.tags,
            &body.namespace,
        )
        .await;
        let mut final_tags = body.tags.clone();
        for t in &auto_tags {
            if !final_tags.iter().any(|existing| existing == t) {
                final_tags.push(t.clone());
            }
        }
        let mem = Memory {
            id: Uuid::new_v4().to_string(),
            tier: body.tier.clone(),
            namespace: body.namespace.clone(),
            title: body.title.clone(),
            content: body.content.clone(),
            tags: final_tags,
            priority: body.priority,
            confidence: body.confidence,
            source: body.source.clone(),
            access_count: 0,
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
            last_accessed_at: None,
            expires_at: body.expires_at.clone(),
            metadata,
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        };
        let ctx = crate::store::CallerContext::for_agent(agent_id.clone());

        // v0.7.0 Wave-3 Continuation 5 (S18 / semantic recall) —
        // compute the embedding before the SAL store call so the
        // postgres `embedding` column lands populated. Without this,
        // `recall_hybrid` filters every row out via
        // `WHERE embedding IS NOT NULL` and semantic queries return 0
        // results. Mirrors the SQLite path (handlers.rs ~L1093) where
        // the embedding is generated outside the DB lock.
        let embedding_text = format!("{} {}", mem.title, mem.content);
        let embedding: Option<Vec<f32>> = match app.embedder.as_ref().as_ref() {
            None => None,
            Some(emb) => emb.embed(&embedding_text).ok(),
        };

        // v0.7.0 Wave-3 Continuation 3 (Phase 20) — governance walk on
        // writes. The postgres branch now enforces the same inheritance
        // chain + approver_type policy as the sqlite path. When the
        // walk lands on an `Approve`-level rule the action is queued in
        // `pending_actions` and we return 202 Accepted with the pending
        // id — the caller must then drive the consensus path through
        // `POST /pending/{id}/approve`.
        let payload_for_pending = serde_json::to_value(&mem).unwrap_or_else(|_| json!({}));
        match app
            .store
            .enforce_governance_action(
                crate::store::GovernedAction::Store,
                &mem.namespace,
                &agent_id,
                None,
                None,
                &payload_for_pending,
            )
            .await
        {
            Ok(crate::models::GovernanceDecision::Allow) => {}
            Ok(crate::models::GovernanceDecision::Deny(reason)) => {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": format!("denied: {reason}")})),
                )
                    .into_response();
            }
            Ok(crate::models::GovernanceDecision::Pending(pending_id)) => {
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({
                        "status": "pending",
                        "pending_id": pending_id,
                        "namespace": mem.namespace,
                        "storage_backend": "postgres",
                    })),
                )
                    .into_response();
            }
            Err(e) => return store_err_to_response(e),
        }

        return match app
            .store
            .store_with_embedding(&ctx, &mem, embedding.as_deref())
            .await
        {
            Ok(id) => {
                // v0.7.0 Wave-3 Continuation 2 Phase 9 — audit emit on
                // postgres write. The audit module is file-based with no
                // SQLite coupling, so the emit chains through the same
                // hash + sequence ladder as a sqlite-backed write. The
                // F2 fix (cross-restart sequence persistence) lights up
                // for postgres-backed daemons through this path.
                if crate::audit::is_enabled() {
                    let scope = mem
                        .metadata
                        .get("scope")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    crate::audit::emit(crate::audit::EventBuilder::new(
                        crate::audit::AuditAction::Store,
                        crate::audit::actor(agent_id.clone(), "http_body", scope.clone()),
                        crate::audit::target_memory(
                            id.clone(),
                            mem.namespace.clone(),
                            Some(mem.title.clone()),
                            Some(mem.tier.to_string()),
                            scope,
                        ),
                    ));
                }
                // F-A2A1.6 (#700, S18) — postgres-branch federation
                // fanout on `create_memory`. Mirrors the sqlite path
                // at handlers/http.rs:937-966 so peers receive the
                // freshly-stored row via `sync_push_via_store`, which
                // re-embeds on arrival (see
                // handlers/federation_receive.rs:387-395). Without
                // this, a recall against a federated reader peer
                // returns 0 rows even after the row settles on the
                // leader — A2A scenario S18 reproduced this with
                // `list=[1,1]` (peer sees the row in keyword list)
                // but `/api/v1/recall` empty top-K (no embedding on
                // peer's `embedding` column → pgvector cosine filter
                // strips every row).
                //
                // Failure handling: fanout failures surface as 503
                // with `Retry-After: 2` mirroring sqlite. The local
                // commit has already landed; per ADR-0001 the
                // substrate does NOT roll back on quorum failure.
                if let Some(fed) = app.federation.as_ref() {
                    let mut mem_echo = mem.clone();
                    mem_echo.id = id.clone();
                    match crate::federation::broadcast_store_quorum(fed, &mem_echo).await {
                        Ok(tracker) => {
                            if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                                let payload =
                                    crate::federation::QuorumNotMetPayload::from_err(&err);
                                return (
                                    StatusCode::SERVICE_UNAVAILABLE,
                                    [("Retry-After", "2")],
                                    Json(serde_json::to_value(&payload).unwrap_or_default()),
                                )
                                    .into_response();
                            }
                        }
                        Err(err) => {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("Retry-After", "2")],
                                Json(serde_json::to_value(&payload).unwrap_or_default()),
                            )
                                .into_response();
                        }
                    }
                }
                let mut payload = serde_json::to_value(&mem).unwrap_or_else(|_| json!({}));
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert("id".to_string(), serde_json::Value::String(id));
                    // v0.7.0 L5 — echo LLM-generated tags as a dedicated
                    // `auto_tags` field, matching MCP `handle_store`'s
                    // response shape at `src/mcp.rs:1909-1911`. Operator-
                    // supplied tags continue to land in the regular
                    // `tags` array; `auto_tags` lets callers detect
                    // which tags were LLM-derived without diffing.
                    if !auto_tags.is_empty() {
                        obj.insert("auto_tags".to_string(), json!(auto_tags));
                    }
                }
                (StatusCode::CREATED, Json(payload)).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    // v0.7.0 L5 — fire the LLM `auto_tag` autonomy hook BEFORE the
    // embedding pass + DB lock. Both LLM and embedder calls are
    // network/CPU work that must not happen under the single shared
    // `Mutex<Connection>` on a multi-agent daemon. Gated to
    // autonomous/smart tiers (`tier_config.llm_model.is_some()`) and
    // skipped when operator supplied tags — see `maybe_auto_tag` for
    // the full gate list. Mirrors MCP `handle_store`'s gate at
    // `src/mcp.rs:1812-1822`.
    let auto_tags = maybe_auto_tag(
        &app,
        &body.title,
        &body.content,
        &body.tags,
        &body.namespace,
    )
    .await;

    // Issue #219: generate the embedding BEFORE taking the DB lock. Embedding
    // (MiniLM ONNX / nomic via Ollama) is 10-200ms of work we do not want
    // holding the single `Mutex<Connection>` on a multi-agent daemon.
    //
    // v0.7.0 Round-2 F10 — call α's `Embedder::embed_with_status` so we
    // capture the success/skip/fail outcome alongside the vector. The
    // success-path response stays silent on `Indexed`; non-`Indexed`
    // outcomes are surfaced as `embed_status` on the response body so
    // the caller can tell semantic recall will miss this row until a
    // re-index. Keyword-only deployments (embedder=None) report
    // `Indexed` so the response shape is unchanged on nodes where the
    // semantic layer is intentionally absent.
    let embedding_text = format!("{} {}", body.title, body.content);
    let (embedding, embed_status): (Option<Vec<f32>>, EmbedStatus) =
        match app.embedder.as_ref().as_ref() {
            None => (None, EmbedStatus::Indexed),
            Some(emb) => emb.embed_with_status(&embedding_text),
        };

    // v0.6.3.1 P2 (G6) — resolve `on_conflict` policy. HTTP defaults to
    // 'error' (no legacy v1 backward-compat to honor); callers that want
    // the v0.6.3 silent-merge behaviour must pass on_conflict='merge'.
    let on_conflict_mode = body.on_conflict.as_deref().unwrap_or("error");
    if !matches!(on_conflict_mode, "error" | "merge" | "version") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "invalid on_conflict '{on_conflict_mode}' (expected error|merge|version)"
                )
            })),
        )
            .into_response();
    }

    let now = Utc::now();
    let lock = state.lock().await;
    let expires_at = body.expires_at.or_else(|| {
        body.ttl_secs
            .or(lock.2.ttl_for_tier(&body.tier))
            .map(|s| (now + Duration::seconds(s)).to_rfc3339())
    });

    // v0.6.3.1 P2 (G6) — apply the conflict policy before building the
    // canonical row. Mirror MCP handle_store: 'error' returns 409 with a
    // typed payload; 'version' rewrites the title to a free suffix;
    // 'merge' falls through to db::insert which keeps the legacy
    // INSERT...ON CONFLICT upsert.
    let resolved_title = match on_conflict_mode {
        "error" => match db::find_by_title_namespace(&lock.0, &body.title, &body.namespace) {
            Ok(Some(existing_id)) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "code": "CONFLICT",
                        "error": format!(
                            "memory with title '{}' already exists in namespace '{}'",
                            body.title, body.namespace
                        ),
                        "existing_id": existing_id,
                    })),
                )
                    .into_response();
            }
            Ok(None) => body.title.clone(),
            Err(e) => {
                tracing::error!("on_conflict lookup failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "conflict check failed"})),
                )
                    .into_response();
            }
        },
        "version" => match db::next_versioned_title(&lock.0, &body.title, &body.namespace) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("on_conflict=version failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "could not pick a versioned title"})),
                )
                    .into_response();
            }
        },
        _ => body.title.clone(),
    };

    // v0.7.0 L5 — merge LLM-derived `auto_tags` with operator-supplied
    // `body.tags`. Operator tags lead; auto-tag entries that duplicate
    // an existing operator tag are dropped to avoid double-counting on
    // FTS5 weighting downstream. `auto_tags` will be `Vec::new()` when
    // the LLM hook was skipped (operator supplied tags, content too
    // short, internal namespace, tier has no llm_model, Ollama
    // unreachable) so the union is a no-op on the keyword/semantic path.
    let mut merged_tags = body.tags.clone();
    for t in &auto_tags {
        if !merged_tags.iter().any(|existing| existing == t) {
            merged_tags.push(t.clone());
        }
    }

    let mem = Memory {
        id: Uuid::new_v4().to_string(),
        tier: body.tier,
        namespace: body.namespace,
        title: resolved_title,
        content: body.content,
        tags: merged_tags,
        priority: body.priority.clamp(1, 10),
        confidence: body.confidence.clamp(0.0, 1.0),
        source: body.source,
        access_count: 0,
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
        last_accessed_at: None,
        expires_at,
        metadata,
        reflection_depth: 0,
        memory_kind: crate::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };

    // Task 1.9: governance enforcement (store-side).
    {
        use crate::models::{GovernanceDecision, GovernedAction};
        let agent_for_gov = mem
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let payload = serde_json::to_value(&mem).unwrap_or_default();
        match db::enforce_governance(
            &lock.0,
            GovernedAction::Store,
            &mem.namespace,
            &agent_for_gov,
            None,
            None,
            &payload,
        ) {
            Ok(GovernanceDecision::Allow) => {}
            Ok(GovernanceDecision::Deny(reason)) => {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": format!("store denied by governance: {reason}")})),
                )
                    .into_response();
            }
            Ok(GovernanceDecision::Pending(pending_id)) => {
                // v0.6.2 (S34): fan out the new pending row so peers can
                // approve / reject / list it. Load the canonical row we
                // just inserted and broadcast before responding.
                let pending_row = db::get_pending_action(&lock.0, &pending_id).ok().flatten();
                // v0.7.0 K4 — fire the `approval_requested` webhook
                // event through the existing subscription dispatcher so
                // K10's Approval API HTTP+SSE handler picks it up. Done
                // BEFORE the lock drops so the subscriber list query has
                // a connection; the actual HTTP POSTs spawn detached
                // threads (fire-and-forget). Best-effort: a dispatch
                // failure must not roll back the pending row.
                crate::subscriptions::dispatch_approval_requested(&lock.0, &pending_id, &lock.1);
                let namespace = mem.namespace.clone();
                drop(lock);
                if let (Some(pa), Some(fed)) = (pending_row.as_ref(), app.federation.as_ref()) {
                    match crate::federation::broadcast_pending_quorum(fed, pa).await {
                        Ok(tracker) => {
                            if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                                let payload =
                                    crate::federation::QuorumNotMetPayload::from_err(&err);
                                return (
                                    StatusCode::SERVICE_UNAVAILABLE,
                                    [("Retry-After", "2")],
                                    Json(serde_json::to_value(&payload).unwrap_or_default()),
                                )
                                    .into_response();
                            }
                        }
                        Err(err) => {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("Retry-After", "2")],
                                Json(serde_json::to_value(&payload).unwrap_or_default()),
                            )
                                .into_response();
                        }
                    }
                }
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({
                        "status": "pending",
                        "pending_id": pending_id,
                        "reason": "governance requires approval",
                        "action": "store",
                        "namespace": namespace,
                    })),
                )
                    .into_response();
            }
            Err(e) => {
                tracing::error!("governance error: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "governance check failed"})),
                )
                    .into_response();
            }
        }
    }

    // Check for contradictions
    let contradictions =
        db::find_contradictions(&lock.0, &mem.title, &mem.namespace).unwrap_or_default();
    let contradiction_ids: Vec<String> = contradictions
        .iter()
        .filter(|c| c.id != mem.id)
        .map(|c| c.id.clone())
        .collect();

    // v0.7.0 Round-2 F7 — per-agent quota gate. Round-1 evidence: 500
    // HTTP stores from a single agent_id incremented zero rows in
    // `agent_quotas` while the same agent's MCP-side stamp incremented
    // correctly. The MCP store path (src/mcp.rs:1691) calls
    // `quotas::check_and_record` ahead of `db::insert` and refunds on
    // insert failure; mirror that here so the HTTP path is no longer a
    // quota-bypass surface. Bytes counted = (title + content +
    // serialized metadata) — same shape the MCP path uses so cross-
    // path totals stay coherent.
    let quota_agent_id = mem
        .metadata
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let raw_payload_bytes = mem.title.len()
        + mem.content.len()
        + serde_json::to_string(&mem.metadata)
            .map(|s| s.len())
            .unwrap_or(0);
    let payload_bytes = match i64::try_from(raw_payload_bytes) {
        Ok(v) => v,
        Err(_) => {
            // M10 (v0.7.0 round-2) — saturating cast surfaced. usize
            // overflowed i64 (rare; would require >9 EiB of metadata
            // on a 64-bit host). Operators need to see this in logs
            // because the quota row gets clamped to the maximum,
            // which makes that single store look unbounded from the
            // dashboard's perspective until they investigate.
            tracing::warn!(
                agent_id = %quota_agent_id,
                raw_bytes = raw_payload_bytes,
                "quota byte-count saturated at i64::MAX for agent={}; \
                 metadata may be excessively large",
                if quota_agent_id.is_empty() {
                    "<anonymous>"
                } else {
                    quota_agent_id.as_str()
                }
            );
            i64::MAX
        }
    };
    let quota_op = crate::quotas::QuotaOp::Memory {
        bytes: payload_bytes,
    };
    if !quota_agent_id.is_empty() {
        if let Err(e) = crate::quotas::check_and_record(&lock.0, &quota_agent_id, quota_op) {
            // Map QuotaCheckError to the same wire shape the rest of
            // the daemon uses for quota breaches: 429 with a
            // `code: "QUOTA_EXCEEDED"` envelope so callers can switch
            // on the limit name. Substrate errors bubble up as 500
            // because the row was never written.
            return match e {
                crate::quotas::QuotaCheckError::Quota(qe) => (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({
                        "code": "QUOTA_EXCEEDED",
                        "error": qe.to_string(),
                        "limit": qe.limit.as_str(),
                        "current": qe.current,
                        "max": qe.max,
                        "agent_id": qe.agent_id,
                    })),
                )
                    .into_response(),
                crate::quotas::QuotaCheckError::Sql(se) => {
                    tracing::error!("quota substrate error: {se}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "quota check failed"})),
                    )
                        .into_response()
                }
            };
        }
    }

    match db::insert(&lock.0, &mem) {
        Ok(actual_id) => {
            // Issue #219: persist the embedding and warm the HNSW index so
            // semantic recall can find this memory. Previously the HTTP path
            // stored the row but never called `set_embedding`, silently
            // excluding every HTTP-authored memory from semantic search.
            if let Some(ref vec) = embedding
                && let Err(e) = db::set_embedding(&lock.0, &actual_id, vec)
            {
                tracing::warn!("failed to store embedding for {actual_id}: {e}");
            }
            // Drop the DB lock before taking the vector index lock.
            drop(lock);
            if let Some(vec) = embedding {
                let mut idx_lock = app.vector_index.lock().await;
                if let Some(idx) = idx_lock.as_mut() {
                    idx.insert(actual_id.clone(), vec);
                }
            }
            // #196: echo the resolved agent_id so callers don't need a follow-up get.
            let resolved_agent_id = mem
                .metadata
                .get("agent_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // PR-5 (issue #487): security audit trail for HTTP store.
            crate::audit::emit(crate::audit::EventBuilder::new(
                crate::audit::AuditAction::Store,
                crate::audit::actor(
                    resolved_agent_id.clone().unwrap_or_default(),
                    "http_body",
                    mem.metadata
                        .get("scope")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                ),
                crate::audit::target_memory(
                    actual_id.clone(),
                    mem.namespace.clone(),
                    Some(mem.title.clone()),
                    Some(mem.tier.to_string()),
                    mem.metadata
                        .get("scope")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                ),
            ));
            let mut response = json!({
                "id": actual_id,
                "tier": mem.tier,
                "namespace": mem.namespace,
                "title": mem.title,
                "agent_id": resolved_agent_id,
            });
            if !contradiction_ids.is_empty() {
                response["potential_contradictions"] = json!(contradiction_ids);
            }
            // v0.7.0 L5 — echo LLM-generated tags as a dedicated
            // `auto_tags` field, matching MCP `handle_store`'s
            // response at `src/mcp.rs:1909-1911`. Operator tags continue
            // to round-trip through `tags`; clients that want to know
            // which tags were LLM-derived inspect `auto_tags`.
            if !auto_tags.is_empty() {
                response["auto_tags"] = json!(auto_tags);
            }
            // v0.7.0 Round-2 F10 — surface embed_status to the caller
            // when α's `embed_with_status` reported anything other than
            // `Indexed` (skipped: oversized content / empty body, or
            // failed: embedder timeout, ollama unreachable, model load
            // failure, …). Indexed is intentionally NOT surfaced so
            // the existing response shape is unchanged for the common
            // case; the skip/fail signal is a positive presence marker
            // rather than a free-form enum every client has to switch
            // on.
            if embed_status.is_degraded() {
                response["embed_status"] = json!(embed_status.as_str());
                let reason = embed_status.reason();
                if !reason.is_empty() {
                    response["embed_status_reason"] = json!(reason);
                }
            }
            // v0.7 federation: fan out to peers when --quorum-writes is
            // configured. The local commit already landed; if quorum
            // is not met we return 503 but we do NOT roll back the
            // local write — per ADR-0001, caller sees
            // BackendUnavailable{quorum} and the sync-daemon's
            // eventual-consistency loop catches straggling peers up.
            if let Some(fed) = app.federation.as_ref() {
                let mut mem_echo = mem.clone();
                mem_echo.id = actual_id.clone();
                match crate::federation::broadcast_store_quorum(fed, &mem_echo).await {
                    Ok(tracker) => match crate::federation::finalise_quorum(&tracker) {
                        Ok(got) => {
                            response["quorum_acks"] = json!(got);
                            return (StatusCode::CREATED, Json(response)).into_response();
                        }
                        Err(err) => {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("Retry-After", "2")],
                                Json(serde_json::to_value(&payload).unwrap_or_default()),
                            )
                                .into_response();
                        }
                    },
                    Err(err) => {
                        let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("Retry-After", "2")],
                            Json(serde_json::to_value(&payload).unwrap_or_default()),
                        )
                            .into_response();
                    }
                }
            }
            (StatusCode::CREATED, Json(response)).into_response()
        }
        Err(e) => {
            // v0.7.0 Round-2 F7 — insert failed AFTER we committed the
            // quota counter; refund so the agent's quota reflects only
            // successful stores (mirrors the MCP path at
            // src/mcp.rs:1706). Refund is best-effort — a refund
            // failure is logged but does not change the response.
            if !quota_agent_id.is_empty() {
                if let Err(re) = crate::quotas::refund_op(&lock.0, &quota_agent_id, quota_op) {
                    tracing::warn!(
                        "quota refund_op failed for agent {}: {}",
                        &quota_agent_id,
                        re
                    );
                }
            }
            // v0.7.0 L1-6 Deliverable E — surface the substrate
            // governance pre-write hook's refusal as `403 FORBIDDEN`
            // with code `GOVERNANCE_REFUSED` and the operator-authored
            // reason verbatim. The substrate wraps the refusal in a
            // typed `storage::GovernanceRefusal` propagated via
            // `anyhow::Error`; downcasting here keeps the
            // happy-path-cheap `?`-friendly return shape upstream.
            if let Some(refusal) = e.downcast_ref::<crate::storage::GovernanceRefusal>() {
                tracing::info!(
                    "create_memory refused by substrate governance: {}",
                    refusal.reason
                );
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({
                        "code": "GOVERNANCE_REFUSED",
                        "error": refusal.reason,
                    })),
                )
                    .into_response();
            }
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn register_agent(
    State(app): State<AppState>,
    Json(body): Json<RegisterAgentBody>,
) -> impl IntoResponse {
    if let Err(e) = validate::validate_agent_id(&body.agent_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    if let Err(e) = validate::validate_agent_type(&body.agent_type) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let capabilities = body.capabilities.unwrap_or_default();
    if let Err(e) = validate::validate_capabilities(&capabilities) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    let lock = app.db.lock().await;
    let register_result =
        db::register_agent(&lock.0, &body.agent_id, &body.agent_type, &capabilities);
    // Read the persisted `_agents` row back so we can fan it out to peers.
    // The cluster-wide S12 invariant is that an agent registered on node-1
    // is visible on node-4 — which only holds when the `_agents` namespace
    // replicates via `broadcast_store_quorum`.
    let registered_mem = match &register_result {
        Ok(id) => db::get(&lock.0, id).ok().flatten(),
        Err(_) => None,
    };
    drop(lock);

    match register_result {
        Ok(id) => {
            if let (Some(fed), Some(mem)) = (app.federation.as_ref(), registered_mem.as_ref()) {
                match crate::federation::broadcast_store_quorum(fed, mem).await {
                    Ok(tracker) => {
                        if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("Retry-After", "2")],
                                Json(serde_json::to_value(&payload).unwrap_or_default()),
                            )
                                .into_response();
                        }
                    }
                    Err(e) => {
                        tracing::warn!("register_agent fanout error (local committed): {e:?}");
                    }
                }
            }
            (
                StatusCode::CREATED,
                Json(json!({
                    "registered": true,
                    "id": id,
                    "agent_id": body.agent_id,
                    "agent_type": body.agent_type,
                    "capabilities": capabilities,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Task 1.9 — pending_actions endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PendingListQuery {
    #[serde(default)]
    pub status: Option<String>,
    /// Optional namespace filter — S34 uses `?namespace=...&limit=50`.
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default = "default_pending_limit")]
    pub limit: Option<usize>,
}

#[allow(clippy::unnecessary_wraps)]
fn default_pending_limit() -> Option<usize> {
    Some(100)
}

pub async fn list_pending(
    State(app): State<AppState>,
    Query(p): Query<PendingListQuery>,
) -> impl IntoResponse {
    let limit = p.limit.unwrap_or(100).min(1000);

    // v0.7.0 Wave-3 Continuation 5 — postgres-backed daemons read
    // from the `pending_actions` table directly. The full governance
    // pipeline (Phase 20 / Cont 4 chain walk) writes pending rows on
    // both backends; this list path lights them up on the read side
    // so S34's "bob lists pending → approve/reject → charlie sees
    // approved" round-trip works end-to-end on postgres.
    #[cfg(feature = "sal-postgres")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        return match crate::store::postgres::list_pending_actions_via_store(
            &app.store,
            p.status.as_deref(),
            p.namespace.as_deref(),
            limit,
        )
        .await
        {
            Ok(items) => Json(json!({
                "count": items.len(),
                "pending": items,
                "storage_backend": "postgres",
            }))
            .into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::list_pending_actions(&lock.0, p.status.as_deref(), limit) {
        Ok(items) => Json(json!({"count": items.len(), "pending": items})).into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn approve_pending(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body_bytes: axum::body::Bytes,
) -> impl IntoResponse {
    use crate::db::ApproveOutcome;
    use crate::models::PendingDecision;
    // S5-C1 (v0.7.0 fix campaign 2026-05-13): privileged governance
    // endpoints MUST verify HMAC. The legacy `api_key_auth` middleware
    // pass-throughs when `api_key` is unset (default!), which means an
    // attacker could approve any pending action by spoofing `X-Agent-Id`.
    // We mirror the K10 SSE handler's posture and require
    // `X-AI-Memory-Signature` on every inbound approve request,
    // regardless of `api_key` configuration. Without a server-wide
    // `[hooks.subscription].hmac_secret`, `verify_approval_hmac`
    // refuses every request — the safe default.
    if let Err(status) = super::verify_approval_hmac(&headers, &body_bytes) {
        return (
            status,
            Json(json!({
                "error": "invalid or missing X-AI-Memory-Signature",
                "hint": "POST /api/v1/pending/{id}/approve requires HMAC signing per K7's pattern. \
                        Set [hooks.subscription] hmac_secret in config and send \
                        X-AI-Memory-Signature: sha256=<HMAC-SHA256(SHA256(secret), \"<ts>.<body>\")> \
                        with X-AI-Memory-Timestamp: <unix-epoch-secs>."
            })),
        )
            .into_response();
    }
    let state = app.db.clone();
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let agent_id = match crate::identity::resolve_http_agent_id(None, header_agent_id) {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid agent_id: {e}")})),
            )
                .into_response();
        }
    };

    // v0.7.0 Wave-3 Continuation 3 (Phase 20) — postgres-backed approve
    // routes through the FULL governance pipeline:
    // - inheritance-chain walk over `namespace_meta` (with explicit
    //   parent + `/`-derived ancestors, bounded + cycle-safe)
    // - approver_type variations: Human / Agent(required) / Consensus(N)
    // - multi-vote consensus state machine: registered-agent gating,
    //   case-insensitive duplicate-vote dedup, threshold transition
    // - audit emit + structured response envelope (Approved / Pending
    //   with vote count + quorum / Rejected with reason)
    //
    // Federation fanout for the decision + executed memory remains
    // sqlite-only (the broadcast_pending_decision_quorum path uses
    // sqlite-coupled fed-tracker state); postgres operators relying on
    // multi-node consistency should poll peers.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        use crate::store::ApproveOutcome as SalOutcome;
        let ctx = crate::store::CallerContext::for_agent(agent_id.clone());
        return match app
            .store
            .governance_approve_with_consensus(&ctx, &id, &agent_id)
            .await
        {
            Ok(SalOutcome::Approved) => {
                if crate::audit::is_enabled() {
                    crate::audit::emit(crate::audit::EventBuilder::new(
                        crate::audit::AuditAction::Approve,
                        crate::audit::actor(agent_id.clone(), "http_header", None),
                        crate::audit::target_memory(id.clone(), String::new(), None, None, None),
                    ));
                }
                // v0.7.0 Wave-3 Continuation 5 (S34) — execute the
                // approved action so the memory materialises in the
                // namespace where the cert oracle expects it. Mirrors
                // sqlite's `db::execute_pending_action` for the
                // `store` / `delete` / `promote` action types.
                let executed_id: Option<String> =
                    match app.store.execute_pending_action(&ctx, &id).await {
                        Ok(eid) => eid,
                        Err(e) => {
                            tracing::warn!(
                                "approve_pending: execute_pending_action failed for {id}: {e}"
                            );
                            None
                        }
                    };
                Json(json!({
                    "approved": true,
                    "id": id,
                    "decided_by": agent_id,
                    "executed": executed_id.is_some(),
                    "memory_id": executed_id,
                    "storage_backend": "postgres",
                }))
                .into_response()
            }
            Ok(SalOutcome::Pending { votes, quorum }) => (
                StatusCode::ACCEPTED,
                Json(json!({
                    "approved": false,
                    "status": "pending",
                    "id": id,
                    "votes": votes,
                    "quorum": quorum,
                    "reason": "consensus threshold not yet reached",
                    "storage_backend": "postgres",
                })),
            )
                .into_response(),
            Ok(SalOutcome::Rejected(reason)) => (
                StatusCode::FORBIDDEN,
                Json(json!({"error": format!("approve rejected: {reason}")})),
            )
                .into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = state.lock().await;
    match db::approve_with_approver_type(&lock.0, &id, &agent_id) {
        Ok(ApproveOutcome::Approved) => match db::execute_pending_action(&lock.0, &id) {
            Ok(memory_id) => {
                // v0.6.2 (S34): fan out the decision AND the resulting
                // memory so approve on one node makes the governed write
                // visible on every peer. Drop the DB lock before any
                // outbound HTTP.
                let produced_mem = memory_id
                    .as_deref()
                    .and_then(|mid| db::get(&lock.0, mid).ok().flatten());
                drop(lock);
                if let Some(fed) = app.federation.as_ref() {
                    let decision = PendingDecision {
                        id: id.clone(),
                        approved: true,
                        decider: agent_id.clone(),
                    };
                    match crate::federation::broadcast_pending_decision_quorum(fed, &decision).await
                    {
                        Ok(tracker) => {
                            if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                                let payload =
                                    crate::federation::QuorumNotMetPayload::from_err(&err);
                                return (
                                    StatusCode::SERVICE_UNAVAILABLE,
                                    [("Retry-After", "2")],
                                    Json(serde_json::to_value(&payload).unwrap_or_default()),
                                )
                                    .into_response();
                            }
                        }
                        Err(err) => {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("Retry-After", "2")],
                                Json(serde_json::to_value(&payload).unwrap_or_default()),
                            )
                                .into_response();
                        }
                    }
                    // If approval produced a brand-new memory (store
                    // path), also broadcast it so peers have the row.
                    // delete / promote paths produce no new memory
                    // (the pending payload carries memory_id).
                    if let Some(ref mem) = produced_mem
                        && let Some(resp) = fanout_or_503(&app, mem).await
                    {
                        return resp;
                    }
                }
                Json(json!({
                    "approved": true,
                    "id": id,
                    "decided_by": agent_id,
                    "executed": true,
                    "memory_id": memory_id,
                }))
                .into_response()
            }
            Err(e) => {
                tracing::error!("execute pending error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "approved but execution failed"})),
                )
                    .into_response()
            }
        },
        Ok(ApproveOutcome::Pending { votes, quorum }) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "approved": false,
                "status": "pending",
                "id": id,
                "votes": votes,
                "quorum": quorum,
                "reason": "consensus threshold not yet reached",
            })),
        )
            .into_response(),
        Ok(ApproveOutcome::Rejected(reason)) => (
            StatusCode::FORBIDDEN,
            Json(json!({"error": format!("approve rejected: {reason}")})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn reject_pending(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body_bytes: axum::body::Bytes,
) -> impl IntoResponse {
    use crate::models::PendingDecision;
    // S5-C1 (v0.7.0 fix campaign 2026-05-13): parity with approve_pending.
    // Legacy reject endpoint MUST verify HMAC for the same reason — an
    // unsigned reject is just as dangerous (denial-of-service against
    // governance state, write-amplifies pending row churn).
    if let Err(status) = super::verify_approval_hmac(&headers, &body_bytes) {
        return (
            status,
            Json(json!({
                "error": "invalid or missing X-AI-Memory-Signature",
                "hint": "POST /api/v1/pending/{id}/reject requires HMAC signing per K7's pattern. \
                        Set [hooks.subscription] hmac_secret in config and send \
                        X-AI-Memory-Signature: sha256=<HMAC-SHA256(SHA256(secret), \"<ts>.<body>\")> \
                        with X-AI-Memory-Timestamp: <unix-epoch-secs>."
            })),
        )
            .into_response();
    }
    let state = app.db.clone();
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let agent_id = match crate::identity::resolve_http_agent_id(None, header_agent_id) {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid agent_id: {e}")})),
            )
                .into_response();
        }
    };

    // v0.7.0 Wave-3 Continuation 2 (Phase 11) — postgres-backed reject.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent(agent_id.clone());
        return match app.store.pending_decide(&ctx, &id, false, &agent_id).await {
            Ok(true) => {
                if crate::audit::is_enabled() {
                    crate::audit::emit(crate::audit::EventBuilder::new(
                        crate::audit::AuditAction::Reject,
                        crate::audit::actor(agent_id.clone(), "http_header", None),
                        crate::audit::target_memory(id.clone(), String::new(), None, None, None),
                    ));
                }
                Json(json!({
                    "rejected": true,
                    "id": id,
                    "decided_by": agent_id,
                    "storage_backend": "postgres",
                }))
                .into_response()
            }
            Ok(false) => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "pending action not found or already decided"})),
            )
                .into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = state.lock().await;
    match db::decide_pending_action(&lock.0, &id, false, &agent_id) {
        Ok(true) => {
            drop(lock);
            // v0.6.2 (S34): fan out the reject so peers converge.
            if let Some(fed) = app.federation.as_ref() {
                let decision = PendingDecision {
                    id: id.clone(),
                    approved: false,
                    decider: agent_id.clone(),
                };
                match crate::federation::broadcast_pending_decision_quorum(fed, &decision).await {
                    Ok(tracker) => {
                        if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("Retry-After", "2")],
                                Json(serde_json::to_value(&payload).unwrap_or_default()),
                            )
                                .into_response();
                        }
                    }
                    Err(err) => {
                        let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("Retry-After", "2")],
                            Json(serde_json::to_value(&payload).unwrap_or_default()),
                        )
                            .into_response();
                    }
                }
            }
            Json(json!({"rejected": true, "id": id, "decided_by": agent_id})).into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "pending action not found or already decided"})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn list_agents(State(app): State<AppState>) -> impl IntoResponse {
    // v0.7.0 Wave-3 Continuation — postgres-backed daemons project from
    // the `_agents` namespace via the SAL `list` trait method, mirroring
    // how sqlite's `db::list_agents` reads from the same namespace.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("daemon");
        let filter = crate::store::Filter {
            namespace: Some("_agents".to_string()),
            limit: 1000,
            ..Default::default()
        };
        return match app.store.list(&ctx, &filter).await {
            Ok(memories) => {
                let agents: Vec<serde_json::Value> = memories
                    .iter()
                    .filter_map(|m| {
                        let meta = m.metadata.as_object()?;
                        let agent_id = meta.get("agent_id")?.as_str()?;
                        let agent_type = meta
                            .get("agent_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let capabilities = meta
                            .get("capabilities")
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!([]));
                        Some(json!({
                            "agent_id": agent_id,
                            "agent_type": agent_type,
                            "capabilities": capabilities,
                            "registered_at": m.created_at,
                        }))
                    })
                    .collect();
                (
                    StatusCode::OK,
                    Json(json!({"count": agents.len(), "agents": agents})),
                )
                    .into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::list_agents(&lock.0) {
        Ok(agents) => (
            StatusCode::OK,
            Json(json!({"count": agents.len(), "agents": agents})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn get_memory(State(app): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 — Postgres-backed daemons dispatch through the
    // SAL trait. The legacy `db::resolve_id` path is SQLite-bound (it
    // walks `memories` + `memory_links` directly through the
    // mutex-guarded rusqlite connection); routing the postgres branch
    // through `app.store` keeps the wire-shape identical while
    // hitting the right backend. SQLite-backed daemons keep the
    // legacy direct-rusqlite path for v0.7.0 binary parity.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("ai:http");
        return match app.store.get(&ctx, &id).await {
            Ok(mem) => {
                // List_links surfaces the full edge set (no namespace
                // filter) so the postgres adapter's `list_links` walks
                // its `memory_links` table and the local-side filter
                // narrows to edges anchored at this memory id.
                let edges = match app.store.list_links(None).await {
                    Ok(rows) => rows
                        .into_iter()
                        .filter(|l| l.source_id == mem.id || l.target_id == mem.id)
                        .collect::<Vec<_>>(),
                    Err(e) => {
                        tracing::warn!(
                            "store.list_links during get_memory failed: {e}; \
                             returning memory with empty links"
                        );
                        Vec::new()
                    }
                };
                Json(json!({"memory": mem, "links": edges})).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::resolve_id(&lock.0, &id) {
        Ok(Some(mem)) => {
            let links = db::get_links(&lock.0, &mem.id).unwrap_or_default();
            Json(json!({"memory": mem, "links": links})).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response(),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("ambiguous ID prefix") {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
            }
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn update_memory(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateMemory>,
) -> impl IntoResponse {
    let state = app.db.clone();
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    if let Err(e) = validate::validate_update(&body) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 — Postgres-backed daemons take the SAL trait
    // dispatch path. The trait's `update` accepts an `UpdatePatch`
    // shape; map the `UpdateMemory` body into the trait shape and
    // delegate. The legacy SQLite path below threads federation,
    // embedder regen, audit, and governance hooks; Postgres takes
    // the simpler shape until those layers are also trait-routed.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let patch = crate::store::UpdatePatch {
            title: body.title.clone(),
            content: body.content.clone(),
            tier: body.tier.clone(),
            namespace: body.namespace.clone(),
            tags: body.tags.clone(),
            priority: body.priority,
            confidence: body.confidence,
            metadata: body.metadata.clone(),
        };
        let ctx = crate::store::CallerContext::for_agent("ai:http");
        return match app.store.update(&ctx, &id, patch).await {
            Ok(()) => {
                // Re-fetch through the trait so the response payload
                // mirrors the legacy SQLite path's "return the updated
                // row" wire shape.
                match app.store.get(&ctx, &id).await {
                    Ok(mem) => Json(json!(mem)).into_response(),
                    Err(_) => Json(json!({"updated": true, "id": id})).into_response(),
                }
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = state.lock().await;
    // Resolve prefix if exact ID not found
    let resolved_id = match db::resolve_id(&lock.0, &id) {
        Ok(Some(mem)) => mem.id,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("ambiguous ID prefix") {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
            }
            tracing::error!("handler error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };
    // Preserve existing agent_id when caller provides new metadata — provenance
    // is immutable after first write (see NHI design in crate::identity).
    let preserved_metadata = body.metadata.as_ref().map(|new_meta| {
        let existing_meta = db::get(&lock.0, &resolved_id).ok().flatten().map_or_else(
            || serde_json::Value::Object(serde_json::Map::new()),
            |m| m.metadata,
        );
        crate::identity::preserve_agent_id(&existing_meta, new_meta)
    });
    match db::update(
        &lock.0,
        &resolved_id,
        body.title.as_deref(),
        body.content.as_deref(),
        body.tier.as_ref(),
        body.namespace.as_deref(),
        body.tags.as_ref(),
        body.priority,
        body.confidence,
        body.expires_at.as_deref(),
        preserved_metadata.as_ref(),
    ) {
        Ok((true, _)) => {
            let mem = db::get(&lock.0, &resolved_id).ok().flatten();
            // Issue #219: regenerate the embedding when the searchable text
            // (title/content) changed. Without this, the semantic index keeps
            // pointing at the old vector and stale semantic recall results
            // linger even after the row is updated.
            let content_changed = body.title.is_some() || body.content.is_some();
            let mut lock_opt = Some(lock);
            if content_changed && let Some(ref m) = mem {
                let text = format!("{} {}", m.title, m.content);
                if let Some(emb) = app.embedder.as_ref().as_ref() {
                    match emb.embed(&text) {
                        Ok(vec) => {
                            if let Some(ref l) = lock_opt
                                && let Err(e) = db::set_embedding(&l.0, &resolved_id, &vec)
                            {
                                tracing::warn!(
                                    "failed to refresh embedding for {resolved_id}: {e}"
                                );
                            }
                            // Drop DB lock before touching vector index.
                            lock_opt.take();
                            let mut idx_lock = app.vector_index.lock().await;
                            if let Some(idx) = idx_lock.as_mut() {
                                idx.remove(&resolved_id);
                                idx.insert(resolved_id.clone(), vec);
                            }
                        }
                        Err(e) => tracing::warn!("embedding regeneration failed: {e}"),
                    }
                }
            }
            // Drop the DB lock before fanning out — peers POST back to
            // our sync_push so we'd deadlock if we held it.
            drop(lock_opt);
            // v0.6.0.1: fan out the mutation to peers so remote readers
            // see the update, not the pre-update row. insert_if_newer on
            // peers sees a newer updated_at and applies.
            if let (Some(fed), Some(m)) = (app.federation.as_ref(), mem.as_ref())
                && let Ok(tracker) = crate::federation::broadcast_store_quorum(fed, m).await
                && let Err(err) = crate::federation::finalise_quorum(&tracker)
            {
                let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [("Retry-After", "2")],
                    Json(serde_json::to_value(&payload).unwrap_or_default()),
                )
                    .into_response();
            }
            Json(json!(mem)).into_response()
        }
        Ok((false, _)) => {
            (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("already exists in namespace") {
                return (StatusCode::CONFLICT, Json(json!({"error": msg}))).into_response();
            }
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn delete_memory(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let state = app.db.clone();
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 — Postgres-backed daemons dispatch through the
    // SAL trait. The legacy delete path threads governance, audit,
    // and federation fanout through the SQLite mutex; those layers
    // (governance owner-walk, audit chain, quorum broadcast) are
    // SQLite-bound today, so the postgres-eligible delete is the
    // simpler "delete by id" surface the SAL trait already provides.
    // Operators who need the full governance + audit + quorum bundle
    // on Postgres should follow the migration plan in
    // `docs/postgres-age-guide.md`.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        // Resolve the target memory before delete so the audit emit
        // captures namespace + title metadata (Phase 9 — audit emit
        // parity on postgres).
        let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
        let agent_id = crate::identity::resolve_http_agent_id(None, header_agent_id)
            .unwrap_or_else(|_| "ai:http".to_string());
        let ctx = crate::store::CallerContext::for_agent(agent_id.clone());
        let target = app.store.get(&ctx, &id).await.ok();

        // F-A2A1.2 (#700) — governance enforcement on the postgres delete
        // path. Mirrors the sqlite gate at line ~1913 below: a denied
        // delete returns 403; an `Approve`-level policy queues a pending
        // action and returns 202 Accepted. Without this gate the postgres
        // branch silently bypassed the namespace standard's `delete=`
        // rule, allowing any caller to delete a row in a governed
        // namespace. Closes the postgres half of the same surface S34/S60
        // exercise on the write path.
        if let Some(ref mem) = target {
            use crate::models::GovernanceDecision;
            let memory_owner = mem
                .metadata
                .get("agent_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let payload = json!({"id": mem.id, "title": mem.title});
            match app
                .store
                .enforce_governance_action(
                    crate::store::GovernedAction::Delete,
                    &mem.namespace,
                    &agent_id,
                    Some(&mem.id),
                    memory_owner.as_deref(),
                    &payload,
                )
                .await
            {
                Ok(GovernanceDecision::Allow) => {}
                Ok(GovernanceDecision::Deny(reason)) => {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({"error": format!("delete denied by governance: {reason}")})),
                    )
                        .into_response();
                }
                Ok(GovernanceDecision::Pending(pending_id)) => {
                    return (
                        StatusCode::ACCEPTED,
                        Json(json!({
                            "status": "pending",
                            "pending_id": pending_id,
                            "reason": "governance requires approval",
                            "action": "delete",
                            "memory_id": mem.id,
                            "storage_backend": "postgres",
                        })),
                    )
                        .into_response();
                }
                Err(e) => return store_err_to_response(e),
            }
        }

        return match app.store.delete(&ctx, &id).await {
            Ok(()) => {
                if crate::audit::is_enabled() {
                    let (namespace, title, tier) = target
                        .as_ref()
                        .map(|m| {
                            (
                                m.namespace.clone(),
                                Some(m.title.clone()),
                                Some(m.tier.to_string()),
                            )
                        })
                        .unwrap_or_else(|| (String::new(), None, None));
                    crate::audit::emit(crate::audit::EventBuilder::new(
                        crate::audit::AuditAction::Delete,
                        crate::audit::actor(agent_id, "http_header", None),
                        crate::audit::target_memory(id.clone(), namespace, title, tier, None),
                    ));
                }
                (StatusCode::OK, Json(json!({"deleted": true, "id": id}))).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = state.lock().await;
    // Resolve the target memory so governance has owner context.
    let target = match db::resolve_id(&lock.0, &id) {
        Ok(Some(m)) => m,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("ambiguous ID prefix") {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
            }
            tracing::error!("handler error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };

    // Task 1.9: governance enforcement (delete-side).
    {
        use crate::models::{GovernanceDecision, GovernedAction};
        let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
        let agent_id = match crate::identity::resolve_http_agent_id(None, header_agent_id) {
            Ok(a) => a,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid agent_id: {e}")})),
                )
                    .into_response();
            }
        };
        let mem_owner = target
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let payload = json!({"id": target.id, "title": target.title});
        match db::enforce_governance(
            &lock.0,
            GovernedAction::Delete,
            &target.namespace,
            &agent_id,
            Some(&target.id),
            mem_owner.as_deref(),
            &payload,
        ) {
            Ok(GovernanceDecision::Allow) => {}
            Ok(GovernanceDecision::Deny(reason)) => {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": format!("delete denied by governance: {reason}")})),
                )
                    .into_response();
            }
            Ok(GovernanceDecision::Pending(pending_id)) => {
                // v0.6.2 (S34): fan out the new pending delete row so peers
                // see consistent governance queue state.
                let pending_row = db::get_pending_action(&lock.0, &pending_id).ok().flatten();
                // v0.7.0 K4 — surface the new row through the
                // subscription dispatcher (`approval_requested`). See
                // the store-side companion call for rationale.
                crate::subscriptions::dispatch_approval_requested(&lock.0, &pending_id, &lock.1);
                let target_id = target.id.clone();
                drop(lock);
                if let (Some(pa), Some(fed)) = (pending_row.as_ref(), app.federation.as_ref()) {
                    match crate::federation::broadcast_pending_quorum(fed, pa).await {
                        Ok(tracker) => {
                            if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                                let payload =
                                    crate::federation::QuorumNotMetPayload::from_err(&err);
                                return (
                                    StatusCode::SERVICE_UNAVAILABLE,
                                    [("Retry-After", "2")],
                                    Json(serde_json::to_value(&payload).unwrap_or_default()),
                                )
                                    .into_response();
                            }
                        }
                        Err(err) => {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("Retry-After", "2")],
                                Json(serde_json::to_value(&payload).unwrap_or_default()),
                            )
                                .into_response();
                        }
                    }
                }
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({
                        "status": "pending",
                        "pending_id": pending_id,
                        "reason": "governance requires approval",
                        "action": "delete",
                        "memory_id": target_id,
                    })),
                )
                    .into_response();
            }
            Err(e) => {
                tracing::error!("governance error: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "governance check failed"})),
                )
                    .into_response();
            }
        }
    }

    let delete_outcome = db::delete(&lock.0, &target.id);
    // v0.6.4-017 — G9 HTTP webhook parity. Fire `memory_delete` after
    // the row is gone (mirrors the MCP pattern at mcp.rs:2227). Snapshot
    // fields come from the pre-delete `target`. Best-effort,
    // fire-and-forget: dispatch does a quick subscriber lookup on the
    // current connection and spawns a thread for the HTTP POST so the
    // response is never blocked. Held inside the lock so the subscriber
    // list query has a connection — release happens after.
    if matches!(delete_outcome, Ok(true)) {
        let details = serde_json::to_value(crate::subscriptions::DeleteEventDetails {
            title: target.title.clone(),
            tier: target.tier.to_string(),
        })
        .ok();
        let owner_aid = target
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        crate::subscriptions::dispatch_event_with_details(
            &lock.0,
            "memory_delete",
            &target.id,
            &target.namespace,
            owner_aid.as_deref(),
            &lock.1,
            details,
        );
    }
    // Drop DB lock before fanning out — peers POST back to our
    // sync_push and we'd deadlock on the shared Mutex if we held it.
    drop(lock);
    match delete_outcome {
        Ok(true) => {
            // PR-5 (issue #487): security audit trail for HTTP delete.
            let owner = target
                .metadata
                .get("agent_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    headers
                        .get("x-agent-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("anonymous")
                        .to_string()
                });
            crate::audit::emit(crate::audit::EventBuilder::new(
                crate::audit::AuditAction::Delete,
                crate::audit::actor(owner, "http_header", None),
                crate::audit::target_memory(
                    target.id.clone(),
                    target.namespace.clone(),
                    Some(target.title.clone()),
                    Some(target.tier.to_string()),
                    None,
                ),
            ));
            // v0.6.0.1: propagate tombstone via sync_push.deletions.
            if let Some(fed) = app.federation.as_ref()
                && let Ok(tracker) =
                    crate::federation::broadcast_delete_quorum(fed, &target.id).await
                && let Err(err) = crate::federation::finalise_quorum(&tracker)
            {
                let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [("Retry-After", "2")],
                    Json(serde_json::to_value(&payload).unwrap_or_default()),
                )
                    .into_response();
            }
            Json(json!({"deleted": true})).into_response()
        }
        _ => (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response(),
    }
}

#[allow(clippy::too_many_lines)]
pub async fn promote_memory(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let state = app.db.clone();
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 Continuation 5 (state-flake / S16+S49) — postgres-
    // backed daemons resolve the memory through the SAL trait so a
    // freshly-stored row promotes correctly across daemon restart.
    // Without this branch the handler reaches into the scratch SQLite
    // db (`:memory:` in test, stale on droplet after disposable DB
    // reset) and returns 404 — the documented Wave 4 R2 flake.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
        let agent_id = match crate::identity::resolve_http_agent_id(None, header_agent_id) {
            Ok(a) => a,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid agent_id: {e}")})),
                )
                    .into_response();
            }
        };
        let ctx = crate::store::CallerContext::for_agent(&agent_id);
        // F-A2A1.4 (#700, S16/S49) — bounded retry on NotFound. A
        // freshly-stored row that travelled through a read replica or
        // is still settling in WAL flush can briefly return
        // NotFound from the SAL `get`. The 22-failure triage (memory
        // 9ffaa55d) classified this as Bucket-A: the row exists, the
        // promote handler just races the visibility window. Retry up
        // to 4 times with bounded backoff (5/10/15/20 ms — 50 ms
        // total) before surfacing 404 — well below the 2 s daemon
        // p99 SLO and dwarfed by typical store-side replication
        // latency. See `get_with_visibility_retry` for the helper.
        let target = match get_with_visibility_retry(app.store.as_ref(), &ctx, &id).await {
            Ok(m) => m,
            Err(crate::store::StoreError::NotFound { .. }) => {
                return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"})))
                    .into_response();
            }
            Err(e) => return store_err_to_response(e),
        };

        // F-A2A1.2 (#700) — governance enforcement on the postgres promote
        // path. Mirrors the sqlite gate at line ~2169 below: an `owner`
        // policy on the namespace standard denies a non-owner promote
        // (403); an `approve`-level policy queues a pending action (202).
        // The postgres branch previously skipped this gate, letting any
        // caller promote a row to `long` tier regardless of namespace
        // governance.
        {
            use crate::models::GovernanceDecision;
            let memory_owner = target
                .metadata
                .get("agent_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let payload = json!({"id": target.id});
            match app
                .store
                .enforce_governance_action(
                    crate::store::GovernedAction::Promote,
                    &target.namespace,
                    &agent_id,
                    Some(&target.id),
                    memory_owner.as_deref(),
                    &payload,
                )
                .await
            {
                Ok(GovernanceDecision::Allow) => {}
                Ok(GovernanceDecision::Deny(reason)) => {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({"error": format!("promote denied by governance: {reason}")})),
                    )
                        .into_response();
                }
                Ok(GovernanceDecision::Pending(pending_id)) => {
                    return (
                        StatusCode::ACCEPTED,
                        Json(json!({
                            "status": "pending",
                            "pending_id": pending_id,
                            "reason": "governance requires approval",
                            "action": "promote",
                            "memory_id": target.id,
                            "storage_backend": "postgres",
                        })),
                    )
                        .into_response();
                }
                Err(e) => return store_err_to_response(e),
            }
        }

        let patch = crate::store::UpdatePatch {
            tier: Some(Tier::Long),
            ..Default::default()
        };
        return match app.store.update(&ctx, &target.id, patch).await {
            Ok(()) => {
                // F-A2A1.4 (#700, S16/S49) — post-promote federation
                // fanout on the postgres branch. Mirrors the sqlite
                // path at lines ~2406-2417: after a successful local
                // tier-update, re-fetch the row to capture the new
                // tier + cleared expiry and broadcast via
                // `broadcast_store_quorum` so peers' projections of
                // the same memory inherit the tier ladder. Without
                // this, a `notify` recipient on peer-B still sees the
                // row at its pre-promote tier and a recall against
                // `tier=long` on peer-B silently misses it.
                //
                // Failure handling: fanout failures surface as 503
                // with `Retry-After: 2` mirroring sqlite. The local
                // tier update has already committed — per ADR-0001
                // we do NOT roll back the local commit on quorum
                // failure; the sync daemon's eventual-consistency
                // loop catches stragglers.
                if let Some(fed) = app.federation.as_ref() {
                    let promoted_mem = match app.store.get(&ctx, &target.id).await {
                        Ok(m) => Some(m),
                        Err(_) => None,
                    };
                    if let Some(ref m) = promoted_mem {
                        match crate::federation::broadcast_store_quorum(fed, m).await {
                            Ok(tracker) => {
                                if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                                    let payload =
                                        crate::federation::QuorumNotMetPayload::from_err(&err);
                                    return (
                                        StatusCode::SERVICE_UNAVAILABLE,
                                        [("Retry-After", "2")],
                                        Json(serde_json::to_value(&payload).unwrap_or_default()),
                                    )
                                        .into_response();
                                }
                            }
                            Err(err) => {
                                let payload =
                                    crate::federation::QuorumNotMetPayload::from_err(&err);
                                return (
                                    StatusCode::SERVICE_UNAVAILABLE,
                                    [("Retry-After", "2")],
                                    Json(serde_json::to_value(&payload).unwrap_or_default()),
                                )
                                    .into_response();
                            }
                        }
                    }
                }
                Json(json!({
                    "promoted": true,
                    "id": target.id,
                    "tier": "long",
                    "storage_backend": "postgres",
                }))
                .into_response()
            }
            Err(crate::store::StoreError::NotFound { .. }) => {
                (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = state.lock().await;
    // Resolve prefix if exact ID not found — capture full memory for governance.
    let target = match db::resolve_id(&lock.0, &id) {
        Ok(Some(mem)) => mem,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("ambiguous ID prefix") {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
            }
            tracing::error!("handler error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };
    // Task 1.9: governance enforcement (promote-side).
    {
        use crate::models::{GovernanceDecision, GovernedAction};
        let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
        let agent_id = match crate::identity::resolve_http_agent_id(None, header_agent_id) {
            Ok(a) => a,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid agent_id: {e}")})),
                )
                    .into_response();
            }
        };
        let mem_owner = target
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let payload = json!({"id": target.id});
        match db::enforce_governance(
            &lock.0,
            GovernedAction::Promote,
            &target.namespace,
            &agent_id,
            Some(&target.id),
            mem_owner.as_deref(),
            &payload,
        ) {
            Ok(GovernanceDecision::Allow) => {}
            Ok(GovernanceDecision::Deny(reason)) => {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": format!("promote denied by governance: {reason}")})),
                )
                    .into_response();
            }
            Ok(GovernanceDecision::Pending(pending_id)) => {
                // v0.6.2 (S34): fan out the new pending promote row too.
                let pending_row = db::get_pending_action(&lock.0, &pending_id).ok().flatten();
                // v0.7.0 K4 — surface the new row through the
                // subscription dispatcher (`approval_requested`). See
                // the store-side companion call for rationale.
                crate::subscriptions::dispatch_approval_requested(&lock.0, &pending_id, &lock.1);
                let target_id = target.id.clone();
                drop(lock);
                if let (Some(pa), Some(fed)) = (pending_row.as_ref(), app.federation.as_ref()) {
                    match crate::federation::broadcast_pending_quorum(fed, pa).await {
                        Ok(tracker) => {
                            if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                                let payload =
                                    crate::federation::QuorumNotMetPayload::from_err(&err);
                                return (
                                    StatusCode::SERVICE_UNAVAILABLE,
                                    [("Retry-After", "2")],
                                    Json(serde_json::to_value(&payload).unwrap_or_default()),
                                )
                                    .into_response();
                            }
                        }
                        Err(err) => {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("Retry-After", "2")],
                                Json(serde_json::to_value(&payload).unwrap_or_default()),
                            )
                                .into_response();
                        }
                    }
                }
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({
                        "status": "pending",
                        "pending_id": pending_id,
                        "reason": "governance requires approval",
                        "action": "promote",
                        "memory_id": target_id,
                    })),
                )
                    .into_response();
            }
            Err(e) => {
                tracing::error!("governance error: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "governance check failed"})),
                )
                    .into_response();
            }
        }
    }

    let resolved_id = target.id.clone();
    match db::update(
        &lock.0,
        &resolved_id,
        None,
        None,
        Some(&Tier::Long),
        None,
        None,
        None,
        None,
        None,
        None,
    ) {
        Ok((true, _)) => {
            if let Err(e) = lock.0.execute(
                "UPDATE memories SET expires_at = NULL WHERE id = ?1",
                rusqlite::params![resolved_id],
            ) {
                tracing::error!("promote clear expiry failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal server error"})),
                )
                    .into_response();
            }
            // v0.6.0.1: fan out the promoted memory so peers pick up the
            // new tier + cleared expiry via insert_if_newer's newer-wins merge.
            let promoted_mem = db::get(&lock.0, &resolved_id).ok().flatten();
            // v0.6.4-017 — G9 HTTP webhook parity. Fire `memory_promote`
            // (tier mode — HTTP only does tier promotion, MCP also does
            // vertical). Mirrors mcp.rs:2369 pattern.
            let owner_aid = target
                .metadata
                .get("agent_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let details = serde_json::to_value(crate::subscriptions::PromoteEventDetails {
                mode: "tier".to_string(),
                tier: Some("long".to_string()),
                to_namespace: None,
                clone_id: None,
            })
            .ok();
            crate::subscriptions::dispatch_event_with_details(
                &lock.0,
                "memory_promote",
                &resolved_id,
                &target.namespace,
                owner_aid.as_deref(),
                &lock.1,
                details,
            );
            drop(lock);
            if let (Some(fed), Some(m)) = (app.federation.as_ref(), promoted_mem.as_ref())
                && let Ok(tracker) = crate::federation::broadcast_store_quorum(fed, m).await
                && let Err(err) = crate::federation::finalise_quorum(&tracker)
            {
                let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [("Retry-After", "2")],
                    Json(serde_json::to_value(&payload).unwrap_or_default()),
                )
                    .into_response();
            }
            Json(json!({"promoted": true, "id": resolved_id, "tier": "long"})).into_response()
        }
        Ok((false, _)) => {
            (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
        }
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn list_memories(
    State(app): State<AppState>,
    Query(p): Query<ListQuery>,
) -> impl IntoResponse {
    // #197: validate agent_id filter values
    if let Some(ref aid) = p.agent_id
        && let Err(e) = validate::validate_agent_id(aid)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid agent_id filter: {e}")})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 — Postgres-backed daemons dispatch through the
    // SAL trait. The trait's `Filter` shape carries
    // `(namespace, tier, tags_any, agent_id, since, until, limit)`,
    // which is the same projection the legacy `db::list` accepts plus
    // a deterministic ordering. The `min_priority` and `offset`
    // filters that exist only on the SQLite path are not yet exposed
    // through the trait — when set on a Postgres daemon they are
    // silently ignored (logged at debug). Offset can be emulated
    // client-side by raising `limit` and slicing; min_priority is
    // tracked for trait extension in the next wave.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        if p.offset.unwrap_or(0) > 0 {
            tracing::debug!(
                "list_memories on postgres: ?offset is unsupported on the SAL trait; ignored"
            );
        }
        if p.min_priority.is_some() {
            tracing::debug!(
                "list_memories on postgres: ?min_priority is unsupported on the SAL trait; ignored"
            );
        }
        let limit = p.limit.unwrap_or(20).min(MAX_BULK_SIZE);
        let since = p
            .since
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc));
        let until = p
            .until
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc));
        let filter = crate::store::Filter {
            namespace: p.namespace.clone(),
            tier: p.tier.clone(),
            tags_any: p
                .tags
                .as_deref()
                .map(|s| s.split(',').map(str::to_string).collect())
                .unwrap_or_default(),
            agent_id: p.agent_id.clone(),
            since,
            until,
            limit,
        };
        let ctx = crate::store::CallerContext::for_agent("ai:http");
        return match app.store.list(&ctx, &filter).await {
            Ok(mems) => Json(json!({"memories": mems, "count": mems.len()})).into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    // v0.6.2 (S40): raise ceiling from 200 → `MAX_BULK_SIZE` (1000) so bulk
    // fanout scenarios that POST 500+ rows to a leader can verify full
    // peer delivery via a single `GET /memories?limit=N` (previously the
    // list silently capped at 200 regardless of whether fanout worked).
    // Default remains 20 — only explicit `?limit=` callers see the
    // higher ceiling.
    let limit = p.limit.unwrap_or(20).min(MAX_BULK_SIZE);
    match db::list(
        &lock.0,
        p.namespace.as_deref(),
        p.tier.as_ref(),
        limit,
        p.offset.unwrap_or(0),
        p.min_priority,
        p.since.as_deref(),
        p.until.as_deref(),
        p.tags.as_deref(),
        p.agent_id.as_deref(),
    ) {
        Ok(mems) => Json(json!({"memories": mems, "count": mems.len()})).into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn search_memories(
    State(app): State<AppState>,
    Query(p): Query<SearchQuery>,
) -> impl IntoResponse {
    if p.q.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "query is required"})),
        )
            .into_response();
    }
    // #197: validate agent_id filter values
    if let Some(ref aid) = p.agent_id
        && let Err(e) = validate::validate_agent_id(aid)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid agent_id filter: {e}")})),
        )
            .into_response();
    }
    // #151 visibility: validate --as-agent namespace if supplied
    if let Some(ref a) = p.as_agent
        && let Err(e) = validate::validate_namespace(a)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid as_agent: {e}")})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 — Postgres-backed daemons dispatch through the
    // SAL trait. The Postgres adapter's `search` runs the same
    // text-search projection as SQLite's FTS5 path with the trait's
    // `Filter` carried verbatim; result wire-shape matches the
    // legacy `db::search` envelope.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let limit = p.limit.unwrap_or(20).min(MAX_BULK_SIZE);
        let since = p
            .since
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc));
        let until = p
            .until
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&chrono::Utc));
        let filter = crate::store::Filter {
            namespace: p.namespace.clone(),
            tier: p.tier.clone(),
            tags_any: p
                .tags
                .as_deref()
                .map(|s| s.split(',').map(str::to_string).collect())
                .unwrap_or_default(),
            agent_id: p.agent_id.clone(),
            since,
            until,
            limit,
        };
        let ctx = crate::store::CallerContext {
            agent_id: "ai:http".to_string(),
            as_agent: p.as_agent.clone(),
            request_id: None,
        };
        return match app.store.search(&ctx, &p.q, &filter).await {
            Ok(r) => Json(json!({"results": r, "count": r.len(), "query": p.q})).into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    // v0.6.2 (S40): mirror the `list_memories` ceiling raise so search
    // over a bulk-populated namespace isn't also capped at 200.
    let limit = p.limit.unwrap_or(20).min(MAX_BULK_SIZE);
    match db::search(
        &lock.0,
        &p.q,
        p.namespace.as_deref(),
        p.tier.as_ref(),
        limit,
        p.min_priority,
        p.since.as_deref(),
        p.until.as_deref(),
        p.tags.as_deref(),
        p.agent_id.as_deref(),
        p.as_agent.as_deref(),
        false,
    ) {
        Ok(r) => Json(json!({"results": r, "count": r.len(), "query": p.q})).into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// v0.7.0 (issue #518) — when `session_default == true` AND the
/// caller omitted a given filter axis, splice in the configured
/// `[agents.defaults.recall_scope]` value. Always returns the
/// (namespace, since, tier, limit) tuple that subsequent handler
/// code uses, regardless of whether the splice fired. The
/// `recall_scope_tier` value is plumbed through to the postgres
/// SAL path (which carries a `Filter.tier`) — sqlite recall does
/// not currently expose a tier filter, so this field is a no-op on
/// the legacy path.
///
/// Resolution: explicit args > recall_scope defaults > compiled
/// defaults.
#[allow(clippy::type_complexity)]
fn apply_recall_scope_defaults(
    app: &AppState,
    session_default: Option<bool>,
    explicit_namespace: Option<String>,
    explicit_since: Option<String>,
    explicit_limit: Option<usize>,
) -> (Option<String>, Option<String>, Option<String>, usize) {
    let want_splice = session_default.unwrap_or(false);
    let scope_opt: Option<&crate::config::RecallScope> = if want_splice {
        app.recall_scope.as_ref().as_ref()
    } else {
        None
    };

    let namespace = explicit_namespace.or_else(|| {
        scope_opt
            .and_then(|s| s.namespaces.as_ref())
            .and_then(|v| v.first())
            .cloned()
    });

    let since = explicit_since.or_else(|| {
        scope_opt.and_then(|s| {
            s.since.as_deref().and_then(|d| {
                crate::config::parse_duration_string(d).map(|dur| {
                    let cutoff = chrono::Utc::now() - dur;
                    cutoff.to_rfc3339()
                })
            })
        })
    });

    let tier = scope_opt.and_then(|s| s.tier.clone());

    let limit_explicit = explicit_limit;
    let resolved_limit = match limit_explicit {
        Some(v) => v,
        None => match scope_opt.and_then(|s| s.limit) {
            Some(v) => v as usize,
            None => 10,
        },
    };
    let resolved_limit = resolved_limit.min(50);

    (namespace, since, tier, resolved_limit)
}

pub async fn recall_memories_get(
    State(app): State<AppState>,
    Query(p): Query<RecallQuery>,
) -> impl IntoResponse {
    // Accept `context` (canonical), `query` (cert harness alias —
    // S79 uses `?query=…`), or `q` (search-style alias — the parity
    // suite uses `?q=…`). Cert oracles continue to work.
    let ctx = p
        .context
        .clone()
        .or_else(|| p.query.clone())
        .or_else(|| p.q.clone())
        .unwrap_or_default();
    if ctx.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "context (or query) is required"})),
        )
            .into_response();
    }
    // Phase P6 (R1): `budget_tokens=0` is now a valid request meaning
    // "return zero memories" — see `db::apply_token_budget`. The
    // earlier Ultrareview #348 hard-reject is replaced by always
    // round-tripping the requested budget in the response so a
    // genuinely buggy uninitialised counter is still observable.
    if let Some(ref a) = p.as_agent
        && let Err(e) = validate::validate_namespace(a)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid as_agent: {e}")})),
        )
            .into_response();
    }
    // v0.7.0 (issue #518) — splice `[agents.defaults.recall_scope]`
    // when `session_default=true` AND the caller omitted the
    // matching filter axis. Resolution: explicit args win.
    let (ns_resolved, since_resolved, tier_resolved, limit) = apply_recall_scope_defaults(
        &app,
        p.session_default,
        p.namespace.clone(),
        p.since.clone(),
        p.limit,
    );
    recall_response(
        &app,
        &ctx,
        ns_resolved.as_deref(),
        limit,
        p.tags.as_deref(),
        since_resolved.as_deref(),
        p.until.as_deref(),
        p.as_agent.as_deref(),
        p.budget_tokens,
        tier_resolved.as_deref(),
        p.has_citations.unwrap_or(false),
        p.source_uri_prefix.as_deref(),
    )
    .await
}

pub async fn recall_memories_post(
    State(app): State<AppState>,
    Json(body): Json<RecallBody>,
) -> impl IntoResponse {
    // Accept either `context` (canonical) or `query` (cert harness
    // alias used by S79). Reject only when both are missing/empty.
    let ctx_val = body.resolved_query();
    if ctx_val.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "context (or query) is required"})),
        )
            .into_response();
    }
    // Phase P6 (R1): `budget_tokens=0` is now a valid request — see
    // the matching note on the GET handler above.
    if let Some(ref a) = body.as_agent
        && let Err(e) = validate::validate_namespace(a)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid as_agent: {e}")})),
        )
            .into_response();
    }
    // v0.7.0 (issue #518) — see GET handler for the resolution rule.
    let (ns_resolved, since_resolved, tier_resolved, limit) = apply_recall_scope_defaults(
        &app,
        body.session_default,
        body.namespace.clone(),
        body.since.clone(),
        body.limit,
    );
    recall_response(
        &app,
        &ctx_val,
        ns_resolved.as_deref(),
        limit,
        body.tags.as_deref(),
        since_resolved.as_deref(),
        body.until.as_deref(),
        body.as_agent.as_deref(),
        body.budget_tokens,
        tier_resolved.as_deref(),
        body.has_citations.unwrap_or(false),
        body.source_uri_prefix.as_deref(),
    )
    .await
}

/// v0.6.2 (S18): shared HTTP recall implementation. Uses `db::recall_hybrid`
/// (semantic + FTS adaptive blend) when the embedder is loaded — matching
/// how the MCP `memory_recall` handler wires recall at src/mcp.rs:1157.
/// Gracefully falls back to `db::recall` (keyword-only) when the embedder
/// is not present or embedding the query fails. Closes the gap where the
/// HTTP surface was keyword-only regardless of server tier — scenario-18
/// surfaced the black-hole on peers that fanned out memories but never
/// exercised the semantic recall path.
///
/// v0.7.0 Wave-3 Continuation — when `app.storage_backend` is
/// `Postgres`, dispatch through `app.store.search` for keyword recall.
/// The full hybrid (FTS + semantic + adaptive blend + reranker + touch
/// ops) pipeline remains sqlite-only in v0.7.0; postgres deployments
/// fall back to keyword-only recall through the postgres `to_tsvector`
/// FTS surface, which is functionally equivalent for the keyword half
/// and surfaces a `mode=keyword` envelope so clients can detect the
/// degraded mode without an out-of-band feature probe.
#[allow(clippy::too_many_arguments)]
async fn recall_response(
    app: &AppState,
    context: &str,
    namespace: Option<&str>,
    limit: usize,
    tags: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    as_agent: Option<&str>,
    budget_tokens: Option<usize>,
    // v0.7.0 (issue #518) — spliced
    // `[agents.defaults.recall_scope].tier` when the caller passed
    // `session_default=true`. Applied on the postgres SAL path
    // (`Filter.tier`); ignored on the sqlite path because the legacy
    // `db::recall` / `db::recall_hybrid` functions do not expose a
    // tier filter parameter.
    recall_scope_tier: Option<&str>,
    // v0.7.0 Form 4 (issue #757) — fact-provenance post-filters.
    // Applied in Rust after the substrate-level recall returns so
    // the existing `db::recall` / `db::recall_hybrid` signatures
    // stay stable. Composes with every other filter.
    has_citations: bool,
    source_uri_prefix: Option<&str>,
) -> axum::response::Response {
    // `recall_scope_tier` is consumed only on the postgres SAL branch
    // (line 3026). Suppress the unused-variable lint when the sal
    // feature is off — same idiom as `url_was_synthesized` in
    // hook_subscribers.rs.
    #[cfg(not(feature = "sal"))]
    let _ = recall_scope_tier;
    // v0.7.0 Wave-3 Continuation 2 (Phase 10) — postgres-backed
    // hybrid recall via the SAL trait. Embeds the query AND dispatches
    // through `app.store.recall_hybrid` so the postgres adapter applies
    // the FTS + semantic + adaptive blend pipeline (mirror of
    // db::recall_hybrid in sqlite). Touch ops fire after the response
    // payload is assembled so access_count + TTL extension + auto-
    // promotion + priority ladders apply on postgres exactly as on
    // sqlite.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        // Embed the query before issuing the trait call. None when the
        // embedder is unavailable; the trait's recall_hybrid degrades
        // to the FTS-only pool with a synthetic semantic component.
        let query_emb: Option<Vec<f32>> = if let Some(emb) = app.embedder.as_ref().as_ref() {
            match emb.embed(context) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("recall (postgres): embed failed, keyword-only: {e}");
                    None
                }
            }
        } else {
            None
        };
        let mode = if query_emb.is_some() {
            "hybrid"
        } else {
            "keyword"
        };

        let ctx_caller =
            crate::store::CallerContext::for_agent(as_agent.unwrap_or("daemon").to_string());
        let mut filter = crate::store::Filter {
            namespace: namespace.map(str::to_string),
            limit,
            ..Default::default()
        };
        // v0.7.0 (issue #518) — splice `recall_scope.tier` when the
        // caller passed `session_default=true` and omitted an
        // explicit tier filter on the request. The HTTP recall
        // surface today carries no `tier` query parameter, so an
        // explicit-vs-default conflict cannot arise yet — the splice
        // is unconditional when present.
        if let Some(t) = recall_scope_tier
            && let Some(parsed) = crate::models::Tier::from_str(t)
        {
            filter.tier = Some(parsed);
        }
        if let Some(t) = tags {
            filter.tags_any = t
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
        if let Some(s) = since
            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s)
        {
            filter.since = Some(dt.into());
        }
        if let Some(u) = until
            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(u)
        {
            filter.until = Some(dt.into());
        }
        return match app
            .store
            .recall_hybrid(&ctx_caller, context, query_emb.as_deref(), &filter)
            .await
        {
            Ok(scored_pairs) => {
                // v0.7.0 Form 4 (issue #757) — fact-provenance post-filter
                // applies on the postgres SAL path too. Touch ops fire on
                // the FILTERED set so a memory the caller filtered out by
                // provenance does not leak through to the access_count
                // ladder.
                let scored_pairs = crate::cli::recall::apply_form4_recall_filters(
                    scored_pairs,
                    has_citations,
                    source_uri_prefix,
                );
                let touch_ids: Vec<String> =
                    scored_pairs.iter().map(|(m, _)| m.id.clone()).collect();
                let scored: Vec<serde_json::Value> = scored_pairs
                    .iter()
                    .map(|(m, s)| {
                        let mut v = serde_json::to_value(m).unwrap_or_default();
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert("score".to_string(), json!((*s * 1000.0).round() / 1000.0));
                        }
                        v
                    })
                    .collect();
                // Touch ops AFTER assembling the response payload so the
                // observable response is what the caller wanted (access_count
                // pre-touch); the touch fires inside the trait call's own
                // transaction.
                if let Err(e) = app.store.touch_after_recall(&touch_ids).await {
                    tracing::warn!("recall (postgres): touch_after_recall failed: {e}");
                }
                let mut resp = json!({
                    "memories": scored,
                    "count": scored.len(),
                    "tokens_used": 0,
                    "mode": mode,
                    "storage_backend": "postgres",
                });
                if let Some(b) = budget_tokens {
                    resp["budget_tokens"] = json!(b);
                }
                Json(resp).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    // Embed the query BEFORE grabbing the DB lock — embed() is CPU-heavy
    // and holding the SQLite mutex across it serialises unrelated writes.
    let query_emb: Option<Vec<f32>> = if let Some(emb) = app.embedder.as_ref().as_ref() {
        match emb.embed(context) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("recall: embedder query failed, falling back to keyword-only: {e}");
                None
            }
        }
    } else {
        None
    };

    let lock = app.db.lock().await;
    let short_extend = lock.2.short_extend_secs;
    let mid_extend = lock.2.mid_extend_secs;

    let (result, mode) = if let Some(ref qe) = query_emb {
        let vi_guard = app.vector_index.lock().await;
        let vi_ref = vi_guard.as_ref();
        let r = db::recall_hybrid(
            &lock.0,
            context,
            qe,
            namespace,
            limit,
            tags,
            since,
            until,
            vi_ref,
            short_extend,
            mid_extend,
            as_agent,
            budget_tokens,
            app.scoring.as_ref(),
            false,
        );
        drop(vi_guard);
        (r, "hybrid")
    } else {
        let r = db::recall(
            &lock.0,
            context,
            namespace,
            limit,
            tags,
            since,
            until,
            short_extend,
            mid_extend,
            as_agent,
            budget_tokens,
            false,
        );
        (r, "keyword")
    };

    match result {
        Ok((r, outcome)) => {
            // v0.7.0 Form 4 (issue #757) — fact-provenance post-filter.
            let r =
                crate::cli::recall::apply_form4_recall_filters(r, has_citations, source_uri_prefix);
            let scored: Vec<serde_json::Value> = r
                .iter()
                .map(|(m, s)| {
                    let mut v = serde_json::to_value(m).unwrap_or_default();
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("score".to_string(), json!((*s * 1000.0).round() / 1000.0));
                    }
                    v
                })
                .collect();
            let mut resp = json!({
                "memories": scored,
                "count": scored.len(),
                "tokens_used": outcome.tokens_used,
                "mode": mode,
            });
            if let Some(b) = budget_tokens {
                resp["budget_tokens"] = json!(b);
                // Phase P6 (R1) meta block — same shape as the MCP path.
                resp["meta"] = json!({
                    "budget_tokens_used": outcome.tokens_used,
                    "budget_tokens_remaining": outcome.tokens_remaining.unwrap_or(0),
                    "memories_dropped": outcome.memories_dropped,
                    "budget_overflow": outcome.budget_overflow,
                });
            }
            Json(resp).into_response()
        }
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn forget_memories(
    State(app): State<AppState>,
    Json(body): Json<ForgetQuery>,
) -> impl IntoResponse {
    // v0.7.0 Wave-3 Continuation 3 (Phase 13) — route through SAL trait
    // on postgres-backed daemons. Sqlite-backed daemons keep the legacy
    // `db::forget` free-function path verbatim.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let archive_flag = {
            let lock = app.db.lock().await;
            lock.3
        };
        let ctx = crate::store::CallerContext::for_agent("http");
        return match app
            .store
            .forget(
                &ctx,
                body.namespace.as_deref(),
                body.pattern.as_deref(),
                body.tier.as_ref(),
                archive_flag,
            )
            .await
        {
            Ok(n) => Json(json!({"deleted": n})).into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::forget(
        &lock.0,
        body.namespace.as_deref(),
        body.pattern.as_deref(),
        body.tier.as_ref(),
        lock.3, // archive_on_gc
    ) {
        Ok(n) => Json(json!({"deleted": n})).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct ContradictionsQuery {
    /// Topic to group candidate memories by. Resolved via (in order):
    /// `metadata.topic` exact match, then `title` exact match, then FTS
    /// content substring. At least one of `topic` or `namespace` is required.
    pub topic: Option<String>,
    /// Namespace to scope the search. Optional — default is cross-namespace.
    pub namespace: Option<String>,
    /// Pagination cap. Defaults to 50, hard max 200.
    pub limit: Option<usize>,
}

/// HTTP handler for v0.6.0.1 issue #321 — surfaces contradiction candidates
/// over the same REST surface scenarios use, so a2a-gate scenario-6 and any
/// future federation-level contradiction probe don't have to go through the
/// MCP stdio path.
///
/// Returns `{memories, links}` where:
/// - `memories` are the candidates grouped by topic/title (respecting the
///   UPSERT (title, namespace) invariant: if writers collided, only the LWW
///   survivor is returned — callers should use distinct titles per writer).
/// - `links` includes any existing `contradicts` rows from the `memory_links`
///   table PLUS a heuristic synthesis: when ≥2 candidates share a topic/title
///   but have materially different content, emit a synthetic `contradicts`
///   relation between each pair. The synthesized links carry
///   `relation:"contradicts"` and a `synthesized:true` flag so callers can
///   distinguish them from LLM-detected or operator-authored links.
///
/// Heuristic-only intentionally — LLM-backed detection (the existing MCP
/// `memory_detect_contradiction` tool) stays MCP-scoped so the HTTP surface
/// has no runtime LLM dependency. A follow-up issue can add opt-in LLM
/// resolution when `config.tier == Smart | Autonomous`.
#[allow(clippy::too_many_lines)]
pub async fn detect_contradictions(
    State(app): State<AppState>,
    Query(q): Query<ContradictionsQuery>,
) -> impl IntoResponse {
    if q.topic.is_none() && q.namespace.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "at least one of `topic` or `namespace` is required"})),
        )
            .into_response();
    }
    if let Some(ref ns) = q.namespace
        && let Err(e) = validate::validate_namespace(ns)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    // v0.6.2 (S40): raise to `MAX_BULK_SIZE` so a detect-contradictions
    // sweep over a bulk-populated namespace isn't silently capped at 200.
    let limit = q.limit.unwrap_or(50).min(MAX_BULK_SIZE);

    // v0.7.0 Wave-3 Continuation 3 (Phase 15) — postgres-backed daemons
    // route through the SAL trait. The non-LLM (rule-based +
    // heuristic-pairwise) contradictions detector works on both backends
    // because it's purely metadata-driven; this branch lists candidates
    // through `app.store.list` then runs the same pairwise heuristic.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("http");
        let filter = crate::store::Filter {
            namespace: q.namespace.clone(),
            limit,
            ..Default::default()
        };
        let all = match app.store.list(&ctx, &filter).await {
            Ok(v) => v,
            Err(e) => return store_err_to_response(e),
        };
        let candidates: Vec<Memory> = match q.topic.as_deref() {
            Some(t) => all
                .into_iter()
                .filter(|m| {
                    m.metadata
                        .get("topic")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| s == t)
                        || m.title == t
                })
                .collect(),
            None => all,
        };
        // Existing contradicts links via SAL — list all then filter by
        // (source ∈ candidates ∧ target ∈ candidates ∧ relation contains
        // "contradict"). We could narrow `list_links` by namespace when
        // q.namespace is set; for cross-namespace topic queries we need
        // the full set anyway.
        let candidate_ids: std::collections::HashSet<String> =
            candidates.iter().map(|m| m.id.clone()).collect();
        let mut existing_links: Vec<serde_json::Value> = Vec::new();
        if let Ok(all_links) = app.store.list_links(q.namespace.as_deref()).await {
            for link in all_links {
                // v0.7.0 fix campaign R1-M4 — relation is now typed.
                // Historic substring match tightened to a precise
                // variant compare.
                if matches!(
                    link.relation,
                    crate::models::MemoryLinkRelation::Contradicts
                ) && candidate_ids.contains(&link.source_id)
                    && candidate_ids.contains(&link.target_id)
                {
                    existing_links.push(json!({
                        "source_id": link.source_id,
                        "target_id": link.target_id,
                        "relation": link.relation,
                        "synthesized": false,
                    }));
                }
            }
        }
        existing_links.sort_by_key(|v| {
            (
                v.get("source_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("target_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("relation")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
            )
        });
        existing_links.dedup_by_key(|v| {
            (
                v.get("source_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("target_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("relation")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
            )
        });
        let mut synth_links: Vec<serde_json::Value> = Vec::new();
        for (i, a) in candidates.iter().enumerate() {
            for b in candidates.iter().skip(i + 1) {
                let same_topic = match q.topic.as_deref() {
                    Some(_) => true,
                    None => a.title == b.title,
                };
                if same_topic && a.content != b.content && a.id != b.id {
                    synth_links.push(json!({
                        "source_id": a.id,
                        "target_id": b.id,
                        "relation": "contradicts",
                        "synthesized": true,
                    }));
                }
            }
        }
        let mut links = existing_links;
        links.extend(synth_links);
        return Json(json!({
            "memories": candidates,
            "links": links,
            "storage_backend": "postgres",
        }))
        .into_response();
    }

    let lock = app.db.lock().await;
    let all = match db::list(
        &lock.0,
        q.namespace.as_deref(),
        None,
        limit,
        0,
        None,
        None,
        None,
        None,
        None,
    ) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("detect_contradictions list error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };

    // Topic match: metadata.topic == topic OR title == topic. Kept as a
    // retained filter rather than pushing to SQL because metadata is JSON
    // and the match predicate may evolve.
    let candidates: Vec<Memory> = match q.topic.as_deref() {
        Some(t) => all
            .into_iter()
            .filter(|m| {
                m.metadata
                    .get("topic")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s == t)
                    || m.title == t
            })
            .collect(),
        None => all,
    };

    // Existing contradicts links involving any candidate.
    let candidate_ids: std::collections::HashSet<String> =
        candidates.iter().map(|m| m.id.clone()).collect();
    let mut existing_links: Vec<serde_json::Value> = Vec::new();
    for id in &candidate_ids {
        if let Ok(links) = db::get_links(&lock.0, id) {
            for link in links {
                // v0.7.0 fix campaign R1-M4 — relation is now typed.
                // The historic substring match on "contradict" is
                // tightened to a precise variant compare.
                if matches!(
                    link.relation,
                    crate::models::MemoryLinkRelation::Contradicts
                ) && candidate_ids.contains(&link.source_id)
                    && candidate_ids.contains(&link.target_id)
                {
                    existing_links.push(json!({
                        "source_id": link.source_id,
                        "target_id": link.target_id,
                        "relation": link.relation,
                        "synthesized": false,
                    }));
                }
            }
        }
    }
    // Dedup — each (source,target,relation) appears at most once.
    existing_links.sort_by_key(|v| {
        (
            v.get("source_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            v.get("target_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            v.get("relation")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
        )
    });
    existing_links.dedup_by_key(|v| {
        (
            v.get("source_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            v.get("target_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            v.get("relation")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
        )
    });

    // Heuristic: when ≥2 candidates share a topic/title but content
    // differs, synthesize pairwise contradicts links. Marked
    // synthesized:true so callers can treat operator-authored links as
    // higher-confidence than this fallback.
    let mut synth_links: Vec<serde_json::Value> = Vec::new();
    for (i, a) in candidates.iter().enumerate() {
        for b in candidates.iter().skip(i + 1) {
            let same_topic = match q.topic.as_deref() {
                Some(_) => true,
                None => a.title == b.title,
            };
            if same_topic && a.content != b.content && a.id != b.id {
                synth_links.push(json!({
                    "source_id": a.id,
                    "target_id": b.id,
                    "relation": "contradicts",
                    "synthesized": true,
                }));
            }
        }
    }

    let mut links = existing_links;
    links.extend(synth_links);

    Json(json!({
        "memories": candidates,
        "links": links,
    }))
    .into_response()
}

pub async fn list_namespaces(State(app): State<AppState>) -> impl IntoResponse {
    // v0.7.0 Wave-3 Continuation — postgres-backed daemons aggregate the
    // distinct namespaces from `memories` via the SAL `list` method.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("daemon");
        let filter = crate::store::Filter {
            limit: 1_000_000,
            ..Default::default()
        };
        return match app.store.list(&ctx, &filter).await {
            Ok(memories) => {
                let mut ns: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
                for m in memories {
                    ns.insert(m.namespace);
                }
                let v: Vec<String> = ns.into_iter().collect();
                Json(json!({"namespaces": v})).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::list_namespaces(&lock.0) {
        Ok(ns) => Json(json!({"namespaces": ns})).into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// Query parameters for `GET /api/v1/taxonomy` (Pillar 1 / Stream A).
#[derive(Debug, Deserialize)]
pub struct TaxonomyQuery {
    /// Restrict to memories at this namespace OR any descendant. Trailing
    /// `/` is tolerated. Omit to walk the whole tree.
    pub prefix: Option<String>,
    /// Alias for `prefix` — the cert harness (S44) uses `?root=…`. Both
    /// forms route to the same code path; `prefix` wins when both are
    /// supplied.
    #[serde(default)]
    pub root: Option<String>,
    /// Max levels to descend below the prefix (defaults to 8 — the
    /// hierarchy hard cap).
    pub depth: Option<usize>,
    /// Cap on the number of `(namespace, count)` rows we walk into the
    /// tree. Densest namespaces win when truncated. Defaults to 1000.
    pub limit: Option<usize>,
}

/// `GET /api/v1/taxonomy` — REST mirror of the MCP `memory_get_taxonomy`
/// tool. Returns the prefix's hierarchical tree with per-node and
/// subtree counts, plus an honest `total_count` and a `truncated`
/// flag when `limit` dropped rows from the walk.
pub async fn get_taxonomy(
    State(app): State<AppState>,
    Query(p): Query<TaxonomyQuery>,
) -> impl IntoResponse {
    let prefix_owned: Option<String> = p
        .prefix
        .as_deref()
        .or(p.root.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string());
    if let Some(pref) = prefix_owned.as_deref()
        && let Err(e) = validate::validate_namespace(pref)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid namespace_prefix: {e}")})),
        )
            .into_response();
    }
    let depth = p
        .depth
        .unwrap_or(crate::models::MAX_NAMESPACE_DEPTH)
        .min(crate::models::MAX_NAMESPACE_DEPTH);
    let limit = p.limit.unwrap_or(1000).clamp(1, 10_000);

    // v0.7.0 Wave-3 Continuation 4 (Bucket E / S44) — full hierarchical
    // taxonomy walk for postgres-backed daemons. Uses
    // `taxonomy_namespaces_via_store` to project a single `GROUP BY
    // namespace` aggregate (so we don't pull every memory row into
    // memory), then assembles the hierarchical tree with honest
    // `subtree_count` so the cert oracle can detect dishonest
    // truncation.
    #[cfg(feature = "sal-postgres")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let pairs = match crate::store::postgres::taxonomy_namespaces_via_store(
            &app.store,
            prefix_owned.as_deref(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => return store_err_to_response(e),
        };
        // Collapse the SQL-aggregated `(namespace, count)` rows into a
        // hierarchical tree whose nodes carry both their direct
        // `count` (memories whose namespace exactly matches this node)
        // and the transitive `subtree_count` (sum across the node and
        // all descendants).
        let total_count: usize = pairs
            .iter()
            .map(|(_, c)| usize::try_from(*c).unwrap_or(0))
            .sum();

        // Node:
        //   key = full namespace path
        //   own_count = memories at this exact namespace
        //   subtree_count = own_count + sum over descendant subtree_counts
        // Build by ensuring every ancestor node exists (own_count = 0
        // for synthesised intermediates), then accumulating subtree
        // counts bottom-up via stable iteration.
        let mut nodes: std::collections::BTreeMap<String, (usize /* own */, usize /* subtree */)> =
            std::collections::BTreeMap::new();
        for (ns, cnt) in &pairs {
            let cnt_us = usize::try_from(*cnt).unwrap_or(0);
            // Ensure each prefix-segment ancestor exists (above prefix_owned
            // if any). For example, namespace `a/b/c/d` under prefix `a/b`
            // creates nodes for `a/b/c` and `a/b/c/d`.
            let segments: Vec<&str> = ns.split('/').collect();
            for i in 1..=segments.len() {
                let path = segments[..i].join("/");
                nodes.entry(path).or_insert((0, 0));
            }
            // Stamp own_count on the leaf node.
            nodes
                .entry(ns.clone())
                .and_modify(|v| v.0 = cnt_us)
                .or_insert((cnt_us, 0));
        }
        // Compute subtree_count: walk paths longest-first so children
        // are summed before their parents. Since BTreeMap orders by
        // string, walk in reverse-sorted order.
        // First pass: seed each node's subtree_count = own_count.
        for (_k, v) in nodes.iter_mut() {
            v.1 = v.0;
        }
        // Second pass: collect parent->child pairs, then accumulate.
        let keys: Vec<String> = nodes.keys().cloned().collect();
        for k in keys.iter().rev() {
            // Find immediate parent by trimming trailing `/segment`.
            if let Some(pos) = k.rfind('/') {
                let parent = &k[..pos];
                if let Some(parent_node) = nodes.get(parent).copied() {
                    let child_subtree = nodes.get(k).map(|v| v.1).unwrap_or(0);
                    if let Some(p) = nodes.get_mut(parent) {
                        p.1 = parent_node.1 + child_subtree;
                    }
                }
            }
        }

        // Project the prefix-rooted tree at the requested depth. When
        // no prefix is supplied, treat the synthesized "" root as the
        // top of the world; otherwise root the tree at prefix_owned.
        let root_ns = prefix_owned.clone().unwrap_or_default();
        let truncated = pairs.len() > limit;

        // Recursive node builder. `current_depth` counts levels below
        // root_ns (root_ns is depth 0). We bound the recursion by
        // `depth` to mirror the v0.6.3 SQLite contract.
        fn build_node(
            node_ns: &str,
            nodes: &std::collections::BTreeMap<String, (usize, usize)>,
            depth_left: usize,
        ) -> serde_json::Value {
            let (own, subtree) = nodes.get(node_ns).copied().unwrap_or((0, 0));
            let mut children: Vec<serde_json::Value> = Vec::new();
            if depth_left > 0 {
                // A child is any node whose namespace starts with
                // `<node_ns>/` AND has exactly one extra segment.
                let prefix_match = if node_ns.is_empty() {
                    String::new()
                } else {
                    format!("{node_ns}/")
                };
                let parent_segs = if node_ns.is_empty() {
                    0
                } else {
                    node_ns.split('/').count()
                };
                for k in nodes.keys() {
                    if k == node_ns {
                        continue;
                    }
                    if !node_ns.is_empty() && !k.starts_with(&prefix_match) {
                        continue;
                    }
                    if k.split('/').count() == parent_segs + 1 {
                        children.push(build_node(k, nodes, depth_left - 1));
                    }
                }
            }
            serde_json::json!({
                "namespace": node_ns,
                "count": own,
                "subtree_count": subtree,
                "children": children,
            })
        }
        let root_node = build_node(&root_ns, &nodes, depth);
        return Json(json!({
            "tree": root_node,
            "total_count": total_count,
            "truncated": truncated,
            "storage_backend": "postgres",
        }))
        .into_response();
    }

    // Suppress unused-warning when sal feature is enabled (prefix_owned moves above).
    let _ = depth;

    let lock = app.db.lock().await;
    match db::get_taxonomy(&lock.0, prefix_owned.as_deref(), depth, limit) {
        Ok(tax) => Json(json!({
            "tree": tax.tree,
            "total_count": tax.total_count,
            "truncated": tax.truncated,
        }))
        .into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// Request body for `POST /api/v1/check_duplicate` (Pillar 2 / Stream D).
#[derive(Debug, Deserialize)]
pub struct CheckDuplicateBody {
    pub title: String,
    pub content: String,
    /// Restrict the duplicate scan to this namespace. Omit to scan all
    /// namespaces.
    pub namespace: Option<String>,
    /// Cosine similarity threshold for declaring a duplicate. Clamped
    /// to >= 0.5 inside `db::check_duplicate`. Defaults to the tuned
    /// `DUPLICATE_THRESHOLD_DEFAULT` when omitted.
    pub threshold: Option<f32>,
}

/// `POST /api/v1/check_duplicate` — REST mirror of the MCP
/// `memory_check_duplicate` tool. Embeds `title + content`, scans
/// embedded live memories, and returns the highest-cosine match plus
/// `is_duplicate`/`suggested_merge` derived from the (clamped)
/// threshold.
pub async fn check_duplicate(
    State(app): State<AppState>,
    Json(body): Json<CheckDuplicateBody>,
) -> impl IntoResponse {
    if let Err(e) = validate::validate_title(&body.title) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid title: {e}")})),
        )
            .into_response();
    }
    if let Err(e) = validate::validate_content(&body.content) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid content: {e}")})),
        )
            .into_response();
    }
    let namespace = body
        .namespace
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(ns) = namespace
        && let Err(e) = validate::validate_namespace(ns)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid namespace: {e}")})),
        )
            .into_response();
    }
    let threshold = body.threshold.unwrap_or(db::DUPLICATE_THRESHOLD_DEFAULT);

    // v0.7.0 Wave-3 Continuation 4 (Bucket E / S48) — postgres-backed
    // daemons now perform an exact-content sweep through the SAL
    // `list` projection. When an embedder is loaded the call also
    // computes the query embedding and hands it to
    // `recall_hybrid`; the highest-cosine match becomes the nearest
    // candidate. Without an embedder the fallback walks the
    // namespace via `list` and surfaces any row whose
    // `(title, content)` tuple matches exactly (the same content-hash
    // short-circuit `db::check_duplicate_with_text` uses on sqlite,
    // before the embedding pass).
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("daemon");
        let filter = crate::store::Filter {
            namespace: namespace.map(str::to_string),
            limit: 1000,
            ..Default::default()
        };
        let mut nearest: Option<(crate::models::Memory, f64)> = None;
        let mut scanned = 0_u64;
        // Exact-content sweep first — cheap, deterministic, no embed.
        match app.store.list(&ctx, &filter).await {
            Ok(rows) => {
                for m in rows {
                    scanned += 1;
                    if m.content == body.content && m.title == body.title {
                        nearest = Some((m, 1.0));
                        break;
                    }
                }
            }
            Err(e) => return store_err_to_response(e),
        }
        // If exact match didn't surface, optionally try embedding-based
        // hybrid recall with the title+content as the query.
        if nearest.is_none()
            && let Some(emb) = app.embedder.as_ref().as_ref()
        {
            let embedding_text = format!("{} {}", body.title, body.content);
            if let Ok(qe) = emb.embed(&embedding_text) {
                let recall_filter = crate::store::Filter {
                    namespace: namespace.map(str::to_string),
                    limit: 5,
                    ..Default::default()
                };
                if let Ok(scored_pairs) = app
                    .store
                    .recall_hybrid(&ctx, &embedding_text, Some(&qe), &recall_filter)
                    .await
                {
                    if let Some((m, s)) = scored_pairs.into_iter().next() {
                        nearest = Some((m, s));
                    }
                }
                drop(qe);
            }
        }
        let (is_duplicate, near_json) = if let Some((m, score)) = nearest {
            let is_dup = score >= f64::from(threshold);
            (
                is_dup,
                json!({
                    "id": m.id,
                    "title": m.title,
                    "namespace": m.namespace,
                    "score": score,
                }),
            )
        } else {
            (false, serde_json::Value::Null)
        };
        return Json(json!({
            "is_duplicate": is_duplicate,
            "threshold": threshold,
            "nearest": near_json,
            "suggested_merge": is_duplicate,
            "candidates_scanned": scanned,
            "storage_backend": "postgres",
        }))
        .into_response();
    }

    // Embed before taking the DB lock — same rationale as create_memory
    // (issue #219). The embedder call is 10-200ms; we don't want it
    // serialised behind the connection mutex.
    let embedding_text = format!("{} {}", body.title, body.content);
    let query_embedding = match app.embedder.as_ref().as_ref() {
        Some(emb) => match emb.embed(&embedding_text) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("embedding generation failed: {e}");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"error": "embedder failed to encode input"})),
                )
                    .into_response();
            }
        },
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "memory_check_duplicate requires the embedder; daemon must be started with semantic tier or above"
                })),
            )
                .into_response();
        }
    };

    let lock = app.db.lock().await;
    // Round-2 F18 — short-circuit on raw-content hash equality before
    // falling through to embedding cosine similarity (parity with MCP
    // path).
    let check = match db::check_duplicate_with_text(
        &lock.0,
        &query_embedding,
        &embedding_text,
        namespace,
        threshold,
    ) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("handler error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };

    let nearest_json = check.nearest.as_ref().map(|m| {
        json!({
            "id": m.id,
            "title": m.title,
            "namespace": m.namespace,
            "similarity": (m.similarity * 1000.0).round() / 1000.0,
        })
    });
    let suggested_merge = if check.is_duplicate {
        check.nearest.as_ref().map(|m| m.id.clone())
    } else {
        None
    };

    Json(json!({
        "is_duplicate": check.is_duplicate,
        "threshold": check.threshold,
        "nearest": nearest_json,
        "suggested_merge": suggested_merge,
        "candidates_scanned": check.candidates_scanned,
    }))
    .into_response()
}

/// Request body for `POST /api/v1/entities` (Pillar 2 / Stream B).
#[derive(Debug, Deserialize)]
pub struct EntityRegisterBody {
    pub canonical_name: String,
    pub namespace: String,
    /// Aliases that should resolve to this entity. Blanks are skipped;
    /// duplicates collapse via `entity_aliases`'s primary key.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Arbitrary metadata to merge onto the entity memory. `kind` is
    /// always overwritten with `"entity"`.
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// Override the resolved NHI for this request's
    /// `metadata.agent_id`. Falls back to the `X-Agent-Id` header
    /// when omitted.
    pub agent_id: Option<String>,
}

/// Query parameters for `GET /api/v1/entities/by_alias` (Pillar 2 /
/// Stream B).
#[derive(Debug, Deserialize)]
pub struct EntityByAliasQuery {
    pub alias: String,
    pub namespace: Option<String>,
}

/// `POST /api/v1/entities` — REST mirror of the MCP
/// `memory_entity_register` tool. Idempotent on
/// `(canonical_name, namespace)`; merges aliases on re-registration.
pub async fn entity_register(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<EntityRegisterBody>,
) -> impl IntoResponse {
    if let Err(e) = validate::validate_title(&body.canonical_name) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid canonical_name: {e}")})),
        )
            .into_response();
    }
    if let Err(e) = validate::validate_namespace(&body.namespace) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid namespace: {e}")})),
        )
            .into_response();
    }

    let agent_id = body
        .agent_id
        .as_deref()
        .or_else(|| headers.get("x-agent-id").and_then(|v| v.to_str().ok()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(aid) = agent_id.as_deref()
        && let Err(e) = validate::validate_agent_id(aid)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid agent_id: {e}")})),
        )
            .into_response();
    }

    let extra_metadata = if body.metadata.is_object() {
        body.metadata.clone()
    } else {
        json!({})
    };

    // v0.7.0 Wave-3 Continuation — postgres-backed daemons register
    // the entity as a regular memory (title = canonical_name,
    // namespace = body.namespace, kind=entity in metadata) via the
    // SAL `store` method. The wire shape mirrors the SQLite path.
    //
    // v0.7.0 Wave-3 Continuation 4 (Bucket E / S47) — alias-union
    // persistence on re-register. The SAL `store` method upserts on
    // `(title, namespace)`, but a naive overwrite of `metadata.aliases`
    // erases any aliases registered previously. To preserve the
    // canonical SQLite contract (`db::entity_register` unions aliases
    // across registrations), we first list any matching entity row and
    // union its prior aliases into the incoming set before the upsert.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let aid = agent_id
            .clone()
            .unwrap_or_else(|| "anonymous:entity-register".to_string());
        let ctx = crate::store::CallerContext::for_agent(aid.clone());

        // Pull the prior entity row, if any, so we can union aliases
        // across registrations. This is a single namespace-scoped
        // `list` plus an in-memory match by canonical_name; the data
        // volume per namespace is small (entities rather than memories
        // proper) so the linear scan is acceptable.
        let prior_aliases: Vec<String> = {
            let filter = crate::store::Filter {
                namespace: Some(body.namespace.clone()),
                limit: 10_000,
                ..Default::default()
            };
            match app.store.list(&ctx, &filter).await {
                Ok(rows) => rows
                    .into_iter()
                    .find(|m| {
                        m.title == body.canonical_name
                            && m.metadata.get("kind").and_then(|v| v.as_str()) == Some("entity")
                    })
                    .and_then(|m| {
                        m.metadata
                            .get("aliases")
                            .and_then(|v| v.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str().map(str::to_string))
                                    .collect()
                            })
                    })
                    .unwrap_or_default(),
                Err(_) => Vec::new(),
            }
        };

        // Union: preserve insertion order (prior first, then new),
        // de-dup case-sensitively to match `db::entity_register`.
        let mut union: Vec<String> = Vec::new();
        for a in prior_aliases.iter().chain(body.aliases.iter()) {
            if !union.iter().any(|x| x == a) {
                union.push(a.clone());
            }
        }

        let now = Utc::now().to_rfc3339();
        let mut metadata = extra_metadata.clone();
        let meta = metadata.as_object_mut().expect("verified above");
        meta.insert("kind".to_string(), json!("entity"));
        meta.insert("aliases".to_string(), json!(union.clone()));
        meta.insert("agent_id".to_string(), json!(aid));
        let mem = Memory {
            id: Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: body.namespace.clone(),
            title: body.canonical_name.clone(),
            content: format!(
                "Entity registration: {} (aliases: {})",
                body.canonical_name,
                union.join(", ")
            ),
            tags: vec!["entity".to_string()],
            priority: 5,
            confidence: 1.0,
            source: "entity-register".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata,
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        };
        // F-A2A1.5 (#705) — governance enforcement on the postgres
        // entity-register path. Mirrors the F-A2A1.2 delete/promote gates
        // and the Wave-3 Continuation 3 create_memory gate: entity rows
        // are governance-relevant writes (they upsert a `Memory` row in
        // the requested namespace), so the postgres branch must consult
        // `enforce_governance_action(Store, ...)` before the upsert. Deny
        // returns 403; Pending returns 202 + pending_id. Without this
        // gate, postgres-backed daemons silently allowed any caller to
        // register entities into namespaces governed by `write=owner` or
        // `write=approve` standards, defeating the same A2A surface
        // F-A2A1.2 closed for delete/promote and create_memory.
        {
            use crate::models::GovernanceDecision;
            let payload_for_pending = serde_json::to_value(&mem).unwrap_or_else(|_| json!({}));
            match app
                .store
                .enforce_governance_action(
                    crate::store::GovernedAction::Store,
                    &mem.namespace,
                    &aid,
                    None,
                    None,
                    &payload_for_pending,
                )
                .await
            {
                Ok(GovernanceDecision::Allow) => {}
                Ok(GovernanceDecision::Deny(reason)) => {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({
                            "error": format!("entity_register denied by governance: {reason}"),
                        })),
                    )
                        .into_response();
                }
                Ok(GovernanceDecision::Pending(pending_id)) => {
                    return (
                        StatusCode::ACCEPTED,
                        Json(json!({
                            "status": "pending",
                            "pending_id": pending_id,
                            "reason": "governance requires approval",
                            "action": "store",
                            "namespace": mem.namespace,
                            "storage_backend": "postgres",
                        })),
                    )
                        .into_response();
                }
                Err(e) => return store_err_to_response(e),
            }
        }

        let created = prior_aliases.is_empty();
        return match app.store.store(&ctx, &mem).await {
            Ok(id) => (
                if created {
                    StatusCode::CREATED
                } else {
                    StatusCode::OK
                },
                Json(json!({
                    "entity_id": id,
                    "canonical_name": body.canonical_name,
                    "namespace": body.namespace,
                    "aliases": union,
                    "created": created,
                })),
            )
                .into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::entity_register(
        &lock.0,
        &body.canonical_name,
        &body.namespace,
        &body.aliases,
        &extra_metadata,
        agent_id.as_deref(),
    ) {
        Ok(reg) => {
            let status = if reg.created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            (
                status,
                Json(json!({
                    "entity_id": reg.entity_id,
                    "canonical_name": reg.canonical_name,
                    "namespace": reg.namespace,
                    "aliases": reg.aliases,
                    "created": reg.created,
                })),
            )
                .into_response()
        }
        Err(e) => {
            // Title-collision errors carry a stable, recognisable
            // substring; surface them as 409 Conflict so callers can
            // distinguish a genuine name clash from internal failure.
            let msg = e.to_string();
            if msg.contains("non-entity memory") {
                return (StatusCode::CONFLICT, Json(json!({"error": msg}))).into_response();
            }
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// `GET /api/v1/entities/by_alias?alias=<>&namespace=<>` — REST mirror
/// of the MCP `memory_entity_get_by_alias` tool. Returns
/// `{ found: false, ... }` with HTTP 200 when no entity claims the
/// alias under the filter, so callers don't have to disambiguate
/// "no match" from a server error.
pub async fn entity_get_by_alias(
    State(app): State<AppState>,
    Query(p): Query<EntityByAliasQuery>,
) -> impl IntoResponse {
    let alias = p.alias.trim();
    if alias.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "alias is required"})),
        )
            .into_response();
    }
    let namespace = p
        .namespace
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(ns) = namespace
        && let Err(e) = validate::validate_namespace(ns)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid namespace: {e}")})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 Continuation — postgres-backed daemons walk the
    // namespace's `kind=entity` memories via the SAL `list` method
    // and match against `metadata.aliases` client-side.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("daemon");
        let filter = crate::store::Filter {
            namespace: namespace.map(str::to_string),
            limit: 1000,
            ..Default::default()
        };
        return match app.store.list(&ctx, &filter).await {
            Ok(memories) => {
                for m in &memories {
                    let Some(meta) = m.metadata.as_object() else {
                        continue;
                    };
                    let Some(kind) = meta.get("kind").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    if kind != "entity" {
                        continue;
                    }
                    let aliases: Vec<String> = meta
                        .get("aliases")
                        .and_then(|v| v.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    if aliases.iter().any(|a| a.eq_ignore_ascii_case(alias))
                        || m.title.eq_ignore_ascii_case(alias)
                    {
                        return Json(json!({
                            "found": true,
                            "entity_id": m.id,
                            "canonical_name": m.title,
                            "namespace": m.namespace,
                            "aliases": aliases,
                        }))
                        .into_response();
                    }
                }
                Json(json!({
                    "found": false,
                    "entity_id": null,
                    "canonical_name": null,
                    "namespace": null,
                    "aliases": [],
                }))
                .into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::entity_get_by_alias(&lock.0, alias, namespace) {
        Ok(Some(rec)) => Json(json!({
            "found": true,
            "entity_id": rec.entity_id,
            "canonical_name": rec.canonical_name,
            "namespace": rec.namespace,
            "aliases": rec.aliases,
        }))
        .into_response(),
        Ok(None) => Json(json!({
            "found": false,
            "entity_id": null,
            "canonical_name": null,
            "namespace": null,
            "aliases": [],
        }))
        .into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// Query parameters for `GET /api/v1/kg/timeline` (Pillar 2 / Stream C).
#[derive(Debug, Deserialize)]
pub struct KgTimelineQuery {
    pub source_id: String,
    pub since: Option<String>,
    pub until: Option<String>,
    pub limit: Option<usize>,
}

/// `GET /api/v1/kg/timeline?source_id=<>&since=<>&until=<>&limit=<>` —
/// REST mirror of the MCP `memory_kg_timeline` tool. Returns outbound
/// link assertions from `source_id` ordered by `valid_from ASC`.
pub async fn kg_timeline(
    State(app): State<AppState>,
    Query(p): Query<KgTimelineQuery>,
) -> impl IntoResponse {
    if let Err(e) = validate::validate_id(&p.source_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid source_id: {e}")})),
        )
            .into_response();
    }
    let since = p.since.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let until = p.until.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(s) = since
        && let Err(e) = validate::validate_expires_at_format(s)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid since: {e}")})),
        )
            .into_response();
    }
    if let Some(u) = until
        && let Err(e) = validate::validate_expires_at_format(u)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid until: {e}")})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 Continuation — postgres dispatches via the
    // PostgresStore::kg_timeline helper. The adapter resolves AGE vs
    // CTE backend at connect time and projects rows in the shared
    // `KgTimelineRow` shape so the wire envelope stays parity-equal
    // to the SQLite path.
    #[cfg(feature = "sal-postgres")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let limit = p.limit;
        return match crate::store::postgres::kg_timeline_via_store(
            &app.store,
            &p.source_id,
            since,
            until,
            limit,
        )
        .await
        {
            Ok(events) => {
                let events_json: Vec<serde_json::Value> = events
                    .iter()
                    .map(|e| {
                        json!({
                            "target_id": e.target_id,
                            "relation": e.relation,
                            "valid_from": e.valid_from,
                            "valid_until": e.valid_until,
                            "observed_by": e.observed_by,
                            "title": e.title,
                            "target_namespace": e.target_namespace,
                        })
                    })
                    .collect();
                Json(json!({
                    "source_id": p.source_id,
                    "events": events_json,
                    "count": events.len(),
                }))
                .into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::kg_timeline(&lock.0, &p.source_id, since, until, p.limit) {
        Ok(events) => {
            let events_json: Vec<serde_json::Value> = events
                .iter()
                .map(|e| {
                    json!({
                        "target_id": e.target_id,
                        "relation": e.relation,
                        "valid_from": e.valid_from,
                        "valid_until": e.valid_until,
                        "observed_by": e.observed_by,
                        "title": e.title,
                        "target_namespace": e.target_namespace,
                    })
                })
                .collect();
            Json(json!({
                "source_id": p.source_id,
                "events": events_json,
                "count": events.len(),
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// JSON body for `POST /api/v1/kg/invalidate` (Pillar 2 / Stream C —
/// `memory_kg_invalidate`). The link is identified by its composite
/// key; `valid_until` defaults to wall-clock now when omitted.
#[derive(Debug, Deserialize)]
pub struct KgInvalidateBody {
    pub source_id: String,
    pub target_id: String,
    pub relation: String,
    pub valid_until: Option<String>,
}

/// `POST /api/v1/kg/invalidate` — REST mirror of `memory_kg_invalidate`.
/// 200 with `{found: true, …, previous_valid_until}` when the link
/// existed; 404 with `{found: false}` when no link matches the triple.
pub async fn kg_invalidate(
    State(app): State<AppState>,
    Json(body): Json<KgInvalidateBody>,
) -> impl IntoResponse {
    if let Err(e) = validate::validate_link(&body.source_id, &body.target_id, &body.relation) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let valid_until = body
        .valid_until
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(ts) = valid_until
        && let Err(e) = validate::validate_expires_at_format(ts)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid valid_until: {e}")})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 Continuation — postgres dispatches via the
    // PostgresStore::kg_invalidate helper.
    #[cfg(feature = "sal-postgres")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        return match crate::store::postgres::kg_invalidate_via_store(
            &app.store,
            &body.source_id,
            &body.target_id,
            &body.relation,
            valid_until,
        )
        .await
        {
            Ok(res) if res.found => (
                StatusCode::OK,
                Json(json!({
                    "found": true,
                    "source_id": body.source_id,
                    "target_id": body.target_id,
                    "relation": body.relation,
                    "valid_until": res.valid_until,
                    "previous_valid_until": res.previous_valid_until,
                })),
            )
                .into_response(),
            Ok(_) => (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "found": false,
                    "source_id": body.source_id,
                    "target_id": body.target_id,
                    "relation": body.relation,
                })),
            )
                .into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::invalidate_link(
        &lock.0,
        &body.source_id,
        &body.target_id,
        &body.relation,
        valid_until,
    ) {
        Ok(Some(res)) => (
            StatusCode::OK,
            Json(json!({
                "found": true,
                "source_id": body.source_id,
                "target_id": body.target_id,
                "relation": body.relation,
                "valid_until": res.valid_until,
                "previous_valid_until": res.previous_valid_until,
            })),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "found": false,
                "source_id": body.source_id,
                "target_id": body.target_id,
                "relation": body.relation,
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

// ============================================================================
// v0.7.0 Wave-3 Continuation 6 — three REST endpoints closing F7 cert-harness
// gaps (S52 `links/verify`, S61 `quota/status`, S65 `kg/find_paths`).
// ============================================================================

/// JSON body for `POST /api/v1/quota/status`.
///
/// `agent_id` is required when the caller wants a single-agent
/// snapshot; omitting it returns the full table (operator surface).
/// `namespace` is accepted for forward-compat — quotas today are
/// agent-scoped, but the wire shape leaves room for namespace-scoped
/// caps in a future wave.
#[derive(Debug, Deserialize)]
pub struct QuotaStatusBody {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
}

/// `POST /api/v1/quota/status` — read the agent's quota row, or the
/// full table when `agent_id` is omitted. Returns the canonical
/// `QuotaStatus` JSON projection.
///
/// Dispatches via `app.store.quota_status(agent_id)` so postgres-backed
/// daemons read from the postgres `agent_quotas` table rather than the
/// scratch sqlite connection.
pub async fn quota_status_handler(
    State(app): State<AppState>,
    Json(body): Json<QuotaStatusBody>,
) -> impl IntoResponse {
    if let Some(agent_id) = body.agent_id.as_deref() {
        if let Err(e) = validate::validate_agent_id(agent_id) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid agent_id: {e}")})),
            )
                .into_response();
        }

        // Postgres-backed daemons MUST take the SAL trait dispatch — the
        // scratch sqlite connection at `app.db` has no `agent_quotas`
        // rows.
        #[cfg(feature = "sal")]
        if matches!(app.storage_backend, StorageBackend::Postgres) {
            return match app.store.quota_status(agent_id).await {
                Ok(status) => Json(json!(status)).into_response(),
                Err(e) => store_err_to_response(e),
            };
        }

        let lock = app.db.lock().await;
        return match crate::quotas::get_status(&lock.0, agent_id) {
            Ok(status) => Json(json!(status)).into_response(),
            Err(e) => {
                tracing::error!("quota_status handler error: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal server error"})),
                )
                    .into_response()
            }
        };
    }

    // No agent_id supplied — operator-facing list path.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        return match app.store.quota_status_list().await {
            Ok(rows) => Json(json!({"quotas": rows, "count": rows.len()})).into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match crate::quotas::list_status(&lock.0) {
        Ok(rows) => {
            let count = rows.len();
            Json(json!({"quotas": rows, "count": count})).into_response()
        }
        Err(e) => {
            tracing::error!("quota_status list handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// JSON body for `POST /api/v1/kg/find_paths`.
///
/// `source_id` + `target_id` are required. `max_depth` defaults to the
/// adapter's `FIND_PATHS_DEFAULT_DEPTH`; `max_results` clamps the
/// returned path count.
#[derive(Debug, Deserialize)]
pub struct FindPathsBody {
    pub source_id: String,
    pub target_id: String,
    #[serde(default)]
    pub max_depth: Option<usize>,
    #[serde(default)]
    pub max_results: Option<usize>,
}

/// `POST /api/v1/kg/find_paths` — enumerate up to N paths between two
/// memories. Wraps the SAL [`MemoryStore::find_paths`] surface so both
/// SQLite (recursive CTE) and Postgres (AGE Cypher / CTE fallback)
/// dispatch through the same handler.
///
/// Wire shape: `{paths: [[id, id, ...], ...], count}`. Each inner
/// array is the chain of memory ids from `source_id` to `target_id`,
/// inclusive.
pub async fn kg_find_paths(
    State(app): State<AppState>,
    Json(body): Json<FindPathsBody>,
) -> impl IntoResponse {
    if let Err(e) = validate::validate_id(&body.source_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid source_id: {e}")})),
        )
            .into_response();
    }
    if let Err(e) = validate::validate_id(&body.target_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid target_id: {e}")})),
        )
            .into_response();
    }

    #[cfg(feature = "sal")]
    {
        return match app
            .store
            .find_paths(
                &body.source_id,
                &body.target_id,
                body.max_depth,
                body.max_results,
            )
            .await
        {
            Ok(paths) => {
                if crate::audit::is_enabled() {
                    crate::audit::emit(crate::audit::EventBuilder::new(
                        crate::audit::AuditAction::Recall,
                        crate::audit::actor("ai:http", "http_body", None),
                        crate::audit::target_memory(
                            body.source_id.clone(),
                            String::new(),
                            Some(format!("find_paths -> {}", body.target_id)),
                            None,
                            None,
                        ),
                    ));
                }
                let count = paths.len();
                Json(json!({
                    "paths": paths,
                    "count": count,
                    "source_id": body.source_id,
                    "target_id": body.target_id,
                }))
                .into_response()
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("max_depth") || msg.contains("depth") {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({"error": msg})),
                    )
                        .into_response();
                }
                store_err_to_response(e)
            }
        };
    }

    #[cfg(not(feature = "sal"))]
    {
        let _ = app;
        let _ = body;
        (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "find_paths requires --features sal"})),
        )
            .into_response()
    }
}

/// JSON body for `POST /api/v1/links/verify`.
///
/// Either `source_id` (with optional `target_id`) OR `link_id` MUST be
/// supplied. `link_id` on every adapter is the canonical
/// `source_id|target_id|relation` triple — the trait does not expose a
/// rowid surface for links.
#[derive(Debug, Deserialize)]
pub struct VerifyLinkBody {
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub link_id: Option<String>,
    /// v0.7.0 H5 (round-2) — caller-supplied anti-replay nonce.
    /// Expected to be a fresh UUID v4 per verify call. The handler
    /// hashes `(canonical_link_id, verification_nonce)` into a 32-byte
    /// SHA-256 fingerprint and rejects exact-repeat tuples with 409
    /// Conflict. When `[verify] require_nonce = true` is set, missing
    /// nonces produce 400 Bad Request; otherwise a deprecation WARN
    /// is logged and the verify proceeds. See
    /// [`crate::identity::replay`] for the LRU memory bound.
    #[serde(default)]
    pub verification_nonce: Option<String>,
}

/// `POST /api/v1/links/verify` — re-verify a stored link's signature
/// (when present) and project the resolved attest level. Wire shape:
/// `{verified, attest_level, signature_present, observed_by, source_id,
/// target_id, relation, findings}`.
///
/// **v0.7.0 H5 (round-2)** — anti-replay surface. Every successful
/// verify gets a `(canonical_link_id, verification_nonce)` fingerprint
/// recorded in a bounded in-memory LRU (10 000 entries, ~512 KB
/// resident). Repeats produce 409 Conflict so a captured `verify_link`
/// request cannot be replayed indefinitely against the same daemon.
/// The LRU is per-process and per-replica — see
/// [`crate::identity::replay`] for the threat model.
pub async fn verify_link_handler(
    State(app): State<AppState>,
    Json(body): Json<VerifyLinkBody>,
) -> impl IntoResponse {
    if body.source_id.is_none() && body.link_id.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "verify_link requires either source_id or link_id",
                "fields": ["source_id", "link_id"],
            })),
        )
            .into_response();
    }
    if let Some(s) = body.source_id.as_deref()
        && let Err(e) = validate::validate_id(s)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid source_id: {e}")})),
        )
            .into_response();
    }
    if let Some(t) = body.target_id.as_deref()
        && let Err(e) = validate::validate_id(t)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid target_id: {e}")})),
        )
            .into_response();
    }

    // v0.7.0 H5 (round-2) — anti-replay gate. We treat an empty
    // string the same as missing — a `""` nonce trivially collides
    // with itself and would silently neuter the cache.
    let nonce_opt: Option<&str> = body.verification_nonce.as_deref().filter(|s| !s.is_empty());
    match (nonce_opt, app.verify_require_nonce) {
        (None, true) => {
            // Strict mode + missing nonce → 400. The wire shape includes
            // the offending field name so the client can fix the call.
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "verification_nonce is required when [verify] require_nonce = true",
                    "fields": ["verification_nonce"],
                })),
            )
                .into_response();
        }
        (None, false) => {
            // Back-compat mode: log a deprecation WARN and let the
            // verify proceed. Operators see this in journalctl and
            // can decide when to flip require_nonce on.
            tracing::warn!(
                target: "ai_memory::verify",
                "POST /api/v1/links/verify called without verification_nonce — \
                 replay protection is disabled for this request. Add a fresh \
                 UUID-v4 nonce to opt into H5 dedup; flip [verify] require_nonce = true \
                 to enforce."
            );
        }
        (Some(_), _) => {
            // Will be checked below after we know the canonical
            // link triple (so the fingerprint is stable regardless
            // of whether the request used `(source_id, target_id)`
            // or `link_id` to identify the row).
        }
    }

    #[cfg(feature = "sal")]
    {
        let filter = crate::store::VerifyFilter {
            source_id: body.source_id.clone(),
            target_id: body.target_id.clone(),
            link_id: body.link_id.clone(),
        };
        return match app.store.verify_link(filter).await {
            Ok(report) => {
                // H5: derive the canonical link_id from the resolved
                // triple. We use this rather than the request's
                // `link_id` (which may be unset) so the fingerprint
                // is stable across the two filter shapes a caller
                // might use. The signature bytes are not exposed via
                // VerifyLinkReport, so the fingerprint covers
                // `(canonical_id, nonce)` — sufficient to dedup an
                // exact replay of the same request without depending
                // on the trait surface exposing the raw signature.
                if let Some(nonce) = nonce_opt {
                    let canonical_id = format!(
                        "{}|{}|{}",
                        report.source_id, report.target_id, report.relation
                    );
                    // Empty bytes as the "signature" component —
                    // the resolved link's identity already binds the
                    // signature material via canonical_id.
                    let decision = app.replay_cache.record_and_check(&canonical_id, b"", nonce);
                    if matches!(decision, crate::identity::replay::ReplayDecision::Replay) {
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({"error": "verification replay detected"})),
                        )
                            .into_response();
                    }
                }
                if crate::audit::is_enabled() {
                    crate::audit::emit(crate::audit::EventBuilder::new(
                        crate::audit::AuditAction::Link,
                        crate::audit::actor("ai:http", "http_body", None),
                        crate::audit::target_memory(
                            report.source_id.clone(),
                            String::new(),
                            Some(format!(
                                "verify -> {} {}",
                                report.target_id, report.relation
                            )),
                            None,
                            None,
                        ),
                    ));
                }
                Json(json!(report)).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    #[cfg(not(feature = "sal"))]
    {
        let _ = app;
        let _ = body;
        (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "verify_link requires --features sal"})),
        )
            .into_response()
    }
}

/// JSON body for `POST /api/v1/kg/query` (Pillar 2 / Stream C —
/// `memory_kg_query`). POST is used because `allowed_agents` is a list;
/// keeping it in a body avoids over-long query strings and keeps the
/// surface symmetric with `POST /api/v1/kg/invalidate`. `max_depth`
/// defaults to 1 and is bounded by `KG_QUERY_MAX_SUPPORTED_DEPTH`.
#[derive(Debug, Deserialize)]
pub struct KgQueryBody {
    /// Canonical name. Aliased by `from` (S82's wire shape).
    #[serde(default)]
    pub source_id: Option<String>,
    /// `from` alias for `source_id` — the cert harness S82 uses
    /// `{from, to, max_depth, rel_types}`.
    #[serde(default)]
    pub from: Option<String>,
    /// Optional target id — when present the query is interpreted as
    /// a find-path between (`source_id`, `to`); kg_query's existing
    /// surface ignores it but accepting it keeps the wire shape
    /// flexible for the cert harness.
    #[serde(default)]
    pub to: Option<String>,
    pub max_depth: Option<usize>,
    pub valid_at: Option<String>,
    pub allowed_agents: Option<Vec<String>>,
    pub limit: Option<usize>,
    /// NHI-P3-T7 (v0.7.0 NHI testing): when omitted or false, the
    /// "current view" filter excludes edges whose `valid_until` lies
    /// in the past (invalidated via `memory_kg_invalidate`). Pass
    /// `true` to traverse the full historical link graph.
    #[serde(default)]
    pub include_invalidated: bool,
    /// Optional relation-type filter — accepted for forward-compat
    /// with the find_paths shape; unused on the current trait
    /// surface (CTE walks `:related_to` only).
    #[serde(default)]
    pub rel_types: Option<Vec<String>>,
}

/// `POST /api/v1/kg/query` — REST mirror of the MCP `memory_kg_query`
/// tool. Returns outbound multi-hop traversal from `source_id` (1..=5
/// hops) filtered by the temporal/agent windows. 400 for invalid
/// IDs/timestamps; 422 when `max_depth` exceeds the supported ceiling
/// (clearer than 500 for what is a documented limitation, not an
/// internal error).
pub async fn kg_query(
    State(app): State<AppState>,
    Json(body): Json<KgQueryBody>,
) -> impl IntoResponse {
    // S82's wire shape sends `from` instead of `source_id`; resolve
    // the canonical id from either field with `source_id` taking
    // precedence when both are supplied.
    let source_id = body
        .source_id
        .clone()
        .or_else(|| body.from.clone())
        .unwrap_or_default();
    if let Err(e) = validate::validate_id(&source_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid source_id: {e}")})),
        )
            .into_response();
    }
    let max_depth = body.max_depth.unwrap_or(1);
    let valid_at = body
        .valid_at
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(t) = valid_at
        && let Err(e) = validate::validate_expires_at_format(t)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid valid_at: {e}")})),
        )
            .into_response();
    }
    let allowed_agents: Option<Vec<String>> = body.allowed_agents.as_ref().map(|v| {
        v.iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });
    if let Some(agents) = allowed_agents.as_ref() {
        for a in agents {
            if let Err(e) = validate::validate_agent_id(a) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid allowed_agents entry: {e}")})),
                )
                    .into_response();
            }
        }
    }

    // v0.7.0 Wave-3 Continuation — postgres dispatches via the
    // PostgresStore::kg_query helper. Backend (AGE vs CTE) is
    // resolved at adapter connect time. Temporal/agent filters are
    // applied client-side post-traversal because the AGE Cypher
    // path returns the unfiltered topology — match the SQLite
    // recursive-CTE wire shape.
    #[cfg(feature = "sal-postgres")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        return match crate::store::postgres::kg_query_via_store(
            &app.store,
            &source_id,
            max_depth,
            body.include_invalidated,
        )
        .await
        {
            Ok(nodes) => {
                // S82's wire shape — when `to` is supplied, project a
                // single-path `paths` array of node-id chains so the
                // find-paths style consumer can read the result back
                // without a separate `find_paths` route.
                let memories_json: Vec<serde_json::Value> = nodes
                    .iter()
                    .map(|n| {
                        json!({
                            "target_id": n.target_id,
                            "relation": n.relation,
                            "depth": n.depth,
                            "path": n.path,
                        })
                    })
                    .collect();
                let mut paths_json: Vec<serde_json::Value> = Vec::new();
                if let Some(target) = body.to.as_deref() {
                    // Find the first traversal path that ends at `target`
                    // and project the chain as a list of node ids.
                    for n in &nodes {
                        if n.target_id == target {
                            let chain: Vec<String> =
                                n.path.split("->").map(str::to_string).collect();
                            paths_json.push(serde_json::Value::Array(
                                chain.into_iter().map(serde_json::Value::String).collect(),
                            ));
                            break;
                        }
                    }
                } else {
                    for n in &nodes {
                        paths_json.push(serde_json::Value::String(n.path.clone()));
                    }
                }
                Json(json!({
                    "source_id": source_id,
                    "max_depth": max_depth,
                    "memories": memories_json,
                    "paths": paths_json,
                    "count": nodes.len(),
                }))
                .into_response()
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("max_depth") || msg.contains("depth") {
                    (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(json!({"error": msg})),
                    )
                        .into_response()
                } else {
                    store_err_to_response(e)
                }
            }
        };
    }

    let lock = app.db.lock().await;
    match db::kg_query(
        &lock.0,
        &source_id,
        max_depth,
        valid_at,
        allowed_agents.as_deref(),
        body.limit,
        body.include_invalidated,
    ) {
        Ok(nodes) => {
            let memories_json: Vec<serde_json::Value> = nodes
                .iter()
                .map(|n| {
                    json!({
                        "target_id": n.target_id,
                        "relation": n.relation,
                        "valid_from": n.valid_from,
                        "valid_until": n.valid_until,
                        "observed_by": n.observed_by,
                        "title": n.title,
                        "target_namespace": n.target_namespace,
                        "depth": n.depth,
                        "path": n.path,
                    })
                })
                .collect();
            let paths_json: Vec<&str> = nodes.iter().map(|n| n.path.as_str()).collect();
            Json(json!({
                "source_id": source_id,
                "max_depth": max_depth,
                "memories": memories_json,
                "paths": paths_json,
                "count": nodes.len(),
            }))
            .into_response()
        }
        Err(e) => {
            // The `kg_query` DB layer raises explicit errors for
            // depth=0 and for max_depth past the supported ceiling;
            // those are caller-fixable, not server faults.
            let msg = e.to_string();
            if msg.contains("max_depth") {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"error": msg})),
                )
                    .into_response();
            }
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// v0.7.0 G-PHASE-E-1 (#706) — canonical field whitelist for the
/// `/api/v1/links` create + delete bodies. Includes the canonical names
/// and the S82 aliases (`from` / `to` / `rel_type`). Any field outside
/// this set surfaces as a structured `unknown_field` 400 rather than
/// being silently defaulted (the pre-#706 behaviour, where a typoed
/// `link_type` would land a link with `relation = "related_to"`).
const ALLOWED_LINK_BODY_FIELDS: &[&str] = &[
    "source_id",
    "from",
    "target_id",
    "to",
    "relation",
    "rel_type",
];

/// v0.7.0 G-PHASE-E-1 (#706) — closed set of relation values accepted
/// by the SQL CHECK constraint on `memory_links.relation` (migration
/// 0027). Used to surface a structured `invalid_relation` 400 from the
/// HTTP handler before the INSERT crashes with a generic CHECK error.
const ALLOWED_LINK_RELATIONS: &[&str] = &[
    "related_to",
    "supersedes",
    "contradicts",
    "derived_from",
    "reflects_on",
];

/// Return the list of unknown fields in `raw` against
/// [`ALLOWED_LINK_BODY_FIELDS`]. Returns `None` when every key is
/// recognised (including the empty-body case) so callers can use
/// `if let Some(unknown) = …` to branch cleanly into the 400 path.
fn unknown_link_body_fields(raw: &serde_json::Value) -> Option<Vec<String>> {
    let obj = raw.as_object()?;
    let mut unknown: Vec<String> = obj
        .keys()
        .filter(|k| !ALLOWED_LINK_BODY_FIELDS.contains(&k.as_str()))
        .cloned()
        .collect();
    if unknown.is_empty() {
        None
    } else {
        unknown.sort();
        Some(unknown)
    }
}

pub async fn create_link(
    State(app): State<AppState>,
    Json(raw): Json<serde_json::Value>,
) -> impl IntoResponse {
    // v0.7.0 G-PHASE-E-1 (#706) — reject unknown fields with a
    // structured 400 instead of silently defaulting `relation` to
    // `related_to`. The canonical shape is `{source_id|from,
    // target_id|to, relation|rel_type}`; anything else (e.g. the
    // common-typo `link_type`) is a caller bug that previously
    // surfaced as a silently-defaulted insert.
    if let Some(unknown) = unknown_link_body_fields(&raw) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "unknown_field", "fields": unknown})),
        )
            .into_response();
    }
    let body: LinkBody = match serde_json::from_value(raw) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    // S82's wire shape uses `{from, to, rel_type}`; resolve canonical
    // (source_id, target_id, relation) from either field set.
    let (source_id, target_id, relation) = body.resolved();
    if let Err(e) = validate::validate_link(&source_id, &target_id, &relation) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    // v0.7.0 G-PHASE-E-1 (#706) — the SQL-side CHECK constraint on
    // `memory_links.relation` (migration 0027) admits only the five
    // canonical relations. `validate_relation` is intentionally more
    // permissive (accepts arbitrary `[a-z0-9_]+`) for forward-compat,
    // but anything outside the canonical set will crash the INSERT
    // with a generic CHECK violation. Pre-flight the relation against
    // the closed set here so callers get a structured 400 instead of
    // a generic 500.
    if crate::models::MemoryLinkRelation::from_str(&relation).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_relation",
                "got": relation,
                "allowed": ALLOWED_LINK_RELATIONS,
            })),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 — Postgres-backed daemons take the SAL trait
    // dispatch path. The trait's `link_signed` returns the resolved
    // `attest_level` so the wire response carries the same byte shape
    // as the legacy `db::create_link_signed` path. Federation fanout
    // is omitted on the postgres branch — quorum-broadcast is still
    // SQLite-bound and lighting it up on Postgres is a follow-on
    // wave.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let now = Utc::now().to_rfc3339();
        // v0.7.0 fix campaign R1-M4 — wrap the wire String relation
        // into the typed `MemoryLinkRelation`. `validate_link` (above)
        // already vetted the relation against the closed set, so a
        // parse failure here would be a bug; fall back to the default
        // rather than 500 the request.
        let relation_typed =
            crate::models::MemoryLinkRelation::from_str(&relation).unwrap_or_default();
        let link = MemoryLink {
            source_id: source_id.clone(),
            target_id: target_id.clone(),
            relation: relation_typed,
            created_at: now,
            valid_from: None,
            valid_until: None,
            observed_by: None,
            signature: None,
        };
        let ctx = crate::store::CallerContext::for_agent("ai:http");
        return match app
            .store
            .link_signed(&ctx, &link, app.active_keypair.as_ref().as_ref())
            .await
        {
            Ok(attest_level) => {
                if crate::audit::is_enabled() {
                    crate::audit::emit(crate::audit::EventBuilder::new(
                        crate::audit::AuditAction::Link,
                        crate::audit::actor("ai:http", "http_body", None),
                        crate::audit::target_memory(
                            source_id.clone(),
                            String::new(),
                            Some(format!("{target_id} -> {relation}")),
                            None,
                            None,
                        ),
                    ));
                }
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "linked": true,
                        "source_id": source_id,
                        "target_id": target_id,
                        "relation": relation,
                        "attest_level": attest_level,
                    })),
                )
                    .into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    // v0.7 H2 — sign with the active keypair when one was loaded at
    // startup. Falls back to unsigned (signature NULL, attest_level
    // "unsigned") when no keypair is configured. Either way the chosen
    // attest level is surfaced on the wire response so callers can
    // observe whether their link was signed without re-querying.
    let create_result = db::create_link_signed(
        &lock.0,
        &source_id,
        &target_id,
        &relation,
        app.active_keypair.as_ref().as_ref(),
    );
    // v0.6.4-017 — G9 HTTP webhook parity. Fire `memory_link_created`
    // after db::create_link commits (mirrors mcp.rs:2569). The link
    // itself does not carry a namespace; we look up the source memory
    // for the namespace + owner agent_id so the event payload matches
    // the MCP contract.
    if create_result.is_ok() {
        let (link_namespace, link_owner) = db::get(&lock.0, &source_id).ok().flatten().map_or_else(
            || ("global".to_string(), None),
            |m| {
                let owner = m
                    .metadata
                    .get("agent_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                (m.namespace, owner)
            },
        );
        let details = serde_json::to_value(crate::subscriptions::LinkCreatedEventDetails {
            target_id: target_id.clone(),
            relation: relation.clone(),
        })
        .ok();
        crate::subscriptions::dispatch_event_with_details(
            &lock.0,
            "memory_link_created",
            &source_id,
            &link_namespace,
            link_owner.as_deref(),
            &lock.1,
            details,
        );
    }
    // Drop DB lock before fanning out — peers POST back to our sync_push
    // and we'd deadlock on the shared Mutex if we held it.
    drop(lock);
    match create_result {
        Ok(attest_level) => {
            // v0.6.2 (#325): propagate link to peers.
            if let Some(fed) = app.federation.as_ref() {
                // v0.7.0 fix campaign R1-M4 — `validate_link` already
                // gated `relation` against the closed set; the parse
                // here cannot fail in practice.
                let relation_typed =
                    crate::models::MemoryLinkRelation::from_str(&relation).unwrap_or_default();
                let link = crate::models::MemoryLink {
                    source_id: source_id.clone(),
                    target_id: target_id.clone(),
                    relation: relation_typed,
                    created_at: chrono::Utc::now().to_rfc3339(),
                    // H3 wire fields are populated by `export_links`
                    // on the next bulk re-sync; the immediate fanout
                    // path stays unsigned to avoid a redundant DB
                    // round-trip just to fish out the freshly-written
                    // signature row. Receivers will land this as
                    // `unsigned` until a periodic reconciliation pulls
                    // the signed row via `export_links`.
                    signature: None,
                    observed_by: None,
                    valid_from: None,
                    valid_until: None,
                };
                match crate::federation::broadcast_link_quorum(fed, &link).await {
                    Ok(tracker) => {
                        if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("Retry-After", "2")],
                                Json(serde_json::to_value(&payload).unwrap_or_default()),
                            )
                                .into_response();
                        }
                    }
                    Err(e) => {
                        tracing::warn!("link fanout error (local committed): {e:?}");
                    }
                }
            }
            // v0.7 H2 — surface attest_level on the wire so callers
            // can tell signed vs unsigned without re-querying.
            (
                StatusCode::CREATED,
                Json(json!({"linked": true, "attest_level": attest_level})),
            )
                .into_response()
        }
        Err(e) => {
            // v0.7.0 fix-campaign A3 (LINK-PARITY, #690) — map the
            // two new storage-layer refusals to their canonical HTTP
            // status codes. Cycle refusals are 409 CONFLICT (the
            // graph state conflicts with the new edge); K9 deny is
            // 403 FORBIDDEN. Anything else stays 500 — those are
            // server faults the caller cannot fix by retrying with
            // different inputs.
            let msg = e.to_string();
            if msg.starts_with(db::LINK_CYCLE_ERR_PREFIX) {
                return (StatusCode::CONFLICT, Json(json!({"error": msg}))).into_response();
            }
            if msg.starts_with(db::LINK_PERMISSION_DENIED_ERR_PREFIX) {
                return (StatusCode::FORBIDDEN, Json(json!({"error": msg}))).into_response();
            }
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// v0.6.2 (#325) — DELETE /api/v1/links. Removes the directional link
/// `source_id → target_id` locally. Deletion is NOT fanned out in v0.6.2:
/// the receiving-side API is `db::delete_link`, and `sync_push` does not
/// yet carry a link-tombstone list. Full link tombstones ship with v0.7
/// CRDT-lite. For current scenario coverage (scenario-11 tests create),
/// create-link fanout is sufficient.
pub async fn delete_link(
    State(app): State<AppState>,
    Json(raw): Json<serde_json::Value>,
) -> impl IntoResponse {
    // v0.7.0 G-PHASE-E-1 (#706) — mirror create_link: reject unknown
    // fields with a structured 400 so caller bugs surface loudly
    // instead of silently defaulting.
    if let Some(unknown) = unknown_link_body_fields(&raw) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "unknown_field", "fields": unknown})),
        )
            .into_response();
    }
    let body: LinkBody = match serde_json::from_value(raw) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    let (source_id, target_id, relation) = body.resolved();
    if let Err(e) = validate::validate_link(&source_id, &target_id, &relation) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let lock = app.db.lock().await;
    let delete_result = db::delete_link(&lock.0, &source_id, &target_id);
    drop(lock);
    match delete_result {
        Ok(removed) => Json(json!({"deleted": removed})).into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn get_links(State(app): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 — Postgres-backed daemons walk `list_links` (no
    // namespace filter — the same projection as the legacy
    // `db::get_links` returns) and narrow client-side to edges
    // anchored at `id`. The trait does not (yet) expose a per-anchor
    // edge probe; this filter is O(|edges|), which matches the
    // SQLite path's behaviour for typical workloads.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        return match app.store.list_links(None).await {
            Ok(rows) => {
                let edges: Vec<_> = rows
                    .into_iter()
                    .filter(|l| l.source_id == id || l.target_id == id)
                    .collect();
                Json(json!({"links": edges})).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::get_links(&lock.0, &id) {
        Ok(links) => Json(json!({"links": links})).into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn get_stats(State(app): State<AppState>) -> impl IntoResponse {
    // v0.7.0 Wave-3 Continuation — postgres-backed daemons project a
    // basic count from the SAL `list` method. Detailed per-tier
    // breakdown + DB file size + WAL counters are sqlite-only fields
    // and surface as `null` on postgres so clients see a consistent
    // top-level shape.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("daemon");
        let filter = crate::store::Filter {
            limit: 1_000_000,
            ..Default::default()
        };
        return match app.store.list(&ctx, &filter).await {
            Ok(memories) => {
                let total = memories.len();
                let mut short = 0usize;
                let mut mid = 0usize;
                let mut long = 0usize;
                let mut by_namespace: std::collections::BTreeMap<String, usize> =
                    std::collections::BTreeMap::new();
                for m in &memories {
                    match m.tier {
                        Tier::Short => short += 1,
                        Tier::Mid => mid += 1,
                        Tier::Long => long += 1,
                    }
                    *by_namespace.entry(m.namespace.clone()).or_insert(0) += 1;
                }
                Json(json!({
                    "total_memories": total,
                    "by_tier": {
                        "short": short,
                        "mid": mid,
                        "long": long,
                    },
                    "by_namespace": by_namespace,
                    "storage_backend": "postgres",
                }))
                .into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::stats(&lock.0, &lock.1) {
        Ok(s) => Json(json!(s)).into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn run_gc(State(app): State<AppState>) -> impl IntoResponse {
    // v0.7.0 Wave-3 Continuation 3 (Phase 17) — postgres-backed daemons
    // route through the SAL trait. Returns the same `{expired_deleted}`
    // envelope so wire shape is backend-blind.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let archive_flag = {
            let lock = app.db.lock().await;
            lock.3
        };
        return match app.store.run_gc(archive_flag).await {
            Ok(n) => {
                Json(json!({"expired_deleted": n, "storage_backend": "postgres"})).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::gc(&lock.0, lock.3) {
        Ok(n) => Json(json!({"expired_deleted": n})).into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn export_memories(State(app): State<AppState>) -> impl IntoResponse {
    // v0.7.0 Wave-3 Continuation 3 (Phase 18) — postgres-backed daemons
    // route through the SAL trait. Wire shape preserved:
    // `{memories, links, count, exported_at}`.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let mems = match app.store.export_memories().await {
            Ok(v) => v,
            Err(e) => return store_err_to_response(e),
        };
        let links = match app.store.export_links().await {
            Ok(v) => v,
            Err(e) => return store_err_to_response(e),
        };
        let count = mems.len();
        return Json(json!({
            "memories": mems,
            "links": links,
            "count": count,
            "exported_at": Utc::now().to_rfc3339(),
            "storage_backend": "postgres",
        }))
        .into_response();
    }

    let lock = app.db.lock().await;
    match (db::export_all(&lock.0), db::export_links(&lock.0)) {
        (Ok(memories), Ok(links)) => {
            let count = memories.len();
            Json(json!({"memories": memories, "links": links, "count": count, "exported_at": Utc::now().to_rfc3339()})).into_response()
        }
        (Err(e), _) | (_, Err(e)) => {
            tracing::error!("export error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn import_memories(
    State(app): State<AppState>,
    Json(body): Json<ImportBody>,
) -> impl IntoResponse {
    if body.memories.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("import limited to {} memories", MAX_BULK_SIZE)})),
        )
            .into_response();
    }
    // v0.7.0 Wave-3 Continuation 3 (Phase 18) — postgres-backed daemons
    // route through the SAL trait. We re-use `app.store.store(...)` per
    // memory (the upsert path that preserves agent_id immutability) and
    // `app.store.link(...)` for each link; partial-success surfaces the
    // same `{imported, errors}` envelope as the sqlite path.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("http-import");
        let mut imported = 0usize;
        let mut errors: Vec<String> = Vec::new();
        let mut pending: Vec<serde_json::Value> = Vec::new();
        for mem in body.memories {
            if let Err(e) = validate::validate_memory(&mem) {
                errors.push(format!("{}: {}", mem.id, e));
                continue;
            }

            // F-A2A1.5 (#705) — governance enforcement on the postgres
            // import path. Mirrors the F-A2A1.2 delete/promote gates and
            // the Wave-3 Continuation 3 create_memory gate: each imported
            // row is a Store action and must be gated by the destination
            // namespace's standard. Deny rows accumulate into `errors`
            // alongside other per-row failures; Pending rows accumulate
            // into `pending` with their pending_id so the caller can
            // drive consensus. Without this gate, postgres-backed
            // daemons silently bypassed namespace governance on the
            // bulk-import surface (same A2A bypass cluster fold-A2A1.2
            // closed on delete/promote/create paths).
            use crate::models::GovernanceDecision;
            let agent_id = mem
                .metadata
                .get("agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or("http-import");
            let payload_for_pending = serde_json::to_value(&mem).unwrap_or_else(|_| json!({}));
            match app
                .store
                .enforce_governance_action(
                    crate::store::GovernedAction::Store,
                    &mem.namespace,
                    agent_id,
                    None,
                    None,
                    &payload_for_pending,
                )
                .await
            {
                Ok(GovernanceDecision::Allow) => {}
                Ok(GovernanceDecision::Deny(reason)) => {
                    errors.push(format!("{}: import denied by governance: {reason}", mem.id));
                    continue;
                }
                Ok(GovernanceDecision::Pending(pending_id)) => {
                    pending.push(json!({
                        "id": mem.id,
                        "namespace": mem.namespace,
                        "pending_id": pending_id,
                    }));
                    continue;
                }
                Err(e) => {
                    errors.push(format!("{}: governance error: {e}", mem.id));
                    continue;
                }
            }

            match app.store.store(&ctx, &mem).await {
                Ok(_) => imported += 1,
                Err(e) => errors.push(format!("{}: {}", mem.id, e)),
            }
        }
        for link in body.links.unwrap_or_default() {
            if validate::validate_link(&link.source_id, &link.target_id, link.relation.as_str())
                .is_err()
            {
                continue;
            }
            let _ = app.store.link(&ctx, &link).await;
        }
        return Json(json!({
            "imported": imported,
            "errors": errors,
            "pending": pending,
            "storage_backend": "postgres",
        }))
        .into_response();
    }

    let lock = app.db.lock().await;
    let mut imported = 0usize;
    let mut errors = Vec::new();
    for mem in body.memories {
        if let Err(e) = validate::validate_memory(&mem) {
            errors.push(format!("{}: {}", mem.id, e));
            continue;
        }
        match db::insert(&lock.0, &mem) {
            Ok(_) => imported += 1,
            Err(e) => errors.push(format!("{}: {}", mem.id, e)),
        }
    }
    for link in body.links.unwrap_or_default() {
        if validate::validate_link(&link.source_id, &link.target_id, link.relation.as_str())
            .is_err()
        {
            continue;
        }
        let _ = db::create_link(
            &lock.0,
            &link.source_id,
            &link.target_id,
            link.relation.as_str(),
        );
    }
    Json(json!({"imported": imported, "errors": errors})).into_response()
}

#[derive(serde::Deserialize)]
pub struct ImportBody {
    pub memories: Vec<Memory>,
    #[serde(default)]
    pub links: Option<Vec<MemoryLink>>,
}

#[derive(serde::Deserialize)]
pub struct ConsolidateBody {
    pub ids: Vec<String>,
    pub title: String,
    /// v0.7.0 L7 — was required (`summary: String`), which caused the
    /// axum `Json<T>` extractor to return 422 UNPROCESSABLE ENTITY for
    /// MCP-parity payloads that ship `{use_llm: true}` and rely on the
    /// daemon to materialize the summary via the LLM (matching
    /// `handle_consolidate` at `src/mcp.rs:5008-5028`). Now optional;
    /// when absent the handler asks `app.llm.summarize_memories` to
    /// produce a real summary, otherwise (no LLM wired) we synthesise
    /// a deterministic concat fallback so the row still lands.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default = "default_ns")]
    pub namespace: String,
    #[serde(default)]
    pub tier: Option<Tier>,
    /// Optional `agent_id` for the consolidator (attributable on the result).
    /// If unset, resolved from `X-Agent-Id` header or per-request anonymous id.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// v0.7.0 L7 — explicit opt-in from S51-style MCP-parity callers
    /// that the daemon should compute the summary via the LLM rather
    /// than echoing a caller-supplied one. Today the gate is permissive:
    /// when `summary` is absent, the LLM path runs whether or not
    /// `use_llm` is set; the field is preserved for forward-compat with
    /// future "force LLM even when summary supplied" semantics.
    #[serde(default)]
    pub use_llm: bool,
}
fn default_ns() -> String {
    "global".to_string()
}

/// v0.7.0 L7 — resolve the consolidation `summary` field when the
/// caller omits it. Mirrors the MCP `handle_consolidate` auto-summary
/// path at `src/mcp.rs:5008-5028`: when an LLM is wired and the source
/// memories can be fetched, run `summarize_memories` on `(title,
/// content)` pairs. When no LLM is wired (keyword / semantic tiers, or
/// Ollama unreachable at boot), fall back to a deterministic
/// title-concat string so the consolidation still succeeds — S51 only
/// gates on `summary_len >= 20`, and the fallback is comfortably above
/// that for any 2-id call with non-trivial titles.
///
/// The blocking Ollama call is wrapped in `tokio::task::spawn_blocking`
/// to keep the async runtime healthy under load — same pattern as
/// `maybe_auto_tag`.
async fn resolve_consolidate_summary(app: &AppState, ids: &[String]) -> Result<String, Response> {
    // Collect (title, content) pairs from the appropriate backend so
    // the LLM has the actual source material. SAL on postgres; legacy
    // db on sqlite. A missing source memory short-circuits to 400 with
    // the offending id, matching the MCP path.
    let pairs = fetch_consolidate_source_pairs(app, ids).await?;

    // No LLM available — deterministic concat fallback. Titles only
    // (not full content) so the result stays a "summary" rather than a
    // verbatim concat that S51's `is_verbatim_concat` heuristic would
    // flag.
    let llm_arc = app.llm.clone();
    if llm_arc.is_none() || pairs.is_empty() {
        let titles: Vec<String> = pairs.iter().map(|(t, _)| t.clone()).collect();
        return Ok(format!(
            "Consolidated summary of {} memories: {}",
            titles.len(),
            titles.join("; ")
        ));
    }

    let llm_timeout = app.llm_call_timeout;
    // H8 (v0.7.0 round-2) — bound the Ollama summarize call by the
    // configured per-LLM-call timeout (default 30s). On timeout we
    // degrade to the deterministic concat fallback below (already the
    // L7 LLM-absent path).
    let join = tokio::time::timeout(
        llm_timeout,
        tokio::task::spawn_blocking(move || {
            let llm = match llm_arc.as_ref() {
                Some(c) => c,
                None => return Ok(String::new()),
            };
            llm.summarize_memories(&pairs)
        }),
    )
    .await;

    match join {
        Ok(Ok(Ok(s))) if !s.trim().is_empty() => Ok(s),
        Err(_) => {
            tracing::warn!(
                "H8: LLM call (summarize_memories) exceeded {}s timeout — falling back to \
                 deterministic concat",
                llm_timeout.as_secs()
            );
            Ok("Consolidated summary (LLM timeout; deterministic fallback)".to_string())
        }
        Ok(_) => {
            // LLM returned an empty body or errored (or the join task
            // panicked) — fall back to a deterministic concat-of-titles
            // fallback. Logging on the error branch only so a successful
            // empty response doesn't spam the daemon log.
            Ok("Consolidated summary (LLM unavailable; deterministic fallback)".to_string())
        }
    }
}

/// v0.7.0 L7 — fetch `(title, content)` pairs for each source memory in
/// a consolidation request, picking the storage backend off `AppState`.
/// Missing ids surface as a 400 response so the caller's mistake is
/// distinguishable from a daemon-side LLM failure.
async fn fetch_consolidate_source_pairs(
    app: &AppState,
    ids: &[String],
) -> Result<Vec<(String, String)>, Response> {
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("ai:http");
        let mut out: Vec<(String, String)> = Vec::with_capacity(ids.len());
        for id in ids {
            match app.store.get(&ctx, id).await {
                Ok(mem) => out.push((mem.title, mem.content)),
                Err(crate::store::StoreError::NotFound { .. }) => {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": format!("memory not found: {id}")})),
                    )
                        .into_response());
                }
                Err(e) => return Err(store_err_to_response(e)),
            }
        }
        return Ok(out);
    }

    let lock = app.db.lock().await;
    let mut out: Vec<(String, String)> = Vec::with_capacity(ids.len());
    for id in ids {
        match db::get(&lock.0, id) {
            Ok(Some(mem)) => out.push((mem.title, mem.content)),
            Ok(None) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("memory not found: {id}")})),
                )
                    .into_response());
            }
            Err(e) => {
                tracing::error!("consolidate source lookup failed: {e}");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal server error"})),
                )
                    .into_response());
            }
        }
    }
    Ok(out)
}

pub async fn consolidate_memories(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ConsolidateBody>,
) -> impl IntoResponse {
    // v0.7.0 L7 — materialize the summary up front so the downstream
    // validation + storage paths see a concrete `&str`. When the caller
    // supplied one, use it verbatim; when absent, ask the LLM (matching
    // the MCP `handle_consolidate` auto-summary contract); when neither
    // is available, synthesise a deterministic concat of the source
    // titles so the row still lands rather than 422'ing on a wire-shape
    // mismatch S51 has tripped on.
    let summary = match body.summary.clone() {
        Some(s) if !s.is_empty() => s,
        _ => match resolve_consolidate_summary(&app, &body.ids).await {
            Ok(s) => s,
            Err(resp) => return resp,
        },
    };

    if let Err(e) =
        validate::validate_consolidate(&body.ids, &body.title, &summary, &body.namespace)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let consolidator_agent_id =
        match crate::identity::resolve_http_agent_id(body.agent_id.as_deref(), header_agent_id) {
            Ok(id) => id,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid agent_id: {e}")})),
                )
                    .into_response();
            }
        };
    let tier = body.tier.unwrap_or(Tier::Long);
    let source_ids = body.ids.clone();

    // v0.7.0 Wave-3 Continuation 3 (Phase 14) — postgres-backed daemons
    // route through the SAL trait. Returns a structured 201/error envelope
    // that mirrors the sqlite path; the cross-namespace
    // `memory_consolidated` event + federation fanout are both
    // sqlite-only features (the sqlite branch below preserves them).
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent(&consolidator_agent_id);
        return match app
            .store
            .consolidate(
                &ctx,
                &body.ids,
                &body.title,
                &summary,
                &body.namespace,
                &tier,
                "consolidation",
                &consolidator_agent_id,
            )
            .await
        {
            Ok(new_id) => (
                StatusCode::CREATED,
                Json(json!({
                    "id": new_id,
                    "consolidated": body.ids.len(),
                    "summary": summary,
                    // v0.7.0 L7-followup — also emit the materialised summary
                    // as `content` and inside a nested `memory` object so the
                    // S51 scenario reader (which falls through
                    // `cbody.get("summary") or cbody.get("content") or
                    // (cbody.get("memory") or {}).get("content")` under a
                    // ternary that requires `memory` to be a dict) sees a
                    // non-empty string regardless of which branch its
                    // operator precedence resolves to. Without the `memory`
                    // dict the whole expression collapses to `""` even
                    // though `summary` is set — see
                    // `scenarios/51_autonomous_tier_suite.py:140-145`.
                    "content": summary,
                    "memory": {
                        "id": new_id,
                        "title": body.title,
                        "content": summary,
                        "namespace": body.namespace,
                    },
                    "storage_backend": "postgres",
                })),
            )
                .into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    let consolidate_result = db::consolidate(
        &lock.0,
        &body.ids,
        &body.title,
        &summary,
        &body.namespace,
        &tier,
        "consolidation",
        &consolidator_agent_id,
    );
    // Read the newly consolidated memory back so we can fanout — must do
    // this inside the same lock window because db::consolidate deletes
    // the source rows as part of its transaction.
    let new_mem = match &consolidate_result {
        Ok(new_id) => db::get(&lock.0, new_id).ok().flatten(),
        Err(_) => None,
    };
    // v0.6.4-017 — G9 HTTP webhook parity. Fire `memory_consolidated`
    // after db::consolidate commits (mirrors mcp.rs:2723). The new
    // memory's id goes in the outer envelope; source ids in details.
    if let Ok(new_id) = &consolidate_result {
        let details = serde_json::to_value(crate::subscriptions::ConsolidatedEventDetails {
            source_ids: source_ids.clone(),
            source_count: source_ids.len(),
        })
        .ok();
        crate::subscriptions::dispatch_event_with_details(
            &lock.0,
            "memory_consolidated",
            new_id,
            &body.namespace,
            Some(&consolidator_agent_id),
            &lock.1,
            details,
        );
    }
    // Drop DB lock before fanning out — peers POST back to our sync_push
    // and we'd deadlock on the shared Mutex if we held it.
    drop(lock);
    match consolidate_result {
        Ok(new_id) => {
            // v0.6.2 (#326): propagate consolidation to peers so
            // `metadata.consolidated_from_agents` and the deleted sources
            // are in sync across the mesh.
            if let (Some(fed), Some(mem)) = (app.federation.as_ref(), new_mem) {
                match crate::federation::broadcast_consolidate_quorum(fed, &mem, &source_ids).await
                {
                    Ok(tracker) => {
                        if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("Retry-After", "2")],
                                Json(serde_json::to_value(&payload).unwrap_or_default()),
                            )
                                .into_response();
                        }
                    }
                    Err(e) => {
                        tracing::warn!("consolidate fanout error (local committed): {e:?}");
                    }
                }
            }
            (
                StatusCode::CREATED,
                Json(json!({
                    "id": new_id,
                    "consolidated": body.ids.len(),
                    "summary": summary,
                    // v0.7.0 L7-followup — see postgres branch above for
                    // the rationale. Mirroring `content` and a nested
                    // `memory` dict here keeps both backends emitting the
                    // same wire shape so S51 passes regardless of whether
                    // the daemon is sqlite- or postgres-backed.
                    "content": summary,
                    "memory": {
                        "id": new_id,
                        "title": body.title,
                        "content": summary,
                        "namespace": body.namespace,
                    },
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// v0.7.0 L6 — `/api/v1/auto_tag` + `/api/v1/expand_query` (S51 surface)
// ---------------------------------------------------------------------------
//
// S51 (autonomous-tier LLM surface) exercises four HTTP endpoints:
// `auto_tag`, `consolidate`, `expand_query`, `detect_contradiction`.
// Pre-L6 the daemon only registered `consolidate` + `contradictions`;
// the other two were available via MCP only. L6 adds the two missing
// REST endpoints with response shapes that match what S51 reads from
// the body (`tags: [...]` and `expansions: [...]`), gated by
// `app.llm.is_some()` so the keyword / semantic tiers (no LLM wired)
// surface a clean 503 instead of a confusing 500.

/// Request body for `POST /api/v1/auto_tag`.
///
/// Two shapes are accepted to keep the surface compatible with both
/// the S51 contract (`{memory_id, namespace}`) and ad-hoc callers that
/// want to tag a free-text title + content blob without storing it
/// first (`{title, content}`). At least one of `(memory_id, title)`
/// must be present.
#[derive(serde::Deserialize, Default)]
pub struct AutoTagBody {
    /// S51 shape — id of an already-stored memory whose `(title,
    /// content)` will be fetched and tagged.
    #[serde(default)]
    pub memory_id: Option<String>,
    /// Optional namespace (S51 sends this for forward-compat; the
    /// underlying LLM call is namespace-agnostic).
    #[serde(default)]
    pub namespace: Option<String>,
    /// Ad-hoc shape — tag this title + content directly without a
    /// preceding store. Used when an operator wants to dry-run the
    /// tag prompt against an arbitrary string.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

/// `POST /api/v1/auto_tag` — generate semantic tags for a memory via
/// the configured LLM (Ollama by default).
///
/// Wire shape:
/// - request: `{memory_id, namespace}` or `{title, content}`
/// - response 200: `{tags: [..], memory_id: <id or null>}`
/// - response 503: `{error: "LLM not configured"}` when no LLM is wired
/// - response 400: validation / missing-body errors
///
/// The blocking Ollama call is wrapped in `tokio::task::spawn_blocking`
/// mirroring [`maybe_auto_tag`] so the runtime stays responsive when
/// the model is slow.
pub async fn auto_tag_handler(
    State(app): State<AppState>,
    Json(body): Json<AutoTagBody>,
) -> impl IntoResponse {
    if app.llm.is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "LLM not configured"})),
        )
            .into_response();
    }

    // Resolve (title, content). S51 sends `memory_id`; we fetch the
    // memory from the active backend. Ad-hoc callers may instead
    // supply title+content inline.
    let (title, content, resolved_id): (String, String, Option<String>) =
        if let Some(id) = body.memory_id.as_deref() {
            if let Err(e) = validate::validate_id(id) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": e.to_string()})),
                )
                    .into_response();
            }
            match fetch_memory_for_handler(&app, id).await {
                Ok(mem) => (mem.title, mem.content, Some(id.to_string())),
                Err(resp) => return resp,
            }
        } else {
            match (body.title.clone(), body.content.clone()) {
                (Some(t), Some(c)) => (t, c, None),
                _ => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": "auto_tag requires memory_id (preferred) or title+content"
                        })),
                    )
                        .into_response();
                }
            }
        };

    let llm_arc = app.llm.clone();
    let auto_tag_model = app.auto_tag_model.as_ref().clone();
    let title_owned = title;
    let content_owned = content;
    let llm_timeout = app.llm_call_timeout;
    // H8 (v0.7.0 round-2) — bound the Ollama call by the configured
    // per-LLM-call timeout (default 30s). On timeout return an empty
    // tag list with a 200 — preserves the L6/S51 contract that 200 is
    // never withheld when the operator asked for tags but Ollama was
    // slow (matches the "LLM-absent fallback" branch the keyword/
    // semantic tiers already exercise).
    let join = tokio::time::timeout(
        llm_timeout,
        tokio::task::spawn_blocking(move || {
            let llm = match llm_arc.as_ref() {
                Some(c) => c,
                None => return Ok(Vec::new()),
            };
            llm.auto_tag(&title_owned, &content_owned, auto_tag_model.as_deref())
        }),
    )
    .await;

    let tags = match join {
        Ok(Ok(Ok(tags))) => tags.into_iter().take(AUTO_TAG_MAX_TAGS).collect::<Vec<_>>(),
        Ok(Ok(Err(e))) => {
            tracing::warn!("L6: auto_tag LLM call failed: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("LLM auto_tag failed: {e}")})),
            )
                .into_response();
        }
        Ok(Err(e)) => {
            tracing::warn!("L6: auto_tag spawn_blocking join failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
        Err(_) => {
            tracing::warn!(
                "H8: LLM call (auto_tag) exceeded {}s timeout — returning empty tag list",
                llm_timeout.as_secs()
            );
            Vec::new()
        }
    };

    (
        StatusCode::OK,
        Json(json!({
            "tags": tags,
            "memory_id": resolved_id,
        })),
    )
        .into_response()
}

/// Request body for `POST /api/v1/expand_query`.
#[derive(serde::Deserialize, Default)]
pub struct ExpandQueryBody {
    pub query: String,
    #[serde(default)]
    pub namespace: Option<String>,
}

/// `POST /api/v1/expand_query` — generate semantic reformulations of a
/// free-text query via the configured LLM.
///
/// Wire shape:
/// - request: `{query, namespace?}`
/// - response 200: `{expansions: [..], original: <q>}`
/// - response 503: `{error: "LLM not configured"}` when no LLM is wired
/// - response 400: empty / missing query
pub async fn expand_query_handler(
    State(app): State<AppState>,
    Json(body): Json<ExpandQueryBody>,
) -> impl IntoResponse {
    if app.llm.is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "LLM not configured"})),
        )
            .into_response();
    }
    let query = body.query.trim().to_string();
    if query.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "query is required"})),
        )
            .into_response();
    }

    let llm_arc = app.llm.clone();
    let query_owned = query.clone();
    let llm_timeout = app.llm_call_timeout;
    // H8 (v0.7.0 round-2) — bound the Ollama call by the configured
    // per-LLM-call timeout (default 30s). On timeout return an empty
    // expansion list — matches the LLM-absent fallback shape.
    let join = tokio::time::timeout(
        llm_timeout,
        tokio::task::spawn_blocking(move || {
            let llm = match llm_arc.as_ref() {
                Some(c) => c,
                None => return Ok(Vec::new()),
            };
            llm.expand_query(&query_owned)
        }),
    )
    .await;

    let expansions = match join {
        Ok(Ok(Ok(terms))) => terms,
        Ok(Ok(Err(e))) => {
            tracing::warn!("L6: expand_query LLM call failed: {e}");
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("LLM expand_query failed: {e}")})),
            )
                .into_response();
        }
        Ok(Err(e)) => {
            tracing::warn!("L6: expand_query spawn_blocking join failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
        Err(_) => {
            tracing::warn!(
                "H8: LLM call (expand_query) exceeded {}s timeout — returning empty expansion list",
                llm_timeout.as_secs()
            );
            Vec::new()
        }
    };

    (
        StatusCode::OK,
        Json(json!({
            "expansions": expansions,
            "original": query,
        })),
    )
        .into_response()
}

/// v0.7.0 L6/L7 — fetch a single memory by id off the active storage
/// backend. Returns a structured 4xx/5xx response on miss / lookup
/// failure so the calling handler can `return Err(resp)`.
async fn fetch_memory_for_handler(app: &AppState, id: &str) -> Result<Memory, Response> {
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("ai:http");
        return match app.store.get(&ctx, id).await {
            Ok(mem) => Ok(mem),
            Err(crate::store::StoreError::NotFound { .. }) => Err((
                StatusCode::NOT_FOUND,
                Json(json!({"error": format!("memory not found: {id}")})),
            )
                .into_response()),
            Err(e) => Err(store_err_to_response(e)),
        };
    }

    let lock = app.db.lock().await;
    match db::get(&lock.0, id) {
        Ok(Some(mem)) => Ok(mem),
        Ok(None) => Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("memory not found: {id}")})),
        )
            .into_response()),
        Err(e) => {
            tracing::error!("memory lookup failed: {e}");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response())
        }
    }
}

// ---------------------------------------------------------------------------
// v0.7.0 L9 — `GET /api/v1/tools/list` (NHI-D-501-postgres-traits)
// ---------------------------------------------------------------------------
//
// HTTP parity for the MCP `tools/list` JSON-RPC method. Surfaces the
// canonical tool catalog the daemon advertises under its resolved
// `Profile`, computed from in-memory configuration only — no DB access
// — so the postgres and sqlite paths return byte-identical bodies.
//
// NHI surfaced this as `NHI-D-501-postgres-traits` because the
// postgres-gated daemon returned the generic 501 envelope for the path
// even though the response is pure enumeration. The 501 was a false
// negative: the handler can be implemented entirely off `app.profile`
// + `app.mcp_config`.

/// `GET /api/v1/tools/list` — enumerate the MCP tools currently
/// advertised under the daemon's resolved [`Profile`]. The response
/// shape mirrors MCP `tools/list`: `{tools: [{name, description, ...}],
/// schema_version: <tag>}`. Backend-agnostic — works on both sqlite
/// and postgres daemons because the data is configuration, not user
/// content.
pub async fn tools_list(State(app): State<AppState>) -> impl IntoResponse {
    // `tool_definitions_for_profile` already applies the C2 / C4
    // trims that match the MCP `tools/list` shape. No further shaping
    // is needed for the HTTP wire — the field names line up with the
    // MCP JSON-RPC payload exactly.
    let defs = crate::mcp::tool_definitions_for_profile(app.profile.as_ref());
    (StatusCode::OK, Json(defs)).into_response()
}

// ---------------------------------------------------------------------------
// v0.7.0 L10 — `POST /api/v1/memory_load_family`
// ---------------------------------------------------------------------------
//
// HTTP parity for the MCP `memory_load_family` tool. Filters memories
// by `metadata.family` (a free-form JSON field stamped by the B1 path)
// and returns the top-k recent + high-priority rows. NHI surfaced
// `NHI-D-501-postgres-loadfamily` for the same reason as L9 — the
// endpoint was 501'd on postgres even though `app.store.list(...)`
// already exposes the underlying scan. The handler now dispatches
// through SAL on postgres and through `db::list` on sqlite, doing a
// post-filter on `metadata.family` in-memory because that field is not
// yet a first-class SAL filter axis.

/// Request body for `POST /api/v1/memory_load_family`.
#[derive(serde::Deserialize)]
pub struct LoadFamilyBody {
    /// One of: core, lifecycle, graph, governance, power, meta,
    /// archive, other. Validated against [`Family::all`].
    pub family: String,
    /// Optional namespace narrowing. When omitted the scan spans every
    /// namespace, matching the MCP tool's "no namespace = all" rule.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Top-K cap. Default 20, clamped to `[1, 100]` for response-budget
    /// reasons (mirroring `handle_load_family`).
    #[serde(default)]
    pub k: Option<u64>,
}

/// `POST /api/v1/memory_load_family` — return the top-K recent +
/// high-priority memories tagged with the requested family.
///
/// Wire shape:
/// - request: `{family, namespace?, k?}`
/// - response 200: `{family, namespace, k, count, memories: [..]}`
/// - response 400: unknown family / bad namespace
pub async fn load_family_handler(
    State(app): State<AppState>,
    Json(body): Json<LoadFamilyBody>,
) -> impl IntoResponse {
    use std::str::FromStr;

    let family = match Family::from_str(&body.family) {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    if let Some(ref ns) = body.namespace
        && let Err(e) = validate::validate_namespace(ns)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    let k_raw = body.k.unwrap_or(20);
    let k = usize::try_from(k_raw).unwrap_or(usize::MAX).clamp(1, 100);
    let family_name = family.name();

    // v0.7.0 Wave-3 — postgres path. Pull a generous superset via the
    // SAL trait then filter on `metadata.family` in memory; the trait
    // filter axes don't yet include metadata fields. Cap the prefetch
    // at MAX_BULK_SIZE so a postgres daemon can't be coerced into
    // loading the whole table on a small `k`.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let filter = crate::store::Filter {
            namespace: body.namespace.clone(),
            tier: None,
            tags_any: Vec::new(),
            agent_id: None,
            since: None,
            until: None,
            limit: MAX_BULK_SIZE,
        };
        let ctx = crate::store::CallerContext::for_agent("ai:http");
        return match app.store.list(&ctx, &filter).await {
            Ok(all) => {
                let mut filtered: Vec<Memory> = all
                    .into_iter()
                    .filter(|m| {
                        m.metadata.get("family").and_then(serde_json::Value::as_str)
                            == Some(family_name)
                    })
                    .collect();
                // priority DESC, updated_at DESC (mirrors handle_load_family).
                filtered.sort_by(|a, b| {
                    b.priority
                        .cmp(&a.priority)
                        .then_with(|| b.updated_at.cmp(&a.updated_at))
                });
                filtered.truncate(k);
                let count = filtered.len();
                Json(json!({
                    "family": family_name,
                    "namespace": body.namespace,
                    "k": k,
                    "count": count,
                    "memories": filtered,
                }))
                .into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    // Sqlite path — reuse the MCP `handle_load_family` SQL verbatim by
    // calling it through with the same parameter shape (a `Value`).
    let lock = app.db.lock().await;
    let params = json!({
        "family": family_name,
        "namespace": body.namespace,
        "k": k,
    });
    match crate::mcp::handle_load_family(&lock.0, &params) {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

pub async fn bulk_create(
    State(app): State<AppState>,
    Json(bodies): Json<Vec<CreateMemory>>,
) -> impl IntoResponse {
    if bodies.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("bulk operations limited to {} items", MAX_BULK_SIZE)})),
        )
            .into_response();
    }
    let now = Utc::now();

    // v0.7.0 Wave-3 Continuation — postgres-backed daemons stream each
    // row through `app.store.store(...)`. Federation fanout below stays
    // sqlite-only because the federation transport assumes the
    // SQLite-on-disk model; postgres deployments use the postgres replica
    // mechanism for cross-node visibility, not HTTP fanout. The wire
    // shape (created+errors counts) matches the sqlite path exactly.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("daemon");
        let mut created: usize = 0;
        let mut errors: Vec<String> = Vec::new();
        let mut pending: Vec<serde_json::Value> = Vec::new();
        for body in bodies {
            if let Err(e) = validate::validate_create(&body) {
                errors.push(format!("{}: {}", body.title, e));
                continue;
            }
            let expires_at = body.expires_at.clone().or_else(|| {
                body.ttl_secs
                    .map(|s| (now + Duration::seconds(s)).to_rfc3339())
            });
            let mem = Memory {
                id: Uuid::new_v4().to_string(),
                tier: body.tier,
                namespace: body.namespace,
                title: body.title,
                content: body.content,
                tags: body.tags,
                priority: body.priority.clamp(1, 10),
                confidence: body.confidence.clamp(0.0, 1.0),
                source: body.source,
                access_count: 0,
                created_at: now.to_rfc3339(),
                updated_at: now.to_rfc3339(),
                last_accessed_at: None,
                expires_at,
                metadata: body.metadata,
                reflection_depth: 0,
                memory_kind: crate::models::MemoryKind::Observation,
                entity_id: None,
                persona_version: None,
                citations: Vec::new(),
                source_uri: None,
                source_span: None,
            };

            // F-A2A1.5 (#705) — governance enforcement on the postgres
            // bulk_create path. Mirrors F-A2A1.2 delete/promote and the
            // Wave-3 Continuation 3 create_memory gate. Each row is a
            // Store action against its own namespace, so the standard's
            // `write=` rule must be consulted per row. Deny rows
            // accumulate into `errors`; Pending rows accumulate into
            // `pending` with their pending_id. Without this gate,
            // postgres-backed daemons silently bypassed namespace
            // governance on the bulk-create surface (same A2A bypass
            // cluster fold-A2A1.2 closed on delete/promote/create
            // paths).
            use crate::models::GovernanceDecision;
            let agent_id = mem
                .metadata
                .get("agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or("daemon");
            let payload_for_pending = serde_json::to_value(&mem).unwrap_or_else(|_| json!({}));
            match app
                .store
                .enforce_governance_action(
                    crate::store::GovernedAction::Store,
                    &mem.namespace,
                    agent_id,
                    None,
                    None,
                    &payload_for_pending,
                )
                .await
            {
                Ok(GovernanceDecision::Allow) => {}
                Ok(GovernanceDecision::Deny(reason)) => {
                    errors.push(format!(
                        "{}: bulk_create denied by governance: {reason}",
                        mem.title
                    ));
                    continue;
                }
                Ok(GovernanceDecision::Pending(pending_id)) => {
                    pending.push(json!({
                        "title": mem.title,
                        "namespace": mem.namespace,
                        "pending_id": pending_id,
                    }));
                    continue;
                }
                Err(e) => {
                    errors.push(format!("{}: governance error: {e}", mem.title));
                    continue;
                }
            }

            match app.store.store(&ctx, &mem).await {
                Ok(_) => created += 1,
                Err(e) => errors.push(e.to_string()),
            }
        }
        return Json(json!({
            "created": created,
            "errors": errors,
            "pending": pending,
        }))
        .into_response();
    }

    // Stage 1 — validate + insert locally. Collect the successfully-inserted
    // `Memory` values so we can fanout each one after we release the DB lock
    // (peers POST to our /sync/push and we'd deadlock on the Mutex if we
    // held it across the network call).
    let mut created_mems: Vec<Memory> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    {
        let lock = app.db.lock().await;
        for body in bodies {
            if let Err(e) = validate::validate_create(&body) {
                errors.push(format!("{}: {}", body.title, e));
                continue;
            }
            let expires_at = body.expires_at.or_else(|| {
                body.ttl_secs
                    .or(lock.2.ttl_for_tier(&body.tier))
                    .map(|s| (now + Duration::seconds(s)).to_rfc3339())
            });
            let mem = Memory {
                id: Uuid::new_v4().to_string(),
                tier: body.tier,
                namespace: body.namespace,
                title: body.title,
                content: body.content,
                tags: body.tags,
                priority: body.priority.clamp(1, 10),
                confidence: body.confidence.clamp(0.0, 1.0),
                source: body.source,
                access_count: 0,
                created_at: now.to_rfc3339(),
                updated_at: now.to_rfc3339(),
                last_accessed_at: None,
                expires_at,
                metadata: body.metadata,
                reflection_depth: 0,
                memory_kind: crate::models::MemoryKind::Observation,
                entity_id: None,
                persona_version: None,
                citations: Vec::new(),
                source_uri: None,
                source_span: None,
            };
            match db::insert(&lock.0, &mem) {
                Ok(_) => created_mems.push(mem),
                Err(e) => errors.push(e.to_string()),
            }
        }
    }
    // Stage 2 — federation fanout, once per successfully-inserted row.
    //
    // v0.6.2 (S40): we run each row's `broadcast_store_quorum` *concurrently*
    // via `tokio::task::JoinSet`, bounded by a semaphore so we never have
    // more than `BULK_FANOUT_CONCURRENCY` in-flight fanouts at a time. The
    // prior form looped sequentially and paid one full ack-round-trip per
    // row — 500 rows × ~100ms = 50s, dwarfing the scenario's 20s settle
    // window so peers only received the first ~200 writes in time.
    //
    // Why a bound instead of unbounded? Unbounded (`JoinSet.spawn` for
    // each row at once) fires N × peers concurrent reqwest POSTs. At N=500
    // × 3 peers = 1500 concurrent TCP connects this exhausts ephemeral
    // ports and the reqwest client's connection pool, manifesting as
    // `network: error sending request` on most rows. A bound of 32
    // concurrent fanouts still pipelines the ack round-trip (100ms per
    // row × 500 / 32 ≈ 1.6s wall), well inside the 20s scenario budget.
    //
    // Each row's broadcast still uses the full quorum contract (local +
    // W-1 peer acks or 503). The semaphore only limits concurrency; it
    // does NOT weaken any single row's guarantees. Non-quorum errors
    // land in `errors` with the row id prefix, exactly as before. On a
    // quorum miss we keep going — a single row's miss must not abort the
    // other 499 the caller just paid for (bulk semantics, deliberately
    // weaker than `create_memory`'s 503 short-circuit).
    // Concurrency bound balances:
    //   - Speedup over sequential: N / bound × ack — need bound ≥ a few to
    //     clear 500 rows × 100ms ack inside the scenario's 20s settle.
    //   - Peer-side contention: every concurrent fanout lands a sync_push
    //     POST on the same SQLite Mutex on each peer. Too many in-flight
    //     serialize at the peer's DB lock and either timeout the quorum
    //     window or hit reqwest connection-pool / ephemeral-port limits
    //     on the leader side.
    //
    // 8 is a conservative compromise: 500 × 100ms / 8 ≈ 6.2s wall, comfortably
    // under the scenario's 20s budget while keeping the peer's per-writer
    // queue short enough to avoid timeouts under typical testbook load.
    // Tuned via the `BULK_FANOUT_CONCURRENCY` module constant.
    if let Some(fed) = app.federation.as_ref() {
        let sem = Arc::new(tokio::sync::Semaphore::new(BULK_FANOUT_CONCURRENCY));
        let mut joins: tokio::task::JoinSet<(String, Result<(), String>)> =
            tokio::task::JoinSet::new();
        for mem in &created_mems {
            let fed = fed.clone();
            let mem = mem.clone();
            let sem = sem.clone();
            joins.spawn(async move {
                // `acquire_owned` + a semaphore the task owns a clone of
                // means the permit lives for the task's lifetime — it's
                // released only when the task completes. A closed
                // semaphore would be a bug; surface it via the error
                // channel and keep going.
                let Ok(_permit) = sem.acquire_owned().await else {
                    return (mem.id.clone(), Err("fanout semaphore closed".to_string()));
                };
                let id = mem.id.clone();
                let outcome = match crate::federation::broadcast_store_quorum(&fed, &mem).await {
                    Ok(tracker) => match crate::federation::finalise_quorum(&tracker) {
                        Ok(_) => Ok(()),
                        Err(err) => Err(err.to_string()),
                    },
                    Err(e) => {
                        tracing::warn!(
                            "bulk_create: fanout for {id} failed (local committed): {e:?}"
                        );
                        Ok(())
                    }
                };
                (id, outcome)
            });
        }
        while let Some(res) = joins.join_next().await {
            match res {
                Ok((id, Err(err))) => errors.push(format!("{id}: {err}")),
                Ok((_, Ok(()))) => {}
                Err(e) => tracing::warn!("bulk_create: fanout task join error: {e:?}"),
            }
        }

        // v0.6.2 Patch 2 (S40): terminal catchup batch. Per-row quorum
        // met above, but the post-quorum detach path — even with
        // retry-once in `post_and_classify` — can still leave a peer
        // one row behind under sustained SQLite-mutex contention (v3r26
        // hermes-tls 499/500 and v3r27 ironclaw-off 499/500 both tripped
        // the scenario despite the retry). A single batched `sync_push`
        // per peer with every committed row closes the gap: peer's
        // `insert_if_newer` no-ops rows it already has and applies the
        // missing one. O(1) extra POST per peer vs O(N) per-row retries.
        //
        // Errors are logged and folded into the response `errors` array
        // but do NOT fail the bulk write — quorum was already met, so
        // the HTTP contract is satisfied. The catchup only strengthens
        // eventual consistency within the scenario settle window.
        if !created_mems.is_empty() {
            let catchup_errors = crate::federation::bulk_catchup_push(fed, &created_mems).await;
            for (peer_id, err) in catchup_errors {
                errors.push(format!("catchup to {peer_id}: {err}"));
            }
        }
    }
    Json(json!({"created": created_mems.len(), "errors": errors})).into_response()
}

// ---------------------------------------------------------------------------
// Archive endpoints
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ArchiveListQuery {
    pub namespace: Option<String>,
    #[serde(default = "default_archive_limit")]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[allow(clippy::unnecessary_wraps)]
fn default_archive_limit() -> Option<usize> {
    Some(50)
}

pub async fn list_archive(
    State(app): State<AppState>,
    Query(q): Query<ArchiveListQuery>,
) -> impl IntoResponse {
    // Ultrareview #350: validate limit range. `usize` already precludes
    // negative values at the serde layer, but `limit=0` silently
    // returned an empty page — indistinguishable from "no results".
    // Require 1..=1000 and reject 0 with a specific error.
    if matches!(q.limit, Some(0)) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "limit must be >= 1"})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 Continuation — postgres-backed daemons project from
    // the `archived_memories` table via the SAL adapter. The trait does
    // not yet expose archive operations, so we dispatch via the typed
    // `PostgresStore::list_archived` helper added under feature
    // `sal-postgres`. Returns the same wire envelope as sqlite.
    #[cfg(feature = "sal-postgres")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let limit = q.limit.unwrap_or(50).clamp(1, 1000);
        let offset = q.offset.unwrap_or(0);
        return match crate::store::postgres::list_archived_via_store(
            &app.store,
            q.namespace.as_deref(),
            limit,
            offset,
        )
        .await
        {
            Ok(items) => Json(json!({"archived": items, "count": items.len()})).into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0);
    match db::list_archived(&lock.0, q.namespace.as_deref(), limit, offset) {
        Ok(items) => Json(json!({"archived": items, "count": items.len()})).into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn restore_archive(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    // v0.7.0 Wave-3 Continuation 3 (Phase 19) — postgres-backed daemons
    // route through the SAL `archive_restore` trait method. Federation
    // fanout for restore stays sqlite-only (the `broadcast_restore_quorum`
    // path uses sqlite-coupled fed-tracker state); postgres-backed
    // operators relying on multi-node consistency should poll peers.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("http");
        return match app.store.archive_restore(&ctx, &id).await {
            Ok(true) => Json(json!({"restored": true, "id": id, "storage_backend": "postgres"}))
                .into_response(),
            Ok(false) => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "not found in archive"})),
            )
                .into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let restored = {
        let lock = app.db.lock().await;
        match db::restore_archived(&lock.0, &id) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("handler error: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal server error"})),
                )
                    .into_response();
            }
        }
    };
    if !restored {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "not found in archive"})),
        )
            .into_response();
    }

    // v0.6.2 (S29): broadcast the restore to peers so they move the row
    // from `archived_memories` → `memories` in lockstep. Without this, a
    // POST /api/v1/archive/{id}/restore on node-1 leaves node-2..4 with
    // the row still archived, so node-4 never sees M1 re-enter the active
    // set (the testbook-v3 S29 assertion). Same posture as
    // `archive_by_ids`: on a quorum miss we short-circuit with 503 so
    // operators can retry.
    if let Some(fed) = app.federation.as_ref() {
        match crate::federation::broadcast_restore_quorum(fed, &id).await {
            Ok(tracker) => {
                if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                    let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [("Retry-After", "2")],
                        Json(serde_json::to_value(&payload).unwrap_or_default()),
                    )
                        .into_response();
                }
            }
            Err(e) => {
                // Local commit already landed — sync-daemon catches
                // stragglers. Same posture as `fanout_or_503`.
                tracing::warn!("restore fanout error (local committed): {e:?}");
            }
        }
    }

    Json(json!({"restored": true, "id": id})).into_response()
}

#[derive(Debug, Deserialize)]
pub struct PurgeQuery {
    pub older_than_days: Option<i64>,
}

pub async fn purge_archive(
    State(app): State<AppState>,
    Query(q): Query<PurgeQuery>,
) -> impl IntoResponse {
    // v0.7.0 Wave-3 Continuation 3 (Phase 19) — postgres-backed daemons
    // route through the SAL trait. Wire shape preserved: `{purged}`.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        return match app.store.archive_purge(q.older_than_days).await {
            Ok(n) => Json(json!({"purged": n, "storage_backend": "postgres"})).into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::purge_archive(&lock.0, q.older_than_days) {
        Ok(n) => Json(json!({"purged": n})).into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn archive_stats(State(app): State<AppState>) -> impl IntoResponse {
    // v0.7.0 Wave-3 Continuation — postgres-backed daemons aggregate
    // counts directly from the `archived_memories` table.
    #[cfg(feature = "sal-postgres")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        return match crate::store::postgres::archive_stats_via_store(&app.store).await {
            Ok(v) => Json(v).into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::archive_stats(&lock.0) {
        Ok(archive_stats) => Json(archive_stats).into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// Request body for `POST /api/v1/archive` — S29 explicit archive.
#[derive(Debug, Deserialize)]
pub struct ArchiveByIdsBody {
    pub ids: Vec<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// POST /api/v1/archive — explicit archive of the given memory ids
/// (S29). For each id:
///   1. Call `db::archive_memory` locally to soft-move the row.
///   2. If federation is configured, broadcast via
///      `broadcast_archive_quorum` so peers land in the same terminal
///      state (row out of `memories`, row into `archived_memories`).
///
/// On a quorum miss for ANY id, short-circuit with 503 via the shared
/// `fanout_or_503`-style payload. This matches the posture of the
/// delete + consolidate fanout endpoints.
///
/// Response body:
/// ```json
/// {"archived": [id1, id2], "missing": [id3], "count": 2}
/// ```
/// where `missing` enumerates ids that had no live row locally (common
/// during retries). The response never includes content/metadata — use
/// `GET /api/v1/archive` to list archive entries.
pub async fn archive_by_ids(
    State(app): State<AppState>,
    Json(body): Json<ArchiveByIdsBody>,
) -> impl IntoResponse {
    // Bound the batch the same way bulk_create / sync_push do.
    if body.ids.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("archive limited to {} ids per request", MAX_BULK_SIZE)})),
        )
            .into_response();
    }
    // Validate all ids up-front so we never start mutating on a bad batch.
    for id in &body.ids {
        if let Err(e) = validate::validate_id(id) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid id {id}: {e}")})),
            )
                .into_response();
        }
    }
    let reason = body.reason.as_deref().unwrap_or("archive").to_string();
    let mut archived: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    // v0.7.0 Wave-3 Continuation 3 (Phase 19) — postgres-backed daemons
    // route through the SAL `archive_by_ids` trait method. The federation
    // fanout stays sqlite-only; postgres operators relying on multi-node
    // consistency should poll peers.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("http");
        // Run per-id so we can split archived vs missing — the trait
        // method bulk-archives but doesn't tell us which were missing,
        // so we probe each via the count delta.
        for id in &body.ids {
            match app
                .store
                .archive_by_ids(&ctx, std::slice::from_ref(id), Some(&reason))
                .await
            {
                Ok(1) => archived.push(id.clone()),
                Ok(_) => missing.push(id.clone()),
                Err(e) => return store_err_to_response(e),
            }
        }
        return (
            StatusCode::OK,
            Json(json!({
                "archived": archived,
                "missing": missing,
                "count": archived.len(),
                "reason": reason,
                "storage_backend": "postgres",
            })),
        )
            .into_response();
    }

    for id in &body.ids {
        // Local archive. Hold the lock only across this one call per id so
        // we can release it before a potentially slow network fanout.
        let moved = {
            let lock = app.db.lock().await;
            match db::archive_memory(&lock.0, id, Some(&reason)) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("archive_by_ids: archive_memory({id}) failed: {e}");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "internal server error"})),
                    )
                        .into_response();
                }
            }
        };
        if !moved {
            // Row wasn't live locally — record as missing but keep going.
            // Do NOT fan out (peers can't know to archive from a row they
            // may have under a different state; the originator's local
            // state is the trigger).
            missing.push(id.clone());
            continue;
        }

        // Fanout. Mirror the shape used by the other
        // quorum-backed write endpoints (delete, consolidate) — on a
        // miss, surface the `quorum_not_met` payload with 503 + Retry-After.
        if let Some(fed) = app.federation.as_ref() {
            match crate::federation::broadcast_archive_quorum(fed, id).await {
                Ok(tracker) => {
                    if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                        let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("Retry-After", "2")],
                            Json(serde_json::to_value(&payload).unwrap_or_default()),
                        )
                            .into_response();
                    }
                }
                Err(e) => {
                    // Local commit already landed — sync-daemon catches
                    // stragglers. Same posture as `fanout_or_503`.
                    tracing::warn!("archive fanout error (local committed): {e:?}");
                }
            }
        }
        archived.push(id.clone());
    }

    (
        StatusCode::OK,
        Json(json!({
            "archived": archived,
            "missing": missing,
            "count": archived.len(),
            "reason": reason,
        })),
    )
        .into_response()
}
