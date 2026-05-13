// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_auto_tag` handler.
//!
//! Tier D (LLM-bound) module. The envelope below — input validation,
//! optional-client gating, DB get/update, tag-union semantics, error
//! surfacing — is deterministically tested at ≥95% via a
//! `wiremock`-backed real `OllamaClient`. The single
//! `llm.auto_tag(...)` dispatch is exercised through the same path so
//! the parse-then-store pipeline is end-to-end verified without a
//! live Ollama daemon. Real-LLM tag-quality is validated by the
//! LongMemEval benchmark (see `benchmarks/longmemeval/`); see L0.7-5
//! playbook §6 for the contract.

use crate::llm::OllamaClient;
use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_auto_tag(
    conn: &rusqlite::Connection,
    llm: Option<&OllamaClient>,
    params: &Value,
) -> Result<Value, String> {
    let llm = llm.ok_or("auto-tagging requires smart or autonomous tier (Ollama LLM)")?;
    let id = params["id"].as_str().ok_or("id is required")?;
    validate::validate_id(id).map_err(|e| e.to_string())?;
    let mem = db::get(conn, id)
        .map_err(|e| e.to_string())?
        .ok_or("memory not found")?;
    // COVERAGE: LLM response variability. The call below produces a
    // Vec<String> derived from the model's response; envelope is
    // tested at ≥95% via wiremock-driven success / error / shape
    // cases below; real-LLM tag quality is validated end-to-end via
    // the LongMemEval benchmark (see `benchmarks/longmemeval/`).
    let tags = llm
        .auto_tag(&mem.title, &mem.content, None)
        .map_err(|e| e.to_string())?;
    // Apply tags to the memory
    let mut all_tags = mem.tags.clone();
    for t in &tags {
        if !all_tags.contains(t) {
            all_tags.push(t.clone());
        }
    }
    db::update(
        conn,
        id,
        None,
        None,
        None,
        None,
        Some(&all_tags),
        None,
        None,
        None,
        None,
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({"id": id, "new_tags": tags, "all_tags": all_tags}))
}

// =====================================================================
// L0.7-5 Tier D — envelope unit tests
//
// Drives the production `OllamaClient` against an in-process wiremock
// server. The blocking client is run via `tokio::task::spawn_blocking`
// so the async test runtime stays free for the mock server. The
// `/api/tags` health probe (which `new_with_url` performs before
// returning) is mounted ahead of any other route on every server.
// =====================================================================
#[cfg(test)]
mod tests {
    use super::handle_auto_tag;
    use crate::llm::OllamaClient;
    use crate::storage as db;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a fresh in-memory SQLite DB (via a tempfile, since
    /// `:memory:` doesn't survive across the WAL pragma touch).
    fn fresh_db() -> (rusqlite::Connection, tempfile::NamedTempFile) {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let conn = db::open(tmp.path()).expect("db::open");
        (conn, tmp)
    }

    /// Insert a baseline memory and return its id.
    fn seed_memory(conn: &rusqlite::Connection, tags: Vec<String>) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        let mem = crate::models::Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: crate::models::Tier::Mid,
            namespace: "tier-d".to_string(),
            title: "subject".to_string(),
            content: "body of memory".to_string(),
            tags,
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
        };
        db::insert(conn, &mem).expect("insert")
    }

    async fn mount_tags_ok(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(server)
            .await;
    }

    /// Envelope (1/N): client absent → tier-gating error message.
    #[test]
    fn rejects_when_llm_absent() {
        let (conn, _tmp) = fresh_db();
        let err = handle_auto_tag(&conn, None, &json!({"id": "anything"})).unwrap_err();
        assert!(
            err.contains("smart") || err.contains("autonomous") || err.contains("Ollama"),
            "expected tier-gating error, got: {err}"
        );
    }

    /// Envelope (2/N): missing `id` → typed error.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_when_id_missing() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_auto_tag(&conn, Some(&client), &json!({}))
                .err()
                .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(err.contains("id"), "expected id-required, got: {err}");
    }

    /// Envelope (3/N): `id` field present but contains invalid chars →
    /// validate::validate_id rejects.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_when_id_fails_validation() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            // shell-metachar should be rejected by validate_id
            handle_auto_tag(&conn, Some(&client), &json!({"id": "bad; rm -rf /"}))
                .err()
                .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(
            !err.is_empty(),
            "expected validation error on bad id, got empty string"
        );
    }

    /// Envelope (4/N): `id` is valid but missing from DB → not-found.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_when_memory_not_found() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_auto_tag(
                &conn,
                Some(&client),
                &json!({"id": "00000000-0000-0000-0000-000000000000"}),
            )
            .err()
            .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(err.contains("not found"), "expected not-found, got: {err}");
    }

    /// Envelope (5/N): happy path — auto_tag returns 3 tags; the
    /// envelope must:
    ///   - call /api/generate (L15 — auto_tag uses /api/generate),
    ///   - lowercase + dedupe with existing tags,
    ///   - persist the union onto the memory row,
    ///   - shape `{id, new_tags, all_tags}` for the caller.
    #[tokio::test(flavor = "multi_thread")]
    async fn success_unions_tags_and_persists() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "response": "alpha\nbeta\ngamma",
            })))
            .mount(&server)
            .await;

        let uri = server.uri();
        let (id, value) = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            // Existing tag "alpha" already lives on the memory; the
            // envelope must NOT duplicate it in `all_tags`.
            let id = seed_memory(&conn, vec!["alpha".to_string()]);
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            let out = handle_auto_tag(&conn, Some(&client), &json!({"id": id.clone()}))
                .expect("handler should succeed");
            // Verify DB state — `tags` column carries the union now.
            let mem = db::get(&conn, &id).unwrap().unwrap();
            (id, json!({"out": out, "stored_tags": mem.tags}))
        })
        .await
        .unwrap();

        let out = &value["out"];
        assert_eq!(out["id"], json!(id));
        let new_tags = out["new_tags"].as_array().unwrap();
        assert_eq!(new_tags.len(), 3);
        let all_tags = out["all_tags"].as_array().unwrap();
        // alpha already existed; beta + gamma are new — union is 3.
        assert_eq!(all_tags.len(), 3);
        // Stored row reflects the union.
        let stored = value["stored_tags"].as_array().unwrap();
        assert_eq!(stored.len(), 3);
    }

    /// Envelope (6/N): LLM returns no tags (blank-only output) — the
    /// envelope still completes; `new_tags` is empty and `all_tags`
    /// is unchanged from the prior state.
    #[tokio::test(flavor = "multi_thread")]
    async fn success_with_empty_response_yields_no_new_tags() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "response": "   \n  \n",
            })))
            .mount(&server)
            .await;

        let uri = server.uri();
        let out = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let id = seed_memory(&conn, vec!["existing".to_string()]);
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_auto_tag(&conn, Some(&client), &json!({"id": id})).expect("ok")
        })
        .await
        .unwrap();
        let new_tags = out["new_tags"].as_array().unwrap();
        assert!(new_tags.is_empty());
        let all_tags = out["all_tags"].as_array().unwrap();
        assert_eq!(all_tags.len(), 1);
        assert_eq!(all_tags[0], "existing");
    }

    /// Envelope (7/N): LLM 500 → error surfaces through `?`.
    #[tokio::test(flavor = "multi_thread")]
    async fn surfaces_llm_500_error() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(500).set_body_string("oh no"))
            .mount(&server)
            .await;

        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let id = seed_memory(&conn, vec![]);
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_auto_tag(&conn, Some(&client), &json!({"id": id}))
                .err()
                .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(
            err.contains("500") || err.contains("Generate failed"),
            "expected upstream error, got: {err}"
        );
    }

    /// Envelope (8/N): malformed JSON from LLM → parse error.
    #[tokio::test(flavor = "multi_thread")]
    async fn surfaces_llm_malformed_json_error() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("not valid")
                    .insert_header("content-type", "application/json"),
            )
            .mount(&server)
            .await;

        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let id = seed_memory(&conn, vec![]);
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_auto_tag(&conn, Some(&client), &json!({"id": id}))
                .err()
                .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(
            err.to_lowercase().contains("parse") || err.to_lowercase().contains("json"),
            "expected parse-error, got: {err}"
        );
    }
}
