// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use axum::{
    Json,
    extract::{FromRef, FromRequest, Request, State, rejection::JsonRejection},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::de::DeserializeOwned;
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use crate::config::{ResolvedTtl, TierConfig};
use crate::db;
use crate::embeddings::{Embed, Embedder};
use crate::hnsw::VectorIndex;
use crate::profile::Family;

pub type Db = Arc<Mutex<(rusqlite::Connection, std::path::PathBuf, ResolvedTtl, bool)>>;

/// v0.7.0 Wave-3 — declared storage backend for the daemon.
///
/// Surfaced through the `/capabilities` payload so operators and clients
/// can detect whether the daemon is backed by the bundled SQLite path
/// (the historical default) or by the SAL-routed Postgres adapter.
///
/// The variant resolves once at `serve()` startup from the
/// `--store-url` flag (when set) or the `--db` path (when absent), and
/// is stable across the process lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageBackend {
    /// Bundled SQLite — the production default. Every handler operates
    /// on the `Db` connection directly and the SAL handle in `AppState`
    /// wraps the same connection for parity tests + the v0.7.0 Wave-3
    /// trait-routed code paths.
    Sqlite,
    /// Postgres — selected when `serve --store-url postgres://...` is
    /// passed and the binary was built with `--features sal-postgres`.
    /// Handlers that have been migrated to dispatch through the
    /// [`crate::store::MemoryStore`] trait operate against the
    /// `PostgresStore` adapter; handlers that have not yet migrated
    /// surface `501 Not Implemented` with a clear `storage_backend`
    /// hint so operators can plan the rollout.
    Postgres,
}

impl StorageBackend {
    /// Stable lowercase tag for log lines, the `/capabilities`
    /// `storage_backend` field, and the `ai-memory doctor` report.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
        }
    }
}

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
    /// v0.7.0 A5 — resolved tool [`Profile`] for this daemon. The
    /// HTTP `/capabilities` endpoint needs it to compute the v3
    /// `summary` / `to_describe_to_user` / `tools[].callable_now`
    /// fields, which reflect the profile the running server actually
    /// advertises in `tools/list`. Mirrors the MCP-dispatch threading
    /// at `src/mcp.rs:3760`.
    pub profile: Arc<crate::profile::Profile>,
    /// v0.7.0 A5 — resolved [`McpConfig`] for this daemon. Carries
    /// the optional `[mcp.allowlist]` table that v3's per-tool
    /// `callable_now` and top-level `agent_permitted_families` honor.
    /// `Arc<Option<...>>` rather than `Option<Arc<...>>` so cloning
    /// the AppState stays cheap; absent allowlist (the v0.6.4 default)
    /// shows up as `Arc<None>`.
    pub mcp_config: Arc<Option<crate::config::McpConfig>>,
    /// v0.7 Track H — H2 outbound link signing. The keypair loaded at
    /// daemon startup (or `None` when the operator hasn't generated
    /// one yet). When `Some`, every `db::create_link_signed` call from
    /// HTTP handlers signs the link with this key and stamps
    /// `attest_level = "self_signed"`; when `None`, links go in
    /// unsigned, preserving v0.6.4 behaviour for unmigrated deployments.
    /// H3 will reuse this handle for outbound writes that need to
    /// carry the same signing identity.
    pub active_keypair: Arc<Option<crate::identity::keypair::AgentKeypair>>,
    /// v0.7.0 B3 — pre-computed embeddings for each [`Family`]
    /// descriptor. Filled asynchronously after boot from
    /// [`family_descriptors`] and reused by B2's
    /// `memory_smart_load(intent)` to do a fast cosine match between
    /// an intent string and the eight family descriptors.
    ///
    /// **CI fix (v0.7 B3-fix)**: held behind `RwLock<Option<…>>` and
    /// filled by a detached `tokio::spawn` task launched from
    /// `bootstrap_serve` rather than synchronously on the serve
    /// startup path. The original synchronous precompute would block
    /// HTTP `/health` past the integration suite's 5 s
    /// `wait_for_health` budget on CI runners without a pre-warmed
    /// `hf-hub` model cache. `None` means "not yet populated"; an
    /// empty inner `Vec` means "embedder unavailable, will never be
    /// populated"; either case makes `best_family_match` return
    /// `None` and B2's smart loader degrades to its non-embedding
    /// match path.
    pub family_embeddings: Arc<RwLock<Option<Vec<(Family, Vec<f32>)>>>>,

    // ----- v0.7.0 Wave-3 — adapter selection ------------------------
    /// v0.7.0 Wave-3 — declared storage backend for this daemon.
    ///
    /// Resolved once from `--store-url` (or `--db` fallback) at
    /// `serve()` startup; stable across the process lifetime.
    /// Surfaced through `/api/v1/capabilities.storage_backend` and
    /// consulted by trait-eligible handlers to decide whether to
    /// dispatch through `app.store` or fall back to the legacy
    /// `db::*` free-function code path.
    pub storage_backend: StorageBackend,
    /// v0.7.0 Wave-3 — polymorphic [`MemoryStore`] handle.
    ///
    /// Always populated. For [`StorageBackend::Sqlite`] it wraps a
    /// `SqliteStore` opened against the same on-disk database as the
    /// [`AppState::db`] connection (the two views see the same rows).
    /// For [`StorageBackend::Postgres`] it wraps a `PostgresStore`
    /// connected to the operator-supplied URL.
    ///
    /// Only available under `--features sal`. Standard builds keep
    /// the legacy `db::*` free-function path verbatim.
    ///
    /// [`MemoryStore`]: crate::store::MemoryStore
    #[cfg(feature = "sal")]
    pub store: Arc<dyn crate::store::MemoryStore>,

    // ----- v0.7.0 L5 — LLM client for autonomy hooks ----------------
    /// v0.7.0 L5 — optional LLM client used by the HTTP `create_memory`
    /// handler to fire the `auto_tag` autonomy hook on stores, matching
    /// the behaviour the MCP `handle_store` path has provided since
    /// v0.6.0.0 (`src/mcp.rs:1823-1833`). `None` when the daemon's
    /// configured [`FeatureTier`] does not request an LLM (keyword /
    /// semantic) or when Ollama is unreachable at startup; in either
    /// case the create_memory handler silently skips the hook so the
    /// store still succeeds.
    pub llm: Arc<Option<crate::llm::OllamaClient>>,

    /// v0.7.0 L15 — dedicated model id for `auto_tag` (and other short
    /// structured-output LLM calls). When `Some`, [`maybe_auto_tag`]
    /// passes the value as `OllamaClient::auto_tag(.., Some(model))` so
    /// the call hits a fast tag-friendly model (default config recommends
    /// `gemma3:4b`, ~0.7s p50) instead of the reasoning-tier `llm_model`
    /// (Gemma 4 thinking can take 15s to emit a 5-tag list). When `None`
    /// the call falls back to the client's configured model. Wrapped in
    /// `Arc<Option<...>>` so cloning the AppState stays cheap and the
    /// absent case (the v0.7.0.0 default) is a cheap `Arc<None>`.
    pub auto_tag_model: Arc<Option<String>>,

    /// v0.7.0 H8 (round-2) — per-LLM-call wall-clock timeout. Wraps
    /// every `tokio::task::spawn_blocking` invocation of an Ollama
    /// call (`auto_tag`, `expand_query`, `summarize_memories`, ...)
    /// in `tokio::time::timeout`. On timeout the handler logs at
    /// `warn` and continues on the LLM-absent fallback path
    /// (already exists per L5/L7). Resolved at boot from
    /// `AppConfig::effective_llm_call_timeout_secs` (default 30s).
    pub llm_call_timeout: std::time::Duration,

    /// v0.7.0 H5 (round-2) — bounded in-memory LRU keyed on
    /// `(link_id, signature, verification_nonce)`. Consulted by
    /// [`verify_link_handler`] to reject exact-repeat verify
    /// requests with 409 Conflict. See
    /// [`crate::identity::replay::ReplayCache`] for the memory bound
    /// (~512 KB at the 10 000-entry capacity) + threat model.
    pub replay_cache: Arc<crate::identity::replay::ReplayCache>,

    /// v0.7.0 H5 (round-2) — strict mode for the verify replay
    /// guard. When `true`, every `POST /api/v1/links/verify` request
    /// body MUST include a `verification_nonce` field; missing or
    /// empty nonces produce 400 Bad Request. Default `false` keeps
    /// the v0.6.x verify-anytime semantics and logs a deprecation
    /// WARN on the missing-nonce path instead. Operators opt into
    /// strict mode via `[verify] require_nonce = true` in
    /// `config.toml`.
    pub verify_require_nonce: bool,

    /// v0.7.0 (issue #519) — resolved `autonomous_hooks` flag (from
    /// config.toml + `AI_MEMORY_AUTONOMOUS_HOOKS` env). Consulted by
    /// the HTTP `create_memory` path's [`maybe_detect_conflicts`]
    /// helper as the global default when a request omits the per-call
    /// `detect_conflicts` override. `false` preserves the v0.6.x
    /// post-hoc-only contradiction surface.
    pub autonomous_hooks: bool,

    /// v0.7.0 (issue #518) — resolved
    /// `[agents.defaults.recall_scope]` block. `Some` carries the
    /// session-default namespace / since / tier / limit filters
    /// spliced into recall requests that pass `session_default=true`
    /// and omit one or more filter fields. `None` (the default for
    /// existing single-tenant deployments) preserves v0.6.x recall
    /// semantics — every cross-session recall must spell its filters
    /// out explicitly.
    ///
    /// Wrapped in `Arc<Option<...>>` so cloning the AppState stays
    /// cheap and the absent case (every deployment that hasn't
    /// opted in yet) is a single `Arc<None>`.
    pub recall_scope: Arc<Option<crate::config::RecallScope>>,

    /// v0.7.0 Policy-Engine Item 3 (2026-05-14) — deferred-audit
    /// queue handle. Captures every `governance.refusal` event
    /// from the storage `GOVERNANCE_PRE_WRITE` hook and submits it
    /// to a background drainer task that chain-logs the refusal to
    /// `signed_events` on a FRESH `Connection` (separate from the
    /// substrate writer's connection — closes the re-entrant-deadlock
    /// gap the old `_no_audit` variant traded the chain-log property
    /// for).
    ///
    /// The queue is `Clone` (cheap `Arc` semantics over an mpsc
    /// sender) so each callsite (storage hook closure, future MCP
    /// `governance_state` tool, future Prometheus scrape) can hold
    /// its own producer handle without contention.
    ///
    /// Always present on `bootstrap_serve` — the drainer is spawned
    /// unconditionally before the storage hook installs. The
    /// `Option<...>` shape lets tests inject `None` in scaffolds
    /// that don't need the audit chain.
    pub deferred_audit_queue: Arc<Option<crate::governance::deferred_audit::DeferredAuditQueue>>,
}

/// v0.7.0 B3 — canonical 1-2 sentence English descriptors for each
/// [`Family`]. Used at boot to pre-compute embeddings that B2's
/// `memory_smart_load(intent)` cosine-matches against an intent
/// string. Order tracks [`Family::all()`] (declaration order) so the
/// returned slice is stable across releases. Wording is chosen to
/// reflect the *user-facing* purpose of each family, not its tool
/// names — the embedder needs natural-language signal, not enum
/// labels, for the cosine match to be meaningful.
#[must_use]
pub fn family_descriptors() -> &'static [(Family, &'static str)] {
    &[
        (
            Family::Core,
            "Store, recall, list, get, and search memories. The basic \
             read and write operations for saving facts and looking \
             them up later.",
        ),
        (
            Family::Lifecycle,
            "Update, delete, forget, garbage-collect, and promote \
             memories. Operations that change a memory's state, tier, \
             or visibility over time.",
        ),
        (
            Family::Graph,
            "Knowledge-graph queries, timelines, links between \
             memories, entity registration, taxonomy lookup, and \
             replay or verification of stored relationships.",
        ),
        (
            Family::Governance,
            "Approval workflows, namespace standards, and \
             subscriptions. Operations that gate or shape what other \
             agents are allowed to write or see.",
        ),
        (
            Family::Power,
            "Advanced reasoning helpers: consolidate duplicates, \
             detect contradictions, check for duplicates, auto-tag, \
             expand a query, and inspect the inbox.",
        ),
        (
            Family::Meta,
            "Server capabilities, agent registration and listing, \
             session bootstrap, and aggregate stats. Operations that \
             describe the memory system itself rather than its \
             contents.",
        ),
        (
            Family::Archive,
            "List, restore, purge, and report stats on archived \
             memories. The cold-storage tier where forgotten or aged-out \
             memories live until they are pruned.",
        ),
        (
            Family::Other,
            "Subscription listing and out-of-band notifications. \
             Auxiliary operations that don't fit the other families.",
        ),
    ]
}

impl AppState {
    /// v0.7.0 B3 — pre-compute the family-descriptor embedding cache.
    /// Iterates the eight descriptors from [`family_descriptors`] and
    /// runs each through the embedder once. Returns an empty vector
    /// when the embedder is `None` (keyword-only deployments) or when
    /// any single descriptor fails to embed — the latter is logged at
    /// `warn` and the cache is still returned empty so boot stays
    /// fault-tolerant. The returned vector is intended to be wrapped
    /// in `Arc::new(...)` and stored in [`AppState::family_embeddings`].
    #[must_use]
    pub fn precompute_family_embeddings(embedder: Option<&dyn Embed>) -> Vec<(Family, Vec<f32>)> {
        let Some(embedder) = embedder else {
            return Vec::new();
        };
        let descriptors = family_descriptors();
        let mut out: Vec<(Family, Vec<f32>)> = Vec::with_capacity(descriptors.len());
        for (family, descriptor) in descriptors {
            match embedder.embed(descriptor) {
                Ok(v) => out.push((*family, v)),
                Err(e) => {
                    tracing::warn!(
                        family = family.name(),
                        error = %e,
                        "B3: failed to embed family descriptor; \
                         family_embeddings will be empty",
                    );
                    return Vec::new();
                }
            }
        }
        out
    }

    /// v0.7.0 B3 — embed `intent` and return the family-descriptor
    /// with the highest cosine similarity, paired with its score.
    /// Returns `None` if the cache is not yet populated (the
    /// asynchronous precompute task has not finished, or the
    /// embedder is unavailable so the cache will never populate) or
    /// if the embedder is unavailable now. This is the entry point
    /// B2's `memory_smart_load(intent)` uses to pick which family to
    /// load.
    ///
    /// Uses `try_read()` so a slow concurrent writer (the boot-time
    /// precompute task still finalising its write) cannot block the
    /// caller — on contention we degrade to `None` and the smart
    /// loader's non-embedding fallback path takes over.
    #[must_use]
    pub fn best_family_match(&self, intent: &str) -> Option<(Family, f32)> {
        let guard = self.family_embeddings.try_read().ok()?;
        let cache = guard.as_ref()?;
        if cache.is_empty() {
            return None;
        }
        let embedder = self.embedder.as_ref().as_ref()?;
        let intent_vec = embedder.embed(intent).ok()?;
        let mut best: Option<(Family, f32)> = None;
        for (family, descriptor_vec) in cache.iter() {
            let score = Embedder::cosine_similarity(&intent_vec, descriptor_vec);
            match best {
                Some((_, prev)) if prev >= score => {}
                _ => best = Some((*family, score)),
            }
        }
        best
    }
}

impl FromRef<AppState> for Db {
    fn from_ref(app: &AppState) -> Self {
        app.db.clone()
    }
}

/// v0.7.0 Wave-3 — uniform 501 NOT IMPLEMENTED response for handlers
/// that have not yet migrated to the [`crate::store::MemoryStore`]
/// trait dispatch path on Postgres-backed daemons.
///
/// Returns a stable, machine-parseable JSON envelope so operator
/// scripts can recognise the v0.7.0 Wave-3 schism without parsing
/// free-form strings:
///
/// ```json
/// {
///   "error": "endpoint not yet implemented for postgres-backed daemon",
///   "endpoint": "<route>",
///   "storage_backend": "postgres",
///   "remediation": "use sqlite-backed daemon or wait for v0.7.x trait coverage"
/// }
/// ```
///
/// Wired into the un-migrated handlers below so a postgres-backed
/// daemon never silently falls back to the empty in-memory SQLite
/// scratch DB and corrupts the operator's mental model of where
/// their data lives. As handlers migrate to the trait this call
/// site count goes to zero.
#[cfg(feature = "sal")]
#[must_use]
pub fn postgres_not_implemented(endpoint: &'static str) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "endpoint not yet implemented for postgres-backed daemon",
            "endpoint": endpoint,
            "storage_backend": "postgres",
            "remediation": "use sqlite-backed daemon or wait for v0.7.x trait coverage",
        })),
    )
        .into_response()
}

/// v0.7.0 Wave-3 Continuation — postgres-supported endpoint allow-list.
///
/// Returns `true` if the given (method, path) tuple has a handler that
/// has been migrated to dispatch through the [`crate::store::MemoryStore`]
/// trait when the daemon is postgres-backed. Anything not in this list
/// is shielded by [`postgres_route_gate`] middleware which surfaces
/// 501 NOT IMPLEMENTED rather than letting the un-migrated handler
/// silently fall through to the empty in-memory scratch SQLite database
/// that `bootstrap_serve` opens for the postgres-backed `app.db` field.
///
/// The matching is path-pattern aware:
/// - exact equality for fixed paths (e.g. `/api/v1/memories`)
/// - prefix match for sub-resources (e.g. `/api/v1/memories/{id}`)
///
/// As handlers migrate they get added here. Pre-existing CRUD entries
/// match what Wave-3 phase 3 already wired through `app.store`.
#[cfg(feature = "sal")]
#[must_use]
pub fn postgres_endpoint_supported(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;

    // Health and metadata always pass through — they don't touch user data.
    if path == "/api/v1/health"
        || path == "/api/v1/capabilities"
        || path == "/metrics"
        || path == "/api/v1/metrics"
    {
        return true;
    }

    // Approval SSE stream — read-only metadata stream, not user-data.
    if path == "/api/v1/approvals/stream" && method == Method::GET {
        return true;
    }

    match (method.as_str(), path) {
        // Wave-3 phase 3 — core CRUD (commit c049500).
        ("POST", "/api/v1/memories") | ("GET", "/api/v1/memories") => true,
        ("GET" | "PUT" | "DELETE", p) if memory_id_path(p) => true,
        ("GET", "/api/v1/search") => true,
        ("POST", "/api/v1/links") => true,
        ("GET", p) if links_id_path(p) => true,
        // Wave-3 continuation — list_pending (read-only).
        ("GET", "/api/v1/pending") => true,
        // Wave-3 continuation — list_agents (read-only).
        ("GET", "/api/v1/agents") => true,
        // Wave-3 continuation — list_namespaces (read-only).
        ("GET", "/api/v1/namespaces") => {
            // GET /api/v1/namespaces with no query string lists namespaces.
            // The same path with ?namespace=... fetches a standard which is
            // also gated through SAL via get_namespace_standard_qs.
            true
        }
        // Wave-3 continuation — KG endpoints (postgres adapter has impls).
        ("POST", "/api/v1/kg/query")
        | ("GET", "/api/v1/kg/timeline")
        | ("POST", "/api/v1/kg/invalidate") => true,
        // Continuation 6 — three new HTTP endpoints (S52, S61, S65).
        ("POST", "/api/v1/kg/find_paths")
        | ("POST", "/api/v1/links/verify")
        | ("POST", "/api/v1/quota/status") => true,
        // Wave-3 continuation — entity registry.
        ("POST", "/api/v1/entities") | ("GET", "/api/v1/entities/by_alias") => true,
        // Wave-3 continuation — stats (basic count).
        ("GET", "/api/v1/stats") => true,
        // Wave-3 continuation — bulk write.
        ("POST", "/api/v1/memories/bulk") => true,
        // Wave-3 continuation — recall fallback (keyword via search).
        ("GET" | "POST", "/api/v1/recall") => true,
        // Wave-3 continuation — archive list/stats (read-only).
        ("GET", "/api/v1/archive") => true,
        ("GET", "/api/v1/archive/stats") => true,
        // Wave-3 continuation — taxonomy and check_duplicate.
        ("GET", "/api/v1/taxonomy") => true,
        ("POST", "/api/v1/check_duplicate") => true,
        // Wave-3 continuation — list_subscriptions, inbox.
        ("GET", "/api/v1/subscriptions") => true,
        ("GET", "/api/v1/inbox") => true,
        // Wave-3 Continuation 2 — federation push/pull (Phase 8).
        ("POST", "/api/v1/sync/push") => true,
        ("GET", "/api/v1/sync/since") => true,
        // Wave-3 Continuation 2 — governance write paths (Phase 11).
        ("POST", p) if pending_decide_path(p) => true,
        ("POST", p) if namespace_standard_post_path(p) => true,
        ("DELETE", p) if namespace_standard_delete_path(p) => true,
        ("POST", "/api/v1/namespaces") => true,
        ("DELETE", "/api/v1/namespaces") => true,
        // Wave-3 Continuation 3 — lifecycle write paths (Phase 13/14/16/17/18/19).
        ("POST", "/api/v1/forget") => true,
        ("POST", "/api/v1/consolidate") => true,
        ("GET", "/api/v1/contradictions") => true,
        // v0.7.0 L6 — S51 autonomous-tier endpoints. Both are
        // LLM-only (no DB access for the request body itself) so the
        // postgres gate just needs to pass them through to the
        // handler, which handles the 503 fallback when no LLM is
        // wired.
        ("POST", "/api/v1/auto_tag") => true,
        ("POST", "/api/v1/expand_query") => true,
        // v0.7.0 L9 / L10 — HTTP parity for `tools/list` and
        // `memory_load_family`. `tools/list` is pure config
        // enumeration (no DB); `memory_load_family` reads through the
        // SAL trait on the postgres path.
        ("GET", "/api/v1/tools/list") => true,
        ("POST", "/api/v1/memory_load_family") => true,
        ("POST", "/api/v1/notify") => true,
        ("POST", "/api/v1/gc") => true,
        ("POST", "/api/v1/import") => true,
        ("GET", "/api/v1/export") => true,
        ("POST", "/api/v1/archive") => true,
        ("DELETE", "/api/v1/archive") => true,
        ("POST", "/api/v1/archive/purge") => true,
        ("POST", p) if archive_restore_path(p) => true,
        // Wave-3 Continuation 3 — remaining write paths the sqlite path
        // already wires through `app.store` in their handlers (these
        // were soft-routed by the legacy db:: free-functions before
        // Continuation 3, so the gate now allow-lists them so the gate
        // doesn't 501 a working sqlite-routed handler on a postgres
        // daemon. Each handler internally enforces postgres-vs-sqlite
        // dispatch, so the gate's job is just to permit the request to
        // reach the handler).
        ("POST", "/api/v1/agents") => true,
        ("DELETE", "/api/v1/links") => true,
        ("POST", "/api/v1/subscriptions") | ("DELETE", "/api/v1/subscriptions") => true,
        ("POST", "/api/v1/session/start") => true,
        ("POST", p) if memory_promote_path(p) => true,
        ("POST", p) if approvals_decide_path(p) => true,
        _ => false,
    }
}

/// Path matcher for `/api/v1/memories/{id}/promote`.
#[cfg(feature = "sal")]
fn memory_promote_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/v1/memories/") else {
        return false;
    };
    rest.ends_with("/promote") && rest.split('/').count() == 2
}

/// Path matcher for `POST /api/v1/approvals/{pending_id}` (HMAC-gated).
#[cfg(feature = "sal")]
fn approvals_decide_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/v1/approvals/") else {
        return false;
    };
    !rest.is_empty() && rest != "stream" && !rest.contains('/')
}

/// Path matcher for `/api/v1/archive/{id}/restore`.
#[cfg(feature = "sal")]
fn archive_restore_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/v1/archive/") else {
        return false;
    };
    rest.ends_with("/restore") && rest.split('/').count() == 2
}

/// Path matcher for `/api/v1/pending/{id}/approve|reject`.
#[cfg(feature = "sal")]
fn pending_decide_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/v1/pending/") else {
        return false;
    };
    matches!(rest.split_once('/'), Some((_, "approve" | "reject")))
}

/// Path matcher for `POST /api/v1/namespaces/{ns}/standard`.
#[cfg(feature = "sal")]
fn namespace_standard_post_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/v1/namespaces/") else {
        return false;
    };
    rest.ends_with("/standard") && rest.split('/').count() == 2
}

/// Path matcher for `DELETE /api/v1/namespaces/{ns}/standard`.
#[cfg(feature = "sal")]
fn namespace_standard_delete_path(p: &str) -> bool {
    namespace_standard_post_path(p)
}

/// Path matcher for `/api/v1/memories/{id}` (no further sub-segment).
#[cfg(feature = "sal")]
fn memory_id_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/v1/memories/") else {
        return false;
    };
    // Reject the bulk path and any further sub-segments.
    if rest == "bulk" {
        return false;
    }
    !rest.contains('/')
}

/// Path matcher for `/api/v1/links/{id}`.
#[cfg(feature = "sal")]
fn links_id_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/v1/links/") else {
        return false;
    };
    !rest.is_empty() && !rest.contains('/')
}

/// v0.7.0 Wave-3 Continuation — middleware that gates un-migrated
/// handlers when the daemon is postgres-backed.
///
/// Sits in the request pipeline after `api_key_auth` so authn still
/// applies, then short-circuits any (method, path) tuple not in
/// [`postgres_endpoint_supported`] with a structured 501 response.
///
/// On sqlite-backed daemons this is a pure pass-through — every path
/// is supported because the legacy `db::*` free-function code path is
/// the active path and `app.db` is the real on-disk database.
///
/// This is the load-bearing correctness fix for postgres-backed
/// daemons: without it, any un-migrated handler would silently use
/// the empty in-memory scratch SQLite database that `bootstrap_serve`
/// opens against the `--db` path (which is unused on postgres) and
/// either return empty results (read paths) or write to the wrong
/// database (write paths). The gate makes that impossible.
#[cfg(feature = "sal")]
pub async fn postgres_route_gate(
    State(app): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if !matches!(app.storage_backend, StorageBackend::Postgres) {
        return next.run(req).await;
    }

    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if postgres_endpoint_supported(&method, &path) {
        return next.run(req).await;
    }

    tracing::debug!(
        method = %method,
        path = %path,
        "postgres-backed daemon: 501 for un-migrated endpoint"
    );

    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "endpoint not yet implemented for postgres-backed daemon",
            "endpoint": path,
            "method": method.as_str(),
            "storage_backend": "postgres",
            "remediation": "use sqlite-backed daemon or wait for v0.7.x trait coverage; \
                            see docs/postgres-age-guide.md for the supported endpoint inventory",
        })),
    )
        .into_response()
}

/// v0.7.0 Wave-3 — translate a [`crate::store::StoreError`] into the
/// daemon's standard HTTP error envelope. Centralised so every
/// trait-routed handler reports backend errors with the same shape.
///
/// v0.7.0 M12 — every variant whose `to_string()` may carry adapter-
/// originating payload (connection strings, file paths, raw sqlx
/// diagnostics) is routed through [`sanitize_store_err_message`]
/// before landing in the HTTP envelope. The raw error is still
/// captured to the structured tracing log for operators; the wire
/// surface only carries the scrubbed message so an authenticated
/// client cannot exfiltrate the postgres URL by triggering a typed
/// error path.
#[cfg(feature = "sal")]
#[must_use]
pub fn store_err_to_response(e: crate::store::StoreError) -> Response {
    use crate::store::StoreError;
    let (status, msg) = match &e {
        StoreError::NotFound { .. } => (StatusCode::NOT_FOUND, "not found".to_string()),
        StoreError::Conflict { .. } => (
            StatusCode::CONFLICT,
            sanitize_store_err_message(&e.to_string()),
        ),
        StoreError::PermissionDenied { .. } => (
            StatusCode::FORBIDDEN,
            sanitize_store_err_message(&e.to_string()),
        ),
        StoreError::InvalidInput { .. } => (
            StatusCode::BAD_REQUEST,
            sanitize_store_err_message(&e.to_string()),
        ),
        StoreError::UnsupportedCapability { capability } => (
            StatusCode::NOT_IMPLEMENTED,
            format!("backend does not support capability: {capability}"),
        ),
        StoreError::IntegrityFailed { .. } | StoreError::BackendUnavailable { .. } => {
            tracing::error!("store backend error: {e}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "storage backend unavailable".to_string(),
            )
        }
        _ => {
            tracing::error!("store backend error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal server error".to_string(),
            )
        }
    };
    (status, Json(json!({"error": msg}))).into_response()
}

/// v0.7.0 M12 — scrub adapter-originating payload from a
/// [`crate::store::StoreError`]'s display string before it lands in an
/// HTTP response. The redaction targets three families of leakage the
/// M12 audit found in real sqlx + filesystem error paths:
///
/// 1. **Connection-string-like fragments** — anything matching the
///    `scheme://user:pass@host[:port]/db` shape. The entire run from
///    the scheme through the next whitespace / quote / brace boundary
///    is replaced with `[redacted-url]` so an authenticated caller
///    cannot read the postgres URL out of a wrapped
///    `sqlx::Error::Configuration("invalid url postgres://…")` (or any
///    other variant whose Display interpolates the connection target).
/// 2. **Absolute filesystem paths** — anything starting with `/` and
///    running through a typical path charset gets replaced with
///    `[redacted-path]`. Closes the
///    `sqlx::Error::Io("/var/lib/postgresql/…")` family.
///
/// The function is deliberately textual (byte scan) rather than
/// variant-aware: the cost of a missed leak (PII / credential
/// exposure) far outweighs the cost of over-sanitization (a slightly
/// less specific error message). Operators who need the raw
/// diagnostic still get it via the structured tracing log emitted at
/// the call site.
#[cfg(feature = "sal")]
#[must_use]
pub fn sanitize_store_err_message(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        // Look for "://" — strong signal of a URL.
        if i + 2 < bytes.len() && &bytes[i..i + 3] == b"://" {
            // Walk backward through any scheme characters we already
            // emitted, then pop them from `out` and replace the whole
            // run with the sentinel.
            let mut scheme_start = i;
            while scheme_start > 0 {
                let c = bytes[scheme_start - 1];
                if c.is_ascii_alphanumeric() || c == b'+' || c == b'-' || c == b'.' {
                    scheme_start -= 1;
                } else {
                    break;
                }
            }
            let pop = i - scheme_start;
            out.truncate(out.len().saturating_sub(pop));
            out.push_str("[redacted-url]");
            // Skip past "://" plus the rest of the URL run (anything
            // not whitespace/quote/brace/paren/comma/semicolon/angle).
            i += 3;
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_whitespace()
                    || c == b'"'
                    || c == b'\''
                    || c == b'`'
                    || c == b'{'
                    || c == b'}'
                    || c == b'('
                    || c == b')'
                    || c == b','
                    || c == b';'
                    || c == b'<'
                    || c == b'>'
                {
                    break;
                }
                i += 1;
            }
            continue;
        }

        // Absolute paths — require a separator/boundary before the '/'
        // so we don't gut "1/2" inside an unrelated diagnostic.
        if bytes[i] == b'/'
            && (i == 0
                || matches!(
                    bytes[i - 1],
                    b' ' | b'\t' | b'\n' | b'"' | b'\'' | b'(' | b'[' | b'=' | b':'
                ))
            && i + 1 < bytes.len()
            && (bytes[i + 1].is_ascii_alphanumeric()
                || bytes[i + 1] == b'_'
                || bytes[i + 1] == b'.')
        {
            out.push_str("[redacted-path]");
            i += 1;
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_alphanumeric() || c == b'/' || c == b'.' || c == b'_' || c == b'-' {
                    i += 1;
                } else {
                    break;
                }
            }
            continue;
        }

        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(all(test, feature = "sal"))]
mod store_err_sanitize_tests {
    use super::sanitize_store_err_message;

    #[test]
    fn sanitize_redacts_postgres_url() {
        let leak = "connection failed for postgres://admin:hunter2@db.internal:5432/ai_memory";
        let clean = sanitize_store_err_message(leak);
        assert!(!clean.contains("postgres://"), "raw scheme leaked: {clean}");
        assert!(!clean.contains("hunter2"), "password leaked: {clean}");
        assert!(!clean.contains("db.internal"), "host leaked: {clean}");
        assert!(
            clean.contains("[redacted-url]"),
            "missing sentinel: {clean}"
        );
    }

    #[test]
    fn sanitize_redacts_filesystem_path() {
        let leak = "open /var/lib/postgresql/data/global/pg_control failed";
        let clean = sanitize_store_err_message(leak);
        assert!(!clean.contains("/var/lib"), "raw path leaked: {clean}");
        assert!(
            clean.contains("[redacted-path]"),
            "missing sentinel: {clean}"
        );
    }

    #[test]
    fn sanitize_passes_through_clean_diagnostics() {
        let clean_input = "memory not found: abc-123";
        let out = sanitize_store_err_message(clean_input);
        assert_eq!(out, clean_input);
    }

    #[test]
    fn sanitize_handles_multiple_leaks() {
        let leak = "sqlx error at postgres://u:p@h/db touching /etc/secret/key";
        let clean = sanitize_store_err_message(leak);
        assert!(!clean.contains("postgres://"));
        assert!(!clean.contains("/etc/secret"));
        assert!(clean.contains("[redacted-url]"));
        assert!(clean.contains("[redacted-path]"));
    }

    #[test]
    fn sanitize_preserves_relative_paths() {
        // A literal "/" surrounded by digits ("1/2") must NOT be
        // treated as a path. Regression test for the boundary check.
        let raw = "ratio 1/2 over 3/4";
        let out = sanitize_store_err_message(raw);
        assert_eq!(out, raw, "fraction-like content must not be redacted");
    }

    #[test]
    fn sanitize_handles_unicode_in_clean_message() {
        let raw = "memory not found: \u{1F4DD}-id-with-emoji";
        let out = sanitize_store_err_message(raw);
        assert!(out.contains("memory not found"));
    }

    #[test]
    fn sanitize_redacts_url_at_start_of_message() {
        let leak = "postgres://u:p@h/db is unreachable";
        let clean = sanitize_store_err_message(leak);
        assert!(clean.starts_with("[redacted-url]"));
    }
}

// ---------------------------------------------------------------------------
// L0.7-6 Tier E coverage — exercise the helper surface that does not
// require a real Axum runtime: percent decoder, constant-time compare,
// store-error wire-shape mapper, postgres endpoint matrix, AppState
// helpers. The router-bound paths (api_key_auth full pipeline,
// JsonOrBadRequest extractor, postgres_route_gate live middleware)
// remain integration-only per coverage/policy.md.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod transport_helpers_tests {
    use super::*;

    #[test]
    fn percent_decode_handles_typical_keys() {
        assert_eq!(percent_decode_lossy("abc"), "abc");
        assert_eq!(percent_decode_lossy("a%2Bb"), "a+b");
        assert_eq!(percent_decode_lossy("hello%20world"), "hello world");
        assert_eq!(percent_decode_lossy("%2F%3D%3F"), "/=?");
    }

    #[test]
    fn percent_decode_passes_through_invalid_escapes() {
        // Invalid hex digits => pass through verbatim.
        assert_eq!(percent_decode_lossy("a%ZZb"), "a%ZZb");
        // Truncated escape at end => verbatim.
        assert_eq!(percent_decode_lossy("a%2"), "a%2");
    }

    #[test]
    fn constant_time_eq_handles_equal_and_diff_inputs() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn storage_backend_as_str_round_trip() {
        assert_eq!(StorageBackend::Sqlite.as_str(), "sqlite");
        assert_eq!(StorageBackend::Postgres.as_str(), "postgres");
    }

    #[test]
    fn family_descriptors_returns_eight_entries() {
        // Order must match Family::all() declaration order — see the
        // upstream `family_descriptors` doc comment.
        let d = family_descriptors();
        assert_eq!(d.len(), 8, "expected 8 family descriptors, got {}", d.len());
        // Every descriptor is a non-empty English sentence.
        for (family, text) in d {
            assert!(!text.is_empty(), "descriptor for {family:?} is empty");
            assert!(
                text.len() > 20,
                "descriptor for {family:?} too short: {text}"
            );
        }
    }

    #[test]
    fn precompute_family_embeddings_no_embedder_returns_empty() {
        // The fast path of `precompute_family_embeddings`: when the
        // embedder is `None` (keyword tier or load failure) the
        // function returns an empty vector and never touches the
        // descriptor list. Pin the contract here so a future refactor
        // that swaps the early return for a panic catches the test.
        let out = AppState::precompute_family_embeddings(None);
        assert!(out.is_empty());
    }

    #[test]
    fn extract_missing_fields_finds_single_field() {
        let msg =
            "Failed to deserialize the JSON body: missing field `content` at line 1 column 14";
        let fields = extract_missing_fields(msg);
        assert_eq!(fields, vec!["content".to_string()]);
    }

    #[test]
    fn extract_missing_fields_finds_multiple_fields() {
        let msg = "missing field `title` and missing field `content`";
        let fields = extract_missing_fields(msg);
        assert_eq!(fields, vec!["title".to_string(), "content".to_string()]);
    }

    #[test]
    fn extract_missing_fields_dedups_repeats() {
        let msg = "missing field `name` ... missing field `name` again";
        let fields = extract_missing_fields(msg);
        assert_eq!(fields, vec!["name".to_string()]);
    }

    #[test]
    fn extract_missing_fields_returns_empty_for_clean_message() {
        assert!(extract_missing_fields("no missing fields here").is_empty());
    }

    #[test]
    fn extract_missing_fields_rejects_non_identifier_content() {
        // The function light-validates so a hostile body cannot smuggle
        // arbitrary content into the response envelope.
        let msg = "missing field `<script>` injection attempt";
        let fields = extract_missing_fields(msg);
        // The `<script>` payload contains `<` and `>` which are not
        // ascii_alphanumeric / _ / - so the field is dropped.
        assert!(fields.is_empty(), "non-ident content must be rejected");
    }

    #[test]
    fn extract_missing_fields_accepts_underscores_and_dashes() {
        let msg = "missing field `agent_id-x` here";
        let fields = extract_missing_fields(msg);
        assert_eq!(fields, vec!["agent_id-x".to_string()]);
    }

    #[test]
    fn extract_missing_fields_handles_unterminated_backtick() {
        // No trailing backtick → break the loop without panicking.
        let msg = "missing field `unterminated";
        let fields = extract_missing_fields(msg);
        assert!(fields.is_empty());
    }
}

#[cfg(all(test, feature = "sal"))]
mod transport_postgres_gate_tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn postgres_gate_always_passes_health_and_metrics() {
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/health"));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/capabilities"
        ));
        assert!(postgres_endpoint_supported(&Method::GET, "/metrics"));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/metrics"));
    }

    #[test]
    fn postgres_gate_passes_core_crud() {
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/memories"
        ));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/memories"
        ));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/search"));
        assert!(postgres_endpoint_supported(&Method::POST, "/api/v1/links"));
    }

    #[test]
    fn postgres_gate_passes_memory_id_paths() {
        // GET / PUT / DELETE on /api/v1/memories/{id} are supported.
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/memories/abc-123"
        ));
        assert!(postgres_endpoint_supported(
            &Method::PUT,
            "/api/v1/memories/abc-123"
        ));
        assert!(postgres_endpoint_supported(
            &Method::DELETE,
            "/api/v1/memories/abc-123"
        ));
        // POST on a single id is not in the matrix.
        assert!(!postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/memories/abc-123"
        ));
        // /api/v1/memories/bulk is its own endpoint (not memory_id_path).
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/memories/bulk"
        ));
    }

    #[test]
    fn postgres_gate_passes_links_id_paths() {
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/links/link-id-1"
        ));
        // Empty trailing segment must not match.
        assert!(!postgres_endpoint_supported(&Method::GET, "/api/v1/links/"));
    }

    #[test]
    fn postgres_gate_passes_kg_paths() {
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/kg/query"
        ));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/kg/timeline"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/kg/invalidate"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/kg/find_paths"
        ));
    }

    #[test]
    fn postgres_gate_passes_quota_verify_entities() {
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/links/verify"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/quota/status"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/entities"
        ));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/entities/by_alias"
        ));
    }

    #[test]
    fn postgres_gate_passes_archive_paths() {
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/archive"));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/archive/stats"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/archive"
        ));
        assert!(postgres_endpoint_supported(
            &Method::DELETE,
            "/api/v1/archive"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/archive/purge"
        ));
        // archive_restore_path: /api/v1/archive/{id}/restore
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/archive/abc/restore"
        ));
        assert!(!postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/archive/abc/restore/other"
        ));
    }

    #[test]
    fn postgres_gate_passes_namespace_standard_paths() {
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/namespaces/proj/standard"
        ));
        assert!(postgres_endpoint_supported(
            &Method::DELETE,
            "/api/v1/namespaces/proj/standard"
        ));
        assert!(!postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/namespaces/standard"
        ));
    }

    #[test]
    fn postgres_gate_passes_pending_decide_paths() {
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/pending/p1/approve"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/pending/p1/reject"
        ));
        // Non-approve-reject suffix must not match.
        assert!(!postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/pending/p1/foo"
        ));
    }

    #[test]
    fn postgres_gate_passes_approvals_decide_paths() {
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/approvals/abc-123"
        ));
        // /api/v1/approvals/stream is excluded from decide path.
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/approvals/stream"
        ));
    }

    #[test]
    fn postgres_gate_passes_memory_promote_path() {
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/memories/abc/promote"
        ));
        assert!(!postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/memories/abc/promote/extra"
        ));
    }

    #[test]
    fn postgres_gate_passes_remaining_write_paths() {
        assert!(postgres_endpoint_supported(&Method::POST, "/api/v1/forget"));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/consolidate"
        ));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/contradictions"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/auto_tag"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/expand_query"
        ));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/tools/list"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/memory_load_family"
        ));
        assert!(postgres_endpoint_supported(&Method::POST, "/api/v1/notify"));
        assert!(postgres_endpoint_supported(&Method::POST, "/api/v1/gc"));
        assert!(postgres_endpoint_supported(&Method::POST, "/api/v1/import"));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/export"));
        assert!(postgres_endpoint_supported(&Method::POST, "/api/v1/agents"));
        assert!(postgres_endpoint_supported(
            &Method::DELETE,
            "/api/v1/links"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/subscriptions"
        ));
        assert!(postgres_endpoint_supported(
            &Method::DELETE,
            "/api/v1/subscriptions"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/session/start"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/sync/push"
        ));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/sync/since"
        ));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/pending"));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/agents"));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/namespaces"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/namespaces"
        ));
        assert!(postgres_endpoint_supported(
            &Method::DELETE,
            "/api/v1/namespaces"
        ));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/stats"));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/taxonomy"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/check_duplicate"
        ));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/subscriptions"
        ));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/inbox"));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/recall"));
        assert!(postgres_endpoint_supported(&Method::POST, "/api/v1/recall"));
    }

    #[test]
    fn postgres_gate_rejects_unknown_paths() {
        // Anything not in the allow-list must return false so the
        // route gate surfaces 501 instead of silently routing to the
        // empty scratch SQLite DB.
        assert!(!postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/this/is/not/a/real/endpoint"
        ));
        assert!(!postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/unknown"
        ));
    }

    #[test]
    fn postgres_not_implemented_carries_endpoint_and_remediation() {
        let resp = postgres_not_implemented("/api/v1/test");
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_IMPLEMENTED);
    }

    #[test]
    fn store_err_to_response_maps_every_variant_to_status() {
        use crate::store::StoreError;
        let r = store_err_to_response(StoreError::NotFound {
            id: "x".to_string(),
        });
        assert_eq!(r.status(), axum::http::StatusCode::NOT_FOUND);

        let r = store_err_to_response(StoreError::Conflict {
            id: "x".to_string(),
        });
        assert_eq!(r.status(), axum::http::StatusCode::CONFLICT);

        let r = store_err_to_response(StoreError::PermissionDenied {
            action: "r".to_string(),
            target: "t".to_string(),
            reason: "x".to_string(),
        });
        assert_eq!(r.status(), axum::http::StatusCode::FORBIDDEN);

        let r = store_err_to_response(StoreError::InvalidInput {
            detail: "bad".to_string(),
        });
        assert_eq!(r.status(), axum::http::StatusCode::BAD_REQUEST);

        let r = store_err_to_response(StoreError::UnsupportedCapability {
            capability: "X".to_string(),
        });
        assert_eq!(r.status(), axum::http::StatusCode::NOT_IMPLEMENTED);

        let r = store_err_to_response(StoreError::IntegrityFailed {
            detail: "d".to_string(),
        });
        assert_eq!(r.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);

        let r = store_err_to_response(StoreError::BackendUnavailable {
            backend: "p".to_string(),
            detail: "d".to_string(),
        });
        assert_eq!(r.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);

        let r = store_err_to_response(StoreError::Backend(crate::store::BoxBackendError::new(
            "raw",
        )));
        assert_eq!(r.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }
}

pub(crate) const MAX_BULK_SIZE: usize = 1000;

// ---------------------------------------------------------------------------
// v0.7.0 Round-2 F9 — JSON body extractor that returns 400 (not axum's
// default 422) for missing/malformed fields, with a sanitized response
// envelope `{ "error": "...", "fields": ["..."] }` so callers can switch
// on the field name without parsing a free-form serde message.
// ---------------------------------------------------------------------------

/// Wrapping extractor that delegates to `axum::Json<T>` but rewrites
/// every rejection to `400 Bad Request` with a structured body shaped
/// like the rest of the daemon's error envelopes
/// (`{"error": ..., "fields": [...]}`).
///
/// Applied to the HTTP store path so a body missing `content` (or any
/// other required field) returns 400 + a field-name hint instead of
/// axum's default 422 Unprocessable Entity. The 422 default leaks the
/// raw serde error string ("Failed to deserialize the JSON body...
/// missing field `content` at line 1 column 14"), which forces clients
/// into substring matching on a non-stable diagnostic message; the
/// `fields` array is the structured replacement.
pub struct JsonOrBadRequest<T>(pub T);

impl<S, T> FromRequest<S> for JsonOrBadRequest<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rej) => Err(json_rejection_to_400(&rej)),
        }
    }
}

/// Convert an axum `JsonRejection` into a `400 Bad Request` response
/// with the daemon's standard `{"error": ..., "fields": [...]}` shape.
/// The `fields` array best-effort-extracts missing field names from
/// the underlying serde error message; on parse failure it is left
/// empty so callers can still rely on the envelope shape.
fn json_rejection_to_400(rej: &JsonRejection) -> Response {
    let raw_msg = rej.body_text();
    // serde_json's "missing field" diagnostic: `missing field \`<name>\``.
    // We extract the backtick-quoted identifier and surface it both as
    // a sanitized human message and as the structured `fields` array.
    let fields = extract_missing_fields(&raw_msg);
    let error_msg = if let Some(first) = fields.first() {
        format!("missing required field: {first}")
    } else {
        // Generic malformed-body fallback (syntax error, type error,
        // etc.). Sanitized to avoid leaking the raw serde diagnostic
        // (which can include positional info from the request body).
        match rej {
            JsonRejection::JsonSyntaxError(_) => "malformed JSON body".to_string(),
            JsonRejection::MissingJsonContentType(_) => {
                "expected Content-Type: application/json".to_string()
            }
            _ => "invalid request body".to_string(),
        }
    };
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": error_msg,
            "fields": fields,
        })),
    )
        .into_response()
}

/// Best-effort scan of a serde-error message for `missing field
/// \`<name>\`` occurrences. Returns the de-duplicated list of field
/// names in order of appearance. When no match is found (e.g. a type
/// error or syntax error) the returned vector is empty so the caller
/// falls back to the generic "invalid request body" message.
fn extract_missing_fields(msg: &str) -> Vec<String> {
    let needle = "missing field `";
    let mut out: Vec<String> = Vec::new();
    let mut rest = msg;
    while let Some(idx) = rest.find(needle) {
        let after = &rest[idx + needle.len()..];
        if let Some(end) = after.find('`') {
            let name = &after[..end];
            // Light validation — reject anything that doesn't look like
            // a serde field identifier so a hostile body cannot smuggle
            // arbitrary content into the response envelope.
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                && !out.iter().any(|existing| existing == name)
            {
                out.push(name.to_string());
            }
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// v0.7.0 Round-2 F10 — embed-status surface for the HTTP store path.
//
// When the embedder times out / refuses oversized content / otherwise
// fails to produce a vector, the row still commits (correct — embeddings
// are an enhancement layer, not a write-path gate) but the HTTP response
// must surface that fact so the caller can tell semantic recall will
// silently miss this memory until a re-index. Prior to F10 the daemon
// returned 201 with no signal whatsoever.
//
// The canonical [`crate::embeddings::EmbedStatus`] enum + the
// [`crate::embeddings::Embedder::embed_with_status`] producer were
// landed by Fix-Agent α (Round-2 F6); the HTTP wiring below is the
// F10 consumer side that turns the producer's signal into a response
// field on non-`Indexed` outcomes.
// ---------------------------------------------------------------------------

/// v0.6.2 (S40): maximum number of per-row `broadcast_store_quorum` fanouts
/// in flight at once during `bulk_create`. Replaces the prior sequential
/// for-loop (which paid 100ms × N rows of wall time and blew past the
/// testbook's 20s settle on N=500) with bounded concurrency. The bound
/// balances speedup against peer-side `SQLite` Mutex contention and the
/// leader-side reqwest connection-pool / ephemeral-port envelope. See the
/// comment above the loop in `bulk_create` for the full rationale.
pub(crate) const BULK_FANOUT_CONCURRENCY: usize = 8;

/// Shared state for API key authentication middleware.
#[derive(Clone)]
pub struct ApiKeyState {
    pub key: Option<String>,
}

/// Constant-time byte-slice equality. Doesn't short-circuit on the
/// Percent-decode a URL-encoded query value in place. Invalid `%XX`
/// escapes are passed through verbatim (lossy). Ultrareview #337.
#[inline]
pub(crate) fn percent_decode_lossy(input: &str) -> String {
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
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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
