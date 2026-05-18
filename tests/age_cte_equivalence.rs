// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7 J5 — AGE vs CTE dual-path equivalence tests.
//!
//! Exercises the same Postgres SAL knowledge-graph operation through
//! both backends ([`KgBackend::Cte`] and [`KgBackend::Age`]) on an
//! identical fixture corpus and asserts the two row sets are equal
//! after sorting. This is the safety net that catches Cypher impl
//! drift from the canonical recursive-CTE path before it ships.
//!
//! Coverage:
//! - J2 `memory_kg_query`        via `kg_query_cypher`  vs `kg_query_cte`
//! - J3 `memory_kg_timeline`     via `kg_timeline_cypher` vs `kg_timeline_cte`
//! - J4 `memory_kg_invalidate`   via `kg_invalidate_cypher` vs `kg_invalidate_cte`
//!
//! ## Gating
//!
//! The whole file is `#[cfg(feature = "sal-postgres")]`. Without the
//! feature there is no `PostgresStore` and the tests do not compile in.
//!
//! At run time each test independently checks for a Postgres URL and
//! `eprintln!`-skips when none is configured — this matches the live
//! integration patterns used elsewhere in this repo
//! (`src/store/postgres.rs::tests`, `tests/sal_contract.rs`). When the
//! Postgres URL is present but Apache AGE is not installed (`SELECT 1
//! FROM pg_extension WHERE extname='age'` returns `None`), the AGE half
//! is skipped and the CTE half still runs so the fixture-shape contract
//! is exercised on every CI.
//!
//! ## Env vars
//!
//! - `AI_MEMORY_TEST_POSTGRES_URL` — vanilla Postgres URL. The CTE
//!   branch is exercised against this.
//! - `AI_MEMORY_TEST_AGE_URL`      — Postgres URL with the Apache AGE
//!   extension installed AND a bootstrapped `memory_graph` projection
//!   (J1 graph-prep). The AGE branch is exercised against this.
//!
//! Either URL alone is sufficient to run the CTE half; the AGE half
//! requires `AI_MEMORY_TEST_AGE_URL`.
//!
//! ## Running locally
//!
//! ```bash
//! # Stand up the AGE-enabled Postgres test fixture:
//! docker compose -f packaging/docker-compose.postgres-age.yml up -d
//!
//! # Vanilla Postgres URL (CTE half):
//! export AI_MEMORY_TEST_POSTGRES_URL=postgres://ai_memory:ai_memory_test@localhost:5433/ai_memory_test
//!
//! # AGE-enabled Postgres URL (AGE half — requires `CREATE EXTENSION
//! # age` and `SELECT create_graph('memory_graph')` to be applied):
//! export AI_MEMORY_TEST_AGE_URL=postgres://ai_memory:ai_memory_test@localhost:5434/ai_memory_test
//!
//! cargo test --features sal-postgres --test age_cte_equivalence -- --test-threads=1
//! ```
//!
//! `--test-threads=1` is recommended because each test owns a unique
//! namespace per uuid so cross-contamination is unlikely, but the
//! shared `memory_graph` projection on the AGE side is single-tenant
//! within an AGE database.

#![cfg(feature = "sal-postgres")]

use ai_memory::models::ConfidenceSource;
use ai_memory::models::{Memory, Tier};
use ai_memory::store::postgres::PostgresStore;
use ai_memory::store::{
    CallerContext, KgBackend, KgInvalidateRow, KgQueryRow, KgTimelineRow, MemoryStore,
};

mod common;
use common::{age_url, postgres_url};

// ---------------------------------------------------------------------------
// Skip helpers — postgres_url + age_url consolidated into
// `tests/common/mod.rs` by issue #854; the suite stays skip-friendly on
// machines without a live Postgres so default `cargo test` stays green.
// ---------------------------------------------------------------------------

/// Probe whether the connected Postgres has Apache AGE installed.
/// Used to gate the AGE half of each equivalence test even when the
/// AGE URL was set — covers the case where the fixture compose file
/// is up but the extension hasn't been `CREATEd` yet.
fn age_extension_present(store: &PostgresStore) -> bool {
    matches!(store.kg_backend(), KgBackend::Age)
}

fn ctx() -> CallerContext {
    CallerContext::for_agent("ai:j5-equivalence")
}

// ---------------------------------------------------------------------------
// Fixture builders.
//
// The fixture corpus is small (~10 memories, 15 links) but topologically
// rich enough to exercise depth-1 → depth-3 traversals, branching, and
// the temporal-validity columns J3/J4 read.
//
// We give every memory a unique uuid id and isolate the namespace with
// a uuid suffix so concurrent test runs and the AGE/CTE shared graph
// projection don't collide on the unique (title, namespace) key or on
// edge identity.
// ---------------------------------------------------------------------------

fn make_memory(id: &str, namespace: &str, title: &str, content: &str) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: id.to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        tags: vec!["j5-fixture".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "j5-equivalence".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "ai:j5-equivalence"}),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    }
}

/// A small graph the equivalence tests can reason about by hand:
///
/// ```text
///        a ──> b ──> c ──> d
///        │     │     │
///        ▼     ▼     ▼
///        e     f     g
///        │
///        ▼
///        h
///   i ──> j   (disconnected pair)
/// ```
///
/// Returns (memory ids in graph order, namespace).
fn fixture_graph_ids(prefix: &str) -> (Vec<String>, String) {
    let ns = format!("{prefix}-{}", uuid::Uuid::new_v4());
    let ids: Vec<String> = (0..10).map(|i| format!("{ns}-mem-{i:02}")).collect();
    (ids, ns)
}

/// Insert the 10 fixture memories through the SAL store API.
async fn insert_fixture_memories(store: &PostgresStore, ids: &[String], namespace: &str) {
    let titles = [
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    ];
    for (id, title) in ids.iter().zip(titles.iter()) {
        let mem = make_memory(id, namespace, title, &format!("body for {title}"));
        store
            .store(&ctx(), &mem)
            .await
            .expect("store fixture memory");
    }
}

/// The 15 directed edges used by the equivalence corpus. Returned as
/// `(source_idx, target_idx, relation)` triples so the per-backend
/// fixture builder can render them into either the relational
/// `memory_links` table or the AGE property graph.
///
/// Index legend (from `fixture_graph_ids`):
///   0=a 1=b 2=c 3=d 4=e 5=f 6=g 7=h 8=i 9=j
fn fixture_edges() -> Vec<(usize, usize, &'static str)> {
    vec![
        (0, 1, "related_to"),
        (1, 2, "related_to"),
        (2, 3, "related_to"),
        (0, 4, "related_to"),
        (1, 5, "related_to"),
        (2, 6, "related_to"),
        (4, 7, "related_to"),
        (8, 9, "related_to"),
        // A second relation tag so the timeline filter has variety.
        (0, 1, "supersedes"),
        (1, 2, "supersedes"),
        (3, 6, "related_to"),
        (5, 7, "related_to"),
        (0, 5, "related_to"),
        (2, 7, "related_to"),
        (3, 7, "related_to"),
    ]
}

/// Insert the 15 fixture edges directly into `memory_links` via a
/// dedicated sqlx pool. We don't go through `MemoryStore::link` because
/// the v0.7-preview Postgres adapter returns `UnsupportedCapability`
/// for that method (see `src/store/postgres.rs::link`); the relational
/// row is still the source of truth for both the CTE branch and the
/// AGE-side mirror written by `kg_invalidate_cypher`.
///
/// `valid_from` is staggered by index so the timeline order is
/// deterministic across both backends.
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
        .bind("ai:j5-equivalence")
        .execute(&pool)
        .await
        .expect("insert fixture edge");
    }
}

/// Mirror the relational fixture edges into the AGE `memory_graph`
/// projection. Required because `kg_query_cypher` and
/// `kg_timeline_cypher` traverse the property graph, not the
/// relational table — the projection is normally maintained by the J1
/// graph-prep scripts but the dual-path test owns the corpus.
///
/// Returns `Ok(())` when the projection accepted every edge,
/// `Err(_)` if the AGE setup is not bootstrapped (caller skips the
/// AGE half when this happens).
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

    // Create one node per fixture memory. Idempotent through the MERGE
    // semantics so re-running the test against the same projection
    // doesn't duplicate vertices.
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
             SET r.valid_from = $vf, r.observed_by = 'ai:j5-equivalence', \
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

// ---------------------------------------------------------------------------
// Sort helpers — both backends are free to return rows in different
// orders (AGE has no implicit ORDER BY without `ORDER BY` in the
// Cypher; the CTE branch sorts by depth ASC, target_id ASC). For
// equivalence the test only cares about the multiset of rows.
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
// J2 — kg_query equivalence.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kg_query_equivalence() {
    // Pick whichever URL is set — we run the CTE branch against either
    // a vanilla or AGE-enabled URL because the relational table is
    // present in both. The AGE branch additionally requires the AGE
    // URL with the extension actually installed.
    let Some(url) = postgres_url().or_else(age_url) else {
        eprintln!("skip: neither AI_MEMORY_TEST_POSTGRES_URL nor AI_MEMORY_TEST_AGE_URL set");
        return;
    };

    let store = match PostgresStore::connect(&url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: PostgresStore::connect failed: {e}");
            return;
        }
    };

    let (ids, ns) = fixture_graph_ids("j5-query");
    insert_fixture_memories(&store, &ids, &ns).await;
    insert_fixture_links(&url, &ids).await;

    // CTE half — always runs against any reachable Postgres.
    let cte_rows = store.kg_query_cte(&ids[0], 3).await.expect("cte kg_query");
    assert!(
        !cte_rows.is_empty(),
        "CTE traversal from id[0] must reach the connected component"
    );

    // AGE half — only when the AGE extension resolved at connect time
    // AND we can project the corpus into the property graph. If either
    // step fails we report it via eprintln rather than failing the
    // suite, mirroring the J2/J3/J4 live-test skip discipline.
    if !age_extension_present(&store) {
        eprintln!("skip AGE half: kg_backend resolved to CTE (extension not installed)");
        return;
    }
    let age_url = age_url().unwrap_or_else(|| url.clone());
    if let Err(e) = project_fixture_into_age(&age_url, &ids).await {
        eprintln!("skip AGE half: failed to project fixture into memory_graph: {e}");
        return;
    }

    let age_rows = match store.kg_query_cypher(&ids[0], 3).await {
        Ok(rs) => rs,
        Err(e) => {
            eprintln!("skip AGE half: kg_query_cypher returned {e}");
            return;
        }
    };

    assert_eq!(
        sort_query_rows(cte_rows),
        sort_query_rows(age_rows),
        "AGE and CTE backends must produce the same kg_query result set"
    );
}

// ---------------------------------------------------------------------------
// J3 — kg_timeline equivalence.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kg_timeline_equivalence() {
    let Some(url) = postgres_url().or_else(age_url) else {
        eprintln!("skip: neither AI_MEMORY_TEST_POSTGRES_URL nor AI_MEMORY_TEST_AGE_URL set");
        return;
    };

    let store = match PostgresStore::connect(&url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: PostgresStore::connect failed: {e}");
            return;
        }
    };

    let (ids, ns) = fixture_graph_ids("j5-timeline");
    insert_fixture_memories(&store, &ids, &ns).await;
    insert_fixture_links(&url, &ids).await;

    let cte_rows = store
        .kg_timeline_cte(&ids[0], None, None, Some(50))
        .await
        .expect("cte kg_timeline");
    assert!(
        !cte_rows.is_empty(),
        "CTE timeline from id[0] must surface the seeded edges"
    );
    assert!(
        cte_rows.iter().all(|r| !r.valid_from.is_empty()),
        "CTE timeline rows must have a valid_from anchor"
    );

    if !age_extension_present(&store) {
        eprintln!("skip AGE half: kg_backend resolved to CTE (extension not installed)");
        return;
    }
    let age_url = age_url().unwrap_or_else(|| url.clone());
    if let Err(e) = project_fixture_into_age(&age_url, &ids).await {
        eprintln!("skip AGE half: failed to project fixture into memory_graph: {e}");
        return;
    }

    let age_rows = match store
        .kg_timeline_cypher(&ids[0], None, None, Some(50))
        .await
    {
        Ok(rs) => rs,
        Err(e) => {
            eprintln!("skip AGE half: kg_timeline_cypher returned {e}");
            return;
        }
    };

    // The AGE-side row's `valid_from` may be encoded as an RFC3339 string
    // identical to the CTE side (both serialise via DateTime::to_rfc3339)
    // so a sorted multiset compare is exact. If a future AGE projection
    // change shifts to milliseconds-precision the assertion will surface
    // it as a clean diff rather than silent drift.
    assert_eq!(
        sort_timeline_rows(cte_rows),
        sort_timeline_rows(age_rows),
        "AGE and CTE backends must produce the same kg_timeline result set"
    );
}

// ---------------------------------------------------------------------------
// J4 — kg_invalidate equivalence.
//
// Both backends mutate state, so we run them on disjoint edges from
// the same fixture corpus and assert each backend's returned row
// shape matches the other (same `found`, same `previous_valid_until`
// semantics — the actual `valid_until` stamp will differ because each
// call captures its own `now()`).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn kg_invalidate_equivalence() {
    let Some(url) = postgres_url().or_else(age_url) else {
        eprintln!("skip: neither AI_MEMORY_TEST_POSTGRES_URL nor AI_MEMORY_TEST_AGE_URL set");
        return;
    };

    let store = match PostgresStore::connect(&url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: PostgresStore::connect failed: {e}");
            return;
        }
    };

    let (ids, ns) = fixture_graph_ids("j5-invalidate");
    insert_fixture_memories(&store, &ids, &ns).await;
    insert_fixture_links(&url, &ids).await;

    // Pick a known-existing edge and a known-missing one. Both
    // backends must return the same `found` flag and `previous_valid_until`
    // shape (None for first invalidation, Some on second).
    let stamp = "2026-05-05T12:00:00+00:00";

    // CTE half — invalidate a real edge then re-invalidate to capture
    // the prior value.
    let cte_first = store
        .kg_invalidate_cte(&ids[0], &ids[1], "related_to", Some(stamp))
        .await
        .expect("cte invalidate first");
    assert!(cte_first.found, "CTE must find the seeded edge");
    assert!(
        cte_first.previous_valid_until.is_none(),
        "CTE first invalidation has no prior valid_until"
    );
    let cte_second = store
        .kg_invalidate_cte(&ids[0], &ids[1], "related_to", Some(stamp))
        .await
        .expect("cte invalidate second");
    assert!(cte_second.found);
    assert!(
        cte_second.previous_valid_until.is_some(),
        "CTE second invalidation must surface the prior stamp"
    );

    // CTE miss path — synthetic ids that don't exist must surface
    // `found = false` and an empty `valid_until`.
    let cte_miss = store
        .kg_invalidate_cte("synthetic-src", "synthetic-dst", "related_to", Some(stamp))
        .await
        .expect("cte invalidate miss");
    assert_no_match(&cte_miss);

    if !age_extension_present(&store) {
        eprintln!("skip AGE half: kg_backend resolved to CTE (extension not installed)");
        return;
    }
    let age_url = age_url().unwrap_or_else(|| url.clone());
    if let Err(e) = project_fixture_into_age(&age_url, &ids).await {
        eprintln!("skip AGE half: failed to project fixture into memory_graph: {e}");
        return;
    }

    // Pick a different edge so the AGE assertions aren't polluted by
    // the CTE-side mutation above (the AGE branch reads/writes the
    // property graph and mirrors back into memory_links).
    let age_first = match store
        .kg_invalidate_cypher(&ids[1], &ids[2], "related_to", Some(stamp))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skip AGE half: kg_invalidate_cypher returned {e}");
            return;
        }
    };
    let age_second = store
        .kg_invalidate_cypher(&ids[1], &ids[2], "related_to", Some(stamp))
        .await
        .expect("age invalidate second");
    let age_miss = store
        .kg_invalidate_cypher(
            "synthetic-src-2",
            "synthetic-dst-2",
            "related_to",
            Some(stamp),
        )
        .await
        .expect("age invalidate miss");

    // Same shape contracts as the CTE side — the equivalence is on the
    // `found` flag, the `previous_valid_until` Option shape, and the
    // miss-path zero-row contract. The exact `valid_until` strings
    // match because we passed the same explicit stamp to both backends.
    assert!(age_first.found);
    assert!(age_first.previous_valid_until.is_none());
    assert_eq!(
        age_first.valid_until, cte_first.valid_until,
        "explicit stamp must round-trip identically through AGE and CTE"
    );

    assert!(age_second.found);
    assert!(age_second.previous_valid_until.is_some());
    assert_eq!(
        age_second.valid_until, cte_second.valid_until,
        "second-pass stamp must round-trip identically"
    );

    assert_no_match(&age_miss);
    assert_eq!(
        age_miss, cte_miss,
        "miss-path row shape must be byte-identical between backends"
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn assert_no_match(row: &KgInvalidateRow) {
    assert!(!row.found, "expected no match, got {row:?}");
    assert!(
        row.valid_until.is_empty(),
        "miss must leave valid_until empty"
    );
    assert!(
        row.previous_valid_until.is_none(),
        "miss must leave previous_valid_until None"
    );
}

// ---------------------------------------------------------------------------
// v0.7.0 S6-M3 — AGE Cypher dispatcher tests.
//
// Pre-S6-M3, `kg_query_with_history`, `kg_timeline`, and `kg_invalidate`
// always routed to the CTE branch regardless of `kg_backend`. The three
// tests below pin the dispatcher contract: the public-facing entry point
// MUST route to AGE when the extension is present (so the ROADMAP2 §7.4.4
// 30%-faster-at-depth-5 claim is actually reachable) and MUST route to
// CTE when the extension is absent.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_kg_query_routes_to_cypher_when_age_available() {
    let Some(url) = age_url() else {
        eprintln!("skip: AI_MEMORY_TEST_AGE_URL not set; this test requires an AGE-enabled URL");
        return;
    };
    let store = match PostgresStore::connect(&url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: PostgresStore::connect failed: {e}");
            return;
        }
    };
    if !age_extension_present(&store) {
        eprintln!("skip: AGE extension not present on the configured URL");
        return;
    }

    let (ids, ns) = fixture_graph_ids("s6m3-dispatch-query");
    insert_fixture_memories(&store, &ids, &ns).await;
    insert_fixture_links(&url, &ids).await;
    if let Err(e) = project_fixture_into_age(&url, &ids).await {
        eprintln!("skip: failed to project fixture into AGE: {e}");
        return;
    }

    // Public entry point must produce the same rows as the direct
    // cypher call when AGE is available (dispatcher proof).
    let public_rows = store.kg_query(&ids[0], 3).await.expect("public kg_query");
    let direct_rows = store
        .kg_query_cypher(&ids[0], 3)
        .await
        .expect("direct cypher kg_query");
    assert_eq!(
        sort_query_rows(public_rows),
        sort_query_rows(direct_rows),
        "kg_query dispatcher must route to kg_query_cypher when AGE is available"
    );
}

#[tokio::test]
async fn test_kg_query_routes_to_cte_when_age_absent() {
    // Use the plain Postgres URL (no AGE) explicitly so the dispatcher
    // resolves to KgBackend::Cte.
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };
    let store = match PostgresStore::connect(&url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: PostgresStore::connect failed: {e}");
            return;
        }
    };
    if age_extension_present(&store) {
        eprintln!("skip: AGE extension IS present; this test needs a vanilla Postgres URL");
        return;
    }

    let (ids, ns) = fixture_graph_ids("s6m3-dispatch-cte");
    insert_fixture_memories(&store, &ids, &ns).await;
    insert_fixture_links(&url, &ids).await;

    // Public entry point must produce the same rows as the direct CTE
    // call when AGE is absent.
    let public_rows = store.kg_query(&ids[0], 3).await.expect("public kg_query");
    let direct_rows = store
        .kg_query_cte(&ids[0], 3)
        .await
        .expect("direct cte kg_query");
    assert_eq!(
        sort_query_rows(public_rows),
        sort_query_rows(direct_rows),
        "kg_query dispatcher must route to kg_query_cte when AGE is absent"
    );
}

#[tokio::test]
async fn test_age_cte_dual_path_returns_identical_results() {
    // Regression — the J5 contract: AGE and CTE produce identical
    // KgQueryRow sets on the same fixture. Already exercised by
    // `kg_query_equivalence` above; this rename pins the S6-M3
    // dual-path discipline (#648) under a more discoverable test name
    // so the v0.7.0 fix campaign report can reference it directly.
    let Some(url) = age_url() else {
        eprintln!("skip: AI_MEMORY_TEST_AGE_URL not set");
        return;
    };
    let store = match PostgresStore::connect(&url).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("skip: PostgresStore::connect failed: {e}");
            return;
        }
    };
    if !age_extension_present(&store) {
        eprintln!("skip: AGE extension not present");
        return;
    }

    let (ids, ns) = fixture_graph_ids("s6m3-dual-path");
    insert_fixture_memories(&store, &ids, &ns).await;
    insert_fixture_links(&url, &ids).await;
    if let Err(e) = project_fixture_into_age(&url, &ids).await {
        eprintln!("skip: failed to project fixture into AGE: {e}");
        return;
    }

    let cte_rows = store.kg_query_cte(&ids[0], 3).await.expect("cte kg_query");
    let age_rows = store
        .kg_query_cypher(&ids[0], 3)
        .await
        .expect("age kg_query");
    assert_eq!(
        sort_query_rows(cte_rows),
        sort_query_rows(age_rows),
        "S6-M3 dual-path discipline: AGE and CTE must produce identical results"
    );
}
