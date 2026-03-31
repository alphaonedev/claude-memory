use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::{Duration, Utc};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::db;
use crate::models::*;

pub type Db = Arc<Mutex<(rusqlite::Connection, std::path::PathBuf)>>;

pub async fn health() -> impl IntoResponse {
    Json(json!({"status": "ok", "service": "claude-memory"}))
}

pub async fn create_memory(
    State(state): State<Db>,
    Json(body): Json<CreateMemory>,
) -> impl IntoResponse {
    if body.content.len() > MAX_CONTENT_SIZE {
        return (StatusCode::BAD_REQUEST, Json(json!({
            "error": format!("content exceeds max size of {} bytes", MAX_CONTENT_SIZE)
        }))).into_response();
    }
    let now = Utc::now();
    let expires_at = body.expires_at.or_else(|| {
        body.ttl_secs
            .or(body.tier.default_ttl_secs())
            .map(|secs| (now + Duration::seconds(secs)).to_rfc3339())
    });
    let mem = Memory {
        id: Uuid::new_v4().to_string(),
        tier: body.tier,
        namespace: body.namespace,
        title: body.title,
        content: body.content,
        tags: body.tags,
        priority: body.priority.clamp(1, 10),
        access_count: 0,
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
        last_accessed_at: None,
        expires_at,
    };
    let lock = state.lock().await;
    match db::insert(&lock.0, &mem) {
        Ok(()) => (StatusCode::CREATED, Json(json!(mem))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn get_memory(
    State(state): State<Db>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let lock = state.lock().await;
    match db::get(&lock.0, &id) {
        Ok(Some(mem)) => Json(json!(mem)).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn update_memory(
    State(state): State<Db>,
    Path(id): Path<String>,
    Json(body): Json<UpdateMemory>,
) -> impl IntoResponse {
    if let Some(ref c) = body.content {
        if c.len() > MAX_CONTENT_SIZE {
            return (StatusCode::BAD_REQUEST, Json(json!({
                "error": format!("content exceeds max size of {} bytes", MAX_CONTENT_SIZE)
            }))).into_response();
        }
    }
    let lock = state.lock().await;
    match db::update(
        &lock.0, &id,
        body.title.as_deref(), body.content.as_deref(),
        body.tier.as_ref(), body.namespace.as_deref(),
        body.tags.as_ref(), body.priority, body.expires_at.as_deref(),
    ) {
        Ok(true) => {
            let mem = db::get(&lock.0, &id).ok().flatten();
            Json(json!(mem)).into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn delete_memory(
    State(state): State<Db>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let lock = state.lock().await;
    match db::delete(&lock.0, &id) {
        Ok(true) => Json(json!({"deleted": true})).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn list_memories(
    State(state): State<Db>,
    Query(params): Query<ListQuery>,
) -> impl IntoResponse {
    let lock = state.lock().await;
    let limit = params.limit.unwrap_or(20).min(200);
    let offset = params.offset.unwrap_or(0);
    match db::list(&lock.0, params.namespace.as_deref(), params.tier.as_ref(), limit, offset, params.min_priority) {
        Ok(memories) => Json(json!({"memories": memories, "count": memories.len()})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn search_memories(
    State(state): State<Db>,
    Query(params): Query<SearchQuery>,
) -> impl IntoResponse {
    if params.q.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "query is required"}))).into_response();
    }
    let lock = state.lock().await;
    let limit = params.limit.unwrap_or(20).min(200);
    match db::search(&lock.0, &params.q, params.namespace.as_deref(), params.tier.as_ref(), limit, params.min_priority) {
        Ok(results) => Json(json!({"results": results, "count": results.len(), "query": params.q})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

/// GET /recall — for short queries via query params
pub async fn recall_memories_get(
    State(state): State<Db>,
    Query(params): Query<RecallQuery>,
) -> impl IntoResponse {
    let context = params.context.unwrap_or_default();
    if context.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "context is required"}))).into_response();
    }
    let lock = state.lock().await;
    let limit = params.limit.unwrap_or(10).min(50);
    match db::recall(&lock.0, &context, params.namespace.as_deref(), limit) {
        Ok(results) => Json(json!({"memories": results, "count": results.len()})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

/// POST /recall — for long context strings via request body
pub async fn recall_memories_post(
    State(state): State<Db>,
    Json(body): Json<RecallBody>,
) -> impl IntoResponse {
    if body.context.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "context is required"}))).into_response();
    }
    let lock = state.lock().await;
    let limit = body.limit.unwrap_or(10).min(50);
    match db::recall(&lock.0, &body.context, body.namespace.as_deref(), limit) {
        Ok(results) => Json(json!({"memories": results, "count": results.len()})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn list_namespaces(State(state): State<Db>) -> impl IntoResponse {
    let lock = state.lock().await;
    match db::list_namespaces(&lock.0) {
        Ok(namespaces) => Json(json!({"namespaces": namespaces})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn get_stats(State(state): State<Db>) -> impl IntoResponse {
    let lock = state.lock().await;
    match db::stats(&lock.0, &lock.1) {
        Ok(stats) => Json(json!(stats)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn run_gc(State(state): State<Db>) -> impl IntoResponse {
    let lock = state.lock().await;
    match db::gc(&lock.0) {
        Ok(count) => Json(json!({"expired_deleted": count})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn export_memories(State(state): State<Db>) -> impl IntoResponse {
    let lock = state.lock().await;
    match db::export_all(&lock.0) {
        Ok(memories) => Json(json!({"memories": memories, "count": memories.len(), "exported_at": Utc::now().to_rfc3339()})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

pub async fn import_memories(
    State(state): State<Db>,
    Json(body): Json<ImportBody>,
) -> impl IntoResponse {
    let lock = state.lock().await;
    let mut imported = 0usize;
    let mut errors = Vec::new();
    for mem in body.memories {
        match db::insert(&lock.0, &mem) {
            Ok(()) => imported += 1,
            Err(e) => errors.push(format!("{}: {}", mem.id, e)),
        }
    }
    Json(json!({"imported": imported, "errors": errors})).into_response()
}

#[derive(serde::Deserialize)]
pub struct ImportBody {
    pub memories: Vec<Memory>,
}

pub async fn bulk_create(
    State(state): State<Db>,
    Json(bodies): Json<Vec<CreateMemory>>,
) -> impl IntoResponse {
    let now = Utc::now();
    let lock = state.lock().await;
    let mut created = 0usize;
    let mut errors = Vec::new();
    for body in bodies {
        if body.content.len() > MAX_CONTENT_SIZE {
            errors.push(format!("{}: content exceeds max size", body.title));
            continue;
        }
        let expires_at = body.expires_at.or_else(|| {
            body.ttl_secs
                .or(body.tier.default_ttl_secs())
                .map(|secs| (now + Duration::seconds(secs)).to_rfc3339())
        });
        let mem = Memory {
            id: Uuid::new_v4().to_string(),
            tier: body.tier,
            namespace: body.namespace,
            title: body.title,
            content: body.content,
            tags: body.tags,
            priority: body.priority.clamp(1, 10),
            access_count: 0,
            created_at: now.to_rfc3339(),
            updated_at: now.to_rfc3339(),
            last_accessed_at: None,
            expires_at,
        };
        match db::insert(&lock.0, &mem) {
            Ok(()) => created += 1,
            Err(e) => errors.push(e.to_string()),
        }
    }
    Json(json!({"created": created, "errors": errors})).into_response()
}
