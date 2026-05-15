// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.x Form 2 acceptance tests (issue #755) — synchronous
//! atomise-before-embed mode.
//!
//! Three tests cover the three `AutoAtomiseMode` variants:
//!
//! * Synchronous — source memory exists archived with
//!   `atomised_into > 0` BEFORE `memory_store` returns; atoms are
//!   queryable via FTS5 immediately.
//! * Deferred — existing WT-1-D behaviour preserved (atomiser runs
//!   on the worker thread; the source has `atomised_into = NULL`
//!   immediately after the response returns).
//! * Off — no atomisation occurs at all.

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
    clippy::redundant_closure_for_method_calls
)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use ai_memory::atomisation::curator::{Atom, Curator, CuratorError};
use ai_memory::atomisation::{Atomiser, AtomiserConfig};
use ai_memory::config::{FeatureTier, ResolvedTtl};
use ai_memory::hooks::pre_store::{AutoAtomisationDispatch, install_auto_atomise_dispatch};
use ai_memory::models::{
    ApproverType, AutoAtomiseMode, GovernanceLevel, GovernancePolicy, Memory, Tier,
};
use ai_memory::storage as db;

use chrono::Utc;
use rusqlite::Connection;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// MockCurator — programmable deterministic.
// ---------------------------------------------------------------------------

struct MockCurator {
    state: Arc<Mutex<MockState>>,
}

struct MockState {
    responses: Vec<Result<Vec<Atom>, CuratorError>>,
    calls: usize,
}

impl Curator for MockCurator {
    fn decompose(
        &self,
        _body: &str,
        _max_atom_tokens: u32,
        _max_retries: u32,
    ) -> Result<Vec<Atom>, CuratorError> {
        let mut s = self.state.lock().unwrap();
        s.calls += 1;
        if s.responses.is_empty() {
            return Err(CuratorError::MalformedResponse(
                "mock: response queue exhausted".into(),
            ));
        }
        s.responses.remove(0)
    }
}

fn shared_state() -> Arc<Mutex<MockState>> {
    static SLOT: OnceLock<Arc<Mutex<MockState>>> = OnceLock::new();
    SLOT.get_or_init(|| {
        Arc::new(Mutex::new(MockState {
            responses: Vec::new(),
            calls: 0,
        }))
    })
    .clone()
}

fn enqueue_atoms(texts: &[&str]) {
    let arc = shared_state();
    let mut s = arc.lock().unwrap();
    s.responses.push(Ok(texts
        .iter()
        .map(|t| Atom {
            text: (*t).to_string(),
        })
        .collect()));
}

fn reset_mock() {
    let arc = shared_state();
    let mut s = arc.lock().unwrap();
    s.responses.clear();
    s.calls = 0;
}

fn mock_call_count() -> usize {
    let arc = shared_state();
    let n = arc.lock().unwrap().calls;
    n
}

// ---------------------------------------------------------------------------
// Shared DB path + dispatch slot (process-wide).
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

fn shared_db_path() -> &'static PathBuf {
    static SHARED: OnceLock<PathBuf> = OnceLock::new();
    SHARED.get_or_init(|| {
        let root = local_runs_root();
        std::fs::create_dir_all(&root).ok();
        root.join(format!("form-2-synchronous-{}.db", uuid::Uuid::new_v4()))
    })
}

fn ensure_dispatch_installed() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    let _ = INSTALLED.get_or_init(|| {
        let curator: Box<dyn Curator> = Box::new(MockCurator {
            state: shared_state(),
        });
        let atomiser = Arc::new(Atomiser::new(
            curator,
            None,
            AtomiserConfig::default(),
            FeatureTier::Smart,
        ));
        let dispatch = AutoAtomisationDispatch {
            db_path: shared_db_path().clone(),
            atomiser,
        };
        let _ = install_auto_atomise_dispatch(dispatch);
    });
}

fn test_serial() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

fn open_shared_db() -> Connection {
    db::open(shared_db_path()).expect("open shared db")
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn seed_policy(conn: &Connection, ns: &str, policy: GovernancePolicy) {
    let now = Utc::now().to_rfc3339();
    let gov_metadata = json!({
        "agent_id": "ai:test",
        "governance": serde_json::to_value(&policy).unwrap(),
    });
    let std_mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: ns.to_string(),
        title: format!("__standard_{ns}_{}", uuid::Uuid::new_v4().simple()),
        content: "standard".into(),
        created_at: now.clone(),
        updated_at: now,
        metadata: gov_metadata,
        ..Default::default()
    };
    let std_id = db::insert(conn, &std_mem).expect("seed standard");
    db::set_namespace_standard(conn, ns, &std_id, None).expect("set standard");
}

fn make_policy(mode: Option<AutoAtomiseMode>, enable_legacy_flag: bool) -> GovernancePolicy {
    GovernancePolicy {
        write: GovernanceLevel::Any,
        promote: GovernanceLevel::Any,
        delete: GovernanceLevel::Owner,
        approver: ApproverType::Human,
        inherit: true,
        max_reflection_depth: None,
        auto_export_reflections_to_filesystem: None,
        // The deferred path still keys off auto_atomise = true to fire;
        // the synchronous path keys off auto_atomise_mode = Synchronous
        // directly.
        auto_atomise: if enable_legacy_flag { Some(true) } else { None },
        auto_atomise_threshold_cl100k: Some(20),
        auto_atomise_max_atom_tokens: Some(50),
        auto_persona_trigger_every_n_memories: None,
        auto_export_personas_to_filesystem: None,
        auto_atomise_mode: mode,
        legacy_per_pair_classifier: None,
        auto_classify_kind: None,
    }
}

fn long_body() -> String {
    let unit = "The kubernetes rolling deploy strategy required canary instance health checks. \
                The pod readiness probe must pass before traffic shifts. Failures roll back the \
                deployment within 30 seconds. ";
    unit.repeat(8)
}

fn store_through_mcp(conn: &Connection, db_path: &std::path::Path, ns: &str) -> Value {
    let ttl = ResolvedTtl::default();
    ai_memory::mcp::tools::handle_store_for_tests(
        conn,
        db_path,
        &json!({
            "title": format!("form-2-{}-{}", ns, uuid::Uuid::new_v4()),
            "content": long_body(),
            "namespace": ns,
        }),
        None,
        None,
        None,
        &ttl,
        false,
        None,
        None,
    )
    .expect("memory_store ok")
}

fn read_atomised_into(conn: &Connection, id: &str) -> Option<i64> {
    conn.query_row(
        "SELECT atomised_into FROM memories WHERE id = ?1",
        [id],
        |r| r.get::<_, Option<i64>>(0),
    )
    .ok()
    .flatten()
}

fn count_atoms_for_source(conn: &Connection, source_id: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE atom_of = ?1",
        [source_id],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Test 1: Synchronous mode — source archived BEFORE response returns;
// atoms queryable immediately.
// ---------------------------------------------------------------------------

#[test]
fn synchronous_mode_archives_source_and_atoms_visible_before_response() {
    let _guard = test_serial().lock().unwrap_or_else(|e| e.into_inner());
    ensure_dispatch_installed();
    reset_mock();
    enqueue_atoms(&[
        "Canary instance health checks must pass.",
        "Failures roll back within 30 seconds.",
    ]);

    let conn = open_shared_db();
    let ns = format!("sync-ns-{}", uuid::Uuid::new_v4().simple());
    seed_policy(
        &conn,
        &ns,
        make_policy(Some(AutoAtomiseMode::Synchronous), false),
    );

    let resp = store_through_mcp(&conn, shared_db_path(), &ns);
    let source_id = resp["id"].as_str().expect("response id").to_string();

    // Form 2 hard guarantee: source's atomised_into > 0 BEFORE the
    // response handler returned to us.
    let atomised_into = read_atomised_into(&conn, &source_id);
    assert!(
        atomised_into.is_some_and(|n| n > 0),
        "source must be archived with atomised_into > 0 synchronously, got: {atomised_into:?}",
    );

    // Atoms exist with atom_of pointing at the source.
    let atom_count = count_atoms_for_source(&conn, &source_id);
    assert_eq!(atom_count, 2, "two atoms emitted by mock curator");

    // The response carries the synchronous-mode marker.
    assert_eq!(resp["atomise_mode"].as_str(), Some("synchronous"));
    assert_eq!(resp["atomise_outcome"].as_str(), Some("atomised"));

    // Curator was called exactly once (synchronous, no deferred replay).
    assert_eq!(
        mock_call_count(),
        1,
        "synchronous mode: exactly one curator call"
    );
}

// ---------------------------------------------------------------------------
// Test 2: Deferred mode — existing WT-1-D behaviour preserved.
// `atomised_into` is NULL right after the response returns; the worker
// thread completes asynchronously.
// ---------------------------------------------------------------------------

#[test]
fn deferred_mode_preserves_existing_behaviour() {
    let _guard = test_serial().lock().unwrap_or_else(|e| e.into_inner());
    ensure_dispatch_installed();
    reset_mock();
    // Don't enqueue atoms: the deferred path consumes the queue async;
    // for this test we only need to verify the SYNC side-effect path
    // did NOT fire. Pushing an empty Ok will let the worker thread
    // succeed in the background without affecting the synchronous
    // observation.
    enqueue_atoms(&["a1", "a2"]);

    let conn = open_shared_db();
    let ns = format!("deferred-ns-{}", uuid::Uuid::new_v4().simple());
    // Deferred mode is enabled when auto_atomise_mode=Deferred OR
    // (auto_atomise=true AND auto_atomise_mode=None) per the
    // resolve table. We test the explicit-Deferred form here.
    seed_policy(
        &conn,
        &ns,
        make_policy(Some(AutoAtomiseMode::Deferred), false),
    );

    let resp = store_through_mcp(&conn, shared_db_path(), &ns);
    let source_id = resp["id"].as_str().expect("response id").to_string();

    // Form 2 deferred-mode contract: source's atomised_into is NULL at
    // the moment the response returned. The worker thread may complete
    // it later — that is the deferred semantic.
    let atomised_into_immediate = read_atomised_into(&conn, &source_id);
    assert!(
        atomised_into_immediate.is_none(),
        "deferred mode: atomised_into must be NULL right after store returns, got: {atomised_into_immediate:?}",
    );

    // The response does NOT carry the synchronous-mode marker.
    assert!(resp.get("atomise_mode").is_none() || resp["atomise_mode"] != "synchronous");
}

// ---------------------------------------------------------------------------
// Test 3: Off mode — no atomisation occurs at all.
// ---------------------------------------------------------------------------

#[test]
fn off_mode_skips_atomisation_entirely() {
    let _guard = test_serial().lock().unwrap_or_else(|e| e.into_inner());
    ensure_dispatch_installed();
    reset_mock();
    let calls_before = mock_call_count();

    let conn = open_shared_db();
    let ns = format!("off-ns-{}", uuid::Uuid::new_v4().simple());
    seed_policy(&conn, &ns, make_policy(Some(AutoAtomiseMode::Off), false));

    let resp = store_through_mcp(&conn, shared_db_path(), &ns);
    let source_id = resp["id"].as_str().expect("response id").to_string();

    // Source is NOT archived — atomised_into stays NULL.
    let atomised_into = read_atomised_into(&conn, &source_id);
    assert!(
        atomised_into.is_none(),
        "Off mode: atomised_into must remain NULL, got: {atomised_into:?}",
    );
    let atom_count = count_atoms_for_source(&conn, &source_id);
    assert_eq!(atom_count, 0, "Off mode: zero atoms");

    // No curator call landed synchronously.
    assert_eq!(
        mock_call_count(),
        calls_before,
        "Off mode: curator must not be called",
    );

    // Response must not carry the synchronous-mode marker either.
    assert!(resp.get("atomise_mode").is_none() || resp["atomise_mode"] != "synchronous");
}
