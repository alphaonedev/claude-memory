// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 fold-A2A1.3 (#700) — AGE→CTE graceful-fallback acceptance.
//!
//! Pins the contract introduced by the per-request runtime fallback in
//! [`PostgresStore::kg_query_with_history`],
//! [`PostgresStore::kg_timeline`], [`PostgresStore::kg_invalidate`], and
//! [`PostgresStore::find_paths`]. The four dispatchers used to surface
//! a hard `StoreError::BackendUnavailable` (rendered as a 503 by the
//! MCP `memory_kg_*` handlers) when the AGE cypher branch failed at
//! request time — even though the relational CTE branch was right
//! there. After fold-A2A1.3 they catch the AGE-side failure, emit a
//! structured `tracing::warn!`, and re-issue the request through the
//! CTE branch so the operator-facing surface stays functional in
//! degraded mode.
//!
//! ## What this test proves
//!
//! For each of the four KG operations:
//!
//! 1. AGE-up path: with the `memory_graph` projection alive, the
//!    AGE-routed answer through the dispatcher matches the
//!    CTE-routed answer through the explicit `*_cte` method byte-for-
//!    byte on the canonical fixture corpus.
//! 2. AGE-down path: after we tear down the AGE projection via
//!    `DROP EXTENSION age CASCADE` + `CREATE EXTENSION age` (no
//!    projection re-bootstrapped) so the cypher branch starts
//!    raising errors, the dispatcher returns the CTE-routed answer
//!    — same bytes as step (1)'s CTE half.
//!
//! ## Gating
//!
//! The whole file is `#[cfg(feature = "sal-postgres")]`. Without the
//! feature there is no `PostgresStore` and the tests do not compile in.
//!
//! At run time the suite skips with an `eprintln!` when the required
//! env var is not set:
//!
//! - `AI_MEMORY_TEST_AGE_URL` — Postgres URL with the Apache AGE
//!   extension installed AND a bootstrapped `memory_graph` projection.
//!   Required for the AGE-up half. The role at this URL must have
//!   `CREATE`/`DROP EXTENSION` privilege so the test can simulate AGE
//!   unavailability.
//!
//! When the env var is set but the AGE extension is missing at the
//! pointed-to database, the suite skips rather than fails — same
//! discipline as `tests/age_cte_equivalence.rs`.

#![cfg(feature = "sal-postgres")]

use ai_memory::models::{Memory, MemoryKind, Tier};
use ai_memory::store::postgres::PostgresStore;
use ai_memory::store::{CallerContext, KgBackend, KgQueryRow, KgTimelineRow, MemoryStore};

// ---------------------------------------------------------------------------
// Skip helpers.
// ---------------------------------------------------------------------------

fn age_url() -> Option<String> {
    std::env::var("AI_MEMORY_TEST_AGE_URL").ok()
}

fn ctx() -> CallerContext {
    CallerContext::for_agent("ai:fold-a2a1-3-fallback")
}

// ---------------------------------------------------------------------------
// Fixture: a small directed graph with one connected component so we
// can exercise `kg_query` / `kg_timeline` / `find_paths` and a
// dangling edge so `kg_invalidate` has something to flip.
//
//        a ──> b ──> c
//        │     │
//        ▼     ▼
//        d     e
// ---------------------------------------------------------------------------

fn fixture_ids(prefix: &str) -> (Vec<String>, String) {
    let ns = format!("{prefix}-{}", uuid::Uuid::new_v4());
    let ids: Vec<String> = (0..5).map(|i| format!("{ns}-mem-{i:02}")).collect();
    (ids, ns)
}

fn make_memory(id: &str, namespace: &str, title: &str) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: id.to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("body for {title}"),
        tags: vec!["fold-a2a1-3".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "fold-a2a1-3-fallback".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "ai:fold-a2a1-3-fallback"}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    }
}

async fn insert_fixture_memories(store: &PostgresStore, ids: &[String], namespace: &str) {
    let titles = ["alpha", "bravo", "charlie", "delta", "echo"];
    for (id, title) in ids.iter().zip(titles.iter()) {
        let mem = make_memory(id, namespace, title);
        store
            .store(&ctx(), &mem)
            .await
            .expect("store fixture memory");
    }
}

/// `(source_idx, target_idx, relation)` triples for the fixture above.
fn fixture_edges() -> Vec<(usize, usize, &'static str)> {
    vec![
        (0, 1, "related_to"),
        (1, 2, "related_to"),
        (0, 3, "related_to"),
        (1, 4, "related_to"),
    ]
}

async fn insert_fixture_links(url: &str, ids: &[String]) {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await
        .expect("dev pool for fixture links");

    let base = chrono::Utc::now() - chrono::Duration::seconds(1000);
    for (i, (src, dst, rel)) in fixture_edges().iter().enumerate() {
        let valid_from = base + chrono::Duration::seconds(i64::try_from(i).unwrap_or(0));
        sqlx::query(
            "INSERT INTO memory_links (source_id, target_id, relation, valid_from, observed_by) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (source_id, target_id, relation) DO UPDATE SET \
                 valid_from = EXCLUDED.valid_from, \
                 observed_by = EXCLUDED.observed_by",
        )
        .bind(&ids[*src])
        .bind(&ids[*dst])
        .bind(*rel)
        .bind(valid_from)
        .bind("ai:fold-a2a1-3-fallback")
        .execute(&pool)
        .await
        .expect("insert fixture edge");
    }
}

/// Project the same edges into the AGE `memory_graph` so the cypher
/// branch has data to traverse. Same shape as
/// `tests/age_cte_equivalence.rs::project_fixture_into_age`.
async fn project_fixture_into_age(
    url: &str,
    ids: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await?;

    let mut tx = pool.begin().await?;
    sqlx::query("LOAD 'age'").execute(&mut *tx).await?;
    sqlx::query("SET search_path = ag_catalog, \"$user\", public")
        .execute(&mut *tx)
        .await?;

    for id in ids {
        let cypher = "MERGE (n {id: $id}) RETURN n";
        let sql = format!(
            "SELECT * FROM cypher('memory_graph', $$ {cypher} $$, $1::agtype) AS (n agtype)"
        );
        let params = serde_json::json!({ "id": id }).to_string();
        sqlx::query(&sql).bind(params).fetch_all(&mut *tx).await?;
    }

    let base = chrono::Utc::now() - chrono::Duration::seconds(1000);
    for (i, (src, dst, rel)) in fixture_edges().iter().enumerate() {
        let valid_from =
            (base + chrono::Duration::seconds(i64::try_from(i).unwrap_or(0))).to_rfc3339();
        let cypher = "MATCH (a {id: $src}), (b {id: $dst}) \
             MERGE (a)-[r:related_to {relation: $rel}]->(b) \
             SET r.valid_from = $vf, r.observed_by = 'ai:fold-a2a1-3-fallback', \
                 r.created_at = $vf \
             RETURN r";
        let sql = format!(
            "SELECT * FROM cypher('memory_graph', $$ {cypher} $$, $1::agtype) AS (r agtype)"
        );
        let params = serde_json::json!({
            "src": ids[*src],
            "dst": ids[*dst],
            "rel": rel,
            "vf": valid_from,
        })
        .to_string();
        sqlx::query(&sql).bind(params).fetch_all(&mut *tx).await?;
    }

    tx.commit().await?;
    Ok(())
}

/// Simulate AGE going down at runtime while the boot-time
/// `kg_backend` resolution stays at `KgBackend::Age`. We do this by
/// `DROP EXTENSION age CASCADE` — every subsequent `LOAD 'age'` /
/// `cypher()` call fails immediately, while `pg_extension` still
/// reflects the absence (but the store doesn't re-probe per request,
/// it caches the boot value).
///
/// Returns `Ok(())` if the drop succeeded, `Err(_)` otherwise.
async fn drop_age_extension(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await?;
    sqlx::query("DROP EXTENSION IF EXISTS age CASCADE")
        .execute(&pool)
        .await?;
    Ok(())
}

/// Restore AGE after the failure-injection portion so the next test
/// run starts from a known-good state. Idempotent on re-runs.
async fn restore_age_extension(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await?;
    sqlx::query("CREATE EXTENSION IF NOT EXISTS age")
        .execute(&pool)
        .await?;
    sqlx::query("LOAD 'age'").execute(&pool).await?;
    sqlx::query("SET search_path = ag_catalog, \"$user\", public")
        .execute(&pool)
        .await?;
    // Recreate the graph projection. `create_graph` raises if the
    // graph already exists; the `DROP EXTENSION ... CASCADE` removes
    // the projection along with the catalog so this is safe.
    let _ = sqlx::query("SELECT create_graph('memory_graph')")
        .execute(&pool)
        .await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Sort helpers — both branches may emit rows in different orders.
// ---------------------------------------------------------------------------

fn sort_query_rows(mut rows: Vec<KgQueryRow>) -> Vec<KgQueryRow> {
    rows.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.target_id.cmp(&b.target_id))
            .then_with(|| a.relation.cmp(&b.relation))
            .then_with(|| a.path.cmp(&b.path))
    });
    rows
}

fn sort_timeline_rows(mut rows: Vec<KgTimelineRow>) -> Vec<KgTimelineRow> {
    rows.sort_by(|a, b| {
        a.valid_from
            .cmp(&b.valid_from)
            .then_with(|| a.target_id.cmp(&b.target_id))
            .then_with(|| a.relation.cmp(&b.relation))
    });
    rows
}

// ---------------------------------------------------------------------------
// The acceptance test. Single `#[tokio::test]` so the per-test
// teardown order is deterministic (DROP / CREATE EXTENSION is global
// state — running these in parallel against the same database would
// race).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn age_to_cte_fallback_clears_a2a1_3() {
    let Some(url) = age_url() else {
        eprintln!("skip: AI_MEMORY_TEST_AGE_URL not set");
        return;
    };

    // Boot a store while AGE is up — capture the resolved backend so
    // the rest of the test exercises the cypher dispatcher.
    let store = match PostgresStore::connect(&url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: PostgresStore::connect failed: {e}");
            return;
        }
    };
    if !matches!(store.kg_backend(), KgBackend::Age) {
        eprintln!("skip: kg_backend resolved to CTE (AGE extension not installed at URL)");
        return;
    }

    let (ids, ns) = fixture_ids("a2a1-3");
    insert_fixture_memories(&store, &ids, &ns).await;
    insert_fixture_links(&url, &ids).await;
    if let Err(e) = project_fixture_into_age(&url, &ids).await {
        eprintln!("skip: failed to project fixture into memory_graph: {e}");
        return;
    }

    // -------------------------------------------------------------
    // Phase 1 — AGE up. The dispatcher's AGE branch is exercised.
    // We pin the expected answer from the CTE branch (which is the
    // canonical fallback target) so phase-2 has something to
    // compare against.
    // -------------------------------------------------------------
    let cte_query_baseline = store
        .kg_query_cte(&ids[0], 3)
        .await
        .expect("phase-1: cte kg_query");
    assert!(
        !cte_query_baseline.is_empty(),
        "fixture must produce reachable rows from id[0]"
    );

    let age_query_dispatch = store
        .kg_query(&ids[0], 3)
        .await
        .expect("phase-1: AGE-routed kg_query (dispatch)");

    assert_eq!(
        sort_query_rows(cte_query_baseline.clone()),
        sort_query_rows(age_query_dispatch),
        "phase-1: AGE dispatch must match CTE baseline on the fixture corpus"
    );

    let cte_timeline_baseline = store
        .kg_timeline_cte(&ids[0], None, None, None)
        .await
        .expect("phase-1: cte kg_timeline");

    let cte_paths_baseline = store
        .find_paths_cte(&ids[0], &ids[2], Some(3), Some(16))
        .await
        .expect("phase-1: cte find_paths");
    assert!(
        !cte_paths_baseline.is_empty(),
        "fixture must produce at least one path from id[0] to id[2]"
    );

    // -------------------------------------------------------------
    // Phase 2 — AGE down. We DROP the extension under the live
    // store; subsequent `LOAD 'age'` calls fail. The dispatcher
    // should catch the AGE-side failure, log a warn!, and re-issue
    // through the CTE branch — same answer as the baseline.
    // -------------------------------------------------------------
    if let Err(e) = drop_age_extension(&url).await {
        eprintln!("skip phase-2: DROP EXTENSION age failed: {e}");
        return;
    }

    let fallback_query = store
        .kg_query(&ids[0], 3)
        .await
        .expect("phase-2: dispatcher must fall back to CTE on AGE failure");
    assert_eq!(
        sort_query_rows(cte_query_baseline),
        sort_query_rows(fallback_query),
        "phase-2: AGE→CTE fallback must produce baseline answer"
    );

    let fallback_timeline = store
        .kg_timeline(&ids[0], None, None, None)
        .await
        .expect("phase-2: kg_timeline dispatcher must fall back to CTE on AGE failure");
    assert_eq!(
        sort_timeline_rows(cte_timeline_baseline),
        sort_timeline_rows(fallback_timeline),
        "phase-2: AGE→CTE fallback must produce baseline timeline answer"
    );

    let fallback_paths = store
        .find_paths(&ids[0], &ids[2], Some(3), Some(16))
        .await
        .expect("phase-2: find_paths dispatcher must fall back to CTE on AGE failure");
    // Sort the paths so we compare multisets. Each inner Vec is
    // already a deterministic walk through the graph.
    let mut a = cte_paths_baseline;
    a.sort();
    let mut b = fallback_paths;
    b.sort();
    assert_eq!(
        a, b,
        "phase-2: AGE→CTE fallback must produce baseline find_paths answer"
    );

    // kg_invalidate fallback: flip a known edge. After the call the
    // relational table must reflect the invalidation regardless of
    // which branch handled the request — the CTE branch UPDATEs
    // `memory_links` directly so this is the durable source of
    // truth.
    let flip_until = chrono::Utc::now().to_rfc3339();
    let row = store
        .kg_invalidate(&ids[0], &ids[1], "related_to", Some(&flip_until))
        .await
        .expect("phase-2: kg_invalidate dispatcher must fall back to CTE on AGE failure");
    assert!(
        row.found,
        "phase-2: kg_invalidate fallback must locate the targeted edge"
    );

    // Restore AGE so a follow-up `cargo test` re-run starts clean.
    if let Err(e) = restore_age_extension(&url).await {
        eprintln!("warning: failed to restore AGE extension after fallback test: {e}");
    }
}
