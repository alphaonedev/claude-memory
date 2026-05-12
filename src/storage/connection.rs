// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Connection setup for the SQLite substrate. v0.7.0 L0.5-3 extracted
//! `open` + the SQLCipher passphrase helper out of `src/db.rs` into
//! this sub-module. Pure refactor — semantics unchanged.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

use super::migrations::{SCHEMA, migrate};

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
    Ok(conn)
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
