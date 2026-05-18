// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_check_duplicate` handler.

use crate::embeddings::Embed;
use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_check_duplicate(
    conn: &rusqlite::Connection,
    params: &Value,
    embedder: Option<&dyn Embed>,
) -> Result<Value, String> {
    let title = params["title"].as_str().ok_or("title is required")?;
    let content = params["content"].as_str().ok_or("content is required")?;
    let namespace = params["namespace"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    // Float defaults are awkward in JSON schema land — accept either an
    // explicit threshold or fall back to the tuned default. The hard
    // floor is enforced inside `db::check_duplicate`.
    #[allow(clippy::cast_possible_truncation)]
    let threshold = params["threshold"]
        .as_f64()
        .map_or(db::DUPLICATE_THRESHOLD_DEFAULT, |t| t as f32);

    validate::validate_title(title).map_err(|e| e.to_string())?;
    validate::validate_content(content).map_err(|e| e.to_string())?;
    if let Some(ns) = namespace {
        validate::validate_namespace(ns).map_err(|e| e.to_string())?;
    }

    let emb = embedder
        .ok_or("memory_check_duplicate requires the embedder; enable semantic tier or above")?;
    let text = format!("{title} {content}");
    let query_embedding = emb.embed(&text).map_err(|e| e.to_string())?;

    // Round-2 F18 — short-circuit on raw-content hash equality before
    // falling through to embedding cosine similarity. Catches byte-
    // identical duplicates that the embedding pipeline would otherwise
    // cap at ~0.92 due to nomic prefix normalisation.
    let check = db::check_duplicate_with_text(conn, &query_embedding, &text, namespace, threshold)
        .map_err(|e| e.to_string())?;

    // Round similarity to 3 decimals at the response edge — keeps the
    // JSON readable without leaking the f32's full quantisation noise.
    let nearest_json = check.nearest.as_ref().map(|m| {
        json!({
            "id": m.id,
            "title": m.title,
            "namespace": m.namespace,
            "similarity": (m.similarity * 1000.0).round() / 1000.0,
        })
    });
    let suggested_merge = if check.is_duplicate {
        check.nearest.as_ref().map(|m| m.id.clone())
    } else {
        None
    };

    Ok(json!({
        "is_duplicate": check.is_duplicate,
        "threshold": check.threshold,
        "nearest": nearest_json,
        "suggested_merge": suggested_merge,
        "candidates_scanned": check.candidates_scanned,
    }))
}

#[cfg(test)]
mod tests {
    //! Coverage C-2 — focused unit tests for `handle_check_duplicate`.
    //!
    //! Six-category template:
    //! A. happy path — duplicate detected, nearest + suggested_merge surfaced
    //! A. happy path — non-duplicate, nearest still populated when below threshold
    //! B. validation — missing title / content; invalid namespace
    //! C. embedder absence — refusal path (tier-not-enabled)
    //! D. state-dependent — empty DB returns no_match
    //! E. response shape — similarity is rounded to 3 decimals

    use super::*;
    use crate::embeddings::test_support::MockEmbedder;
    use crate::models::{Memory, Tier};
    use crate::storage as db;

    fn fresh_conn() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn make_mem(title: &str, ns: &str) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: format!("content {title}"),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({"agent_id": "ai:test"}),
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

    // C. missing embedder — refusal returned even on valid args
    #[test]
    fn missing_embedder_refuses() {
        let conn = fresh_conn();
        let err = handle_check_duplicate(&conn, &json!({"title": "hi", "content": "world"}), None)
            .unwrap_err();
        assert!(err.contains("requires the embedder"), "got: {err}");
    }

    // B. missing title
    #[test]
    fn missing_title_errors() {
        let conn = fresh_conn();
        let emb = MockEmbedder::new_local().unwrap();
        let err = handle_check_duplicate(&conn, &json!({"content": "x"}), Some(&emb)).unwrap_err();
        assert!(err.contains("title"), "got: {err}");
    }

    // B. missing content
    #[test]
    fn missing_content_errors() {
        let conn = fresh_conn();
        let emb = MockEmbedder::new_local().unwrap();
        let err = handle_check_duplicate(&conn, &json!({"title": "t"}), Some(&emb)).unwrap_err();
        assert!(err.contains("content"), "got: {err}");
    }

    // B. invalid namespace
    #[test]
    fn invalid_namespace_rejected() {
        let conn = fresh_conn();
        let emb = MockEmbedder::new_local().unwrap();
        let err = handle_check_duplicate(
            &conn,
            &json!({"title": "t", "content": "c", "namespace": "has spaces"}),
            Some(&emb),
        )
        .unwrap_err();
        assert!(!err.is_empty(), "expected non-empty error");
    }

    // D. empty DB — no duplicate, nearest is null, suggested_merge null
    #[test]
    fn empty_db_returns_no_duplicate() {
        let conn = fresh_conn();
        let emb = MockEmbedder::new_local().unwrap();
        let resp = handle_check_duplicate(
            &conn,
            &json!({"title": "first", "content": "the very first memory"}),
            Some(&emb),
        )
        .expect("ok");
        assert_eq!(resp["is_duplicate"], false);
        assert!(resp["nearest"].is_null());
        assert!(resp["suggested_merge"].is_null());
        assert!(resp["candidates_scanned"].is_number());
    }

    // A. byte-identical raw text match — F18 short-circuit on text hash equality.
    #[test]
    fn raw_text_short_circuit_detects_byte_identical() {
        let conn = fresh_conn();
        let emb = MockEmbedder::new_local().unwrap();
        // Seed an existing memory with the exact same title+content.
        let title = "dup-title";
        let content = "dup-content";
        let mut mem = make_mem(title, "test");
        mem.content = content.to_string();
        // Stamp the embedding too so the row counts as embedded.
        let text = format!("{title} {content}");
        let embedding = emb.embed(&text).unwrap();
        let id = db::insert(&conn, &mem).unwrap();
        db::set_embedding(&conn, &id, &embedding).unwrap();

        let resp = handle_check_duplicate(
            &conn,
            &json!({"title": title, "content": content}),
            Some(&emb),
        )
        .expect("ok");
        assert_eq!(resp["is_duplicate"], true);
        assert_eq!(resp["suggested_merge"].as_str(), Some(id.as_str()));
        let nearest = &resp["nearest"];
        assert_eq!(nearest["id"].as_str(), Some(id.as_str()));
        // F18 short-circuit returns 1.0 similarity rounded to 3 decimals.
        let sim = nearest["similarity"].as_f64().unwrap();
        assert!(sim >= 0.999, "expected near-1 similarity, got {sim}");
    }

    // A. namespace filter — duplicate only checked within scope.
    #[test]
    fn namespace_filter_applied() {
        let conn = fresh_conn();
        let emb = MockEmbedder::new_local().unwrap();
        let title = "rare-title-xyz";
        let content = "rare-content-xyz";
        // Insert in a *different* namespace; a same-text lookup scoped to
        // "test-other" should NOT see the row in "test-here".
        let mut mem = make_mem(title, "test-here");
        mem.content = content.to_string();
        let text = format!("{title} {content}");
        let embedding = emb.embed(&text).unwrap();
        let id = db::insert(&conn, &mem).unwrap();
        db::set_embedding(&conn, &id, &embedding).unwrap();

        let resp = handle_check_duplicate(
            &conn,
            &json!({
                "title": title,
                "content": content,
                "namespace": "test-other",
            }),
            Some(&emb),
        )
        .expect("ok");
        // Scoped to a different namespace — must NOT be a duplicate.
        assert_eq!(resp["is_duplicate"], false);
    }

    // B. namespace whitespace stripping — empty after trim becomes None.
    #[test]
    fn whitespace_namespace_treated_as_none() {
        let conn = fresh_conn();
        let emb = MockEmbedder::new_local().unwrap();
        let resp = handle_check_duplicate(
            &conn,
            &json!({"title": "t", "content": "c", "namespace": "   "}),
            Some(&emb),
        )
        .expect("ok");
        // Should not error; namespace stripped to None.
        assert!(resp["is_duplicate"].is_boolean());
    }

    // E. explicit threshold is echoed in the response.
    #[test]
    fn explicit_threshold_honored() {
        let conn = fresh_conn();
        let emb = MockEmbedder::new_local().unwrap();
        let resp = handle_check_duplicate(
            &conn,
            &json!({"title": "x", "content": "y", "threshold": 0.99}),
            Some(&emb),
        )
        .expect("ok");
        let threshold = resp["threshold"].as_f64().unwrap();
        // The hard floor is enforced inside `db::check_duplicate_with_text`,
        // but 0.99 is above the floor so it must survive unchanged.
        assert!((threshold - 0.99).abs() < 0.01, "got {threshold}");
    }
}
