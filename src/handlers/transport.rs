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

    /// v0.7.0 #922 — per-peer LRU keyed on `(peer_id, X-Memory-Nonce)`.
    pub federation_nonce_cache: Arc<crate::identity::replay::FederationNonceCache>,

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
///
/// v0.7.0 fold-A2A1.4 (#702) — `mtls_enforced` carries whether the
/// listener this state is mounted on enforces mTLS at the rustls layer
/// (i.e. `--tls-cert + --tls-key + --mtls-allowlist`). When true, the
/// federation endpoints (`/api/v1/sync/*`) are allowed without an
/// `x-api-key` header because the rustls server has already verified
/// the client cert against the operator-pinned allowlist — adding an
/// api-key check on top would force every peer to also carry the
/// shared api-key secret, which is exactly the auth-matrix gap
/// procurement deployments hit (a peer with valid mTLS but no
/// `x-api-key` got 401 and quorum never converged across hosts).
/// Non-federation paths still demand the api-key when configured.
#[derive(Clone, Default)]
pub struct ApiKeyState {
    pub key: Option<String>,
    pub mtls_enforced: bool,
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

    // v0.7.0 fold-A2A1.4 (#702) — mTLS bypass for federation endpoints.
    //
    // The federation peer mesh authenticates via mTLS cert-fingerprint
    // pinning (see `tls::FingerprintAllowlistVerifier` — rustls rejects
    // any TLS connect whose client cert isn't on the operator's
    // allowlist). When that's enforced, a request reaching this
    // middleware has already cleared a stronger authentication step
    // than `x-api-key`. Demanding the api-key on top forces every peer
    // to ALSO carry the shared secret, which causes the cross-host
    // quorum gap procurement-grade deployments hit (the peer's
    // outbound forgets the header → 401 → quorum_not_met). The
    // bypass is scoped to `/api/v1/sync/*` so non-federation surfaces
    // still require the api-key when configured (defense in depth).
    let path = req.uri().path();
    if auth.mtls_enforced && path.starts_with("/api/v1/sync/") {
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
