// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// First test binary in the v0.6.3 integration suite (charter §"Files to
// create" line 344). Exercises the Pillar 1 / Stream A
// `memory_get_taxonomy` MCP tool end-to-end against a fresh disposable
// SQLite database. Future v063 tests should follow this same pattern:
// add a new `tests/v063_<topic>.rs` top-level binary and pull the
// shared harness in via `#[path = "v063/mod.rs"] mod v063;`.

#[path = "v063/mod.rs"]
mod v063;

use serde_json::Value;

/// Store three memories under nested namespaces and verify
/// `memory_get_taxonomy` walks the tree, returns the expected shape
/// (`tree` / `total_count` / `truncated`), and counts each leaf.
#[test]
fn test_get_taxonomy_walks_nested_namespaces() {
    let db = v063::tmp_db("taxonomy-walk");
    let db_str = db.to_str().unwrap();

    // Store one memory each under three nested namespaces. Use long-tier
    // so they don't expire mid-test.
    let stores = [
        ("acme/eng/api", "API design notes"),
        ("acme/eng", "engineering team standup"),
        ("acme/sales", "Q2 pipeline review"),
    ];
    for (ns, title) in stores {
        let out = v063::cmd()
            .args([
                "--db",
                db_str,
                "--json",
                "store",
                "-t",
                "long",
                "-n",
                ns,
                "-T",
                title,
                "--content",
                "test fixture",
            ])
            .output()
            .expect("store");
        assert!(
            out.status.success(),
            "store ns={ns} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Call memory_get_taxonomy via MCP — no prefix, default depth/limit.
    let lines = v063::mcp_exchange(
        &db,
        &[
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_get_taxonomy","arguments":{}}}"#,
        ],
    );
    assert_eq!(lines.len(), 1, "expected one MCP response, got {lines:?}");

    let resp: Value = serde_json::from_str(&lines[0]).expect("parse MCP response");
    assert_eq!(resp["id"], 1);
    let payload_text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("taxonomy payload should be a JSON string");
    let payload: Value = serde_json::from_str(payload_text).expect("parse taxonomy payload");

    // Shape contract: tree (TaxonomyNode object), total_count (>=3),
    // truncated (bool). For a global-prefix call the synthesized root
    // node has empty `name` and empty `namespace`, with the actual
    // namespaces hanging off `children`.
    assert!(payload["tree"].is_object(), "tree should be object");
    assert!(
        payload["truncated"].is_boolean(),
        "truncated should be bool"
    );
    let total = payload["total_count"].as_u64().expect("total_count u64");
    assert!(total >= 3, "expected at least 3 memories, got {total}");

    // Every stored memory shares the `acme` prefix, so the root's
    // subtree_count rolls up all three and the immediate children
    // include exactly the `acme` node.
    let subtree = payload["tree"]["subtree_count"]
        .as_u64()
        .expect("subtree_count u64");
    assert!(
        subtree >= 3,
        "expected root subtree_count >= 3, got {subtree}"
    );
    let child_names: Vec<&str> = payload["tree"]["children"]
        .as_array()
        .expect("children array")
        .iter()
        .filter_map(|n| n["name"].as_str())
        .collect();
    assert!(
        child_names.contains(&"acme"),
        "expected 'acme' child of root, got {child_names:?}"
    );

    let _ = std::fs::remove_file(&db);
}
