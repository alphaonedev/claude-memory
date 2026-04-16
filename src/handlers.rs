// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use axum::{
    Json,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::IntoResponse,
};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::config::ResolvedTtl;
use crate::db;
use crate::models::{
    CreateMemory, ForgetQuery, LinkBody, ListQuery, Memory, MemoryLink, RecallBody, RecallQuery,
    SearchQuery, Tier, UpdateMemory,
};
use crate::validate;

pub type Db = Arc<Mutex<(rusqlite::Connection, std::path::PathBuf, ResolvedTtl, bool)>>;

const MAX_BULK_SIZE: usize = 1000;

/// Shared state for API key authentication middleware.
#[derive(Clone)]
pub struct ApiKeyState {
    pub key: Option<String>,
}

/// Middleware: reject requests with 401 if `api_key` is configured and request
/// doesn't provide a matching `X-API-Key` header or `?api_key=` query param.
/// The `/api/v1/health` endpoint is exempt.
pub async fn api_key_auth(
    State(auth): State<ApiKeyState>,
    req: Request,
    next: Next,
) -> impl IntoResponse {
    let Some(ref expected) = auth.key else {
        // No API key configured — allow all requests
        return next.run(req).await.into_response();
    };

    // Exempt health endpoint
    if req.uri().path() == "/api/v1/health" {
        return next.run(req).await.into_response();
    }

    // Check X-API-Key header
    if let Some(header_val) = req.headers().get("x-api-key")
        && let Ok(val) = header_val.to_str()
        && val == expected.as_str()
    {
        return next.run(req).await.into_response();
    }

    // Check ?api_key= query param
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(val) = pair.strip_prefix("api_key=")
                && val == expected.as_str()
            {
                return next.run(req).await.into_response();
            }
        }
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "missing or invalid API key"})),
    )
        .into_response()
}

pub async fn health(State(state): State<Db>) -> impl IntoResponse {
    let lock = state.lock().await;
    let ok = db::health_check(&lock.0).unwrap_or(false);
    let code = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(json!({"status": if ok { "ok" } else { "error" }, "service": "ai-memory"})),
    )
        .into_response()
}

pub async fn create_memory(
    State(state): State<Db>,
    headers: HeaderMap,
    Json(body): Json<CreateMemory>,
) -> impl IntoResponse {
    if let Err(e) = validate::validate_create(&body) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    // Resolve agent_id via the HTTP precedence chain (body → X-Agent-Id → per-request anonymous)
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let agent_id =
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
    let mut metadata = body.metadata;
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert("agent_id".to_string(), serde_json::Value::String(agent_id));
    }

    let now = Utc::now();
    let lock = state.lock().await;
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
        metadata,
    };

    // Check for contradictions
    let contradictions =
        db::find_contradictions(&lock.0, &mem.title, &mem.namespace).unwrap_or_default();
    let contradiction_ids: Vec<String> = contradictions
        .iter()
        .filter(|c| c.id != mem.id)
        .map(|c| c.id.clone())
        .collect();

    match db::insert(&lock.0, &mem) {
        Ok(actual_id) => {
            let mut response = json!({"id": actual_id, "tier": mem.tier, "namespace": mem.namespace, "title": mem.title});
            if !contradiction_ids.is_empty() {
                response["potential_contradictions"] = json!(contradiction_ids);
            }
            (StatusCode::CREATED, Json(response)).into_response()
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

pub async fn get_memory(State(state): State<Db>, Path(id): Path<String>) -> impl IntoResponse {
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let lock = state.lock().await;
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

pub async fn update_memory(
    State(state): State<Db>,
    Path(id): Path<String>,
    Json(body): Json<UpdateMemory>,
) -> impl IntoResponse {
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

pub async fn delete_memory(State(state): State<Db>, Path(id): Path<String>) -> impl IntoResponse {
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let lock = state.lock().await;
    // Try exact delete first; fall back to prefix resolution
    match db::delete(&lock.0, &id) {
        Ok(true) => Json(json!({"deleted": true})).into_response(),
        Ok(false) => {
            // Prefix fallback
            match db::get_by_prefix(&lock.0, &id) {
                Ok(Some(mem)) => {
                    let full_id = mem.id;
                    match db::delete(&lock.0, &full_id) {
                        Ok(true) => Json(json!({"deleted": true})).into_response(),
                        _ => (StatusCode::NOT_FOUND, Json(json!({"error": "not found"})))
                            .into_response(),
                    }
                }
                Ok(None) => {
                    (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("ambiguous ID prefix") {
                        (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response()
                    } else {
                        tracing::error!("handler error: {e}");
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": "internal server error"})),
                        )
                            .into_response()
                    }
                }
            }
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

pub async fn promote_memory(State(state): State<Db>, Path(id): Path<String>) -> impl IntoResponse {
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
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
    State(state): State<Db>,
    Query(p): Query<ListQuery>,
) -> impl IntoResponse {
    let lock = state.lock().await;
    let limit = p.limit.unwrap_or(20).min(200);
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
    State(state): State<Db>,
    Query(p): Query<SearchQuery>,
) -> impl IntoResponse {
    if p.q.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "query is required"})),
        )
            .into_response();
    }
    let lock = state.lock().await;
    let limit = p.limit.unwrap_or(20).min(200);
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

pub async fn recall_memories_get(
    State(state): State<Db>,
    Query(p): Query<RecallQuery>,
) -> impl IntoResponse {
    let ctx = p.context.unwrap_or_default();
    if ctx.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "context is required"})),
        )
            .into_response();
    }
    let lock = state.lock().await;
    let limit = p.limit.unwrap_or(10).min(50);
    match db::recall(
        &lock.0,
        &ctx,
        p.namespace.as_deref(),
        limit,
        p.tags.as_deref(),
        p.since.as_deref(),
        p.until.as_deref(),
        lock.2.short_extend_secs,
        lock.2.mid_extend_secs,
    ) {
        Ok(r) => {
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
            Json(json!({"memories": scored, "count": scored.len()})).into_response()
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

pub async fn recall_memories_post(
    State(state): State<Db>,
    Json(body): Json<RecallBody>,
) -> impl IntoResponse {
    if body.context.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "context is required"})),
        )
            .into_response();
    }
    let lock = state.lock().await;
    let limit = body.limit.unwrap_or(10).min(50);
    match db::recall(
        &lock.0,
        &body.context,
        body.namespace.as_deref(),
        limit,
        body.tags.as_deref(),
        body.since.as_deref(),
        body.until.as_deref(),
        lock.2.short_extend_secs,
        lock.2.mid_extend_secs,
    ) {
        Ok(r) => {
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
            Json(json!({"memories": scored, "count": scored.len()})).into_response()
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
    State(state): State<Db>,
    Json(body): Json<ForgetQuery>,
) -> impl IntoResponse {
    let lock = state.lock().await;
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

pub async fn list_namespaces(State(state): State<Db>) -> impl IntoResponse {
    let lock = state.lock().await;
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

pub async fn create_link(State(state): State<Db>, Json(body): Json<LinkBody>) -> impl IntoResponse {
    if let Err(e) = validate::validate_link(&body.source_id, &body.target_id, &body.relation) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let lock = state.lock().await;
    match db::create_link(&lock.0, &body.source_id, &body.target_id, &body.relation) {
        Ok(()) => (StatusCode::CREATED, Json(json!({"linked": true}))).into_response(),
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

pub async fn get_links(State(state): State<Db>, Path(id): Path<String>) -> impl IntoResponse {
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let lock = state.lock().await;
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

pub async fn get_stats(State(state): State<Db>) -> impl IntoResponse {
    let lock = state.lock().await;
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

pub async fn run_gc(State(state): State<Db>) -> impl IntoResponse {
    let lock = state.lock().await;
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

pub async fn export_memories(State(state): State<Db>) -> impl IntoResponse {
    let lock = state.lock().await;
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
    State(state): State<Db>,
    Json(body): Json<ImportBody>,
) -> impl IntoResponse {
    if body.memories.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("import limited to {} memories", MAX_BULK_SIZE)})),
        )
            .into_response();
    }
    let lock = state.lock().await;
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
        if validate::validate_link(&link.source_id, &link.target_id, &link.relation).is_err() {
            continue;
        }
        let _ = db::create_link(&lock.0, &link.source_id, &link.target_id, &link.relation);
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
    pub summary: String,
    #[serde(default = "default_ns")]
    pub namespace: String,
    #[serde(default)]
    pub tier: Option<Tier>,
    /// Optional `agent_id` for the consolidator (attributable on the result).
    /// If unset, resolved from `X-Agent-Id` header or per-request anonymous id.
    #[serde(default)]
    pub agent_id: Option<String>,
}
fn default_ns() -> String {
    "global".to_string()
}

pub async fn consolidate_memories(
    State(state): State<Db>,
    headers: HeaderMap,
    Json(body): Json<ConsolidateBody>,
) -> impl IntoResponse {
    if let Err(e) =
        validate::validate_consolidate(&body.ids, &body.title, &body.summary, &body.namespace)
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
    let lock = state.lock().await;
    let tier = body.tier.unwrap_or(Tier::Long);
    match db::consolidate(
        &lock.0,
        &body.ids,
        &body.title,
        &body.summary,
        &body.namespace,
        &tier,
        "consolidation",
        &consolidator_agent_id,
    ) {
        Ok(new_id) => (
            StatusCode::CREATED,
            Json(json!({"id": new_id, "consolidated": body.ids.len()})),
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

pub async fn bulk_create(
    State(state): State<Db>,
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
    let lock = state.lock().await;
    let mut created = 0usize;
    let mut errors = Vec::new();
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
        };
        match db::insert(&lock.0, &mem) {
            Ok(_) => created += 1,
            Err(e) => errors.push(e.to_string()),
        }
    }
    Json(json!({"created": created, "errors": errors})).into_response()
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
    State(state): State<Db>,
    Query(q): Query<ArchiveListQuery>,
) -> impl IntoResponse {
    let lock = state.lock().await;
    let limit = q.limit.unwrap_or(50).min(1000);
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

pub async fn restore_archive(State(state): State<Db>, Path(id): Path<String>) -> impl IntoResponse {
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let lock = state.lock().await;
    match db::restore_archived(&lock.0, &id) {
        Ok(true) => Json(json!({"restored": true, "id": id})).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "not found in archive"})),
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

#[derive(Debug, Deserialize)]
pub struct PurgeQuery {
    pub older_than_days: Option<i64>,
}

pub async fn purge_archive(
    State(state): State<Db>,
    Query(q): Query<PurgeQuery>,
) -> impl IntoResponse {
    let lock = state.lock().await;
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

pub async fn archive_stats(State(state): State<Db>) -> impl IntoResponse {
    let lock = state.lock().await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> Db {
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let path = std::path::PathBuf::from(":memory:");
        Arc::new(Mutex::new((conn, path, ResolvedTtl::default(), true)))
    }

    #[tokio::test]
    async fn health_returns_ok() {
        let state = test_state();
        let lock = state.lock().await;
        let ok = db::health_check(&lock.0).unwrap_or(false);
        assert!(ok);
    }

    #[tokio::test]
    async fn store_and_retrieve_via_state() {
        let state = test_state();
        let lock = state.lock().await;
        let now = Utc::now();
        let mem = Memory {
            id: Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "test".into(),
            title: "Handler test".into(),
            content: "Testing handlers.".into(),
            tags: vec!["test".into()],
            priority: 7,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
        };
        let id = db::insert(&lock.0, &mem).unwrap();
        let got = db::get(&lock.0, &id).unwrap().unwrap();
        assert_eq!(got.title, "Handler test");
    }

    #[tokio::test]
    async fn recall_via_state() {
        let state = test_state();
        let lock = state.lock().await;
        let now = Utc::now();
        let mem = Memory {
            id: Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "test".into(),
            title: "Recall handler test".into(),
            content: "Content for recall.".into(),
            tags: vec![],
            priority: 8,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
        };
        db::insert(&lock.0, &mem).unwrap();
        let results = db::recall(
            &lock.0,
            "recall handler",
            Some("test"),
            10,
            None,
            None,
            None,
            crate::models::SHORT_TTL_EXTEND_SECS,
            crate::models::MID_TTL_EXTEND_SECS,
        )
        .unwrap();
        assert!(!results.is_empty());
        assert!(results[0].1 > 0.0); // has score
    }

    #[tokio::test]
    async fn stats_via_state() {
        let state = test_state();
        let lock = state.lock().await;
        let path = std::path::Path::new(":memory:");
        let s = db::stats(&lock.0, path).unwrap();
        assert_eq!(s.total, 0);
    }

    #[tokio::test]
    async fn bulk_size_limit() {
        assert_eq!(MAX_BULK_SIZE, 1000);
    }

    #[tokio::test]
    async fn list_empty_namespace() {
        let state = test_state();
        let lock = state.lock().await;
        let results = db::list(
            &lock.0,
            Some("nonexistent"),
            None,
            10,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn create_and_update_with_metadata() {
        let state = test_state();
        let lock = state.lock().await;
        let now = Utc::now();

        // Create with metadata
        let mem = Memory {
            id: Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "test".into(),
            title: "HTTP metadata test".into(),
            content: "Testing metadata through handler layer.".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "api".into(),
            access_count: 0,
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"http_test": true, "version": 1}),
        };
        let id = db::insert(&lock.0, &mem).unwrap();

        // Verify metadata persisted
        let got = db::get(&lock.0, &id).unwrap().unwrap();
        assert_eq!(got.metadata["http_test"], true);
        assert_eq!(got.metadata["version"], 1);

        // Update metadata via db::update (same path as update_memory handler)
        let new_meta =
            serde_json::json!({"http_test": true, "version": 2, "updated_by": "handler"});
        let (found, _) = db::update(
            &lock.0,
            &id,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&new_meta),
        )
        .unwrap();
        assert!(found);

        // Verify updated metadata
        let got = db::get(&lock.0, &id).unwrap().unwrap();
        assert_eq!(got.metadata["version"], 2);
        assert_eq!(got.metadata["updated_by"], "handler");
    }

    // --- API key auth middleware tests ---

    use axum::{Router, body::Body, routing::get as axum_get};
    use tower::ServiceExt as _;

    async fn dummy_handler() -> impl IntoResponse {
        (StatusCode::OK, "ok")
    }

    fn auth_app(api_key: Option<&str>) -> Router {
        let auth_state = ApiKeyState {
            key: api_key.map(String::from),
        };
        Router::new()
            .route("/api/v1/health", axum_get(dummy_handler))
            .route("/api/v1/memories", axum_get(dummy_handler))
            .layer(axum::middleware::from_fn_with_state(
                auth_state,
                api_key_auth,
            ))
    }

    #[tokio::test]
    async fn api_key_no_key_configured_allows_all() {
        let app = auth_app(None);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/memories")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_key_valid_header_allows() {
        let app = auth_app(Some("secret123"));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/memories")
                    .header("x-api-key", "secret123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_key_invalid_header_rejected() {
        let app = auth_app(Some("secret123"));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/memories")
                    .header("x-api-key", "wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_key_missing_header_rejected() {
        let app = auth_app(Some("secret123"));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/memories")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_key_valid_query_param_allows() {
        let app = auth_app(Some("secret123"));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/memories?api_key=secret123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn api_key_health_exempt() {
        let app = auth_app(Some("secret123"));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
