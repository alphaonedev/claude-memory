// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Memory query / bulk HTTP handlers — `list_memories`,
//! `search_memories`, `forget_memories`, and `bulk_create`.
//!
//! Extracted from [`super::http`] under issue #650 (handler cap ≤1200
//! LOC). Handler bodies are unchanged; only the module surface moved.
//! Wire compatibility preserved via `pub use memories_query::*` in
//! [`super`].

#![allow(clippy::too_many_lines)]

use crate::models::ConfidenceSource;
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use chrono::{Duration, Utc};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

use crate::db;
use crate::models::{CreateMemory, ForgetQuery, ListQuery, Memory, SearchQuery};
use crate::validate;

use super::AppState;
#[cfg(feature = "sal")]
use super::StorageBackend;
#[cfg(feature = "sal")]
use super::store_err_to_response;
use super::{BULK_FANOUT_CONCURRENCY, MAX_BULK_SIZE};

/// #910 (security-medium, 2026-05-19) — apply the scope=private
/// visibility filter on `GET /api/v1/memories` and `POST /api/v1/kg/query`
/// result sets. Pre-#910, both endpoints returned every row matching
/// the requested namespace/tier/etc. shape regardless of
/// `metadata.scope` — a caller authenticated as `bob` could
/// enumerate `alice`'s scope=private rows by listing the namespace.
///
/// Visibility rule (mirrors `storage::is_visible_to_agent` + the
/// generated `scope_idx` column's COALESCE-to-`private` default):
/// row is returned iff
///   `metadata.scope != "private"` (rows w/o the field are private
///   by the CLAUDE.md NHI contract)
///   OR `metadata.agent_id == caller`.
///
/// Operator-equivalent surfaces (CLI / MCP) already filter via
/// `visibility_clause`; this helper is the missing HTTP-side mirror
/// of that filter on the plain `list` + `kg_query` paths.
#[must_use]
fn is_visible_to_caller(mem: &Memory, caller: &str) -> bool {
    let scope = mem
        .metadata
        .get("scope")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("private");
    if scope != "private" {
        return true;
    }
    let owner = mem
        .metadata
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    owner == caller
}

pub async fn list_memories(
    State(app): State<AppState>,
    headers: HeaderMap,
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

    // #910 (security-medium, 2026-05-19) — resolve the caller via the
    // `X-Agent-Id` header so the scope=private visibility filter below
    // has a known principal to compare `metadata.agent_id` against.
    // Pre-#910 the handler skipped this step entirely and returned
    // every row matching the requested namespace/tier/etc. shape — an
    // attacker could enumerate scope=private rows authored by other
    // agents by listing their namespace. Header-only authentication
    // (no body field on this GET path); anonymous callers get a
    // per-request `anonymous:req-…` id and see only non-private rows.
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let caller = match crate::identity::resolve_http_agent_id(None, header_agent_id) {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid agent_id: {e}")})),
            )
                .into_response();
        }
    };

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
            // #869 audit (Category B — safe default): missing `tags`
            // querystring collapses to empty `Vec<String>` which the
            // SAL `Filter` treats as "no tag filter" — documented.
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
        let ctx = crate::store::CallerContext::for_agent(&caller);
        return match app.store.list(&ctx, &filter).await {
            Ok(mems) => {
                // #910 — post-filter scope=private rows the caller does
                // not own. Done in-process rather than via the SAL
                // `Filter` because the trait's filter shape does not
                // carry a scope axis yet (tracked for the next trait
                // extension wave); the post-filter is correctness-
                // equivalent to a WHERE clause at the SQL layer for
                // the result-set sizes that fit the trait's `limit`.
                let visible: Vec<Memory> = mems
                    .into_iter()
                    .filter(|m| is_visible_to_caller(m, &caller))
                    .collect();
                Json(json!({"memories": &visible, "count": visible.len()})).into_response()
            }
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
        Ok(mems) => {
            // #910 — see postgres branch comment above. `db::list` does
            // NOT apply the visibility-prefix filter that `db::search`
            // and `db::recall_hybrid` use; that gap is what closed the
            // cross-tenant enumeration vector. Post-filter in-process
            // until the next storage-layer wave threads a `caller`
            // through `db::list` and rewrites the WHERE clause to use
            // the same `visibility_clause` helper as the search path.
            let visible: Vec<Memory> = mems
                .into_iter()
                .filter(|m| is_visible_to_caller(m, &caller))
                .collect();
            Json(json!({"memories": &visible, "count": visible.len()})).into_response()
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

pub async fn search_memories(
    State(app): State<AppState>,
    Query(p): Query<SearchQuery>,
) -> impl IntoResponse {
    // #891: source_uri-only queries are valid (Gap 6 #889 reciprocal
    // queries). Reject only when BOTH q and source_uri are empty.
    let source_uri_empty = p.source_uri.as_deref().is_none_or(|s| s.trim().is_empty());
    if p.q.trim().is_empty() && source_uri_empty {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "query or source_uri is required"})),
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
            // #869 audit (Category B — safe default): missing `tags`
            // querystring collapses to empty `Vec<String>` which the
            // SAL `Filter` treats as "no tag filter" — documented.
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
            // #910 — tenant-facing path; never bypass the visibility filter.
            bypass_visibility: false,
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
    // v0.7.0 Provenance Gap 6 (#889) — `?source_uri=X` reciprocal
    // filter. Composes with `?q=…`; when `q` is empty + `source_uri`
    // is set, routes through the index-only `list_by_source_uri`
    // path so callers can ask "give me every memory from this
    // document" without typing a search query.
    let source_uri = p
        .source_uri
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(uri) = source_uri {
        if let Err(e) = validate::validate_source_uri(uri) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid source_uri filter: {e}")})),
            )
                .into_response();
        }
        if p.q.trim().is_empty() {
            return match db::list_by_source_uri(&lock.0, uri, p.namespace.as_deref(), Some(limit)) {
                Ok(r) => {
                    Json(json!({"results": r, "count": r.len(), "source_uri": uri})).into_response()
                }
                Err(e) => {
                    tracing::error!("handler error: {e}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "internal server error"})),
                    )
                        .into_response()
                }
            };
        }
    }
    match db::search_with_source_uri(
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
        source_uri,
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
    headers: HeaderMap,
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

    // #910 SAL-level — resolve the caller so the per-row metadata
    // stamp matches the authenticated principal. Pre-#910 the bulk
    // path stored `body.metadata` verbatim, so rows landed with no
    // agent_id and the subsequent list/get round-trip via the
    // scope=private filter dropped every one of them. Header-only
    // authentication; anonymous callers stamp `anonymous:req-<uuid>`.
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let caller = crate::identity::resolve_http_agent_id(None, header_agent_id)
        .unwrap_or_else(|_| format!("anonymous:req-{}", uuid::Uuid::new_v4()));

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
            // #910 — stamp metadata.agent_id from the resolved caller
            // so the SAL visibility filter recognises the row as
            // owned by the writer on later get/list/recall.
            let mut metadata_stamped = body.metadata;
            if let Some(obj) = metadata_stamped.as_object_mut() {
                obj.insert(
                    "agent_id".to_string(),
                    serde_json::Value::String(caller.clone()),
                );
            }
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
                metadata: metadata_stamped,
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
                version: 1,
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
            // #910 — stamp metadata.agent_id from the resolved caller
            // (sqlite branch mirror of the postgres branch above).
            let mut metadata_stamped = body.metadata;
            if let Some(obj) = metadata_stamped.as_object_mut() {
                obj.insert(
                    "agent_id".to_string(),
                    serde_json::Value::String(caller.clone()),
                );
            }
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
                metadata: metadata_stamped,
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
                version: 1,
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

// ===========================================================================
// #868 — inline tests for `handlers/http.rs`.
//
// The code-review verdict pinned `handlers/http.rs` for "0 inline tests
// across remaining prod LOC". This module establishes the discipline:
// one focused test per #866 stage helper so the next refactor has
// shape-pinning. Not aiming for 100% coverage — the integration suite
// under `tests/` already exercises the orchestrated path end-to-end.
//
// Coverage map (10 tests):
//   - resolve_create_agent_id    (4) header / body / metadata / fallback
//   - resolve_create_conflict_title (3) error → 409, version → suffix, merge → passthrough
//   - embed_create_before_lock   (1) no embedder ⇒ (None, Indexed)
//   - validate_create early-return (1) empty title ⇒ 400
//   - GovernanceRefusal downcast (1) → 403 + GOVERNANCE_REFUSED code
// ===========================================================================
