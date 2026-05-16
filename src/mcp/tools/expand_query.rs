// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_expand_query` handler.
//!
//! Tier D (LLM-bound) module. The envelope below — input parsing,
//! optional-client gating, error surfacing, response shaping — is
//! deterministically tested at ≥95%. The single `llm.expand_query(...)`
//! call dispatches to `OllamaClient` and is exercised in unit tests
//! via a `wiremock`-backed real client so the parse-then-shape pipeline
//! is verified end-to-end without a running Ollama daemon. Real-LLM
//! semantic quality is validated by the LongMemEval benchmark
//! (see `benchmarks/longmemeval/`); see L0.7-5 playbook §6.

use crate::llm::OllamaClient;
use serde_json::{Value, json};
pub(super) fn handle_expand_query(
    llm: Option<&OllamaClient>,
    params: &Value,
) -> Result<Value, String> {
    let llm = llm.ok_or("query expansion requires smart or autonomous tier (Ollama LLM)")?;
    let query = params["query"].as_str().ok_or("query is required")?;
    // COVERAGE: LLM response variability. The call below produces a
    // String whose content depends on the underlying model. Envelope
    // is tested at ≥95% via wiremock-driven success / error / shape
    // cases below; real-LLM behaviour is validated end-to-end via
    // the LongMemEval benchmark (see `benchmarks/longmemeval/`).
    let terms = llm.expand_query(query).map_err(|e| e.to_string())?;
    Ok(json!({"original": query, "expanded_terms": terms}))
}

// =====================================================================
// L0.7-5 Tier D — envelope unit tests
//
// Strategy: drive the production `OllamaClient` against an in-process
// `wiremock` server that speaks the subset of Ollama's HTTP surface
// (`/api/tags` + `/api/chat`). `OllamaClient` is blocking, so each
// test uses `tokio::task::spawn_blocking` to keep the runtime free.
//
// Determinism: every wiremock response is hard-coded; no clocks, no
// retries, no real network. The 5s health-probe timeout caps the
// worst-case flake on the constructor's `is_available` check.
// =====================================================================
#[cfg(test)]
mod tests {
    use super::handle_expand_query;
    use crate::llm::OllamaClient;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Mount the permissive `/api/tags` health-check responder so
    /// `OllamaClient::new_with_url` returns Ok (it probes /api/tags as
    /// a liveness gate before returning the client).
    async fn mount_tags_ok(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(server)
            .await;
    }

    /// Envelope (1/N): missing LLM client → typed error string. This
    /// is the keyword-tier fallthrough — the daemon constructs no
    /// client when the operator is on the keyword / semantic tier.
    #[test]
    fn rejects_when_llm_absent() {
        let err = handle_expand_query(None, &json!({"query": "anything"})).unwrap_err();
        assert!(
            err.contains("smart") || err.contains("autonomous") || err.contains("Ollama"),
            "expected tier-gating error, got: {err}"
        );
    }

    /// Envelope (2/N): missing `query` field → typed error string.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_when_query_missing() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_expand_query(Some(&client), &json!({}))
                .err()
                .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(err.contains("query"), "expected query-required, got: {err}");
    }

    /// Envelope (3/N): `query` field present but wrong type (not a
    /// string) → typed error. Mirrors the as_str() guard.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_when_query_is_not_string() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_expand_query(Some(&client), &json!({"query": 42}))
                .err()
                .unwrap_or_default()
        })
        .await
        .unwrap();
        assert!(err.contains("query"), "expected query-required, got: {err}");
    }

    /// Envelope (4/N): happy path — LLM returns three terms newline-
    /// delimited; the envelope must surface them as `expanded_terms`
    /// alongside the unchanged `original` query.
    #[tokio::test(flavor = "multi_thread")]
    async fn success_shapes_expanded_terms() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"content": "alpha\nbeta\ngamma\n"},
            })))
            .mount(&server)
            .await;

        let uri = server.uri();
        let value = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_expand_query(Some(&client), &json!({"query": "neural networks"}))
        })
        .await
        .unwrap()
        .expect("handler should succeed");
        assert_eq!(value["original"], "neural networks");
        let terms = value["expanded_terms"].as_array().unwrap();
        assert_eq!(terms.len(), 3);
        assert_eq!(terms[0], "alpha");
        assert_eq!(terms[1], "beta");
        assert_eq!(terms[2], "gamma");
    }

    /// Envelope (5/N): LLM returns an empty body — the envelope still
    /// completes; `expanded_terms` is an empty array. This is the
    /// "no-op" / LLM-returns-nothing path called out in the playbook.
    #[tokio::test(flavor = "multi_thread")]
    async fn success_with_empty_response_yields_no_terms() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "message": {"content": "\n\n   \n"},
            })))
            .mount(&server)
            .await;

        let uri = server.uri();
        let value = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_expand_query(Some(&client), &json!({"query": "x"}))
        })
        .await
        .unwrap()
        .expect("handler should succeed");
        let terms = value["expanded_terms"].as_array().unwrap();
        assert!(terms.is_empty(), "blank-only response collapses to []");
    }

    /// Envelope (6/N): LLM 500 → error surfaces through the `?` ladder
    /// as a stringified anyhow error. Tests the `map_err(|e| e.to_string())`
    /// adapter on the LLM call line.
    #[tokio::test(flavor = "multi_thread")]
    async fn surfaces_llm_500_error_through_envelope() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_expand_query(Some(&client), &json!({"query": "q"}))
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

    /// Envelope (7/N): LLM returns malformed JSON → error string
    /// reflects the parse failure. Reaches the `parse chat response`
    /// arm in `OllamaClient::generate`.
    #[tokio::test(flavor = "multi_thread")]
    async fn surfaces_llm_malformed_json_error() {
        let server = MockServer::start().await;
        mount_tags_ok(&server).await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("{ not valid")
                    .insert_header("content-type", "application/json"),
            )
            .mount(&server)
            .await;

        let uri = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let client = OllamaClient::new_with_url(&uri, "test-model").unwrap();
            handle_expand_query(Some(&client), &json!({"query": "q"}))
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
