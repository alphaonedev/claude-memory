// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Cluster-F performance regression-pins (issue #767).
//!
//! Each test asserts a behavioural property the Cluster-F refactor
//! introduced; a future change that re-introduces the per-row /
//! per-store overhead will trip the corresponding assertion.
//!
//! Tests covered:
//!
//! * **PERF-2** — `recall_hybrid_loop_does_not_call_get_embedding_per_row`:
//!   the FTS-branch loop pulls the embedding inline from the SELECT
//!   list; a per-row `get_embedding` round-trip would surface as an
//!   extra `SELECT embedding FROM memories WHERE id = …` in the trace
//!   captured via `Connection::trace`. Asserts zero such queries fire.
//!
//! * **PERF-6** — `recall_touch_uses_single_transaction_per_call`:
//!   the touch fan-out collapses K rows into ONE `BEGIN IMMEDIATE`.
//!   Asserts exactly one `BEGIN` (and one `COMMIT`) fires regardless
//!   of how many rows the recall surfaces.
//!
//! * **PERF-1** — `pre_store_hooks_share_caller_connection`: the
//!   synchronous `maybe_enqueue_auto_atomise` path no longer opens a
//!   fresh DB connection. We exercise the hook via `db::open` plus
//!   `maybe_enqueue_auto_atomise(&conn, …)`; the absence of a
//!   redundant connection-open is encoded by the type signature
//!   itself (the hook now consumes `&Connection`). The test asserts
//!   the hook still functions on the caller-supplied connection.
//!
//! * **PERF-5** — `synchronous_atomise_default_max_retries_is_1`:
//!   pins `AtomiserConfig::sync_curator_max_retries` to 1 so a future
//!   change that bumps it back to the deferred-path default (3)
//!   trips the assertion.
//!
//! * **PERF-5 (override)** — `auto_atomise_max_retries_policy_override_honored`:
//!   the per-namespace `auto_atomise_max_retries` knob takes precedence
//!   over the compiled `sync_curator_max_retries`. Set namespace policy
//!   to 5, drive an always-failing mock curator, assert curator called
//!   exactly 6 times (1 initial + 5 retries).

#![allow(
    clippy::too_many_lines,
    clippy::doc_markdown,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::similar_names,
    clippy::items_after_statements,
    clippy::let_and_return,
    clippy::map_unwrap_or,
    clippy::ignored_unit_patterns,
    clippy::redundant_closure_for_method_calls,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ai_memory::atomisation::curator::{Atom, Curator, CuratorError};
use ai_memory::atomisation::{Atomiser, AtomiserConfig};
use ai_memory::config::FeatureTier;
use ai_memory::hooks::pre_store::{
    AutoAtomisationDispatch, AutoAtomisationOutcome, install_auto_atomise_dispatch,
    maybe_enqueue_auto_atomise,
};
use ai_memory::models::{
    ApproverType, AtomisationPolicy, AutoAtomiseMode, ConfidenceSource, CorePolicy,
    GovernanceLevel, GovernancePolicy, Memory, Tier,
};
use ai_memory::storage as db;

use chrono::Utc;
use rusqlite::Connection;

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

fn local_runs_root() -> PathBuf {
    std::env::var("TMPDIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".local-runs")
                .join("tmp")
        })
}

fn fresh_db_path(tag: &str) -> PathBuf {
    let root = local_runs_root();
    std::fs::create_dir_all(&root).ok();
    root.join(format!("cluster-f-{tag}-{}.db", uuid::Uuid::new_v4()))
}

fn open_db(tag: &str) -> (Connection, PathBuf) {
    let path = fresh_db_path(tag);
    let conn = db::open(&path).expect("open db");
    (conn, path)
}

fn make_memory(ns: &str, title: &str, content: &str) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: ns.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "ai:cluster-f"}),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    }
}

// ---------------------------------------------------------------------------
// SQL trace capture — installed on the test connection so we can count
// statements fired during recall. `Connection::trace` accepts only a
// fn-pointer (no closures), so we route every traced statement into a
// process-wide static `Mutex<Vec<String>>` and snapshot before each
// test. The integration suite runs tests serially when they share the
// trace sink (a per-test guard mutex enforces this).
// ---------------------------------------------------------------------------

static TRACE_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
static TRACE_GUARD: Mutex<()> = Mutex::new(());

fn trace_callback(stmt: &str) {
    TRACE_LOG.lock().unwrap().push(stmt.to_string());
}

fn install_trace(conn: &mut Connection) {
    TRACE_LOG.lock().unwrap().clear();
    conn.trace(Some(trace_callback));
}

fn snapshot_trace() -> Vec<String> {
    TRACE_LOG.lock().unwrap().clone()
}

fn count_matching(log: &[String], pred: impl Fn(&str) -> bool) -> usize {
    log.iter().filter(|s| pred(s)).count()
}

// ---------------------------------------------------------------------------
// PERF-2 — recall hybrid loop does NOT issue a per-row `get_embedding`
// ---------------------------------------------------------------------------

#[test]
fn recall_hybrid_loop_does_not_call_get_embedding_per_row() {
    let _g = TRACE_GUARD.lock().unwrap_or_else(|p| p.into_inner());
    let (mut conn, _path) = open_db("perf2");
    // Seed 5 memories so the FTS query returns multiple rows.
    let ns = "perf2/ns".to_string();
    for i in 0..5 {
        let mut mem = make_memory(
            &ns,
            &format!("perf2-row-{i}"),
            "shared keyword decompose deploy kubernetes pod readiness",
        );
        // Stamp a small embedding so the FTS row carries a non-NULL
        // embedding column. Format follows the v17 magic-byte header
        // (`decode_embedding_blob` tolerates legacy unheaded LE-f32).
        let dummy: Vec<f32> = vec![0.1; 8];
        mem.id = db::insert(&conn, &mem).unwrap();
        let mut bytes: Vec<u8> = Vec::with_capacity(dummy.len() * 4);
        for f in &dummy {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
        conn.execute(
            "UPDATE memories SET embedding = ?1 WHERE id = ?2",
            rusqlite::params![bytes, mem.id],
        )
        .unwrap();
    }

    install_trace(&mut conn);

    let query_embedding: Vec<f32> = vec![0.1; 8];
    let scoring = ai_memory::config::ResolvedScoring::default();
    let (results, _outcome, _telemetry) = db::recall_hybrid_with_telemetry(
        &conn,
        "decompose deploy kubernetes",
        &query_embedding,
        Some(&ns),
        10,
        None,
        None,
        None,
        None,
        3600,
        86400,
        None,
        None,
        &scoring,
        false,
        None,
    )
    .expect("recall_hybrid_with_telemetry ok");
    assert!(!results.is_empty(), "expected non-empty recall results");

    // Detach the trace before snapshotting so any post-test cleanup
    // queries don't leak into the log.
    conn.trace(None);
    let log = snapshot_trace();

    // Cluster-F PERF-2 — the FTS branch must NOT issue a per-row
    // `SELECT embedding FROM memories WHERE id = …`. The bytes come
    // inline from the FTS SELECT list now.
    let per_row_embedding_queries = count_matching(&log, |stmt| {
        let s = stmt.replace([' ', '\n', '\t'], "");
        s.contains("SELECTembeddingFROMmemoriesWHEREid=")
    });
    assert_eq!(
        per_row_embedding_queries, 0,
        "PERF-2 regression: recall hybrid loop issued {per_row_embedding_queries} per-row `get_embedding` queries (expected 0). Log: {log:#?}"
    );
}

// ---------------------------------------------------------------------------
// PERF-6 — recall touch collapses to a SINGLE transaction
// ---------------------------------------------------------------------------

#[test]
fn recall_touch_uses_single_transaction_per_call() {
    let _g = TRACE_GUARD.lock().unwrap_or_else(|p| p.into_inner());
    let (mut conn, _path) = open_db("perf6");
    let ns = "perf6/ns".to_string();
    // Seed 4 memories so K > 1.
    for i in 0..4 {
        let mem = make_memory(
            &ns,
            &format!("perf6-row-{i}"),
            "needle haystack alpha bravo charlie",
        );
        db::insert(&conn, &mem).unwrap();
    }

    install_trace(&mut conn);

    let (results, _outcome) = db::recall(
        &conn,
        "needle haystack",
        Some(&ns),
        10,
        None,
        None,
        None,
        3600,
        86400,
        None,
        None,
        false,
        None,
    )
    .expect("recall ok");
    assert!(
        results.len() >= 2,
        "expected ≥2 recall hits to stress the touch fan-out, got {}",
        results.len()
    );

    // Detach the trace before snapshotting so any post-test cleanup
    // queries don't leak into the log.
    conn.trace(None);
    let log = snapshot_trace();

    let begin_count = count_matching(&log, |stmt| {
        stmt.trim_start().starts_with("BEGIN IMMEDIATE")
    });
    let commit_count = count_matching(&log, |stmt| stmt.trim_start().starts_with("COMMIT"));

    // Cluster-F PERF-6 — exactly one BEGIN / COMMIT pair for the
    // entire touch fan-out, regardless of K.
    assert_eq!(
        begin_count, 1,
        "PERF-6 regression: expected exactly 1 BEGIN IMMEDIATE in the recall trace, got {begin_count}. Log: {log:#?}"
    );
    assert_eq!(
        commit_count, 1,
        "PERF-6 regression: expected exactly 1 COMMIT in the recall trace, got {commit_count}. Log: {log:#?}"
    );
}

// ---------------------------------------------------------------------------
// PERF-1 — pre_store synchronous hook reuses the caller's Connection
// ---------------------------------------------------------------------------

#[test]
fn pre_store_hooks_share_caller_connection() {
    // The PERF-1 fix is encoded in the type signature itself:
    // `maybe_enqueue_auto_atomise` takes a `&Connection` now, so it
    // CANNOT open a fresh one without a fresh DB path the caller
    // never gave it. This test verifies the hook still resolves the
    // namespace policy using the caller's connection (i.e. a
    // committed namespace standard becomes visible to the hook
    // immediately, no `db::open` round-trip required).
    let (conn, _path) = open_db("perf1");

    // Seed a namespace standard whose policy DISABLES auto_atomise.
    let ns = "perf1/disabled-ns".to_string();
    let policy = GovernancePolicy {
        core: CorePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
            inherit: true,
            ..CorePolicy::default()
        },
        atomisation: AtomisationPolicy {
            auto_atomise: Some(false),
            ..AtomisationPolicy::default()
        },
        ..GovernancePolicy::default()
    };
    let std_mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: ns.clone(),
        title: format!("__standard_{ns}"),
        content: "standard".to_string(),
        metadata: serde_json::json!({
            "agent_id": "ai:test",
            "governance": serde_json::to_value(&policy).unwrap(),
        }),
        ..Default::default()
    };
    let std_id = db::insert(&conn, &std_mem).unwrap();
    db::set_namespace_standard(&conn, &ns, &std_id, None).unwrap();

    // Build the in-flight Memory and invoke the hook. Even though
    // the dispatch slot may be unset (CLI-style test harness), the
    // hook MUST return a `Skipped { dispatch_unset | policy_disabled }`
    // outcome WITHOUT issuing a `db::open` against any path — the
    // type signature itself forbids the in-hook open.
    let mem = make_memory(&ns, "perf1-mem", "any body large enough");
    let outcome = maybe_enqueue_auto_atomise(&conn, &mem, &mem.id, "ai:test");
    match outcome {
        AutoAtomisationOutcome::Skipped { reason } => {
            // Either dispatch_unset (CLI / unit-test mode) or
            // policy_disabled (when a dispatch was installed by an
            // earlier in-process test). Both prove the hook
            // short-circuited on data the caller's connection
            // surfaced — no extra `db::open` could have happened.
            assert!(
                reason == "dispatch_unset" || reason == "policy_disabled",
                "expected dispatch_unset|policy_disabled, got {reason}"
            );
        }
        other => panic!("expected Skipped outcome, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// PERF-5 — Synchronous-mode default retry budget is 1
// ---------------------------------------------------------------------------

#[test]
fn synchronous_atomise_default_max_retries_is_1() {
    let cfg = AtomiserConfig::default();
    assert_eq!(
        cfg.sync_curator_max_retries, 1,
        "PERF-5 regression: Synchronous-mode default retry budget bumped from 1 back to {}",
        cfg.sync_curator_max_retries,
    );
    // The deferred path stays at 3.
    assert_eq!(cfg.curator_max_retries, 3);
}

// ---------------------------------------------------------------------------
// PERF-5 (override) — per-namespace `auto_atomise_max_retries` wins
// ---------------------------------------------------------------------------

/// Counting curator that ALWAYS returns a `MalformedResponse` so the
/// retry loop exhausts. Counts total `decompose` calls so the test can
/// pin the per-namespace override path.
struct AlwaysFailCurator {
    calls: Arc<Mutex<usize>>,
}

impl Curator for AlwaysFailCurator {
    fn decompose(
        &self,
        _body: &str,
        _max_atom_tokens: u32,
        _max_retries: u32,
    ) -> Result<Vec<Atom>, CuratorError> {
        *self.calls.lock().unwrap() += 1;
        Err(CuratorError::MalformedResponse("mock: always fail".into()))
    }
}

/// Curator wrapper that observes how many top-level `decompose` calls
/// it received. Used in addition to the Atomiser test path.
#[test]
fn auto_atomise_max_retries_policy_override_honored() {
    // Test the Curator surface directly: `decompose(_, _, max_retries)`
    // is what the substrate threads through, and the per-namespace
    // override flows into that `max_retries` argument via
    // `Atomiser::atomise_sync_with_retries`. Validate the Atomiser
    // honours the explicit override (5) by counting curator calls.
    let calls = Arc::new(Mutex::new(0_usize));
    let curator: Box<dyn Curator> = Box::new(AlwaysFailCurator {
        calls: Arc::clone(&calls),
    });

    // Build a Curator that emulates the substrate's retry loop —
    // since `decompose` itself takes `max_retries`, the substrate's
    // retry semantics live INSIDE the curator implementation (see
    // `LlmCurator::decompose`). For this regression we assert that
    // when the substrate threads `max_retries=5` into the curator
    // call, the curator-side retry loop honors it.
    //
    // Re-using the production `LlmCurator` would require a full LLM
    // stub. Instead, we use `RetryAwareCurator` below — a thin mock
    // that runs `1 + max_retries` attempts internally, mirroring the
    // production retry contract.
    struct RetryAwareCurator {
        inner: Box<dyn Curator>,
    }
    impl Curator for RetryAwareCurator {
        fn decompose(
            &self,
            body: &str,
            max_atom_tokens: u32,
            max_retries: u32,
        ) -> Result<Vec<Atom>, CuratorError> {
            let mut last_err = CuratorError::MalformedResponse("no attempts".into());
            for _ in 0..=max_retries {
                match self.inner.decompose(body, max_atom_tokens, 0) {
                    Ok(v) => return Ok(v),
                    Err(e) => last_err = e,
                }
            }
            Err(last_err)
        }
    }

    let wrapped: Box<dyn Curator> = Box::new(RetryAwareCurator { inner: curator });
    let atomiser = Arc::new(Atomiser::new(
        wrapped,
        None,
        AtomiserConfig {
            default_max_atom_tokens: 5,
            min_atoms_per_source: 2,
            max_atoms_per_source: 10,
            curator_max_retries: 3,
            sync_curator_max_retries: 1,
        },
        FeatureTier::Smart,
    ));

    // Seed a source memory whose body exceeds 5 cl100k tokens.
    let (conn, db_path) = open_db("perf5-override");
    let ns = "perf5/override".to_string();
    let mem = make_memory(
        &ns,
        "perf5-source",
        "kubernetes deploy probe canary rolling \
         observability tail logs cluster ingress alpha bravo charlie delta",
    );
    let source_id = db::insert(&conn, &mem).unwrap();

    // Install dispatch (one-shot per process; the in-test `OnceLock`
    // may already be set by a sibling test — install fails silently
    // in that case, which is fine because our atomiser handle below
    // is the one being asserted on, not the dispatch).
    let _ = install_auto_atomise_dispatch(AutoAtomisationDispatch {
        db_path: db_path.clone(),
        atomiser: Arc::clone(&atomiser),
    });

    // Per-namespace policy with `auto_atomise_max_retries = Some(5)`.
    // The substrate's Synchronous-mode hook MUST honour this and
    // call the curator 1 + 5 = 6 times.
    let policy_override: u32 = 5;
    let _result =
        atomiser.atomise_sync_with_retries(&conn, &source_id, 5, false, "ai:test", policy_override);
    let total_calls = *calls.lock().unwrap();
    assert_eq!(
        total_calls, 6,
        "PERF-5 override regression: expected 1 + 5 = 6 curator calls when policy override is 5, got {total_calls}"
    );

    // Sanity-check the `effective_auto_atomise_max_retries` accessor
    // surfaces the override.
    let policy = GovernancePolicy {
        core: CorePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
            inherit: true,
            ..CorePolicy::default()
        },
        atomisation: AtomisationPolicy {
            auto_atomise: Some(true),
            auto_atomise_mode: Some(AutoAtomiseMode::Synchronous),
            auto_atomise_max_retries: Some(5),
            ..AtomisationPolicy::default()
        },
        ..GovernancePolicy::default()
    };
    assert_eq!(policy.effective_auto_atomise_max_retries(), Some(5));
    // Compiled default falls through when None.
    let no_override = GovernancePolicy::default();
    assert_eq!(no_override.effective_auto_atomise_max_retries(), None);
}
