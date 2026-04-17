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
    use std::io::Write;
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-size-test-{}.db", uuid::Uuid::new_v4()));

    let huge_content = "x".repeat(70_000);
    // Pipe huge content via stdin (-c -) to avoid Windows' ~8191-char argv
    // limit on CreateProcess (ERROR_FILENAME_EXCED_RANGE / code 206).
    let mut child = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "-T",
            "too big",
            "-c",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(huge_content.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
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
    use std::io::Write;
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("ai-memory-val-size-{}.db", uuid::Uuid::new_v4()));

    let huge = "x".repeat(70_000);
    // Pipe via stdin (-c -) for Windows argv-length compatibility.
    let mut child = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "store",
            "-T",
            "huge",
            "-c",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(huge.as_bytes())
        .unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
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
    assert_eq!(tools.len(), 31, "expected 31 MCP tools");

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
    assert!(tool_names.contains(&"memory_agent_register"));
    assert!(tool_names.contains(&"memory_agent_list"));

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
fn test_version_flag_matches_cargo_pkg_version() {
    // Pin the CLI --version output to whatever Cargo.toml says, so the test
    // stays green across release-train bumps (0.5.4-patch.6 → 0.6.0-alpha.0
    // → 0.6.0-alpha.1 → …) without having to be re-hardcoded each time.
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let expected = env!("CARGO_PKG_VERSION");
    let output = cmd(binary).args(["--version"]).output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(expected),
        "--version output should contain CARGO_PKG_VERSION ({expected}), got: {stdout}"
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

/// Task 1.2 coverage gap: verify `metadata.agent_id` is present in the
/// `memory_recall` response shape. The spec deliberately excluded the recall
/// path from `--agent-id` filtering, but the field still needs to be visible
/// in the response for downstream tooling to act on provenance.
#[test]
fn test_agentid_visible_in_recall_response() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let db_path = fresh_db();

    // Store two memories by different agents. Using non-hyphenated tokens so
    // FTS5 tokenizer doesn't split them.
    for (agent, title) in [("alice", "RecallAgentATitle"), ("bob", "RecallAgentBTitle")] {
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
                "DistinctiveRecallToken body content",
            ])
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    // Recall via MCP in JSON format — agent_id must be visible on every returned memory.
    let req1 = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let req2 = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_recall","arguments":{"context":"DistinctiveRecallToken","format":"json"}}}"#;

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

    let stdout = String::from_utf8_lossy(&output.stdout);
    let last_line = stdout.trim().lines().last().unwrap();
    let resp: serde_json::Value = serde_json::from_str(last_line).unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let data: serde_json::Value = serde_json::from_str(text).unwrap();
    let memories = data["memories"].as_array().expect("memories array");
    assert!(
        memories.len() >= 2,
        "recall should return both memories, got: {data}"
    );

    let agents: Vec<String> = memories
        .iter()
        .filter_map(|m| m["metadata"]["agent_id"].as_str().map(ToString::to_string))
        .collect();
    assert!(
        agents.contains(&"alice".to_string()),
        "recall memories must include alice's agent_id, got: {agents:?}"
    );
    assert!(
        agents.contains(&"bob".to_string()),
        "recall memories must include bob's agent_id, got: {agents:?}"
    );

    let _ = std::fs::remove_file(&db_path);
}

/// Task 1.2 coverage gap: verify `agent_id` round-trips through both TOON
/// non-compact (`format: "toon"`) and JSON formats. TOON **compact** format
/// deliberately omits `metadata` for token efficiency (see src/toon.rs
/// `MEMORY_FIELDS_COMPACT`), so `agent_id` is NOT visible in that format —
/// that's a known tradeoff tracked separately. This test pins the two formats
/// where agent_id must show up.
#[test]
fn test_agentid_visible_in_toon_and_json() {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let db_path = fresh_db();

    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "toon-alice",
            "--json",
            "store",
            "-T",
            "ToonTestMemoryTitle",
            "-c",
            "ToonDistinctiveContent body",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    // Non-compact TOON — metadata column present, agent_id must appear.
    let req1 = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let req_toon = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_list","arguments":{"format":"toon"}}}"#;
    let req_json = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_list","arguments":{"format":"json"}}}"#;

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
                writeln!(stdin, "{req_toon}").ok();
                writeln!(stdin, "{req_json}").ok();
            }
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("failed to run mcp");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert!(
        lines.len() >= 3,
        "expected 3 responses (init+2 tools), got:\n{stdout}"
    );

    // lines[0] = initialize response; lines[1] = TOON tool call; lines[2] = JSON tool call.
    // TOON (non-compact)
    let resp_toon: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let text_toon = resp_toon["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text_toon.contains("toon-alice"),
        "TOON (non-compact) must surface agent_id within metadata; got:\n{text_toon}"
    );

    // JSON
    let resp_json: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    let text_json = resp_json["result"]["content"][0]["text"].as_str().unwrap();
    let data: serde_json::Value = serde_json::from_str(text_json).unwrap();
    let stored_agent = data["memories"][0]["metadata"]["agent_id"].as_str();
    assert_eq!(
        stored_agent,
        Some("toon-alice"),
        "JSON response must surface agent_id; got:\n{data}"
    );

    let _ = std::fs::remove_file(&db_path);
}

// ---------------------------------------------------------------------------
// Task 1.3 — Agent Registration
// ---------------------------------------------------------------------------

fn fresh_agent_db() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ai-memory-agentreg-{}.db", uuid::Uuid::new_v4()))
}

fn register_via_cli(
    binary: &str,
    db_path: &std::path::Path,
    agent_id: &str,
    agent_type: &str,
    capabilities: &str,
) -> std::process::Output {
    cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "agents",
            "register",
            "--agent-id",
            agent_id,
            "--agent-type",
            agent_type,
            "--capabilities",
            capabilities,
        ])
        .output()
        .unwrap()
}

#[test]
fn test_agent_register_and_list() {
    let db_path = fresh_agent_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    let out = register_via_cli(
        binary,
        &db_path,
        "alice",
        "ai:claude-opus-4.6",
        "recall,store",
    );
    assert!(
        out.status.success(),
        "register failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reg: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(reg["registered"], true);
    assert_eq!(reg["agent_id"], "alice");
    assert_eq!(reg["agent_type"], "ai:claude-opus-4.6");

    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "agents",
            "list",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let listed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(listed["count"], 1);
    let first = &listed["agents"][0];
    assert_eq!(first["agent_id"], "alice");
    assert_eq!(first["agent_type"], "ai:claude-opus-4.6");
    assert_eq!(first["capabilities"][0], "recall");
    assert_eq!(first["capabilities"][1], "store");
    assert!(first["registered_at"].as_str().unwrap().contains('T'));
    assert!(first["last_seen_at"].as_str().unwrap().contains('T'));
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_agent_register_duplicate_preserves_registered_at() {
    let db_path = fresh_agent_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    let _ = register_via_cli(binary, &db_path, "bob", "ai:codex-5.4", "search");
    // Read back the original registered_at
    let out1 = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "agents",
            "list",
        ])
        .output()
        .unwrap();
    let listed1: serde_json::Value = serde_json::from_slice(&out1.stdout).unwrap();
    let reg_at_1 = listed1["agents"][0]["registered_at"]
        .as_str()
        .unwrap()
        .to_string();

    // Sleep enough that now() moves forward
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Re-register with a different type + capabilities
    let out2 = register_via_cli(binary, &db_path, "bob", "human", "review,approve");
    assert!(out2.status.success());

    let out3 = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "agents",
            "list",
        ])
        .output()
        .unwrap();
    let listed2: serde_json::Value = serde_json::from_slice(&out3.stdout).unwrap();
    assert_eq!(listed2["count"], 1, "duplicate must upsert, not append");
    let row = &listed2["agents"][0];
    assert_eq!(
        row["agent_type"], "human",
        "agent_type updates on re-register"
    );
    assert_eq!(row["capabilities"][0], "review");
    assert_eq!(
        row["registered_at"].as_str().unwrap(),
        reg_at_1,
        "registered_at preserved across re-register"
    );
    assert_ne!(
        row["last_seen_at"].as_str().unwrap(),
        reg_at_1,
        "last_seen_at bumps on re-register"
    );
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_agent_register_rejects_invalid_type() {
    let db_path = fresh_agent_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    let out = register_via_cli(binary, &db_path, "carol", "bogus-type", "");
    assert!(
        !out.status.success(),
        "invalid agent_type should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid agent_type"),
        "expected validation message, got: {stderr}"
    );

    // Confirm no row was created.
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "agents",
            "list",
        ])
        .output()
        .unwrap();
    let listed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(listed["count"], 0);
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_agent_register_rejects_invalid_agent_id() {
    let db_path = fresh_agent_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    let out = register_via_cli(binary, &db_path, "evil;id", "system", "");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.to_lowercase().contains("agent_id"),
        "expected agent_id validation message, got: {stderr}"
    );
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_agents_list_uses_reserved_namespace() {
    let db_path = fresh_agent_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    let _ = register_via_cli(binary, &db_path, "dave", "system", "heartbeat");

    // Agent row lives in the _agents namespace
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "list",
            "-n",
            "_agents",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let listed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let count = listed["count"].as_u64().unwrap_or(0);
    assert_eq!(count, 1, "agent must occupy _agents namespace");

    let title = listed["memories"][0]["title"].as_str().unwrap_or("");
    assert_eq!(title, "agent:dave");

    let agent_type = listed["memories"][0]["metadata"]["agent_type"]
        .as_str()
        .unwrap_or("");
    assert_eq!(agent_type, "system");

    // And does NOT appear under the default (global) namespace
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "list",
            "-n",
            "global",
        ])
        .output()
        .unwrap();
    let listed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(listed["count"].as_u64().unwrap_or(0), 0);
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_mcp_agent_register_and_list() {
    use std::io::Write;
    let db_path = fresh_agent_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    let mut child = cmd(binary)
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
        .unwrap();

    let stdin = child.stdin.as_mut().unwrap();
    let init = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"clientInfo":{"name":"test-suite","version":"1.0"}}
    });
    writeln!(stdin, "{init}").unwrap();

    let reg = serde_json::json!({
        "jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{
            "name":"memory_agent_register",
            "arguments":{
                "agent_id":"mcp-eve",
                "agent_type":"ai:grok-4.2",
                "capabilities":["code","chat"]
            }
        }
    });
    writeln!(stdin, "{reg}").unwrap();

    let list = serde_json::json!({
        "jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"memory_agent_list","arguments":{}}
    });
    writeln!(stdin, "{list}").unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        lines.len() >= 3,
        "expected 3 JSON-RPC responses, got {}:\n{stdout}",
        lines.len()
    );

    let reg_resp: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let reg_text = reg_resp["result"]["content"][0]["text"].as_str().unwrap();
    let reg_json: serde_json::Value = serde_json::from_str(reg_text).unwrap();
    assert_eq!(reg_json["registered"], true);
    assert_eq!(reg_json["agent_id"], "mcp-eve");

    let list_resp: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    let list_text = list_resp["result"]["content"][0]["text"].as_str().unwrap();
    let list_json: serde_json::Value = serde_json::from_str(list_text).unwrap();
    assert_eq!(list_json["count"], 1);
    assert_eq!(list_json["agents"][0]["agent_id"], "mcp-eve");
    assert_eq!(list_json["agents"][0]["agent_type"], "ai:grok-4.2");
    let caps = list_json["agents"][0]["capabilities"].as_array().unwrap();
    assert_eq!(caps.len(), 2);

    let _ = std::fs::remove_file(&db_path);
}

// ---------------------------------------------------------------------------
// Task 1.2 follow-ups (#196-#199)
// ---------------------------------------------------------------------------

fn fresh_followup_db() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ai-memory-followup-{}.db", uuid::Uuid::new_v4()))
}

#[test]
fn test_196_cli_store_echoes_agent_id() {
    // CLI already returned full metadata.agent_id pre-#196; this locks in the
    // behavior so a future refactor doesn't regress it.
    let db_path = fresh_followup_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let out = cmd(binary)
        .env_remove("AI_MEMORY_AGENT_ID")
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "echo-test",
            "--json",
            "store",
            "-T",
            "echo-probe",
            "-c",
            "content",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["metadata"]["agent_id"], "echo-test");
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_196_mcp_store_echoes_resolved_agent_id() {
    use std::io::Write;
    let db_path = fresh_followup_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    let mut child = cmd(binary)
        .env_remove("AI_MEMORY_AGENT_ID")
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
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"clientInfo":{"name":"echo-test","version":"1"}}
        })
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"memory_store","arguments":{
                "title":"echo-mcp","content":"hi","agent_id":"mcp-echo"
            }}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let resp: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let body: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(
        body["agent_id"], "mcp-echo",
        "#196: MCP memory_store must echo agent_id in response"
    );
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_197_cli_list_rejects_invalid_agent_id_filter() {
    let db_path = fresh_followup_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "list",
            "--agent-id",
            "alice bob",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "#197: list must reject invalid agent_id filter with non-zero exit"
    );
    let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
    assert!(
        err.contains("agent_id"),
        "expected agent_id validation message, got: {err}"
    );
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_197_cli_search_rejects_invalid_agent_id_filter() {
    let db_path = fresh_followup_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "search",
            "foo",
            "--agent-id",
            "evil;id",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "#197: search must reject invalid agent_id filter"
    );
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_197_mcp_list_rejects_invalid_agent_id_filter() {
    use std::io::Write;
    let db_path = fresh_followup_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    let mut child = cmd(binary)
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
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"memory_list","arguments":{"agent_id":"alice bob"}}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let resp: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    // MCP tool errors surface via result.isError=true + error text.
    let is_error = resp["result"]["isError"].as_bool().unwrap_or(false);
    assert!(
        is_error,
        "#197: MCP memory_list must return isError=true for invalid agent_id filter; got: {resp}"
    );
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_198_anonymize_env_skips_host_fallback() {
    let db_path = fresh_followup_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    let out = cmd(binary)
        .env_remove("AI_MEMORY_AGENT_ID")
        .env("AI_MEMORY_ANONYMIZE", "1")
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-T",
            "anon-probe",
            "-c",
            "content",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "store failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = v["metadata"]["agent_id"].as_str().unwrap_or("");
    assert!(
        id.starts_with("anonymous:"),
        "#198: AI_MEMORY_ANONYMIZE=1 must collapse fallback to anonymous:; got: {id}"
    );
    assert!(!id.starts_with("host:"), "must not leak hostname");
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_199_toon_compact_surfaces_agent_id() {
    // Build a minimal response object and render via the library's TOON path
    // by exercising `memory_list` through the MCP surface with format=toon_compact.
    use std::io::Write;
    let db_path = fresh_followup_db();
    let binary = env!("CARGO_BIN_EXE_ai-memory");

    // Seed a memory stamped with a known agent_id via CLI (fast), then verify
    // TOON compact output surfaces it.
    let seed = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "toon-agent",
            "store",
            "-T",
            "toon-compact-probe",
            "-c",
            "hi",
            "-n",
            "toon-ns",
        ])
        .output()
        .unwrap();
    assert!(seed.status.success());

    let mut child = cmd(binary)
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
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"memory_list","arguments":{
                "namespace":"toon-ns","format":"toon_compact"
            }}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let resp: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    // Header must declare agent_id column.
    assert!(
        text.contains("agent_id"),
        "#199: toon_compact header must include agent_id column; got:\n{text}"
    );
    // Data row must contain the stamped id.
    assert!(
        text.contains("toon-agent"),
        "#199: toon_compact data row must surface agent_id value; got:\n{text}"
    );
    let _ = std::fs::remove_file(&db_path);
}

// ---------------------------------------------------------------------------
// Task 1.5 — Visibility Rules (scope-based filtering)
// ---------------------------------------------------------------------------

fn fresh_scope_db() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ai-memory-scope-{}.db", uuid::Uuid::new_v4()))
}

/// Seed a memory with an explicit scope + namespace.
fn seed_scoped(binary: &str, db_path: &std::path::Path, namespace: &str, title: &str, scope: &str) {
    let out = cmd(binary)
        .env_remove("AI_MEMORY_AGENT_ID")
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            "seed",
            "--json",
            "store",
            "-n",
            namespace,
            "-T",
            title,
            "-c",
            "content",
            "-t",
            "long",
            "--scope",
            scope,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "seed failed for {title}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn recall_as_agent(
    binary: &str,
    db_path: &std::path::Path,
    as_agent: &str,
    context: &str,
) -> Vec<String> {
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "recall",
            context,
            "--as-agent",
            as_agent,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "recall failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["memories"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|m| m["title"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn test_scope_private_visible_only_in_exact_namespace() {
    let db = fresh_scope_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    seed_scoped(
        bin,
        &db,
        "alphaone/eng/platform/agent-1",
        "priv-self",
        "private",
    );
    seed_scoped(
        bin,
        &db,
        "alphaone/eng/platform/agent-2",
        "priv-sibling",
        "private",
    );
    seed_scoped(bin, &db, "alphaone/eng/platform", "priv-parent", "private");

    let titles = recall_as_agent(bin, &db, "alphaone/eng/platform/agent-1", "priv");
    assert!(titles.contains(&"priv-self".to_string()));
    assert!(!titles.contains(&"priv-sibling".to_string()));
    assert!(!titles.contains(&"priv-parent".to_string()));
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_scope_team_visible_in_parent_subtree() {
    let db = fresh_scope_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    seed_scoped(bin, &db, "alphaone/eng/platform", "team-at-parent", "team");
    seed_scoped(
        bin,
        &db,
        "alphaone/eng/platform/agent-5",
        "team-sibling",
        "team",
    );
    seed_scoped(bin, &db, "alphaone/eng/ops", "team-other-team", "team");
    seed_scoped(bin, &db, "other-org/eng/platform", "team-other-org", "team");

    let titles = recall_as_agent(bin, &db, "alphaone/eng/platform/agent-1", "team");
    assert!(titles.contains(&"team-at-parent".to_string()));
    assert!(titles.contains(&"team-sibling".to_string()));
    assert!(!titles.contains(&"team-other-team".to_string()));
    assert!(!titles.contains(&"team-other-org".to_string()));
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_scope_unit_visible_in_grandparent_subtree() {
    let db = fresh_scope_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    seed_scoped(bin, &db, "alphaone/eng", "unit-at-grand", "unit");
    seed_scoped(
        bin,
        &db,
        "alphaone/eng/ops/agent-7",
        "unit-other-team",
        "unit",
    );
    seed_scoped(bin, &db, "alphaone/sales", "unit-other-unit", "unit");

    let titles = recall_as_agent(bin, &db, "alphaone/eng/platform/agent-1", "unit");
    assert!(titles.contains(&"unit-at-grand".to_string()));
    assert!(titles.contains(&"unit-other-team".to_string()));
    assert!(!titles.contains(&"unit-other-unit".to_string()));
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_scope_org_visible_across_whole_org() {
    let db = fresh_scope_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    seed_scoped(bin, &db, "alphaone", "org-at-root", "org");
    seed_scoped(bin, &db, "alphaone/sales/xyz", "org-other-branch", "org");
    seed_scoped(bin, &db, "other-corp", "org-outsider", "org");

    let titles = recall_as_agent(bin, &db, "alphaone/eng/platform/agent-1", "org");
    assert!(titles.contains(&"org-at-root".to_string()));
    assert!(titles.contains(&"org-other-branch".to_string()));
    assert!(!titles.contains(&"org-outsider".to_string()));
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_scope_collective_always_visible() {
    let db = fresh_scope_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    seed_scoped(
        bin,
        &db,
        "completely-unrelated-ns",
        "coll-anywhere",
        "collective",
    );

    let titles = recall_as_agent(bin, &db, "alphaone/eng/platform/agent-1", "coll");
    assert!(titles.contains(&"coll-anywhere".to_string()));
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_scope_missing_treated_as_private() {
    let db = fresh_scope_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    // seed WITHOUT scope (legacy-style)
    cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "store",
            "-n",
            "alphaone/eng/platform/agent-1",
            "-T",
            "legacy-at-self",
            "-c",
            "legacy",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "store",
            "-n",
            "alphaone/eng/platform/agent-2",
            "-T",
            "legacy-at-sibling",
            "-c",
            "legacy",
            "-t",
            "long",
        ])
        .output()
        .unwrap();

    let titles = recall_as_agent(bin, &db, "alphaone/eng/platform/agent-1", "legacy");
    assert!(titles.contains(&"legacy-at-self".to_string()));
    assert!(!titles.contains(&"legacy-at-sibling".to_string()));
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_scope_no_as_agent_returns_all() {
    let db = fresh_scope_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    seed_scoped(bin, &db, "alphaone/eng/platform", "all-1", "private");
    seed_scoped(bin, &db, "other/ns", "all-2", "team");
    seed_scoped(bin, &db, "yet-another", "all-3", "collective");

    // No --as-agent: visibility filtering disabled, all 3 visible
    let out = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "--json", "recall", "all"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let titles: Vec<String> = v["memories"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|m| m["title"].as_str().map(str::to_string))
        .collect();
    assert!(titles.contains(&"all-1".to_string()));
    assert!(titles.contains(&"all-2".to_string()));
    assert!(titles.contains(&"all-3".to_string()));
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_scope_search_respects_as_agent() {
    let db = fresh_scope_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    seed_scoped(
        bin,
        &db,
        "alphaone/eng/platform/agent-1",
        "search-my",
        "private",
    );
    seed_scoped(
        bin,
        &db,
        "alphaone/eng/platform/agent-2",
        "search-neighbor",
        "private",
    );

    let out = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--json",
            "search",
            "search",
            "--as-agent",
            "alphaone/eng/platform/agent-1",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let titles: Vec<String> = v["results"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|m| m["title"].as_str().map(str::to_string))
        .collect();
    assert!(titles.contains(&"search-my".to_string()));
    assert!(!titles.contains(&"search-neighbor".to_string()));
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_scope_flat_namespace_only_sees_exact_match_plus_collective() {
    let db = fresh_scope_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    seed_scoped(bin, &db, "global", "flat-private", "private");
    seed_scoped(bin, &db, "other", "flat-elsewhere", "private");
    seed_scoped(bin, &db, "global", "flat-team-at-self", "team");
    seed_scoped(bin, &db, "shared", "flat-collective", "collective");

    let titles = recall_as_agent(bin, &db, "global", "flat");
    assert!(titles.contains(&"flat-private".to_string()));
    assert!(!titles.contains(&"flat-elsewhere".to_string()));
    assert!(titles.contains(&"flat-collective".to_string()));
    // Flat agent has no parent; team-scope with no team_prefix → invisible
    assert!(!titles.contains(&"flat-team-at-self".to_string()));
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_scope_invalid_rejected_at_store() {
    let db = fresh_scope_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let out = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "store",
            "-T",
            "bad-scope",
            "-c",
            "x",
            "--scope",
            "public",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "invalid --scope must be rejected");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("invalid scope"),
        "expected validator message, got: {err}"
    );
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_scope_invalid_as_agent_rejected() {
    let db = fresh_scope_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let out = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "recall",
            "x",
            "--as-agent",
            "has space",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "invalid --as-agent must be rejected");
    let _ = std::fs::remove_file(&db);
}

// ---------------------------------------------------------------------------
// Task 1.6 — N-Level Rule Inheritance
// ---------------------------------------------------------------------------

fn fresh_inherit_db() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ai-memory-inherit-{}.db", uuid::Uuid::new_v4()))
}

/// Seed a standard memory in a namespace, then set_standard it.
fn seed_standard(
    binary: &str,
    db_path: &std::path::Path,
    namespace: &str,
    title: &str,
    content: &str,
) -> String {
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-n",
            namespace,
            "-T",
            title,
            "-c",
            content,
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "seed store failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = v["id"].as_str().unwrap().to_string();

    // Set via MCP (CLI doesn't expose set_namespace_standard)
    use std::io::Write;
    let mut child = cmd(binary)
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
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"memory_namespace_set_standard","arguments":{
                "namespace": namespace,
                "id": id,
            }}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let _ = child.wait_with_output();
    id
}

/// Invoke memory_namespace_get_standard via MCP, returning the parsed body.
fn get_standard_inherit(
    binary: &str,
    db_path: &std::path::Path,
    namespace: &str,
) -> serde_json::Value {
    use std::io::Write;
    let mut child = cmd(binary)
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
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"memory_namespace_get_standard","arguments":{
                "namespace": namespace,
                "inherit": true,
            }}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let resp: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

#[test]
fn test_inherit_4_level_chain() {
    let db = fresh_inherit_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");

    // Seed standards at all 4 levels plus global
    seed_standard(bin, &db, "*", "global-policy", "Global: always follow CLA");
    seed_standard(bin, &db, "alphaone", "org-policy", "Org: use Apache-2.0");
    seed_standard(
        bin,
        &db,
        "alphaone/eng",
        "unit-policy",
        "Unit: cargo fmt strict",
    );
    seed_standard(
        bin,
        &db,
        "alphaone/eng/platform",
        "team-policy",
        "Team: weekly sync Wednesday",
    );

    let body = get_standard_inherit(bin, &db, "alphaone/eng/platform");
    let chain = body["chain"].as_array().expect("chain array");
    // Chain order: most-general first
    assert_eq!(
        chain
            .iter()
            .map(|v| v.as_str().unwrap_or(""))
            .collect::<Vec<_>>(),
        vec!["*", "alphaone", "alphaone/eng", "alphaone/eng/platform"]
    );

    let standards = body["standards"].as_array().expect("standards array");
    assert_eq!(standards.len(), 4, "all 4 standards resolved");
    assert_eq!(standards[0]["title"], "global-policy");
    assert_eq!(standards[1]["title"], "org-policy");
    assert_eq!(standards[2]["title"], "unit-policy");
    assert_eq!(standards[3]["title"], "team-policy");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_inherit_missing_intermediates_skipped() {
    let db = fresh_inherit_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");

    // Global and team set; org and unit have NO standard
    seed_standard(bin, &db, "*", "global-only", "Global");
    seed_standard(bin, &db, "alphaone/eng/platform", "team-only", "Team");

    let body = get_standard_inherit(bin, &db, "alphaone/eng/platform");
    let chain = body["chain"].as_array().unwrap();
    assert_eq!(chain.len(), 4, "chain still contains all 4 path elements");

    let standards = body["standards"].as_array().unwrap();
    assert_eq!(standards.len(), 2, "only the 2 with standards resolve");
    assert_eq!(standards[0]["title"], "global-only");
    assert_eq!(standards[1]["title"], "team-only");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_inherit_preserves_3_level_flat_behavior() {
    // Legacy flat namespaces use explicit parent chain (namespace_meta).
    // This test regresses the historical 3-level behavior.
    let db = fresh_inherit_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");

    seed_standard(bin, &db, "*", "g", "g");
    // Flat namespaces — no `/`
    seed_standard(bin, &db, "ai-memory", "ai-mem-std", "ai-mem-std");

    let body = get_standard_inherit(bin, &db, "ai-memory");
    let standards = body["standards"].as_array().unwrap();
    // At minimum global + ai-memory resolve
    let titles: Vec<String> = standards
        .iter()
        .filter_map(|s| s["title"].as_str().map(str::to_string))
        .collect();
    assert!(titles.contains(&"g".to_string()));
    assert!(titles.contains(&"ai-mem-std".to_string()));
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_inherit_recall_auto_prepends_chain() {
    // session_start / recall should already inject the chain when namespace is set.
    let db = fresh_inherit_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");

    seed_standard(bin, &db, "alphaone", "org-s", "org standard");
    seed_standard(bin, &db, "alphaone/eng", "unit-s", "unit standard");
    seed_standard(bin, &db, "alphaone/eng/platform", "team-s", "team standard");

    // Store an unrelated memory in the team namespace
    let _ = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "store",
            "-n",
            "alphaone/eng/platform",
            "-T",
            "regular-note",
            "-c",
            "something",
            "-t",
            "long",
        ])
        .output()
        .unwrap();

    // Invoke recall via MCP and look for standards[]
    use std::io::Write;
    let mut child = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "mcp", "--tier", "keyword"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"memory_recall","arguments":{
                "context": "note",
                "namespace": "alphaone/eng/platform",
                "format": "json"
            }}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let resp: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let body: serde_json::Value = serde_json::from_str(text).unwrap();

    // Multiple standards → "standards" key (plural). 3 levels set, all should resolve.
    let standards = body["standards"].as_array().expect("standards array");
    let titles: Vec<String> = standards
        .iter()
        .filter_map(|s| s["title"].as_str().map(str::to_string))
        .collect();
    assert!(titles.contains(&"org-s".to_string()));
    assert!(titles.contains(&"unit-s".to_string()));
    assert!(titles.contains(&"team-s".to_string()));
    // Order: most-general first
    assert_eq!(titles[0], "org-s");
    assert_eq!(titles[titles.len() - 1], "team-s");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_inherit_global_only() {
    // Only global `*` has a standard — chain still walks cleanly.
    let db = fresh_inherit_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");

    seed_standard(bin, &db, "*", "only-global", "global rule");
    let body = get_standard_inherit(bin, &db, "alphaone/eng/platform/agent-1");
    let standards = body["standards"].as_array().unwrap();
    assert_eq!(standards.len(), 1);
    assert_eq!(standards[0]["title"], "only-global");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_inherit_no_standards_returns_empty() {
    let db = fresh_inherit_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let body = get_standard_inherit(bin, &db, "alphaone/eng/platform");
    let standards = body["standards"].as_array().unwrap();
    assert!(standards.is_empty());
    assert_eq!(body["count"], 0);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_inherit_deep_namespace_8_levels() {
    let db = fresh_inherit_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");

    // 8-level path matches MAX_NAMESPACE_DEPTH
    let deep = "a/b/c/d/e/f/g/h";
    seed_standard(bin, &db, "*", "root-s", "root");
    seed_standard(bin, &db, "a", "top-s", "top");
    seed_standard(bin, &db, deep, "leaf-s", "leaf");

    let body = get_standard_inherit(bin, &db, deep);
    let chain = body["chain"].as_array().unwrap();
    assert_eq!(chain.len(), 9, "* + 8 /-levels");
    let standards = body["standards"].as_array().unwrap();
    assert_eq!(standards.len(), 3, "only the 3 set");
    assert_eq!(standards[0]["title"], "root-s");
    assert_eq!(standards[2]["title"], "leaf-s");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_inherit_default_omits_chain() {
    // Without inherit=true, the old single-namespace response shape is used.
    let db = fresh_inherit_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");

    seed_standard(bin, &db, "alphaone", "org-only", "org");

    // get_standard with inherit=false (default) must return single-object shape
    use std::io::Write;
    let mut child = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "mcp", "--tier", "keyword"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"memory_namespace_get_standard","arguments":{
                "namespace": "alphaone",
            }}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let resp: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let body: serde_json::Value = serde_json::from_str(text).unwrap();

    assert!(
        body["chain"].is_null(),
        "chain must not be present without inherit=true"
    );
    assert_eq!(body["title"], "org-only");
    let _ = std::fs::remove_file(&db);
}

// ---------------------------------------------------------------------------
// Task 1.7 — Vertical Memory Promotion
// ---------------------------------------------------------------------------

fn fresh_vpromote_db() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ai-memory-vpromote-{}.db", uuid::Uuid::new_v4()))
}

fn seed_memory_at(binary: &str, db_path: &std::path::Path, namespace: &str, title: &str) -> String {
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-n",
            namespace,
            "-T",
            title,
            "-c",
            "content",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "seed failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["id"].as_str().unwrap().to_string()
}

#[test]
fn test_vpromote_clones_to_ancestor_and_links() {
    let db = fresh_vpromote_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let src_id = seed_memory_at(bin, &db, "alphaone/eng/platform/agent-1", "runbook");

    let out = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--json",
            "promote",
            &src_id,
            "--to-namespace",
            "alphaone/eng",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "vpromote failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["mode"], "vertical");
    assert_eq!(v["to_namespace"], "alphaone/eng");
    let clone_id = v["clone_id"].as_str().unwrap().to_string();
    assert_ne!(clone_id, src_id, "clone must have distinct ID");

    let src = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "--json", "get", &src_id])
        .output()
        .unwrap();
    let src_v: serde_json::Value = serde_json::from_slice(&src.stdout).unwrap();
    assert_eq!(
        src_v["memory"]["namespace"],
        "alphaone/eng/platform/agent-1"
    );

    let clone = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "--json", "get", &clone_id])
        .output()
        .unwrap();
    let clone_v: serde_json::Value = serde_json::from_slice(&clone.stdout).unwrap();
    assert_eq!(clone_v["memory"]["namespace"], "alphaone/eng");
    assert_eq!(clone_v["memory"]["title"], "runbook");

    let links = clone_v["links"].as_array().expect("links array");
    assert!(
        links.iter().any(|l| {
            l["source_id"].as_str() == Some(&clone_id)
                && l["target_id"].as_str() == Some(&src_id)
                && l["relation"] == "derived_from"
        }),
        "clone must link derived_from → source; got: {links:?}"
    );
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_vpromote_rejects_non_ancestor() {
    let db = fresh_vpromote_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let src_id = seed_memory_at(bin, &db, "alphaone/eng/platform", "runbook");
    let out = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "promote",
            &src_id,
            "--to-namespace",
            "other-org",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "promote to non-ancestor must fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.to_lowercase().contains("ancestor") || err.to_lowercase().contains("not"),
        "expected ancestry error, got: {err}"
    );
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_vpromote_rejects_self_namespace() {
    let db = fresh_vpromote_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let src_id = seed_memory_at(bin, &db, "alphaone/eng", "runbook");
    let out = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "promote",
            &src_id,
            "--to-namespace",
            "alphaone/eng",
        ])
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "promote to self-namespace must fail (no-op)"
    );
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_vpromote_without_flag_preserves_tier_behavior() {
    let db = fresh_vpromote_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let out = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--json",
            "store",
            "-n",
            "alphaone/eng",
            "-T",
            "to-bump",
            "-c",
            "x",
            "-t",
            "mid",
        ])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = v["id"].as_str().unwrap().to_string();

    let out = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "--json", "promote", &id])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["promoted"], true);
    assert_eq!(v["tier"], "long");

    let get_out = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "--json", "get", &id])
        .output()
        .unwrap();
    let g: serde_json::Value = serde_json::from_slice(&get_out.stdout).unwrap();
    assert_eq!(g["memory"]["tier"], "long");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_vpromote_to_root_ancestor() {
    let db = fresh_vpromote_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let src_id = seed_memory_at(bin, &db, "alphaone/eng/platform/agent-1", "root-promo");
    let out = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--json",
            "promote",
            &src_id,
            "--to-namespace",
            "alphaone",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["mode"], "vertical");
    assert_eq!(v["to_namespace"], "alphaone");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_vpromote_flat_namespace_cannot_promote() {
    let db = fresh_vpromote_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let src_id = seed_memory_at(bin, &db, "global", "flat");
    let out = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "promote",
            &src_id,
            "--to-namespace",
            "some-other-ns",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "flat namespace cannot be promoted");
    let _ = std::fs::remove_file(&db);
}

// ---------------------------------------------------------------------------
// Task 1.8 — Governance Metadata
// ---------------------------------------------------------------------------

fn fresh_gov_db() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ai-memory-gov-{}.db", uuid::Uuid::new_v4()))
}

fn store_std_mem(binary: &str, db_path: &std::path::Path, namespace: &str, title: &str) -> String {
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-n",
            namespace,
            "-T",
            title,
            "-c",
            "policy-body",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    v["id"].as_str().unwrap().to_string()
}

fn mcp_call(
    binary: &str,
    db_path: &std::path::Path,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    use std::io::Write;
    let mut child = cmd(binary)
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
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name": name, "arguments": args}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let resp: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    serde_json::from_str(text).unwrap()
}

#[test]
fn test_governance_set_and_get_roundtrip() {
    let db = fresh_gov_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let sid = store_std_mem(bin, &db, "alphaone/eng", "eng-policy");

    let gov = serde_json::json!({
        "write": "registered",
        "promote": "approve",
        "delete": "owner",
        "approver": {"agent": "maintainer"}
    });
    let set_resp = mcp_call(
        bin,
        &db,
        "memory_namespace_set_standard",
        serde_json::json!({
            "namespace": "alphaone/eng",
            "id": sid,
            "governance": gov.clone(),
        }),
    );
    assert_eq!(set_resp["set"], true);
    assert_eq!(set_resp["governance"], gov);

    let get_resp = mcp_call(
        bin,
        &db,
        "memory_namespace_get_standard",
        serde_json::json!({"namespace": "alphaone/eng"}),
    );
    assert_eq!(get_resp["governance"]["write"], "registered");
    assert_eq!(get_resp["governance"]["promote"], "approve");
    assert_eq!(get_resp["governance"]["delete"], "owner");
    assert_eq!(get_resp["governance"]["approver"]["agent"], "maintainer");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_governance_default_returned_when_unset() {
    let db = fresh_gov_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let sid = store_std_mem(bin, &db, "plain", "plain-policy");

    let _ = mcp_call(
        bin,
        &db,
        "memory_namespace_set_standard",
        serde_json::json!({"namespace": "plain", "id": sid}),
    );
    let get_resp = mcp_call(
        bin,
        &db,
        "memory_namespace_get_standard",
        serde_json::json!({"namespace": "plain"}),
    );
    let gov = &get_resp["governance"];
    assert_eq!(gov["write"], "any");
    assert_eq!(gov["promote"], "any");
    assert_eq!(gov["delete"], "owner");
    assert_eq!(gov["approver"], "human");
    let _ = std::fs::remove_file(&db);
}

/// Invoke a tool and return the raw MCP response envelope — preserves
/// `isError` + content-text for rejection tests where the content[0].text
/// is an error string, not parseable JSON.
fn mcp_call_raw(
    binary: &str,
    db_path: &std::path::Path,
    name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    use std::io::Write;
    let mut child = cmd(binary)
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
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name": name, "arguments": args}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    serde_json::from_str(lines[1]).unwrap()
}

#[test]
fn test_governance_invalid_rejected() {
    let db = fresh_gov_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let sid = store_std_mem(bin, &db, "alphaone/eng", "bogus-policy");

    let resp = mcp_call_raw(
        bin,
        &db,
        "memory_namespace_set_standard",
        serde_json::json!({
            "namespace": "alphaone/eng",
            "id": sid,
            "governance": {
                "write": "open-to-all",
                "promote": "any",
                "delete": "any",
                "approver": "human"
            }
        }),
    );
    assert_eq!(
        resp["result"]["isError"], true,
        "bogus level must return isError=true; got {resp}"
    );
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_governance_consensus_quorum_rejected() {
    let db = fresh_gov_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let sid = store_std_mem(bin, &db, "alphaone", "cons-policy");

    let resp = mcp_call_raw(
        bin,
        &db,
        "memory_namespace_set_standard",
        serde_json::json!({
            "namespace": "alphaone",
            "id": sid,
            "governance": {
                "write": "approve",
                "promote": "any",
                "delete": "owner",
                "approver": {"consensus": 0}
            }
        }),
    );
    assert_eq!(
        resp["result"]["isError"], true,
        "consensus(0) must return isError=true; got {resp}"
    );
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_governance_inherit_path_surfaces_per_level() {
    let db = fresh_gov_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");

    let org_id = store_std_mem(bin, &db, "alphaone", "org-pol");
    let team_id = store_std_mem(bin, &db, "alphaone/eng", "team-pol");

    let _ = mcp_call(
        bin,
        &db,
        "memory_namespace_set_standard",
        serde_json::json!({
            "namespace": "alphaone",
            "id": org_id,
            "governance": {
                "write": "any", "promote": "any", "delete": "owner",
                "approver": "human"
            }
        }),
    );
    let _ = mcp_call(
        bin,
        &db,
        "memory_namespace_set_standard",
        serde_json::json!({
            "namespace": "alphaone/eng",
            "id": team_id,
            "governance": {
                "write": "registered", "promote": "owner", "delete": "approve",
                "approver": {"consensus": 2}
            }
        }),
    );

    let resp = mcp_call(
        bin,
        &db,
        "memory_namespace_get_standard",
        serde_json::json!({"namespace": "alphaone/eng", "inherit": true}),
    );
    let standards = resp["standards"].as_array().unwrap();
    assert!(standards.len() >= 2);
    let org = standards
        .iter()
        .find(|s| s["namespace"] == "alphaone")
        .unwrap();
    let team = standards
        .iter()
        .find(|s| s["namespace"] == "alphaone/eng")
        .unwrap();
    assert_eq!(org["governance"]["write"], "any");
    assert_eq!(team["governance"]["write"], "registered");
    assert_eq!(team["governance"]["approver"]["consensus"], 2);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_governance_legacy_memory_defaults_not_mutated() {
    let db = fresh_gov_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let sid = store_std_mem(bin, &db, "legacy", "legacy-policy");

    let _ = mcp_call(
        bin,
        &db,
        "memory_namespace_set_standard",
        serde_json::json!({"namespace": "legacy", "id": sid}),
    );
    let out = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "--json", "get", &sid])
        .output()
        .unwrap();
    let mem_val: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let metadata = &mem_val["memory"]["metadata"];
    assert!(
        metadata.get("governance").is_none(),
        "no governance param must not inject a policy; got metadata={metadata}"
    );
    let _ = std::fs::remove_file(&db);
}

// ---------------------------------------------------------------------------
// Task 1.9 — Governance Enforcement
// ---------------------------------------------------------------------------

fn fresh_enforce_db() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ai-memory-enforce-{}.db", uuid::Uuid::new_v4()))
}

/// Set a governance policy on a namespace. Seeds the standard memory under
/// `owner_agent_id`, then calls memory_namespace_set_standard with the policy.
fn set_governance(
    binary: &str,
    db_path: &std::path::Path,
    namespace: &str,
    governance: serde_json::Value,
    owner_agent_id: &str,
) {
    let out = cmd(binary)
        .env_remove("AI_MEMORY_AGENT_ID")
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            owner_agent_id,
            "--json",
            "store",
            "-n",
            namespace,
            "-T",
            &format!("{namespace}-standard"),
            "-c",
            "policy",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let sid = v["id"].as_str().unwrap().to_string();

    use std::io::Write;
    let mut child = cmd(binary)
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
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"memory_namespace_set_standard","arguments":{
                "namespace": namespace,
                "id": sid,
                "governance": governance,
            }}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let _ = child.wait_with_output();
}

#[test]
fn test_enforce_any_allows_store() {
    let db = fresh_enforce_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({"write":"any","promote":"any","delete":"any","approver":"human"}),
        "owner",
    );
    let out = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "stranger",
            "--json",
            "store",
            "-n",
            "alphaone",
            "-T",
            "any-write",
            "-c",
            "hi",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "any-write must allow: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_enforce_registered_blocks_unregistered() {
    let db = fresh_enforce_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({"write":"registered","promote":"any","delete":"owner","approver":"human"}),
        "owner",
    );
    let out = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "stranger-not-registered",
            "store",
            "-n",
            "alphaone",
            "-T",
            "blocked-write",
            "-c",
            "hi",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success(), "unregistered must be blocked");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not a registered agent"), "got: {err}");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_enforce_registered_allows_registered() {
    let db = fresh_enforce_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let _ = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "agents",
            "register",
            "--agent-id",
            "alice",
            "--agent-type",
            "human",
        ])
        .output()
        .unwrap();
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({"write":"registered","promote":"any","delete":"owner","approver":"human"}),
        "owner",
    );
    let out = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "alice",
            "store",
            "-n",
            "alphaone",
            "-T",
            "allowed-write",
            "-c",
            "hi",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_enforce_owner_blocks_non_owner_delete() {
    let db = fresh_enforce_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({"write":"any","promote":"any","delete":"owner","approver":"human"}),
        "owner",
    );
    let store = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "alice",
            "--json",
            "store",
            "-n",
            "alphaone",
            "-T",
            "alice-mem",
            "-c",
            "hi",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    let id = serde_json::from_slice::<serde_json::Value>(&store.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let del = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "bob",
            "delete",
            &id,
        ])
        .output()
        .unwrap();
    assert!(!del.status.success());
    let err = String::from_utf8_lossy(&del.stderr);
    assert!(err.contains("not the owner"), "got: {err}");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_enforce_owner_allows_self_delete() {
    let db = fresh_enforce_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({"write":"any","promote":"any","delete":"owner","approver":"human"}),
        "owner",
    );
    let store = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "alice",
            "--json",
            "store",
            "-n",
            "alphaone",
            "-T",
            "alice-mem-2",
            "-c",
            "hi",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    let id = serde_json::from_slice::<serde_json::Value>(&store.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let del = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "alice",
            "delete",
            &id,
        ])
        .output()
        .unwrap();
    assert!(del.status.success());
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_enforce_approve_queues_pending() {
    let db = fresh_enforce_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({"write":"approve","promote":"any","delete":"owner","approver":"human"}),
        "owner",
    );
    let out = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "alice",
            "--json",
            "store",
            "-n",
            "alphaone",
            "-T",
            "queued",
            "-c",
            "hi",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "pending");
    assert!(v["pending_id"].is_string());
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_enforce_pending_list_and_approve() {
    let db = fresh_enforce_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({"write":"approve","promote":"any","delete":"owner","approver":"human"}),
        "owner",
    );
    let queued = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "alice",
            "--json",
            "store",
            "-n",
            "alphaone",
            "-T",
            "needs-approval",
            "-c",
            "hi",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    let pending_id =
        serde_json::from_slice::<serde_json::Value>(&queued.stdout).unwrap()["pending_id"]
            .as_str()
            .unwrap()
            .to_string();

    let list = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "--json", "pending", "list"])
        .output()
        .unwrap();
    assert!(list.status.success());
    let lv: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(lv["count"], 1);

    let ap = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "approver",
            "--json",
            "pending",
            "approve",
            &pending_id,
        ])
        .output()
        .unwrap();
    assert!(ap.status.success());
    let av: serde_json::Value = serde_json::from_slice(&ap.stdout).unwrap();
    assert_eq!(av["approved"], true);
    assert_eq!(av["decided_by"], "approver");

    let ap2 = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "approver",
            "pending",
            "approve",
            &pending_id,
        ])
        .output()
        .unwrap();
    assert!(!ap2.status.success());
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_enforce_pending_reject_status() {
    let db = fresh_enforce_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({"write":"approve","promote":"any","delete":"owner","approver":"human"}),
        "owner",
    );
    let queued = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "alice",
            "--json",
            "store",
            "-n",
            "alphaone",
            "-T",
            "to-reject",
            "-c",
            "hi",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    let pending_id =
        serde_json::from_slice::<serde_json::Value>(&queued.stdout).unwrap()["pending_id"]
            .as_str()
            .unwrap()
            .to_string();

    let rj = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "approver",
            "--json",
            "pending",
            "reject",
            &pending_id,
        ])
        .output()
        .unwrap();
    assert!(rj.status.success());
    let rv: serde_json::Value = serde_json::from_slice(&rj.stdout).unwrap();
    assert_eq!(rv["rejected"], true);

    let list = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--json",
            "pending",
            "list",
            "--status",
            "rejected",
        ])
        .output()
        .unwrap();
    let lv: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(lv["count"], 1);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_enforce_unset_falls_back_to_default_policy() {
    // Default: { write: Any, promote: Any, delete: Owner, approver: Human }
    let db = fresh_enforce_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let out = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "anyone",
            "--json",
            "store",
            "-n",
            "anywhere",
            "-T",
            "default-allow",
            "-c",
            "hi",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "default write policy is Any");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_enforce_promote_with_approve_policy() {
    let db = fresh_enforce_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({"write":"any","promote":"approve","delete":"any","approver":"human"}),
        "owner",
    );
    let store = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "alice",
            "--json",
            "store",
            "-n",
            "alphaone",
            "-T",
            "will-be-promoted",
            "-c",
            "hi",
            "-t",
            "mid",
        ])
        .output()
        .unwrap();
    let id = serde_json::from_slice::<serde_json::Value>(&store.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let pr = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "alice",
            "--json",
            "promote",
            &id,
        ])
        .output()
        .unwrap();
    assert!(pr.status.success());
    let v: serde_json::Value = serde_json::from_slice(&pr.stdout).unwrap();
    assert_eq!(v["status"], "pending");
    assert!(v["pending_id"].is_string());
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_enforce_mcp_pending_tools() {
    let db = fresh_enforce_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({"write":"approve","promote":"any","delete":"owner","approver":"human"}),
        "owner",
    );
    let queued = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "alice",
            "--json",
            "store",
            "-n",
            "alphaone",
            "-T",
            "mcp-pending-flow",
            "-c",
            "x",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    let pending_id =
        serde_json::from_slice::<serde_json::Value>(&queued.stdout).unwrap()["pending_id"]
            .as_str()
            .unwrap()
            .to_string();

    use std::io::Write;
    let mut child = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "mcp", "--tier", "keyword"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"memory_pending_list","arguments":{}}
        })
    )
    .unwrap();
    writeln!(stdin, "{}",
        serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"memory_pending_approve","arguments":{"id": pending_id, "agent_id": "approver-mcp"}}
        })).unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    let list_resp: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let list_text = list_resp["result"]["content"][0]["text"].as_str().unwrap();
    let list_body: serde_json::Value = serde_json::from_str(list_text).unwrap();
    assert_eq!(list_body["count"], 1);

    let appr_resp: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    let appr_text = appr_resp["result"]["content"][0]["text"].as_str().unwrap();
    let appr_body: serde_json::Value = serde_json::from_str(appr_text).unwrap();
    assert_eq!(appr_body["approved"], true);
    let _ = std::fs::remove_file(&db);
}

// ---------------------------------------------------------------------------
// Task 1.10 — Approver Types (Human/Agent/Consensus) + auto-execute
// ---------------------------------------------------------------------------

fn fresh_approver_db() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ai-memory-approver-{}.db", uuid::Uuid::new_v4()))
}

fn queue_store(
    binary: &str,
    db_path: &std::path::Path,
    namespace: &str,
    title: &str,
    requester: &str,
) -> String {
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            requester,
            "--json",
            "store",
            "-n",
            namespace,
            "-T",
            title,
            "-c",
            "body",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "queue store failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "pending", "expected pending, got: {v}");
    v["pending_id"].as_str().unwrap().to_string()
}

fn approve(
    binary: &str,
    db_path: &std::path::Path,
    pending_id: &str,
    approver: &str,
) -> std::process::Output {
    cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--agent-id",
            approver,
            "--json",
            "pending",
            "approve",
            pending_id,
        ])
        .output()
        .unwrap()
}

#[test]
fn test_approver_human_any_approver_accepted() {
    let db = fresh_approver_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({"write":"approve","promote":"any","delete":"owner","approver":"human"}),
        "owner",
    );
    let pid = queue_store(bin, &db, "alphaone", "human-target", "alice");
    let out = approve(bin, &db, &pid, "any-random-approver");
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["approved"], true);
    assert_eq!(v["executed"], true);
    assert!(v["memory_id"].is_string());
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_approver_agent_rejects_wrong_caller() {
    let db = fresh_approver_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({
            "write":"approve","promote":"any","delete":"owner",
            "approver":{"agent":"maintainer"}
        }),
        "owner",
    );
    let pid = queue_store(bin, &db, "alphaone", "agent-target", "alice");
    let out = approve(bin, &db, &pid, "some-other-agent");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("designated approver") || err.to_lowercase().contains("rejected"),
        "got: {err}"
    );
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_approver_agent_accepts_matching_caller() {
    let db = fresh_approver_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({
            "write":"approve","promote":"any","delete":"owner",
            "approver":{"agent":"maintainer"}
        }),
        "owner",
    );
    let pid = queue_store(bin, &db, "alphaone", "agent-ok", "alice");
    let out = approve(bin, &db, &pid, "maintainer");
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["approved"], true);
    assert_eq!(v["executed"], true);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_approver_consensus_below_threshold_pending() {
    let db = fresh_approver_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({
            "write":"approve","promote":"any","delete":"owner",
            "approver":{"consensus":3}
        }),
        "owner",
    );
    let pid = queue_store(bin, &db, "alphaone", "cons-target", "alice");

    let v1 = approve(bin, &db, &pid, "approver-1");
    assert!(v1.status.success());
    let j1: serde_json::Value = serde_json::from_slice(&v1.stdout).unwrap();
    assert_eq!(j1["approved"], false);
    assert_eq!(j1["status"], "pending");
    assert_eq!(j1["votes"], 1);
    assert_eq!(j1["quorum"], 3);

    let v2 = approve(bin, &db, &pid, "approver-2");
    let j2: serde_json::Value = serde_json::from_slice(&v2.stdout).unwrap();
    assert_eq!(j2["votes"], 2);
    assert_eq!(j2["approved"], false);

    let list = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "--json", "pending", "list"])
        .output()
        .unwrap();
    let lv: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(lv["count"], 1);
    assert_eq!(lv["pending"][0]["status"], "pending");
    assert_eq!(lv["pending"][0]["approvals"].as_array().unwrap().len(), 2);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_approver_consensus_threshold_auto_executes() {
    let db = fresh_approver_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({
            "write":"approve","promote":"any","delete":"owner",
            "approver":{"consensus":2}
        }),
        "owner",
    );
    let pid = queue_store(bin, &db, "alphaone", "cons-exec", "alice");

    let v1 = approve(bin, &db, &pid, "a1");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&v1.stdout).unwrap()["approved"],
        false
    );
    let v2 = approve(bin, &db, &pid, "a2");
    assert!(v2.status.success());
    let j2: serde_json::Value = serde_json::from_slice(&v2.stdout).unwrap();
    assert_eq!(j2["approved"], true);
    assert_eq!(j2["executed"], true);
    let memory_id = j2["memory_id"].as_str().unwrap();

    let list = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--json",
            "list",
            "-n",
            "alphaone",
        ])
        .output()
        .unwrap();
    let lv: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let titles: Vec<String> = lv["memories"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .filter_map(|m| m["title"].as_str().map(str::to_string))
        .collect();
    assert!(titles.contains(&"cons-exec".to_string()));
    let ids: Vec<String> = lv["memories"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str().map(str::to_string))
        .collect();
    assert!(ids.iter().any(|i| i == memory_id));
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_approver_consensus_same_agent_does_not_double_count() {
    let db = fresh_approver_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({
            "write":"approve","promote":"any","delete":"owner",
            "approver":{"consensus":2}
        }),
        "owner",
    );
    let pid = queue_store(bin, &db, "alphaone", "cons-dup", "alice");

    let v1a = approve(bin, &db, &pid, "a1");
    let v1b = approve(bin, &db, &pid, "a1");
    let j1a: serde_json::Value = serde_json::from_slice(&v1a.stdout).unwrap();
    let j1b: serde_json::Value = serde_json::from_slice(&v1b.stdout).unwrap();
    assert_eq!(j1a["votes"], 1);
    assert_eq!(j1b["votes"], 1, "same agent must not double-count");

    let v2 = approve(bin, &db, &pid, "a2");
    let j2: serde_json::Value = serde_json::from_slice(&v2.stdout).unwrap();
    assert_eq!(j2["approved"], true);
    assert_eq!(j2["executed"], true);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_approver_agent_rejected_not_counted_for_consensus() {
    let db = fresh_approver_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({
            "write":"approve","promote":"any","delete":"owner",
            "approver":{"agent":"only-me"}
        }),
        "owner",
    );
    let pid = queue_store(bin, &db, "alphaone", "agent-no-count", "alice");
    let rej = approve(bin, &db, &pid, "wrong-caller");
    assert!(!rej.status.success());

    let list = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "--json", "pending", "list"])
        .output()
        .unwrap();
    let lv: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(lv["count"], 1);
    assert_eq!(lv["pending"][0]["status"], "pending");
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_approver_delete_consensus_executes_delete() {
    let db = fresh_approver_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    set_governance(
        bin,
        &db,
        "alphaone",
        serde_json::json!({
            "write":"any","promote":"any","delete":"approve",
            "approver":{"consensus":2}
        }),
        "owner",
    );
    let st = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "alice",
            "--json",
            "store",
            "-n",
            "alphaone",
            "-T",
            "will-delete",
            "-c",
            "x",
            "-t",
            "long",
        ])
        .output()
        .unwrap();
    let id = serde_json::from_slice::<serde_json::Value>(&st.stdout).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let del = cmd(bin)
        .args([
            "--db",
            db.to_str().unwrap(),
            "--agent-id",
            "alice",
            "--json",
            "delete",
            &id,
        ])
        .output()
        .unwrap();
    assert!(del.status.success());
    let jd: serde_json::Value = serde_json::from_slice(&del.stdout).unwrap();
    assert_eq!(jd["status"], "pending");
    let pid = jd["pending_id"].as_str().unwrap().to_string();

    let _ = approve(bin, &db, &pid, "v1");
    let v2 = approve(bin, &db, &pid, "v2");
    let j2: serde_json::Value = serde_json::from_slice(&v2.stdout).unwrap();
    assert_eq!(j2["approved"], true);
    assert_eq!(j2["executed"], true);

    let get = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "get", &id])
        .output()
        .unwrap();
    assert!(
        !get.status.success(),
        "memory must be deleted after consensus"
    );
    let _ = std::fs::remove_file(&db);
}

// ---------------------------------------------------------------------------
// Task 1.11 — Context-Budget-Aware Recall
// ---------------------------------------------------------------------------

fn fresh_budget_db() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ai-memory-budget-{}.db", uuid::Uuid::new_v4()))
}

fn store_sized(binary: &str, db_path: &std::path::Path, title: &str, content: &str, priority: i32) {
    let out = cmd(binary)
        .args([
            "--db",
            db_path.to_str().unwrap(),
            "--json",
            "store",
            "-T",
            title,
            "-c",
            content,
            "-t",
            "long",
            "-p",
            &priority.to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "store failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn recall_with_budget(
    binary: &str,
    db_path: &std::path::Path,
    context: &str,
    budget: Option<usize>,
) -> serde_json::Value {
    let budget_str = budget.map(|n| n.to_string());
    let mut args: Vec<&str> = vec![
        "--db",
        db_path.to_str().unwrap(),
        "--json",
        "recall",
        context,
    ];
    if let Some(ref b) = budget_str {
        args.push("--budget-tokens");
        args.push(b);
    }
    let out = cmd(binary).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "recall failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn test_budget_unlimited_returns_all() {
    let db = fresh_budget_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    store_sized(bin, &db, "t1", "alpha match foo", 5);
    store_sized(bin, &db, "t2", "alpha match bar", 5);
    store_sized(bin, &db, "t3", "alpha match baz", 5);

    let v = recall_with_budget(bin, &db, "alpha", None);
    assert_eq!(v["count"], 3);
    assert!(v["tokens_used"].as_u64().unwrap() > 0);
    assert!(v.get("budget_tokens").is_none_or(|v| v.is_null()));
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_budget_truncates_to_fit() {
    let db = fresh_budget_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    let body = "alpha match ".repeat(3);
    for i in 1..=5 {
        store_sized(bin, &db, &format!("t{i}"), &body, 10 - i);
    }

    let v = recall_with_budget(bin, &db, "alpha", Some(25));
    let count = v["count"].as_u64().unwrap() as usize;
    assert!(
        count >= 1 && count < 5,
        "budget must truncate; got count={count}"
    );
    let tokens_used = v["tokens_used"].as_u64().unwrap();
    assert!(
        tokens_used <= 25,
        "tokens_used ({tokens_used}) must be <= budget (25)"
    );
    assert_eq!(v["budget_tokens"], 25);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_budget_zero_returns_empty() {
    let db = fresh_budget_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    store_sized(bin, &db, "x", "something to find", 5);

    let v = recall_with_budget(bin, &db, "something", Some(1));
    assert_eq!(v["count"], 0);
    assert_eq!(v["tokens_used"], 0);
    assert_eq!(v["budget_tokens"], 1);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_budget_preserves_rank_order() {
    let db = fresh_budget_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    store_sized(
        bin,
        &db,
        "low-pri",
        "alpha match filler content here longer",
        1,
    );
    store_sized(bin, &db, "high-pri", "alpha match short", 10);

    let v = recall_with_budget(bin, &db, "alpha", Some(8));
    let mems = v["memories"].as_array().unwrap();
    if !mems.is_empty() {
        assert_eq!(mems[0]["title"], "high-pri");
    }
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_budget_response_includes_metadata() {
    let db = fresh_budget_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    store_sized(bin, &db, "meta-test", "response metadata check", 5);

    let v = recall_with_budget(bin, &db, "response", Some(100));
    assert!(v["tokens_used"].as_u64().is_some());
    assert_eq!(v["budget_tokens"], 100);
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_budget_touch_only_surviving() {
    let db = fresh_budget_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    store_sized(bin, &db, "in-budget", "alpha short", 10);
    store_sized(
        bin,
        &db,
        "out-of-budget",
        &("alpha ".to_string() + &"x".repeat(200)),
        1,
    );

    let v = recall_with_budget(bin, &db, "alpha", Some(5));
    let mems = v["memories"].as_array().unwrap();
    assert!(!mems.is_empty());

    let list = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "--json", "list"])
        .output()
        .unwrap();
    let lv: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
    let excluded = lv["memories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["title"] == "out-of-budget")
        .unwrap();
    assert_eq!(
        excluded["access_count"], 0,
        "excluded memory must not have access_count bumped"
    );
    let _ = std::fs::remove_file(&db);
}

#[test]
fn test_budget_mcp_tool_schema_and_response() {
    use std::io::Write;
    let db = fresh_budget_db();
    let bin = env!("CARGO_BIN_EXE_ai-memory");
    store_sized(bin, &db, "mcp-target", "mcp budget test content", 5);

    let mut child = cmd(bin)
        .args(["--db", db.to_str().unwrap(), "mcp", "--tier", "keyword"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = child.stdin.as_mut().unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}})
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"memory_recall","arguments":{
                "context":"budget",
                "budget_tokens": 200,
                "format":"json"
            }}
        })
    )
    .unwrap();
    stdin.flush().unwrap();
    drop(child.stdin.take());
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    let tools_resp: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    let recall_tool = tools_resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "memory_recall")
        .unwrap();
    assert!(
        recall_tool["inputSchema"]["properties"]["budget_tokens"].is_object(),
        "memory_recall must advertise budget_tokens"
    );

    let call_resp: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    let text = call_resp["result"]["content"][0]["text"].as_str().unwrap();
    let body: serde_json::Value = serde_json::from_str(text).unwrap();
    assert!(body["tokens_used"].as_u64().is_some());
    assert_eq!(body["budget_tokens"], 200);
    let _ = std::fs::remove_file(&db);
}
