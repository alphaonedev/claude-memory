// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! HTTP handlers for the v0.7.0 knowledge-graph + entity surface (#650
//! follow-up per-domain split). Each handler is a thin Axum-layer
//! wrapper around the SAL `MemoryStore` trait (postgres path) or the
//! legacy `db::*` API (sqlite path), shaping the result into the
//! canonical wire envelope.
//!
//! All handlers were extracted verbatim from `src/handlers/http.rs`
//! (commit `12e1253`, lines 4169-5013 + 5192-5419); wire compatibility
//! is preserved via the `pub use kg::*` re-export from
//! `src/handlers/mod.rs`. The split keeps the kg/entity domain in
//! a single ~1 100-line module while shrinking the legacy
//! `handlers/http.rs` toward the long-term ≤600-LOC target.
//!
//! Functions in this module:
//!   - `entity_register`        (POST /api/v1/entities)
//!   - `entity_get_by_alias`    (GET  /api/v1/entities/by_alias)
//!   - `kg_timeline`            (GET  /api/v1/kg/timeline)
//!   - `kg_invalidate`          (POST /api/v1/kg/invalidate)
//!   - `kg_find_paths`          (POST /api/v1/kg/find_paths)
//!   - `kg_query`               (POST /api/v1/kg/query)

#![allow(clippy::too_many_lines)]

#[cfg(feature = "sal")]
use crate::models::ConfidenceSource;
use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
#[cfg(feature = "sal")]
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
#[cfg(feature = "sal")]
use uuid::Uuid;

use crate::db;
#[cfg(feature = "sal")]
use crate::models::{Memory, Tier};
use crate::validate;

use super::AppState;
#[cfg(feature = "sal")]
use super::StorageBackend;
#[cfg(feature = "sal")]
use super::store_err_to_response;

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
                // #869 audit (Category B — safe default): a missing
                // `aliases` field or a non-entity row collapses to
                // empty `Vec<String>`, which is the documented
                // "first-time registration" path (no prior aliases to
                // union against the new ones).
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
            confidence_source: ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
            version: 1,
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
                    // #869 audit (Category B — safe default): an entity
                    // with no `aliases` array collapses to empty
                    // `Vec<String>`; the lookup falls through to the
                    // `m.title.eq_ignore_ascii_case(alias)` branch.
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
    headers: HeaderMap,
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

    // #910 SAL-level — resolve the caller so the trait method's
    // visibility filter (path-traversal flavour) sees the right
    // principal. Header-only authentication on this POST surface;
    // anonymous callers get a per-request `anonymous:req-…` id.
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

    #[cfg(feature = "sal")]
    {
        let ctx = crate::store::CallerContext::for_agent(&caller);
        return match app
            .store
            .find_paths(
                &ctx,
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
        let _ = caller;
        (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "find_paths requires --features sal"})),
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

/// #910 (security-medium, 2026-05-19) — apply the scope=private
/// visibility filter on `POST /api/v1/kg/query` traversal results.
/// Pre-#910 the handler returned every reachable target node from
/// the recursive-CTE / AGE Cypher walk; a target whose
/// `metadata.scope == "private"` was visible to any caller who could
/// pass `kg_query` validation, including callers other than the
/// target's `metadata.agent_id` owner. The fix mirrors the post-
/// filter applied in `memories_query::list_memories` — a row is
/// visible iff `metadata.scope != "private"` OR
/// `metadata.agent_id == caller`. Rows we cannot fetch (deleted
/// since the traversal, in another namespace the caller cannot
/// read, etc.) fail-closed (excluded).
#[cfg(feature = "sal-postgres")]
async fn kg_query_filter_visible(
    app: &AppState,
    caller: &str,
    target_ids: Vec<String>,
) -> std::collections::HashSet<String> {
    use std::collections::HashSet;
    let mut visible: HashSet<String> = HashSet::with_capacity(target_ids.len());
    let ctx = crate::store::CallerContext::for_agent(caller);
    for id in target_ids {
        if let Ok(mem) = app.store.get(&ctx, &id).await {
            let scope = mem
                .metadata
                .get("scope")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("private");
            let owner = mem
                .metadata
                .get("agent_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if scope != "private" || owner == caller {
                visible.insert(id);
            }
        }
    }
    visible
}

/// `POST /api/v1/kg/query` — REST mirror of the MCP `memory_kg_query`
/// tool. Returns outbound multi-hop traversal from `source_id` (1..=5
/// hops) filtered by the temporal/agent windows. 400 for invalid
/// IDs/timestamps; 422 when `max_depth` exceeds the supported ceiling
/// (clearer than 500 for what is a documented limitation, not an
/// internal error).
pub async fn kg_query(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<KgQueryBody>,
) -> impl IntoResponse {
    // #910 (security-medium, 2026-05-19) — resolve the caller via the
    // `X-Agent-Id` header so the scope=private visibility filter
    // below has a known principal to compare `metadata.agent_id`
    // against. Pre-#910 `kg_query` returned every reachable target
    // node regardless of the target memory's `metadata.scope` — a
    // caller could enumerate scope=private targets owned by other
    // agents by walking from a public source row. Anonymous callers
    // get a per-request `anonymous:req-…` id and see only
    // non-private targets.
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

    // S82's wire shape sends `from` instead of `source_id`; resolve
    // the canonical id from either field with `source_id` taking
    // precedence when both are supplied.
    //
    // #869 audit (Category B — safe default): empty `String` flows
    // into `validate_id` below which returns a typed 400 with the
    // "invalid source_id" envelope.
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
                // #910 — fetch each target's metadata, filter by the
                // scope=private visibility rule (see
                // `kg_query_filter_visible`). Pre-#910 every reachable
                // target was returned verbatim regardless of the
                // target's owner / scope.
                let target_ids: Vec<String> = nodes.iter().map(|n| n.target_id.clone()).collect();
                let visible = kg_query_filter_visible(&app, &caller, target_ids).await;
                let nodes: Vec<_> = nodes
                    .into_iter()
                    .filter(|n| visible.contains(&n.target_id))
                    .collect();

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

    // #910 — apply scope=private visibility filter on the SQLite path
    // too. The kg_query DB function returns the full reachable
    // topology with target metadata absent from the row shape; we
    // post-fetch each target's `metadata.scope` / `metadata.agent_id`
    // inside the same lock window so the filter sees an atomic view
    // of the traversal.
    let lock = app.db.lock().await;
    let kg_res = db::kg_query(
        &lock.0,
        &source_id,
        max_depth,
        valid_at,
        allowed_agents.as_deref(),
        body.limit,
        body.include_invalidated,
    );
    let nodes_opt = match &kg_res {
        Ok(nodes) => {
            let mut visible: std::collections::HashSet<String> =
                std::collections::HashSet::with_capacity(nodes.len());
            for n in nodes {
                if let Ok(Some(mem)) = db::get(&lock.0, &n.target_id) {
                    let scope = mem
                        .metadata
                        .get("scope")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("private");
                    let owner = mem
                        .metadata
                        .get("agent_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    if scope != "private" || owner == caller {
                        visible.insert(n.target_id.clone());
                    }
                }
            }
            Some(visible)
        }
        Err(_) => None,
    };
    drop(lock);
    match kg_res {
        Ok(nodes) => {
            let visible = nodes_opt.unwrap_or_default();
            let nodes: Vec<_> = nodes
                .into_iter()
                .filter(|n| visible.contains(&n.target_id))
                .collect();
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
