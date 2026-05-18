// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7 J8 — AGE-vs-CTE knowledge-graph traversal bench gate.
//!
//! Measures `kg_query` p50/p95/p99 latency at depth = 5 against both
//! [`KgBackend`] variants on a fixed corpus and asserts the AGE p95 is
//! at least 30% faster than the CTE p95 (i.e. `age_p95 ≤ 0.7 × cte_p95`).
//! The whole point of pulling Apache AGE into the v0.7 substrate is the
//! traversal speedup; if AGE isn't winning by a meaningful margin we
//! want a CI signal so the maintainers can drop the complexity.
//!
//! ## Skip discipline
//!
//! Real AGE coverage requires a live Postgres + AGE fixture, which is
//! not guaranteed in CI (the `postgres-age` compose file is opt-in and
//! the action runner doesn't always have it standing up). The bench
//! follows the same env-gated skip pattern as
//! `tests/age_cte_equivalence.rs`:
//!
//!   - `AI_MEMORY_TEST_AGE_URL`      — Postgres URL with AGE installed
//!     AND the `memory_graph` projection bootstrapped (J1). Required
//!     to run the AGE half.
//!   - `AI_MEMORY_TEST_POSTGRES_URL` — vanilla Postgres URL. The CTE
//!     half can run against either URL; we prefer this when set so the
//!     CTE path doesn't share contention with the AGE branch in tests
//!     run against the same database.
//!
//! Failure modes (per the J8 spec, this is a soft gate — Postgres+AGE
//! may not be available in CI):
//!
//!   - **No Postgres URL set.** Print "skipped: no Postgres URL"
//!     to stderr and exit 0. The bench-gate workflow can surface the
//!     skip as a warning.
//!   - **Postgres URL set, AGE not installed.** Run the CTE half so the
//!     fixture/measure plumbing is exercised, print "skipped AGE half:
//!     extension not installed", emit a CTE-only JSON artifact, and
//!     exit 0.
//!   - **Both halves ran, AGE is NOT >= 30% faster.** Exit non-zero so
//!     CI flags the regression. This is the "is AGE actually pulling
//!     its weight?" assertion the spec calls out.
//!
//! ## Output
//!
//!   - `target/bench/age-vs-cte.json` — structured report (per-backend
//!     percentiles, ratio, gate verdict).
//!   - Markdown summary table to stdout so CI logs carry the report
//!     even when the JSON artifact isn't uploaded.
//!
//! ## Workload
//!
//! 200 fixture memories, 4 directed edges per memory (~800 edges)
//! arranged so a depth-5 traversal from the root touches a meaningful
//! subset of the graph. We run 30 measured iterations after a 5-iter
//! warm-up (matches the J8 spec).
//!
//! `harness = false` + ad-hoc main mirrors `benches/harness_bench.rs`
//! because we need (a) per-backend tagged percentiles in a single
//! artifact and (b) regression-gate behaviour at process exit, neither
//! of which Criterion's bench_function shape gives us cleanly.

// Bench-only file. Percentile math casts u128 nanosecond samples to
// f64 for human-readable reporting; the main function body is the
// orchestration of the whole bench harness (setup + warm-up + sample
// + report + gate-check) and exceeds the default `too_many_lines`
// threshold by design.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::doc_markdown
)]

use std::path::PathBuf;
#[cfg(feature = "sal-postgres")]
use std::time::Instant;

const ITERATIONS: usize = 30;
const WARMUP: usize = 5;
const DEPTH: usize = 5;
const FIXTURE_NODES: usize = 200;
/// AGE p95 must be at most this fraction of the CTE p95 — i.e. >= 30%
/// faster. The J8 spec states this explicitly.
const AGE_SPEEDUP_RATIO: f64 = 0.70;

/// One row of the J8 report.
#[derive(serde::Serialize)]
struct BackendReport {
    backend: &'static str,
    iterations: usize,
    depth: usize,
    p50_us: u128,
    p95_us: u128,
    p99_us: u128,
    min_us: u128,
    max_us: u128,
}

/// Top-level structured report written to `target/bench/age-vs-cte.json`.
#[derive(serde::Serialize)]
struct Report {
    schema: &'static str,
    version: &'static str,
    iterations: usize,
    warmup: usize,
    depth: usize,
    fixture_nodes: usize,
    /// `"ran"` / `"skipped_no_postgres"` / `"skipped_no_age"` /
    /// `"failed_age_too_slow"` — top-level outcome the CI workflow can
    /// pivot on without parsing the per-backend rows.
    status: &'static str,
    /// `Some(_)` when the AGE half ran. The gate ratio is
    /// `age_p95 / cte_p95`. When only one half ran the field is `None`.
    age_speedup_ratio: Option<f64>,
    /// The hard ratio threshold the gate compares against
    /// (`AGE_SPEEDUP_RATIO`). Surfaced so the JSON consumer doesn't
    /// have to hard-code the policy.
    gate_max_ratio: f64,
    backends: Vec<BackendReport>,
    /// Free-form context (the env-var skip message, gate verdict text).
    /// CI surface this in the workflow summary.
    notes: Vec<String>,
}

#[cfg(feature = "sal-postgres")]
fn percentile(sorted: &[u128], pct: f64) -> u128 {
    assert!(!sorted.is_empty(), "percentile of empty sample set");
    let rank = ((pct / 100.0) * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

fn write_report(report: &Report) {
    let out_dir = PathBuf::from("target/bench");
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("[age_vs_cte] failed to create {}: {e}", out_dir.display());
        return;
    }
    let out_file = out_dir.join("age-vs-cte.json");
    match std::fs::write(&out_file, serde_json::to_string_pretty(report).unwrap()) {
        Ok(()) => eprintln!("[age_vs_cte] wrote {}", out_file.display()),
        Err(e) => eprintln!("[age_vs_cte] failed to write {}: {e}", out_file.display()),
    }
}

fn print_markdown(report: &Report) {
    println!("# v0.7 J8 — AGE vs CTE kg_query bench (depth = {DEPTH})\n");
    println!("status: `{}`\n", report.status);
    if report.backends.is_empty() {
        println!("_no measurements collected; see notes below._\n");
    } else {
        println!(
            "| backend | iters | depth | p50 (ms) | p95 (ms) | p99 (ms) | min (ms) | max (ms) |"
        );
        println!("|---|---:|---:|---:|---:|---:|---:|---:|");
        for r in &report.backends {
            println!(
                "| {} | {} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} |",
                r.backend,
                r.iterations,
                r.depth,
                r.p50_us as f64 / 1000.0,
                r.p95_us as f64 / 1000.0,
                r.p99_us as f64 / 1000.0,
                r.min_us as f64 / 1000.0,
                r.max_us as f64 / 1000.0,
            );
        }
        println!();
    }
    if let Some(ratio) = report.age_speedup_ratio {
        println!(
            "AGE p95 / CTE p95 = **{:.3}** (gate: <= {:.3})\n",
            ratio, report.gate_max_ratio
        );
    }
    if !report.notes.is_empty() {
        println!("## notes\n");
        for n in &report.notes {
            println!("- {n}");
        }
        println!();
    }
}

#[cfg(not(feature = "sal-postgres"))]
fn main() {
    // Default builds (no `sal-postgres` feature) cannot exercise the
    // PostgresStore at all, so emit a clean skip artifact and exit 0.
    // This keeps `cargo bench --bench age_vs_cte -- --test` green on
    // every developer laptop without forcing a Postgres install.
    let report = Report {
        schema: "ai-memory.bench.age-vs-cte.v1",
        version: env!("CARGO_PKG_VERSION"),
        iterations: ITERATIONS,
        warmup: WARMUP,
        depth: DEPTH,
        fixture_nodes: FIXTURE_NODES,
        status: "skipped_no_postgres",
        age_speedup_ratio: None,
        gate_max_ratio: AGE_SPEEDUP_RATIO,
        backends: Vec::new(),
        notes: vec![
            "skipped: built without `sal-postgres` feature; AGE/CTE bench requires Postgres SAL"
                .to_string(),
        ],
    };
    print_markdown(&report);
    write_report(&report);
    eprintln!(
        "[age_vs_cte] skipped: built without sal-postgres feature; rebuild with --features sal-postgres to run"
    );
}

#[cfg(feature = "sal-postgres")]
fn main() {
    use ai_memory::store::KgBackend;

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let mut notes: Vec<String> = Vec::new();
        let mut backends: Vec<BackendReport> = Vec::new();

        // Resolve URLs. Prefer the dedicated AGE URL for the AGE half,
        // and the vanilla URL for the CTE half so both branches can run
        // against their dedicated fixture compose service when both are
        // configured. Falling through to the other URL is fine — the
        // CTE branch only needs the relational `memory_links` table,
        // which is present on every Postgres install.
        let age_url = std::env::var("AI_MEMORY_TEST_AGE_URL").ok();
        let pg_url = std::env::var("AI_MEMORY_TEST_POSTGRES_URL").ok();
        let cte_url = pg_url.clone().or_else(|| age_url.clone());

        let Some(cte_url) = cte_url else {
            eprintln!(
                "[age_vs_cte] skipped: neither AI_MEMORY_TEST_POSTGRES_URL nor AI_MEMORY_TEST_AGE_URL set"
            );
            let report = Report {
                schema: "ai-memory.bench.age-vs-cte.v1",
                version: env!("CARGO_PKG_VERSION"),
                iterations: ITERATIONS,
                warmup: WARMUP,
                depth: DEPTH,
                fixture_nodes: FIXTURE_NODES,
                status: "skipped_no_postgres",
                age_speedup_ratio: None,
                gate_max_ratio: AGE_SPEEDUP_RATIO,
                backends: Vec::new(),
                notes: vec![
                    "skipped: no Postgres URL in env; set AI_MEMORY_TEST_POSTGRES_URL and/or AI_MEMORY_TEST_AGE_URL to run".to_string(),
                ],
            };
            print_markdown(&report);
            write_report(&report);
            return;
        };

        // ---- CTE half ----
        let (root_id, cte_samples) = match measure_cte(&cte_url).await {
            Ok(out) => out,
            Err(e) => {
                eprintln!("[age_vs_cte] CTE measurement failed: {e}");
                let report = Report {
                    schema: "ai-memory.bench.age-vs-cte.v1",
                    version: env!("CARGO_PKG_VERSION"),
                    iterations: ITERATIONS,
                    warmup: WARMUP,
                    depth: DEPTH,
                    fixture_nodes: FIXTURE_NODES,
                    status: "skipped_no_postgres",
                    age_speedup_ratio: None,
                    gate_max_ratio: AGE_SPEEDUP_RATIO,
                    backends: Vec::new(),
                    notes: vec![format!("skipped: CTE measurement errored: {e}")],
                };
                print_markdown(&report);
                write_report(&report);
                return;
            }
        };
        let cte_p95 = percentile(&cte_samples, 95.0);
        backends.push(report_for("cte", &cte_samples));
        notes.push(format!(
            "CTE measured against {} (root_id = {root_id})",
            redact(&cte_url)
        ));

        // ---- AGE half (only when AGE URL is set AND extension present) ----
        let Some(age_url) = age_url else {
            notes.push(
                "skipped AGE half: AI_MEMORY_TEST_AGE_URL not set (set it to a Postgres URL with AGE installed)"
                    .to_string(),
            );
            let report = Report {
                schema: "ai-memory.bench.age-vs-cte.v1",
                version: env!("CARGO_PKG_VERSION"),
                iterations: ITERATIONS,
                warmup: WARMUP,
                depth: DEPTH,
                fixture_nodes: FIXTURE_NODES,
                status: "skipped_no_age",
                age_speedup_ratio: None,
                gate_max_ratio: AGE_SPEEDUP_RATIO,
                backends,
                notes,
            };
            print_markdown(&report);
            write_report(&report);
            return;
        };

        match measure_age(&age_url, &root_id).await {
            Ok(Some(age_samples)) => {
                let age_p95 = percentile(&age_samples, 95.0);
                backends.push(report_for("age", &age_samples));
                let ratio = age_p95 as f64 / cte_p95.max(1) as f64;
                notes.push(format!(
                    "AGE measured against {}; ratio age_p95 / cte_p95 = {ratio:.3} (gate <= {AGE_SPEEDUP_RATIO:.2})",
                    redact(&age_url)
                ));
                let (status, gate_passed) = if ratio <= AGE_SPEEDUP_RATIO {
                    ("ran", true)
                } else {
                    ("failed_age_too_slow", false)
                };
                let report = Report {
                    schema: "ai-memory.bench.age-vs-cte.v1",
                    version: env!("CARGO_PKG_VERSION"),
                    iterations: ITERATIONS,
                    warmup: WARMUP,
                    depth: DEPTH,
                    fixture_nodes: FIXTURE_NODES,
                    status,
                    age_speedup_ratio: Some(ratio),
                    gate_max_ratio: AGE_SPEEDUP_RATIO,
                    backends,
                    notes,
                };
                print_markdown(&report);
                write_report(&report);
                if !gate_passed {
                    eprintln!(
                        "[age_vs_cte] FAIL: AGE p95 = {age_p95}us, CTE p95 = {cte_p95}us, ratio = {ratio:.3} > {AGE_SPEEDUP_RATIO:.2}"
                    );
                    std::process::exit(1);
                }
                eprintln!(
                    "[age_vs_cte] OK: AGE p95 = {age_p95}us, CTE p95 = {cte_p95}us, ratio = {ratio:.3} <= {AGE_SPEEDUP_RATIO:.2}"
                );
            }
            Ok(None) => {
                notes.push(
                    "skipped AGE half: kg_backend resolved to CTE (extension not installed at the AGE URL)"
                        .to_string(),
                );
                let report = Report {
                    schema: "ai-memory.bench.age-vs-cte.v1",
                    version: env!("CARGO_PKG_VERSION"),
                    iterations: ITERATIONS,
                    warmup: WARMUP,
                    depth: DEPTH,
                    fixture_nodes: FIXTURE_NODES,
                    status: "skipped_no_age",
                    age_speedup_ratio: None,
                    gate_max_ratio: AGE_SPEEDUP_RATIO,
                    backends,
                    notes,
                };
                print_markdown(&report);
                write_report(&report);
            }
            Err(e) => {
                notes.push(format!("skipped AGE half: measurement errored: {e}"));
                let report = Report {
                    schema: "ai-memory.bench.age-vs-cte.v1",
                    version: env!("CARGO_PKG_VERSION"),
                    iterations: ITERATIONS,
                    warmup: WARMUP,
                    depth: DEPTH,
                    fixture_nodes: FIXTURE_NODES,
                    status: "skipped_no_age",
                    age_speedup_ratio: None,
                    gate_max_ratio: AGE_SPEEDUP_RATIO,
                    backends,
                    notes,
                };
                print_markdown(&report);
                write_report(&report);
            }
        }

        // Suppress unused-import warning under cfg-ed build.
        let _ = KgBackend::Cte;
    });
}

#[cfg(feature = "sal-postgres")]
fn report_for(backend: &'static str, samples: &[u128]) -> BackendReport {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    BackendReport {
        backend,
        iterations: sorted.len(),
        depth: DEPTH,
        p50_us: percentile(&sorted, 50.0),
        p95_us: percentile(&sorted, 95.0),
        p99_us: percentile(&sorted, 99.0),
        min_us: *sorted.first().unwrap(),
        max_us: *sorted.last().unwrap(),
    }
}

/// Strip the password component from a Postgres URL so we don't echo
/// secrets into stdout or the JSON artifact. Best-effort — any value
/// the bench prints lands in CI logs.
#[cfg(feature = "sal-postgres")]
fn redact(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let (scheme, rest) = url.split_at(scheme_end + 3);
        if let Some(at) = rest.find('@') {
            let after = &rest[at..];
            return format!("{scheme}***{after}");
        }
    }
    url.to_string()
}

#[cfg(feature = "sal-postgres")]
async fn measure_cte(
    url: &str,
) -> Result<(String, Vec<u128>), Box<dyn std::error::Error + Send + Sync>> {
    use ai_memory::store::postgres::PostgresStore;

    let store = PostgresStore::connect(url).await?;
    let (ids, ns) = fixture_ids("j8-cte");
    insert_fixture(&store, url, &ids, &ns).await?;
    let root = ids[0].clone();

    // Warm-up — discard.
    for _ in 0..WARMUP {
        let _ = store.kg_query_cte(&root, DEPTH).await?;
    }

    let mut samples = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        let _ = store.kg_query_cte(&root, DEPTH).await?;
        samples.push(start.elapsed().as_micros());
    }
    Ok((root, samples))
}

#[cfg(feature = "sal-postgres")]
async fn measure_age(
    url: &str,
    _root_hint: &str,
) -> Result<Option<Vec<u128>>, Box<dyn std::error::Error + Send + Sync>> {
    use ai_memory::store::KgBackend;
    use ai_memory::store::postgres::PostgresStore;

    let store = PostgresStore::connect(url).await?;
    if !matches!(store.kg_backend(), KgBackend::Age) {
        return Ok(None);
    }

    // The AGE branch traverses `memory_graph`, not `memory_links`, so
    // we have to project a corpus into the property graph. We seed a
    // fresh fixture under a unique namespace so concurrent runs don't
    // collide on the unique (title, namespace) key or on edge identity
    // in the shared graph projection.
    let (ids, ns) = fixture_ids("j8-age");
    insert_fixture(&store, url, &ids, &ns).await?;
    if let Err(e) = project_into_age(url, &ids).await {
        // The AGE URL is set and the extension is present, but the
        // graph projection isn't usable. Surface as a None skip rather
        // than an error so CI treats it the same as "AGE not installed".
        eprintln!("[age_vs_cte] AGE projection failed: {e}");
        return Ok(None);
    }

    let root = ids[0].clone();
    for _ in 0..WARMUP {
        let _ = store.kg_query_cypher(&root, DEPTH).await?;
    }
    let mut samples = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        let _ = store.kg_query_cypher(&root, DEPTH).await?;
        samples.push(start.elapsed().as_micros());
    }
    Ok(Some(samples))
}

#[cfg(feature = "sal-postgres")]
fn fixture_ids(prefix: &str) -> (Vec<String>, String) {
    let ns = format!("{prefix}-{}", uuid::Uuid::new_v4());
    let ids: Vec<String> = (0..FIXTURE_NODES)
        .map(|i| format!("{ns}-mem-{i:04}"))
        .collect();
    (ids, ns)
}

/// Build the fixture corpus: `FIXTURE_NODES` memories, each with edges
/// to the next four nodes (mod N). This gives a graph dense enough that
/// a depth-5 traversal from the root reaches a meaningful subset of the
/// nodes (the BFS frontier grows roughly 4^5 before deduplication).
#[cfg(feature = "sal-postgres")]
async fn insert_fixture(
    store: &ai_memory::store::postgres::PostgresStore,
    url: &str,
    ids: &[String],
    namespace: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use ai_memory::models::{Memory, Tier};
    use ai_memory::store::{CallerContext, MemoryStore};

    let ctx = CallerContext::for_agent("ai:j8-bench");
    let now = chrono::Utc::now().to_rfc3339();
    for (i, id) in ids.iter().enumerate() {
        let mem = Memory {
            id: id.clone(),
            tier: Tier::Mid,
            namespace: namespace.to_string(),
            title: format!("j8-bench-{i:04}"),
            content: format!("J8 bench fixture node {i}"),
            tags: vec!["j8-fixture".to_string()],
            priority: 5,
            confidence: 1.0,
            source: "j8-bench".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now.clone(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id": "ai:j8-bench"}),
            // v0.7.0 Task 1/8 — substrate-native reflection depth. J8
            // bench fixture nodes are caller-equivalent (depth 0), so the
            // KG traversal benchmark targets the same shape it would in
            // production.
            reflection_depth: 0,
            memory_kind: ai_memory::models::MemoryKind::Observation,
            // v0.7.0 QW-2 + Form 4 + Form 5 fields. Bench fixtures use
            // defaults (no persona, no citations, caller-provided
            // confidence) — KG-traversal perf doesn't depend on these.
            entity_id: None,
            persona_version: None,
            citations: vec![],
            source_uri: None,
            source_span: None,
            confidence_source: ai_memory::models::ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
            version: 1,
        };
        store.store(&ctx, &mem).await?;
    }

    // Direct sqlx insert into memory_links (mirrors tests/age_cte_equivalence.rs
    // — `MemoryStore::link` returns UnsupportedCapability on the v0.7
    // Postgres adapter so the relational table is populated directly).
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(url)
        .await?;
    let base = chrono::Utc::now() - chrono::Duration::seconds(1000);
    let n = ids.len();
    let mut edge_idx: i64 = 0;
    for (i, src) in ids.iter().enumerate() {
        for k in 1..=4 {
            let dst = &ids[(i + k) % n];
            let valid_from = base + chrono::Duration::seconds(edge_idx);
            sqlx::query(
                "INSERT INTO memory_links (source_id, target_id, relation, valid_from, observed_by) \
                 VALUES ($1, $2, $3, $4, $5) \
                 ON CONFLICT (source_id, target_id, relation) DO UPDATE SET \
                     valid_from = EXCLUDED.valid_from, observed_by = EXCLUDED.observed_by",
            )
            .bind(src)
            .bind(dst)
            .bind("related_to")
            .bind(valid_from)
            .bind("ai:j8-bench")
            .execute(&pool)
            .await?;
            edge_idx += 1;
        }
    }
    Ok(())
}

/// Mirror the relational fixture into the AGE `memory_graph` projection
/// so `kg_query_cypher` has a graph to traverse. Same approach as
/// `tests/age_cte_equivalence.rs::project_fixture_into_age`. Returns
/// `Err(_)` when the projection isn't bootstrapped (no `memory_graph`,
/// or AGE not loadable in this session).
#[cfg(feature = "sal-postgres")]
async fn project_into_age(
    url: &str,
    ids: &[String],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
    let n = ids.len();
    let mut edge_idx: i64 = 0;
    for (i, src) in ids.iter().enumerate() {
        for k in 1..=4 {
            let dst = &ids[(i + k) % n];
            let valid_from = (base + chrono::Duration::seconds(edge_idx)).to_rfc3339();
            let cypher = "MATCH (a {id: $src}), (b {id: $dst}) \
                 MERGE (a)-[r:related_to {relation: $rel}]->(b) \
                 SET r.valid_from = $vf, r.observed_by = 'ai:j8-bench', \
                     r.created_at = $vf \
                 RETURN r";
            let sql = format!(
                "SELECT * FROM cypher('memory_graph', $$ {cypher} $$, $1::agtype) AS (r agtype)"
            );
            let params = serde_json::json!({
                "src": src,
                "dst": dst,
                "rel": "related_to",
                "vf": valid_from,
            })
            .to_string();
            sqlx::query(&sql).bind(params).fetch_all(&mut *tx).await?;
            edge_idx += 1;
        }
    }
    tx.commit().await?;
    Ok(())
}
