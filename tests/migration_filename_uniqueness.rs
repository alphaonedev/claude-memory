// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Cluster-J (issue #767, COR-13 + DOC-20) — migration filename
//! uniqueness pin.
//!
//! The migration ladders in `src/storage/migrations.rs` (sqlite) and
//! `src/store/postgres.rs` (postgres) load each step's SQL via
//! `include_str!("../../migrations/{sqlite,postgres}/NNNN_*.sql")`.
//! Two files sharing a `NNNN_` numeric prefix is currently inert
//! because the Rust ladder picks one explicit path per arm — but the
//! collision is a landmine for any external migration tool
//! (`refinery`, `sqlx-cli`, hand-rolled shell scripts) that orders
//! migrations by the leading sequence number alone.
//!
//! This test enumerates every `.sql` file in both migration
//! directories, parses the 4-digit numeric prefix, and asserts no two
//! files in the same directory share a prefix. The pin runs without
//! any project features so it's cheap to keep green and surfaces the
//! issue the moment a future patch lands a colliding file.
//!
//! The duplicate that motivated the pin (v0.7.0 audit):
//! - `migrations/sqlite/0031_v07_namespace_auto_atomise.sql` vs
//!   `migrations/sqlite/0031_v07_persona.sql`
//! - `migrations/postgres/0018_v07_namespace_auto_atomise.sql` vs
//!   `migrations/postgres/0018_v07_persona.sql`
//!
//! The Cluster-J fix deletes the `_namespace_auto_atomise.sql` orphans
//! (which were docs masquerading as migrations — `SELECT 1;` stamps
//! not referenced by any `include_str!`). This test pins the cleanup
//! so the collision can't reappear unnoticed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Return every `.sql` file directly inside `dir`, sorted by filename.
fn list_sql_files(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("read_dir {}: {err}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|ext| ext.to_str()) == Some("sql"))
        .collect();
    entries.sort();
    entries
}

/// Parse the leading numeric prefix of a migration filename
/// (`0031_v07_persona.sql` -> `"0031"`). Returns `None` if the file
/// does not follow the `NNNN_` convention so existing non-conforming
/// files (e.g., bootstrap schemas) are skipped rather than fenced in
/// by the pin.
fn numeric_prefix(filename: &str) -> Option<&str> {
    let underscore = filename.find('_')?;
    let prefix = &filename[..underscore];
    if !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_digit()) {
        Some(prefix)
    } else {
        None
    }
}

/// Assert no two `.sql` files in `dir` share a numeric prefix. On
/// failure, reports every colliding (prefix -> filenames) pair so the
/// operator can fix all collisions in one pass.
fn assert_unique_prefixes(dir: &Path) {
    let files = list_sql_files(dir);
    let mut buckets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in &files {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(prefix) = numeric_prefix(name) else {
            continue;
        };
        buckets
            .entry(prefix.to_string())
            .or_default()
            .push(name.to_string());
    }

    let collisions: Vec<(String, Vec<String>)> = buckets
        .into_iter()
        .filter(|(_, names)| names.len() > 1)
        .collect();

    assert!(
        collisions.is_empty(),
        "migration filename-prefix collisions in {}:\n{}",
        dir.display(),
        collisions
            .iter()
            .map(|(prefix, names)| format!("  {prefix}: {names:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn sqlite_migration_filenames_have_unique_numeric_prefix() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/sqlite");
    assert!(
        dir.is_dir(),
        "expected migrations/sqlite dir at {}",
        dir.display()
    );
    assert_unique_prefixes(&dir);
}

#[test]
fn postgres_migration_filenames_have_unique_numeric_prefix() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations/postgres");
    assert!(
        dir.is_dir(),
        "expected migrations/postgres dir at {}",
        dir.display()
    );
    assert_unique_prefixes(&dir);
}
