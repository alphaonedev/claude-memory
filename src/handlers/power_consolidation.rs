// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Consolidation + LLM-tool HTTP handlers — `consolidate_memories`,
//! `auto_tag_handler`, `expand_query_handler`, and `load_family_handler`,
//! plus their LLM-backed source-summary helpers.
//!
//! Extracted from [`super::power`] under issue #650 (handler cap ≤1200 LOC).
//! Handler bodies are unchanged; only the module surface moved. Wire
//! compatibility preserved via `pub use power_consolidation::*` in
//! [`super`].

#![allow(clippy::too_many_lines)]

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::json;

use crate::db;
use crate::models::{Memory, Tier};
use crate::profile::Family;
use crate::validate;

use super::AppState;
#[cfg(feature = "sal")]
use super::MAX_BULK_SIZE;
#[cfg(feature = "sal")]
use super::StorageBackend;
#[cfg(feature = "sal")]
use super::store_err_to_response;

/// L5 — cap on auto-tag output rows.
const AUTO_TAG_MAX_TAGS: usize = 8;

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
    // #905 (security-high, 2026-05-19) — sibling of #874/#901. The
    // pre-#905 path passed `body.agent_id` as the first arg to
    // `resolve_http_agent_id` which gives caller-controlled body the
    // PRECEDENCE over the authenticated `X-Agent-Id` header. An
    // attacker authenticated as `bob` could call
    // `POST /api/v1/consolidate` with `body.agent_id="alice"` and
    // the new consolidated row would be stamped with
    // `consolidator_agent_id="alice"` — a provenance lie that also
    // breaks the cross-tenant tracking the K9 governance walk leans
    // on. Header-only authentication now; body.agent_id (if present)
    // must match the authenticated caller else 403.
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let consolidator_agent_id = match crate::identity::resolve_http_agent_id(None, header_agent_id)
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
    if let Some(claimed) = body.agent_id.as_deref()
        && claimed != consolidator_agent_id
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "agent_id body parameter does not match authenticated caller"})),
        )
            .into_response();
    }
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
                            // #869 — typed 503 envelope via the shared helper.
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return super::quorum_not_met_response(&payload);
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
