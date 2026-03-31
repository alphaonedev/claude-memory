use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::process::Command;

fn binary_path() -> String {
    // Build the binary first, then find it
    let output = Command::new("cargo")
        .args(["build", "--release"])
        .output()
        .expect("failed to build binary");
    assert!(output.status.success(), "cargo build failed");

    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1", "--no-deps"])
        .output()
        .expect("failed to get cargo metadata");
    let meta: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let target_dir = meta["target_directory"].as_str().unwrap().to_string();
    format!("{}/release/claude-memory", target_dir)
}

fn seed_memories(binary: &str, db_path: &str, count: usize) {
    for i in 0..count {
        let title = format!("bench-memory-{}", i);
        let content = format!(
            "This is benchmark memory number {} with some searchable content about topic-{} and category-{}",
            i,
            i % 50,
            i % 10
        );
        let priority = ((i % 10) + 1).to_string();
        let output = Command::new(binary)
            .args([
                "--db",
                db_path,
                "store",
                "-t",
                "long",
                "-n",
                "bench",
                "-T",
                &title,
                "--content",
                &content,
                "-p",
                &priority,
            ])
            .output()
            .expect("failed to store memory");
        assert!(output.status.success(), "store failed for memory {}", i);
    }
}

fn bench_recall(c: &mut Criterion) {
    let binary = binary_path();
    let dir = std::env::temp_dir();
    let db_path = dir
        .join(format!(
            "claude-memory-bench-recall-{}.db",
            uuid::Uuid::new_v4()
        ))
        .to_str()
        .unwrap()
        .to_string();

    // Seed 1000 memories
    seed_memories(&binary, &db_path, 1000);

    let mut group = c.benchmark_group("recall");

    group.bench_function("short_query", |b| {
        b.iter(|| {
            let output = Command::new(&binary)
                .args([
                    "--db",
                    &db_path,
                    "--json",
                    "recall",
                    black_box("topic"),
                    "-n",
                    "bench",
                ])
                .output()
                .expect("recall failed");
            assert!(output.status.success());
        });
    });

    group.bench_function("medium_query", |b| {
        b.iter(|| {
            let output = Command::new(&binary)
                .args([
                    "--db",
                    &db_path,
                    "--json",
                    "recall",
                    black_box("benchmark memory searchable content"),
                    "-n",
                    "bench",
                ])
                .output()
                .expect("recall failed");
            assert!(output.status.success());
        });
    });

    group.bench_function("long_query", |b| {
        b.iter(|| {
            let output = Command::new(&binary)
                .args([
                    "--db", &db_path, "--json", "recall",
                    black_box("benchmark memory number with some searchable content about topic and category"),
                    "-n", "bench",
                ])
                .output()
                .expect("recall failed");
            assert!(output.status.success());
        });
    });

    group.finish();

    let _ = std::fs::remove_file(&db_path);
}

fn bench_search(c: &mut Criterion) {
    let binary = binary_path();
    let dir = std::env::temp_dir();
    let db_path = dir
        .join(format!(
            "claude-memory-bench-search-{}.db",
            uuid::Uuid::new_v4()
        ))
        .to_str()
        .unwrap()
        .to_string();

    seed_memories(&binary, &db_path, 1000);

    let mut group = c.benchmark_group("search");

    group.bench_function("simple_search", |b| {
        b.iter(|| {
            let output = Command::new(&binary)
                .args(["--db", &db_path, "--json", "search", black_box("topic")])
                .output()
                .expect("search failed");
            assert!(output.status.success());
        });
    });

    group.bench_function("filtered_search", |b| {
        b.iter(|| {
            let output = Command::new(&binary)
                .args([
                    "--db",
                    &db_path,
                    "--json",
                    "search",
                    black_box("category"),
                    "-n",
                    "bench",
                    "-t",
                    "long",
                ])
                .output()
                .expect("search failed");
            assert!(output.status.success());
        });
    });

    group.finish();

    let _ = std::fs::remove_file(&db_path);
}

fn bench_insert(c: &mut Criterion) {
    let binary = binary_path();
    let dir = std::env::temp_dir();
    let db_path = dir
        .join(format!(
            "claude-memory-bench-insert-{}.db",
            uuid::Uuid::new_v4()
        ))
        .to_str()
        .unwrap()
        .to_string();

    let mut group = c.benchmark_group("insert");
    let mut counter = 0u64;

    group.bench_function("store_memory", |b| {
        b.iter(|| {
            counter += 1;
            let title = format!("insert-bench-{}", counter);
            let output = Command::new(&binary)
                .args([
                    "--db",
                    &db_path,
                    "store",
                    "-t",
                    "mid",
                    "-n",
                    "bench-insert",
                    "-T",
                    &title,
                    "--content",
                    "Benchmark content for measuring insert throughput",
                    "-p",
                    "5",
                ])
                .output()
                .expect("store failed");
            assert!(output.status.success());
        });
    });

    group.finish();

    let _ = std::fs::remove_file(&db_path);
}

criterion_group!(benches, bench_recall, bench_search, bench_insert);
criterion_main!(benches);
