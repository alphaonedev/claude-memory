// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// #873 — this file currently exceeds the 250-line per-function budget
// in `create_memory` (#866) and several other large handlers; the
// per-function `#[allow(clippy::too_many_lines)]` attributes inside
// keep the warn-level lint green while the splits land. Module-level
// allow is the belt-and-braces in case a function grows past
// threshold without picking up its own attribute. Tracked for split
// as #866 + #868.
#![allow(clippy::too_many_lines)]

use crate::db;
#[cfg(feature = "sal")]
use crate::models::Memory;

use super::AppState;
#[cfg(feature = "sal")]
use super::StorageBackend;

/// v0.7.0 L5 — minimum content length (chars) below which the HTTP
/// `create_memory` handler skips the `auto_tag` autonomy hook. Mirrors
/// the constant the MCP `handle_store` path uses (`AUTONOMY_MIN_CONTENT_LEN`
/// at `src/mcp.rs:1405`) so a memory that's too short to be meaningfully
/// tagged doesn't burn a 30s Ollama round-trip on each store.
const AUTO_TAG_MIN_CONTENT_LEN: usize = 50;
/// v0.7.0 L5 — maximum number of auto-generated tags merged into the
/// memory. Mirrors `mcp.rs:1827-1828` so postgres + sqlite + MCP all
/// converge on the same on-disk shape.
const AUTO_TAG_MAX_TAGS: usize = 8;

/// v0.7.0 fold-A2A1.6 (#700, S16/S49) — `app.store.get` with bounded
/// retry on [`crate::store::StoreError::NotFound`].
///
/// Why this exists: on a postgres-backed daemon a freshly-stored row
/// can briefly return NotFound from the SAL `get` while WAL flush
/// settles or the read query hits a still-replicating standby. The
/// 22-failure A2A triage (memory `9ffaa55d`) classified this as
/// Bucket-A: the row exists, the promote handler just races the
/// visibility window. Returning a one-shot 404 surfaces a flake to
/// the operator even though a 5 ms retry would have caught the
/// (eventually-consistent) row.
///
/// Retry budget: 5 + 10 + 15 + 20 ms = 50 ms wall clock, evenly
/// dwarfed by the 2 s daemon p99 SLO. Any other StoreError class
/// (e.g. backend down, integrity failure) returns immediately
/// without retry — those are not visibility-race symptoms.
#[cfg(feature = "sal")]
pub(super) async fn get_with_visibility_retry(
    store: &dyn crate::store::MemoryStore,
    ctx: &crate::store::CallerContext,
    id: &str,
) -> crate::store::StoreResult<Memory> {
    let mut attempt: u32 = 0;
    loop {
        match store.get(ctx, id).await {
            Ok(m) => return Ok(m),
            Err(crate::store::StoreError::NotFound { .. }) if attempt < 4 => {
                let backoff_ms = u64::from(5 * (attempt + 1));
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// v0.7.0 L5 — fire the LLM `auto_tag` hook for a freshly-built memory.
///
/// Returns the list of LLM-generated tags (capped at
/// [`AUTO_TAG_MAX_TAGS`]) when every gate is satisfied:
///   - The daemon's configured [`crate::config::FeatureTier`] declares
///     an `llm_model` (the smart / autonomous tier capability —
///     `tier_config.llm_model.is_some()`).
///   - The operator did NOT pre-populate `tags` on the request
///     (auto-tag never overwrites operator-supplied tags).
///   - The content is at least [`AUTO_TAG_MIN_CONTENT_LEN`] chars
///     (too-short content has no useful taggable signal).
///   - The namespace is not internal / system (starts with `_`) —
///     matches MCP's `handle_store` skip at `src/mcp.rs:1818`.
///   - An LLM client is wired on `AppState` and the Ollama endpoint
///     is reachable.
///
/// On any LLM error the function returns `Vec::new()` and logs a
/// `tracing::warn!` — auto_tag is a soft hook and a failure must not
/// fail the store (mirrors MCP `handle_store` at `src/mcp.rs:1830`).
///
/// The blocking Ollama call is wrapped in `tokio::task::spawn_blocking`
/// to keep the async runtime healthy under load — matches the embedder
/// pattern at `src/daemon_runtime.rs:1182`.
pub(crate) async fn maybe_auto_tag(
    app: &AppState,
    title: &str,
    content: &str,
    operator_tags: &[String],
    namespace: &str,
) -> Vec<String> {
    if !operator_tags.is_empty() {
        return Vec::new();
    }
    if content.len() < AUTO_TAG_MIN_CONTENT_LEN {
        return Vec::new();
    }
    if namespace.starts_with('_') {
        return Vec::new();
    }
    if app.tier_config.llm_model.is_none() {
        return Vec::new();
    }
    let llm_arc = app.llm.clone();
    if llm_arc.is_none() {
        return Vec::new();
    }
    // v0.7.0 L15 — when the operator has configured a dedicated tag
    // model (`auto_tag_model = "..."` in config.toml), pass it through
    // so the call hits the fast structured-output model instead of the
    // reasoning-tier llm_model. Closes the NHI-D-autotag-empty finding
    // where Gemma 4 thinking-mode would generate 400+ tokens for a
    // 5-tag list and hit the 30s tail latency.
    let auto_tag_model = app.auto_tag_model.as_ref().clone();
    let title_owned = title.to_string();
    let content_owned = content.to_string();
    let llm_timeout = app.llm_call_timeout;
    // H8 (v0.7.0 round-2) — bound the Ollama call by the configured
    // per-LLM-call timeout (default 30s). On timeout we degrade to the
    // LLM-absent fallback (empty tags) — same shape the keyword /
    // semantic tiers already return when no LLM is wired (L5/L7).
    let join = tokio::time::timeout(
        llm_timeout,
        tokio::task::spawn_blocking(move || {
            let llm = match llm_arc.as_ref() {
                Some(c) => c,
                None => return Ok(Vec::new()),
            };
            llm.auto_tag(&title_owned, &content_owned, auto_tag_model.as_deref())
        }),
    )
    .await;
    match join {
        Ok(Ok(Ok(tags))) => tags.into_iter().take(AUTO_TAG_MAX_TAGS).collect(),
        Ok(Ok(Err(e))) => {
            tracing::warn!("L5: auto_tag hook failed: {e}");
            Vec::new()
        }
        Ok(Err(e)) => {
            tracing::warn!("L5: auto_tag spawn_blocking join failed: {e}");
            Vec::new()
        }
        Err(_) => {
            tracing::warn!(
                "H8: LLM call (auto_tag) exceeded {}s timeout — falling back to no tags",
                llm_timeout.as_secs()
            );
            Vec::new()
        }
    }
}

/// v0.7.0 (issue #519) — same-namespace conflict probe fired during
/// `create_memory`. Mirrors the MCP `handle_store` autonomy hook's
/// `detect_contradiction` loop (`src/mcp.rs:1830-1850`) but lives on the
/// HTTP path so a smart/autonomous-tier daemon surfaces conflicts in the
/// 201 response without requiring the caller to follow up with a manual
/// `memory_detect_contradiction`.
///
/// Gating layers (any false → returns empty):
///   1. `request_override`:
///       `Some(true)`  → force-on regardless of `autonomous_hooks`
///       `Some(false)` → force-off regardless of `autonomous_hooks`
///       `None`        → defer to `autonomous_hooks`
///   2. tier — only smart/autonomous (`tier_config.llm_model.is_some()`)
///   3. LLM client wired (`app.llm`)
///   4. content ≥ 50 chars (matches `AUTO_TAG_MIN_CONTENT_LEN`)
///   5. namespace not `_*` (internal)
///
/// The probe is best-effort: any LLM error or timeout returns an empty
/// vec — never fails the parent store. Bounded by the H8 per-LLM-call
/// timeout (default 30s) the same way `maybe_auto_tag` is.
//
// v0.7.0 (round-2) — call sites for this helper are still being
// wired in the create_memory hot path; the function is staged for
// the next round so we silence the dead-code warning rather than
// rip out the implementation. Tracked in issue #519.
#[allow(dead_code)]
async fn maybe_detect_conflicts(
    app: &AppState,
    title: &str,
    content: &str,
    namespace: &str,
    request_override: Option<bool>,
) -> Vec<ConflictReport> {
    let enabled = match request_override {
        Some(b) => b,
        None => app.autonomous_hooks,
    };
    if !enabled
        || content.len() < AUTO_TAG_MIN_CONTENT_LEN
        || namespace.starts_with('_')
        || app.tier_config.llm_model.is_none()
    {
        return Vec::new();
    }
    let llm_arc = app.llm.clone();
    if llm_arc.is_none() {
        return Vec::new();
    }

    // Pull same-namespace candidates that could contradict the new memory.
    // Cap at 8 to bound LLM cost (8 × 30s worst-case = 4 min if every probe
    // tail-times-out; in practice most return in 0.7s on gemma3:4b).
    let candidates: Vec<(String, String, String)> =
        match fetch_namespace_candidates(app, namespace, title, 8).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("L?: maybe_detect_conflicts candidate fetch failed: {e}");
                return Vec::new();
            }
        };

    let llm_timeout = app.llm_call_timeout;
    let new_content = content.to_string();
    let mut out: Vec<ConflictReport> = Vec::new();
    for (cand_id, cand_title, cand_content) in candidates {
        let llm_arc_cl = llm_arc.clone();
        let cand_content_cl = cand_content.clone();
        let new_content_cl = new_content.clone();
        let join = tokio::time::timeout(
            llm_timeout,
            tokio::task::spawn_blocking(move || {
                let llm = match llm_arc_cl.as_ref() {
                    Some(c) => c,
                    None => return Ok(false),
                };
                llm.detect_contradiction(&new_content_cl, &cand_content_cl)
            }),
        )
        .await;
        match join {
            Ok(Ok(Ok(true))) => out.push(ConflictReport {
                id: cand_id,
                title: cand_title,
                suggested_merge: None,
            }),
            Ok(Ok(Ok(false))) => {}
            Ok(Ok(Err(e))) => tracing::warn!("detect_contradiction LLM error for {cand_id}: {e}"),
            Ok(Err(e)) => tracing::warn!("detect_contradiction join error for {cand_id}: {e}"),
            Err(_) => tracing::warn!(
                "H8: LLM call (detect_contradiction) exceeded {}s timeout for {cand_id} — skipping",
                llm_timeout.as_secs()
            ),
        }
    }
    out
}

/// Fetch up to `limit` same-namespace memories whose title is NOT byte-equal
/// to the incoming title (we want potentially-contradictory siblings, not
/// the row that an UPSERT would target). Routes through the active storage
/// backend.
//
// v0.7.0 (round-2) — only used by the staged-in `maybe_detect_conflicts`
// helper above; silence dead_code under pedantic until #519 wires the
// call site through create_memory.
#[allow(dead_code)]
async fn fetch_namespace_candidates(
    app: &AppState,
    namespace: &str,
    new_title: &str,
    limit: usize,
) -> Result<Vec<(String, String, String)>, String> {
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("ai:http");
        let filter = crate::store::Filter {
            namespace: Some(namespace.to_string()),
            limit: limit + 1,
            ..crate::store::Filter::default()
        };
        let mems = app
            .store
            .list(&ctx, &filter)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(mems
            .into_iter()
            .filter(|m| m.title != new_title)
            .take(limit)
            .map(|m| (m.id, m.title, m.content))
            .collect());
    }
    let lock = app.db.lock().await;
    let mems = db::list(
        &lock.0,
        Some(namespace),
        None,
        limit + 1,
        0,
        None,
        None,
        None,
        None,
        None,
    )
    .map_err(|e| e.to_string())?;
    Ok(mems
        .into_iter()
        .filter(|m| m.title != new_title)
        .take(limit)
        .map(|m| (m.id, m.title, m.content))
        .collect())
}

/// v0.7.0 (issue #519) — a single same-namespace memory the LLM flagged as
/// contradictory with the incoming row. Surfaced in the create_memory
/// response under `conflicts: [...]` when proactive detection ran.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConflictReport {
    pub id: String,
    pub title: String,
    /// LLM-proposed merged content. Future expansion (#519 §"suggested
    /// merge"). For v0.7.0 ship-scope this is left `None`; the caller can
    /// follow up with `memory_consolidate` using the reported ids. The
    /// field reserves the wire shape so callers can branch on it now.
    pub suggested_merge: Option<String>,
}

// v0.7.0 issue #897 — Coverage regression on the post-Wave-1-split
// `src/handlers/http.rs` shim. Path-A test additions: directly
// exercise the gate ladders + sqlite-branch traversal of the three
// helpers that live in this file (`maybe_auto_tag`,
// `maybe_detect_conflicts`, `fetch_namespace_candidates`). The
// `#[cfg(test)]` gating keeps these out of the production binary
// — pure test addition, no production behavior change.
#[cfg(test)]
#[allow(clippy::too_many_lines)]
mod cov897_tests {
    use super::{
        AUTO_TAG_MIN_CONTENT_LEN, ConflictReport, fetch_namespace_candidates, maybe_auto_tag,
        maybe_detect_conflicts,
    };
    use crate::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
    use crate::handlers::{AppState, Db, StorageBackend};
    use crate::models::{Memory, Tier};
    use chrono::Utc;
    use std::sync::Arc;
    use tokio::sync::{Mutex, RwLock};
    use uuid::Uuid;

    fn build_app(tier: FeatureTier, autonomous: bool) -> (AppState, tempfile::NamedTempFile) {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let path = tmp.path().to_path_buf();
        let _ = crate::db::open(&path).expect("db::open");
        let conn = crate::db::open(&path).expect("reopen");
        let db: Db = Arc::new(Mutex::new((
            conn,
            path.clone(),
            ResolvedTtl::default(),
            true,
        )));
        #[cfg(feature = "sal")]
        let store: Arc<dyn crate::store::MemoryStore> =
            Arc::new(crate::store::sqlite::SqliteStore::open(&path).expect("open SqliteStore"));
        let app = AppState {
            db,
            embedder: Arc::new(None),
            vector_index: Arc::new(Mutex::new(None)),
            federation: Arc::new(None),
            tier_config: Arc::new(tier.config()),
            scoring: Arc::new(ResolvedScoring::default()),
            profile: Arc::new(crate::profile::Profile::core()),
            mcp_config: Arc::new(None),
            active_keypair: Arc::new(None),
            family_embeddings: Arc::new(RwLock::new(Some(Vec::new()))),
            storage_backend: StorageBackend::Sqlite,
            #[cfg(feature = "sal")]
            store,
            llm: Arc::new(None),
            auto_tag_model: Arc::new(None),
            llm_call_timeout: std::time::Duration::from_secs(30),
            replay_cache: Arc::new(crate::identity::replay::ReplayCache::default()),
            verify_require_nonce: false,
            federation_nonce_cache: Arc::new(
                crate::identity::replay::FederationNonceCache::default(),
            ),
            autonomous_hooks: autonomous,
            recall_scope: Arc::new(None),
            deferred_audit_queue: Arc::new(None),
        };
        (app, tmp)
    }

    fn seed_memory(app: &AppState, namespace: &str, title: &str, content: &str) {
        let now = Utc::now().to_rfc3339();
        let mem = Memory {
            id: Uuid::new_v4().to_string(),
            title: title.to_string(),
            content: content.to_string(),
            namespace: namespace.to_string(),
            tier: Tier::Mid,
            created_at: now.clone(),
            updated_at: now,
            ..Default::default()
        };
        let lock = app.db.try_lock().expect("uncontended lock for seed");
        crate::db::insert(&lock.0, &mem).expect("insert");
    }

    // ---- maybe_auto_tag: the llm-arc fast-path on a Smart-tier app -----
    //
    // The lib-tier `maybe_auto_tag_gate_matrix_l5` test already covers
    // the operator-tags / short-content / internal-namespace / no-llm-
    // model branches. This case completes the gate ladder: Smart tier
    // sets `tier_config.llm_model = Some(...)`, the caller passes
    // permissive args (long content, no tags, public namespace), but
    // `app.llm = Arc::new(None)` — so the function must short-circuit
    // at the `llm_arc.is_none()` check rather than fall through to
    // the spawn_blocking path.
    #[tokio::test]
    async fn cov897_maybe_auto_tag_smart_tier_no_llm_arc_short_circuits() {
        let (app, _tmp) = build_app(FeatureTier::Smart, false);
        let r = maybe_auto_tag(
            &app,
            "title",
            &"x".repeat(AUTO_TAG_MIN_CONTENT_LEN + 10),
            &[],
            "public-ns",
        )
        .await;
        assert!(
            r.is_empty(),
            "Smart tier + llm=None must short-circuit, got {r:?}"
        );
    }

    // ---- maybe_detect_conflicts: full gate-ladder coverage -------------

    #[tokio::test]
    async fn cov897_detect_conflicts_disabled_by_default_returns_empty() {
        // autonomous_hooks=false + no per-request override → disabled.
        let (app, _tmp) = build_app(FeatureTier::Smart, false);
        let r = maybe_detect_conflicts(
            &app,
            "t",
            &"x".repeat(AUTO_TAG_MIN_CONTENT_LEN + 10),
            "ns",
            None,
        )
        .await;
        assert!(r.is_empty(), "disabled-by-config returns empty");
    }

    #[tokio::test]
    async fn cov897_detect_conflicts_request_override_false_forces_off() {
        // autonomous_hooks=true would normally enable; request override
        // Some(false) must force-off.
        let (app, _tmp) = build_app(FeatureTier::Smart, true);
        let r = maybe_detect_conflicts(
            &app,
            "t",
            &"x".repeat(AUTO_TAG_MIN_CONTENT_LEN + 10),
            "ns",
            Some(false),
        )
        .await;
        assert!(r.is_empty(), "override=Some(false) returns empty");
    }

    #[tokio::test]
    async fn cov897_detect_conflicts_short_content_returns_empty() {
        // Override forces enabled, but content is below 50 chars.
        let (app, _tmp) = build_app(FeatureTier::Smart, false);
        let r = maybe_detect_conflicts(&app, "t", "short", "ns", Some(true)).await;
        assert!(r.is_empty(), "short content returns empty");
    }

    #[tokio::test]
    async fn cov897_detect_conflicts_internal_namespace_returns_empty() {
        let (app, _tmp) = build_app(FeatureTier::Smart, false);
        let r = maybe_detect_conflicts(
            &app,
            "t",
            &"x".repeat(AUTO_TAG_MIN_CONTENT_LEN + 10),
            "_internal",
            Some(true),
        )
        .await;
        assert!(r.is_empty(), "internal namespace returns empty");
    }

    #[tokio::test]
    async fn cov897_detect_conflicts_no_llm_model_returns_empty() {
        // Keyword tier has `llm_model = None` — gate ladder line 199.
        let (app, _tmp) = build_app(FeatureTier::Keyword, false);
        let r = maybe_detect_conflicts(
            &app,
            "t",
            &"x".repeat(AUTO_TAG_MIN_CONTENT_LEN + 10),
            "ns",
            Some(true),
        )
        .await;
        assert!(r.is_empty(), "no llm_model returns empty");
    }

    #[tokio::test]
    async fn cov897_detect_conflicts_smart_tier_no_llm_arc_returns_empty() {
        // Smart tier has llm_model=Some, but app.llm=None → line 204-206.
        let (app, _tmp) = build_app(FeatureTier::Smart, false);
        let r = maybe_detect_conflicts(
            &app,
            "t",
            &"x".repeat(AUTO_TAG_MIN_CONTENT_LEN + 10),
            "ns",
            Some(true),
        )
        .await;
        assert!(r.is_empty(), "Smart tier + llm=None returns empty");
    }

    // ---- fetch_namespace_candidates: sqlite-branch traversal -----------

    #[tokio::test]
    async fn cov897_fetch_candidates_empty_namespace_returns_empty() {
        // Empty DB → empty candidate set; exercises the sqlite branch
        // (lines 291-310) cleanly without hitting any candidates.
        let (app, _tmp) = build_app(FeatureTier::Keyword, false);
        let out = fetch_namespace_candidates(&app, "empty-ns", "new-title", 8)
            .await
            .expect("sqlite list succeeds on empty db");
        assert!(out.is_empty(), "empty namespace returns no candidates");
    }

    #[tokio::test]
    async fn cov897_fetch_candidates_filters_byte_equal_title() {
        // Seed three rows in `ns-cand`; the function must return rows
        // whose title is NOT byte-equal to `new_title`. With three
        // seeded titles ["alpha", "beta", "gamma"] and new_title="beta"
        // we expect exactly ["alpha", "gamma"].
        let (app, _tmp) = build_app(FeatureTier::Keyword, false);
        seed_memory(&app, "ns-cand", "alpha", "content-alpha");
        seed_memory(&app, "ns-cand", "beta", "content-beta");
        seed_memory(&app, "ns-cand", "gamma", "content-gamma");
        let out = fetch_namespace_candidates(&app, "ns-cand", "beta", 8)
            .await
            .expect("sqlite list succeeds");
        let titles: Vec<&str> = out.iter().map(|(_, t, _)| t.as_str()).collect();
        assert_eq!(out.len(), 2, "filters byte-equal title, got {titles:?}");
        assert!(titles.contains(&"alpha"), "alpha present in {titles:?}");
        assert!(titles.contains(&"gamma"), "gamma present in {titles:?}");
        assert!(!titles.contains(&"beta"), "beta filtered from {titles:?}");
    }

    #[tokio::test]
    async fn cov897_fetch_candidates_honors_limit() {
        // Seed 5 rows; ask for limit=2 — internal cap is limit+1=3
        // candidates pulled, then post-filter `.take(limit)`. With a
        // distinct new_title (no byte-equal match), `.take(2)` yields
        // exactly 2 rows.
        let (app, _tmp) = build_app(FeatureTier::Keyword, false);
        for i in 0..5 {
            seed_memory(
                &app,
                "ns-limit",
                &format!("title-{i}"),
                &format!("content-{i}"),
            );
        }
        let out = fetch_namespace_candidates(&app, "ns-limit", "no-match", 2)
            .await
            .expect("sqlite list succeeds");
        assert_eq!(out.len(), 2, "limit honored");
    }

    // ---- ConflictReport: pinned wire shape -----------------------------
    //
    // The struct lands in the create_memory response envelope under
    // `conflicts: [...]`; pin its serialized shape so a future refactor
    // doesn't silently rename a wire field.
    #[test]
    fn cov897_conflict_report_serializes_to_pinned_wire_shape() {
        let r = ConflictReport {
            id: "mem-id-123".to_string(),
            title: "conflicting title".to_string(),
            suggested_merge: None,
        };
        let v = serde_json::to_value(&r).expect("serialize");
        assert_eq!(v["id"], "mem-id-123");
        assert_eq!(v["title"], "conflicting title");
        assert!(v["suggested_merge"].is_null(), "None ⇒ null on the wire");
    }
}
