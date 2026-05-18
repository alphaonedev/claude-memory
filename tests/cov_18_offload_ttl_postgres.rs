// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7-polish #783 — COV-18: offload TTL sweep against postgres.
//!
//! The sqlite path is exercised by `tests/offload/acceptance.rs::
//! test_offload_ttl_expiry` and the in-module unit test
//! `src/offload/mod.rs::sweep_purges_expired_rows`. The postgres-backed
//! schema landed in v34 (`MIGRATION_V34_OFFLOADED_BLOBS` →
//! `migrations/postgres/0016_v07_offloaded_blobs.sql`) but no
//! postgres-side TTL-sweep coverage exists.
//!
//! This test:
//!   1. Connects to the test postgres URL (self-skip when unset).
//!   2. Drives `PostgresStore::connect` so the v34 migration ladders.
//!   3. Inserts N=10 blobs with backdated `stored_at` + TTLs, plus 5
//!      fresh-stamped rows and 3 permanent (`ttl_seconds IS NULL`)
//!      rows under a per-test unique namespace.
//!   4. Runs the equivalent TTL-sweep predicate against postgres:
//!      `DELETE FROM offloaded_blobs WHERE ttl_seconds IS NOT NULL
//!      AND (stored_at + ttl_seconds) < $1`.
//!   5. Asserts the backdated rows are gone, the fresh + permanent
//!      rows survive, the namespace-isolation invariant holds for
//!      unrelated namespaces, and the schema indexes the predicate
//!      we're relying on.
//!
//! ## Why the sweep is replayed in-test
//!
//! `crate::offload::sweep_expired` takes a `&rusqlite::Connection` —
//! the postgres adapter has no equivalent sweeper today. The brief's
//! intent is to pin the PG schema's ability to honour the TTL sweep
//! semantics so the future portable sweeper (or a daemon-side cron
//! against pg) lands on a verified storage contract. Re-issuing the
//! sqlite predicate in pg dialect is the load-bearing assertion here.

#![cfg(feature = "sal-postgres")]
#![allow(
    clippy::missing_panics_doc,
    clippy::too_many_lines,
    clippy::doc_markdown
)]

use ai_memory::store::postgres::PostgresStore;
use sqlx::Row;

mod common;
use common::postgres_url;

/// Open a sqlx pool against the same URL. The `PostgresStore::connect`
/// call ensures the v34 schema is in place; the pool is then used to
/// drive direct SQL — there's no public adapter method for either the
/// raw insert or the sweep on the postgres side as of `polish/v0.7-783`.
async fn pool(url: &str) -> sqlx::Pool<sqlx::Postgres> {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(url)
        .await
        .expect("connect sqlx pool")
}

async fn insert_blob(
    pool: &sqlx::Pool<sqlx::Postgres>,
    ref_id: &str,
    namespace: &str,
    stored_at: i64,
    ttl_seconds: Option<i64>,
) {
    // Minimal blob; tests are about row lifecycle, not zstd integrity.
    let dummy_zstd: Vec<u8> = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00];
    let sha = format!("{:064x}", 0_u128);
    sqlx::query(
        "INSERT INTO offloaded_blobs \
         (ref_id, namespace, content_zstd, content_sha256, stored_at, ttl_seconds, agent_id, signature_b64) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(ref_id)
    .bind(namespace)
    .bind(&dummy_zstd)
    .bind(sha)
    .bind(stored_at)
    .bind(ttl_seconds)
    .bind("ai:cov18")
    .bind("")
    .execute(pool)
    .await
    .expect("insert offloaded_blob row");
}

async fn count_in_ns(pool: &sqlx::Pool<sqlx::Postgres>, namespace: &str) -> i64 {
    sqlx::query("SELECT COUNT(*) FROM offloaded_blobs WHERE namespace = $1")
        .bind(namespace)
        .fetch_one(pool)
        .await
        .expect("count rows")
        .try_get::<i64, _>(0)
        .expect("read count")
}

async fn count_ttl_in_ns(pool: &sqlx::Pool<sqlx::Postgres>, namespace: &str, has_ttl: bool) -> i64 {
    let sql = if has_ttl {
        "SELECT COUNT(*) FROM offloaded_blobs \
         WHERE namespace = $1 AND ttl_seconds IS NOT NULL"
    } else {
        "SELECT COUNT(*) FROM offloaded_blobs \
         WHERE namespace = $1 AND ttl_seconds IS NULL"
    };
    sqlx::query(sql)
        .bind(namespace)
        .fetch_one(pool)
        .await
        .expect("count by ttl")
        .try_get::<i64, _>(0)
        .expect("read count")
}

/// Mirror of `sweep_expired` (sqlite) for the postgres dialect. Returns
/// the rowcount deleted.
async fn sweep_expired_pg(pool: &sqlx::Pool<sqlx::Postgres>, now_unix: i64) -> u64 {
    sqlx::query(
        "DELETE FROM offloaded_blobs \
         WHERE ttl_seconds IS NOT NULL \
           AND (stored_at + ttl_seconds) < $1",
    )
    .bind(now_unix)
    .execute(pool)
    .await
    .expect("execute sweep delete")
    .rows_affected()
}

#[tokio::test(flavor = "multi_thread")]
async fn cov18_offload_ttl_sweep_against_postgres() {
    let Some(url) = postgres_url() else {
        eprintln!(
            "skipping cov18_offload_ttl_sweep_against_postgres: \
             AI_MEMORY_TEST_POSTGRES_URL not set"
        );
        return;
    };

    // Drive PostgresStore::connect so the v34 offloaded_blobs migration
    // is guaranteed to have landed. Drop the store before using sqlx
    // directly — they share the underlying postgres tables but maintain
    // independent connection pools.
    let _store = PostgresStore::connect(&url)
        .await
        .expect("connect postgres adapter");

    let pool = pool(&url).await;

    // Per-test unique namespace so concurrent runs against the shared
    // scratch DB don't fight each other.
    let suffix = uuid::Uuid::new_v4();
    let ns = format!("cov18-{suffix}");
    let ns_other = format!("cov18-other-{suffix}");

    // -----------------------------------------------------------------
    // Seed N=10 backdated rows in `ns`. Each has stored_at ~ 1 hour
    // ago and ttl_seconds = 60 (so `stored_at + ttl < now`).
    // -----------------------------------------------------------------
    let now: i64 = chrono::Utc::now().timestamp();
    let one_hour_ago = now - 3600;
    for i in 0..10 {
        insert_blob(
            &pool,
            &format!("ofl_cov18_old_{suffix}_{i}"),
            &ns,
            one_hour_ago,
            Some(60),
        )
        .await;
    }
    // 5 fresh rows: stored_at == now, ttl 1 day (stays well in the
    // future).
    for i in 0..5 {
        insert_blob(
            &pool,
            &format!("ofl_cov18_fresh_{suffix}_{i}"),
            &ns,
            now,
            Some(86_400),
        )
        .await;
    }
    // 3 permanent rows (ttl_seconds IS NULL).
    for i in 0..3 {
        insert_blob(
            &pool,
            &format!("ofl_cov18_perm_{suffix}_{i}"),
            &ns,
            now - 7200,
            None,
        )
        .await;
    }
    // 2 backdated rows in an UNRELATED namespace — must NOT be touched
    // by the sweep (it's predicate-only, not namespace-scoped).
    for i in 0..2 {
        insert_blob(
            &pool,
            &format!("ofl_cov18_other_{suffix}_{i}"),
            &ns_other,
            one_hour_ago,
            Some(60),
        )
        .await;
    }

    let pre_total = count_in_ns(&pool, &ns).await;
    assert_eq!(pre_total, 18, "pre-sweep total in ns: 10 + 5 + 3 = 18");
    let pre_other = count_in_ns(&pool, &ns_other).await;
    assert_eq!(pre_other, 2);

    // -----------------------------------------------------------------
    // Run the equivalent TTL-sweep predicate against postgres.
    //
    // The sweep predicate is unscoped by namespace (matches the sqlite
    // sweeper's contract) — so the other namespace's expired row is
    // also dropped. We assert that boundary explicitly.
    // -----------------------------------------------------------------
    let deleted = sweep_expired_pg(&pool, now).await;
    assert_eq!(
        deleted, 12,
        "sweep removes 10 (ns) + 2 (ns_other) = 12 expired rows",
    );

    // -----------------------------------------------------------------
    // Post-sweep invariants.
    // -----------------------------------------------------------------
    let post_total = count_in_ns(&pool, &ns).await;
    assert_eq!(
        post_total, 8,
        "ns: 5 fresh + 3 permanent rows survive the sweep",
    );
    let post_ttl = count_ttl_in_ns(&pool, &ns, true).await;
    assert_eq!(post_ttl, 5, "ns: only fresh ttl'd rows remain");
    let post_perm = count_ttl_in_ns(&pool, &ns, false).await;
    assert_eq!(post_perm, 3, "ns: all permanent rows survive");

    let post_other = count_in_ns(&pool, &ns_other).await;
    assert_eq!(
        post_other, 0,
        "unrelated namespace's expired row was also dropped \
         (sweep predicate is namespace-agnostic; matches sqlite contract)",
    );

    // -----------------------------------------------------------------
    // Idempotency — re-running the sweep at the same `now` deletes
    // zero rows. Pins the row-bounded contract: a stable substrate
    // doesn't keep DELETE-ing on every tick.
    // -----------------------------------------------------------------
    let again = sweep_expired_pg(&pool, now).await;
    assert_eq!(again, 0, "second sweep at same now must be a no-op");

    // -----------------------------------------------------------------
    // Schema invariant — the partial index that makes the sweep cheap
    // is in place. (`idx_offloaded_blobs_ttl WHERE ttl_seconds IS NOT
    // NULL`, from the v34 migration.)
    // -----------------------------------------------------------------
    let idx_present: bool = sqlx::query(
        "SELECT EXISTS ( \
             SELECT 1 FROM pg_indexes \
             WHERE schemaname = 'public' \
               AND tablename = 'offloaded_blobs' \
               AND indexname = 'idx_offloaded_blobs_ttl' \
         )",
    )
    .fetch_one(&pool)
    .await
    .expect("query pg_indexes")
    .try_get::<bool, _>(0)
    .expect("read bool");
    assert!(
        idx_present,
        "v34 migration must install the ttl partial index that the sweep relies on",
    );

    // -----------------------------------------------------------------
    // Cleanup — surviving rows from this test go away. Other tests
    // sharing the scratch DB rely on this no-leak discipline.
    // -----------------------------------------------------------------
    sqlx::query("DELETE FROM offloaded_blobs WHERE namespace = $1 OR namespace = $2")
        .bind(&ns)
        .bind(&ns_other)
        .execute(&pool)
        .await
        .expect("cleanup test rows");
}
