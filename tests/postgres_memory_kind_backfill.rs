// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact
// on regression assertions.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]
//! v0.7.0 cluster-I — postgres `memory_kind` backfill migration regression.
//!
//! Pins the postgres-side QW-2 (persona-as-artifact) migration
//! contract (`migrations/postgres/0018_v07_persona.sql`,
//! `PostgresStore::migrate_v36`). The SQLite branch is already pinned
//! by `tests/wt_1_a_schema_migration.rs`; this file closes the
//! symmetrical postgres gap surfaced by issue #767 / COV-6.
//!
//! Asserts that on a postgres database that was bootstrapped at a
//! pre-v36 schema (i.e. `memories.memory_kind`, `memories.entity_id`,
//! `memories.persona_version` columns absent and the partial
//! `idx_personas_by_entity` index absent), a fresh
//! `PostgresStore::connect()` call:
//!
//! 1. Re-adds `memory_kind TEXT NOT NULL DEFAULT 'observation'` via
//!    the v36 migration's idempotent `ALTER TABLE ... ADD COLUMN IF
//!    NOT EXISTS`.
//! 2. Backfills pre-existing rows with the default value
//!    `'observation'` (the column's `NOT NULL DEFAULT` clause does
//!    the work — postgres stamps every legacy row at the moment
//!    the column appears).
//! 3. Re-adds `entity_id TEXT` and `persona_version INTEGER` (both
//!    NULLable; legacy rows preserve their `NULL` payloads).
//! 4. Re-creates the partial index
//!    `idx_personas_by_entity ON memories(entity_id, namespace)
//!    WHERE memory_kind = 'persona'` so persona-by-entity lookups
//!    stay covered.
//! 5. Re-stamps `schema_version` at the final v38 marker (the v36
//!    step is on the migration ladder between v35 and v37/v38 —
//!    re-running migrate brings the DB to the head of the ladder,
//!    not just back to 36).
//!
//! # Gating
//!
//! Requires both:
//!   - `feature = "sal-postgres"` so `PostgresStore` exists.
//!   - `AI_MEMORY_TEST_POSTGRES_URL` set at run time to a fresh,
//!     disposable database. The test bootstraps its own schema, mutates
//!     it to simulate a pre-v36 legacy state, and re-runs the
//!     migration ladder against the same database.
//!
//! Without either, the tests `eprintln!` a skip message and return
//! cleanly — matching the pattern used by `tests/sal_v07_postgres_findings.rs`
//! and the existing `tests/postgres_schema_parity.rs`.

#![cfg(feature = "sal-postgres")]

use ai_memory::store::postgres::PostgresStore;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

mod common;
use common::postgres_url;

/// Open an out-of-band `sqlx` pool against the same URL the adapter
/// uses. We deliberately bypass `PostgresStore` for the mutation +
/// inspection queries so the test is independent of the adapter's
/// own helper surface — a regression in `PostgresStore` cannot mask
/// a real schema drift, and we can stage the legacy-DB shape without
/// the adapter clamping it back.
async fn inspection_pool(url: &str) -> PgPool {
    PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(url)
        .await
        .expect("inspection pool connect")
}

/// Probe whether `column` exists on `memories`.
async fn column_exists(pool: &PgPool, column: &str) -> bool {
    let row: Option<(i32,)> = sqlx::query_as(
        "SELECT 1 FROM information_schema.columns
         WHERE table_name = 'memories' AND column_name = $1",
    )
    .bind(column)
    .fetch_optional(pool)
    .await
    .expect("query information_schema.columns");
    row.is_some()
}

/// Probe whether the named index exists.
async fn index_exists(pool: &PgPool, index_name: &str) -> bool {
    let row: Option<(i32,)> =
        sqlx::query_as("SELECT 1 FROM pg_class WHERE relname = $1 AND relkind = 'i'")
            .bind(index_name)
            .fetch_optional(pool)
            .await
            .expect("query pg_class for index");
    row.is_some()
}

/// Read the current head of `schema_version`.
async fn read_schema_version(pool: &PgPool) -> i32 {
    let v: Option<i32> = sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM schema_version")
        .fetch_optional(pool)
        .await
        .expect("read schema_version");
    v.unwrap_or(0)
}

/// Roll the postgres database back to a simulated "pre-v36" shape:
///   - drop `memory_kind`, `entity_id`, `persona_version` columns,
///   - drop `idx_personas_by_entity` partial index,
///   - delete the schema_version row(s) >= 36 so the next migrate()
///     re-runs migrate_v36 onwards.
///
/// `CASCADE` on the column drops is needed because the partial index
/// depends on `memory_kind`. The order (index first, then columns) is
/// belt-and-braces — `DROP INDEX IF EXISTS` is a no-op if the column
/// drop CASCADE already nuked it.
async fn roll_back_to_pre_v36(pool: &PgPool) {
    // Wipe the table contents first so the column drops don't have to
    // deal with rows referencing dropped column data via partial-index
    // entries. (Belt-and-braces; ALTER ... DROP COLUMN CASCADE handles
    // this on its own, but the test inserts its own legacy rows below
    // so starting empty keeps the assertion arithmetic clean.)
    sqlx::query("TRUNCATE memories CASCADE")
        .execute(pool)
        .await
        .expect("truncate memories");

    sqlx::query("DROP INDEX IF EXISTS idx_personas_by_entity")
        .execute(pool)
        .await
        .expect("drop idx_personas_by_entity");

    sqlx::query("ALTER TABLE memories DROP COLUMN IF EXISTS persona_version")
        .execute(pool)
        .await
        .expect("drop persona_version");
    sqlx::query("ALTER TABLE memories DROP COLUMN IF EXISTS entity_id")
        .execute(pool)
        .await
        .expect("drop entity_id");
    sqlx::query("ALTER TABLE memories DROP COLUMN IF EXISTS memory_kind CASCADE")
        .execute(pool)
        .await
        .expect("drop memory_kind");

    // Remove the v36+ stamps so the migration ladder treats this DB as
    // "needs migrate_v36, _v37, _v38" on next connect.
    sqlx::query("DELETE FROM schema_version WHERE version >= 36")
        .execute(pool)
        .await
        .expect("delete schema_version >= 36");
}

/// Seed a legacy row WITHOUT the v36 columns. The row uses the minimal
/// pre-v36 column set; the v36 migration must backfill `memory_kind`
/// to the default `'observation'` on every such row.
async fn seed_legacy_row(pool: &PgPool, id: &str, namespace: &str, title: &str) {
    sqlx::query(
        "INSERT INTO memories
            (id, tier, namespace, title, content, tags, priority, confidence,
             source, access_count, created_at, updated_at, metadata)
         VALUES
            ($1, 'mid', $2, $3, 'legacy-body', '[]'::jsonb, 5, 1.0,
             'test', 0, '2026-05-14T00:00:00Z', '2026-05-14T00:00:00Z',
             '{}'::jsonb)",
    )
    .bind(id)
    .bind(namespace)
    .bind(title)
    .execute(pool)
    .await
    .expect("seed legacy row");
}

// -----------------------------------------------------------------------
// Test — full backfill cycle.
// -----------------------------------------------------------------------

#[tokio::test]
async fn migrate_v36_restores_memory_kind_column_and_backfills_legacy_rows() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    // Phase 1 — initial connect runs the full ladder. After this the
    // DB is at the head version with all v36+ columns present. We
    // bind the handle so its pool stays alive across the schema-shape
    // assertions below, then drop it explicitly before the rollback
    // mutates the catalog out-of-band.
    let store_initial = PostgresStore::connect(&url).await.expect("initial connect");
    let pool = inspection_pool(&url).await;

    // Sanity: post-initial-connect, the v36 columns + index must exist
    // (validates that the bootstrap actually landed; otherwise the test
    // below would tautologically pass).
    assert!(
        column_exists(&pool, "memory_kind").await,
        "pre-condition: memory_kind must exist after initial connect"
    );
    assert!(
        column_exists(&pool, "entity_id").await,
        "pre-condition: entity_id must exist after initial connect"
    );
    assert!(
        column_exists(&pool, "persona_version").await,
        "pre-condition: persona_version must exist after initial connect"
    );
    assert!(
        index_exists(&pool, "idx_personas_by_entity").await,
        "pre-condition: idx_personas_by_entity must exist after initial connect"
    );

    // Drop the initial store handle so its pool releases connections
    // before we mutate the schema out-of-band. sqlx's after_connect
    // hook holds no DDL locks past statement boundaries, but releasing
    // the handle removes one more source of catalog contention before
    // we hit ALTER TABLE.
    drop(store_initial);

    // Phase 2 — roll back to a simulated pre-v36 legacy state and
    // seed three legacy rows (no memory_kind / entity_id /
    // persona_version values).
    roll_back_to_pre_v36(&pool).await;

    assert!(
        !column_exists(&pool, "memory_kind").await,
        "phase-2: memory_kind must be ABSENT after rollback"
    );
    assert!(
        !column_exists(&pool, "entity_id").await,
        "phase-2: entity_id must be ABSENT after rollback"
    );
    assert!(
        !column_exists(&pool, "persona_version").await,
        "phase-2: persona_version must be ABSENT after rollback"
    );
    assert!(
        !index_exists(&pool, "idx_personas_by_entity").await,
        "phase-2: idx_personas_by_entity must be ABSENT after rollback"
    );
    let v_pre = read_schema_version(&pool).await;
    assert!(
        v_pre < 36,
        "phase-2: schema_version must be < 36 after rollback (got {v_pre})"
    );

    seed_legacy_row(&pool, "legacy-1", "cov-6-backfill", "legacy title 1").await;
    seed_legacy_row(&pool, "legacy-2", "cov-6-backfill", "legacy title 2").await;
    seed_legacy_row(&pool, "legacy-3", "cov-6-backfill", "legacy title 3").await;

    // Phase 3 — re-connect. PostgresStore::connect runs bootstrap
    // (CREATE TABLE IF NOT EXISTS = no-op on the still-present
    // `memories` table) then migrate(), which walks the ladder from
    // wherever schema_version sits. With v_pre < 36, migrate_v36 will
    // fire and apply the idempotent ALTER TABLE + CREATE INDEX +
    // schema_version stamp.
    // The handle is intentionally consumed by `drop` immediately —
    // we only need the side-effect (migrate ladder runs to head); the
    // assertions below all go through the out-of-band `pool`.
    drop(
        PostgresStore::connect(&url)
            .await
            .expect("re-connect after rollback"),
    );

    // -----------------------------------------------------------------
    // Assertion 1 — memory_kind column is back.
    // -----------------------------------------------------------------
    assert!(
        column_exists(&pool, "memory_kind").await,
        "post-migrate: memory_kind must be restored by migrate_v36"
    );

    // -----------------------------------------------------------------
    // Assertion 2 — every legacy row got the default 'observation'.
    // The migration's `ADD COLUMN ... NOT NULL DEFAULT 'observation'`
    // stamps every pre-existing row at the moment the column appears.
    // -----------------------------------------------------------------
    let legacy_row_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM memories WHERE namespace = 'cov-6-backfill'")
            .fetch_one(&pool)
            .await
            .expect("count legacy rows");
    assert_eq!(
        legacy_row_count, 3,
        "post-migrate: all 3 seeded legacy rows must survive the migration"
    );

    let backfilled_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memories
         WHERE namespace = 'cov-6-backfill' AND memory_kind = 'observation'",
    )
    .fetch_one(&pool)
    .await
    .expect("count backfilled rows");
    assert_eq!(
        backfilled_count, 3,
        "post-migrate: every legacy row must be backfilled with memory_kind='observation'"
    );

    // No legacy row should carry NULL — the column is NOT NULL.
    let null_kind_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memories
         WHERE namespace = 'cov-6-backfill' AND memory_kind IS NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count null memory_kind rows");
    assert_eq!(
        null_kind_count, 0,
        "post-migrate: NOT NULL contract on memory_kind must hold"
    );

    // -----------------------------------------------------------------
    // Assertion 3 — companion columns (entity_id, persona_version)
    // are back. Both are NULLable; legacy rows preserve their NULL
    // payloads (no backfill expected, just the column re-add).
    // -----------------------------------------------------------------
    assert!(
        column_exists(&pool, "entity_id").await,
        "post-migrate: entity_id must be restored by migrate_v36"
    );
    assert!(
        column_exists(&pool, "persona_version").await,
        "post-migrate: persona_version must be restored by migrate_v36"
    );

    let nonnull_entity_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memories
         WHERE namespace = 'cov-6-backfill' AND entity_id IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .expect("count non-null entity_id");
    assert_eq!(
        nonnull_entity_count, 0,
        "post-migrate: legacy rows must keep NULL entity_id (no implicit backfill)"
    );

    // -----------------------------------------------------------------
    // Assertion 4 — partial index re-created and functional. We
    // probe pg_class for the index name AND insert a persona row to
    // exercise the index (no error on insert proves the partial-
    // index predicate `memory_kind = 'persona'` compiles cleanly).
    // -----------------------------------------------------------------
    assert!(
        index_exists(&pool, "idx_personas_by_entity").await,
        "post-migrate: idx_personas_by_entity must be restored by migrate_v36"
    );

    // Insert a persona row that the partial index covers. If the
    // predicate is malformed or the column types don't match the
    // index expression, this insert errors.
    sqlx::query(
        "INSERT INTO memories
            (id, tier, namespace, title, content, tags, priority, confidence,
             source, access_count, created_at, updated_at, metadata,
             memory_kind, entity_id, persona_version)
         VALUES
            ('persona-1', 'long', 'cov-6-backfill', 'persona artefact',
             'persona body', '[]'::jsonb, 5, 1.0,
             'test', 0, '2026-05-14T00:00:00Z', '2026-05-14T00:00:00Z',
             '{}'::jsonb, 'persona', 'entity-alpha', 1)",
    )
    .execute(&pool)
    .await
    .expect("insert persona row covered by idx_personas_by_entity");

    // The persona row should be retrievable via the (entity_id,
    // namespace) lookup the partial index is sized for.
    let persona_lookup: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM memories
         WHERE entity_id = 'entity-alpha'
           AND namespace = 'cov-6-backfill'
           AND memory_kind = 'persona'",
    )
    .fetch_optional(&pool)
    .await
    .expect("entity_id lookup");
    assert_eq!(
        persona_lookup.map(|t| t.0).as_deref(),
        Some("persona-1"),
        "post-migrate: persona row must be reachable via the (entity_id, namespace) index"
    );

    // -----------------------------------------------------------------
    // Assertion 5 — schema_version stamped at the head of the ladder.
    // migrate() re-walks v36 -> v37 -> v38; the final stamp is the
    // postgres CURRENT_SCHEMA_VERSION (currently 38).
    // -----------------------------------------------------------------
    let v_after = read_schema_version(&pool).await;
    assert!(
        v_after >= 38,
        "post-migrate: schema_version must reach >= 38 (head of postgres ladder); got {v_after}"
    );

    // -----------------------------------------------------------------
    // Cleanup — remove the rows we inserted so the test leaves the
    // database in the same logical state as it found it (modulo the
    // schema_version stamp, which only ratchets upward).
    // -----------------------------------------------------------------
    sqlx::query("DELETE FROM memories WHERE namespace = 'cov-6-backfill'")
        .execute(&pool)
        .await
        .expect("cleanup test rows");
}
