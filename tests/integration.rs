// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// Integration tests — all run through the CLI binary
//
// AI_MEMORY_NO_CONFIG=1 prevents loading ~/.config/ai-memory/config.toml
// which may set tier=autonomous and trigger embedder/LLM initialization.

fn cmd(binary: &str) -> std::process::Command {
    let mut c = std::process::Command::new(binary);
    c.env("AI_MEMORY_NO_CONFIG", "1");
    c
}

#[test]
fn test_cli_store_and_recall() {
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-cli-test-{}.db", uuid::Uuid::new_v4()));
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // Store
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-n",
            "test-project",
            "-T",
            "Rust is great",
            "--content",
            "Rust provides memory safety without garbage collection",
            "--tags",
            "rust,language",
            "-p",
            "8",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "store failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stored["tier"], "long");
    assert_eq!(stored["namespace"], "test-project");

    // Recall
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "recall",
            "Rust memory safety",
            "-n",
            "test-project",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "recall failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recalled: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(recalled["count"].as_u64().unwrap() >= 1);

    // Search
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "search",
            "Rust",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let searched: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(searched["count"].as_u64().unwrap() >= 1);

    // List
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "list"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let listed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(listed["count"].as_u64().unwrap() >= 1);

    // Stats
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "stats"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(stats["total"].as_u64().unwrap() >= 1);

    // Namespaces
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "namespaces"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let ns: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!ns["namespaces"].as_array().unwrap().is_empty());

    // Export
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "export"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let exported: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(exported["count"].as_u64().unwrap() >= 1);

    // Delete
    let id = stored["id"].as_str().unwrap();
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "delete", id])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Cleanup
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_deduplication() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-dedup-test-{}.db", uuid::Uuid::new_v4()));

    // Store same title+namespace twice
    for content in ["first version", "second version"] {
        let output = cmd(binary)
            .args([
                "--db",
                db_path.to_str().unwrap(),
                "--json",
                "store",
                "-T",
                "same title",
                "-n",
                "same-ns",
                "--content",
                content,
                "-p",
                "5",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
    }

    // Should only have 1 memory (deduped)
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "stats"])
        .output()
        .unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        stats["total"].as_u64().unwrap(),
        1,
        "deduplication failed — expected 1 memory"
    );

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_gc_removes_expired() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-gc-test-{}.db", uuid::Uuid::new_v4()));

    // Store a short-term memory (6h TTL) — we can't easily test real expiry,
    // but we can verify gc runs without error
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "short",
            "-T",
            "ephemeral thought",
            "--content",
            "goes away",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "gc"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let gc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // Not expired yet (6h TTL), so 0 deleted
    assert_eq!(gc["expired_deleted"].as_u64().unwrap(), 0);

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_content_size_limit() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-size-test-{}.db", uuid::Uuid::new_v4()));

    let huge_content = "x".repeat(70_000);
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "-T",
            "too big",
            "--content",
            &huge_content,
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "should reject oversized content");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_import_export_roundtrip() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db1 = dir.join(format!("ai-memory-export-{}.db", uuid::Uuid::new_v4()));
    let db2 = dir.join(format!("ai-memory-import-{}.db", uuid::Uuid::new_v4()));

    // Store in db1
    let output = cmd(binary)
        .args([
            "--db",
            db1.to_str().unwrap(),
            "store",
            "-t",
            "long",
            "-T",
            "portable memory",
            "--content",
            "travels between machines",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Export from db1
    let output = cmd(binary)
        .args(["--db", db1.to_str().unwrap(), "export"])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Import into db2
    let export_output = cmd(binary)
        .args(["--db", db1.to_str().unwrap(), "export"])
        .output()
        .unwrap();

    let mut child = cmd(binary)
        .args(["--db", db2.to_str().unwrap(), "--json", "import"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&export_output.stdout)
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "import failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    // Verify db2 has the memory
    let output = cmd(binary)
        .args(["--db", db2.to_str().unwrap(), "--json", "stats"])
        .output()
        .unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        stats["total"].as_u64().unwrap() >= 1,
        "import roundtrip failed"
    );

    let _ = std::fs::remove_file(&db1);
    let _ = std::fs::remove_file(&db2);
}

// --- Validation rejection tests ---

#[test]
fn test_reject_empty_title() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-val-title-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "-T",
            "",
            "--content",
            "some content",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "should reject empty title");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_reject_bad_source() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-val-source-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "-T",
            "test",
            "--content",
            "content",
            "-S",
            "invalid_source",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "should reject bad source");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_reject_bad_namespace() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-val-ns-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "-T",
            "test",
            "--content",
            "content",
            "-n",
            "bad namespace",
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "should reject namespace with spaces"
    );

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_reject_oversized_content() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-val-size-{}.db", uuid::Uuid::new_v4()));

    let huge = "x".repeat(70_000);
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "-T",
            "huge",
            "--content",
            &huge,
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "should reject oversized content");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_reject_bad_priority() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-val-prio-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "-T",
            "test",
            "--content",
            "content",
            "-p",
            "0",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "should reject priority 0");

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "-T",
            "test2",
            "--content",
            "content",
            "-p",
            "11",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "should reject priority 11");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_reject_bad_confidence() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-val-conf-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "-T",
            "test",
            "--content",
            "content",
            "--confidence",
            "1.5",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "should reject confidence > 1.0");

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "-T",
            "test2",
            "--content",
            "content",
            "--confidence",
            "-0.1",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "should reject confidence < 0.0");

    let _ = std::fs::remove_file(&db_path);
}

// --- Recall scoring order ---

#[test]
fn test_recall_priority_order() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-order-{}.db", uuid::Uuid::new_v4()));

    for (title, priority) in [
        ("alpha recall test", "2"),
        ("beta recall test", "9"),
        ("gamma recall test", "5"),
    ] {
        let output = cmd(binary)
            .args([
                "--db",
                db_path.to_str().unwrap(),
                "--json",
                "store",
                "-t",
                "long",
                "-n",
                "order-test",
                "-T",
                title,
                "--content",
                &format!("content about recall testing for {}", title),
                "-p",
                priority,
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "store failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "recall",
            "recall test",
            "-n",
            "order-test",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let recalled: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let memories = recalled["memories"].as_array().unwrap();
    assert!(memories.len() >= 2, "should recall at least 2 memories");
    // Highest priority (9) should come first
    let first_priority = memories[0]["priority"].as_i64().unwrap();
    let second_priority = memories[1]["priority"].as_i64().unwrap();
    assert!(
        first_priority >= second_priority,
        "higher priority should come first: {} vs {}",
        first_priority,
        second_priority
    );

    let _ = std::fs::remove_file(&db_path);
}

// --- TTL assignment ---

#[test]
fn test_ttl_assignment() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-ttl-{}.db", uuid::Uuid::new_v4()));

    // Store short-term
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "short",
            "-n",
            "ttl-test",
            "-T",
            "short lived",
            "--content",
            "expires soon",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let short: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        short["expires_at"].is_string(),
        "short-term should have expires_at"
    );

    // Store mid-term
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "mid",
            "-n",
            "ttl-test",
            "-T",
            "mid lived",
            "--content",
            "expires later",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let mid: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        mid["expires_at"].is_string(),
        "mid-term should have expires_at"
    );

    // Store long-term
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-n",
            "ttl-test",
            "-T",
            "long lived",
            "--content",
            "never expires",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let long: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        long["expires_at"].is_null(),
        "long-term should NOT have expires_at"
    );

    let _ = std::fs::remove_file(&db_path);
}

// --- Auto-promotion ---

#[test]
fn test_auto_promotion() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!(
        "ai-memory-promote-auto-{}.db",
        uuid::Uuid::new_v4()
    ));

    // Store a mid-term memory
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "mid",
            "-n",
            "promo-test",
            "-T",
            "promotable memory",
            "--content",
            "this memory should be promoted after enough accesses",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = stored["id"].as_str().unwrap().to_string();

    // Recall 6 times (promotion threshold is 5)
    for _ in 0..6 {
        let output = cmd(binary)
            .args([
                "--db",
                db_path.to_str().unwrap(),
                "--json",
                "recall",
                "promotable memory",
                "-n",
                "promo-test",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
    }

    // Verify it became long-term
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "get", &id])
        .output()
        .unwrap();
    assert!(output.status.success());
    let got: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        got["memory"]["tier"], "long",
        "memory should have been auto-promoted to long"
    );
    assert!(
        got["memory"]["expires_at"].is_null(),
        "promoted memory should have no expiry"
    );

    let _ = std::fs::remove_file(&db_path);
}

// --- Forget by pattern ---

#[test]
fn test_forget_by_pattern() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-forget-{}.db", uuid::Uuid::new_v4()));

    // Store 3 memories, 2 with "ephemeral" in content
    for (title, content) in [
        ("keep this one", "permanent important data"),
        ("forget alpha", "ephemeral data to remove"),
        ("forget beta", "ephemeral data to discard"),
    ] {
        let output = cmd(binary)
            .args([
                "--db",
                db_path.to_str().unwrap(),
                "store",
                "-t",
                "long",
                "-n",
                "forget-test",
                "-T",
                title,
                "--content",
                content,
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
    }

    // Verify 3 exist
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "stats"])
        .output()
        .unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stats["total"].as_u64().unwrap(), 3);

    // Forget by pattern
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "forget",
            "-p",
            "ephemeral",
            "-n",
            "forget-test",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let forgot: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        forgot["deleted"].as_u64().unwrap() >= 1,
        "should have deleted at least 1"
    );

    // Verify count decreased
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "stats"])
        .output()
        .unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        stats["total"].as_u64().unwrap() < 3,
        "total should have decreased"
    );

    let _ = std::fs::remove_file(&db_path);
}

// --- Namespace isolation ---

#[test]
fn test_namespace_isolation() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-nsiso-{}.db", uuid::Uuid::new_v4()));

    // Store in ns-a
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "-t",
            "long",
            "-n",
            "ns-a",
            "-T",
            "alpha secret data",
            "--content",
            "isolation test alpha content data",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Store in ns-b
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "-t",
            "long",
            "-n",
            "ns-b",
            "-T",
            "beta secret data",
            "--content",
            "isolation test beta content data",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Recall in ns-a should not return ns-b
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "recall",
            "secret data",
            "-n",
            "ns-a",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let recalled: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    for mem in recalled["memories"].as_array().unwrap() {
        assert_eq!(
            mem["namespace"].as_str().unwrap(),
            "ns-a",
            "namespace isolation broken: found ns-b memory in ns-a recall"
        );
    }

    let _ = std::fs::remove_file(&db_path);
}

// --- Link creation ---

#[test]
fn test_link_creation() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-link-{}.db", uuid::Uuid::new_v4()));

    // Store two memories
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-n",
            "link-test",
            "-T",
            "link source",
            "--content",
            "source content",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let src: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let src_id = src["id"].as_str().unwrap().to_string();

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-n",
            "link-test",
            "-T",
            "link target",
            "--content",
            "target content",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let tgt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let tgt_id = tgt["id"].as_str().unwrap().to_string();

    // Link them
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "link",
            &src_id,
            &tgt_id,
            "-r",
            "related_to",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Get source and verify links appear
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "get", &src_id])
        .output()
        .unwrap();
    assert!(output.status.success());
    let got: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let links = got["links"].as_array().unwrap();
    assert!(!links.is_empty(), "links should not be empty after linking");

    let _ = std::fs::remove_file(&db_path);
}

// --- Consolidation ---

#[test]
fn test_consolidation() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-consol-{}.db", uuid::Uuid::new_v4()));

    let mut ids = Vec::new();
    for (title, content) in [
        (
            "consol alpha",
            "first piece of knowledge about consolidation",
        ),
        (
            "consol beta",
            "second piece of knowledge about consolidation",
        ),
        (
            "consol gamma",
            "third piece of knowledge about consolidation",
        ),
    ] {
        let output = cmd(binary)
            .args([
                "--db",
                db_path.to_str().unwrap(),
                "--json",
                "store",
                "-t",
                "mid",
                "-n",
                "consol-test",
                "-T",
                title,
                "--content",
                content,
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        ids.push(stored["id"].as_str().unwrap().to_string());
    }

    // Verify 3 exist
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "stats"])
        .output()
        .unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stats["total"].as_u64().unwrap(), 3);

    // Consolidate
    let ids_str = ids.join(",");
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "consolidate",
            &ids_str,
            "-T",
            "consolidated knowledge",
            "-s",
            "all three pieces combined",
            "-n",
            "consol-test",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "consolidate failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify total decreased (3 removed, 1 added = 1)
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "stats"])
        .output()
        .unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        stats["total"].as_u64().unwrap() < 3,
        "total should have decreased after consolidation"
    );

    let _ = std::fs::remove_file(&db_path);
}

// --- Promote command ---

#[test]
fn test_promote_command() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-promote-cmd-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "mid",
            "-n",
            "promote-test",
            "-T",
            "to be promoted",
            "--content",
            "this will become long-term",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = stored["id"].as_str().unwrap().to_string();

    // Promote
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "promote", &id])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Verify tier=long
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "get", &id])
        .output()
        .unwrap();
    assert!(output.status.success());
    let got: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(got["memory"]["tier"], "long");

    let _ = std::fs::remove_file(&db_path);
}

// --- Namespaces command ---

#[test]
fn test_namespaces_command() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-ns-cmd-{}.db", uuid::Uuid::new_v4()));

    // Store in two namespaces
    for (ns, title) in [("ns-alpha", "alpha mem"), ("ns-beta", "beta mem")] {
        let output = cmd(binary)
            .args([
                "--db",
                db_path.to_str().unwrap(),
                "store",
                "-t",
                "long",
                "-n",
                ns,
                "-T",
                title,
                "--content",
                "test content",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
    }

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "namespaces"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let ns: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let namespaces = ns["namespaces"].as_array().unwrap();
    let ns_names: Vec<&str> = namespaces
        .iter()
        .map(|n| n["namespace"].as_str().unwrap())
        .collect();
    assert!(ns_names.contains(&"ns-alpha"), "should contain ns-alpha");
    assert!(ns_names.contains(&"ns-beta"), "should contain ns-beta");

    let _ = std::fs::remove_file(&db_path);
}

// --- Unicode handling ---

#[test]
fn test_unicode_handling() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-unicode-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-n",
            "unicode-test",
            "-T",
            "Memoria en espanol y japones",
            "--content",
            "Contenido con acentos: cafe, nino, resumen. Also Japanese: konnichiwa sekai",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "store with unicode failed");

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "recall",
            "espanol japones",
            "-n",
            "unicode-test",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let recalled: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        recalled["count"].as_u64().unwrap() >= 1,
        "should recall unicode memory"
    );

    let _ = std::fs::remove_file(&db_path);
}

// --- Boundary values ---

#[test]
fn test_boundary_priority_min() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-bnd-pmin-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-T",
            "min priority",
            "--content",
            "boundary test",
            "-p",
            "1",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "priority=1 should be valid");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_boundary_priority_max() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-bnd-pmax-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-T",
            "max priority",
            "--content",
            "boundary test",
            "-p",
            "10",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "priority=10 should be valid");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_boundary_confidence_zero() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-bnd-c0-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-T",
            "zero confidence",
            "--content",
            "boundary test",
            "--confidence",
            "0.0",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "confidence=0.0 should be valid");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_boundary_confidence_one() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-bnd-c1-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-T",
            "full confidence",
            "--content",
            "boundary test",
            "--confidence",
            "1.0",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "confidence=1.0 should be valid");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_boundary_max_title_length() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-bnd-tlen-{}.db", uuid::Uuid::new_v4()));

    let long_title = "a".repeat(512);
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-T",
            &long_title,
            "--content",
            "boundary test",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "512-char title should be valid");

    let _ = std::fs::remove_file(&db_path);
}

// --- Export includes links ---

#[test]
fn test_export_includes_links() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-explink-{}.db", uuid::Uuid::new_v4()));

    // Store two memories
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-n",
            "explink",
            "-T",
            "export link src",
            "--content",
            "source",
        ])
        .output()
        .unwrap();
    let src: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let src_id = src["id"].as_str().unwrap().to_string();

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-n",
            "explink",
            "-T",
            "export link tgt",
            "--content",
            "target",
        ])
        .output()
        .unwrap();
    let tgt: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let tgt_id = tgt["id"].as_str().unwrap().to_string();

    // Link them
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "link",
            &src_id,
            &tgt_id,
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Export
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "export"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let exported: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let links = exported["links"].as_array().unwrap();
    assert!(!links.is_empty(), "export should include links");

    let _ = std::fs::remove_file(&db_path);
}

// --- Import roundtrip with count match ---

#[test]
fn test_import_roundtrip_count_match() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db1 = dir.join(format!("ai-memory-irt-src-{}.db", uuid::Uuid::new_v4()));
    let db2 = dir.join(format!("ai-memory-irt-dst-{}.db", uuid::Uuid::new_v4()));

    // Store 3 memories in db1
    for i in 0..3 {
        let output = cmd(binary)
            .args([
                "--db",
                db1.to_str().unwrap(),
                "store",
                "-t",
                "long",
                "-n",
                "irt-test",
                "-T",
                &format!("roundtrip mem {}", i),
                "--content",
                &format!("content for roundtrip {}", i),
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
    }

    // Get source count
    let output = cmd(binary)
        .args(["--db", db1.to_str().unwrap(), "--json", "stats"])
        .output()
        .unwrap();
    let src_stats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let src_count = src_stats["total"].as_u64().unwrap();

    // Export from db1
    let export_output = cmd(binary)
        .args(["--db", db1.to_str().unwrap(), "export"])
        .output()
        .unwrap();
    assert!(export_output.status.success());

    // Import into db2
    let mut child = cmd(binary)
        .args(["--db", db2.to_str().unwrap(), "--json", "import"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&export_output.stdout)
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success());

    // Verify counts match
    let output = cmd(binary)
        .args(["--db", db2.to_str().unwrap(), "--json", "stats"])
        .output()
        .unwrap();
    let dst_stats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let dst_count = dst_stats["total"].as_u64().unwrap();
    assert_eq!(
        src_count, dst_count,
        "import count should match export count"
    );

    let _ = std::fs::remove_file(&db1);
    let _ = std::fs::remove_file(&db2);
}

// --- Update via CLI ---

#[test]
fn test_update_via_cli() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-update-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-n",
            "update-test",
            "-T",
            "original title",
            "--content",
            "original content",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = stored["id"].as_str().unwrap().to_string();

    // Update title
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "update",
            &id,
            "-T",
            "updated title",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Verify changed
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "get", &id])
        .output()
        .unwrap();
    assert!(output.status.success());
    let got: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(got["memory"]["title"], "updated title");

    let _ = std::fs::remove_file(&db_path);
}

// --- Stats accuracy ---

#[test]
fn test_stats_accuracy() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-statsacc-{}.db", uuid::Uuid::new_v4()));

    let count = 5;
    for i in 0..count {
        let output = cmd(binary)
            .args([
                "--db",
                db_path.to_str().unwrap(),
                "store",
                "-t",
                "long",
                "-n",
                "stats-test",
                "-T",
                &format!("stats mem {}", i),
                "--content",
                &format!("content {}", i),
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
    }

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "stats"])
        .output()
        .unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        stats["total"].as_u64().unwrap(),
        count,
        "stats total should match stored count"
    );

    let _ = std::fs::remove_file(&db_path);
}

// --- GC only removes expired ---

#[test]
fn test_gc_preserves_long_term() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-gckeep-{}.db", uuid::Uuid::new_v4()));

    // Store short-term and long-term
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "short",
            "-n",
            "gc-test",
            "-T",
            "short lived gc test",
            "--content",
            "will have TTL",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-n",
            "gc-test",
            "-T",
            "long lived gc test",
            "--content",
            "will persist forever",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let long_stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let long_id = long_stored["id"].as_str().unwrap().to_string();

    // Run GC (short hasn't expired yet, so nothing deleted)
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "gc"])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Verify long-term still exists
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "get", &long_id])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "long-term memory should survive GC"
    );

    let _ = std::fs::remove_file(&db_path);
}

// --- Search with --since ---

#[test]
fn test_search_with_since_future() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-since-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "-t",
            "long",
            "-n",
            "since-test",
            "-T",
            "searchable since test",
            "--content",
            "this should not appear with future since",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Search with --since far in the future
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "search",
            "searchable",
            "--since",
            "2099-01-01T00:00:00Z",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let results: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        results["count"].as_u64().unwrap(),
        0,
        "future --since should return 0 results"
    );

    let _ = std::fs::remove_file(&db_path);
}

// --- Health endpoint via HTTP ---

#[test]
fn test_health_endpoint() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-health-{}.db", uuid::Uuid::new_v4()));

    // Find a free port
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    // Start the server in the background
    let mut child = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "serve",
            "--port",
            &port.to_string(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    // Wait for server to start
    let url = format!("http://127.0.0.1:{}/api/v1/health", port);
    let mut ok = false;
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(output) = std::process::Command::new("curl")
            .args(["-s", "-o", "/dev/null", "-w", "%{http_code}", &url])
            .output()
        {
            let code = String::from_utf8_lossy(&output.stdout);
            if code == "200" {
                ok = true;
                break;
            }
        }
    }

    // Kill the server
    let _ = child.kill();
    let _ = child.wait();

    assert!(ok, "health endpoint should return 200");

    let _ = std::fs::remove_file(&db_path);
}

// === MCP Protocol Tests ===

#[test]
fn test_mcp_initialize() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-mcp-init-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(
                    stdin,
                    r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{}}}}"#
                )
                .ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("invalid JSON response");
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["serverInfo"]["name"], "ai-memory");
    assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
    assert!(resp["result"]["capabilities"]["tools"].is_object());

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_mcp_tools_list() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-mcp-tools-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(
                    stdin,
                    r#"{{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{{}}}}"#
                )
                .ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("invalid JSON response");
    let tools = resp["result"]["tools"]
        .as_array()
        .expect("tools should be array");
    assert_eq!(tools.len(), 26, "expected 26 MCP tools");

    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(tool_names.contains(&"memory_store"));
    assert!(tool_names.contains(&"memory_recall"));
    assert!(tool_names.contains(&"memory_search"));
    assert!(tool_names.contains(&"memory_list"));
    assert!(tool_names.contains(&"memory_delete"));
    assert!(tool_names.contains(&"memory_promote"));
    assert!(tool_names.contains(&"memory_forget"));
    assert!(tool_names.contains(&"memory_stats"));
    assert!(tool_names.contains(&"memory_update"));
    assert!(tool_names.contains(&"memory_get"));
    assert!(tool_names.contains(&"memory_link"));
    assert!(tool_names.contains(&"memory_get_links"));
    assert!(tool_names.contains(&"memory_consolidate"));

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_mcp_store_and_recall() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-mcp-store-{}.db", uuid::Uuid::new_v4()));

    // Send store then recall in sequence
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"memory_store","arguments":{{"title":"MCP test memory","content":"This was stored via MCP protocol","tier":"long","priority":8}}}}}}"#).ok();
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memory_recall","arguments":{{"context":"MCP test"}}}}}}"#).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 responses");

    // Verify store response
    let store_resp: serde_json::Value =
        serde_json::from_str(lines[0]).expect("invalid store response");
    assert_eq!(store_resp["id"], 1);
    assert!(
        store_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"id\"")
    );

    // Verify recall response
    let recall_resp: serde_json::Value =
        serde_json::from_str(lines[1]).expect("invalid recall response");
    assert_eq!(recall_resp["id"], 2);
    let recall_text = recall_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(recall_text.contains("MCP test memory"));

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_mcp_invalid_jsonrpc_version() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-mcp-ver-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(
                    stdin,
                    r#"{{"jsonrpc":"1.0","id":1,"method":"initialize","params":{{}}}}"#
                )
                .ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("invalid JSON response");
    assert!(
        resp["error"].is_object(),
        "expected error for invalid jsonrpc version"
    );
    assert_eq!(resp["error"]["code"], -32600);

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_mcp_unknown_tool() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-mcp-unk-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"nonexistent_tool","arguments":{{}}}}}}"#).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("invalid JSON response");
    // Tool errors come back as isError in MCP spec
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("unknown tool"), "expected unknown tool error");
    assert_eq!(resp["result"]["isError"], true);

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_mcp_missing_tool_name() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-mcp-noname-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"arguments":{{}}}}}}"#).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("invalid JSON response");
    assert!(
        resp["error"].is_object(),
        "expected error for missing tool name"
    );
    assert_eq!(resp["error"]["code"], -32602);

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_mcp_stats() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-mcp-stats-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"memory_stats","arguments":{{}}}}}}"#).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("invalid JSON response");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("total"));
    assert!(text.contains("by_tier"));

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_mcp_prompts_list() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-mcp-prompts-{}.db", uuid::Uuid::new_v4()));

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(
                    stdin,
                    r#"{{"jsonrpc":"2.0","id":1,"method":"prompts/list","params":{{}}}}"#
                )
                .ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("invalid JSON response");
    let prompts = resp["result"]["prompts"]
        .as_array()
        .expect("prompts should be array");
    assert_eq!(prompts.len(), 2);
    assert_eq!(prompts[0]["name"], "recall-first");
    assert_eq!(prompts[1]["name"], "memory-workflow");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_mcp_prompts_get_recall_first() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!(
        "ai-memory-mcp-prompt-get-{}.db",
        uuid::Uuid::new_v4()
    ));

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"prompts/get","params":{{"name":"recall-first"}}}}"#).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let resp: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("invalid JSON response");
    let messages = resp["result"]["messages"]
        .as_array()
        .expect("messages should be array");
    assert_eq!(messages.len(), 1);
    let text = messages[0]["content"]["text"]
        .as_str()
        .expect("text content");
    assert!(
        text.contains("RECALL FIRST"),
        "should contain recall-first rule"
    );
    assert!(text.contains("TOON"), "should mention TOON format");
    assert!(
        text.contains("memory_recall"),
        "should reference memory_recall tool"
    );

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_mcp_recall_default_toon() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!(
        "ai-memory-mcp-toon-def-{}.db",
        uuid::Uuid::new_v4()
    ));

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"memory_store","arguments":{{"title":"TOON default test","content":"Testing.","tier":"long","namespace":"test"}}}}}}"#).ok();
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memory_recall","arguments":{{"context":"TOON test","namespace":"test"}}}}}}"#).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines.len() >= 2,
        "expected >=2 responses, got {}",
        lines.len()
    );

    let recall_resp: serde_json::Value =
        serde_json::from_str(lines[1]).expect("invalid recall response");
    let text = recall_resp["result"]["content"][0]["text"]
        .as_str()
        .expect("recall text");
    assert!(
        text.contains("memories[") || text.starts_with("count:"),
        "default should be TOON compact, got: {}",
        &text[..text.len().min(100)]
    );
    assert!(text.contains("|"), "should contain pipe delimiters");

    let _ = std::fs::remove_file(&db_path);
}

// --- Patch 2 (v0.5.4.2) tests ---

#[test]
fn test_cli_validate_id_rejects_invalid() {
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-validate-id-{}.db", uuid::Uuid::new_v4()));
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // delete with empty/whitespace ID
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "delete", "   "])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "should reject empty/whitespace ID"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("id cannot be empty"), "stderr: {}", stderr);

    // update with empty ID
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "update",
            "  ",
            "--content",
            "test",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success(), "should reject empty ID on update");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_tier_downgrade_rejected() {
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!(
        "ai-memory-tier-downgrade-{}.db",
        uuid::Uuid::new_v4()
    ));
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // Store a long-term memory
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-T",
            "Important Fact",
            "--content",
            "This is permanent",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = stored["id"].as_str().unwrap();

    // Attempt to downgrade to short — silently clamped, tier stays long
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "update",
            id,
            "--tier",
            "short",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "update should succeed (silent clamp, not error)"
    );

    // Verify memory is still long-term (downgrade was silently blocked)
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "get", id])
        .output()
        .unwrap();
    let got: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(got["memory"]["tier"].as_str().unwrap(), "long");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_tier_upgrade_allowed() {
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!(
        "ai-memory-tier-upgrade-{}.db",
        uuid::Uuid::new_v4()
    ));
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // Store a short-term memory
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "short",
            "-T",
            "Temp Note",
            "--content",
            "Upgrade me",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = stored["id"].as_str().unwrap();

    // Upgrade to long
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "update",
            id,
            "--tier",
            "long",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "should allow short→long upgrade");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_duplicate_title_no_self_contradiction() {
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-selfref-{}.db", uuid::Uuid::new_v4()));
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // Store a memory
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-T",
            "Dupe Test",
            "--content",
            "Original",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Store again with same title (upsert)
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-T",
            "Dupe Test",
            "--content",
            "Updated via upsert",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // Should NOT have potential_contradictions pointing to self
    if let Some(contras) = stored["potential_contradictions"].as_array() {
        let id = stored["id"].as_str().unwrap();
        for c in contras {
            assert_ne!(c.as_str().unwrap(), id, "self-contradiction detected");
        }
    }

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_promote_clears_expires_at() {
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-promote-{}.db", uuid::Uuid::new_v4()));
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // Store a short-term memory (has expires_at)
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "short",
            "-T",
            "Promote Expiry",
            "--content",
            "Should clear expiry",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = stored["id"].as_str().unwrap();
    assert!(
        stored["expires_at"].as_str().is_some(),
        "short should have expires_at"
    );

    // Promote
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "promote", id])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Verify expires_at is cleared
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "get", id])
        .output()
        .unwrap();
    let got: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(got["memory"]["tier"].as_str().unwrap(), "long");
    assert!(
        got["memory"]["expires_at"].is_null(),
        "expires_at should be null after promote, got: {:?}",
        got["memory"]["expires_at"]
    );

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_version_flag_patch6() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let output = cmd(binary).args(["--version"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0.5.4-patch.6"),
        "version should be 0.5.4-patch.6, got: {}",
        stdout
    );
}

// --- Patch 4: auto-detect parent by prefix ---

#[test]
fn test_namespace_auto_detect_parent() {
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!(
        "ai-memory-ns-autoparent-{}.db",
        uuid::Uuid::new_v4()
    ));
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // Store parent namespace standard
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-n",
            "myproject",
            "-T",
            "Project Standard",
            "--content",
            "Project-level rules",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let parent_stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let parent_id = parent_stored["id"].as_str().unwrap().to_string();

    // Store child namespace standard
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-n",
            "myproject-tests",
            "-T",
            "Test Standard",
            "--content",
            "Test-level rules",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let child_stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let child_id = child_stored["id"].as_str().unwrap().to_string();

    // Set parent standard first, then child (no explicit parent — should auto-detect)
    let mcp_input = format!(
        "{}\n{}\n{}\n{}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memory_namespace_set_standard","arguments":{{"namespace":"myproject","id":"{}"}}}}}}"#,
            parent_id
        ),
        format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"memory_namespace_set_standard","arguments":{{"namespace":"myproject-tests","id":"{}"}}}}}}"#,
            child_id
        ),
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory_recall","arguments":{"context":"rules","namespace":"myproject-tests","format":"json"}}}"#,
    );

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(mcp_input.as_bytes()).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines.len() >= 4,
        "expected 4 MCP responses, got {}",
        lines.len()
    );

    // Parse recall response — should have "standards" array with both parent and child
    let recall_resp: serde_json::Value = serde_json::from_str(lines[3]).unwrap();
    let recall_text = recall_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    let recall_data: serde_json::Value = serde_json::from_str(recall_text).unwrap();

    // Should have standards array (parent + child = 2 levels)
    assert!(
        recall_data.get("standards").is_some(),
        "recall should include 'standards' array for multi-level layering, got: {}",
        recall_data
    );
    let standards = recall_data["standards"].as_array().unwrap();
    assert_eq!(
        standards.len(),
        2,
        "should have 2 standards (parent + child)"
    );

    // First should be parent (broader scope first)
    assert_eq!(standards[0]["title"], "Project Standard");
    // Second should be child (more specific)
    assert_eq!(standards[1]["title"], "Test Standard");

    let _ = std::fs::remove_file(&db_path);
}

// --- Patch 3: namespace standard auto-prepend via MCP ---

#[test]
fn test_mcp_namespace_standard_auto_prepend() {
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-ns-auto-{}.db", uuid::Uuid::new_v4()));
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // Store a standard memory
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-n",
            "test-ns",
            "-T",
            "NS Standard",
            "--content",
            "Follow these rules",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let std_id = stored["id"].as_str().unwrap().to_string();

    // Store another memory in same namespace
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-n",
            "test-ns",
            "-T",
            "Regular memory",
            "--content",
            "Some content",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    // Set standard via MCP, then recall with namespace
    let mcp_input = format!(
        "{}\n{}\n{}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memory_namespace_set_standard","arguments":{{"namespace":"test-ns","id":"{}"}}}}}}"#,
            std_id
        ),
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_recall","arguments":{"context":"rules","namespace":"test-ns","format":"json"}}}"#,
    );

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(mcp_input.as_bytes()).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines.len() >= 3,
        "expected 3 MCP responses, got {}",
        lines.len()
    );

    // Parse recall response (line 3)
    let recall_resp: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    let recall_text = recall_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    let recall_data: serde_json::Value = serde_json::from_str(recall_text).unwrap();

    // Standard should be present as a separate field
    assert!(
        recall_data.get("standard").is_some(),
        "recall should include 'standard' field"
    );
    assert_eq!(recall_data["standard"]["id"], std_id);
    assert_eq!(recall_data["standard"]["title"], "NS Standard");

    // Standard should NOT be duplicated in the memories array
    if let Some(memories) = recall_data["memories"].as_array() {
        for m in memories {
            assert_ne!(
                m["id"].as_str().unwrap_or(""),
                &std_id,
                "standard should be deduplicated from memories array"
            );
        }
    }

    let _ = std::fs::remove_file(&db_path);
}

// --- Patch 3: cascade cleanup on delete ---

#[test]
fn test_namespace_standard_cascade_on_delete() {
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-ns-cascade-{}.db", uuid::Uuid::new_v4()));
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // Store and set as standard
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-n",
            "cascade-ns",
            "-T",
            "Will be deleted",
            "--content",
            "Standard to delete",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let std_id = stored["id"].as_str().unwrap().to_string();

    // Set standard, then delete the memory, then get standard
    let mcp_input = format!(
        "{}\n{}\n{}\n{}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memory_namespace_set_standard","arguments":{{"namespace":"cascade-ns","id":"{}"}}}}}}"#,
            std_id
        ),
        format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"memory_delete","arguments":{{"id":"{}"}}}}}}"#,
            std_id
        ),
        r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory_namespace_get_standard","arguments":{"namespace":"cascade-ns"}}}"#,
    );

    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                stdin.write_all(mcp_input.as_bytes()).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines.len() >= 4,
        "expected 4 MCP responses, got {}",
        lines.len()
    );

    // After delete, get_standard should return null (cascade cleaned up)
    let get_resp: serde_json::Value = serde_json::from_str(lines[3]).unwrap();
    let get_text = get_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    let get_data: serde_json::Value = serde_json::from_str(get_text).unwrap();
    assert!(
        get_data["standard_id"].is_null(),
        "standard should be null after deleting the standard memory, got: {}",
        get_data
    );

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_mcp_store_with_metadata() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-mcp-meta-{}.db", uuid::Uuid::new_v4()));

    // Store with metadata, then recall in JSON format to verify it persists
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"memory_store","arguments":{{"title":"Metadata MCP test","content":"Testing metadata via MCP","tier":"long","metadata":{{"agent_id":"claude-test","session":42}}}}}}}}"#).ok();
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memory_recall","arguments":{{"context":"Metadata MCP test","format":"json"}}}}}}"#).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 responses, got: {stdout}");

    // Parse store response to get the ID
    let store_resp: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let store_text = store_resp["result"]["content"][0]["text"].as_str().unwrap();
    let store_data: serde_json::Value = serde_json::from_str(store_text).unwrap();
    assert!(store_data["id"].is_string(), "store should return an id");

    // Parse recall response — should contain metadata
    let recall_resp: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let recall_text = recall_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let recall_data: serde_json::Value = serde_json::from_str(recall_text).unwrap();
    let memories = recall_data["memories"].as_array().unwrap();
    assert!(!memories.is_empty(), "recall should return results");
    assert_eq!(memories[0]["metadata"]["agent_id"], "claude-test");
    assert_eq!(memories[0]["metadata"]["session"], 42);

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_mcp_update_metadata() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!(
        "ai-memory-mcp-meta-upd-{}.db",
        uuid::Uuid::new_v4()
    ));

    // Store with initial metadata
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"memory_store","arguments":{{"title":"Update meta test","content":"Initial content","tier":"long","metadata":{{"version":1}}}}}}}}"#).ok();
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memory_recall","arguments":{{"context":"Update meta test","format":"json"}}}}}}"#).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 2, "expected 2 responses");

    // Get the stored ID
    let store_resp: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let store_text = store_resp["result"]["content"][0]["text"].as_str().unwrap();
    let store_data: serde_json::Value = serde_json::from_str(store_text).unwrap();
    let id = store_data["id"].as_str().unwrap();

    // Update metadata via a second MCP session, then get to verify
    let output2 = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"memory_update","arguments":{{"id":"{}","metadata":{{"version":2,"updated":true}}}}}}}}"#, id).ok();
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"memory_get","arguments":{{"id":"{}"}}}}}}"#, id).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout2 = String::from_utf8_lossy(&output2.stdout);
    let lines2: Vec<&str> = stdout2.trim().lines().collect();
    assert_eq!(lines2.len(), 2, "expected 2 responses from update session");

    // Verify update succeeded
    let update_resp: serde_json::Value = serde_json::from_str(lines2[0]).unwrap();
    let update_text = update_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let update_data: serde_json::Value = serde_json::from_str(update_text).unwrap();
    assert_eq!(update_data["updated"], true);

    // Verify get returns new metadata
    let get_resp: serde_json::Value = serde_json::from_str(lines2[1]).unwrap();
    let get_text = get_resp["result"]["content"][0]["text"].as_str().unwrap();
    let get_data: serde_json::Value = serde_json::from_str(get_text).unwrap();
    assert_eq!(get_data["metadata"]["version"], 2);
    assert_eq!(get_data["metadata"]["updated"], true);

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_mcp_store_invalid_metadata_defaults_to_empty() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!(
        "ai-memory-mcp-meta-inv-{}.db",
        uuid::Uuid::new_v4()
    ));

    // Store with metadata as array (invalid — should default to {})
    // Then store with metadata as string (invalid — should default to {})
    // Then store with metadata as null (invalid — should default to {})
    // Verify all three have empty metadata
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                // metadata as array
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"memory_store","arguments":{{"title":"Array meta","content":"test","tier":"long","metadata":[1,2,3]}}}}}}"#).ok();
                // metadata as string
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memory_store","arguments":{{"title":"String meta","content":"test","tier":"long","metadata":"not an object"}}}}}}"#).ok();
                // metadata as null
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"memory_store","arguments":{{"title":"Null meta","content":"test","tier":"long","metadata":null}}}}}}"#).ok();
                // Recall all to verify
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"memory_list","arguments":{{"format":"json"}}}}}}"#).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 4, "expected 4 responses, got: {stdout}");

    // All three stores should succeed (invalid metadata silently defaults to {})
    for i in 0..3 {
        let resp: serde_json::Value = serde_json::from_str(lines[i]).unwrap();
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let data: serde_json::Value = serde_json::from_str(text).unwrap();
        assert!(
            data["id"].is_string(),
            "store {} should succeed, got: {}",
            i + 1,
            text
        );
    }

    // List should show all 3 with metadata that contains ONLY the NHI-hardened
    // agent_id injected by handle_store (Task 1.2). The invalid input metadata
    // is replaced with `{}` first, then agent_id is added.
    let list_resp: serde_json::Value = serde_json::from_str(lines[3]).unwrap();
    let list_text = list_resp["result"]["content"][0]["text"].as_str().unwrap();
    let list_data: serde_json::Value = serde_json::from_str(list_text).unwrap();
    let memories = list_data["memories"].as_array().unwrap();
    assert_eq!(memories.len(), 3);
    for mem in memories {
        let meta = mem["metadata"]
            .as_object()
            .unwrap_or_else(|| panic!("metadata must be an object, got: {}", mem["metadata"]));
        assert_eq!(
            meta.len(),
            1,
            "invalid input metadata should reduce to just agent_id, got: {:?}",
            meta
        );
        assert!(
            meta.contains_key("agent_id"),
            "agent_id must be present after injection, got: {:?}",
            meta
        );
        let id = meta["agent_id"].as_str().unwrap_or_default();
        assert!(
            !id.is_empty() && (id.starts_with("host:") || id.starts_with("anonymous:")),
            "injected agent_id must use NHI-prefixed default, got: {id}"
        );
    }

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_mcp_dedup_replaces_metadata() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!(
        "ai-memory-mcp-meta-dup-{}.db",
        uuid::Uuid::new_v4()
    ));

    // Store with metadata v1, then store same title+namespace with metadata v2
    // The MCP dedup path goes through db::update, not db::insert upsert
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "mcp"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"memory_store","arguments":{{"title":"Dedup meta test","content":"first","tier":"long","namespace":"test","metadata":{{"version":1}}}}}}}}"#).ok();
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memory_store","arguments":{{"title":"Dedup meta test","content":"second","tier":"long","namespace":"test","metadata":{{"version":2,"extra":"added"}}}}}}}}"#).ok();
                writeln!(stdin, r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"memory_recall","arguments":{{"context":"Dedup meta test","namespace":"test","format":"json"}}}}}}"#).ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert_eq!(lines.len(), 3, "expected 3 responses, got: {stdout}");

    // Second store should indicate dedup
    let store2: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let store2_text = store2["result"]["content"][0]["text"].as_str().unwrap();
    let store2_data: serde_json::Value = serde_json::from_str(store2_text).unwrap();
    assert_eq!(store2_data["duplicate"], true);

    // Recall should return the memory with v2 metadata
    let recall: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    let recall_text = recall["result"]["content"][0]["text"].as_str().unwrap();
    let recall_data: serde_json::Value = serde_json::from_str(recall_text).unwrap();
    let memories = recall_data["memories"].as_array().unwrap();
    assert!(!memories.is_empty());
    assert_eq!(memories[0]["metadata"]["version"], 2);
    assert_eq!(memories[0]["metadata"]["extra"], "added");
}

#[test]
fn test_cli_prefix_id_resolution() {
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-prefix-test-{}.db", uuid::Uuid::new_v4()));
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // Store a memory
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-t",
            "long",
            "-T",
            "Prefix resolution test",
            "--content",
            "Testing that short IDs work with get, update, promote, and delete",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "store failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let full_id = stored["id"].as_str().unwrap().to_string();
    let short_id = &full_id[..8];

    // Get by short prefix
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "get", short_id])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "get by prefix failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let got: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(got["memory"]["id"], full_id);
    assert_eq!(got["memory"]["title"], "Prefix resolution test");

    // Update by short prefix
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "update",
            short_id,
            "--content",
            "Updated via prefix",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "update by prefix failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify updated content
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "get", &full_id])
        .output()
        .unwrap();
    assert!(output.status.success());
    let got: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(got["memory"]["content"], "Updated via prefix");

    // Delete by short prefix
    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "delete",
            short_id,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "delete by prefix failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify deleted
    let output = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "get", &full_id])
        .output()
        .unwrap();
    assert!(!output.status.success(), "get after delete should fail");

    let _ = std::fs::remove_file(&db_path);
}

// ===========================================================================
// Task 1.2 — Agent Identity in Metadata (NHI-hardened)
// ===========================================================================

/// Helper: fresh DB path for each test.
fn fresh_db() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ai-memory-agentid-{}.db", uuid::Uuid::new_v4()))
}

/// Helper: extract `metadata.agent_id` from a stored-memory JSON payload.
fn agent_id_of(v: &serde_json::Value) -> String {
    v["metadata"]["agent_id"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

#[test]
fn test_agentid_explicit_flag_wins() {
    let db_path = fresh_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "alice",
            "--json",
            "store",
            "-T",
            "nhi-explicit",
            "-c",
            "hi",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "store failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(agent_id_of(&stored), "alice");
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_agentid_default_is_nhi_prefixed() {
    let db_path = fresh_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // Ensure no env override leaks in.
    let output = cmd(binary)
        .env_remove("AI_MEMORY_AGENT_ID")
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-T",
            "nhi-default",
            "-c",
            "hi",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = agent_id_of(&stored);
    // One of: host:<sanitized-hostname>:pid-<pid>-<uuid8>
    //      or anonymous:pid-<pid>-<uuid8>
    assert!(
        id.starts_with("host:") || id.starts_with("anonymous:"),
        "expected NHI-prefixed default, got: {id}"
    );
    assert!(
        id.contains(":pid-"),
        "expected pid discriminator, got: {id}"
    );
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_agentid_env_var_supplies_default() {
    let db_path = fresh_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    let output = cmd(binary)
        .env("AI_MEMORY_AGENT_ID", "charlie")
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-T",
            "nhi-env",
            "-c",
            "hi",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(agent_id_of(&stored), "charlie");
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_agentid_list_filter() {
    let db_path = fresh_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    for (agent, title) in [("alice", "nhi-list-a"), ("bob", "nhi-list-b")] {
        let out = cmd(binary)
            .args([
                "--db",
                db_path.to_str().unwrap(),
                "--agent-id",
                agent,
                "--json",
                "store",
                "-T",
                title,
                "-c",
                "content",
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "store {agent} failed");
    }

    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "list",
            "--agent-id",
            "alice",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let resp: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let memories = resp["memories"].as_array().expect("memories array");
    assert_eq!(memories.len(), 1, "should return only alice's memory");
    assert_eq!(agent_id_of(&memories[0]), "alice");
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_agentid_search_filter() {
    let db_path = fresh_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    for (agent, title) in [("alice", "nhi_search_a"), ("bob", "nhi_search_b")] {
        let out = cmd(binary)
            .args([
                "--db",
                db_path.to_str().unwrap(),
                "--agent-id",
                agent,
                "--json",
                "store",
                "-T",
                title,
                "-c",
                "NhiSearchableToken body text",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "store {agent} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "search",
            "NhiSearchableToken",
            "--agent-id",
            "bob",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let resp: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let results = resp["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1, "should return only bob's result");
    assert_eq!(agent_id_of(&results[0]), "bob");
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_agentid_update_preserves_provenance() {
    let db_path = fresh_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "alice",
            "--json",
            "store",
            "-T",
            "nhi-update",
            "-c",
            "v1",
        ])
        .output()
        .unwrap();
    let stored: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = stored["id"].as_str().unwrap().to_string();
    assert_eq!(agent_id_of(&stored), "alice");

    // Update content as a different agent (content + confidence) — agent_id must not change.
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "bob",
            "update",
            &id,
            "-c",
            "v2",
            "--confidence",
            "0.9",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = cmd(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "get", &id])
        .output()
        .unwrap();
    let got: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let mem = &got["memory"];
    assert_eq!(agent_id_of(mem), "alice", "provenance must be immutable");
    assert_eq!(mem["content"], "v2");
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_agentid_dedup_preserves_original() {
    let db_path = fresh_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // First store with alice.
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "alice",
            "--json",
            "store",
            "-T",
            "nhi-dedup",
            "-n",
            "ns-dedup",
            "-c",
            "original",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    // Second store with bob under same title+namespace (triggers dedup).
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "bob",
            "--json",
            "store",
            "-T",
            "nhi-dedup",
            "-n",
            "ns-dedup",
            "-c",
            "updated-by-bob",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    // List: should be exactly 1 memory, agent_id still alice.
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "list",
            "-n",
            "ns-dedup",
        ])
        .output()
        .unwrap();
    let resp: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let memories = resp["memories"].as_array().unwrap();
    assert_eq!(memories.len(), 1);
    assert_eq!(agent_id_of(&memories[0]), "alice");
    assert_eq!(memories[0]["content"], "updated-by-bob");
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_agentid_validator_rejects_bad_input() {
    let db_path = fresh_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // Shell metacharacter must be rejected before any DB write.
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "alice;rm",
            "store",
            "-T",
            "nhi-bad-agent",
            "-c",
            "c",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "expected rejection for metachar");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        err.contains("agent_id"),
        "expected agent_id error, got: {err}"
    );

    // Whitespace must be rejected.
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "alice bob",
            "store",
            "-T",
            "nhi-bad-agent-ws",
            "-c",
            "c",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "expected rejection for whitespace");
    let _ = std::fs::remove_file(&db_path);
}

/// Regression test for a red-team finding (T-3 from the 2026-04-16 assessment):
/// the MCP `memory_update` tool accepted a metadata object that overwrote
/// `metadata.agent_id`, letting any caller rewrite the recorded author of an
/// existing memory — bypassing the immutability invariant documented in the
/// NHI design. Verified fixed by wiring `identity::preserve_agent_id` into
/// `mcp::handle_update` alongside the existing store/dedup/HTTP paths.
#[test]
fn test_mcp_update_preserves_agent_id() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let db_path = fresh_db();

    // 1. CLI store with alice as author
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "alice",
            "--json",
            "store",
            "-T",
            "mcp-update-preserve",
            "-c",
            "v1",
        ])
        .output()
        .unwrap();
    let stored: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = stored["id"].as_str().unwrap().to_string();
    assert_eq!(agent_id_of(&stored), "alice");

    // 2. MCP memory_update with metadata including a hostile agent_id
    let req1 = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let req2 = format!(
        r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"memory_update","arguments":{{"id":"{id}","metadata":{{"agent_id":"attacker","side_field":"ok"}}}}}}}}"#,
    );

    let output = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "mcp",
            "--tier",
            "keyword",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(ref mut stdin) = child.stdin {
                writeln!(stdin, "{req1}").ok();
                writeln!(stdin, "{req2}").ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    // 3. Parse the MCP response and assert provenance held.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let last_line = stdout.trim().lines().last().unwrap();
    let resp: serde_json::Value = serde_json::from_str(last_line).unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let data: serde_json::Value = serde_json::from_str(text).unwrap();
    let returned_meta = &data["memory"]["metadata"];

    assert_eq!(
        returned_meta["agent_id"], "alice",
        "agent_id must be preserved across MCP update, got: {returned_meta}"
    );
    assert_eq!(
        returned_meta["side_field"], "ok",
        "other metadata fields must still be writable"
    );

    let _ = std::fs::remove_file(&db_path);
}

// ===========================================================================
// Regression tests for the systematic sweep that followed T-3 discovery.
// Each of these found a new real gap in additional metadata-writing paths.
// ===========================================================================

/// GAP 1: Import forgery — an attacker-crafted JSON file could claim any
/// `metadata.agent_id` because `cmd_import` blindly trusted the imported
/// value. Fix: restamp with caller's id by default; `--trust-source` preserves.
#[test]
fn test_import_restamps_agent_id_by_default() {
    let db_path = fresh_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let forge_path = std::env::temp_dir().join(format!("forge-{}.json", uuid::Uuid::new_v4()));

    let forged = serde_json::json!({
        "memories": [{
            "id": "a0000000-0000-0000-0000-000000000001",
            "tier": "long",
            "namespace": "forge",
            "title": "forgery",
            "content": "claim",
            "tags": [],
            "priority": 9,
            "confidence": 1.0,
            "source": "user",
            "access_count": 0,
            "created_at": "2026-04-16T10:00:00+00:00",
            "updated_at": "2026-04-16T10:00:00+00:00",
            "metadata": {"agent_id": "alphaonedev@admin", "other": "data"}
        }],
        "links": []
    });
    std::fs::write(&forge_path, forged.to_string()).unwrap();

    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "alice",
            "import",
        ])
        .stdin(std::fs::File::open(&forge_path).unwrap())
        .output()
        .unwrap();
    assert!(out.status.success());

    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "list",
            "-n",
            "forge",
        ])
        .output()
        .unwrap();
    let resp: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let meta = &resp["memories"][0]["metadata"];
    assert_eq!(
        meta["agent_id"], "alice",
        "import must restamp agent_id with caller's id, got: {meta}"
    );
    assert_eq!(
        meta["imported_from_agent_id"], "alphaonedev@admin",
        "original claim must be preserved for forensics"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(&forge_path);
}

#[test]
fn test_import_trust_source_preserves_agent_id() {
    let db_path = fresh_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let forge_path = std::env::temp_dir().join(format!("backup-{}.json", uuid::Uuid::new_v4()));

    let backup = serde_json::json!({
        "memories": [{
            "id": "b0000000-0000-0000-0000-000000000001",
            "tier": "long",
            "namespace": "backup",
            "title": "genuine-backup",
            "content": "c",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "user",
            "access_count": 0,
            "created_at": "2026-04-16T10:00:00+00:00",
            "updated_at": "2026-04-16T10:00:00+00:00",
            "metadata": {"agent_id": "original-alice"}
        }],
        "links": []
    });
    std::fs::write(&forge_path, backup.to_string()).unwrap();

    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "restorer",
            "import",
            "--trust-source",
        ])
        .stdin(std::fs::File::open(&forge_path).unwrap())
        .output()
        .unwrap();
    assert!(out.status.success());

    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "list",
            "-n",
            "backup",
        ])
        .output()
        .unwrap();
    let resp: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        resp["memories"][0]["metadata"]["agent_id"], "original-alice",
        "--trust-source must preserve agent_id from JSON"
    );

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(&forge_path);
}

/// GAP 2: Consolidate attribution was nondeterministic (last-write-wins from
/// merged metadata). Fix: consolidator's agent_id becomes authoritative;
/// original authors preserved in `metadata.consolidated_from_agents`.
#[test]
fn test_consolidate_attributes_to_consolidator() {
    let db_path = fresh_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    let mut ids = Vec::new();
    for (agent, title) in [("alice", "cg-1"), ("bob", "cg-2"), ("charlie", "cg-3")] {
        let out = cmd(binary)
            .args([
                "--db",
                db_path.to_str().unwrap(),
                "--agent-id",
                agent,
                "--json",
                "store",
                "-T",
                title,
                "-n",
                "cg",
                "-c",
                "c",
            ])
            .output()
            .unwrap();
        let j: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        ids.push(j["id"].as_str().unwrap().to_string());
    }

    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "consolidator-dana",
            "consolidate",
            "--title",
            "merged",
            "--summary",
            "A+B+C",
            "--namespace",
            "cg",
            &ids.join(","),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "consolidate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "list",
            "-n",
            "cg",
        ])
        .output()
        .unwrap();
    let resp: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let meta = &resp["memories"][0]["metadata"];

    assert_eq!(
        meta["agent_id"], "consolidator-dana",
        "consolidator's agent_id must be authoritative"
    );
    let sources = meta["consolidated_from_agents"].as_array().unwrap();
    let source_strs: Vec<&str> = sources.iter().filter_map(|v| v.as_str()).collect();
    assert!(source_strs.contains(&"alice"));
    assert!(source_strs.contains(&"bob"));
    assert!(source_strs.contains(&"charlie"));

    let _ = std::fs::remove_file(&db_path);
}

/// GAP 3: `cmd_mine` produced memories with no `agent_id`, making them orphan
/// w.r.t. every filter. Fix: inject caller's id + `mined_from` source tag.
#[test]
fn test_mine_stamps_caller_agent_id() {
    let db_path = fresh_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // Minimal Claude conversations.json the miner can parse.
    let mine_dir = std::env::temp_dir().join(format!("mine-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&mine_dir).unwrap();
    // Claude's format is JSONL (one conversation object per line, no outer array).
    let conv_path = mine_dir.join("conversations.jsonl");
    let conv = serde_json::json!({
        "uuid": "c1",
        "name": "A test conversation",
        "created_at": "2026-04-16T00:00:00Z",
        "updated_at": "2026-04-16T00:00:00Z",
        "chat_messages": [
            {"uuid":"m1","text":"hello from claude","sender":"human","created_at":"2026-04-16T00:00:00Z"},
            {"uuid":"m2","text":"hi back","sender":"assistant","created_at":"2026-04-16T00:00:01Z"},
            {"uuid":"m3","text":"continue","sender":"human","created_at":"2026-04-16T00:00:02Z"}
        ]
    });
    std::fs::write(&conv_path, format!("{conv}\n")).unwrap();

    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "miner-eve",
            "--json",
            "mine",
            "--format",
            "claude",
            "--min-messages",
            "1",
            conv_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "mine failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "list",
            "-n",
            "claude-export",
        ])
        .output()
        .unwrap();
    let resp: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let memories = resp["memories"].as_array().unwrap();
    assert!(
        !memories.is_empty(),
        "mine should have imported at least 1 memory"
    );
    for mem in memories {
        assert_eq!(
            mem["metadata"]["agent_id"], "miner-eve",
            "mined memories must carry caller's agent_id, got: {}",
            mem["metadata"]
        );
        assert!(
            mem["metadata"]["mined_from"].is_string(),
            "mined memories must be tagged with mined_from source"
        );
    }

    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_dir_all(&mine_dir);
}
