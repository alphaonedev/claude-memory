// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Connection setup for the SQLite substrate. v0.7.0 L0.5-3 extracted
//! `open` + the SQLCipher passphrase helper out of `src/db.rs` into
//! this sub-module. Pure refactor — semantics unchanged.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

use super::migrations::{SCHEMA, migrate};

/// v0.7.0 fix campaign R1-M2 (#690) — defense-in-depth CHECK
/// constraints applied as `CREATE TRIGGER IF NOT EXISTS` statements
/// after the schema-version migration ladder runs. Sourced from
/// `migrations/sqlite/0023_v07_check_constraints.sql`.
///
/// We deliberately apply this OUTSIDE the version-bumped migration
/// ladder in [`super::migrations::migrate`] because that file is owned
/// by a parallel L0.7-2 stream during the v0.7.0 fix campaign. Running
/// the triggers from here keeps the substrate guard in place without
/// requiring a coordinated `CURRENT_SCHEMA_VERSION` bump. Both the
/// triggers and the surrounding bootstrap are idempotent — re-running
/// `open` (which happens on every fresh `db::open` call) is a no-op
/// after the first apply.
const CHECK_CONSTRAINT_TRIGGERS_SQLITE: &str =
    include_str!("../../migrations/sqlite/0023_v07_check_constraints.sql");

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path).context("failed to open database")?;
    apply_sqlcipher_key(&conn)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(SCHEMA)
        .context("failed to initialize schema")?;
    migrate(&conn)?;
    apply_check_constraint_triggers(&conn)
        .context("failed to apply R1-M2 CHECK-constraint triggers")?;
    Ok(conn)
}

/// Apply the defense-in-depth CHECK triggers from migration 0023.
///
/// `CREATE TRIGGER IF NOT EXISTS` is idempotent — re-running is a
/// no-op. We detect whether the triggers are already installed via a
/// single read against `sqlite_master` and skip the install entirely
/// when they exist; this keeps `db::open` lock-free on every-call-
/// after-the-first and avoids contending with concurrent writers on
/// startup (a 5-second-bounded boot path can't afford to wait on a
/// `BEGIN EXCLUSIVE` against a held writer transaction).
///
/// On the first install, we DO wrap the batch in `BEGIN IMMEDIATE`
/// (not `EXCLUSIVE`) so two parallel `open()` calls race deterministically
/// rather than dead-locking. Pre-existing rows that violate any of
/// the constraints are NOT migrated away (silent data loss is worse
/// than a known-violating row); we instead emit a `tracing::warn!`
/// count of violators so operators can surface them in their next
/// cleanup pass.
fn apply_check_constraint_triggers(conn: &Connection) -> Result<()> {
    // Cheap idempotency probe — if our sentinel trigger is present,
    // the migration already ran on this database. Pure read against
    // `sqlite_master`, no lock acquired.
    let already_installed: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master \
             WHERE type = 'trigger' AND name = 'memories_ck_tier_ins')",
            [],
            |r| r.get::<_, i64>(0).map(|n| n != 0),
        )
        .unwrap_or(false);
    if already_installed {
        return Ok(());
    }

    // Pre-flight: count any rows that violate the upcoming constraints.
    // Surfaces a loud warning rather than silently dropping bad data.
    // Each query is best-effort — a missing column (very old schema)
    // returns zero rather than failing the open() path.
    let count_violations =
        |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0) };
    let bad_tier = count_violations(
        "SELECT COUNT(*) FROM memories WHERE tier NOT IN ('short', 'mid', 'long')",
    );
    let bad_priority =
        count_violations("SELECT COUNT(*) FROM memories WHERE priority < 1 OR priority > 10");
    let bad_confidence = count_violations(
        "SELECT COUNT(*) FROM memories WHERE confidence < 0.0 OR confidence > 1.0",
    );
    let bad_relation = count_violations(
        "SELECT COUNT(*) FROM memory_links \
         WHERE relation NOT IN ('related_to', 'supersedes', 'contradicts', 'derived_from', 'reflects_on', 'derives_from')",
    );
    let bad_attest = count_violations(
        "SELECT COUNT(*) FROM memory_links \
         WHERE attest_level IS NOT NULL \
           AND attest_level NOT IN ('unsigned', 'self_signed', 'peer_attested')",
    );
    let total_bad = bad_tier + bad_priority + bad_confidence + bad_relation + bad_attest;
    if total_bad > 0 {
        tracing::warn!(
            target: "ai_memory::storage::checks",
            "R1-M2 CHECK trigger install: \
             pre-existing constraint violations detected — \
             memories.tier={bad_tier}, memories.priority={bad_priority}, \
             memories.confidence={bad_confidence}, \
             memory_links.relation={bad_relation}, \
             memory_links.attest_level={bad_attest}. \
             Triggers will still install; future writes that touch these \
             rows will fail loudly until the values are repaired."
        );
    }

    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        conn.execute_batch(CHECK_CONSTRAINT_TRIGGERS_SQLITE)
            .context("apply CHECK-constraint triggers")?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// v0.6.0.0 — apply the SQLCipher passphrase (PRAGMA key) when the
/// `sqlcipher` cargo feature is built-in AND a passphrase has been
/// provided via `AI_MEMORY_DB_PASSPHRASE` env var. The recommended
/// way to set the env var is via the `--db-passphrase-file <path>`
/// CLI flag, which reads the passphrase from a root-readable file
/// and exports the env for the daemon's lifetime only. Passing the
/// passphrase directly as an env var works but leaks to the process
/// list (`ps -E`, `/proc/<pid>/environ`).
///
/// When the `sqlcipher` feature is NOT enabled, this function is a
/// no-op — standard SQLite has no `PRAGMA key` so setting one errors.
#[cfg(feature = "sqlcipher")]
fn apply_sqlcipher_key(conn: &Connection) -> Result<()> {
    let Ok(passphrase) = std::env::var("AI_MEMORY_DB_PASSPHRASE") else {
        anyhow::bail!(
            "sqlcipher build requires AI_MEMORY_DB_PASSPHRASE (set via --db-passphrase-file <path>)"
        );
    };
    // PRAGMA key must be the FIRST operation on a new connection. The
    // passphrase is quoted with SQL string-literal quoting rules.
    let escaped = passphrase.replace('\'', "''");
    conn.pragma_update(None, "key", format!("'{escaped}'"))
        .context("PRAGMA key failed (wrong passphrase or unencrypted DB?)")?;
    // Verify the key opened the database by running a cheap query.
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })
    .context("SQLCipher unlock verification failed — wrong passphrase?")?;
    Ok(())
}

#[cfg(not(feature = "sqlcipher"))]
#[allow(clippy::unnecessary_wraps)]
fn apply_sqlcipher_key(_conn: &Connection) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// L0.7-6 Tier E unit coverage. The `open` path is already exercised through
// every db-related integration test; these tests pin the idempotency probe
// for the R1-M2 CHECK trigger install and the sqlcipher no-op fall-through.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_round_trip_creates_db_and_runs_migrations() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let conn = open(tmp.path()).expect("open initial");
        // schema_version table must exist and be populated.
        let v: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_version",
                [],
                |r| r.get(0),
            )
            .expect("schema_version readable");
        assert!(v > 0, "expected positive schema version, got {v}");
    }

    #[test]
    fn open_twice_is_idempotent_for_check_triggers() {
        // R1-M2 doc: re-running open() is a no-op for the trigger install
        // because the sentinel `memories_ck_tier_ins` short-circuits the
        // CREATE TRIGGER batch. This test exercises both branches: first
        // open installs triggers; second open hits the already-installed
        // probe and returns early without running CREATE TRIGGER.
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        // First open.
        let _conn1 = open(tmp.path()).expect("first open");
        // Second open against the same path.
        let conn2 = open(tmp.path()).expect("re-open idempotent");
        // Sentinel trigger must exist.
        let n: i64 = conn2
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'trigger' AND name = 'memories_ck_tier_ins'",
                [],
                |r| r.get(0),
            )
            .expect("trigger query");
        assert_eq!(n, 1, "sentinel trigger must be installed exactly once");
    }

    #[test]
    fn open_applies_wal_journal_mode() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let conn = open(tmp.path()).expect("open");
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .expect("journal_mode");
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn open_enables_foreign_keys() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let conn = open(tmp.path()).expect("open");
        let fk: i32 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .expect("foreign_keys");
        assert_eq!(fk, 1, "open() must enable foreign_keys");
    }

    #[test]
    fn check_trigger_rejects_bad_tier_insert() {
        // R1-M2 trigger contract: a write that violates the closed-set
        // CHECK on memories.tier must surface as an error. This test
        // exercises the trigger's actual rejection branch, not just the
        // install. We bypass the validator by writing directly with
        // rusqlite::execute so the trigger is the only thing standing
        // between the bad row and persistence.
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let conn = open(tmp.path()).expect("open");
        let now = chrono::Utc::now().to_rfc3339();
        let res = conn.execute(
            "INSERT INTO memories \
             (id, tier, namespace, title, content, tags, priority, confidence, \
              source, access_count, created_at, updated_at, metadata, reflection_depth) \
             VALUES (?1, 'NOT_A_TIER', 'test', 't', 'c', '[]', 5, 1.0, \
                     'src', 0, ?2, ?2, '{}', 0)",
            rusqlite::params!["bad-tier-id", now],
        );
        assert!(
            res.is_err(),
            "INSERT with bad tier must be rejected by R1-M2 trigger"
        );
    }
}
