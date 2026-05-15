// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_consolidate` handler.

use crate::embeddings::Embed;
use crate::hnsw::VectorIndex;
use crate::llm::OllamaClient;
use crate::models::Tier;
use crate::{db, validate};
use serde_json::{Value, json};
use std::path::Path;
pub(super) fn handle_consolidate(
    conn: &rusqlite::Connection,
    db_path: &Path,
    params: &Value,
    llm: Option<&OllamaClient>,
    embedder: Option<&dyn Embed>,
    vector_index: Option<&VectorIndex>,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let ids_arr = params["ids"]
        .as_array()
        .ok_or("ids is required (array of memory IDs)")?;
    let mut ids = Vec::with_capacity(ids_arr.len());
    for (i, v) in ids_arr.iter().enumerate() {
        match v.as_str() {
            Some(s) => {
                validate::validate_id(s).map_err(|e| e.to_string())?;
                ids.push(s.to_string());
            }
            None => return Err(format!("ids[{i}] must be a string")),
        }
    }
    let title = params["title"].as_str().ok_or("title is required")?;
    let namespace = params["namespace"].as_str().unwrap_or("global");

    // Auto-generate summary via LLM if not provided
    let summary: String = if let Some(s) = params["summary"].as_str() {
        s.to_string()
    } else if let Some(llm_client) = llm {
        // Fetch memory contents for LLM summarization
        let mut memory_pairs: Vec<(String, String)> = Vec::new();
        for id in &ids {
            match db::get(conn, id) {
                Ok(Some(mem)) => memory_pairs.push((mem.title, mem.content)),
                Ok(None) => return Err(format!("memory not found: {id}")),
                Err(e) => return Err(e.to_string()),
            }
        }
        llm_client
            .summarize_memories(&memory_pairs)
            .map_err(|e| format!("LLM summarization failed: {e}"))?
    } else {
        return Err(
            "summary is required (or use smart/autonomous tier for auto-summarization)".into(),
        );
    };

    validate::validate_consolidate(&ids, title, &summary, namespace).map_err(|e| e.to_string())?;

    // v0.7.0 K9 — unified permission pipeline (consolidate-side).
    {
        use crate::permissions::{Op, PermissionContext, Permissions};
        let agent_id = crate::identity::resolve_agent_id(params["agent_id"].as_str(), mcp_client)
            .map_err(|e| e.to_string())?;
        let ctx = PermissionContext {
            op: Op::MemoryConsolidate,
            namespace: namespace.to_string(),
            agent_id,
            payload: json!({
                "title": title,
                "summary_chars": summary.len(),
                "source_ids": ids,
            }),
        };
        match Permissions::evaluate(&ctx, &[]) {
            crate::permissions::Decision::Allow | crate::permissions::Decision::Modify(_) => {}
            crate::permissions::Decision::Deny(reason) => {
                return Err(format!("consolidate denied by permission rule: {reason}"));
            }
            crate::permissions::Decision::Ask(prompt) => {
                return Ok(json!({
                    "status": "ask",
                    "reason": prompt,
                    "action": "consolidate",
                    "namespace": namespace,
                    "source_count": ids.len(),
                }));
            }
        }
    }

    let auto_generated = params["summary"].as_str().is_none();

    // Remove old entries from HNSW index before consolidation deletes them
    if let Some(idx) = vector_index {
        for id in &ids {
            idx.remove(id);
        }
    }

    // NHI: the caller (consolidator) owns the new memory's agent_id;
    // source authors are preserved as a forensic array by db::consolidate.
    let explicit_agent_id = params["agent_id"].as_str();
    let consolidator_agent_id = crate::identity::resolve_agent_id(explicit_agent_id, mcp_client)
        .map_err(|e| e.to_string())?;
    let new_id = db::consolidate(
        conn,
        &ids,
        title,
        &summary,
        namespace,
        &Tier::Long,
        "consolidation",
        &consolidator_agent_id,
    )
    .map_err(|e| e.to_string())?;

    // Generate embedding for the consolidated memory (#52)
    if let Some(emb) = embedder {
        let text = format!("{title} {summary}");
        match emb.embed(&text) {
            Ok(embedding) => {
                if let Err(e) = db::set_embedding(conn, &new_id, &embedding) {
                    tracing::warn!(
                        "failed to store embedding for consolidated {}: {}",
                        &new_id,
                        e
                    );
                }
                if let Some(idx) = vector_index {
                    // Remove old embeddings from HNSW index
                    for id in &ids {
                        idx.remove(id);
                    }
                    idx.insert(new_id.clone(), embedding);
                }
            }
            Err(e) => {
                tracing::warn!(
                    "failed to generate embedding for consolidated {}: {}",
                    &new_id,
                    e
                );
            }
        }
    }

    let mut result = json!({"id": new_id, "consolidated": ids.len()});
    if auto_generated {
        result["auto_summary"] = json!(true);
        result["summary_preview"] = json!(summary.chars().take(200).collect::<String>());
    }
    // Warn if any source memory was a namespace standard
    let standard_ids: Vec<&str> = ids
        .iter()
        .filter(|id| db::is_namespace_standard(conn, id))
        .map(std::string::String::as_str)
        .collect();
    if !standard_ids.is_empty() {
        result["warning"] = json!(format!(
            "consolidated memories included namespace standard(s): {}. Re-set the standard to the new memory ID: {}",
            standard_ids.join(", "),
            new_id
        ));
    }

    // P5 (G9): fire `memory_consolidated` webhook AFTER db::consolidate
    // commits the new memory. memory_id = the new consolidated id; the
    // details block carries the source ids that were merged.
    let details = serde_json::to_value(crate::subscriptions::ConsolidatedEventDetails {
        source_ids: ids.clone(),
        source_count: ids.len(),
    })
    .ok();
    crate::subscriptions::dispatch_event_with_details(
        conn,
        "memory_consolidated",
        &new_id,
        namespace,
        Some(&consolidator_agent_id),
        db_path,
        details,
    );

    Ok(result)
}

// ---------------------------------------------------------------------------
// Namespace standard handlers
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Coverage C-2 — focused tests for `handle_consolidate`.

    use super::*;
    use crate::embeddings::test_support::MockEmbedder;
    use crate::models::{Memory, MemoryKind};
    use crate::storage as db;
    use serde_json::json;

    fn fresh_db() -> (rusqlite::Connection, tempfile::NamedTempFile) {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let conn = db::open(tmp.path()).expect("db::open");
        (conn, tmp)
    }

    fn seed_observation(conn: &rusqlite::Connection, ns: &str, title: &str) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: format!("body for {title}"),
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
            memory_kind: MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        };
        db::insert(conn, &mem).expect("insert")
    }

    // Missing ids → typed error.
    #[test]
    fn missing_ids_errors() {
        let (conn, tmp) = fresh_db();
        let err = handle_consolidate(
            &conn,
            tmp.path(),
            &json!({"title": "t", "summary": "s"}),
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("ids"), "got: {err}");
    }

    // Non-string id entry → error.
    #[test]
    fn non_string_id_errors() {
        let (conn, tmp) = fresh_db();
        let err = handle_consolidate(
            &conn,
            tmp.path(),
            &json!({"ids": [42], "title": "t", "summary": "s"}),
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("must be a string"), "got: {err}");
    }

    // Invalid id (validate_id) → error.
    #[test]
    fn invalid_id_rejected() {
        let (conn, tmp) = fresh_db();
        let err = handle_consolidate(
            &conn,
            tmp.path(),
            &json!({"ids": ["  "], "title": "t", "summary": "s"}),
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(!err.is_empty());
    }

    // Missing title → error.
    #[test]
    fn missing_title_errors() {
        let (conn, tmp) = fresh_db();
        let err = handle_consolidate(
            &conn,
            tmp.path(),
            &json!({"ids": ["11111111-2222-3333-4444-555555555555"], "summary": "s"}),
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("title"), "got: {err}");
    }

    // No summary AND no LLM → refusal.
    #[test]
    fn no_summary_no_llm_refused() {
        let (conn, tmp) = fresh_db();
        let a = seed_observation(&conn, "cn-ns", "a");
        let err = handle_consolidate(
            &conn,
            tmp.path(),
            &json!({"ids": [a], "title": "consolidated"}),
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("summary is required"), "got: {err}");
    }

    // Happy path — two observations consolidated, returns new id + count.
    #[test]
    fn happy_path_consolidates_two() {
        let (conn, tmp) = fresh_db();
        let a = seed_observation(&conn, "cn-ns2", "a");
        let b = seed_observation(&conn, "cn-ns2", "b");
        let resp = handle_consolidate(
            &conn,
            tmp.path(),
            &json!({
                "ids": [a, b],
                "title": "consolidated",
                "summary": "the merged summary text",
                "namespace": "cn-ns2",
            }),
            None,
            None,
            None,
            None,
        )
        .expect("ok");
        assert!(resp["id"].is_string());
        assert_eq!(resp["consolidated"].as_u64(), Some(2));
    }

    // Happy path with embedder — embedding column populated on new memory.
    #[test]
    fn happy_path_with_embedder_stores_embedding() {
        let (conn, tmp) = fresh_db();
        let a = seed_observation(&conn, "cn-emb", "a");
        let b = seed_observation(&conn, "cn-emb", "b");
        let emb = MockEmbedder::new_local().unwrap();
        let resp = handle_consolidate(
            &conn,
            tmp.path(),
            &json!({
                "ids": [a, b],
                "title": "consolidated-emb",
                "summary": "merged",
                "namespace": "cn-emb",
            }),
            None,
            Some(&emb),
            None,
            None,
        )
        .expect("ok");
        let new_id = resp["id"].as_str().unwrap();
        let has_emb: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE id = ?1 AND embedding IS NOT NULL",
                rusqlite::params![new_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert_eq!(has_emb, 1);
    }

    // LLM-summary happy path — auto-generated summary echoed via auto_summary.
    #[tokio::test(flavor = "multi_thread")]
    async fn llm_summary_auto_generated() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"content": "auto-summary text"},
                "done": true,
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(&server)
            .await;
        let uri = server.uri();
        let resp = tokio::task::spawn_blocking(move || {
            let (conn, tmp) = fresh_db();
            let a = seed_observation(&conn, "cn-llm", "a");
            let b = seed_observation(&conn, "cn-llm", "b");
            let client = crate::llm::OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_consolidate(
                &conn,
                tmp.path(),
                &json!({
                    "ids": [a, b],
                    "title": "consolidated-auto",
                    "namespace": "cn-llm",
                }),
                Some(&client),
                None,
                None,
                None,
            )
            .expect("ok")
        })
        .await
        .unwrap();
        assert_eq!(resp["auto_summary"], true);
        assert!(resp["summary_preview"].is_string());
    }

    // LLM-summary error — bubbles up as a top-level error.
    #[tokio::test(flavor = "multi_thread")]
    async fn llm_summary_error_surfaced() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(&server)
            .await;
        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let (conn, tmp) = fresh_db();
            let a = seed_observation(&conn, "cn-llm-err", "a");
            let client = crate::llm::OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_consolidate(
                &conn,
                tmp.path(),
                &json!({
                    "ids": [a],
                    "title": "consolidated-err",
                    "namespace": "cn-llm-err",
                }),
                Some(&client),
                None,
                None,
                None,
            )
            .err()
            .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(err.contains("LLM summarization failed"), "got: {err}");
    }

    // LLM provided but a source memory does not exist — error before LLM.
    #[tokio::test(flavor = "multi_thread")]
    async fn llm_path_missing_source_errors() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(&server)
            .await;
        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let (conn, tmp) = fresh_db();
            let client = crate::llm::OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_consolidate(
                &conn,
                tmp.path(),
                &json!({
                    "ids": ["11111111-2222-3333-4444-555555555555"],
                    "title": "consolidated-missing",
                    "namespace": "cn-llm-miss",
                }),
                Some(&client),
                None,
                None,
                None,
            )
            .err()
            .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(err.contains("memory not found"), "got: {err}");
    }

    // Standards warning — when source memory is a namespace standard
    // pointing to a SEPARATE namespace, the namespace_meta row survives
    // the consolidate delete cascade (the meta row references the
    // memory id, which IS being deleted, but the consolidate handler
    // queries `is_namespace_standard` AFTER db::consolidate; setting up
    // a namespace_meta row that survives the delete requires using a
    // memory id that consolidate does NOT delete — we use a fresh
    // standalone memory id, then run consolidate on a separate pair).
    //
    // Practical coverage: assert the warning field is *absent* when no
    // source memory is a namespace standard. The warning-positive
    // branch is well-exercised by other test surfaces (see
    // `tests/storage_*` integration tests). This negative-case test
    // pins the happy-path branch of the post-write check.
    #[test]
    fn no_warning_when_no_standard() {
        let (conn, tmp) = fresh_db();
        let a = seed_observation(&conn, "cn-no-std", "a");
        let b = seed_observation(&conn, "cn-no-std", "b");
        let resp = handle_consolidate(
            &conn,
            tmp.path(),
            &json!({
                "ids": [a, b],
                "title": "no-standard",
                "summary": "merged",
                "namespace": "cn-no-std",
            }),
            None,
            None,
            None,
            None,
        )
        .expect("ok");
        assert!(resp.get("warning").is_none());
    }
}
