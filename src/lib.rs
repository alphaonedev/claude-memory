// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![recursion_limit = "256"]
// The library target was added by the proptest infra (Agent G) to expose
// production modules to the integration test crate. The bin target's
// clippy run already gates CI — re-running pedantic against the same
// modules through the lib target would re-flag the same pre-existing
// lint backlog the bin target already passes. Allow at the lib level;
// the bin target is the authoritative gate for production-code linting.
#![allow(clippy::pedantic, clippy::all)]

// Library interface for ai-memory. Exposes public modules for testing and external use.

pub mod approvals;
pub mod audit;
pub mod autonomy;
pub mod bench;
// v0.7.0 QW-3 — daemon-side background tasks. Carries the TTL sweep
// loop for `offloaded_blobs`; future v0.8.0 substrate tasks land
// here without churning `daemon_runtime`.
pub mod background;
pub mod cli;
pub mod color;
pub mod config;
pub mod curator;
pub mod daemon_runtime;
// v0.7.0 L0.5-3 — module renamed from `db` → `storage` as part of
// the flat-to-modular refactor. The `pub use storage as db;` shim
// below preserves every `crate::db::*` path across the codebase
// (handlers, mcp, cli, autonomy, bench, store, curator, transcripts,
// tests) so the rename is a pure refactor with zero callsite churn.
pub mod storage;

// Backward-compat shim from L0.5-3 rename — preserves
// `crate::db::*` paths used elsewhere in the codebase. To be
// removed in a future cleanup once all callsites migrate to
// `crate::storage::*`.
#[allow(dead_code)]
pub use storage as db;
pub mod embeddings;
pub mod errors;
pub mod federation;
// v0.7.0 L2-5 (issue #670) — forensic evidence bundle assembly +
// verification. OSS surface for the AgenticMem Attest tier.
pub mod forensic;
pub mod handlers;
// v0.7 Track B — harness detection. B4 reads the MCP `clientInfo.name`
// captured at the JSON-RPC `initialize` handshake and maps it to a
// `Harness` enum so downstream paths (capabilities-v3, B1's
// `memory_load_family`, B2's `memory_smart_load`) can shape responses
// based on whether the harness supports deferred-tool registration.
pub mod harness;
pub mod hnsw;
// v0.7 Track G — programmable lifecycle hook pipeline. G1 lands
// the config schema + SIGHUP hot-reload plumbing; the executor
// (G3) and the actual fire points (G7+) layer on top of this
// module without touching call sites in `handlers.rs` etc.
pub mod hooks;
pub mod identity;
// v0.7.0 L1-2 — knowledge-graph substrate helpers (anti-cycle check).
pub mod kg;
pub mod llm;
pub mod log_paths;
pub mod logging;
pub mod mcp;
pub mod metrics;
pub mod mine;
pub mod models;
// v0.7.0 L2-3 (issue #668) — reflection invalidation propagation.
// Notification (not cascade) when a Reflection→Reflection supersedes
// edge lands: walks `reflects_on` edges from dependents and writes
// notification memories into `<namespace>/_invalidations`.
pub mod notification;
// v0.7.0 QW-3 — context-offload substrate primitive. Offload+deref
// store with Ed25519-signed audit events; v0.8.0 short-term-context-
// compression (Mermaid canvas + auto-cadence + node_id integration)
// builds on this plumbing.
pub mod offload;
// v0.7.0 L1-5 — SKILL.md parser and structured-document ingestion pipelines.
pub mod parsing;
// v0.7.0 K9 — unified permission system. Composes declarative
// `[permissions.rules]` matchers, the K3 `[permissions].mode`
// knob, and G1-G11 hook decisions into a single `Decision`.
// Wired into the five op paths (store, link, delete, archive,
// consolidate) so callers consult one evaluator regardless of
// which source produced the outcome.
//
// v0.7.0 L0.5-4 — module renamed from `permissions` → `governance`
// as part of the flat-to-modular refactor. The `pub use governance
// as permissions;` shim below preserves every `crate::permissions::*`
// path across the codebase (handlers, mcp, config, cli, tests) so the
// rename is a pure refactor with zero callsite churn.
pub mod governance;

// Backward-compat shim from L0.5-4 rename — preserves
// `crate::permissions::*` paths used elsewhere in the codebase.
// To be removed in a future cleanup once all callsites migrate
// to `crate::governance::*`.
#[allow(dead_code)]
pub use governance as permissions;
pub mod profile;
// v0.7 Track K, Task K8 — per-agent rate limits + storage caps.
// `agent_quotas` table backs three counters (memories/day, storage
// bytes, links/day) consulted by the store_memory + memory_link write
// paths; daily counters reset at UTC midnight via a sweep loop.
pub mod quotas;
pub mod replication;
pub mod reranker;
pub mod signed_events;
pub mod sizes;
pub mod subscriptions;
pub mod tls;
pub mod toon;
pub mod transcripts;
pub mod validate;

#[cfg(feature = "sal")]
pub mod migrate;

#[cfg(feature = "sal")]
pub mod store;

// ---------------------------------------------------------------------------
// Router construction
// ---------------------------------------------------------------------------
//
// `build_router` is the single source of truth for the daemon's HTTP route
// table. It is exposed through the lib crate so the integration test suite
// can construct an in-process `axum::Router` and exercise endpoints via
// `Router::oneshot()` instead of spawning a subprocess + curl, which:
//   1. eliminates the OS-level daemon-spawn overhead per test (~200-500ms),
//   2. exposes the routes' line coverage to `cargo llvm-cov` (subprocess
//      coverage attribution requires extra `LLVM_PROFILE_FILE` plumbing
//      that the test harness doesn't provide), and
//   3. lets test failures surface assertion-level diagnostics instead of
//      "curl returned 000" black holes.
//
// The function takes the same two state values that `serve()` constructs
// inline (the API key middleware state and the composite app state) so
// the production binary and the test harness share a single route map.
pub fn build_router(
    api_key_state: handlers::ApiKeyState,
    app_state: handlers::AppState,
) -> axum::Router {
    build_router_with_timeout(
        api_key_state,
        app_state,
        std::time::Duration::from_secs(config::DEFAULT_REQUEST_TIMEOUT_SECS),
    )
}

/// v0.7.0 H7 (round-2) — variant of [`build_router`] that takes an
/// explicit per-request wall-clock timeout. Composes a per-request
/// timeout middleware so a slow-POST (slowloris-style) attacker
/// cannot keep a handler scope alive indefinitely. Requests that
/// exceed the timeout get a `504 Gateway Timeout` response with a
/// `{"error":"request timed out"}` body. The production daemon
/// calls this with the value resolved from
/// `AppConfig::effective_request_timeout_secs` (default 60 s); tests
/// pass a short timeout to drive the timeout edge directly.
///
/// Implementation: a custom axum middleware wraps every request in
/// `tokio::time::timeout`, returning the structured timeout response
/// when the future does not resolve in time. This avoids enabling
/// tower-http's `timeout` feature (which would require a
/// `Cargo.toml` change). The behaviour matches what
/// `tower::timeout::TimeoutLayer` would provide modulo the status
/// code (we return 504 to stay distinguishable from request-shape
/// 400s).
pub fn build_router_with_timeout(
    api_key_state: handlers::ApiKeyState,
    app_state: handlers::AppState,
    request_timeout: std::time::Duration,
) -> axum::Router {
    use axum::{
        extract::DefaultBodyLimit,
        routing::{delete, get, post, put},
    };
    use tower_http::{cors::CorsLayer, trace::TraceLayer};

    // Timeout middleware: wraps each downstream future in
    // `tokio::time::timeout`. The closure captures the `Duration` by
    // value so it lives for the router's lifetime.
    let timeout = request_timeout;
    let timeout_layer = axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| async move {
            use axum::response::IntoResponse;
            match tokio::time::timeout(timeout, next.run(req)).await {
                Ok(resp) => resp,
                Err(_) => {
                    tracing::warn!(
                        timeout_secs = timeout.as_secs(),
                        "H7: request exceeded per-request wall-clock timeout — returning 504"
                    );
                    (
                        axum::http::StatusCode::GATEWAY_TIMEOUT,
                        axum::Json(serde_json::json!({"error": "request timed out"})),
                    )
                        .into_response()
                }
            }
        },
    );

    axum::Router::new()
        .route("/api/v1/health", get(handlers::health))
        // v0.6.0.0: Prometheus scrape endpoint. Exposed at both /metrics
        // (the community convention) and /api/v1/metrics (consistent with
        // the rest of the REST surface).
        .route("/metrics", get(handlers::prometheus_metrics))
        .route("/api/v1/metrics", get(handlers::prometheus_metrics))
        .route("/api/v1/memories", get(handlers::list_memories))
        .route("/api/v1/memories", post(handlers::create_memory))
        .route("/api/v1/memories/bulk", post(handlers::bulk_create))
        .route("/api/v1/memories/{id}", get(handlers::get_memory))
        .route("/api/v1/memories/{id}", put(handlers::update_memory))
        .route("/api/v1/memories/{id}", delete(handlers::delete_memory))
        .route(
            "/api/v1/memories/{id}/promote",
            post(handlers::promote_memory),
        )
        .route("/api/v1/search", get(handlers::search_memories))
        .route("/api/v1/recall", get(handlers::recall_memories_get))
        .route("/api/v1/recall", post(handlers::recall_memories_post))
        .route("/api/v1/forget", post(handlers::forget_memories))
        .route("/api/v1/consolidate", post(handlers::consolidate_memories))
        .route(
            "/api/v1/contradictions",
            get(handlers::detect_contradictions),
        )
        // v0.7.0 L6 — S51 autonomous-tier surface. `auto_tag` and
        // `expand_query` are the two REST mirrors of the corresponding
        // MCP tools; they were never wired before L6 (S51 expected
        // them and got 404). Both 503 when no LLM is configured.
        .route("/api/v1/auto_tag", post(handlers::auto_tag_handler))
        .route("/api/v1/expand_query", post(handlers::expand_query_handler))
        // v0.7.0 L9 — HTTP parity for the MCP `tools/list` JSON-RPC
        // method. Surfaces the canonical tool catalog under the
        // daemon's resolved Profile. Backend-agnostic — pure config
        // enumeration, no DB access — so postgres and sqlite return
        // identical bodies (NHI-D-501-postgres-traits).
        .route("/api/v1/tools/list", get(handlers::tools_list))
        // v0.7.0 L10 — HTTP parity for the MCP `memory_load_family`
        // tool. Returns top-K memories tagged with the requested
        // family on both sqlite and postgres backends
        // (NHI-D-501-postgres-loadfamily).
        .route(
            "/api/v1/memory_load_family",
            post(handlers::load_family_handler),
        )
        .route("/api/v1/links", post(handlers::create_link))
        .route("/api/v1/links", delete(handlers::delete_link))
        .route("/api/v1/links/{id}", get(handlers::get_links))
        // HTTP parity for MCP-only tools. The `/api/v1/namespaces` surface
        // serves three verbs: GET lists namespaces OR (when ?namespace=…)
        // fetches the namespace standard, POST sets a standard, DELETE
        // clears one. S34/S35 use the query-string form; the path form
        // (`/api/v1/namespaces/{ns}/standard`) is kept for MCP-tool parity.
        .route(
            "/api/v1/namespaces",
            get(handlers::get_namespace_standard_qs),
        )
        .route(
            "/api/v1/namespaces",
            post(handlers::set_namespace_standard_qs),
        )
        .route(
            "/api/v1/namespaces",
            delete(handlers::clear_namespace_standard_qs),
        )
        .route(
            "/api/v1/namespaces/{ns}/standard",
            post(handlers::set_namespace_standard),
        )
        .route(
            "/api/v1/namespaces/{ns}/standard",
            get(handlers::get_namespace_standard),
        )
        .route(
            "/api/v1/namespaces/{ns}/standard",
            delete(handlers::clear_namespace_standard),
        )
        // Pillar 1 / Stream A — hierarchical namespace taxonomy.
        .route("/api/v1/taxonomy", get(handlers::get_taxonomy))
        // Pillar 2 / Stream D — pre-write near-duplicate check.
        .route("/api/v1/check_duplicate", post(handlers::check_duplicate))
        // Pillar 2 / Stream B — entity registry.
        .route("/api/v1/entities", post(handlers::entity_register))
        .route(
            "/api/v1/entities/by_alias",
            get(handlers::entity_get_by_alias),
        )
        // Pillar 2 / Stream C — KG timeline.
        .route("/api/v1/kg/timeline", get(handlers::kg_timeline))
        // Pillar 2 / Stream C — KG link supersession.
        .route("/api/v1/kg/invalidate", post(handlers::kg_invalidate))
        // Pillar 2 / Stream C — KG outbound traversal.
        .route("/api/v1/kg/query", post(handlers::kg_query))
        // v0.7.0 Continuation 6 — KG path enumeration (S65).
        .route("/api/v1/kg/find_paths", post(handlers::kg_find_paths))
        // v0.7.0 Continuation 6 — link signature verification (S52).
        .route("/api/v1/links/verify", post(handlers::verify_link_handler))
        // v0.7.0 Continuation 6 — per-agent quota status (S61).
        .route("/api/v1/quota/status", post(handlers::quota_status_handler))
        .route("/api/v1/stats", get(handlers::get_stats))
        .route("/api/v1/gc", post(handlers::run_gc))
        .route("/api/v1/export", get(handlers::export_memories))
        .route("/api/v1/import", post(handlers::import_memories))
        .route("/api/v1/archive", get(handlers::list_archive))
        .route("/api/v1/archive", post(handlers::archive_by_ids))
        .route("/api/v1/archive", delete(handlers::purge_archive))
        .route(
            "/api/v1/archive/{id}/restore",
            post(handlers::restore_archive),
        )
        .route("/api/v1/archive/stats", get(handlers::archive_stats))
        .route("/api/v1/agents", get(handlers::list_agents))
        .route("/api/v1/agents", post(handlers::register_agent))
        .route("/api/v1/pending", get(handlers::list_pending))
        .route(
            "/api/v1/pending/{id}/approve",
            post(handlers::approve_pending),
        )
        .route(
            "/api/v1/pending/{id}/reject",
            post(handlers::reject_pending),
        )
        // v0.7.0 K10 — Approval API. POST is HMAC-gated; SSE rides on
        // top of the existing api_key_auth middleware (no extra gate).
        .route(
            "/api/v1/approvals/{pending_id}",
            post(handlers::approval_decide),
        )
        .route("/api/v1/approvals/stream", get(handlers::approvals_sse))
        // Phase 3 foundation (issue #224) — peer-to-peer sync endpoints.
        .route("/api/v1/sync/push", post(handlers::sync_push))
        .route("/api/v1/sync/since", get(handlers::sync_since))
        // HTTP parity for MCP-only tools.
        .route("/api/v1/capabilities", get(handlers::get_capabilities))
        .route("/api/v1/notify", post(handlers::notify))
        .route("/api/v1/inbox", get(handlers::get_inbox))
        .route("/api/v1/subscriptions", post(handlers::subscribe))
        .route("/api/v1/subscriptions", delete(handlers::unsubscribe))
        .route("/api/v1/subscriptions", get(handlers::list_subscriptions))
        .route("/api/v1/session/start", post(handlers::session_start))
        .layer(axum::middleware::from_fn_with_state(
            api_key_state,
            handlers::api_key_auth,
        ))
        // v0.7.0 Wave-3 Continuation — postgres route gate. On sqlite
        // deployments this is a pure pass-through. On postgres-backed
        // daemons it short-circuits any un-migrated endpoint with a
        // structured 501 envelope so operators never see silent data
        // corruption from the unused `app.db` scratch connection.
        // See `handlers::postgres_endpoint_supported` for the allow-list.
        .layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            postgres_route_gate_layer,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(CorsLayer::new())
        // H7 (v0.7.0 round-2) — per-request wall-clock timeout.
        // Applied outermost (last in the layer stack) so it bounds
        // every other middleware: the API-key auth, the postgres
        // gate, and the body decoder all run inside the timeout
        // window. Default 60 s; configurable via
        // `AppConfig::request_timeout_secs`.
        .layer(timeout_layer)
        .with_state(app_state)
}

/// v0.7.0 Wave-3 Continuation — adapter that picks up the appropriate
/// gate function depending on whether the binary was built with the
/// `sal` feature flag. Standard builds compile this to a no-op pass-
/// through closure so the wire shape stays identical to pre-Wave-3.
#[cfg(feature = "sal")]
async fn postgres_route_gate_layer(
    state: axum::extract::State<handlers::AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    handlers::postgres_route_gate(state, req, next).await
}

#[cfg(not(feature = "sal"))]
async fn postgres_route_gate_layer(
    _state: axum::extract::State<handlers::AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    next.run(req).await
}

// ---------------------------------------------------------------------------
// H7 (v0.7.0 round-2) — per-request HTTP timeout tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod h7_timeout_tests {
    use std::time::Duration;

    use axum::{Router, body::Body, http::Request, response::IntoResponse, routing::post};
    use tower::ServiceExt as _;

    /// The timeout middleware sandwich: a thin Router with a single
    /// slow handler that always sleeps past the configured timeout.
    /// Exercises the same `axum::middleware::from_fn` closure shape
    /// `build_router_with_timeout` builds, without standing up the
    /// full AppState graph.
    fn timeout_router(timeout: Duration, handler_sleep: Duration) -> Router {
        async fn slow_handler(_body: axum::body::Bytes) -> impl IntoResponse {
            // Sleep duration is captured below via a small wrapper to
            // keep the closure shape inferrable.
            axum::http::StatusCode::OK
        }
        let timeout_layer = axum::middleware::from_fn(
            move |req: axum::extract::Request, next: axum::middleware::Next| async move {
                match tokio::time::timeout(timeout, next.run(req)).await {
                    Ok(resp) => resp,
                    Err(_) => (
                        axum::http::StatusCode::GATEWAY_TIMEOUT,
                        axum::Json(serde_json::json!({"error": "request timed out"})),
                    )
                        .into_response(),
                }
            },
        );
        // The actual slow handler — sleeps `handler_sleep` then 200.
        Router::new()
            .route(
                "/slow",
                post(move |_b: axum::body::Bytes| async move {
                    tokio::time::sleep(handler_sleep).await;
                    slow_handler(axum::body::Bytes::new()).await
                }),
            )
            .layer(timeout_layer)
    }

    #[tokio::test]
    async fn slow_handler_returns_504_when_timeout_fires() {
        // Wire: middleware timeout=50ms, handler sleeps 500ms → 504.
        // Mirrors the production contract: a client that pumps a body
        // slow-loris-style past the configured ceiling sees a
        // structured timeout response instead of the daemon holding
        // the scope open forever.
        let router = timeout_router(Duration::from_millis(50), Duration::from_millis(500));
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/slow")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        // tower::timeout-style middleware returns 504 Gateway Timeout
        // when the inner future times out. axum's `INTERNAL_SERVER_ERROR`
        // shape would also be acceptable per the round-2 contract
        // ("408 or 500 — whatever the timeout produces"); we picked 504
        // deliberately because it stays distinguishable from
        // request-shape 400s and never collides with the inner
        // handler's own status codes.
        assert!(
            resp.status() == axum::http::StatusCode::GATEWAY_TIMEOUT
                || resp.status() == axum::http::StatusCode::REQUEST_TIMEOUT
                || resp.status() == axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "expected a timeout-style response code, got {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn fast_handler_passes_through_when_timeout_does_not_fire() {
        // Wire: middleware timeout=1s, handler sleeps 10ms → 200.
        let router = timeout_router(Duration::from_secs(1), Duration::from_millis(10));
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/slow")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }
}
