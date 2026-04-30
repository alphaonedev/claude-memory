// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #487 PR-3 — boot lifecycle tests.
//!
//! These tests prove `ai-memory boot` survives the failure modes that
//! actually wedge AI agents in the wild:
//!
//! 1. **Schema migration on first boot after upgrade.** A user upgrading
//!    from v0.6.3.0 → v0.6.3.1 has a v17 schema; their first session boot
//!    must trigger v17→v18→v19 cleanly and still return the seeded
//!    memories.
//! 2. **Corrupted DB.** Disk error, partial write, malware quarantine —
//!    boot must exit 0 with a `warn` status rather than crashing the
//!    agent's first turn.
//! 3. **Concurrent writer.** A long-running curator daemon can hold a
//!    write transaction; boot must complete within a sane wall-clock
//!    bound (the indexed list path doesn't block on writers).
//!
//! Cross-platform: every path uses `tempfile`, every assertion is over
//! text. Windows-runner-safe.

use ai_memory::{db, models};
use assert_cmd::Command;
use chrono::Utc;
use rusqlite::params;
use std::path::Path;
use tempfile::TempDir;

fn ai_memory(db: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ai-memory").unwrap();
    cmd.env("AI_MEMORY_NO_CONFIG", "1")
        .args(["--db", db.to_str().unwrap()]);
    cmd
}

fn seed_one(db: &Path, namespace: &str, title: &str, content: &str) -> String {
    let conn = db::open(db).unwrap();
    let now = Utc::now().to_rfc3339();
    let mut metadata = models::default_metadata();
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String("lifecycle-test".to_string()),
        );
    }
    let mem = models::Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: models::Tier::Long,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "import".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata,
    };
    db::insert(&conn, &mem).expect("db::insert")
}

#[test]
fn boot_after_v18_to_v19_migration() {
    // Simulate a v0.6.3.0 install: open the DB at the current version,
    // seed a memory, then forcibly roll `schema_version` back. The next
    // `db::open` (which is what `ai-memory boot` calls under the hood)
    // must re-run the v18→v19 migration block and still return the row.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("legacy.db");

    let id = seed_one(&db_path, "lifecycle-ns", "pre-migration-row", "x");
    {
        let conn = db::open(&db_path).unwrap();
        // Roll schema_version back so the next open() forces a re-migrate
        // through the most-recent migration block. v18 + v19 are the
        // current targets; rolling to 17 covers both.
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (17)", [])
            .unwrap();
    }

    // Now invoke the binary — boot calls db::open which runs migrate().
    let assert = ai_memory(&db_path)
        .args([
            "boot",
            "--namespace",
            "lifecycle-ns",
            "--limit",
            "5",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("expected JSON output post-migration; err={e}; got: {stdout}"));
    assert_eq!(parsed["status"], "ok");
    assert_eq!(parsed["count"].as_u64(), Some(1));
    let titles: Vec<String> = parsed["memories"]
        .as_array()
        .expect("memories array")
        .iter()
        .map(|m| m["title"].as_str().unwrap().to_string())
        .collect();
    assert!(
        titles.iter().any(|t| t == "pre-migration-row"),
        "expected seeded row to survive migration; got titles: {titles:?}"
    );

    // Verify the DB is now at the current schema version.
    let conn = db::open(&db_path).unwrap();
    let v: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        v >= 19,
        "expected schema_version >=19 post-migration, got {v}"
    );
    // And the seeded id round-trips.
    let exists: i64 = conn
        .query_row(
            "SELECT count(*) FROM memories WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(exists, 1, "seeded row missing post-migration");
}

#[test]
fn boot_after_db_corruption_recovery() {
    // Seed a real DB, then truncate / overwrite it with garbage. db::open
    // should fail; boot's contract is to surface a `warn` header (not
    // crash) so the agent's session still starts.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("corrupted.db");
    seed_one(&db_path, "ns-corrupt", "row", "x");

    // Corrupt the file: SQLite expects a magic header
    // ("SQLite format 3\0"). Overwriting with garbage breaks that, so any
    // open() returns an error.
    std::fs::write(&db_path, b"this is not a sqlite database file").unwrap();

    let assert = ai_memory(&db_path)
        .args(["boot", "--namespace", "ns-corrupt", "--quiet"])
        .assert()
        .success(); // Exit 0 — graceful degrade is the contract.
    let stdout = std::str::from_utf8(&assert.get_output().stdout).unwrap();
    assert!(
        stdout.starts_with("# ai-memory boot: warn"),
        "corrupted DB must yield warn header, got: {stdout}"
    );
    assert!(
        stdout.contains("db unavailable"),
        "warn header must mention db unavailable, got: {stdout}"
    );
}

#[test]
fn boot_with_concurrent_writer_does_not_block() {
    // Open a write transaction in this test process and hold it across
    // the boot subprocess invocation. The boot path is read-only and uses
    // the indexed `list` query (FTS5 not involved); under WAL mode,
    // readers don't block on writers. Bound: 5 seconds wall clock.
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("concurrent.db");
    seed_one(&db_path, "ns-concurrent", "row-one", "x");
    seed_one(&db_path, "ns-concurrent", "row-two", "y");

    // Take a writer connection and start an explicit IMMEDIATE
    // transaction so it holds a write lock under WAL mode.
    let writer = db::open(&db_path).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; INSERT INTO memories (id, tier, namespace, title, content, tags, priority, confidence, source, access_count, created_at, updated_at, last_accessed_at, expires_at, metadata) VALUES ('concurrent-test', 'mid', 'ns-concurrent', 'pending', 'pending', '[]', 5, 1.0, 'test', 0, datetime('now'), datetime('now'), NULL, NULL, '{}')")
        .unwrap();

    // Now spawn boot. It should complete within 5 s — the WAL reader
    // path does not block on the writer.
    let start = std::time::Instant::now();
    let assert = ai_memory(&db_path)
        .timeout(std::time::Duration::from_secs(5))
        .args([
            "boot",
            "--namespace",
            "ns-concurrent",
            "--limit",
            "5",
            "--format",
            "json",
        ])
        .assert()
        .success();
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_secs() < 5,
        "boot took {elapsed:?} with concurrent writer — should complete fast"
    );

    let stdout = std::str::from_utf8(&assert.get_output().stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    // Both rows committed before BEGIN IMMEDIATE should be visible. The
    // pending writer's row should NOT be — it's still in an uncommitted
    // transaction, so a separate connection can't see it.
    let count = parsed["memories"].as_array().expect("memories array").len();
    assert!(
        count >= 2,
        "expected >=2 already-committed rows; got {count}"
    );
    // Roll back the in-flight writer so the temp dir cleans up cleanly.
    writer.execute_batch("ROLLBACK").unwrap();
}
