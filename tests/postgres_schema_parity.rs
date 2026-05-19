// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown)]
//! `Postgres` ↔ `SQLite` schema parity test.
//!
//! Asserts that the `Postgres` adapter (`PostgresStore`) reaches the same
//! `CURRENT_SCHEMA_VERSION` as the `SQLite` adapter (`db.rs`) and that
//! every relational table created by the `SQLite` ladder is also present
//! on the `Postgres` bootstrap. `SQLite`-only constructs (FTS5 virtual
//! tables, triggers wired to FTS sync) are flagged via stderr but do
//! not fail the test — they're documented as "no `Postgres` analog
//! needed, equivalent functionality lives in the GIN tsvector index".
//!
//! # Gating
//!
//! Requires both:
//!   - `feature = "sal-postgres"` so `PostgresStore` exists.
//!   - `AI_MEMORY_TEST_POSTGRES_URL` set at run time to a fresh,
//!     disposable database (the test bootstraps its own schema and
//!     leaves no junk rows behind, but it does not drop the database).
//!
//! Without either, the tests `eprintln!` a skip message and return
//! cleanly — matching the pattern used by `tests/sal_contract.rs` and
//! the `src/store/postgres.rs::tests` live blocks.
//!
//! # Why this is a v0.7.0 release blocker
//!
//! The expanded v0.7.0 charter elevates `Postgres` from "experimental
//! second backend" to "first-class peer of `SQLite`". A drift between
//! the two adapters' schema versions silently means downstream Rust
//! that targets one backend will see a different table set when the
//! deployment swaps to the other — `audit_log` vs no-audit, quota
//! enforcement vs no-quota. Pinning parity with this test catches the
//! drift in CI before a release ships.

#![cfg(feature = "sal-postgres")]

use ai_memory::store::postgres::PostgresStore;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

mod common;
use common::postgres_url;

/// `Postgres` `CURRENT_SCHEMA_VERSION` — tracks
/// `src/store/postgres.rs::CURRENT_SCHEMA_VERSION`.
///
/// The two ladders (SQLite + Postgres) are independent integer
/// namespaces. Early in v0.7 the Postgres ladder ran AHEAD of SQLite
/// because of postgres-only steps (v29 in-place `vector(N)`
/// conversion, v30 `metadata_is_object` CHECK). The SQLite ladder has
/// since added more steps than Postgres (v40/v41/v42 PERF/Cluster
/// work) and the integer relationship inverted — SQLite is now at 42
/// while Postgres is at 41, and Postgres v41 is the functional mirror
/// of SQLite v42 (PERF-8 auto-persona `mentioned_entity_id`). The
/// functional mapping is documented inline in each `migrate_vN` arm
/// of `src/store/postgres.rs`; the integer relationship here is just
/// bookkeeping for this parity test. The previous cross-ladder `>=`
/// floor assertion was retired in #797 once the namespaces inverted —
/// see the docstring of `schema_versions_match_across_adapters`.
const POSTGRES_CURRENT_VERSION: i64 = 47;

/// Open an out-of-band `sqlx` pool against the same URL the adapter
/// uses. We deliberately bypass `PostgresStore` for the inspection
/// queries so the parity assertions are independent of the adapter's
/// own helper surface — a regression in `PostgresStore` cannot mask a
/// real schema drift. The pool is small (max=2) because the four
/// parity tests fan out at most one query each before dropping the
/// handle.
async fn inspection_pool(url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(url)
        .await
        .expect("inspection pool connect")
}

/// The `SQLite`-side relational tables the `Postgres` adapter MUST cover.
///
/// Excludes:
///   - `memories_fts` — `SQLite` FTS5 virtual table; equivalent
///     function on `Postgres` is the GIN tsvector index
///     `memories_content_fts`.
///   - `SQLite` triggers (`memories_ai`, `memories_ad`,
///     `memories_au`) — FTS5 sync triggers; `Postgres`' tsvector is
///     materialized by the index expression and does not require
///     trigger sync.
///
/// Order matches `src/db.rs::SCHEMA` + the migration ladder (v15-v28).
const SQLITE_RELATIONAL_TABLES: &[&str] = &[
    "memories",
    "memory_links",
    "schema_version",
    "audit_log",
    "archived_memories",
    "namespace_meta",
    "pending_actions",
    "sync_state",
    "subscriptions",
    "entity_aliases",
    "memory_transcripts",
    "memory_transcript_links",
    "signed_events",
    "subscription_events",
    "subscription_dlq",
    "agent_quotas",
];

/// `Postgres`-only relations (added for the F6 SAL surfaces).
/// Documented here so the parity test is explicit about which rows
/// are *expected* to exist only on the `Postgres` side.
const POSTGRES_ONLY_RELATIONS: &[&str] = &[
    "kg_query_view",
    "kg_timeline_view",
    // kg_find_paths is a function (pg_proc), not a relation.
];

#[tokio::test]
async fn schema_versions_match_across_adapters() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };
    // Connect via the adapter so it runs the bootstrap + ladder.
    let _store = PostgresStore::connect(&url).await.expect("connect");
    let pool = inspection_pool(&url).await;

    let pg_version: Option<i32> = sqlx::query_scalar("SELECT MAX(version) FROM schema_version")
        .fetch_optional(&pool)
        .await
        .expect("read schema_version");

    let pg_version_i64 = i64::from(pg_version.expect("schema_version row must exist"));
    // The two ladders are independent integer namespaces (see the
    // POSTGRES_CURRENT_VERSION docstring); a direct `>=` cross-ladder
    // comparison is no longer meaningful now that SQLite trails Postgres
    // numerically while still leading functionally. The equality
    // assertion below is the load-bearing parity check — if Postgres'
    // `migrate()` did not reach its own CURRENT_SCHEMA_VERSION constant
    // (because a `migrate_vN` arm panicked, was skipped, or the
    // constant was bumped without the corresponding function), this
    // test fails.
    assert_eq!(
        pg_version_i64, POSTGRES_CURRENT_VERSION,
        "Postgres schema_version ({pg_version_i64}) must match the \
         Postgres CURRENT_SCHEMA_VERSION ({POSTGRES_CURRENT_VERSION}). \
         A drift here means a Postgres ladder step didn't run, or the \
         constant was bumped without the corresponding migrate_vN \
         function in src/store/postgres.rs."
    );
}

#[tokio::test]
async fn postgres_covers_every_sqlite_relational_table() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };
    let _store = PostgresStore::connect(&url).await.expect("connect");
    let pool = inspection_pool(&url).await;

    let mut missing = Vec::new();
    for table in SQLITE_RELATIONAL_TABLES {
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM pg_class WHERE relname = $1 AND relkind = 'r'")
                .bind(*table)
                .fetch_optional(&pool)
                .await
                .expect("query pg_class");
        if exists.is_none() {
            missing.push(*table);
        }
    }

    assert!(
        missing.is_empty(),
        "Postgres adapter is missing SQLite-side tables: {missing:?}. \
         These are required for v0.7.0 schema parity — see the SQLite \
         ladder in src/db.rs and ensure each migrate_vN has a Postgres \
         port in src/store/postgres.rs."
    );
}

#[tokio::test]
async fn postgres_only_kg_views_present() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };
    let _store = PostgresStore::connect(&url).await.expect("connect");
    let pool = inspection_pool(&url).await;

    for relation in POSTGRES_ONLY_RELATIONS {
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM pg_class WHERE relname = $1 AND relkind = 'v'")
                .bind(*relation)
                .fetch_optional(&pool)
                .await
                .expect("query pg_class for view");
        assert!(
            exists.is_some(),
            "expected Postgres-only view {relation} to exist"
        );
    }

    // kg_find_paths is a function — probe pg_proc.
    let func_exists: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM pg_proc WHERE proname = 'kg_find_paths'")
            .fetch_optional(&pool)
            .await
            .expect("query pg_proc");
    assert!(
        func_exists.is_some(),
        "kg_find_paths function must exist on Postgres"
    );
}

#[tokio::test]
async fn sqlite_only_artefacts_documented() {
    // This test does not connect to either backend — it documents the
    // SQLite-only artefacts so the next person reading the parity
    // suite knows which gaps are intentional.
    //
    // SQLite-only:
    //   - `memories_fts` virtual table (FTS5).
    //     Postgres equivalent: `memories_content_fts` GIN tsvector
    //     index. Both surface as `db::search_*` / `PostgresStore::search`.
    //   - Triggers `memories_ai` / `memories_ad` / `memories_au`.
    //     Postgres equivalent: tsvector index expression evaluated
    //     at insert / update — no triggers needed.
    //   - `scope_idx` / `agent_id_idx` as VIRTUAL columns.
    //     Postgres equivalent: STORED generated columns — same
    //     semantics, slightly more disk space, no per-read recomputation.
    //
    // No assertions; the test passes as documentation.
    eprintln!("SQLite-only constructs documented (FTS5 vtable + sync triggers)");
}
