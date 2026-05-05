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

pub mod audit;
pub mod autonomy;
pub mod bench;
pub mod cli;
pub mod color;
pub mod config;
pub mod curator;
pub mod daemon_runtime;
pub mod db;
pub mod embeddings;
pub mod errors;
pub mod federation;
pub mod handlers;
pub mod hnsw;
// v0.7 Track G — programmable lifecycle hook pipeline. G1 lands
// the config schema + SIGHUP hot-reload plumbing; the executor
// (G3) and the actual fire points (G7+) layer on top of this
// module without touching call sites in `handlers.rs` etc.
pub mod hooks;
pub mod identity;
pub mod llm;
pub mod log_paths;
pub mod logging;
pub mod mcp;
pub mod metrics;
pub mod mine;
pub mod models;
pub mod profile;
pub mod replication;
pub mod reranker;
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
    use axum::{
        extract::DefaultBodyLimit,
        routing::{delete, get, post, put},
    };
    use tower_http::{cors::CorsLayer, trace::TraceLayer};

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
        .layer(TraceLayer::new_for_http())
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(CorsLayer::new())
        .with_state(app_state)
}
