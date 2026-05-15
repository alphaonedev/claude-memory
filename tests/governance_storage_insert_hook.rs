// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L1-6 Deliverable E — substrate `storage::insert` governance
//! pre-write hook integration tests (issue #691).
//!
//! The hook ([`ai_memory::storage::GOVERNANCE_PRE_WRITE`]) is a
//! process-wide `OnceLock<Box<Fn>>` consulted by every
//! `storage::insert*` callsite BEFORE the SQL `INSERT`. Refusal
//! returns `MemoryError::RefusedByGovernance(reason)` with no row
//! written.
//!
//! ## Test architecture under the `OnceLock` constraint
//!
//! A `OnceLock` can only be `.set` once for the process lifetime.
//! The in-process tests in this file therefore install a SINGLE
//! "dispatcher" closure that reads its verdict from a process-wide
//! `Mutex<HookMode>`; each test acquires a serializing mutex, sets
//! the desired mode, and exercises the path. Test 5 spawns the
//! `ai-memory` binary as a subprocess so it gets its own fresh
//! process (no shared `OnceLock`).
//!
//! ## Audit-clean property
//!
//! 1. `hook_set_to_allow_lets_write_through` — Allow verdict → insert
//!    succeeds and the row lands.
//! 2. `hook_set_to_refuse_returns_typed_error` — Refuse verdict →
//!    `MemoryError::RefusedByGovernance(reason)` AND no row written.
//! 3. `hook_refusal_propagates_via_anyhow_downcast` — pre-write hook
//!    refusal wraps in `anyhow::Error` and survives the
//!    `MemoryError::from(anyhow::Error)` conversion at the handler
//!    boundary.
//! 4. `refusal_maps_to_http_403` — drives a real `ai-memory serve`
//!    subprocess with a pre-seeded operator-signed refuse rule on
//!    `Custom { custom_kind = "memory_write" }`; POST
//!    `/api/v1/memories` to a matching namespace returns 403 with
//!    code `GOVERNANCE_REFUSED`.
//! 5. `cli_one_shot_does_not_install_hook` — spawns `ai-memory store`
//!    with a refuse rule already seeded in the DB and asserts the
//!    write succeeds (CLI direct ops are intentionally outside the
//!    hook's scope).

use std::sync::{Mutex, OnceLock};

use ai_memory::db;
use ai_memory::errors::MemoryError;
use ai_memory::models::{Memory, MemoryKind, Tier};
use ai_memory::storage::{self, GovernanceRefusal};

// ---------------------------------------------------------------------------
// Process-wide hook dispatcher (OnceLock workaround)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum HookMode {
    Allow,
    Refuse(String),
}

static HOOK_MODE: OnceLock<Mutex<HookMode>> = OnceLock::new();
static HOOK_FIRE_COUNT: OnceLock<std::sync::atomic::AtomicU64> = OnceLock::new();

fn hook_mode_slot() -> &'static Mutex<HookMode> {
    HOOK_MODE.get_or_init(|| Mutex::new(HookMode::Allow))
}

fn hook_fire_count() -> &'static std::sync::atomic::AtomicU64 {
    HOOK_FIRE_COUNT.get_or_init(|| std::sync::atomic::AtomicU64::new(0))
}

/// Per-test serialization mutex. Every in-process test in this file
/// must `let _g = test_serial().lock().unwrap();` at entry so the
/// shared `HOOK_MODE` state can't race across parallel test runners.
fn test_serial() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

/// Install the in-process dispatcher exactly once. Idempotent —
/// every test calls this; only the first call actually installs.
/// Subsequent calls observe the OnceLock-already-set state and
/// proceed; the dispatcher closure picks up the per-test mode via
/// `hook_mode_slot()`.
fn ensure_hook_installed() {
    let _ = storage::GOVERNANCE_PRE_WRITE.set(Box::new(|_mem: &Memory| {
        hook_fire_count().fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let guard = hook_mode_slot().lock().expect("hook mode mutex poisoned");
        match &*guard {
            HookMode::Allow => Ok(()),
            HookMode::Refuse(reason) => Err(reason.clone()),
        }
    }));
}

fn set_mode(mode: HookMode) {
    *hook_mode_slot().lock().expect("hook mode mutex poisoned") = mode;
}

// ---------------------------------------------------------------------------
// In-process helpers
// ---------------------------------------------------------------------------

fn fresh_conn() -> rusqlite::Connection {
    db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

fn fresh_memory(title: &str, ns: &str) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: ns.to_string(),
        title: title.to_string(),
        content: "test content".to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    }
}

// ---------------------------------------------------------------------------
// Test 1: Allow verdict → insert succeeds, row lands
// ---------------------------------------------------------------------------

#[test]
fn hook_set_to_allow_lets_write_through() {
    let _g = test_serial().lock().unwrap();
    ensure_hook_installed();
    set_mode(HookMode::Allow);

    let conn = fresh_conn();
    let mem = fresh_memory("allow path", "test/allow");
    let before = hook_fire_count().load(std::sync::atomic::Ordering::SeqCst);

    let id = db::insert(&conn, &mem).expect("Allow verdict must not refuse");

    let after = hook_fire_count().load(std::sync::atomic::Ordering::SeqCst);
    assert!(after > before, "hook must fire on every insert");

    // Row landed under the returned id.
    let row = db::get(&conn, &id).unwrap().expect("row must exist");
    assert_eq!(row.title, "allow path");
    assert_eq!(row.namespace, "test/allow");
}

// ---------------------------------------------------------------------------
// Test 2: Refuse verdict → typed error, no row written
// ---------------------------------------------------------------------------

#[test]
fn hook_set_to_refuse_returns_typed_error() {
    let _g = test_serial().lock().unwrap();
    ensure_hook_installed();
    set_mode(HookMode::Refuse("test refusal".to_string()));

    let conn = fresh_conn();
    let mem = fresh_memory("refused path", "test/refuse");

    let err = db::insert(&conn, &mem).expect_err("Refuse verdict must surface as Err");

    // Downcast through the anyhow chain.
    let refusal = err
        .downcast_ref::<GovernanceRefusal>()
        .expect("must wrap GovernanceRefusal");
    assert_eq!(refusal.reason, "test refusal");

    // Transaction-clean refusal: no row written.
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace = 'test/refuse'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "refusal must not leave a row behind");

    // Restore Allow for subsequent tests.
    set_mode(HookMode::Allow);
}

// ---------------------------------------------------------------------------
// Test 3: Anyhow-chain round-trips to MemoryError::RefusedByGovernance
// ---------------------------------------------------------------------------

#[test]
fn hook_refusal_propagates_via_anyhow_downcast() {
    let _g = test_serial().lock().unwrap();
    ensure_hook_installed();
    set_mode(HookMode::Refuse("downcast-test".to_string()));

    let conn = fresh_conn();
    let mem = fresh_memory("downcast", "test/downcast");

    let err = db::insert(&conn, &mem).expect_err("refuse must error");
    let mapped: MemoryError = err.into();
    match mapped {
        MemoryError::RefusedByGovernance(r) => assert_eq!(r, "downcast-test"),
        other => panic!("expected RefusedByGovernance, got {other:?}"),
    }

    set_mode(HookMode::Allow);
}

// ---------------------------------------------------------------------------
// Test 4: Refuse path applies to insert_with_conflict (Error) +
//         insert_if_newer (federation path) too
// ---------------------------------------------------------------------------

#[test]
fn hook_gates_all_three_insert_paths() {
    let _g = test_serial().lock().unwrap();
    ensure_hook_installed();
    set_mode(HookMode::Refuse("blanket refusal".to_string()));

    let conn = fresh_conn();
    let mem = fresh_memory("blanket", "test/blanket");

    let e1 = db::insert(&conn, &mem).expect_err("insert must refuse");
    assert!(e1.downcast_ref::<GovernanceRefusal>().is_some());

    let e2 = db::insert_with_conflict(&conn, &mem, storage::ConflictMode::Error)
        .expect_err("insert_with_conflict(Error) must refuse");
    assert!(e2.downcast_ref::<GovernanceRefusal>().is_some());

    let e3 = db::insert_with_conflict(&conn, &mem, storage::ConflictMode::Merge)
        .expect_err("insert_with_conflict(Merge) must refuse");
    assert!(e3.downcast_ref::<GovernanceRefusal>().is_some());

    let e4 = db::insert_if_newer(&conn, &mem).expect_err("insert_if_newer must refuse");
    assert!(e4.downcast_ref::<GovernanceRefusal>().is_some());

    // No rows landed on any path.
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "no insert path may have written a row");

    set_mode(HookMode::Allow);
}

// ---------------------------------------------------------------------------
// Test 5: CLI one-shot does NOT install the hook
//
// Spawns the `ai-memory store` binary against a fresh DB seeded with
// a (substrate-internal-perspective) refuse rule. The CLI invocation
// must succeed because the hook is only installed in the daemon's
// `serve` boot path. The substrate rules table is consulted only by
// the in-process hook closure; CLI never installs the closure, so
// CLI writes proceed unconditionally — which is the operator
// standing-directive contract.
// ---------------------------------------------------------------------------

#[test]
fn cli_one_shot_does_not_install_hook() {
    // Use TMPDIR-honoring temp dir (project hard rule: no /tmp writes
    // by name; std::env::temp_dir() honors the export TMPDIR set at
    // session bootstrap, which lands under .local-runs/tmp).
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-l16e-cli-{}.db", uuid::Uuid::new_v4()));
    let bin = env!("CARGO_BIN_EXE_ai-memory");

    // Seed a refuse rule into the DB BEFORE the CLI runs. We open
    // the DB to materialize the schema, then INSERT a governance_rules
    // row directly. If the daemon hook had been installed in the CLI
    // path, the subsequent `store` would refuse; instead it must
    // succeed (CLI is intentionally outside the hook's scope).
    {
        let conn = db::open(&db_path).expect("open seed db");
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT INTO governance_rules \
             (id, kind, matcher, severity, reason, namespace, created_by, \
              created_at, enabled, signature, attest_level) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10)",
            rusqlite::params![
                "R-cli-test",
                "custom",
                r#"{"kind":"memory_write"}"#,
                "refuse",
                "CLI must NOT see this refusal",
                "_global",
                "test",
                now,
                1,
                "unsigned",
            ],
        )
        .expect("seed rule");
    }

    // Now run `ai-memory store` against the seeded DB.
    let output = std::process::Command::new(bin)
        .env("AI_MEMORY_NO_CONFIG", "1")
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "--tier",
            "long",
            "--namespace",
            "cli-test-namespace",
            "--title",
            "cli-store-allowed",
            "--content",
            "cli writes must not consult the daemon hook",
        ])
        .output()
        .expect("spawn ai-memory");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "CLI store must succeed despite a refuse rule in the DB \
         (the daemon hook is intentionally NOT installed in CLI mode); \
         stdout={stdout} stderr={stderr}"
    );
    assert!(
        !stderr.to_lowercase().contains("governance-refused")
            && !stderr.to_lowercase().contains("governance_refused"),
        "CLI must NOT surface a governance refusal; stderr={stderr}"
    );

    // Verify the row actually landed.
    {
        let conn = db::open(&db_path).expect("reopen db");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE namespace = ?1",
                rusqlite::params!["cli-test-namespace"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "CLI-stored row must be present");
    }

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", db_path.display()));
}

// ---------------------------------------------------------------------------
// Test 6: HTTP daemon → 403 GOVERNANCE_REFUSED end-to-end
//
// Spawns `ai-memory serve` against a fresh DB seeded with a refuse
// rule on `Custom { kind = "memory_write" }`. POST a memory; the
// substrate's installed hook short-circuits with a refusal which the
// HTTP handler maps to 403 / `GOVERNANCE_REFUSED`.
// ---------------------------------------------------------------------------

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

fn wait_for_health(port: u16) -> bool {
    for _ in 0..100 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(out) = std::process::Command::new("curl")
            .args([
                "-s",
                "-o",
                "/dev/null",
                "-w",
                "%{http_code}",
                &format!("http://127.0.0.1:{port}/api/v1/health"),
            ])
            .output()
            && String::from_utf8_lossy(&out.stdout) == "200"
        {
            return true;
        }
    }
    false
}

#[test]
fn refusal_maps_to_http_403() {
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-l16e-http-{}.db", uuid::Uuid::new_v4()));
    let bin = env!("CARGO_BIN_EXE_ai-memory");

    // Seed a refuse rule into the DB. The daemon hook (installed in
    // bootstrap_serve) will consult this row on every storage::insert.
    {
        let conn = db::open(&db_path).expect("open seed db");
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT INTO governance_rules \
             (id, kind, matcher, severity, reason, namespace, created_by, \
              created_at, enabled, signature, attest_level) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10)",
            rusqlite::params![
                "R-http-test",
                "custom",
                r#"{"kind":"memory_write"}"#,
                "refuse",
                "L1-6 HTTP test: refused by substrate governance",
                "_global",
                "test",
                now,
                1,
                "unsigned",
            ],
        )
        .expect("seed rule");
    }

    let port = free_port();
    let mut child = std::process::Command::new(bin)
        .env("AI_MEMORY_NO_CONFIG", "1")
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "serve",
            "--port",
            &port.to_string(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn ai-memory serve");

    let healthy = wait_for_health(port);
    if !healthy {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&db_path);
        panic!("serve never came healthy");
    }

    // POST a memory — must come back 403.
    let body = serde_json::json!({
        "tier": "long",
        "namespace": "blocked-by-l16e",
        "title": "should not land",
        "content": "if you see this row in the DB the hook leaked",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {},
    })
    .to_string();
    // Project hard rule: no agent-created files under /tmp / /private/tmp / etc.
    // We capture curl's body via stdout (the `STATUS:` separator is
    // parsed below) rather than `-o <path>` — keeps the test free of
    // any tmpfs-side-effect.
    let curl_with_body = std::process::Command::new("curl")
        .args([
            "-s",
            "-w",
            "\nSTATUS:%{http_code}",
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "-d",
            &body,
            &format!("http://127.0.0.1:{port}/api/v1/memories"),
        ])
        .output()
        .expect("curl");

    let _ = child.kill();
    let _ = child.wait();

    let resp = String::from_utf8_lossy(&curl_with_body.stdout);
    assert!(
        resp.contains("STATUS:403"),
        "expected HTTP 403 from refusal, got: {resp}"
    );
    assert!(
        resp.to_uppercase().contains("GOVERNANCE_REFUSED"),
        "expected code GOVERNANCE_REFUSED in body, got: {resp}"
    );

    // Defence-in-depth: the row must NOT have landed.
    {
        let conn = db::open(&db_path).expect("reopen db");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE namespace = 'blocked-by-l16e'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "refusal must not have written a row");
    }

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", db_path.display()));
}
