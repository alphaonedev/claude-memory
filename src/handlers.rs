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

use crate::config::{ResolvedTtl, TierConfig};
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
    /// Resolved [`TierConfig`] for this daemon. Exposed so HTTP
    /// endpoints that mirror MCP tools (notably `/capabilities`) can
    /// reuse the MCP-side report builder without re-parsing config.
    pub tier_config: Arc<TierConfig>,
    /// v0.6.2 (S18): resolved recall scoring config — tier half-lives,
    /// legacy-scoring toggle. Exposed so `recall_memories_get` /
    /// `recall_memories_post` can call `db::recall_hybrid` (semantic
    /// blend) when the embedder is loaded, mirroring how the MCP
    /// `memory_recall` handler already wires it (src/mcp.rs:1157).
    /// Prior to this, HTTP recall was keyword-only regardless of
    /// embedder availability — scenario-18 surfaced the gap.
    pub scoring: Arc<crate::config::ResolvedScoring>,
}

impl FromRef<AppState> for Db {
    fn from_ref(app: &AppState) -> Self {
        app.db.clone()
    }
}

const MAX_BULK_SIZE: usize = 1000;

/// v0.6.2 (S40): maximum number of per-row `broadcast_store_quorum` fanouts
/// in flight at once during `bulk_create`. Replaces the prior sequential
/// for-loop (which paid 100ms × N rows of wall time and blew past the
/// testbook's 20s settle on N=500) with bounded concurrency. The bound
/// balances speedup against peer-side `SQLite` Mutex contention and the
/// leader-side reqwest connection-pool / ephemeral-port envelope. See the
/// comment above the loop in `bulk_create` for the full rationale.
const BULK_FANOUT_CONCURRENCY: usize = 8;

/// Shared state for API key authentication middleware.
#[derive(Clone)]
pub struct ApiKeyState {
    pub key: Option<String>,
}

/// Constant-time byte-slice equality. Doesn't short-circuit on the
/// Percent-decode a URL-encoded query value in place. Invalid `%XX`
/// escapes are passed through verbatim (lossy). Ultrareview #337.
#[inline]
fn percent_decode_lossy(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = (bytes[i + 1] as char).to_digit(16);
            let l = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (h, l) {
                // h and l are single hex digits (0..=15), so h*16 + l
                // is always in 0..=255. Cast is lossless.
                out.push(u8::try_from(h * 16 + l).unwrap_or(0));
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

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

    // Check ?api_key= query param (ultrareview #337: URL-decode
    // before comparison. A key with reserved chars like `+`, `%`,
    // `&` must be percent-encoded by the caller per RFC 3986; the
    // previous raw-compare path silently mismatched those keys and
    // opened an encoded-bypass surface where a key containing `%2B`
    // would compare against `%2B` rather than `+`, producing a
    // different trust decision depending on caller quoting.)
    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some(val) = pair.strip_prefix("api_key=") {
                let decoded = percent_decode_lossy(val);
                if constant_time_eq(decoded.as_bytes(), expected.as_bytes()) {
                    return next.run(req).await.into_response();
                }
            }
        }
    }

    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "missing or invalid API key"})),
    )
        .into_response()
}

pub async fn health(State(app): State<AppState>) -> impl IntoResponse {
    let lock = app.db.lock().await;
    let ok = db::health_check(&lock.0).unwrap_or(false);
    drop(lock);
    let embedder_ready = app.embedder.as_ref().is_some();
    let federation_enabled = app.federation.as_ref().is_some();
    let code = if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    // v0.6.2 (#327): expose embedder status so operators can tell from
    // /health alone whether semantic recall is wired up on this node.
    (
        code,
        Json(json!({
            "status": if ok { "ok" } else { "error" },
            "service": "ai-memory",
            "version": env!("CARGO_PKG_VERSION"),
            "embedder_ready": embedder_ready,
            "federation_enabled": federation_enabled,
        })),
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
                // v0.6.2 (S34): fan out the new pending row so peers can
                // approve / reject / list it. Load the canonical row we
                // just inserted and broadcast before responding.
                let pending_row = db::get_pending_action(&lock.0, &pending_id).ok().flatten();
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

#[allow(clippy::too_many_lines)]
pub async fn approve_pending(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    use crate::db::ApproveOutcome;
    use crate::models::PendingDecision;
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
) -> impl IntoResponse {
    use crate::models::PendingDecision;
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
                // v0.6.2 (S34): fan out the new pending delete row so peers
                // see consistent governance queue state.
                let pending_row = db::get_pending_action(&lock.0, &pending_id).ok().flatten();
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
                // v0.6.2 (S34): fan out the new pending promote row too.
                let pending_row = db::get_pending_action(&lock.0, &pending_id).ok().flatten();
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
    State(app): State<AppState>,
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
    // Ultrareview #348: reject budget_tokens=0 explicitly.
    if p.budget_tokens == Some(0) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "budget_tokens must be >= 1"})),
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
    let limit = p.limit.unwrap_or(10).min(50);
    recall_response(
        &app,
        &ctx,
        p.namespace.as_deref(),
        limit,
        p.tags.as_deref(),
        p.since.as_deref(),
        p.until.as_deref(),
        p.as_agent.as_deref(),
        p.budget_tokens,
    )
    .await
}

pub async fn recall_memories_post(
    State(app): State<AppState>,
    Json(body): Json<RecallBody>,
) -> impl IntoResponse {
    if body.context.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "context is required"})),
        )
            .into_response();
    }
    if body.budget_tokens == Some(0) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "budget_tokens must be >= 1"})),
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
    let limit = body.limit.unwrap_or(10).min(50);
    recall_response(
        &app,
        &body.context,
        body.namespace.as_deref(),
        limit,
        body.tags.as_deref(),
        body.since.as_deref(),
        body.until.as_deref(),
        body.as_agent.as_deref(),
        body.budget_tokens,
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
) -> axum::response::Response {
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
        );
        (r, "keyword")
    };

    match result {
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
                "mode": mode,
            });
            if let Some(b) = budget_tokens {
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
    State(state): State<Db>,
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
    let lock = state.lock().await;
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
                if link.relation.contains("contradict")
                    && candidate_ids.contains(&link.source_id)
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

/// Query parameters for `GET /api/v1/taxonomy` (Pillar 1 / Stream A).
#[derive(Debug, Deserialize)]
pub struct TaxonomyQuery {
    /// Restrict to memories at this namespace OR any descendant. Trailing
    /// `/` is tolerated. Omit to walk the whole tree.
    pub prefix: Option<String>,
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
    State(state): State<Db>,
    Query(p): Query<TaxonomyQuery>,
) -> impl IntoResponse {
    let prefix_owned: Option<String> = p
        .prefix
        .as_deref()
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
    let lock = state.lock().await;
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

pub async fn create_link(
    State(app): State<AppState>,
    Json(body): Json<LinkBody>,
) -> impl IntoResponse {
    if let Err(e) = validate::validate_link(&body.source_id, &body.target_id, &body.relation) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let lock = app.db.lock().await;
    let create_result = db::create_link(&lock.0, &body.source_id, &body.target_id, &body.relation);
    // Drop DB lock before fanning out — peers POST back to our sync_push
    // and we'd deadlock on the shared Mutex if we held it.
    drop(lock);
    match create_result {
        Ok(()) => {
            // v0.6.2 (#325): propagate link to peers.
            if let Some(fed) = app.federation.as_ref() {
                let link = crate::models::MemoryLink {
                    source_id: body.source_id.clone(),
                    target_id: body.target_id.clone(),
                    relation: body.relation.clone(),
                    created_at: chrono::Utc::now().to_rfc3339(),
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
            (StatusCode::CREATED, Json(json!({"linked": true}))).into_response()
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

/// v0.6.2 (#325) — DELETE /api/v1/links. Removes the directional link
/// `source_id → target_id` locally. Deletion is NOT fanned out in v0.6.2:
/// the receiving-side API is `db::delete_link`, and `sync_push` does not
/// yet carry a link-tombstone list. Full link tombstones ship with v0.7
/// CRDT-lite. For current scenario coverage (scenario-11 tests create),
/// create-link fanout is sufficient.
pub async fn delete_link(
    State(app): State<AppState>,
    Json(body): Json<LinkBody>,
) -> impl IntoResponse {
    if let Err(e) = validate::validate_link(&body.source_id, &body.target_id, &body.relation) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    let lock = app.db.lock().await;
    let delete_result = db::delete_link(&lock.0, &body.source_id, &body.target_id);
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
    State(app): State<AppState>,
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
    let lock = app.db.lock().await;
    let tier = body.tier.unwrap_or(Tier::Long);
    let source_ids = body.ids.clone();
    let consolidate_result = db::consolidate(
        &lock.0,
        &body.ids,
        &body.title,
        &body.summary,
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
                Json(json!({"id": new_id, "consolidated": body.ids.len()})),
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
    State(state): State<Db>,
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
    let lock = state.lock().await;
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
    /// v0.6.2 (S29): memory IDs the sender has explicitly archived and
    /// wants propagated. Applied via `db::archive_memory` — a soft move
    /// from `memories` to `archived_memories`. Missing-on-peer IDs no-op.
    /// Distinct from `deletions`, which is a hard DELETE.
    #[serde(default)]
    pub archives: Vec<String>,
    /// v0.6.2 (S29): memory IDs the sender has restored from archive and
    /// wants propagated. Applied via `db::restore_archived` — moves the
    /// row from `archived_memories` back into `memories`. The inverse of
    /// `archives`. Missing-on-peer IDs (no row in the peer's archive
    /// table, or a live row already exists) no-op so replays are safe.
    #[serde(default)]
    pub restores: Vec<String>,
    /// v0.6.2 (#325): memory links the sender wants propagated. Applied
    /// via `db::create_link` on each peer. Duplicates are a no-op thanks
    /// to the unique `(source_id, target_id, relation)` constraint on
    /// `memory_links`.
    #[serde(default)]
    pub links: Vec<MemoryLink>,
    /// v0.6.2 (S34): pending-action rows the sender wants propagated.
    /// Applied via `db::upsert_pending_action` — preserves the originator's
    /// id + status + approvals so the cluster agrees on pending state.
    /// Without this, `POST /api/v1/pending/{id}/approve` on a peer 404s
    /// because the row only exists on the originator.
    #[serde(default)]
    pub pendings: Vec<crate::models::PendingAction>,
    /// v0.6.2 (S34): pending-action decisions the sender wants propagated
    /// so approve/reject on any node lands consistently. Applied via
    /// `db::decide_pending_action` — already-decided rows no-op, replay-safe.
    #[serde(default)]
    pub pending_decisions: Vec<crate::models::PendingDecision>,
    /// v0.6.2 (S35): namespace-standard meta rows the sender wants
    /// propagated. Applied via `db::set_namespace_standard(conn, ns,
    /// standard_id, parent.as_deref())` so the peer's inheritance-chain
    /// walk uses the originator's explicit parent (not a locally
    /// auto-detected one).
    #[serde(default)]
    pub namespace_meta: Vec<crate::models::NamespaceMetaEntry>,
    /// v0.6.2 (S35 follow-up): namespaces whose standard the sender has
    /// *cleared* and wants propagated. Applied via `db::clear_namespace_standard`
    /// — missing-on-peer namespaces no-op so replays are safe. Without
    /// this, alice clearing a standard on node-1 left the row visible on
    /// node-2's peer, breaking cross-peer rule-lifecycle assertions.
    #[serde(default)]
    pub namespace_meta_clears: Vec<String>,
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
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SyncPushBody>,
) -> impl IntoResponse {
    let state = app.db.clone();
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
    if body.archives.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("sync_push limited to {} archives per request", MAX_BULK_SIZE)
            })),
        )
            .into_response();
    }
    if body.restores.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("sync_push limited to {} restores per request", MAX_BULK_SIZE)
            })),
        )
            .into_response();
    }
    if body.pendings.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("sync_push limited to {} pendings per request", MAX_BULK_SIZE)
            })),
        )
            .into_response();
    }
    if body.pending_decisions.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "sync_push limited to {} pending_decisions per request",
                    MAX_BULK_SIZE
                )
            })),
        )
            .into_response();
    }
    if body.namespace_meta.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "sync_push limited to {} namespace_meta per request",
                    MAX_BULK_SIZE
                )
            })),
        )
            .into_response();
    }
    if body.namespace_meta_clears.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "sync_push limited to {} namespace_meta_clears per request",
                    MAX_BULK_SIZE
                )
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
    let mut archived = 0usize;
    let mut restored = 0usize;
    let mut latest_seen: Option<String> = None;

    // v0.6.0.1 (#322): peers that apply a synced memory must also refresh
    // their embedding + HNSW index so downstream semantic recall surfaces
    // the row. Without this, scenario-18 observed a2a-hermes r14 black-hole
    // pattern: substrate CRUD fanout works, but semantic recall on peers
    // silently misses propagated writes.
    //
    // Collect rows that need an embedding refresh and apply AFTER we drop
    // the DB lock (embedder is CPU-heavy; holding the Mutex across that
    // would serialize unrelated writers for hundreds of ms).
    let mut embedding_refresh: Vec<(String, String)> = Vec::new();
    for mem in &body.memories {
        if let Err(e) = validate::validate_memory(mem) {
            tracing::warn!("sync_push: skipping memory {} ({}): {e}", mem.id, mem.title);
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
            Ok(actual_id) => {
                applied += 1;
                embedding_refresh.push((actual_id, format!("{} {}", mem.title, mem.content)));
            }
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

    // v0.6.2 (S29): process explicit archives. Soft-move from `memories`
    // to `archived_memories` — distinct from deletions which hard-delete.
    // Missing rows count as no-op (peer may have already archived or
    // never received the original write).
    for arch_id in &body.archives {
        if validate::validate_id(arch_id).is_err() {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::archive_memory(&lock.0, arch_id, Some("sync_push")) {
            Ok(true) => archived += 1,
            Ok(false) => noop += 1,
            Err(e) => {
                tracing::warn!("sync_push: archive_memory failed for {arch_id}: {e}");
                skipped += 1;
            }
        }
    }

    // v0.6.2 (S29): process explicit restores — the inverse of archives.
    // Move the row from `archived_memories` back into `memories`.
    // No-op posture matches archives: missing rows (peer hasn't received
    // the archive, or the row is already live) count as noop so replays
    // and out-of-order restore/archive pairs don't error.
    for res_id in &body.restores {
        if validate::validate_id(res_id).is_err() {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::restore_archived(&lock.0, res_id) {
            Ok(true) => restored += 1,
            Ok(false) => noop += 1,
            Err(e) => {
                tracing::warn!("sync_push: restore_archived failed for {res_id}: {e}");
                skipped += 1;
            }
        }
    }

    // v0.6.2 (#325): process incoming links. Duplicates are expected on
    // retry / re-sync and collapse to a no-op via the unique index on
    // (source_id, target_id, relation). Invalid ids are skipped silently
    // — same posture as deletions.
    let mut links_applied = 0usize;
    for link in &body.links {
        if validate::validate_link(&link.source_id, &link.target_id, &link.relation).is_err() {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::create_link(&lock.0, &link.source_id, &link.target_id, &link.relation) {
            Ok(()) => links_applied += 1,
            Err(e) => {
                tracing::warn!(
                    "sync_push: create_link failed ({} -> {} / {}): {e}",
                    link.source_id,
                    link.target_id,
                    link.relation
                );
                skipped += 1;
            }
        }
    }

    // v0.6.2 (S34): process incoming pending-action rows. Uses
    // `upsert_pending_action` so replays / races converge on the
    // originator's canonical row. Invalid ids skipped silently.
    let mut pendings_applied = 0usize;
    for pa in &body.pendings {
        if validate::validate_id(&pa.id).is_err() {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::upsert_pending_action(&lock.0, pa) {
            Ok(()) => pendings_applied += 1,
            Err(e) => {
                tracing::warn!("sync_push: upsert_pending_action failed for {}: {e}", pa.id);
                skipped += 1;
            }
        }
    }

    // v0.6.2 (S34): process incoming pending-action decisions. No-op on
    // already-decided rows; that's the steady-state when the originator
    // and this peer both saw the decision. Rejected decisions still
    // transition status so retries on either side see `status != 'pending'`.
    let mut pending_decisions_applied = 0usize;
    for dec in &body.pending_decisions {
        if validate::validate_id(&dec.id).is_err() {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::decide_pending_action(&lock.0, &dec.id, dec.approved, &dec.decider) {
            Ok(true) => {
                pending_decisions_applied += 1;
                // On approve, replay the pending payload so the target
                // write (store/delete/promote) actually lands on this
                // peer — matches the originator's post-approve state.
                if dec.approved {
                    match db::execute_pending_action(&lock.0, &dec.id) {
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                "sync_push: execute_pending_action failed for {}: {e}",
                                dec.id
                            );
                        }
                    }
                }
            }
            Ok(false) => noop += 1, // already decided — converged state
            Err(e) => {
                tracing::warn!(
                    "sync_push: decide_pending_action failed for {}: {e}",
                    dec.id
                );
                skipped += 1;
            }
        }
    }

    // v0.6.2 (S35): process incoming namespace_meta rows. Applies via
    // `set_namespace_standard` so the peer's inheritance-chain walk has
    // the originator's explicit parent link. The standard memory itself
    // rides on the same push via `memories` (or arrived earlier through
    // `broadcast_store_quorum`); the namespace-meta row closes the gap.
    let mut namespace_meta_applied = 0usize;
    for entry in &body.namespace_meta {
        if validate::validate_namespace(&entry.namespace).is_err()
            || validate::validate_id(&entry.standard_id).is_err()
        {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::set_namespace_standard(
            &lock.0,
            &entry.namespace,
            &entry.standard_id,
            entry.parent_namespace.as_deref(),
        ) {
            Ok(()) => namespace_meta_applied += 1,
            Err(e) => {
                tracing::warn!(
                    "sync_push: set_namespace_standard failed for {}: {e}",
                    entry.namespace
                );
                skipped += 1;
            }
        }
    }

    // v0.6.2 (S35 follow-up): process incoming namespace_meta_clears. Applies
    // via `db::clear_namespace_standard` so the peer drops its meta row and
    // subsequent `get_standard` returns empty. Missing-on-peer namespaces
    // no-op (`changed == 0`) — replays are safe.
    let mut namespace_meta_cleared = 0usize;
    for ns in &body.namespace_meta_clears {
        if validate::validate_namespace(ns).is_err() {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::clear_namespace_standard(&lock.0, ns) {
            Ok(true) => namespace_meta_cleared += 1,
            Ok(false) => noop += 1,
            Err(e) => {
                tracing::warn!("sync_push: clear_namespace_standard failed for {ns}: {e}");
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

    // v0.6.0.1 (#322): regenerate embeddings for applied rows so peer-side
    // semantic recall surfaces the propagated memories. Without this,
    // scenario-18 observed the a2a-hermes r14 black-hole pattern:
    // substrate CRUD fanout works, but semantic recall on peers misses.
    //
    // Embedding + set_embedding are serialized under the existing DB lock;
    // HNSW updates happen after we release the lock to avoid contention.
    let mut hnsw_updates: Vec<(String, Vec<f32>)> = Vec::new();
    if !body.dry_run
        && !embedding_refresh.is_empty()
        && let Some(emb) = app.embedder.as_ref().as_ref()
    {
        for (id, text) in &embedding_refresh {
            match emb.embed(text) {
                Ok(vec) => {
                    if let Err(e) = db::set_embedding(&lock.0, id, &vec) {
                        tracing::warn!("sync_push: set_embedding failed for {id}: {e}");
                        continue;
                    }
                    hnsw_updates.push((id.clone(), vec));
                }
                Err(e) => {
                    tracing::warn!("sync_push: embed failed for {id}: {e}");
                }
            }
        }
    }

    // Receiver's current clock, returned so the sender can learn which
    // peers the receiver has seen. Phase 3 Task 3a.1 will use this to
    // short-circuit redundant pushes.
    let receiver_clock = db::sync_state_load(&lock.0, &local_agent_id)
        .unwrap_or_else(|_| crate::models::VectorClock::default());

    // Release DB lock before touching the HNSW index — the vector index
    // has its own mutex and holding both serializes unrelated writers.
    drop(lock);
    if !hnsw_updates.is_empty() {
        let mut idx_lock = app.vector_index.lock().await;
        if let Some(idx) = idx_lock.as_mut() {
            for (id, vec) in hnsw_updates {
                idx.remove(&id);
                idx.insert(id, vec);
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "applied": applied,
            "deleted": deleted,
            "archived": archived,
            "restored": restored,
            "links_applied": links_applied,
            "pendings_applied": pendings_applied,
            "pending_decisions_applied": pending_decisions_applied,
            "namespace_meta_applied": namespace_meta_applied,
            "namespace_meta_cleared": namespace_meta_cleared,
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

    // S39 diagnostic echo (v0.6.2). The testbook scenario writes 6 rows
    // while peer-3 is suspended then queries `/sync/since?since=<ckpt>`
    // and expects the 6 back. When the count comes back 0, the scenario
    // can't tell whether:
    //   a) the server parsed `since` differently than expected,
    //   b) `limit` silently truncated, or
    //   c) the returned timestamps don't actually cover the expected range.
    // Echoing `updated_since` (what the server parsed, verbatim) plus
    // earliest / latest `updated_at` from the result set lets the
    // scenario pin the failure mode without changing any behavior. Fields
    // are additive — no existing caller assertion regresses.
    let earliest_updated_at = mems.first().map(|m| m.updated_at.clone());
    let latest_updated_at = mems.last().map(|m| m.updated_at.clone());

    (
        StatusCode::OK,
        Json(json!({
            "count": mems.len(),
            "limit": limit,
            "updated_since": q.since,
            "earliest_updated_at": earliest_updated_at,
            "latest_updated_at": latest_updated_at,
            "memories": mems,
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// HTTP parity helpers.
// ---------------------------------------------------------------------------

/// Fan out a locally-committed memory to peers via quorum store. On success,
/// returns `None`; on quorum miss, returns `Some(503_response)` for the
/// caller to short-circuit with. Network errors are logged and swallowed —
/// the local commit already landed and the sync-daemon catches stragglers.
async fn fanout_or_503(app: &AppState, mem: &Memory) -> Option<axum::response::Response> {
    let fed = app.federation.as_ref().as_ref()?;
    match crate::federation::broadcast_store_quorum(fed, mem).await {
        Ok(tracker) => match crate::federation::finalise_quorum(&tracker) {
            Ok(_) => None,
            Err(err) => {
                let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                Some(
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [("Retry-After", "2")],
                        Json(serde_json::to_value(&payload).unwrap_or_default()),
                    )
                        .into_response(),
                )
            }
        },
        Err(e) => {
            tracing::warn!("fanout error (local committed): {e:?}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP parity for MCP-only tools (feat/http-parity-for-mcp-only-tools).
//
// Each endpoint below mirrors an existing handler in `mcp.rs`, adapting the
// MCP tool's params shape to the HTTP request surface used by the testbook v3
// scenarios. Where practical the HTTP wrapper delegates straight into
// `crate::mcp::handle_*` with a synthesized params Value so the business-logic
// contract stays single-sourced; where a scenario's assertion conflicts with
// the MCP contract (notably the S33 subscription shape and the S34/S35
// `/api/v1/namespaces` query-string routing), we match the scenario.
// ---------------------------------------------------------------------------

/// Helper — resolve the caller's `agent_id` using the HTTP precedence chain,
/// accepting an optional body value, the `X-Agent-Id` header, and an optional
/// `?agent_id=` query param. Returns a 400 on invalid input; synthesizes an
/// anonymous id on miss.
fn resolve_caller_agent_id(
    body: Option<&str>,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<String, String> {
    // Body → query → header (body wins, query next, header last). Matches the
    // precedence already used by `register_agent` / `create_memory` with
    // query inserted at the same tier as body for handlers that read from
    // the querystring (e.g. GET /inbox?agent_id=...).
    if let Some(id) = body
        && !id.is_empty()
    {
        validate::validate_agent_id(id).map_err(|e| format!("invalid agent_id: {e}"))?;
        return Ok(id.to_string());
    }
    if let Some(id) = query
        && !id.is_empty()
    {
        validate::validate_agent_id(id).map_err(|e| format!("invalid agent_id: {e}"))?;
        return Ok(id.to_string());
    }
    let header_val = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    crate::identity::resolve_http_agent_id(None, header_val)
        .map_err(|e| format!("invalid agent_id: {e}"))
}

// --- /api/v1/capabilities (GET) -------------------------------------------

pub async fn get_capabilities(State(app): State<AppState>) -> impl IntoResponse {
    // Mirrors `mcp::handle_capabilities`. Reranker state isn't tracked on the
    // HTTP AppState (HTTP daemons that wire a cross-encoder record it via
    // the tier config's `cross_encoder` flag, which is enough for scenario
    // S30's equivalence check).
    //
    // v0.6.2 (S18): forward the *runtime* embedder state so
    // `features.embedder_loaded` reports whether the HF model actually
    // materialized at serve startup (not just whether the tier config
    // asked for one). An offline CI runner can fail the model fetch and
    // end up with `semantic_search=true` (from config) but no embedder in
    // the AppState — setup scripts need this signal to refuse to start
    // scenarios that depend on semantic recall.
    let embedder_loaded = app.embedder.as_ref().is_some();
    match crate::mcp::handle_capabilities(app.tier_config.as_ref(), None, embedder_loaded) {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => {
            tracing::error!("capabilities: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

// --- /api/v1/notify (POST) + /api/v1/inbox (GET) ---------------------------

#[derive(Deserialize)]
pub struct NotifyBody {
    pub target_agent_id: String,
    pub title: String,
    /// Accept either `payload` (MCP tool name) or `content` (S32 scenario).
    #[serde(default)]
    pub payload: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub tier: Option<String>,
    /// Optional explicit sender id — falls back to `X-Agent-Id` header.
    #[serde(default)]
    pub agent_id: Option<String>,
}

pub async fn notify(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<NotifyBody>,
) -> impl IntoResponse {
    let Some(payload) = body.payload.or(body.content) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "payload or content is required"})),
        )
            .into_response();
    };
    let sender = match resolve_caller_agent_id(body.agent_id.as_deref(), &headers, None) {
        Ok(id) => id,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    };

    let mut params = json!({
        "target_agent_id": body.target_agent_id,
        "title": body.title,
        "payload": payload,
    });
    if let Some(p) = body.priority {
        params["priority"] = json!(p);
    }
    if let Some(t) = body.tier {
        params["tier"] = json!(t);
    }

    let lock = app.db.lock().await;
    let resolved_ttl = lock.2.clone();
    // Route via the MCP handler so the wire contract stays single-sourced.
    // `mcp_client = Some(&sender)` makes `resolve_agent_id(None, _)` return
    // the caller-resolved HTTP id — same effective provenance.
    let mcp_client = sender.clone();
    let result = crate::mcp::handle_notify(&lock.0, &params, &resolved_ttl, Some(&mcp_client));

    // v0.6.2 (S32): capture the just-inserted notify row and fan it out to
    // peers. Without this, alice's notify on node-1 lands in bob's inbox on
    // node-1 only — when bob polls `/api/v1/inbox` against node-2 he sees
    // nothing. The HTTP wrapper bypassed the `create_memory` fanout path
    // that every other `db::insert` write uses, so we wire it here with the
    // same posture as `fanout_or_503`: on quorum miss return 503; on a
    // network error, swallow (local commit landed, sync-daemon catches up).
    let fanout_mem = match &result {
        Ok(v) => v
            .get("id")
            .and_then(|x| x.as_str())
            .and_then(|id| db::get(&lock.0, id).ok().flatten()),
        Err(_) => None,
    };
    drop(lock);

    match result {
        Ok(v) => {
            if let Some(mem) = fanout_mem
                && let Some(resp) = fanout_or_503(&app, &mem).await
            {
                return resp;
            }
            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
pub struct InboxQuery {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub unread_only: Option<bool>,
    #[serde(default)]
    pub limit: Option<u64>,
}

pub async fn get_inbox(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<InboxQuery>,
) -> impl IntoResponse {
    let owner = match resolve_caller_agent_id(None, &headers, q.agent_id.as_deref()) {
        Ok(id) => id,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    };

    let mut params = json!({"agent_id": owner});
    if let Some(u) = q.unread_only {
        params["unread_only"] = json!(u);
    }
    if let Some(l) = q.limit {
        params["limit"] = json!(l);
    }
    let lock = app.db.lock().await;
    // Pass the resolved owner as `mcp_client` too so `handle_inbox`'s
    // identity-resolution fallback lands on the same id whichever branch
    // it consults (it prefers `params["agent_id"]` when present).
    let result = crate::mcp::handle_inbox(&lock.0, &params, None);
    drop(lock);
    match result {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

// --- /api/v1/subscriptions (POST / DELETE / GET) ---------------------------
//
// Two shapes are supported. The webhook shape from the MCP tool
// (`{url, events, secret, namespace_filter, agent_filter}`) is the primary
// contract. Scenario S33 uses a lighter shape (`{agent_id, namespace}`) to
// express "subscribe this agent to a namespace". We accept both: when a
// namespace is supplied without a URL we synthesize an internal loopback URL
// (`http://localhost/_ns/<agent_id>/<namespace>`) that passes SSRF validation
// and sets `agent_filter`/`namespace_filter` accordingly. This lets S33 round-
// trip without needing a separate subscriptions table.

#[derive(Deserialize)]
pub struct SubscribeBody {
    /// Webhook URL — required for the MCP contract, optional for the S33
    /// namespace-subscription shape.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub events: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub namespace_filter: Option<String>,
    #[serde(default)]
    pub agent_filter: Option<String>,
    /// S33 shape: caller-supplied namespace to track.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Optional explicit subscriber id.
    #[serde(default)]
    pub agent_id: Option<String>,
}

pub async fn subscribe(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SubscribeBody>,
) -> impl IntoResponse {
    let caller = match resolve_caller_agent_id(body.agent_id.as_deref(), &headers, None) {
        Ok(id) => id,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    };

    // Rewrite S33's `{agent_id, namespace}` body into the webhook shape.
    let (url, namespace_filter, agent_filter) = if let Some(u) = body.url {
        (u, body.namespace_filter, body.agent_filter)
    } else {
        let Some(ns) = body.namespace.clone() else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "url or namespace is required"})),
            )
                .into_response();
        };
        // Synthetic loopback URL — passes the SSRF allowlist (localhost
        // loopback hostnames are permitted). The synthetic host encodes
        // (agent_id, namespace) so the GET view can round-trip them.
        let synthetic = format!("http://localhost/_ns/{caller}/{ns}");
        (
            synthetic,
            Some(ns),
            body.agent_filter.or_else(|| Some(caller.clone())),
        )
    };

    let events = body.events.unwrap_or_else(|| "*".to_string());

    // Ensure the caller is a registered agent (the MCP tool enforces this).
    // Auto-register for the S33 shape so scenario callers don't have to
    // pre-call /agents themselves — same auto-create pattern used elsewhere
    // for the HTTP surface.
    let lock = app.db.lock().await;
    let already = db::list_agents(&lock.0)
        .ok()
        .is_some_and(|a| a.iter().any(|x| x.agent_id == caller));
    if !already {
        let _ = db::register_agent(&lock.0, &caller, "ai:generic", &[]);
    }
    // Inline subscribe path — we cannot delegate to `mcp::handle_subscribe`
    // here because that helper re-resolves the caller via
    // `resolve_agent_id(None, Some(mcp_client))`, which synthesizes a
    // `ai:<client>@<host>:pid-N` id rather than using the HTTP-resolved
    // `caller` verbatim. An HTTP caller registered under "ai:bob" must be
    // able to subscribe as "ai:bob", not as "ai:ai:bob@host:pid-N".
    let sub_result: Result<serde_json::Value, String> = (|| {
        crate::subscriptions::validate_url(&url).map_err(|e| e.to_string())?;
        let id = crate::subscriptions::insert(
            &lock.0,
            &crate::subscriptions::NewSubscription {
                url: &url,
                events: &events,
                secret: body.secret.as_deref(),
                namespace_filter: namespace_filter.as_deref(),
                agent_filter: agent_filter.as_deref(),
                created_by: Some(&caller),
            },
        )
        .map_err(|e| e.to_string())?;
        Ok(json!({
            "id": id,
            "url": url,
            "events": events,
            "namespace_filter": namespace_filter,
            "agent_filter": agent_filter,
            "created_by": caller,
        }))
    })();
    // Federate the `_agents` write we may have just done so registration is
    // cluster-wide. (Best-effort — subscriptions themselves live in a
    // separate table that does not ride `sync_push` today.)
    let registered_mem = if already {
        None
    } else {
        db::list(
            &lock.0,
            Some("_agents"),
            None,
            1000,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .ok()
        .and_then(|rows| {
            rows.into_iter()
                .find(|m| m.title == format!("agent:{caller}"))
        })
    };
    drop(lock);

    if let Some(ref mem) = registered_mem
        && let Some(resp) = fanout_or_503(&app, mem).await
    {
        return resp;
    }

    match sub_result {
        Ok(mut v) => {
            // Echo the caller's view of the subscription so S33 can find
            // {namespace, agent_id} keys in the response without relying on
            // the synthetic URL.
            if let Some(obj) = v.as_object_mut() {
                if let Some(ref ns) = namespace_filter {
                    obj.insert("namespace".into(), json!(ns));
                }
                obj.insert("agent_id".into(), json!(caller));
            }
            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
pub struct UnsubscribeQuery {
    #[serde(default)]
    pub id: Option<String>,
    /// S33 shape: (`agent_id`, namespace) lookup.
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
}

pub async fn unsubscribe(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<UnsubscribeQuery>,
) -> impl IntoResponse {
    // Prefer explicit id. If absent, dispatch by (agent_id, namespace) for
    // S33 — find the first matching row from list() and delete it.
    if let Some(id) = q.id.clone() {
        let mut params = json!({"id": id});
        // Keep the key name stable across both handlers' interior shapes.
        let _ = params.as_object_mut();
        let lock = app.db.lock().await;
        let result = crate::mcp::handle_unsubscribe(&lock.0, &params);
        drop(lock);
        return match result {
            Ok(v) => (StatusCode::OK, Json(v)).into_response(),
            Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
        };
    }

    let caller = match resolve_caller_agent_id(None, &headers, q.agent_id.as_deref()) {
        Ok(id) => id,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    };
    let Some(ns) = q.namespace else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "id or (agent_id, namespace) required"})),
        )
            .into_response();
    };

    let lock = app.db.lock().await;
    let subs = crate::subscriptions::list(&lock.0).unwrap_or_default();
    let target = subs.into_iter().find(|s| {
        s.namespace_filter.as_deref() == Some(ns.as_str())
            && (s.agent_filter.as_deref() == Some(caller.as_str())
                || s.created_by.as_deref() == Some(caller.as_str()))
    });
    let outcome = match target {
        Some(s) => crate::subscriptions::delete(&lock.0, &s.id).map(|r| (s.id, r)),
        None => Ok((String::new(), false)),
    };
    drop(lock);
    match outcome {
        Ok((id, removed)) => {
            (StatusCode::OK, Json(json!({"id": id, "removed": removed}))).into_response()
        }
        Err(e) => {
            tracing::error!("unsubscribe: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct ListSubscriptionsQuery {
    #[serde(default)]
    pub agent_id: Option<String>,
}

pub async fn list_subscriptions(
    State(state): State<Db>,
    Query(q): Query<ListSubscriptionsQuery>,
) -> impl IntoResponse {
    let lock = state.lock().await;
    let subs = match crate::subscriptions::list(&lock.0) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("list_subscriptions: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };
    drop(lock);
    // Filter by agent_id when the caller passed one (S33's per-agent view).
    let filtered: Vec<_> = match q.agent_id.as_deref() {
        Some(aid) => subs
            .into_iter()
            .filter(|s| {
                s.agent_filter.as_deref() == Some(aid) || s.created_by.as_deref() == Some(aid)
            })
            .collect(),
        None => subs,
    };
    // Expose the subscribed namespace as a top-level field per row so S33 can
    // read `namespace` directly without probing `namespace_filter`.
    let rows: Vec<serde_json::Value> = filtered
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "url": s.url,
                "events": s.events,
                "namespace": s.namespace_filter,
                "namespace_filter": s.namespace_filter,
                "agent_filter": s.agent_filter,
                "agent_id": s.agent_filter.clone().or(s.created_by.clone()),
                "created_by": s.created_by,
                "created_at": s.created_at,
                "dispatch_count": s.dispatch_count,
                "failure_count": s.failure_count,
            })
        })
        .collect();
    let count = rows.len();
    (
        StatusCode::OK,
        Json(json!({"count": count, "subscriptions": rows})),
    )
        .into_response()
}

// --- /api/v1/namespaces/{ns}/standard (POST / GET / DELETE) ----------------
//    +/api/v1/namespaces (POST with body.namespace, GET/DELETE with ?namespace=)
//
// S34/S35 drive the standard via the bare `/api/v1/namespaces` surface; the
// `/namespaces/{ns}/standard` path is kept for API-shape parity with the MCP
// tool namespace. Both share a single underlying implementation.

#[derive(Deserialize)]
pub struct NamespaceStandardBody {
    /// The memory id representing the standard.
    #[serde(default)]
    pub id: Option<String>,
    /// Optional parent namespace for chain lookups.
    #[serde(default)]
    pub parent: Option<String>,
    /// Optional governance policy to merge into the standard's metadata.
    #[serde(default)]
    pub governance: Option<serde_json::Value>,
    /// Accepted for the path-less `/namespaces` form — ignored when the
    /// namespace is supplied via a URL segment.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Some scenarios nest the payload under `standard` (S34 does so).
    #[serde(default)]
    pub standard: Option<Box<NamespaceStandardBody>>,
}

fn flatten_standard_body(body: NamespaceStandardBody) -> NamespaceStandardBody {
    // When the caller nests fields under `standard: { … }` (S34 shape), pull
    // the inner payload up to the top level so the single code path below
    // can read it uniformly.
    if let Some(inner) = body.standard {
        let mut merged = *inner;
        if merged.namespace.is_none() {
            merged.namespace = body.namespace;
        }
        if merged.id.is_none() {
            merged.id = body.id;
        }
        if merged.parent.is_none() {
            merged.parent = body.parent;
        }
        if merged.governance.is_none() {
            merged.governance = body.governance;
        }
        merged
    } else {
        body
    }
}

fn namespace_standard_params(ns: &str, body: &NamespaceStandardBody) -> serde_json::Value {
    let mut params = json!({"namespace": ns});
    if let Some(ref id) = body.id {
        params["id"] = json!(id);
    }
    if let Some(ref p) = body.parent {
        params["parent"] = json!(p);
    }
    if let Some(ref g) = body.governance {
        params["governance"] = g.clone();
    }
    params
}

async fn set_namespace_standard_inner(
    app: &AppState,
    ns: &str,
    body: NamespaceStandardBody,
) -> axum::response::Response {
    let body = flatten_standard_body(body);
    // Auto-seed a placeholder standard memory when the caller didn't supply
    // an `id`. S34's body is `{governance: …}` with no id — we create a
    // minimal standard memory so the governance policy has a home.
    let lock = app.db.lock().await;
    let resolved_id = if let Some(id) = body.id.clone() {
        id
    } else {
        // Look for an existing placeholder first to keep repeat calls
        // idempotent; otherwise insert a new row.
        let existing = db::list(
            &lock.0,
            Some(ns),
            None,
            1,
            0,
            None,
            None,
            None,
            Some("_namespace_standard"),
            None,
        )
        .ok()
        .and_then(|v| v.into_iter().next());
        if let Some(m) = existing {
            m.id
        } else {
            let now = Utc::now().to_rfc3339();
            let placeholder = Memory {
                id: Uuid::new_v4().to_string(),
                tier: Tier::Long,
                namespace: ns.to_string(),
                title: format!("_standard:{ns}"),
                content: format!("namespace standard for {ns}"),
                tags: vec!["_namespace_standard".to_string()],
                priority: 5,
                confidence: 1.0,
                source: "api".into(),
                access_count: 0,
                created_at: now.clone(),
                updated_at: now,
                last_accessed_at: None,
                expires_at: None,
                metadata: serde_json::json!({"agent_id": "system"}),
            };
            match db::insert(&lock.0, &placeholder) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("namespace_standard: placeholder insert failed: {e}");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "internal server error"})),
                    )
                        .into_response();
                }
            }
        }
    };
    let mut effective = body;
    effective.id = Some(resolved_id.clone());
    let params = namespace_standard_params(ns, &effective);
    let result = crate::mcp::handle_namespace_set_standard(&lock.0, &params);
    // Capture the standard memory so we can fan it out to peers — cluster
    // visibility of governance rules matters for S34/S35.
    let standard_mem = db::get(&lock.0, &resolved_id).ok().flatten();
    // v0.6.2 (S35): also capture the freshly-written namespace_meta row
    // so peers learn the explicit (namespace, standard_id, parent) tuple.
    // Without this, peers auto-detect a parent via `-` prefix which may
    // disagree with what the originator set.
    let meta_entry = db::get_namespace_meta_entry(&lock.0, ns).ok().flatten();
    drop(lock);

    match result {
        Ok(v) => {
            if let Some(ref mem) = standard_mem
                && let Some(resp) = fanout_or_503(app, mem).await
            {
                return resp;
            }
            if let (Some(entry), Some(fed)) = (meta_entry.as_ref(), app.federation.as_ref()) {
                match crate::federation::broadcast_namespace_meta_quorum(fed, entry).await {
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
            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

pub async fn set_namespace_standard(
    State(app): State<AppState>,
    Path(ns): Path<String>,
    Json(body): Json<NamespaceStandardBody>,
) -> impl IntoResponse {
    set_namespace_standard_inner(&app, &ns, body).await
}

#[derive(Deserialize)]
pub struct NamespaceStandardQuery {
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub inherit: Option<bool>,
}

pub async fn get_namespace_standard(
    State(state): State<Db>,
    Path(ns): Path<String>,
    Query(q): Query<NamespaceStandardQuery>,
) -> impl IntoResponse {
    let mut params = json!({"namespace": ns});
    if let Some(inh) = q.inherit {
        params["inherit"] = json!(inh);
    }
    let lock = state.lock().await;
    let result = crate::mcp::handle_namespace_get_standard(&lock.0, &params);
    drop(lock);
    match result {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

pub async fn clear_namespace_standard(
    State(app): State<AppState>,
    Path(ns): Path<String>,
) -> impl IntoResponse {
    clear_namespace_standard_inner(&app, &ns).await
}

// Query-string forms for the S34/S35 `/api/v1/namespaces?namespace=…` shape.
pub async fn set_namespace_standard_qs(
    State(app): State<AppState>,
    Json(body): Json<NamespaceStandardBody>,
) -> impl IntoResponse {
    let Some(ns) = body
        .namespace
        .clone()
        .or_else(|| body.standard.as_ref().and_then(|s| s.namespace.clone()))
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "namespace is required"})),
        )
            .into_response();
    };
    set_namespace_standard_inner(&app, &ns, body).await
}

pub async fn get_namespace_standard_qs(
    State(state): State<Db>,
    Query(q): Query<NamespaceStandardQuery>,
) -> impl IntoResponse {
    // If no namespace is supplied this shares a route with the existing
    // `list_namespaces` GET; the router chains the two so a plain
    // `GET /api/v1/namespaces` still returns the list.
    let Some(ns) = q.namespace.clone() else {
        return list_namespaces(State(state)).await.into_response();
    };
    let mut params = json!({"namespace": ns});
    if let Some(inh) = q.inherit {
        params["inherit"] = json!(inh);
    }
    let lock = state.lock().await;
    let result = crate::mcp::handle_namespace_get_standard(&lock.0, &params);
    drop(lock);
    match result {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

pub async fn clear_namespace_standard_qs(
    State(app): State<AppState>,
    Query(q): Query<NamespaceStandardQuery>,
) -> impl IntoResponse {
    let Some(ns) = q.namespace else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "namespace is required"})),
        )
            .into_response();
    };
    clear_namespace_standard_inner(&app, &ns).await
}

/// v0.6.2 (S35 follow-up): shared implementation for path and query-string
/// clear handlers. Runs the local clear then, on success, fans the cleared
/// namespace out to peers via `broadcast_namespace_meta_clear_quorum`.
/// Returns 503 `quorum_not_met` when federation is configured and the quorum
/// contract fails — matching the pattern established by
/// `set_namespace_standard_inner`.
async fn clear_namespace_standard_inner(app: &AppState, ns: &str) -> axum::response::Response {
    let params = json!({"namespace": ns});
    let lock = app.db.lock().await;
    let result = crate::mcp::handle_namespace_clear_standard(&lock.0, &params);
    drop(lock);
    match result {
        Ok(v) => {
            if let Some(fed) = app.federation.as_ref() {
                let namespaces = vec![ns.to_string()];
                match crate::federation::broadcast_namespace_meta_clear_quorum(fed, &namespaces)
                    .await
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
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

// --- /api/v1/session/start (POST) ------------------------------------------

#[derive(Deserialize)]
pub struct SessionStartBody {
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub agent_id: Option<String>,
}

pub async fn session_start(
    State(state): State<Db>,
    headers: HeaderMap,
    Json(body): Json<SessionStartBody>,
) -> impl IntoResponse {
    // agent_id is optional for session_start; but if supplied it must validate.
    if let Some(ref id) = body.agent_id
        && let Err(e) = validate::validate_agent_id(id)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid agent_id: {e}")})),
        )
            .into_response();
    }
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let _ = header_agent_id; // identity currently informational for session_start
    let mut params = json!({});
    if let Some(ref n) = body.namespace {
        params["namespace"] = json!(n);
    }
    if let Some(l) = body.limit {
        params["limit"] = json!(l);
    }
    let lock = state.lock().await;
    let result = crate::mcp::handle_session_start(&lock.0, &params, None);
    drop(lock);
    match result {
        Ok(mut v) => {
            // Stamp a stable session id so callers (S36) can correlate
            // subsequent writes. We don't persist sessions today; the id is
            // advisory and round-tripped via metadata by the caller.
            if let Some(obj) = v.as_object_mut() {
                obj.entry("session_id")
                    .or_insert_with(|| json!(Uuid::new_v4().to_string()));
                if let Some(ref a) = body.agent_id {
                    obj.insert("agent_id".into(), json!(a));
                }
            }
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
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
            tier_config: Arc::new(crate::config::FeatureTier::Keyword.config()),
            scoring: Arc::new(crate::config::ResolvedScoring::default()),
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
            .with_state(test_app_state(state.clone()));

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
    async fn http_sync_push_applies_archives() {
        // S29 — sync_push must accept an `archives` field and move matching
        // rows from `memories` to `archived_memories` via
        // `db::archive_memory`. Missing ids no-op. The response exposes a
        // new `archived` counter.
        let state = test_state();
        // Seed one row that the peer will ask us to archive; one id that
        // doesn't exist here (must no-op, not error).
        let id = {
            let lock = state.lock().await;
            let now = Utc::now().to_rfc3339();
            let mem = Memory {
                id: Uuid::new_v4().to_string(),
                tier: Tier::Long,
                namespace: "s29".into(),
                title: "Archive M1".into(),
                content: "body".into(),
                tags: vec![],
                priority: 5,
                confidence: 1.0,
                source: "api".into(),
                access_count: 0,
                created_at: now.clone(),
                updated_at: now,
                last_accessed_at: None,
                expires_at: None,
                metadata: serde_json::json!({}),
            };
            db::insert(&lock.0, &mem).unwrap()
        };

        let app = Router::new()
            .route("/api/v1/sync/push", axum_post(sync_push))
            .with_state(test_app_state(state.clone()));

        let body = serde_json::json!({
            "sender_agent_id": "peer-a",
            "sender_clock": {"entries": {}},
            "memories": [],
            "archives": [id, "missing-on-peer"],
            "dry_run": false
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
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["archived"], 1, "live row must be archived");
        assert_eq!(v["noop"], 1, "missing id must no-op");

        // Row is gone from active memories, present in archive, with the
        // correct `sync_push` reason.
        let lock = state.lock().await;
        assert!(db::get(&lock.0, &id).unwrap().is_none());
        let archived = db::list_archived(&lock.0, None, 10, 0).unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0]["id"], id);
        assert_eq!(archived[0]["archive_reason"], "sync_push");
    }

    #[tokio::test]
    async fn http_archive_by_ids_happy_path() {
        // S29 — POST /api/v1/archive with `{ids:[...]}` soft-moves each
        // live row to the archive table with the supplied reason.
        // Missing ids are reported in a `missing` array, not an error.
        let state = test_state();
        let live_id = {
            let lock = state.lock().await;
            let now = Utc::now().to_rfc3339();
            let mem = Memory {
                id: Uuid::new_v4().to_string(),
                tier: Tier::Long,
                namespace: "s29".into(),
                title: "Live for archive".into(),
                content: "will be archived".into(),
                tags: vec![],
                priority: 5,
                confidence: 1.0,
                source: "api".into(),
                access_count: 0,
                created_at: now.clone(),
                updated_at: now,
                last_accessed_at: None,
                expires_at: None,
                metadata: serde_json::json!({}),
            };
            db::insert(&lock.0, &mem).unwrap()
        };

        let app = Router::new()
            .route("/api/v1/archive", axum_post(archive_by_ids))
            .with_state(test_app_state(state.clone()));

        let body = serde_json::json!({
            "ids": [live_id, "does-not-exist"],
            "reason": "scenario_s29"
        });
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/archive")
                    .method("POST")
                    .header("content-type", "application/json")
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
        assert_eq!(v["count"], 1);
        assert_eq!(v["archived"].as_array().unwrap().len(), 1);
        assert_eq!(v["missing"].as_array().unwrap().len(), 1);
        assert_eq!(v["reason"], "scenario_s29");

        // Row is gone from active, present in archive with caller's reason.
        let lock = state.lock().await;
        assert!(db::get(&lock.0, &live_id).unwrap().is_none());
        let archived = db::list_archived(&lock.0, None, 10, 0).unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0]["id"], live_id);
        assert_eq!(archived[0]["archive_reason"], "scenario_s29");
    }

    #[tokio::test]
    async fn http_archive_by_ids_default_reason() {
        // When `reason` is omitted the response + archive row must record
        // the default "archive" reason (matches `db::archive_memory`).
        let state = test_state();
        let live_id = {
            let lock = state.lock().await;
            let now = Utc::now().to_rfc3339();
            let mem = Memory {
                id: Uuid::new_v4().to_string(),
                tier: Tier::Long,
                namespace: "s29-default".into(),
                title: "Default reason".into(),
                content: "c".into(),
                tags: vec![],
                priority: 5,
                confidence: 1.0,
                source: "api".into(),
                access_count: 0,
                created_at: now.clone(),
                updated_at: now,
                last_accessed_at: None,
                expires_at: None,
                metadata: serde_json::json!({}),
            };
            db::insert(&lock.0, &mem).unwrap()
        };

        let app = Router::new()
            .route("/api/v1/archive", axum_post(archive_by_ids))
            .with_state(test_app_state(state.clone()));
        let body = serde_json::json!({"ids": [live_id]});
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/archive")
                    .method("POST")
                    .header("content-type", "application/json")
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
        assert_eq!(v["reason"], "archive");
        let lock = state.lock().await;
        let archived = db::list_archived(&lock.0, None, 10, 0).unwrap();
        assert_eq!(archived[0]["archive_reason"], "archive");
    }

    #[tokio::test]
    async fn http_bulk_create_uses_appstate_and_persists() {
        // S40 prep — bulk_create previously took `State<Db>` with no path
        // to `app.federation`, so every bulk row stayed on the originator.
        // Signature is now `State<AppState>` and each row is persisted.
        let state = test_state();
        let app = Router::new()
            .route("/api/v1/memories/bulk", axum_post(bulk_create))
            .with_state(test_app_state(state.clone()));

        let bodies: Vec<serde_json::Value> = (0..5)
            .map(|i| {
                serde_json::json!({
                    "tier": "long",
                    "namespace": "bulk-appstate",
                    "title": format!("bulk-{i}"),
                    "content": format!("body-{i}"),
                    "tags": [],
                    "priority": 5,
                    "confidence": 1.0,
                    "source": "api",
                    "metadata": {}
                })
            })
            .collect();
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/memories/bulk")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&bodies).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["created"], 5);
        assert!(v["errors"].as_array().unwrap().is_empty());

        // Every row is visible in the DB (was the S40 gap — rows never
        // made it past the local insert loop, leaving peers empty).
        let lock = state.lock().await;
        let rows = db::list(
            &lock.0,
            Some("bulk-appstate"),
            None,
            100,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(rows.len(), 5, "bulk rows must persist via AppState");
    }

    #[tokio::test]
    async fn http_bulk_create_fans_out_with_federation() {
        // S40 — with federation configured, each successfully-inserted row
        // in a bulk call must fan out to every peer. We spin up an axum
        // mock peer that records sync_push POSTs and bulk-create N rows;
        // the mock must see N POSTs (background-detached + foreground).
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::net::TcpListener;

        let state = test_state();

        // Mock peer that counts sync_push POSTs and always acks.
        let count = Arc::new(AtomicUsize::new(0));
        let count_for_peer = count.clone();
        #[derive(Clone)]
        struct MockState {
            count: Arc<AtomicUsize>,
        }
        async fn mock_sync_push(
            axum::extract::State(s): axum::extract::State<MockState>,
            Json(_body): Json<serde_json::Value>,
        ) -> (StatusCode, Json<serde_json::Value>) {
            s.count.fetch_add(1, Ordering::Relaxed);
            (
                StatusCode::OK,
                Json(json!({"applied":1,"noop":0,"skipped":0})),
            )
        }
        let peer_app = Router::new()
            .route("/api/v1/sync/push", axum_post(mock_sync_push))
            .with_state(MockState {
                count: count_for_peer,
            });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, peer_app).await.ok();
        });

        // Build a FederationConfig that targets the mock.
        let peer_url = format!("http://{addr}");
        let fed = crate::federation::FederationConfig::build(
            2, // W=2 — local + 1 peer
            &[peer_url],
            std::time::Duration::from_millis(2000),
            None,
            None,
            None,
            "ai:bulk-test".to_string(),
        )
        .unwrap()
        .expect("federation must be built");

        let app_state = AppState {
            db: state.clone(),
            embedder: Arc::new(None),
            vector_index: Arc::new(Mutex::new(None)),
            federation: Arc::new(Some(fed)),
            tier_config: Arc::new(crate::config::FeatureTier::Keyword.config()),
            scoring: Arc::new(crate::config::ResolvedScoring::default()),
        };
        let router = Router::new()
            .route("/api/v1/memories/bulk", axum_post(bulk_create))
            .with_state(app_state);

        // 4 rows — keeps the test fast while proving fanout ran per-row.
        let n = 4;
        let bodies: Vec<serde_json::Value> = (0..n)
            .map(|i| {
                serde_json::json!({
                    "tier": "long",
                    "namespace": "bulk-fanout",
                    "title": format!("bulk-fanout-{i}"),
                    "content": "c",
                    "tags": [],
                    "priority": 5,
                    "confidence": 1.0,
                    "source": "api",
                    "metadata": {}
                })
            })
            .collect();
        let resp = router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/memories/bulk")
                    .method("POST")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&bodies).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["created"], n);

        // Foreground fanout already waits for W-1 acks per row, so the
        // per-row POST has landed by the time the request returns. v0.6.2
        // Patch 2 (S40) adds a terminal catchup batch — one extra POST
        // per peer with the full row set — so the expected total is
        // `n + 1` per peer. Give detached stragglers a quick window.
        let expected = n + 1;
        for _ in 0..20 {
            if count.load(Ordering::Relaxed) >= expected {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            count.load(Ordering::Relaxed),
            expected,
            "mock peer must receive one sync_push POST per bulk row plus one terminal catchup batch"
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
            .with_state(test_app_state(state));
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
            .with_state(test_app_state(state.clone()));

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
    async fn http_contradictions_surfaces_same_topic_candidates_and_synth_link() {
        // v0.6.0.1 (#321) — GET /api/v1/contradictions?topic=X&namespace=Y
        // returns the candidate memories sharing the topic and a synthesized
        // contradicts link between any pair with differing content.
        let state = test_state();
        let now = Utc::now().to_rfc3339();

        // Seed two memories with metadata.topic=T and DIFFERENT content. We
        // use distinct titles so UPSERT-on-(title,namespace) doesn't dedup —
        // that's the scenario-6 fix in ai2ai-gate.
        {
            let lock = state.lock().await;
            let topic = "sky-color-test";
            for (title, agent, content) in [
                ("sky-color-test-alice", "ai:alice", "sky-color-test is blue"),
                ("sky-color-test-bob", "ai:bob", "sky-color-test is red"),
            ] {
                let mem = Memory {
                    id: Uuid::new_v4().to_string(),
                    tier: Tier::Mid,
                    namespace: "contradictions-test".into(),
                    title: title.into(),
                    content: content.into(),
                    tags: vec![],
                    priority: 5,
                    confidence: 1.0,
                    source: "api".into(),
                    access_count: 0,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                    last_accessed_at: None,
                    expires_at: None,
                    metadata: serde_json::json!({
                        "agent_id": agent,
                        "topic": topic,
                    }),
                };
                db::insert(&lock.0, &mem).unwrap();
            }
        }

        let app = Router::new()
            .route("/api/v1/contradictions", axum_get(detect_contradictions))
            .with_state(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(
                        "/api/v1/contradictions?topic=sky-color-test&namespace=contradictions-test",
                    )
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

        let memories = v["memories"].as_array().unwrap();
        assert_eq!(memories.len(), 2, "both candidates should be returned");

        let links = v["links"].as_array().unwrap();
        let synth_contradict = links.iter().find(|l| {
            l["relation"].as_str() == Some("contradicts")
                && l["synthesized"].as_bool() == Some(true)
        });
        assert!(
            synth_contradict.is_some(),
            "expected a synthesized contradicts link between alice and bob"
        );
    }

    #[tokio::test]
    async fn http_contradictions_requires_topic_or_namespace() {
        // Guard: calling the endpoint with neither topic nor namespace is a
        // 400 — we refuse to scan the whole DB by accident.
        let state = test_state();
        let app = Router::new()
            .route("/api/v1/contradictions", axum_get(detect_contradictions))
            .with_state(state);
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/contradictions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
            .with_state(test_app_state(state.clone()));

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
    async fn http_sync_push_applies_incoming_links() {
        // v0.6.2 (#325) — sync_push's `links` field applies the listed
        // (source, target, relation) triples via db::create_link on the
        // receiver so peer-side link fanout works for scenario-11.
        // (a2a-hermes-v0.6.1-r15.)
        let state = test_state();
        let now = Utc::now().to_rfc3339();

        // Seed two memories on the receiver so the link has valid endpoints.
        let (m1, m2) = {
            let lock = state.lock().await;
            let m1 = Memory {
                id: Uuid::new_v4().to_string(),
                tier: Tier::Mid,
                namespace: "link-fanout".into(),
                title: "source".into(),
                content: "a".into(),
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
            let m1_id = db::insert(&lock.0, &m1).unwrap();
            let m2 = Memory {
                id: Uuid::new_v4().to_string(),
                tier: Tier::Mid,
                namespace: "link-fanout".into(),
                title: "target".into(),
                content: "b".into(),
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
            let m2_id = db::insert(&lock.0, &m2).unwrap();
            (m1_id, m2_id)
        };

        let app = Router::new()
            .route("/api/v1/sync/push", axum_post(sync_push))
            .with_state(test_app_state(state.clone()));

        let body = serde_json::json!({
            "sender_agent_id": "peer-alice",
            "sender_clock": {"entries": {}},
            "memories": [],
            "links": [{
                "source_id": m1,
                "target_id": m2,
                "relation": "related_to",
                "created_at": now,
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
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["links_applied"], 1);

        let lock = state.lock().await;
        let links = db::get_links(&lock.0, &m1).unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_id, m2);
        assert_eq!(links[0].relation, "related_to");
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
    async fn http_sync_since_includes_s39_diagnostic_fields() {
        // S39 — the response must echo `updated_since` (parsed `since`)
        // and earliest/latest `updated_at` from the returned set. This
        // lets the scenario pin whether the server saw the expected
        // checkpoint without changing the set-returning behavior.
        let state = test_state();
        // Seed three rows in strictly-ordered time so earliest != latest.
        let mid_ts = "2024-06-01T00:00:00+00:00";
        let newer_ts = "2025-06-01T00:00:00+00:00";
        let newest_ts = "2026-01-01T00:00:00+00:00";
        {
            let lock = state.lock().await;
            for (title, ts) in [("mid", mid_ts), ("newer", newer_ts), ("newest", newest_ts)] {
                let mem = Memory {
                    id: Uuid::new_v4().to_string(),
                    tier: Tier::Long,
                    namespace: "s39-diag".into(),
                    title: title.into(),
                    content: "c".into(),
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
            .with_state(state.clone());

        // Ask for rows strictly after 2024-01 — should return all 3.
        let since = "2024-01-01T00:00:00%2B00:00";
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/api/v1/sync/since?since={since}"))
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
        assert_eq!(v["count"], 3);
        // Echoed `since` (unparsed, verbatim — that's the point).
        assert_eq!(v["updated_since"], "2024-01-01T00:00:00+00:00");
        assert_eq!(v["earliest_updated_at"], mid_ts);
        assert_eq!(v["latest_updated_at"], newest_ts);

        // Empty set → both timestamp fields are null. The `updated_since`
        // field still echoes the parsed input.
        let empty_app = Router::new()
            .route("/api/v1/sync/since", axum_get(sync_since))
            .with_state(state);
        let resp = empty_app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sync/since?since=2099-01-01T00:00:00%2B00:00")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["count"], 0);
        assert!(v["earliest_updated_at"].is_null());
        assert!(v["latest_updated_at"].is_null());
        assert_eq!(v["updated_since"], "2099-01-01T00:00:00+00:00");
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
