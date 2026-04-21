// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use axum::{
    Json,
    extract::{FromRef, Path, Query, Request, State},
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
use crate::embeddings::Embedder;
use crate::hnsw::VectorIndex;
use crate::models::{
    CreateMemory, ForgetQuery, LinkBody, ListQuery, Memory, MemoryLink, RecallBody, RecallQuery,
    RegisterAgentBody, SearchQuery, Tier, UpdateMemory,
};
use crate::validate;

pub type Db = Arc<Mutex<(rusqlite::Connection, std::path::PathBuf, ResolvedTtl, bool)>>;

/// Composite daemon state (issue #219/v0.7 prep).
///
/// Previously the Axum router held only `Db`. Closing the HTTP embedding gap
/// (semantic recall silently missed HTTP-stored memories because the daemon
/// never generated embeddings) requires the embedder and the in-memory HNSW
/// index to be reachable from write handlers. We introduce `AppState` and
/// use `FromRef` so every existing `State<Db>` handler keeps working
/// unchanged — only the write paths opt into `State<AppState>` to pick up
/// the embedder and vector index.
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub embedder: Arc<Option<Embedder>>,
    pub vector_index: Arc<Mutex<Option<VectorIndex>>>,
    /// v0.7 federation config — `Some` when `--quorum-writes N` +
    /// `--quorum-peers` are configured at serve time. Writes fan out
    /// to peers via `FederationConfig::broadcast_store_quorum` when
    /// this is `Some`.
    pub federation: Arc<Option<crate::federation::FederationConfig>>,
}

impl FromRef<AppState> for Db {
    fn from_ref(app: &AppState) -> Self {
        app.db.clone()
    }
}

const MAX_BULK_SIZE: usize = 1000;

/// Shared state for API key authentication middleware.
#[derive(Clone)]
pub struct ApiKeyState {
    pub key: Option<String>,
}

/// Constant-time byte-slice equality. Doesn't short-circuit on the
/// first mismatched byte, preventing timing-oracle leaks of secret
/// material. Used for API-key comparison (#301 hardening item 3).
#[inline]
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
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
        && constant_time_eq(val.as_bytes(), expected.as_bytes())
    {
        return next.run(req).await.into_response();
    }

    // Check ?api_key= query param
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(val) = pair.strip_prefix("api_key=")
                && constant_time_eq(val.as_bytes(), expected.as_bytes())
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

/// v0.6.0.0 — Prometheus scrape endpoint. Refreshes gauge samples
/// (`ai_memory_memories`) against the current DB before rendering so
/// scrapers see up-to-date counts without needing a background refresh
/// task.
pub async fn prometheus_metrics(State(state): State<Db>) -> impl IntoResponse {
    {
        let lock = state.lock().await;
        if let Ok(stats) = db::stats(&lock.0, &lock.1) {
            crate::metrics::registry()
                .memories_gauge
                .set(stats.total.try_into().unwrap_or(i64::MAX));
        }
    }
    let body = crate::metrics::render();
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

#[allow(clippy::too_many_lines)]
pub async fn create_memory(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateMemory>,
) -> impl IntoResponse {
    let state = app.db.clone();
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

    // Issue #219: generate the embedding BEFORE taking the DB lock. Embedding
    // (MiniLM ONNX / nomic via Ollama) is 10-200ms of work we do not want
    // holding the single `Mutex<Connection>` on a multi-agent daemon.
    let embedding_text = format!("{} {}", body.title, body.content);
    let embedding: Option<Vec<f32>> =
        app.embedder
            .as_ref()
            .as_ref()
            .and_then(|emb| match emb.embed(&embedding_text) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("embedding generation failed: {e}");
                    None
                }
            });

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
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({
                        "status": "pending",
                        "pending_id": pending_id,
                        "reason": "governance requires approval",
                        "action": "store",
                        "namespace": mem.namespace,
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
    State(state): State<Db>,
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

    let lock = state.lock().await;
    match db::register_agent(&lock.0, &body.agent_id, &body.agent_type, &capabilities) {
        Ok(id) => (
            StatusCode::CREATED,
            Json(json!({
                "registered": true,
                "id": id,
                "agent_id": body.agent_id,
                "agent_type": body.agent_type,
                "capabilities": capabilities,
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

// ---------------------------------------------------------------------------
// Task 1.9 — pending_actions endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PendingListQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default = "default_pending_limit")]
    pub limit: Option<usize>,
}

#[allow(clippy::unnecessary_wraps)]
fn default_pending_limit() -> Option<usize> {
    Some(100)
}

pub async fn list_pending(
    State(state): State<Db>,
    Query(p): Query<PendingListQuery>,
) -> impl IntoResponse {
    let limit = p.limit.unwrap_or(100).min(1000);
    let lock = state.lock().await;
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

pub async fn approve_pending(
    State(state): State<Db>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    use crate::db::ApproveOutcome;
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
    let lock = state.lock().await;
    match db::approve_with_approver_type(&lock.0, &id, &agent_id) {
        Ok(ApproveOutcome::Approved) => match db::execute_pending_action(&lock.0, &id) {
            Ok(memory_id) => Json(json!({
                "approved": true,
                "id": id,
                "decided_by": agent_id,
                "executed": true,
                "memory_id": memory_id,
            }))
            .into_response(),
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
    State(state): State<Db>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
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
    let lock = state.lock().await;
    match db::decide_pending_action(&lock.0, &id, false, &agent_id) {
        Ok(true) => {
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

pub async fn list_agents(State(state): State<Db>) -> impl IntoResponse {
    let lock = state.lock().await;
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
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({
                        "status": "pending",
                        "pending_id": pending_id,
                        "reason": "governance requires approval",
                        "action": "delete",
                        "memory_id": target.id,
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
    // Drop DB lock before fanning out — peers POST back to our
    // sync_push and we'd deadlock on the shared Mutex if we held it.
    drop(lock);
    match delete_outcome {
        Ok(true) => {
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
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({
                        "status": "pending",
                        "pending_id": pending_id,
                        "reason": "governance requires approval",
                        "action": "promote",
                        "memory_id": target.id,
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
    State(state): State<Db>,
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
        p.as_agent.as_deref(),
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
    if let Some(ref a) = p.as_agent
        && let Err(e) = validate::validate_namespace(a)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid as_agent: {e}")})),
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
        p.as_agent.as_deref(),
        p.budget_tokens,
    ) {
        Ok((r, tokens_used)) => {
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
                "tokens_used": tokens_used,
            });
            if let Some(b) = p.budget_tokens {
                resp["budget_tokens"] = json!(b);
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
    if let Some(ref a) = body.as_agent
        && let Err(e) = validate::validate_namespace(a)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid as_agent: {e}")})),
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
        body.as_agent.as_deref(),
        body.budget_tokens,
    ) {
        Ok((r, tokens_used)) => {
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
                "tokens_used": tokens_used,
            });
            if let Some(b) = body.budget_tokens {
                resp["budget_tokens"] = json!(b);
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

// ---------------------------------------------------------------------------
// Phase 3 foundation (issue #224) — HTTP sync endpoints.
//
// These ship in v0.6.0 GA as SKELETONS running today's timestamp-aware merge
// (`db::insert_if_newer`). Field-level CRDT-lite merge rules, streaming,
// resume-on-interrupt, and per-peer auth tokens are v0.8.0 targets.
// ---------------------------------------------------------------------------

/// Request body for `POST /api/v1/sync/push`.
#[derive(Deserialize)]
pub struct SyncPushBody {
    /// Claimed `agent_id` of the peer pushing data. Recorded in
    /// `sync_state` for vector clock advancement. Treated as identity
    /// only (not attestation) — same NHI model as every other write.
    pub sender_agent_id: String,
    /// Vector clock the sender had at push time. Foundation accepts it
    /// and stores the latest-seen timestamp; full clock reconciliation
    /// lands with Task 3a.1.
    #[serde(default)]
    #[allow(dead_code)] // Consumed by Task 3a.1 CRDT-lite; shipped now for wire compat.
    pub sender_clock: crate::models::VectorClock,
    /// Memories the sender is offering. Applied via the existing
    /// timestamp-aware merge (`insert_if_newer`).
    pub memories: Vec<Memory>,
    /// Memory IDs the sender has deleted and wants propagated. Applied
    /// via `db::delete`. v0.6.0.1: simple remove (no tombstone row); a
    /// concurrent newer `insert_if_newer` from another peer could revive
    /// the row — a Last-Writer-Wins quirk we live with until v0.7's
    /// CRDT-lite tombstone table lands. In the common 4-node mesh, the
    /// same delete reaches every peer well before any revival window.
    #[serde(default)]
    pub deletions: Vec<String>,
    /// Preview mode — classify and count, do not write.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Deserialize)]
pub struct SyncSinceQuery {
    /// Return memories with `updated_at > since`. Absent = full snapshot.
    pub since: Option<String>,
    /// Pagination cap. Defaults to 500.
    pub limit: Option<usize>,
    /// Caller's claimed `agent_id`; optional but recorded in `sync_state`
    /// so the caller can later push incremental updates.
    pub peer: Option<String>,
}

#[allow(clippy::too_many_lines)]
pub async fn sync_push(
    State(state): State<Db>,
    headers: HeaderMap,
    Json(body): Json<SyncPushBody>,
) -> impl IntoResponse {
    if let Err(e) = validate::validate_agent_id(&body.sender_agent_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid sender_agent_id: {e}")})),
        )
            .into_response();
    }
    // Cap memories per push, matching the bulk-create limit. Without
    // this a malicious peer with a valid mTLS cert could flood the
    // receiver and bottleneck the shared SQLite Mutex (red-team #242).
    if body.memories.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("sync_push limited to {} memories per request", MAX_BULK_SIZE)
            })),
        )
            .into_response();
    }
    if body.deletions.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("sync_push limited to {} deletions per request", MAX_BULK_SIZE)
            })),
        )
            .into_response();
    }
    // Receiver's local identity — default to the caller-supplied header,
    // fall back to the anonymous placeholder. Recorded in sync_state rows.
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let local_agent_id = match crate::identity::resolve_http_agent_id(None, header_agent_id) {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid x-agent-id: {e}")})),
            )
                .into_response();
        }
    };

    let lock = state.lock().await;
    let mut applied = 0usize;
    let mut noop = 0usize;
    let mut skipped = 0usize;
    let mut deleted = 0usize;
    let mut latest_seen: Option<String> = None;

    for mem in &body.memories {
        if validate::validate_memory(mem).is_err() {
            skipped += 1;
            continue;
        }
        if latest_seen
            .as_deref()
            .is_none_or(|current| mem.updated_at.as_str() > current)
        {
            latest_seen = Some(mem.updated_at.clone());
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::insert_if_newer(&lock.0, mem) {
            Ok(_id) => applied += 1,
            Err(e) => {
                tracing::warn!("sync_push: insert_if_newer failed for {}: {e}", mem.id);
                skipped += 1;
            }
        }
    }

    // Process deletions (v0.6.0.1 — scenario 10 fanout). Invalid ids are
    // skipped silently; missing rows count as no-op. Peers that have
    // already GC'd the row see identical post-state.
    for del_id in &body.deletions {
        if validate::validate_id(del_id).is_err() {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::delete(&lock.0, del_id) {
            Ok(true) => deleted += 1,
            Ok(false) => noop += 1,
            Err(e) => {
                tracing::warn!("sync_push: delete failed for {del_id}: {e}");
                skipped += 1;
            }
        }
    }

    // Advance the vector clock with the highest `updated_at` we observed.
    // Skipped in dry-run mode since the caller is only previewing.
    if !body.dry_run
        && let Some(at) = latest_seen.as_deref()
        && let Err(e) = db::sync_state_observe(&lock.0, &local_agent_id, &body.sender_agent_id, at)
    {
        tracing::warn!("sync_push: sync_state_observe failed: {e}");
    }

    // Receiver's current clock, returned so the sender can learn which
    // peers the receiver has seen. Phase 3 Task 3a.1 will use this to
    // short-circuit redundant pushes.
    let receiver_clock = db::sync_state_load(&lock.0, &local_agent_id)
        .unwrap_or_else(|_| crate::models::VectorClock::default());

    (
        StatusCode::OK,
        Json(json!({
            "applied": applied,
            "deleted": deleted,
            "noop": noop,
            "skipped": skipped,
            "dry_run": body.dry_run,
            "receiver_agent_id": local_agent_id,
            "receiver_clock": receiver_clock,
        })),
    )
        .into_response()
}

pub async fn sync_since(
    State(state): State<Db>,
    headers: HeaderMap,
    Query(q): Query<SyncSinceQuery>,
) -> impl IntoResponse {
    // Validate `since` parses as RFC 3339 BEFORE hitting the DB so a
    // garbage timestamp returns a clear 400 instead of a 200 with the
    // entire database (red-team #247).
    if let Some(ref s) = q.since
        && !s.is_empty()
        && chrono::DateTime::parse_from_rfc3339(s).is_err()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid `since` parameter — expected RFC 3339 timestamp"
            })),
        )
            .into_response();
    }
    let limit = q.limit.unwrap_or(500).min(10_000);
    let lock = state.lock().await;
    let mems = match db::memories_updated_since(&lock.0, q.since.as_deref(), limit) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("sync_since: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };

    // Record the puller as a peer so subsequent incremental push/pull
    // pairs have a durable clock entry. Best-effort; don't fail the
    // response if the side-effect write fails.
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    if let (Some(peer), Ok(local_agent_id)) = (
        q.peer.as_deref(),
        crate::identity::resolve_http_agent_id(None, header_agent_id),
    ) && validate::validate_agent_id(peer).is_ok()
        && let Some(last) = mems.last()
        && let Err(e) = db::sync_state_observe(&lock.0, &local_agent_id, peer, &last.updated_at)
    {
        tracing::debug!("sync_since: sync_state_observe failed: {e}");
    }

    (
        StatusCode::OK,
        Json(json!({
            "count": mems.len(),
            "limit": limit,
            "memories": mems,
        })),
    )
        .into_response()
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
        let (results, _tokens) = db::recall(
            &lock.0,
            "recall handler",
            Some("test"),
            10,
            None,
            None,
            None,
            crate::models::SHORT_TTL_EXTEND_SECS,
            crate::models::MID_TTL_EXTEND_SECS,
            None,
            None,
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

    // --- AppState wiring tests (issue #219) ---

    use axum::{Router, body::Body, routing::get as axum_get, routing::post as axum_post};
    use tower::ServiceExt as _;

    fn test_app_state(db: Db) -> AppState {
        AppState {
            db,
            embedder: Arc::new(None),
            vector_index: Arc::new(Mutex::new(None)),
            federation: Arc::new(None),
        }
    }

    #[tokio::test]
    async fn http_create_memory_uses_appstate_and_persists() {
        // Issue #219 regression — HTTP write path must reach `create_memory`
        // via `State<AppState>` and return 201 CREATED. Previously the daemon
        // held only `Db` and had no path to the embedder/vector index.
        let state = test_state();
        let app = Router::new()
            .route("/api/v1/memories", axum_post(create_memory))
            .with_state(test_app_state(state.clone()));

        let body = serde_json::json!({
            "tier": "long",
            "namespace": "http-embed-test",
            "title": "Semantic-ready via HTTP",
            "content": "HTTP-authored memories must now participate in semantic recall.",
            "tags": ["issue-219"],
            "priority": 7,
            "confidence": 1.0,
            "source": "api",
            "metadata": {}
        });
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/memories")
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("x-agent-id", "alice")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        // And the row is present in the DB.
        let lock = state.lock().await;
        let rows = db::list(
            &lock.0,
            Some("http-embed-test"),
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
        assert!(!rows.is_empty(), "HTTP-authored memory must be persisted");
        assert_eq!(rows[0].title, "Semantic-ready via HTTP");
    }

    #[tokio::test]
    async fn http_update_memory_uses_appstate() {
        // Issue #219 — update path must also route via `AppState` so the
        // embedder and vector index are reachable for content-change refresh.
        let state = test_state();
        let now = Utc::now();
        let id = {
            let lock = state.lock().await;
            let mem = Memory {
                id: Uuid::new_v4().to_string(),
                tier: Tier::Long,
                namespace: "http-embed-test".into(),
                title: "Before update".into(),
                content: "Original content.".into(),
                tags: vec![],
                priority: 5,
                confidence: 1.0,
                source: "test".into(),
                access_count: 0,
                created_at: now.to_rfc3339(),
                updated_at: now.to_rfc3339(),
                last_accessed_at: None,
                expires_at: None,
                metadata: serde_json::json!({}),
            };
            db::insert(&lock.0, &mem).unwrap()
        };

        let app = Router::new()
            .route("/api/v1/memories/{id}", axum::routing::put(update_memory))
            .with_state(test_app_state(state.clone()));

        let patch = serde_json::json!({"content": "Updated content for semantic refresh."});
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/v1/memories/{id}"))
                    .method("PUT")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&patch).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // --- Phase 3 foundation HTTP sync tests (issue #224) ---

    #[tokio::test]
    async fn http_sync_push_applies_and_advances_clock() {
        // Smoke test for POST /api/v1/sync/push — memories land in the
        // receiver's DB and the vector clock records the sender's latest
        // `updated_at`. Full CRDT semantics are the v0.8.0 follow-up.
        let state = test_state();
        let app = Router::new()
            .route("/api/v1/sync/push", axum_post(sync_push))
            .with_state(state.clone());

        let now = Utc::now().to_rfc3339();
        let body = serde_json::json!({
            "sender_agent_id": "peer-alice",
            "sender_clock": {"entries": {}},
            "memories": [{
                "id": Uuid::new_v4().to_string(),
                "tier": "long",
                "namespace": "sync-smoke",
                "title": "From peer",
                "content": "Pushed via HTTP sync endpoint.",
                "tags": [],
                "priority": 5,
                "confidence": 1.0,
                "source": "api",
                "access_count": 0,
                "created_at": now,
                "updated_at": now,
                "last_accessed_at": null,
                "expires_at": null,
                "metadata": {"agent_id": "peer-alice"}
            }],
            "dry_run": false
        });
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sync/push")
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("x-agent-id", "local-receiver")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Row landed.
        let lock = state.lock().await;
        let rows = db::list(
            &lock.0,
            Some("sync-smoke"),
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
        assert_eq!(rows.len(), 1);
        // Clock advanced — peer-alice registered against local-receiver.
        let clock = db::sync_state_load(&lock.0, "local-receiver").unwrap();
        assert!(
            clock.latest_from("peer-alice").is_some(),
            "push must record sender in sync_state; got: {:?}",
            clock.entries
        );
    }

    #[tokio::test]
    async fn http_sync_push_rejects_oversized_batch_redteam_242() {
        // Red-team #242 — sync_push must cap memories per request, matching
        // bulk-create's MAX_BULK_SIZE. Without this a malicious peer can
        // flood the receiver and bottleneck the SQLite Mutex.
        let state = test_state();
        let app = Router::new()
            .route("/api/v1/sync/push", axum_post(sync_push))
            .with_state(state);
        let now = Utc::now().to_rfc3339();
        // Build MAX_BULK_SIZE + 1 entries (1001).
        let mems: Vec<serde_json::Value> = (0..=MAX_BULK_SIZE)
            .map(|i| {
                serde_json::json!({
                    "id": Uuid::new_v4().to_string(),
                    "tier": "long",
                    "namespace": "oversize",
                    "title": format!("m{i}"),
                    "content": "x",
                    "tags": [],
                    "priority": 5,
                    "confidence": 1.0,
                    "source": "api",
                    "access_count": 0,
                    "created_at": now,
                    "updated_at": now,
                    "last_accessed_at": null,
                    "expires_at": null,
                    "metadata": {}
                })
            })
            .collect();
        let body = serde_json::json!({
            "sender_agent_id": "peer-flood",
            "sender_clock": {"entries": {}},
            "memories": mems,
            "dry_run": false,
        });
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sync/push")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn http_sync_push_dry_run_applies_nothing() {
        // Phase 3 — dry_run=true must not write.
        let state = test_state();
        let app = Router::new()
            .route("/api/v1/sync/push", axum_post(sync_push))
            .with_state(state.clone());

        let now = Utc::now().to_rfc3339();
        let body = serde_json::json!({
            "sender_agent_id": "peer-bob",
            "sender_clock": {"entries": {}},
            "memories": [{
                "id": Uuid::new_v4().to_string(),
                "tier": "long",
                "namespace": "sync-dryrun",
                "title": "Must not land",
                "content": "Preview only.",
                "tags": [],
                "priority": 5,
                "confidence": 1.0,
                "source": "api",
                "access_count": 0,
                "created_at": now,
                "updated_at": now,
                "last_accessed_at": null,
                "expires_at": null,
                "metadata": {}
            }],
            "dry_run": true
        });
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sync/push")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let lock = state.lock().await;
        let rows = db::list(
            &lock.0,
            Some("sync-dryrun"),
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
        assert!(rows.is_empty(), "dry_run must not write rows");
    }

    #[tokio::test]
    async fn http_sync_push_applies_deletions() {
        // v0.6.0.1 — sync_push's `deletions` field removes the listed ids
        // from the receiver so peer-side tombstone fanout works for
        // scenario-10. (a2a-hermes r14.)
        let state = test_state();
        let now = Utc::now().to_rfc3339();

        let seeded_id = {
            let lock = state.lock().await;
            let mem = Memory {
                id: Uuid::new_v4().to_string(),
                tier: Tier::Mid,
                namespace: "delete-fanout".into(),
                title: "to-be-deleted".into(),
                content: "body".into(),
                tags: vec![],
                priority: 5,
                confidence: 1.0,
                source: "api".into(),
                access_count: 0,
                created_at: now.clone(),
                updated_at: now.clone(),
                last_accessed_at: None,
                expires_at: None,
                metadata: serde_json::json!({"agent_id": "ai:seeder"}),
            };
            db::insert(&lock.0, &mem).unwrap()
        };

        let app = Router::new()
            .route("/api/v1/sync/push", axum_post(sync_push))
            .with_state(state.clone());

        let body = serde_json::json!({
            "sender_agent_id": "peer-alice",
            "sender_clock": {"entries": {}},
            "memories": [],
            "deletions": [seeded_id.clone()],
            "dry_run": false
        });
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sync/push")
                    .method("POST")
                    .header("content-type", "application/json")
                    .header("x-agent-id", "local-receiver")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["deleted"], 1);

        let lock = state.lock().await;
        let gone = db::get(&lock.0, &seeded_id).unwrap();
        assert!(
            gone.is_none(),
            "row should have been tombstoned by sync_push"
        );
    }

    #[tokio::test]
    async fn http_sync_since_streams_new_memories_only() {
        // Phase 3 — GET /api/v1/sync/since?since=<ts> returns only memories
        // with updated_at > ts.
        let state = test_state();
        // Seed one old + one new memory.
        let old_ts = "2020-01-01T00:00:00+00:00";
        let new_ts = Utc::now().to_rfc3339();
        {
            let lock = state.lock().await;
            for (title, ts) in [("old-mem", old_ts), ("new-mem", new_ts.as_str())] {
                let mem = Memory {
                    id: Uuid::new_v4().to_string(),
                    tier: Tier::Long,
                    namespace: "since-test".into(),
                    title: title.into(),
                    content: "body".into(),
                    tags: vec![],
                    priority: 5,
                    confidence: 1.0,
                    source: "api".into(),
                    access_count: 0,
                    created_at: ts.to_string(),
                    updated_at: ts.to_string(),
                    last_accessed_at: None,
                    expires_at: None,
                    metadata: serde_json::json!({}),
                };
                db::insert(&lock.0, &mem).unwrap();
            }
        }

        let app = Router::new()
            .route("/api/v1/sync/since", axum_get(sync_since))
            .with_state(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sync/since?since=2020-06-01T00:00:00%2B00:00")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let titles: Vec<String> = v["memories"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["title"].as_str().map(str::to_string))
            .collect();
        assert_eq!(titles, vec!["new-mem".to_string()]);
    }

    #[tokio::test]
    async fn sync_since_rejects_garbage_timestamp_with_400() {
        // Red-team #247 — `since=garbage` previously returned 200 with all
        // memories. Now must return 400 with a clear error.
        let state = test_state();
        let app = Router::new()
            .route("/api/v1/sync/since", axum_get(sync_since))
            .with_state(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sync/since?since=not-a-date")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["error"].as_str().unwrap().contains("RFC 3339"));
    }

    #[tokio::test]
    async fn sync_state_observe_is_monotonic() {
        // Phase 3 — clock advancement must never go backwards.
        let conn = db::open(std::path::Path::new(":memory:")).unwrap();
        let older = "2020-01-01T00:00:00+00:00";
        let newer = "2026-04-17T00:00:00+00:00";

        db::sync_state_observe(&conn, "local", "peer-a", newer).unwrap();
        // A subsequent older observation must NOT overwrite.
        db::sync_state_observe(&conn, "local", "peer-a", older).unwrap();
        let clock = db::sync_state_load(&conn, "local").unwrap();
        assert_eq!(clock.latest_from("peer-a"), Some(newer));
    }

    // --- API key auth middleware tests ---

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
