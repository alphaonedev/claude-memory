// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7 G9 — concurrent reranker throughput benchmark.
//!
//! Measures the wall-clock time for **N=8 concurrent recall reranks**
//! using two strategies:
//!
//! 1. `direct` — each request calls `CrossEncoder::rerank` independently.
//!    With the Neural variant this serializes through
//!    `Arc<Mutex<BertModel>>` per candidate, which is the regression
//!    G9 targets.
//! 2. `batched` — requests go through `BatchedReranker`, which
//!    coalesces concurrent calls into one tokenize + one forward pass
//!    per worker tick (max 32 in flight, 5ms flush window).
//!
//! Default model: `CrossEncoder::Lexical`. The lexical path runs
//! identically through both strategies, so the bench mostly reports
//! framing overhead — useful as a no-regression smoke. To exercise
//! the real (load-bearing) batching win, run with:
//!
//! ```bash
//! AI_MEMORY_BENCH_NEURAL=1 cargo bench --bench reranker_throughput
//! ```
//!
//! which downloads the ms-marco-MiniLM-L-6-v2 weights via hf-hub and
//! benchmarks the Neural cross-encoder. Expected: ~3× speedup of
//! `batched` over `direct` on an 8-core CPU. The bench exits with
//! non-zero on any panic but does not enforce a throughput threshold —
//! CI runs the lexical path only, neural is operator-driven.

use std::sync::Arc;
use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};

use ai_memory::models::{Memory, Tier};
use ai_memory::reranker::{BatchedReranker, CrossEncoder};

const N_QUERIES: usize = 8;
const CANDIDATES_PER_QUERY: usize = 10;

fn make_memory(i: usize) -> Memory {
    Memory {
        id: format!("bench-{i}"),
        tier: Tier::Mid,
        namespace: "bench".to_string(),
        title: format!("benchmark title number {i} alpha gamma"),
        content: format!(
            "this is a long-form benchmark document body number {i} that contains \
             enough material to give the cross-encoder something to chew on, including \
             alpha gamma rust async tokio reranker cross encoder bert minilm related \
             keywords {i} for variety and bigram diversity"
        ),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "bench".to_string(),
        access_count: 0,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
        // v0.7.0 Task 1/8 — substrate-native reflection depth. Benches
        // mint depth-0 (caller-equivalent) memories so the reranker
        // sees the same shape it would in production. Other Memory
        // fields added since (citations, source_uri, source_span,
        // confidence_*, mentioned_entity_id, entity_id, persona_version,
        // atomised_into, atom_of) take their Default values — none of
        // them affect reranker scoring or batched dispatch.
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        ..Memory::default()
    }
}

fn build_workload() -> Vec<(String, Vec<(Memory, f64)>)> {
    (0..N_QUERIES)
        .map(|q| {
            let cands: Vec<(Memory, f64)> = (0..CANDIDATES_PER_QUERY)
                .map(|c| (make_memory(q * CANDIDATES_PER_QUERY + c), 0.5))
                .collect();
            (format!("alpha gamma query {q}"), cands)
        })
        .collect()
}

fn build_encoder() -> CrossEncoder {
    if std::env::var("AI_MEMORY_BENCH_NEURAL").is_ok() {
        eprintln!(
            "reranker_throughput: building Neural cross-encoder (downloads ~80MB on first run)"
        );
        CrossEncoder::new_neural()
    } else {
        CrossEncoder::new()
    }
}

fn run_direct_concurrent(encoder: &Arc<CrossEncoder>) {
    let workload = build_workload();
    let mut handles = Vec::with_capacity(N_QUERIES);
    for (q, cands) in workload {
        let enc = Arc::clone(encoder);
        handles.push(std::thread::spawn(move || {
            let _ = enc.rerank(&q, cands);
        }));
    }
    for h in handles {
        h.join().expect("worker panicked");
    }
}

fn run_batched_concurrent(batched: &Arc<BatchedReranker>) {
    let workload = build_workload();
    let mut handles = Vec::with_capacity(N_QUERIES);
    for (q, cands) in workload {
        let b = Arc::clone(batched);
        handles.push(std::thread::spawn(move || {
            let _ = b.rerank(&q, cands);
        }));
    }
    for h in handles {
        h.join().expect("worker panicked");
    }
}

fn bench_throughput(c: &mut Criterion) {
    let encoder_direct = Arc::new(build_encoder());
    let encoder_batched = Arc::new(BatchedReranker::new(build_encoder()));

    // Smoke run: print wall-clock for each strategy so operators see
    // the absolute numbers in addition to criterion's distribution.
    let smoke_direct = {
        let t = Instant::now();
        run_direct_concurrent(&encoder_direct);
        t.elapsed()
    };
    let smoke_batched = {
        let t = Instant::now();
        run_batched_concurrent(&encoder_batched);
        t.elapsed()
    };
    eprintln!(
        "reranker_throughput smoke (N={N_QUERIES}, K={CANDIDATES_PER_QUERY}): direct={:.2?}, batched={:.2?}, ratio={:.2}x",
        smoke_direct,
        smoke_batched,
        smoke_direct.as_secs_f64() / smoke_batched.as_secs_f64().max(1e-9),
    );

    let mut group = c.benchmark_group("reranker_throughput");
    group.sample_size(20);
    group.bench_function("direct_concurrent_n8", |b| {
        b.iter(|| run_direct_concurrent(&encoder_direct));
    });
    group.bench_function("batched_concurrent_n8", |b| {
        b.iter(|| run_batched_concurrent(&encoder_batched));
    });
    group.finish();
}

criterion_group!(benches, bench_throughput);
criterion_main!(benches);
