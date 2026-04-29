// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! # Public API
//!
//! Shared CLI test fixtures. **Stable contract** for downstream W5
//! closers (R5/C5/X5).
//!
//! ## Surface
//!
//! ```ignore
//! pub struct TestEnv {
//!     pub db_path: PathBuf,
//!     pub stdout: Vec<u8>,
//!     pub stderr: Vec<u8>,
//!     // _tmp keeps the TempDir alive — DO NOT drop it before the test
//!     // finishes inspecting db_path.
//! }
//!
//! impl TestEnv {
//!     /// Allocate a fresh tempdir + DB path. Schema is NOT initialized;
//!     /// production code paths (db::open) handle migrations idempotently.
//!     pub fn fresh() -> Self;
//!
//!     /// Returns a `CliOutput` borrowing `self.stdout` / `self.stderr`
//!     /// so a `cmd_*` handler can write into the captured buffers.
//!     pub fn output(&mut self) -> CliOutput<'_>;
//!
//!     /// Read captured stdout as UTF-8.
//!     pub fn stdout_str(&self) -> &str;
//!     /// Read captured stderr as UTF-8.
//!     pub fn stderr_str(&self) -> &str;
//! }
//!
//! /// Insert a single deterministic memory row directly via `db::insert`
//! /// (bypasses the CLI entirely). Returns the actual stored ID.
//! pub fn seed_memory(
//!     db_path: &Path,
//!     namespace: &str,
//!     title: &str,
//!     content: &str,
//! ) -> String;
//! ```
//!
//! ## Notes for downstream closers
//!
//! - `TestEnv::fresh()` uses `tempfile::TempDir`; the TempDir is held in
//!   `_tmp` and cleaned up on Drop. Don't replace `db_path` with a path
//!   that would outlive `_tmp` if the test relies on cleanup.
//! - `seed_memory` produces a row with deterministic agent_id
//!   ("test-agent"), tier=mid, priority=5, source=test. Use `db::insert`
//!   directly if you need different shape.
//! - `output()` returns a borrow with the env's lifetime; standard Rust
//!   borrow rules apply (no second mutable borrow of `self.stdout` while
//!   the `CliOutput` is alive).

#![cfg(test)]

use crate::cli::io_writer::CliOutput;
use crate::{db, models};
use chrono::Utc;
use std::path::{Path, PathBuf};

/// Per-test fixture: scratch DB path + captured output buffers.
pub struct TestEnv {
    pub db_path: PathBuf,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    // Held to keep the temp dir alive for the duration of the test.
    _tmp: tempfile::TempDir,
}

impl TestEnv {
    /// Allocate a fresh tempdir + DB path. The DB file is *not* created;
    /// `db::open` will materialize it on first use.
    pub fn fresh() -> Self {
        let _tmp = tempfile::tempdir().expect("tempdir");
        let db_path = _tmp.path().join("ai-memory.db");
        Self {
            db_path,
            stdout: Vec::new(),
            stderr: Vec::new(),
            _tmp,
        }
    }

    /// Borrow self.stdout / self.stderr as a `CliOutput<'_>`.
    ///
    /// Note: this borrows the env mutably *only* for the stdout/stderr
    /// fields. To pass `&self.db_path` alongside the returned
    /// `CliOutput`, take a snapshot of `db_path` *before* calling
    /// `output()` (e.g. `let db = env.db_path.clone();`). The borrow
    /// checker won't let you intersperse `&env.db_path` with a live
    /// `CliOutput<'_>` returned from this method even though the
    /// underlying fields are disjoint.
    pub fn output(&mut self) -> CliOutput<'_> {
        CliOutput::from_std(&mut self.stdout, &mut self.stderr)
    }

    /// Captured stdout, decoded as UTF-8.
    pub fn stdout_str(&self) -> &str {
        std::str::from_utf8(&self.stdout).expect("stdout utf-8")
    }

    /// Captured stderr, decoded as UTF-8.
    pub fn stderr_str(&self) -> &str {
        std::str::from_utf8(&self.stderr).expect("stderr utf-8")
    }
}

/// Insert one deterministic memory row directly via `db::insert`.
/// Returns the stored ID (may equal the generated UUID, or a pre-existing
/// row's id if the upsert merged on hash). Bypasses the CLI entirely.
pub fn seed_memory(db_path: &Path, namespace: &str, title: &str, content: &str) -> String {
    let conn = db::open(db_path).expect("db::open");
    let now = Utc::now().to_rfc3339();
    let mut metadata = models::default_metadata();
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String("test-agent".to_string()),
        );
    }
    let mem = models::Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: models::Tier::Mid,
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
