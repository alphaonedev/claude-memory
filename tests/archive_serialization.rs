// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 issue #861 regression — `db::list_archived` (the read path
//! behind the `memory_archive_list` MCP tool) had two bugs that
//! mangled forget-archived rows:
//!
//! 1. `tags` was emitted as the raw JSON-stringified TEXT blob (`"[\"a\",\"b\"]"`)
//!    instead of being parsed and re-emitted as a JSON array.
//!    Pre-fix shape: `"tags": "[\"a\",\"b\"]"`
//!    Post-fix shape: `"tags": ["a","b"]`
//! 2. `metadata` came back as `{}` even when the source memory had
//!    populated metadata keys (e.g. `agent_id`). Root cause was the
//!    `forget` archive INSERT in `storage::mod::forget` omitting both
//!    the `metadata` column and the SELECT expression — so the row
//!    landed in `archived_memories` with the column's empty default.
//!    The `gc` and explicit-`archive_memory` paths already projected
//!    metadata; only `forget` had the gap.
//!
//! This suite stores a memory with `metadata.agent_id` + a two-tag
//! array, runs `forget(..., archive=true)`, then asserts both bugs
//! stay fixed: `metadata.agent_id` survives, AND `tags` is a real
//! 2-element JSON array.

use ai_memory::db;
use ai_memory::models::{ConfidenceSource, Memory, MemoryKind, Tier};
use chrono::Utc;
use rusqlite::Connection;
use serde_json::json;
use std::path::Path;

fn open_test_db() -> Connection {
    db::open(Path::new(":memory:")).expect("open in-memory db")
}

fn seed_memory(
    conn: &Connection,
    namespace: &str,
    title: &str,
    tags: Vec<String>,
    metadata: serde_json::Value,
) -> String {
    let now = Utc::now().to_rfc3339();
    let m = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("body for {title}"),
        tags,
        priority: 5,
        confidence: 1.0,
        source: "test".into(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata,
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
    };
    db::insert(conn, &m).expect("insert memory")
}

#[test]
fn list_archived_emits_tags_as_array_and_preserves_metadata() {
    let conn = open_test_db();
    let namespace = "issue-861-forget-archive";

    let _id = seed_memory(
        &conn,
        namespace,
        "memory with metadata + tags",
        vec!["alpha".to_string(), "beta".to_string()],
        json!({
            "agent_id": "ai:tester@host",
            "imported_from_agent_id": "ai:upstream",
        }),
    );

    // Forget-archive everything in the namespace. Pre-fix this dropped
    // `metadata` on the floor because the INSERT omitted the column.
    let removed = db::forget(&conn, Some(namespace), None, None, true)
        .expect("forget(namespace, archive=true) must succeed");
    assert_eq!(
        removed, 1,
        "forget must remove exactly the one seeded memory"
    );

    let archived =
        db::list_archived(&conn, Some(namespace), 10, 0).expect("list_archived must succeed");
    assert_eq!(
        archived.len(),
        1,
        "exactly one archived row should surface for the namespace"
    );
    let row = &archived[0];

    // ── Bug #1: tags must be a real JSON array, not a stringified blob.
    let tags = row.get("tags").expect("tags key must be present");
    assert!(tags.is_array(), "tags must be a JSON array, got: {tags:?}");
    let tag_arr = tags.as_array().expect("array");
    assert_eq!(
        tag_arr.len(),
        2,
        "must surface exactly the two seeded tags; got {tag_arr:?}"
    );
    let tag_strs: Vec<&str> = tag_arr
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        tag_strs.contains(&"alpha"),
        "tags must include 'alpha'; got {tag_strs:?}"
    );
    assert!(
        tag_strs.contains(&"beta"),
        "tags must include 'beta'; got {tag_strs:?}"
    );

    // ── Bug #2: metadata.agent_id must survive the forget-archive INSERT.
    let metadata = row.get("metadata").expect("metadata key must be present");
    assert!(
        metadata.is_object(),
        "metadata must be a JSON object, got: {metadata:?}"
    );
    let agent_id = metadata.get("agent_id").and_then(serde_json::Value::as_str);
    assert_eq!(
        agent_id,
        Some("ai:tester@host"),
        "metadata.agent_id must round-trip through forget-archive + list_archived; \
         pre-#861 fix this came back as empty {{}}"
    );
    let imported = metadata
        .get("imported_from_agent_id")
        .and_then(serde_json::Value::as_str);
    assert_eq!(
        imported,
        Some("ai:upstream"),
        "metadata.imported_from_agent_id must also survive; the bug \
         stripped the whole metadata blob, not just one key"
    );

    // Sanity: archive_reason should reflect the forget path, not 'archive'.
    let reason = row
        .get("archive_reason")
        .and_then(serde_json::Value::as_str);
    assert_eq!(
        reason,
        Some("forget"),
        "archive_reason must be 'forget' for the forget-archive path"
    );
}
