// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_update` handler.

use crate::embeddings::Embed;
use crate::hnsw::VectorIndex;
use crate::models::Tier;
use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_update(
    conn: &rusqlite::Connection,
    params: &Value,
    embedder: Option<&dyn Embed>,
    vector_index: Option<&VectorIndex>,
) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    // Resolve prefix if exact ID not found
    let resolved_id = if db::get(conn, id).map_err(|e| e.to_string())?.is_some() {
        id.to_string()
    } else if let Some(mem) = db::get_by_prefix(conn, id).map_err(|e| e.to_string())? {
        mem.id
    } else {
        return Err("memory not found".into());
    };
    let title = params["title"].as_str();
    let content = params["content"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    let namespace = params["namespace"].as_str();
    let tags: Option<Vec<String>> = params["tags"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    });
    // B4 (R2-LOW) — clamp instead of panic. Validation below enforces 1-10.
    let priority = params["priority"]
        .as_i64()
        .map(|p| i32::try_from(p).unwrap_or(i32::MAX));
    let confidence = params["confidence"].as_f64();
    let expires_at = params["expires_at"].as_str();

    if let Some(t) = title {
        validate::validate_title(t).map_err(|e| e.to_string())?;
    }
    if let Some(c) = content {
        validate::validate_content(c).map_err(|e| e.to_string())?;
    }
    if let Some(ns) = &namespace {
        validate::validate_namespace(ns).map_err(|e| e.to_string())?;
    }
    if let Some(ref t) = tags {
        validate::validate_tags(t).map_err(|e| e.to_string())?;
    }
    if let Some(p) = priority {
        validate::validate_priority(p).map_err(|e| e.to_string())?;
    }
    if let Some(c) = confidence {
        validate::validate_confidence(c).map_err(|e| e.to_string())?;
    }
    if let Some(ts) = expires_at {
        // Allow past dates in update for programmatic TTL management and GC testing
        validate::validate_expires_at_format(ts).map_err(|e| e.to_string())?;
    }

    let metadata = if params["metadata"].is_object() {
        let m = params["metadata"].clone();
        validate::validate_metadata(&m).map_err(|e| e.to_string())?;
        // Preserve existing metadata.agent_id — provenance is immutable.
        // Without this, any MCP caller could rewrite the author of any memory.
        let existing = db::get(conn, &resolved_id)
            .map_err(|e| e.to_string())?
            .map_or_else(|| serde_json::json!({}), |m| m.metadata);
        Some(crate::identity::preserve_agent_id(&existing, &m))
    } else {
        None
    };

    let (found, content_changed) = db::update(
        conn,
        &resolved_id,
        title,
        content,
        tier.as_ref(),
        namespace,
        tags.as_ref(),
        priority,
        confidence,
        expires_at,
        metadata.as_ref(),
    )
    .map_err(|e| e.to_string())?;

    if !found {
        return Err("memory not found".into());
    }

    // Regenerate embedding when title or content changed
    if content_changed && let Some(emb) = embedder {
        let mem = db::get(conn, &resolved_id).map_err(|e| e.to_string())?;
        if let Some(ref m) = mem {
            let text = format!("{} {}", m.title, m.content);
            if let Ok(embedding) = emb.embed(&text) {
                let _ = db::set_embedding(conn, &resolved_id, &embedding);
                if let Some(idx) = vector_index {
                    idx.remove(&resolved_id);
                    idx.insert(resolved_id.clone(), embedding);
                }
            }
        }
    }

    let mem = db::get(conn, &resolved_id).map_err(|e| e.to_string())?;
    Ok(json!({"updated": true, "memory": mem}))
}

#[cfg(test)]
mod tests {
    //! L0.7-3 Tier B chunk-A — coverage tests for `handle_update`.
    //!
    //! Six-category template:
    //! A. happy path — title/content/tier/namespace/tags/priority/confidence/expires_at/metadata
    //! B. validation — every gated branch
    //! D. state-dependent — id not found
    //! E. idempotency — repeat update yields same shape
    //! Embedder-bound: `None` path AND `Some(&dyn Embed)` path (re-embed on content change)

    use super::*;
    use crate::embeddings::test_support::MockEmbedder;
    use crate::hnsw::VectorIndex;
    use crate::models::{Memory, Tier as MTier};
    use crate::storage as db;

    fn fresh_conn() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn make_mem(title: &str) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: MTier::Mid,
            namespace: "test".to_string(),
            title: title.to_string(),
            content: format!("body for {title}"),
            tags: vec!["a".to_string()],
            priority: 5,
            confidence: 0.5,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({"agent_id": "ai:owner"}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        }
    }

    // A. happy path — update multiple fields, no embedder
    #[test]
    fn happy_path_updates_all_fields_no_embedder() {
        let conn = fresh_conn();
        let mem = make_mem("orig");
        let id = db::insert(&conn, &mem).expect("ins");
        let out = handle_update(
            &conn,
            &json!({
                "id": id,
                "title": "new title",
                "content": "new body content here",
                "tier": "long",
                "namespace": "ns2",
                "tags": ["x", "y"],
                "priority": 7,
                "confidence": 0.9,
                "expires_at": "2030-01-01T00:00:00Z",
                "metadata": {"k": "v"},
            }),
            None,
            None,
        )
        .expect("ok");
        assert_eq!(out["updated"].as_bool(), Some(true));
        let m = &out["memory"];
        assert_eq!(m["title"].as_str(), Some("new title"));
        assert_eq!(m["namespace"].as_str(), Some("ns2"));
        // agent_id immutability preserved
        assert_eq!(
            m["metadata"]["agent_id"].as_str(),
            Some("ai:owner"),
            "agent_id must be preserved through update"
        );
    }

    // A. prefix resolution branch
    #[test]
    fn prefix_resolution_branch() {
        let conn = fresh_conn();
        let mut mem = make_mem("p");
        mem.id = "fedcba98-1111-2222-3333-444455556666".to_string();
        let _ = db::insert(&conn, &mem).expect("ins");
        let out = handle_update(
            &conn,
            &json!({"id": "fedcba98", "title": "renamed"}),
            None,
            None,
        )
        .expect("prefix ok");
        assert_eq!(out["memory"]["title"].as_str(), Some("renamed"));
    }

    // Embedder Some-path: content changed → re-embed + index touched
    #[test]
    fn embedder_some_path_reembeds_when_content_changes() {
        let conn = fresh_conn();
        let mem = make_mem("xyz");
        let id = db::insert(&conn, &mem).expect("ins");
        let mock = MockEmbedder::new_local().expect("mock");
        let idx = VectorIndex::empty();
        let out = handle_update(
            &conn,
            &json!({"id": id.clone(), "content": "completely new content"}),
            Some(&mock as &dyn crate::embeddings::Embed),
            Some(&idx),
        )
        .expect("ok");
        assert_eq!(out["updated"].as_bool(), Some(true));
        // embedding was written
        let emb = db::get_embedding(&conn, &id).expect("ok").expect("some");
        assert_eq!(emb.len(), 384);
    }

    // Embedder Some-path but no content change (only tags) → no re-embed
    #[test]
    fn embedder_some_path_skips_when_content_unchanged() {
        let conn = fresh_conn();
        let mem = make_mem("nochange");
        let id = db::insert(&conn, &mem).expect("ins");
        let mock = MockEmbedder::new_local().expect("mock");
        let out = handle_update(
            &conn,
            &json!({"id": id.clone(), "tags": ["new-tag"]}),
            Some(&mock as &dyn crate::embeddings::Embed),
            None,
        )
        .expect("ok");
        assert_eq!(out["updated"].as_bool(), Some(true));
        // no embedding stored
        let emb = db::get_embedding(&conn, &id).expect("ok");
        assert!(emb.is_none());
    }

    // B. missing id
    #[test]
    fn missing_id_errors() {
        let conn = fresh_conn();
        let err = handle_update(&conn, &json!({}), None, None).unwrap_err();
        assert!(err.contains("id is required"));
    }

    // B. invalid id format
    #[test]
    fn invalid_id_format_errors() {
        let conn = fresh_conn();
        let err = handle_update(&conn, &json!({"id": ""}), None, None).unwrap_err();
        assert!(!err.is_empty());
    }

    // D. id not found
    #[test]
    fn unknown_id_errors() {
        let conn = fresh_conn();
        let err = handle_update(
            &conn,
            &json!({"id": "11111111-aaaa-bbbb-cccc-dddddddddddd", "title": "x"}),
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("not found"));
    }

    // B. invalid title (empty)
    #[test]
    fn invalid_title_errors() {
        let conn = fresh_conn();
        let mem = make_mem("ok");
        let id = db::insert(&conn, &mem).expect("ins");
        let err = handle_update(&conn, &json!({"id": id, "title": ""}), None, None).unwrap_err();
        assert!(!err.is_empty());
    }

    // B. invalid content (empty)
    #[test]
    fn invalid_content_errors() {
        let conn = fresh_conn();
        let mem = make_mem("ok");
        let id = db::insert(&conn, &mem).expect("ins");
        let err = handle_update(&conn, &json!({"id": id, "content": ""}), None, None).unwrap_err();
        assert!(!err.is_empty());
    }

    // B. invalid namespace (has space)
    #[test]
    fn invalid_namespace_errors() {
        let conn = fresh_conn();
        let mem = make_mem("ok");
        let id = db::insert(&conn, &mem).expect("ins");
        let err = handle_update(
            &conn,
            &json!({"id": id, "namespace": "has space"}),
            None,
            None,
        )
        .unwrap_err();
        assert!(!err.is_empty());
    }

    // B. invalid priority (out of range)
    #[test]
    fn invalid_priority_errors() {
        let conn = fresh_conn();
        let mem = make_mem("ok");
        let id = db::insert(&conn, &mem).expect("ins");
        let err = handle_update(&conn, &json!({"id": id, "priority": 99}), None, None).unwrap_err();
        assert!(!err.is_empty());
    }

    // B. invalid confidence
    #[test]
    fn invalid_confidence_errors() {
        let conn = fresh_conn();
        let mem = make_mem("ok");
        let id = db::insert(&conn, &mem).expect("ins");
        let err =
            handle_update(&conn, &json!({"id": id, "confidence": 5.0}), None, None).unwrap_err();
        assert!(!err.is_empty());
    }

    // B. invalid expires_at format
    #[test]
    fn invalid_expires_at_errors() {
        let conn = fresh_conn();
        let mem = make_mem("ok");
        let id = db::insert(&conn, &mem).expect("ins");
        let err = handle_update(
            &conn,
            &json!({"id": id, "expires_at": "not-a-date"}),
            None,
            None,
        )
        .unwrap_err();
        assert!(!err.is_empty());
    }

    // metadata.agent_id immutability when caller tries to overwrite
    #[test]
    fn metadata_preserves_existing_agent_id() {
        let conn = fresh_conn();
        let mem = make_mem("immut");
        let id = db::insert(&conn, &mem).expect("ins");
        let out = handle_update(
            &conn,
            &json!({"id": id, "metadata": {"agent_id": "ai:other", "note": "hi"}}),
            None,
            None,
        )
        .expect("ok");
        assert_eq!(
            out["memory"]["metadata"]["agent_id"].as_str(),
            Some("ai:owner"),
            "agent_id immutable"
        );
        assert_eq!(out["memory"]["metadata"]["note"].as_str(), Some("hi"));
    }

    // E. idempotency
    #[test]
    fn idempotent_repeated_update() {
        let conn = fresh_conn();
        let mem = make_mem("idem");
        let id = db::insert(&conn, &mem).expect("ins");
        let one =
            handle_update(&conn, &json!({"id": &id, "priority": 8}), None, None).expect("ok 1");
        let two =
            handle_update(&conn, &json!({"id": &id, "priority": 8}), None, None).expect("ok 2");
        assert_eq!(one["updated"], two["updated"]);
    }
}
