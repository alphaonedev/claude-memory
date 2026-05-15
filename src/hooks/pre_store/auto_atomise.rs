// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 WT-1-D — auto-atomisation pre_store substrate hook.
//!
//! When the namespace policy
//! [`crate::models::GovernancePolicy::auto_atomise`] resolves to
//! `Some(true)` for the stored memory's namespace, the substrate-side
//! `pre_store` hook deferred-enqueues a curator atomisation pass on a
//! detached worker thread. The hook NEVER blocks the
//! `memory_store` response — same discipline as the L2-1 reflection-
//! pass curator and the QW-1 `post_reflect` auto-export hook.
//!
//! # Hard guarantees
//!
//! 1. **Non-blocking.** The hook returns synchronously after at most
//!    a token-count + policy resolution. The curator round-trip runs
//!    on a detached `std::thread::spawn`. The `memory_store` latency
//!    on namespaces with `auto_atomise = true` must be within 5% of
//!    the equivalent un-hooked path (acceptance test
//!    `test_auto_atomise_does_not_block_store_response`).
//!
//! 2. **Notify-class.** Failures inside the worker thread (curator
//!    LLM unavailable, race against a concurrent atomisation, etc.)
//!    are logged via `tracing::{info,warn,error}` and NEVER propagate
//!    back to the caller. The memory is already committed; making the
//!    operator chase a transient curator error is worse than a missed
//!    atomisation. The next manual `memory_atomise` call (or a future
//!    sweep) can recover the work.
//!
//! 3. **Capability isolation.** This code is gated by the namespace
//!    policy. An operator who has not explicitly opted in to
//!    `auto_atomise` on the namespace standard's `metadata.governance`
//!    will see no curator round-trips from this module ever.
//!
//! # Wiring
//!
//! The daemon `serve` bootstrap installs an [`AutoAtomisationDispatch`]
//! via [`install_auto_atomise_dispatch`] (one-shot `OnceLock`). The
//! MCP / HTTP / CLI store handlers call [`maybe_enqueue_auto_atomise`]
//! right after a successful `db::insert` returns. When the dispatch
//! is unset (CLI one-shots, the test harness without an Atomiser),
//! the helper is a zero-cost no-op.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::atomisation::{AtomiseError, Atomiser};
use crate::models::Memory;
use crate::storage as db;

/// Outcome surfaced to telemetry by the worker thread. The MCP
/// response shape never carries this — the hook is deferred — but the
/// test harness inspects it via the optional observation channel.
#[derive(Debug, Clone)]
pub enum AutoAtomisationOutcome {
    /// Policy is `None` / `Some(false)` for the namespace, or the
    /// dispatch is unset. The hook short-circuits silently.
    Skipped { reason: &'static str },
    /// Token count fell at or under the configured threshold.
    UnderThreshold { tokens: usize, threshold: u32 },
    /// Worker thread enqueued; the curator round-trip will land
    /// asynchronously. The `memory_store` response has already
    /// returned to the caller by this point.
    Enqueued {
        memory_id: String,
        namespace: String,
    },
}

/// Dispatch handle installed by the daemon. The auto-atomisation
/// hook closes over the database path (so it can re-open a fresh
/// connection on the worker thread — rusqlite connections are not
/// `Send`) and the [`Atomiser`] (which carries the curator + signing
/// key + tunables).
///
/// `Arc`-wrapped so the dispatch is cheaply cloneable into worker
/// threads.
pub struct AutoAtomisationDispatch {
    pub db_path: PathBuf,
    pub atomiser: Arc<Atomiser>,
}

impl std::fmt::Debug for AutoAtomisationDispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoAtomisationDispatch")
            .field("db_path", &self.db_path)
            .field("atomiser", &"<Arc<Atomiser>>")
            .finish()
    }
}

/// Process-wide one-shot dispatch slot. The daemon `serve` bootstrap
/// is the only production caller of `set`. CLI one-shots (`ai-memory
/// store`, `ai-memory recall`, …) leave it unset so the hook is a
/// pure no-op on the operator-direct substrate path.
///
/// Public so tests in the integration suite can install a mock
/// dispatch directly without round-tripping through the daemon
/// bootstrap.
pub static AUTO_ATOMISE_DISPATCH: std::sync::OnceLock<Arc<AutoAtomisationDispatch>> =
    std::sync::OnceLock::new();

/// One-shot install of the dispatch. Returns `Err` when called a
/// second time (the `OnceLock::set` contract); the daemon bootstrap
/// is the only intended caller in production.
///
/// # Errors
/// Returns the supplied dispatch back on second-set so the caller
/// can surface "already installed" to the operator.
pub fn install_auto_atomise_dispatch(
    dispatch: AutoAtomisationDispatch,
) -> Result<(), Arc<AutoAtomisationDispatch>> {
    AUTO_ATOMISE_DISPATCH.set(Arc::new(dispatch))
}

/// Substrate-side hook entry point. Called by every successful
/// `memory_store` write path (MCP `handle_store`, HTTP create_memory
/// handler, CLI store) AFTER the row commits.
///
/// Returns synchronously with the outcome (for telemetry); the
/// caller MUST NOT block on the result — the curator round-trip runs
/// on a detached `std::thread::spawn` when `Outcome::Enqueued` fires.
///
/// # Logic (matches the WT-1-D brief)
///
/// 1. Look up the dispatch; bail with `Skipped { "dispatch_unset" }`
///    when the daemon hasn't installed it (CLI / test mode).
/// 2. Resolve the namespace policy via
///    [`db::resolve_governance_policy`]; fall back to defaults when
///    no policy is configured.
/// 3. If `!policy.effective_auto_atomise()`, return
///    `Skipped { "policy_disabled" }`.
/// 4. Token-count `memory.content` via `cl100k_base`; if the count
///    is `<= threshold`, return `UnderThreshold`.
/// 5. Threshold exceeded → spawn a detached worker thread, return
///    `Enqueued` synchronously.
#[must_use]
pub fn maybe_enqueue_auto_atomise(
    memory: &Memory,
    calling_agent_id: &str,
) -> AutoAtomisationOutcome {
    let Some(dispatch) = AUTO_ATOMISE_DISPATCH.get() else {
        return AutoAtomisationOutcome::Skipped {
            reason: "dispatch_unset",
        };
    };

    // Resolve policy from a fresh connection. The hook is called
    // post-commit so the namespace standard (if any) is visible.
    let policy = match db::open(&dispatch.db_path) {
        Ok(conn) => db::resolve_governance_policy(&conn, &memory.namespace).unwrap_or_default(),
        Err(e) => {
            tracing::warn!(
                target: "pre_store.auto_atomise",
                "policy resolve: failed to open db at {}: {}",
                dispatch.db_path.display(),
                e
            );
            return AutoAtomisationOutcome::Skipped {
                reason: "db_open_failed",
            };
        }
    };

    if !policy.effective_auto_atomise() {
        return AutoAtomisationOutcome::Skipped {
            reason: "policy_disabled",
        };
    }

    let threshold = policy.effective_auto_atomise_threshold_cl100k();
    let tokens = db::count_tokens_cl100k(&memory.content);
    if tokens <= threshold as usize {
        return AutoAtomisationOutcome::UnderThreshold { tokens, threshold };
    }

    let max_atom_tokens = policy.effective_auto_atomise_max_atom_tokens();

    let dispatch_for_thread = Arc::clone(dispatch);
    let memory_id = memory.id.clone();
    let namespace = memory.namespace.clone();
    let agent_id = calling_agent_id.to_string();

    std::thread::spawn(move || {
        run_deferred_atomise(
            &dispatch_for_thread.db_path,
            &dispatch_for_thread.atomiser,
            &memory_id,
            max_atom_tokens,
            &agent_id,
        );
    });

    AutoAtomisationOutcome::Enqueued {
        memory_id: memory.id.clone(),
        namespace,
    }
}

/// v0.7.x Form 2 (#755) — Synchronous-mode entry point.
///
/// Runs the curator pass INSIDE the caller's MCP handler so atoms
/// surface in recall BEFORE the `memory_store` response returns. The
/// caller is responsible for SKIPPING the source-embed step before
/// invoking this function (it checks the namespace policy mode before
/// deciding to embed), so the substrate honours Batman's Form 2
/// "decompose THEN embed" criterion.
///
/// Returns a short telemetry string describing the outcome:
///   - `"atomised"` on success
///   - `"skipped_dispatch_unset"`     dispatch slot empty (CLI / test)
///   - `"skipped_under_threshold"`   token count <= threshold
///   - `"skipped_source_too_small"`  curator returned no productive split
///   - `"skipped_already_atomised"`  source already atomised
///   - `"failed"`                    curator error (logged)
///
/// Errors are logged + swallowed per the same notify-class contract
/// the deferred path uses — a curator outage must not block the
/// memory_store write that has already committed.
#[must_use]
pub fn run_synchronous_auto_atomise(
    conn: &rusqlite::Connection,
    memory: &Memory,
    calling_agent_id: &str,
) -> &'static str {
    let Some(dispatch) = AUTO_ATOMISE_DISPATCH.get() else {
        tracing::info!(
            target: "pre_store.auto_atomise.sync",
            "synchronous-mode dispatch unset for memory={}; substrate stays quiet",
            memory.id,
        );
        return "skipped_dispatch_unset";
    };

    let policy = db::resolve_governance_policy(conn, &memory.namespace).unwrap_or_default();
    let threshold = policy.effective_auto_atomise_threshold_cl100k();
    let tokens = db::count_tokens_cl100k(&memory.content);
    if tokens <= threshold as usize {
        return "skipped_under_threshold";
    }
    let max_atom_tokens = policy.effective_auto_atomise_max_atom_tokens();

    match dispatch
        .atomiser
        .atomise_sync(conn, &memory.id, max_atom_tokens, false, calling_agent_id)
    {
        Ok(result) => {
            tracing::info!(
                target: "pre_store.auto_atomise.sync",
                "synchronous-atomise succeeded: source={} atoms={}",
                result.source_id,
                result.atom_count,
            );
            "atomised"
        }
        Err(AtomiseError::SourceTooSmall) => {
            tracing::info!(
                target: "pre_store.auto_atomise.sync",
                "synchronous-atomise skipped: source={} body too small",
                memory.id,
            );
            "skipped_source_too_small"
        }
        Err(AtomiseError::AlreadyAtomised { .. }) => {
            tracing::info!(
                target: "pre_store.auto_atomise.sync",
                "synchronous-atomise skipped: source={} already atomised",
                memory.id,
            );
            "skipped_already_atomised"
        }
        Err(e) => {
            tracing::error!(
                target: "pre_store.auto_atomise.sync",
                "synchronous-atomise failed for source={}: {:?}",
                memory.id,
                e,
            );
            "failed"
        }
    }
}

/// Worker-thread entry-point.
///
/// Sleeps 100ms for the transaction-commit visibility window (matches
/// the WT-1-D brief), then opens a fresh connection and calls
/// `atomiser.atomise_sync`. Encapsulated as a free function so unit
/// tests can drive it without spawning a thread.
///
/// Errors are logged + swallowed per the notify-class contract.
pub fn run_deferred_atomise(
    db_path: &Path,
    atomiser: &Atomiser,
    memory_id: &str,
    max_atom_tokens: u32,
    calling_agent_id: &str,
) {
    // The 100ms wait gives the originating transaction's WAL frame
    // time to checkpoint past the worker's read horizon on SQLite.
    // On Postgres the wait is operationally unnecessary but harmless
    // (post-commit visibility is immediate).
    std::thread::sleep(std::time::Duration::from_millis(100));

    let conn = match db::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                target: "pre_store.auto_atomise",
                "worker: failed to open db at {} for memory={}: {}",
                db_path.display(),
                memory_id,
                e
            );
            return;
        }
    };

    match atomiser.atomise_sync(&conn, memory_id, max_atom_tokens, false, calling_agent_id) {
        Ok(result) => {
            tracing::info!(
                target: "pre_store.auto_atomise",
                "auto-atomisation succeeded: source={} atoms={}",
                result.source_id,
                result.atom_count
            );
        }
        Err(AtomiseError::AlreadyAtomised {
            source_id,
            existing_atom_ids,
        }) => {
            tracing::info!(
                target: "pre_store.auto_atomise",
                "auto-atomisation skipped (race): source={} already split into {} atoms",
                source_id,
                existing_atom_ids.len()
            );
        }
        Err(AtomiseError::SourceTooSmall) => {
            tracing::warn!(
                target: "pre_store.auto_atomise",
                "auto-atomisation skipped: source={} body fits within max_atom_tokens (curator returned no atoms)",
                memory_id
            );
        }
        Err(AtomiseError::CuratorFailed(reason)) => {
            tracing::error!(
                target: "pre_store.auto_atomise",
                "auto-atomisation curator failed for source={}: {} — operator may retry with `memory_atomise`",
                memory_id,
                reason
            );
        }
        Err(AtomiseError::TierLocked) => {
            tracing::info!(
                target: "pre_store.auto_atomise",
                "auto-atomisation skipped: source={} tier_locked (keyword feature tier)",
                memory_id
            );
        }
        Err(AtomiseError::NotFound) => {
            // Race: memory was deleted between commit and hook
            // fire. Nothing to atomise.
            tracing::info!(
                target: "pre_store.auto_atomise",
                "auto-atomisation skipped: source={} not found (raced with delete?)",
                memory_id
            );
        }
        Err(e) => {
            tracing::error!(
                target: "pre_store.auto_atomise",
                "auto-atomisation failed for source={}: {:?} (full context: {})",
                memory_id,
                e,
                e
            );
        }
    }
}

/// Test-only helper: clear the process-wide dispatch slot. The
/// `OnceLock::set` API is one-shot per process, so the integration
/// suite uses a `Mutex<()>` to serialise tests and re-installs via
/// the public [`AUTO_ATOMISE_DISPATCH`] reference. This helper exists
/// solely so the suite can swap mocks between test cases without
/// spawning a fresh process; production code MUST NOT call it.
#[cfg(test)]
pub fn _test_only_take_dispatch() -> Option<Arc<AutoAtomisationDispatch>> {
    // OnceLock has no `take`. We can't actually clear it; the
    // integration suite installs once and reuses the dispatch
    // across tests by mutating mutable state inside the mock
    // atomiser.
    AUTO_ATOMISE_DISPATCH.get().cloned()
}

// ---------------------------------------------------------------------------
// Unit tests — exercise the policy-resolution + threshold logic without
// spawning a worker thread. Integration tests in `tests/auto_atomise/`
// drive the full deferred-enqueue path with a real Atomiser + mock
// curator.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ApproverType, GovernanceLevel, GovernancePolicy, Tier};
    use chrono::Utc;
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn fresh_db() -> (Connection, TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ai-memory.db");
        let conn = db::open(&path).unwrap();
        (conn, dir, path)
    }

    fn make_memory(ns: &str, content: &str) -> Memory {
        let now = Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: ns.to_string(),
            title: format!("title-{}", uuid::Uuid::new_v4().simple()),
            content: content.to_string(),
            created_at: now.clone(),
            updated_at: now,
            metadata: serde_json::json!({"agent_id": "ai:test"}),
            ..Default::default()
        }
    }

    fn seed_policy(conn: &Connection, ns: &str, policy: GovernancePolicy) {
        let now = Utc::now().to_rfc3339();
        let gov_metadata = serde_json::json!({
            "agent_id": "ai:test",
            "governance": serde_json::to_value(&policy).unwrap(),
        });
        let std_mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: ns.to_string(),
            title: format!("__standard_{ns}"),
            content: "standard".into(),
            created_at: now.clone(),
            updated_at: now,
            metadata: gov_metadata,
            ..Default::default()
        };
        let std_id = db::insert(conn, &std_mem).unwrap();
        db::set_namespace_standard(conn, ns, &std_id, None).unwrap();
    }

    fn opt_in_policy() -> GovernancePolicy {
        GovernancePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
            inherit: true,
            max_reflection_depth: None,
            auto_export_reflections_to_filesystem: None,
            auto_atomise: Some(true),
            auto_atomise_threshold_cl100k: Some(50),
            auto_atomise_max_atom_tokens: Some(20),
            auto_persona_trigger_every_n_memories: None,
            auto_export_personas_to_filesystem: None,
            auto_atomise_mode: None,
            legacy_per_pair_classifier: None,
        }
    }

    #[test]
    fn outcome_variants_render_with_debug() {
        // Spot-check the closed enum renders for telemetry.
        for o in [
            AutoAtomisationOutcome::Skipped {
                reason: "policy_disabled",
            },
            AutoAtomisationOutcome::UnderThreshold {
                tokens: 100,
                threshold: 500,
            },
            AutoAtomisationOutcome::Enqueued {
                memory_id: "m-1".into(),
                namespace: "ns".into(),
            },
        ] {
            let s = format!("{o:?}");
            assert!(!s.is_empty());
        }
    }

    #[test]
    fn dispatch_unset_short_circuits_to_skipped() {
        // The process-wide dispatch slot is empty in the unit-test
        // harness (no daemon bootstrap, no install_auto_atomise_dispatch
        // call). The hook MUST be a zero-cost no-op.
        let mem = make_memory("any-ns", "any body");
        let outcome = maybe_enqueue_auto_atomise(&mem, "ai:test");
        // We can't assert exact reason because the integration tests
        // may have installed a dispatch — but in the unit-test
        // crate boundary the dispatch is process-wide. We accept
        // either "dispatch_unset" OR "policy_disabled" (when an
        // integration test has installed a dispatch but no policy
        // is configured for this namespace).
        match outcome {
            AutoAtomisationOutcome::Skipped { reason } => {
                assert!(
                    reason == "dispatch_unset"
                        || reason == "policy_disabled"
                        || reason == "db_open_failed",
                    "unexpected skip reason: {reason}"
                );
            }
            _ => panic!("expected Skipped on empty/unconfigured dispatch, got {outcome:?}"),
        }
    }

    #[test]
    fn policy_resolution_returns_default_when_no_standard() {
        // When no namespace standard has been configured, the
        // resolver returns None and the caller falls back to
        // `GovernancePolicy::default()` which has `auto_atomise =
        // None` → `effective_auto_atomise()` resolves to false.
        let (conn, _dir, _path) = fresh_db();
        let policy = db::resolve_governance_policy(&conn, "fresh-ns").unwrap_or_default();
        assert!(!policy.effective_auto_atomise());
        assert_eq!(policy.effective_auto_atomise_threshold_cl100k(), 500);
        assert_eq!(policy.effective_auto_atomise_max_atom_tokens(), 200);
    }

    #[test]
    fn policy_resolution_picks_up_opt_in() {
        // Seed an opt-in policy; the resolver must surface
        // `auto_atomise = Some(true)` and the threshold / budget
        // overrides.
        let (conn, _dir, _path) = fresh_db();
        seed_policy(&conn, "opt-in-ns", opt_in_policy());
        let policy = db::resolve_governance_policy(&conn, "opt-in-ns").unwrap_or_default();
        assert!(policy.effective_auto_atomise());
        assert_eq!(policy.effective_auto_atomise_threshold_cl100k(), 50);
        assert_eq!(policy.effective_auto_atomise_max_atom_tokens(), 20);
    }
}
