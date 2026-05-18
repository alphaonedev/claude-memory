// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// #873 — this file currently exceeds the 250-line per-function budget
// in `create_memory` (#866) and several other large handlers; the
// per-function `#[allow(clippy::too_many_lines)]` attributes inside
// keep the warn-level lint green while the splits land. Module-level
// allow is the belt-and-braces in case a function grows past
// threshold without picking up its own attribute. Tracked for split
// as #866 + #868.
#![allow(clippy::too_many_lines)]

use crate::models::ConfidenceSource;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use chrono::{Duration, Utc};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

use crate::db;
use crate::embeddings::EmbedStatus;
use crate::models::{
    CreateMemory, ForgetQuery, ListQuery, Memory, SearchQuery, Tier, UpdateMemory,
};
use crate::validate;

#[cfg(feature = "sal")]
use super::StorageBackend;
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
            confidence_source: ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
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
                // #869 (2026-05-18) — pre-fix the silent
                // `unwrap_or_else(json!({}))` masked a serialise
                // failure as 201 + `{}`. Route through the typed
                // helper that returns a 500 envelope on encode error
                // so the wire surface stays honest.
                let mut payload =
                    match super::to_value_or_500("create_memory.postgres.response", &mem) {
                        Ok(v) => v,
                        Err(resp) => return resp,
                    };
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
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
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
        // #869 — silently degrading to `Value::Null` would let the
        // governance engine see a different payload than the one we
        // were about to commit (rule predicates that key on memory
        // fields would all evaluate against `null` and degenerate to
        // either always-allow or always-deny depending on the rule
        // semantics). Fail closed with a 500 instead.
        let payload = match super::to_value_or_500("create_memory.governance.payload", &mem) {
            Ok(v) => v,
            Err(resp) => return resp,
        };
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

// ---------------------------------------------------------------------------
// Task 1.9 — pending_actions endpoints
// ---------------------------------------------------------------------------

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

// ============================================================================
// v0.7.0 Wave-3 Continuation 6 — three REST endpoints closing F7 cert-harness
// gaps (S52 `links/verify`, S61 `quota/status`, S65 `kg/find_paths`).
// ============================================================================

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
                // Issue #851: do not echo the caller's title back paired
                // with the raw error — both are caller-influenced, and
                // the combo can be used to verify presence/shape of
                // server-side fields. Sanitize and log instead.
                tracing::warn!("bulk_create(postgres): validate_create failed: {e}");
                errors.push(super::sanitize_bulk_row_error(&e.to_string()).to_string());
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
                confidence_source: ConfidenceSource::CallerProvided,
                confidence_signals: None,
                confidence_decayed_at: None,
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
                Err(e) => {
                    // Issue #851: SAL store errors can carry raw
                    // sqlx/sqlite text. Sanitize before echoing.
                    tracing::warn!("bulk_create(postgres): store.store failed: {e}");
                    errors.push(super::sanitize_bulk_row_error(&e.to_string()).to_string());
                }
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
                // Issue #851: do not echo the caller's title back paired
                // with the raw error. Sanitize and log instead.
                tracing::warn!("bulk_create: validate_create failed: {e}");
                errors.push(super::sanitize_bulk_row_error(&e.to_string()).to_string());
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
                confidence_source: ConfidenceSource::CallerProvided,
                confidence_signals: None,
                confidence_decayed_at: None,
            };
            match db::insert(&lock.0, &mem) {
                Ok(_) => created_mems.push(mem),
                Err(e) => {
                    // Issue #851: db::insert errors include raw rusqlite
                    // text (constraint names, SQL fragments). Sanitize.
                    tracing::warn!("bulk_create: db::insert failed: {e}");
                    errors.push(super::sanitize_bulk_row_error(&e.to_string()).to_string());
                }
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
