// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Admin HTTP handlers — agent registration, quota, stats, gc, export,
//! import, and the parity `tools/list` mirror.
//!
//! Extracted from [`super::http`] under issue #650 follow-up 2. The
//! handler bodies are unchanged; only the module-routing import surface
//! moved. Wire compatibility preserved via `pub use admin::*` in
//! [`super`].

#![allow(clippy::too_many_lines)]

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
#[cfg(feature = "sal")]
use uuid::Uuid;

use crate::db;
#[cfg(feature = "sal")]
use crate::models::{ConfidenceSource, Tier};
use crate::models::{Memory, MemoryLink, RegisterAgentBody};
use crate::validate;

use super::AppState;
use super::MAX_BULK_SIZE;
#[cfg(feature = "sal")]
use super::StorageBackend;
#[cfg(feature = "sal")]
use super::store_err_to_response;

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

    // v0.7.0 Wave-3 Continuation 3 — postgres-backed daemons route the
    // agent-registration write through `app.store` so the row lands in
    // the same postgres `_agents` namespace that `list_agents` projects
    // from. Pre-fix this handler wrote through `db::register_agent`
    // against the sqlite scratch `app.db`, leaving postgres-backed
    // daemons with POST→sqlite and GET→postgres asymmetry — registered
    // agents never appeared in the list. Mirrors the import_memories +
    // bulk_create dual-backend dispatch pattern. Federation fanout
    // remains sqlite-only (broadcast_store_quorum uses sqlite-coupled
    // fed-tracker state).
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("daemon");
        let now = Utc::now().to_rfc3339();
        let mut metadata = json!({
            "agent_id": &body.agent_id,
            "agent_type": &body.agent_type,
        });
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert(
                "capabilities".to_string(),
                serde_json::to_value(&capabilities).unwrap_or_else(|_| json!([])),
            );
        }
        let agent_mem = Memory {
            id: Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "_agents".to_string(),
            title: format!("agent:{}", &body.agent_id),
            content: format!("agent registration for {}", &body.agent_id),
            tags: vec!["_agent_registration".to_string()],
            priority: 5,
            confidence: 1.0,
            source: "api".to_string(),
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
        };
        return match app.store.store(&ctx, &agent_mem).await {
            Ok(id) => (
                StatusCode::CREATED,
                Json(json!({
                    "id": id,
                    "agent_id": body.agent_id,
                    "agent_type": body.agent_type,
                    "capabilities": capabilities,
                    "storage_backend": "postgres",
                })),
            )
                .into_response(),
            Err(e) => store_err_to_response(e),
        };
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
                // Issue #851: never echo the raw `e` to the wire paired
                // with the user-supplied id (the combo reflects the
                // caller's request). Sanitize + log instead.
                tracing::warn!(
                    "import_memories(postgres): validate_memory failed for {}: {e}",
                    mem.id
                );
                errors.push(super::sanitize_bulk_row_error(&e.to_string()).to_string());
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
                Err(e) => {
                    // Issue #851: SAL `store.store` errors can carry raw
                    // sqlx/sqlite text — sanitize before echoing.
                    tracing::warn!(
                        "import_memories(postgres): store.store failed for {}: {e}",
                        mem.id
                    );
                    errors.push(super::sanitize_bulk_row_error(&e.to_string()).to_string());
                }
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
            // Issue #851: never echo `<id>: <validate error>` paired —
            // the combo reflects the caller's request and the inner
            // string can carry validate template detail. Sanitize + log.
            tracing::warn!(
                "import_memories: validate_memory failed for {}: {e}",
                mem.id
            );
            errors.push(super::sanitize_bulk_row_error(&e.to_string()).to_string());
            continue;
        }
        match db::insert(&lock.0, &mem) {
            Ok(_) => imported += 1,
            Err(e) => {
                // Issue #851: db::insert errors include raw rusqlite
                // text (SQL fragments, constraint names). Sanitize.
                tracing::warn!("import_memories: db::insert failed for {}: {e}", mem.id);
                errors.push(super::sanitize_bulk_row_error(&e.to_string()).to_string());
            }
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
