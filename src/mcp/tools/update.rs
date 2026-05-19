// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_update` handler.

use crate::embeddings::Embed;
use crate::hnsw::VectorIndex;
use crate::models::{EditSource, Tier};
use crate::storage::VersionConflict;
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
    // v0.7.0 Provenance Gap 2 (#906) — opt-in source_uri patch.
    // Validated below before reaching the storage layer; storage path
    // trusts the value as already-validated.
    let source_uri = params["source_uri"].as_str();
    // v0.7.0 Provenance Gap 1 (#884) — optimistic-concurrency
    // `expected_version` param. When supplied + non-null, the
    // underlying storage::update_with_expected_version refuses the
    // mutation with a typed VersionConflict envelope if the stored
    // row's `version` no longer matches.
    let expected_version = params["expected_version"].as_i64();
    // v0.7.0 Provenance Gap 5 (#888) — typed `edit_source`
    // discriminator. Default `Human` preserves the in-place v0.6.x
    // mutation contract; `Llm` and `Hook` route through the
    // append-and-archive path so the pre-edit content is preserved
    // in `archived_memories` for rewind via `memory_archive_list`.
    let edit_source = params["edit_source"]
        .as_str()
        .and_then(EditSource::from_str)
        .unwrap_or_default();

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
    if let Some(uri) = source_uri {
        validate::validate_source_uri(uri).map_err(|e| e.to_string())?;
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

    // v0.7.0 Provenance Gap 5 (#888) — append-and-archive branch.
    // When `edit_source` is `Llm` or `Hook`, we archive the OLD row
    // with `archive_reason='superseded'`, then mint a NEW row
    // carrying the patched content + a `supersedes` link new→old.
    // Caller's `expected_version` is still honored as the gate.
    if edit_source.appends_and_archives() {
        let result = db::update_with_archive_on_supersede(
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
            source_uri,
            expected_version,
            edit_source,
        )
        .map_err(|e| conflict_or_string(&e))?;
        // Re-embed the NEW row when content changed.
        if let Some(emb) = embedder {
            let new_id = &result.new_id;
            let mem = db::get(conn, new_id).map_err(|e| e.to_string())?;
            if let Some(ref m) = mem {
                let text = format!("{} {}", m.title, m.content);
                if let Ok(embedding) = emb.embed(&text) {
                    let _ = db::set_embedding(conn, new_id, &embedding);
                    if let Some(idx) = vector_index {
                        idx.remove(new_id);
                        idx.insert(new_id.clone(), embedding);
                    }
                }
            }
        }
        let new_mem = db::get(conn, &result.new_id).map_err(|e| e.to_string())?;
        return Ok(json!({
            "updated": true,
            "edit_source": edit_source.as_str(),
            "memory": new_mem,
            "superseded_id": result.archived_id,
            "new_id": result.new_id,
        }));
    }

    let (found, content_changed) = db::update_with_expected_version(
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
        source_uri,
        expected_version,
    )
    .map_err(|e| conflict_or_string(&e))?;

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
    Ok(json!({
        "updated": true,
        "edit_source": edit_source.as_str(),
        "memory": mem,
    }))
}

/// v0.7.0 Provenance Gap 1 (#884) — emit a structured CONFLICT
/// envelope as a JSON string when the underlying storage layer
/// returns a typed [`VersionConflict`]. Other errors stringify
/// verbatim so existing callers and tests continue to see the
/// historic error text.
fn conflict_or_string(e: &anyhow::Error) -> String {
    if let Some(vc) = e.downcast_ref::<VersionConflict>() {
        json!({
            "status": "conflict",
            "id": vc.id,
            "expected_version": vc.expected,
            "current_version": vc.current,
        })
        .to_string()
    } else {
        e.to_string()
    }
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
            confidence_source: crate::models::ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
            version: 1,
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

    // v0.7.0 Provenance Gap 5 (#888) — edit_source=llm routes through the
    // append-and-archive supersede write path: the OLD row lands in
    // archived_memories with archive_reason='superseded', a fresh NEW row
    // is minted carrying the patched content + metadata.superseded_id, and
    // the response surfaces `superseded_id` + `new_id`. Covers the
    // `if edit_source.appends_and_archives()` arm in handle_update
    // (lines 107-148), including the embedder Some-path re-embed of the
    // NEW row + vector-index insert.
    #[test]
    fn edit_source_llm_appends_and_archives_with_embedder() {
        let conn = fresh_conn();
        let mem = make_mem("pre-supersede");
        let id = db::insert(&conn, &mem).expect("ins");
        let mock = MockEmbedder::new_local().expect("mock");
        let idx = VectorIndex::empty();
        let out = handle_update(
            &conn,
            &json!({
                "id": &id,
                "content": "llm-rewritten content body",
                "edit_source": "llm",
            }),
            Some(&mock as &dyn crate::embeddings::Embed),
            Some(&idx),
        )
        .expect("supersede ok");
        assert_eq!(out["updated"].as_bool(), Some(true));
        assert_eq!(out["edit_source"].as_str(), Some("llm"));
        // archived_id == original id; new_id is a freshly-minted uuid
        assert_eq!(out["superseded_id"].as_str(), Some(id.as_str()));
        let new_id = out["new_id"].as_str().expect("new_id present");
        assert_ne!(new_id, id);
        // NEW row carries the patched content + superseded_id pointer
        let new_mem = &out["memory"];
        assert_eq!(
            new_mem["content"].as_str(),
            Some("llm-rewritten content body")
        );
        assert_eq!(
            new_mem["metadata"]["superseded_id"].as_str(),
            Some(id.as_str())
        );
        // Embedding written for the NEW row, indexed by new_id
        let emb = db::get_embedding(&conn, new_id)
            .expect("emb ok")
            .expect("some");
        assert_eq!(emb.len(), 384);
    }

    // v0.7.0 Provenance Gap 5 (#888) — edit_source=hook variant of the
    // append-and-archive path WITHOUT an embedder. Covers the Hook arm of
    // `EditSource::appends_and_archives()`, plus the None-embedder branch
    // inside the supersede block (lines 126 falsy path), AND the
    // happy-path return for the supersede shape (lines 141-147).
    #[test]
    fn edit_source_hook_appends_and_archives_no_embedder() {
        let conn = fresh_conn();
        let mem = make_mem("pre-hook");
        let id = db::insert(&conn, &mem).expect("ins");
        let out = handle_update(
            &conn,
            &json!({
                "id": &id,
                "title": "hook-edited title",
                "edit_source": "hook",
            }),
            None,
            None,
        )
        .expect("hook supersede ok");
        assert_eq!(out["edit_source"].as_str(), Some("hook"));
        assert_eq!(out["superseded_id"].as_str(), Some(id.as_str()));
        let new_id = out["new_id"].as_str().expect("new_id present");
        assert_ne!(new_id, id);
        assert_eq!(out["memory"]["title"].as_str(), Some("hook-edited title"));
        // No embedder → no embedding row for the new id.
        assert!(
            db::get_embedding(&conn, new_id).expect("ok").is_none(),
            "no embedder ⇒ no embedding persisted on the new row"
        );
    }

    // v0.7.0 Provenance Gap 1 (#884) — when `expected_version` is supplied
    // and drifts from the stored row's version, the storage layer returns
    // a typed VersionConflict; handle_update funnels it through
    // `conflict_or_string`, which emits a JSON CONFLICT envelope as the
    // Err string. Covers lines 165 (map_err on the in-place path) +
    // 199-208 (the VersionConflict downcast arm) end-to-end.
    #[test]
    fn expected_version_conflict_returns_json_envelope() {
        let conn = fresh_conn();
        let mem = make_mem("verconflict");
        let id = db::insert(&conn, &mem).expect("ins");
        // Bump version to 2 with a no-expectation update, so the next
        // expected_version=1 call drifts.
        let _ = handle_update(&conn, &json!({"id": &id, "priority": 6}), None, None).expect("bump");
        let err = handle_update(
            &conn,
            &json!({
                "id": &id,
                "title": "stale write",
                "expected_version": 1,
            }),
            None,
            None,
        )
        .unwrap_err();
        // Err is the JSON CONFLICT envelope minted by conflict_or_string.
        let v: serde_json::Value = serde_json::from_str(&err).expect("json envelope");
        assert_eq!(v["status"].as_str(), Some("conflict"));
        assert_eq!(v["id"].as_str(), Some(id.as_str()));
        assert_eq!(v["expected_version"].as_i64(), Some(1));
        assert_eq!(v["current_version"].as_i64(), Some(2));
    }

    // v0.7.0 Provenance Gap 2 (#906) — source_uri opt-in patch is
    // validated before the storage write. Covers the
    // `if let Some(uri) = source_uri { validate::validate_source_uri(...) }`
    // arm at lines 85-87 — both the happy validate-pass branch and the
    // reject branch for a bare string without a recognised scheme.
    #[test]
    fn source_uri_valid_passes_through_and_invalid_rejects() {
        let conn = fresh_conn();
        let mem = make_mem("srcuri");
        let id = db::insert(&conn, &mem).expect("ins");
        // Happy: doc: scheme is accepted by validate_source_uri.
        let ok = handle_update(
            &conn,
            &json!({"id": &id, "source_uri": "doc:internal-ref-42"}),
            None,
            None,
        )
        .expect("valid source_uri");
        assert_eq!(ok["updated"].as_bool(), Some(true));
        assert_eq!(
            ok["memory"]["source_uri"].as_str(),
            Some("doc:internal-ref-42")
        );
        // Reject: bare string without a recognised scheme.
        let err = handle_update(
            &conn,
            &json!({"id": &id, "source_uri": "example.com/no-scheme"}),
            None,
            None,
        )
        .unwrap_err();
        assert!(!err.is_empty(), "source_uri must be rejected");
        assert!(
            err.to_lowercase().contains("source uri")
                || err.to_lowercase().contains("source_uri")
                || err.to_lowercase().contains("scheme"),
            "error should reference source uri / scheme; got: {err}"
        );
    }
}
