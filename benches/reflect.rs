// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (bench scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown, clippy::cast_possible_truncation)]

//! v0.7.0 Task 7/8 (recursive-learning ship-gate) — `db::reflect_with_hooks`
//! end-to-end microbenchmark.
//!
//! Pins the v0.7.x+ regression baseline for the substrate-native
//! recursive-refinement primitive. Measures wall-clock per-call latency
//! at three source-fanout points (1, 3, 7) — the fanout dominates the
//! per-call cost because every source corresponds to one extra link
//! insert inside the same BEGIN IMMEDIATE … COMMIT block.
//!
//! ## What this bench measures
//!
//! For each fanout N ∈ {1, 3, 7}:
//!   1. Open a fresh in-memory SQLite via `db::open(":memory:")`.
//!   2. Insert N source memories.
//!   3. Time exactly one `db::reflect_with_hooks(_, _, &ReflectHooks::empty())`
//!      call producing one reflection memory + N `reflects_on` links.
//!
//! ## Why this bench is harness=false
//!
//! Matches the existing convention in `benches/`: every existing bench
//! uses `harness = false` and runs through Criterion's `criterion_main!`
//! macro. Sticking to that convention keeps the regression-baseline
//! parsing tools (e.g. `cargo benchcmp`) aligned with the other v0.7
//! benches.
//!
//! ## Run
//!
//! ```bash
//! cargo bench --bench reflect
//! ```
//!
//! The Criterion report under `target/criterion/reflect/*/` carries
//! the per-fanout histograms and the slope estimate. The summary text
//! lands on stdout.

use ai_memory::db::{self, ReflectHooks, ReflectInput};
use ai_memory::models::{Memory, Tier};
use chrono::Utc;
use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};

fn make_memory(namespace: &str, title: &str) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("bench source content: {title}"),
        tags: vec!["bench".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "system".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "bench-agent"}),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        // Remaining Memory fields (citations, confidence_*, mentioned_
        // entity_id, source_*, persona/atom fields) take their Default
        // values — none are read by the reflect benchmark path.
        ..Memory::default()
    }
}

fn bench_reflect_fanout(c: &mut Criterion) {
    let mut group = c.benchmark_group("reflect_with_hooks");
    for &fanout in &[1usize, 3, 7] {
        group.throughput(Throughput::Elements(fanout as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(fanout),
            &fanout,
            |b, &fanout| {
                b.iter_with_setup(
                    // Setup: fresh DB + N pre-inserted sources. Setup
                    // is NOT timed by Criterion's iter_with_setup —
                    // the per-call cost we measure is exclusively the
                    // reflect itself.
                    || {
                        let conn =
                            db::open(std::path::Path::new(":memory:")).expect("open in-memory db");
                        let mut source_ids = Vec::with_capacity(fanout);
                        for i in 0..fanout {
                            let mem = make_memory(
                                "bench-reflect",
                                &format!("src-{i}-{}", uuid::Uuid::new_v4().simple()),
                            );
                            let id = db::insert(&conn, &mem).expect("insert source");
                            source_ids.push(id);
                        }
                        (conn, source_ids)
                    },
                    |(conn, source_ids)| {
                        let input = ReflectInput {
                            source_ids,
                            title: format!("bench-refl-{}", uuid::Uuid::new_v4().simple()),
                            content: "synthesised bench reflection".to_string(),
                            namespace: Some("bench-reflect".to_string()),
                            tier: Tier::Mid,
                            tags: vec!["reflection".to_string()],
                            priority: 5,
                            confidence: 1.0,
                            source: "system".to_string(),
                            agent_id: "bench-agent".to_string(),
                            metadata: serde_json::json!({}),
                        };
                        let outcome = db::reflect_with_hooks(&conn, &input, &ReflectHooks::empty())
                            .expect("reflect must succeed");
                        black_box(outcome);
                    },
                );
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_reflect_fanout);
criterion_main!(benches);
