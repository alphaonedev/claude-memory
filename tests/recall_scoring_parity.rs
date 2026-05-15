// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 F6 Gap 7 — recall scoring parity between `SQLite` and Postgres.
//!
//! Seeds an identical 50-memory corpus into both backends with varied
//! tier / priority / confidence / `access_count` / age, runs the same
//! FTS query, then asserts that the top-K orderings agree within a
//! small swap tolerance. The tolerance is published in the assertion
//! message so a future tightening — or relaxation — surfaces clearly.
//!
//! The Postgres leg requires `AI_MEMORY_TEST_POSTGRES_URL`; without it
//! the test exits as a clean skip so the default `cargo test` flow
//! stays offline.

#![cfg(all(feature = "sal", feature = "sal-postgres"))]

use ai_memory::models::{Memory, Tier};
use ai_memory::store::postgres::PostgresStore;
use ai_memory::store::sqlite::SqliteStore;
use ai_memory::store::{CallerContext, Filter, MemoryStore};

fn ctx() -> CallerContext {
    CallerContext::for_agent("ai:recall-parity")
}

fn make_corpus(namespace: &str) -> Vec<Memory> {
    // 50 memories with varied scoring inputs. Every row contains the
    // word "alpha" so the FTS hit set is fully populated; rank-relevant
    // signal comes from the per-row "alpha" repetition density plus the
    // out-of-band scoring factors (tier / priority / confidence /
    // access_count / age).
    let now = chrono::Utc::now();
    (0_usize..50)
        .map(|i| {
            // Tier rotation: 60% short, 30% mid, 10% long.
            let tier = match i % 10 {
                0 => Tier::Long,
                1..=3 => Tier::Mid,
                _ => Tier::Short,
            };
            // Priority rotation 1..=10
            let priority = i32::try_from((i % 10) + 1).unwrap_or(5);
            // Confidence varies 0.4..=1.0 in 0.012 steps.
            #[allow(clippy::cast_precision_loss)]
            let confidence = 0.4 + (i as f64) * 0.012;
            // Access count varies 0..=49.
            let access_count = i64::try_from(i).unwrap_or(0);
            // Age in hours: row 0 is 49 hours old, row 49 is 0 hours.
            let age_hours = 49_i64 - i64::try_from(i).unwrap_or(0);
            let updated = now - chrono::Duration::hours(age_hours);
            let updated_rfc = updated.to_rfc3339();
            // FTS body: each row has the trigger word "alpha" repeated
            // i+1 times so ts_rank / FTS5 can produce a meaningful
            // ranking signal. Trailing filler keeps FTS happy.
            let alpha = "alpha ".repeat(i + 1);
            let content =
                format!("{alpha} memory body number {i} with mixed tokens beta gamma delta");
            Memory {
                id: format!("p-{i:02}"),
                tier,
                namespace: namespace.to_string(),
                title: format!("entry-{i:02}"),
                content,
                tags: vec!["parity".to_string()],
                priority,
                confidence,
                source: "test".to_string(),
                access_count,
                created_at: updated_rfc.clone(),
                updated_at: updated_rfc,
                last_accessed_at: None,
                expires_at: None,
                metadata: serde_json::json!({"agent_id": "ai:recall-parity"}),
                reflection_depth: 0,
                memory_kind: ai_memory::models::MemoryKind::Observation,
                entity_id: None,
                persona_version: None,
                citations: Vec::new(),
                source_uri: None,
                source_span: None,
            }
        })
        .collect()
}

/// Compute the number of swap positions between two orderings on the
/// shared id set: how many positions does each id need to shift
/// between the lists. Returns the maximum `|position_diff|` across
/// the shared subset — a stable, easy-to-explain score that lets us
/// pin a "≤K swaps" tolerance.
fn max_position_drift(a: &[String], b: &[String]) -> usize {
    let pos_b: std::collections::HashMap<&str, usize> = b
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    let mut worst = 0usize;
    for (i, id) in a.iter().enumerate() {
        if let Some(&j) = pos_b.get(id.as_str()) {
            let drift = i.abs_diff(j);
            worst = worst.max(drift);
        }
    }
    worst
}

/// Tolerance: ≤2 swap positions between the two top-10 orderings. The
/// constant lives at module scope so a future tightening (recall-quality
/// work) flips the bound visibly.
const TOLERANCE_SWAPS: usize = 2;

#[tokio::test]
async fn recall_scoring_parity_top_10_within_2_swaps() {
    let Ok(pg_url) = std::env::var("AI_MEMORY_TEST_POSTGRES_URL") else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };

    // Use a per-run namespace to isolate from anything else on the
    // shared Postgres fixture.
    let ns = format!("recall-parity-{}", uuid::Uuid::new_v4());
    let corpus = make_corpus(&ns);

    // Seed both backends.
    let src_tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let sqlite = SqliteStore::open(src_tmp.path()).expect("open sqlite");
    let pg = PostgresStore::connect(&pg_url).await.expect("connect pg");

    for mem in &corpus {
        sqlite.store(&ctx(), mem).await.expect("seed sqlite");
        pg.store(&ctx(), mem).await.expect("seed pg");
    }

    let filter = Filter {
        namespace: Some(ns.clone()),
        limit: 50,
        ..Filter::default()
    };

    let sqlite_hits = sqlite
        .search(&ctx(), "alpha", &filter)
        .await
        .expect("sqlite search");
    let pg_hits = pg
        .search(&ctx(), "alpha", &filter)
        .await
        .expect("pg search");

    // Both must surface the full corpus (every row contains "alpha").
    assert_eq!(
        sqlite_hits.len(),
        50,
        "sqlite must surface every alpha-bearing row"
    );
    assert_eq!(pg_hits.len(), 50, "pg must surface every alpha-bearing row");

    // Top-10 fingerprints. We expect these to agree within a tiny
    // swap window — the SQLite formula is BM25-style and Postgres is
    // ts_rank, so absolute scores differ but the dominant factors
    // (priority + tier + access_count) line the orderings up to a
    // bounded swap.
    let sqlite_top10: Vec<String> = sqlite_hits.iter().take(10).map(|m| m.id.clone()).collect();
    let pg_top10: Vec<String> = pg_hits.iter().take(10).map(|m| m.id.clone()).collect();

    let drift = max_position_drift(&sqlite_top10, &pg_top10);
    assert!(
        drift <= TOLERANCE_SWAPS,
        "top-10 ordering drift exceeded tolerance: \
         sqlite_top10={sqlite_top10:?} \
         pg_top10={pg_top10:?} \
         max_drift={drift} (tolerance={TOLERANCE_SWAPS})"
    );

    // Sanity: the union of the two top-10s must cover at least 8
    // shared ids — otherwise the orderings are recommending different
    // memories, not just shuffling.
    let shared: usize = sqlite_top10
        .iter()
        .filter(|id| pg_top10.contains(id))
        .count();
    assert!(
        shared >= 8,
        "top-10 sets must overlap on at least 8 ids (got {shared}); \
         sqlite_top10={sqlite_top10:?} pg_top10={pg_top10:?}"
    );
}
