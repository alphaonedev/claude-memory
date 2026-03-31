// Integration tests — all run through the CLI binary

#[test]
fn test_cli_store_and_recall() {
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("claude-memory-cli-test-{}.db", uuid::Uuid::new_v4()));
    let binary = env!("CARGO_BIN_EXE_claude-memory");

    // Store
    let output = std::process::Command::new(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "store",
            "-t", "long", "-n", "test-project", "-T", "Rust is great",
            "--content", "Rust provides memory safety without garbage collection",
            "--tags", "rust,language", "-p", "8"])
        .output().unwrap();
    assert!(output.status.success(), "store failed: {}", String::from_utf8_lossy(&output.stderr));
    let stored: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stored["tier"], "long");
    assert_eq!(stored["namespace"], "test-project");

    // Recall
    let output = std::process::Command::new(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "recall", "Rust memory safety", "-n", "test-project"])
        .output().unwrap();
    assert!(output.status.success(), "recall failed: {}", String::from_utf8_lossy(&output.stderr));
    let recalled: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(recalled["count"].as_u64().unwrap() >= 1);

    // Search
    let output = std::process::Command::new(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "search", "Rust"])
        .output().unwrap();
    assert!(output.status.success());
    let searched: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(searched["count"].as_u64().unwrap() >= 1);

    // List
    let output = std::process::Command::new(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "list"])
        .output().unwrap();
    assert!(output.status.success());
    let listed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(listed["count"].as_u64().unwrap() >= 1);

    // Stats
    let output = std::process::Command::new(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "stats"])
        .output().unwrap();
    assert!(output.status.success());
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(stats["total"].as_u64().unwrap() >= 1);

    // Namespaces
    let output = std::process::Command::new(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "namespaces"])
        .output().unwrap();
    assert!(output.status.success());
    let ns: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!ns["namespaces"].as_array().unwrap().is_empty());

    // Export
    let output = std::process::Command::new(binary)
        .args(["--db", db_path.to_str().unwrap(), "export"])
        .output().unwrap();
    assert!(output.status.success());
    let exported: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(exported["count"].as_u64().unwrap() >= 1);

    // Delete
    let id = stored["id"].as_str().unwrap();
    let output = std::process::Command::new(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "delete", id])
        .output().unwrap();
    assert!(output.status.success());

    // Cleanup
    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_deduplication() {
    let binary = env!("CARGO_BIN_EXE_claude-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("claude-memory-dedup-test-{}.db", uuid::Uuid::new_v4()));

    // Store same title+namespace twice
    for content in ["first version", "second version"] {
        let output = std::process::Command::new(binary)
            .args(["--db", db_path.to_str().unwrap(), "--json", "store",
                "-T", "same title", "-n", "same-ns", "--content", content, "-p", "5"])
            .output().unwrap();
        assert!(output.status.success());
    }

    // Should only have 1 memory (deduped)
    let output = std::process::Command::new(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "stats"])
        .output().unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(stats["total"].as_u64().unwrap(), 1, "deduplication failed — expected 1 memory");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_gc_removes_expired() {
    let binary = env!("CARGO_BIN_EXE_claude-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("claude-memory-gc-test-{}.db", uuid::Uuid::new_v4()));

    // Store a short-term memory (6h TTL) — we can't easily test real expiry,
    // but we can verify gc runs without error
    let output = std::process::Command::new(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "store",
            "-t", "short", "-T", "ephemeral thought", "--content", "goes away"])
        .output().unwrap();
    assert!(output.status.success());

    let output = std::process::Command::new(binary)
        .args(["--db", db_path.to_str().unwrap(), "--json", "gc"])
        .output().unwrap();
    assert!(output.status.success());
    let gc: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // Not expired yet (6h TTL), so 0 deleted
    assert_eq!(gc["expired_deleted"].as_u64().unwrap(), 0);

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_content_size_limit() {
    let binary = env!("CARGO_BIN_EXE_claude-memory");
    let dir = std::env::temp_dir();
    let db_path = dir.join(format!("claude-memory-size-test-{}.db", uuid::Uuid::new_v4()));

    let huge_content = "x".repeat(70_000);
    let output = std::process::Command::new(binary)
        .args(["--db", db_path.to_str().unwrap(), "store",
            "-T", "too big", "--content", &huge_content])
        .output().unwrap();
    assert!(!output.status.success(), "should reject oversized content");

    let _ = std::fs::remove_file(&db_path);
}

#[test]
fn test_import_export_roundtrip() {
    let binary = env!("CARGO_BIN_EXE_claude-memory");
    let dir = std::env::temp_dir();
    let db1 = dir.join(format!("claude-memory-export-{}.db", uuid::Uuid::new_v4()));
    let db2 = dir.join(format!("claude-memory-import-{}.db", uuid::Uuid::new_v4()));

    // Store in db1
    let output = std::process::Command::new(binary)
        .args(["--db", db1.to_str().unwrap(), "store",
            "-t", "long", "-T", "portable memory", "--content", "travels between machines"])
        .output().unwrap();
    assert!(output.status.success());

    // Export from db1
    let output = std::process::Command::new(binary)
        .args(["--db", db1.to_str().unwrap(), "export"])
        .output().unwrap();
    assert!(output.status.success());

    // Import into db2
    let export_output = std::process::Command::new(binary)
        .args(["--db", db1.to_str().unwrap(), "export"])
        .output().unwrap();

    let mut child = std::process::Command::new(binary)
        .args(["--db", db2.to_str().unwrap(), "--json", "import"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn().unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(&export_output.stdout).unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success(), "import failed: {}", String::from_utf8_lossy(&result.stderr));

    // Verify db2 has the memory
    let output = std::process::Command::new(binary)
        .args(["--db", db2.to_str().unwrap(), "--json", "stats"])
        .output().unwrap();
    let stats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(stats["total"].as_u64().unwrap() >= 1, "import roundtrip failed");

    let _ = std::fs::remove_file(&db1);
    let _ = std::fs::remove_file(&db2);
}
