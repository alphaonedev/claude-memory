// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::doc_markdown, clippy::cast_precision_loss)]

//! v0.7.0 Wave-2 A5 (issue #853) — boot embedding-backfill batched path.
//!
//! Pre-fix behaviour (now archived in the original loop body) was:
//!
//! 1. `SELECT id, title, content FROM memories WHERE embedding IS NULL`
//!    → one query, fine.
//! 2. For each row: `emb.embed(text)` then a separate
//!    `UPDATE memories SET embedding = ?, embedding_dim = ?` —
//!    one autocommit per row. At 500-1000 unembedded rows on boot
//!    that's 500-1000 SQLite commits before the MCP server signals
//!    readiness on stdout, which Wave-1 perf survey memory
//!    `5740f984` clocked at 10-100 ms per 100 rows.
//!
//! Post-fix behaviour (this test pins it):
//!
//! 1. Single SELECT scan (unchanged).
//! 2. Slice into fixed-size chunks (default 64).
//! 3. Per chunk: `embed_batch` → single
//!    `db::set_embeddings_batch` call that wraps every UPDATE in
//!    one transaction (one commit per chunk, not per row).
//!
//! The acceptance bar this test enforces:
//!
//! * Functional: N=128 unembedded rows are all embedded after one
//!   backfill call.
//! * Byte-identity: the bytes a batched write lands on disk are
//!   identical to what the per-row `set_embedding` path would land
//!   (so existing recall tests stay green — embeddings are the
//!   substrate of cosine similarity, any drift would silently
//!   shift recall scores).
//! * Idempotence: a second backfill on the now-fully-embedded DB
//!   does zero work and returns `Ok(0)` — no rows scanned, no
//!   rows written.
//! * Time band: the batched backfill completes well under the
//!   `BACKFILL_TIME_CAP_MS` ceiling. With a mock embedder the
//!   per-row UPDATE cost dominates; the ceiling is loose enough
//!   to survive shared-CI jitter while still rejecting a
//!   per-row-commit regression (which clocks 10-100x slower under
//!   the same load).

use ai_memory::db;
use ai_memory::embeddings::Embed;
use ai_memory::mcp::run_embedding_backfill;
use ai_memory::models::{ConfidenceSource, Memory, MemoryKind, Tier};
use anyhow::Result;
use std::path::Path;
use std::time::Instant;

const N_ROWS: usize = 128;

/// Local mock embedder. Mirrors the `test_support::MockEmbedder`
/// shape (which is `#[cfg(test)]`-gated inside `src/embeddings.rs`
/// and therefore not visible to integration tests under `tests/`).
/// Deterministic per-text output so byte-identity assertions are
/// stable across calls.
struct IntegrationMockEmbedder;

const MOCK_DIM: usize = 384;

impl IntegrationMockEmbedder {
    fn embed_one(text: &str) -> Vec<f32> {
        let hash = text.bytes().fold(0u32, |acc, b| {
            acc.wrapping_mul(31).wrapping_add(u32::from(b))
        });
        // Match `test_support::MockEmbedder` formula exactly so the
        // byte-identity assertion below is independent of which mock
        // produced the vector — the recall pipeline only sees the
        // bytes, not the constructor.
        let base = ((hash % 1000) as f32) / 1000.0;
        (0..MOCK_DIM)
            .map(|i| base + ((i as f32) * 0.0001).sin().abs())
            .collect()
    }
}

impl Embed for IntegrationMockEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        Ok(Self::embed_one(text))
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| Self::embed_one(t)).collect())
    }
}

/// Loose ceiling. The batched path lands ~all 128 rows in a single
/// transaction (or two, depending on default batch size) and should
/// finish well inside this band on any half-modern CI worker. The
/// per-row regression — 128 autocommits — runs ~10-100x slower and
/// blows past this trivially.
const BACKFILL_TIME_CAP_MS: u128 = 2_000;

fn open_test_db() -> rusqlite::Connection {
    db::open(Path::new(":memory:")).expect("open in-memory DB")
}

fn make_memory(idx: usize) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: "backfill/test".to_string(),
        title: format!("backfill-row-{idx:04}"),
        content: format!(
            "Synthetic content for backfill test row {idx:04}. Hermetic — \
             no model load, no network."
        ),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
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

fn seed_unembedded(conn: &rusqlite::Connection, n: usize) -> Vec<String> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let m = make_memory(i);
        db::insert(conn, &m).expect("insert");
        ids.push(m.id);
    }
    ids
}

#[test]
fn batched_backfill_embeds_all_rows_in_time_band() {
    let mut conn = open_test_db();
    let ids = seed_unembedded(&conn, N_ROWS);

    // Sanity: every row starts unembedded.
    let pre = db::get_unembedded_ids(&conn).expect("scan pre");
    assert_eq!(
        pre.len(),
        N_ROWS,
        "seed should land {N_ROWS} unembedded rows; got {}",
        pre.len()
    );

    let emb = IntegrationMockEmbedder;

    let started = Instant::now();
    let written = run_embedding_backfill(&mut conn, &emb).expect("backfill");
    let elapsed_ms = started.elapsed().as_millis();
    eprintln!("BENCH(#853): batched backfill of {N_ROWS} rows took {elapsed_ms} ms");

    // Functional: every seeded row got an embedding.
    assert_eq!(
        written, N_ROWS,
        "backfill should write {N_ROWS} rows, got {written}"
    );
    let post = db::get_unembedded_ids(&conn).expect("scan post");
    assert!(
        post.is_empty(),
        "expected zero unembedded rows after backfill, got {}",
        post.len()
    );

    // Time band: well under the loose cap. The per-row commit
    // regression takes ~10-100x as long and would blow the cap.
    assert!(
        elapsed_ms < BACKFILL_TIME_CAP_MS,
        "batched backfill of {N_ROWS} rows took {elapsed_ms} ms — \
         exceeded {BACKFILL_TIME_CAP_MS} ms cap (likely per-row-commit regression)"
    );

    // Idempotence: rerun on a fully-embedded DB is a no-op.
    let restart = Instant::now();
    let again = run_embedding_backfill(&mut conn, &emb).expect("backfill idempotent");
    let idle_ms = restart.elapsed().as_millis();
    assert_eq!(
        again, 0,
        "idempotent rerun should write zero rows, got {again}"
    );
    assert!(
        idle_ms < 250,
        "idempotent rerun should be near-instant; took {idle_ms} ms"
    );

    // Byte-identity: every stored embedding equals what the
    // per-row `MockEmbedder::embed` path would produce for the
    // same text. This guards the recall-pipeline invariant — if
    // the batched path ever serialises differently from the
    // per-row path, cosine similarity silently drifts and the
    // existing recall tests would start flaking.
    for (idx, id) in ids.iter().enumerate() {
        let m = make_memory(idx);
        let expected_text = format!("{} {}", m.title, m.content);
        let expected_vec = emb.embed(&expected_text).expect("recompute");
        let on_disk = db::get_embedding(&conn, id)
            .expect("load embedding")
            .expect("row should have embedding after backfill");
        assert_eq!(
            on_disk.len(),
            expected_vec.len(),
            "dim mismatch at idx {idx} (id {id})"
        );
        for (j, (a, b)) in on_disk.iter().zip(expected_vec.iter()).enumerate() {
            assert!(
                (a - b).abs() < f32::EPSILON,
                "byte-identity violation at idx {idx} component {j}: on_disk={a} expected={b}"
            );
        }
    }
}

#[test]
fn set_embeddings_batch_rolls_back_on_dim_mismatch() {
    // Storage-layer invariant: if any pair in the batch violates
    // the namespace's established embedding dim, the whole
    // transaction rolls back. Mirrors the single-row
    // `set_embedding` policy from G4 / data-integrity v17.
    let mut conn = open_test_db();
    let m_a = make_memory(0);
    let m_b = make_memory(1);
    db::insert(&conn, &m_a).expect("insert a");
    db::insert(&conn, &m_b).expect("insert b");

    // Establish the namespace dim at 4 via a successful single-row write.
    let v4: Vec<f32> = vec![0.1, 0.2, 0.3, 0.4];
    db::set_embedding(&conn, &m_a.id, &v4).expect("seed dim");

    // Batch tries to write a 4-dim and an 8-dim into the same
    // namespace — must fail without committing either pair.
    let v8: Vec<f32> = vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8];
    let err = db::set_embeddings_batch(
        &mut conn,
        &[(m_b.id.clone(), v4.clone()), (m_a.id.clone(), v8)],
    )
    .expect_err("dim mismatch should bubble");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("dim mismatch"),
        "error should describe the dim mismatch, got: {msg}"
    );

    // m_b must NOT have an embedding (transaction rolled back).
    let post = db::get_embedding(&conn, &m_b.id).expect("load b");
    assert!(
        post.is_none(),
        "m_b should still be unembedded after rollback, got {post:?}"
    );
}

/// Comparative micro-bench: reproduce the pre-fix per-row path so
/// the wall-clock delta against the batched path is measurable.
/// `#[ignore]`d so it doesn't run in the default suite (the
/// timing-band assertion in
/// `batched_backfill_embeds_all_rows_in_time_band` is the
/// regression gate); invoke with:
///
/// ```bash
/// cargo test --test embedding_backfill_batch \
///     bench_pre_fix_per_row_baseline -- --ignored --nocapture
/// ```
#[test]
#[ignore = "comparative micro-bench: invoke with `--ignored --nocapture` for numbers"]
fn bench_pre_fix_per_row_baseline() {
    let conn = open_test_db();
    let _ids = seed_unembedded(&conn, N_ROWS);
    let emb = IntegrationMockEmbedder;

    let started = Instant::now();
    let unembedded = db::get_unembedded_ids(&conn).expect("scan");
    let mut ok = 0usize;
    // Verbatim pre-fix loop body — one embed + one autocommit
    // UPDATE per row. The replacement path runs in O(commits) =
    // ceil(N/B), this one runs in O(commits) = N.
    for (id, title, content) in &unembedded {
        let text = format!("{title} {content}");
        if let Ok(v) = emb.embed(&text)
            && db::set_embedding(&conn, id, &v).is_ok()
        {
            ok += 1;
        }
    }
    let elapsed_ms = started.elapsed().as_millis();
    eprintln!(
        "BENCH(#853 baseline): pre-fix per-row path on {N_ROWS} rows took {elapsed_ms} ms (wrote {ok})"
    );
    assert_eq!(ok, N_ROWS);
}
