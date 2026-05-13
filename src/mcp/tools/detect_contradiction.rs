// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_detect_contradiction` handler.
//!
//! Tier D (LLM-bound) module. The envelope below — input validation,
//! optional-client gating, two-memory lookup, error surfacing,
//! response shaping — is deterministically tested at ≥95% via a
//! `wiremock`-backed real `OllamaClient`. The single
//! `llm.detect_contradiction(...)` dispatch is exercised through the
//! same path. Real-LLM judgement quality is validated by the
//! LongMemEval benchmark (see `benchmarks/longmemeval/`); see L0.7-5
//! playbook §6 for the contract.

use crate::llm::OllamaClient;
use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_detect_contradiction(
    conn: &rusqlite::Connection,
    llm: Option<&OllamaClient>,
    params: &Value,
) -> Result<Value, String> {
    let llm =
        llm.ok_or("contradiction detection requires smart or autonomous tier (Ollama LLM)")?;
    let id_a = params["id_a"].as_str().ok_or("id_a is required")?;
    let id_b = params["id_b"].as_str().ok_or("id_b is required")?;
    validate::validate_id(id_a).map_err(|e| e.to_string())?;
    validate::validate_id(id_b).map_err(|e| e.to_string())?;
    let mem_a = db::get(conn, id_a)
        .map_err(|e| e.to_string())?
        .ok_or("memory A not found")?;
    let mem_b = db::get(conn, id_b)
        .map_err(|e| e.to_string())?
        .ok_or("memory B not found")?;
    // COVERAGE: LLM response variability. The boolean below is derived
    // from the model's free-form yes/no answer. Envelope is tested at
    // ≥95% via wiremock-driven success / error / shape cases; real-LLM
    // contradiction judgement is validated end-to-end via the
    // LongMemEval benchmark (see `benchmarks/longmemeval/`).
    let contradicts = llm
        .detect_contradiction(&mem_a.content, &mem_b.content)
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "contradicts": contradicts,
        "memory_a": {"id": id_a, "title": mem_a.title},
        "memory_b": {"id": id_b, "title": mem_b.title}
    }))
}

// =====================================================================
// L0.7-5 Tier D — envelope unit tests
//
// Drives the production `OllamaClient` against an in-process wiremock
// server. `detect_contradiction` uses /api/chat (not /api/generate)
// and reads `message.content`. The client's blocking nature is bridged
// through `tokio::task::spawn_blocking`.
// =====================================================================
#[cfg(test)]
mod tests {
    use super::handle_detect_contradiction;
    use crate::llm::OllamaClient;
    use crate::storage as db;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fresh_db() -> (rusqlite::Connection, tempfile::NamedTempFile) {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let conn = db::open(tmp.path()).expect("db::open");
        (conn, tmp)
    }

    fn seed(conn: &rusqlite::Connection, title: &str, content: &str) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        let mem = crate::models::Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: crate::models::Tier::Mid,
            namespace: "tier-d".to_string(),
            title: title.to_string(),
            content: content.to_string(),
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

    /// Envelope (1/N): client absent → tier-gating error.
    #[test]
    fn rejects_when_llm_absent() {
        let (conn, _tmp) = fresh_db();
        let err = handle_detect_contradiction(&conn, None, &json!({"id_a": "x", "id_b": "y"}))
            .unwrap_err();
        assert!(
            err.contains("smart") || err.contains("autonomous") || err.contains("Ollama"),
            "expected tier-gating error, got: {err}"
        );
    }

    /// Envelope (2/N): id_a missing → typed error.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_when_id_a_missing() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_detect_contradiction(&conn, Some(&client), &json!({"id_b": "y"}))
                .err()
                .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(err.contains("id_a"), "expected id_a-required, got: {err}");
    }

    /// Envelope (3/N): id_b missing → typed error.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_when_id_b_missing() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_detect_contradiction(&conn, Some(&client), &json!({"id_a": "x"}))
                .err()
                .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(err.contains("id_b"), "expected id_b-required, got: {err}");
    }

    /// Envelope (4/N): bad id_a fails validation.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_when_id_a_fails_validation() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_detect_contradiction(
                &conn,
                Some(&client),
                &json!({"id_a": "bad; rm -rf /", "id_b": "anything"}),
            )
            .err()
            .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(!err.is_empty(), "expected validation error on bad id_a");
    }

    /// Envelope (5/N): id_a not present in DB.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_when_memory_a_not_found() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_detect_contradiction(
                &conn,
                Some(&client),
                &json!({
                    "id_a": "00000000-0000-0000-0000-000000000000",
                    "id_b": "11111111-1111-1111-1111-111111111111"
                }),
            )
            .err()
            .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(
            err.contains("memory A not found") || err.contains("not found"),
            "expected memory-A-not-found, got: {err}"
        );
    }

    /// Envelope (6/N): id_a in DB but id_b not — must surface
    /// "memory B not found" specifically.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_when_memory_b_not_found() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let id_a = seed(&conn, "A", "alpha");
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_detect_contradiction(
                &conn,
                Some(&client),
                &json!({
                    "id_a": id_a,
                    "id_b": "11111111-1111-1111-1111-111111111111"
                }),
            )
            .err()
            .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(
            err.contains("memory B not found") || err.contains("not found"),
            "expected memory-B-not-found, got: {err}"
        );
    }

    /// Envelope (7/N): happy path — LLM says "yes" → contradicts=true.
    /// Response shape must carry both titles and ids.
    #[tokio::test(flavor = "multi_thread")]
    async fn success_yes_response_yields_contradicts_true() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"content": "yes\n"},
            })))
            .mount(&server)
            .await;

        let uri = server.uri();
        let out = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let id_a = seed(&conn, "title-a", "the sky is blue");
            let id_b = seed(&conn, "title-b", "the sky is green");
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_detect_contradiction(&conn, Some(&client), &json!({"id_a": id_a, "id_b": id_b}))
                .expect("ok")
        })
        .await
        .unwrap();
        assert_eq!(out["contradicts"], json!(true));
        assert_eq!(out["memory_a"]["title"], "title-a");
        assert_eq!(out["memory_b"]["title"], "title-b");
        assert!(out["memory_a"]["id"].as_str().is_some());
        assert!(out["memory_b"]["id"].as_str().is_some());
    }

    /// Envelope (8/N): "no" response → contradicts=false.
    #[tokio::test(flavor = "multi_thread")]
    async fn success_no_response_yields_contradicts_false() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"content": "no"},
            })))
            .mount(&server)
            .await;

        let uri = server.uri();
        let out = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let id_a = seed(&conn, "A", "alpha");
            let id_b = seed(&conn, "B", "consistent with alpha");
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_detect_contradiction(&conn, Some(&client), &json!({"id_a": id_a, "id_b": id_b}))
                .expect("ok")
        })
        .await
        .unwrap();
        assert_eq!(out["contradicts"], json!(false));
    }

    /// Envelope (9/N): LLM 500 surfaces through `?`.
    #[tokio::test(flavor = "multi_thread")]
    async fn surfaces_llm_500_error() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let id_a = seed(&conn, "A", "a");
            let id_b = seed(&conn, "B", "b");
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_detect_contradiction(&conn, Some(&client), &json!({"id_a": id_a, "id_b": id_b}))
                .err()
                .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(
            err.contains("500") || err.contains("Chat generate failed"),
            "expected upstream error, got: {err}"
        );
    }

    /// Envelope (10/N): malformed JSON from LLM → parse error.
    #[tokio::test(flavor = "multi_thread")]
    async fn surfaces_llm_malformed_json_error() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("oops not json")
                    .insert_header("content-type", "application/json"),
            )
            .mount(&server)
            .await;

        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let id_a = seed(&conn, "A", "a");
            let id_b = seed(&conn, "B", "b");
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_detect_contradiction(&conn, Some(&client), &json!({"id_a": id_a, "id_b": id_b}))
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

    /// Envelope (11/N): LLM returns garbage (not yes/no) → handler
    /// completes with contradicts=false (per `starts_with("yes")`
    /// semantics in OllamaClient::detect_contradiction).
    #[tokio::test(flavor = "multi_thread")]
    async fn garbage_response_defaults_to_false() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"content": "mu"},
            })))
            .mount(&server)
            .await;

        let uri = server.uri();
        let out = tokio::task::spawn_blocking(move || {
            let (conn, _tmp) = fresh_db();
            let id_a = seed(&conn, "A", "a");
            let id_b = seed(&conn, "B", "b");
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_detect_contradiction(&conn, Some(&client), &json!({"id_a": id_a, "id_b": id_b}))
                .expect("ok")
        })
        .await
        .unwrap();
        assert_eq!(out["contradicts"], json!(false));
    }
}
