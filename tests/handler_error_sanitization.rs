// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #851 (Wave-2 Tier-A3 SECURITY) — HTTP error message sanitization
//! regression suite.
//!
//! The HTTP surface in `src/handlers.rs` is reachable by unauthenticated
//! callers in the default configuration (the `api_key` middleware is
//! opt-in). This test crate exercises the malformed-request paths and
//! pins the contract:
//!
//!   * 4xx / 5xx response bodies MUST NOT contain raw `SQLite` error
//!     text, SQL fragments, on-disk DB paths, peer URLs, stack hints,
//!     or `agent_id` values learned from internal payloads.
//!   * Response bodies MUST be valid JSON with a top-level `error` field
//!     (wire-compat).
//!   * The handler MUST still emit a non-2xx status — sanitization
//!     never silently flips a failure into a success.
//!
//! Each test issues a request known to trigger one of the leak vectors
//! the audit found, then asserts on the response body for the absence
//! of a list of sentinel strings derived from the runtime's internal
//! state (DB path, SQL keywords, anyhow chain markers, etc.).

#![allow(clippy::needless_pass_by_value)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt as _;

/// Substrings that are NEVER allowed to appear in an HTTP error body.
/// The list deliberately overlaps with what `sanitize_bulk_row_error`
/// strips so a future leak (re-introducing `e.to_string()` in a response
/// payload) shows up as a concrete test failure.
const FORBIDDEN_LEAK_SUBSTRINGS: &[&str] = &[
    // SQLite / SQL hints
    "SqliteFailure",
    "sqlite",
    "SQLITE_",
    "no such table",
    "no such column",
    "UNIQUE constraint",
    "FOREIGN KEY",
    "rusqlite",
    "InvalidQuery",
    "syntax error near",
    "SELECT ",
    "INSERT INTO",
    "UPDATE ",
    "DELETE FROM",
    // Filesystem hints
    "/private/tmp/",
    "/var/folders/",
    "/Users/",
    ".sqlite",
    ".db-wal",
    ".db-shm",
    // anyhow stack hints
    "Caused by:",
    "stack backtrace",
    // peer / federation
    "http://192.",
    "http://10.",
    "http://172.",
    "reqwest::Error",
    "Connection refused",
];

/// Build an in-memory router with the same wiring `build_router` uses
/// in `serve()` so the test exercises the production handler chain.
fn build_router() -> axum::Router {
    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).unwrap();
    let path = std::path::PathBuf::from(":memory:");
    let db: ai_memory::handlers::Db = std::sync::Arc::new(tokio::sync::Mutex::new((
        conn,
        path,
        ai_memory::config::ResolvedTtl::default(),
        true,
    )));
    let app_state = ai_memory::handlers::AppState {
        db,
        embedder: std::sync::Arc::new(None),
        vector_index: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        federation: std::sync::Arc::new(None),
        tier_config: std::sync::Arc::new(ai_memory::config::FeatureTier::Keyword.config()),
        scoring: std::sync::Arc::new(ai_memory::config::ResolvedScoring::default()),
        profile: std::sync::Arc::new(ai_memory::profile::Profile::core()),
        mcp_config: std::sync::Arc::new(None),
        active_keypair: std::sync::Arc::new(None),
        family_embeddings: std::sync::Arc::new(tokio::sync::RwLock::new(Some(Vec::new()))),
        // v0.7.0 Wave-3 + L5/L15/H8 fields added since A3's base.
        storage_backend: ai_memory::handlers::StorageBackend::Sqlite,
        #[cfg(feature = "sal")]
        store: std::sync::Arc::new(
            ai_memory::store::sqlite::SqliteStore::open(std::path::Path::new(":memory:"))
                .expect("open SqliteStore"),
        ),
        llm: std::sync::Arc::new(None),
        auto_tag_model: std::sync::Arc::new(None),
        llm_call_timeout: std::time::Duration::from_secs(30),
        replay_cache: std::sync::Arc::new(ai_memory::identity::replay::ReplayCache::default()),
        verify_require_nonce: false,
        autonomous_hooks: false,
        recall_scope: std::sync::Arc::new(None),
        deferred_audit_queue: std::sync::Arc::new(None),
    };
    let api_key_state = ai_memory::handlers::ApiKeyState {
        key: None,
        mtls_enforced: false,
    };
    ai_memory::build_router(api_key_state, app_state)
}

/// Run the given request through the router and return (status, body-as-string).
async fn run_request(router: axum::Router, req: Request<Body>) -> (StatusCode, String) {
    let resp = router.oneshot(req).await.expect("router oneshot");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .expect("read body");
    let body = String::from_utf8_lossy(&bytes).into_owned();
    (status, body)
}

/// Assert that `body` does not contain any of the FORBIDDEN substrings.
fn assert_no_leaks(context: &str, body: &str) {
    for needle in FORBIDDEN_LEAK_SUBSTRINGS {
        assert!(
            !body.contains(needle),
            "{context}: response body leaks forbidden substring {needle:?}\nbody={body}"
        );
    }
}

/// Assert the body parses as JSON with a top-level `error` field whose
/// value is a string — the contract every sanitized 4xx/5xx path keeps.
fn assert_error_envelope(context: &str, body: &str) {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("{context}: body is not JSON: {e}; body={body}"));
    let err = parsed
        .get("error")
        .unwrap_or_else(|| panic!("{context}: body missing `error` field; body={body}"));
    assert!(
        err.is_string(),
        "{context}: `error` field is not a string; body={body}"
    );
}

// -------------------------------------------------------------------------
// /api/v1/import — pushes per-row errors into a returned `errors[]` array.
// Prior to issue #851 these strings included raw db::insert /
// validate_memory output.
// -------------------------------------------------------------------------

#[tokio::test]
async fn import_with_invalid_memory_returns_sanitized_errors() {
    let router = build_router();
    // Two memories: the first has an empty title (validate_memory will
    // reject), the second has a malformed id (rusqlite-side detection).
    let body = json!({
        "memories": [
            {
                "id": "00000000-0000-0000-0000-000000000001",
                "tier": "short",
                "namespace": "leak-test",
                "title": "",
                "content": "x",
                "tags": [],
                "priority": 1,
                "confidence": 1.0,
                "source": "test",
                "access_count": 0,
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z",
                "last_accessed_at": null,
                "expires_at": null,
                "metadata": {"agent_id": "leak-probe"}
            }
        ]
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/import")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, body) = run_request(router, req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "import returns 200 with partial errors: body={body}"
    );
    assert_no_leaks("import_with_invalid_memory", &body);
    // Echoing the user-supplied memory id paired with the raw error
    // would be observable here; pin its absence specifically.
    assert!(
        !body.contains("00000000-0000-0000-0000-000000000001"),
        "import response leaks the user-supplied memory id: {body}"
    );
}

// -------------------------------------------------------------------------
// /api/v1/memories/bulk — same shape as /import, distinct handler.
// -------------------------------------------------------------------------

#[tokio::test]
async fn bulk_create_with_invalid_row_returns_sanitized_errors() {
    let router = build_router();
    let body = json!([
        {
            "tier": "short",
            "namespace": "leak-test",
            "title": "",
            "content": "",
            "tags": [],
            "priority": 5,
            "confidence": 1.0,
            "source": "test",
            "metadata": {}
        }
    ]);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/memories/bulk")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, body) = run_request(router, req).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "bulk_create returns 200 with partial errors: body={body}"
    );
    assert_no_leaks("bulk_create_with_invalid_row", &body);
    // Specifically: the prior shape was `format!("{}: {}", body.title, e)`,
    // so a title containing a probe value would round-trip. We sent an
    // empty title, so this also pins the safer per-row classification.
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["created"], json!(0));
    let errors = parsed["errors"].as_array().expect("errors[] present");
    assert!(!errors.is_empty(), "expected at least one error row");
    for e in errors {
        let s = e.as_str().expect("error entries are strings");
        // The sanitizer collapses every validate / db / fanout error
        // into one of these five fixed labels.
        assert!(
            matches!(
                s,
                "validation failed"
                    | "conflict: already exists"
                    | "not found"
                    | "forbidden"
                    | "replication unavailable"
                    | "internal error"
            ),
            "bulk_create error entry not in allowlist: {s:?}"
        );
    }
}

// -------------------------------------------------------------------------
// /api/v1/forget — db::forget can return either the safe sentinel
// (`at least one of namespace, pattern, or tier is required`) or a raw
// rusqlite::Error from FTS query parsing. Issue #851 split the two.
// -------------------------------------------------------------------------

#[tokio::test]
async fn forget_with_no_filters_returns_safe_sentinel() {
    let router = build_router();
    let body = json!({});
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/forget")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, body) = run_request(router, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_error_envelope("forget_with_no_filters", &body);
    assert_no_leaks("forget_with_no_filters", &body);
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let msg = parsed["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("at least one of namespace"),
        "expected the safe sentinel for the no-filters case, got: {msg}"
    );
}

// -------------------------------------------------------------------------
// /api/v1/notify — routed through `mcp::handle_notify` which returns
// `Result<_, String>` with potentially raw DB-error text. Issue #851
// added `bad_request_opaque` at the call site.
// -------------------------------------------------------------------------

#[tokio::test]
async fn notify_with_invalid_tier_returns_sanitized_error() {
    let router = build_router();
    // All required fields present (so axum's JSON layer is satisfied)
    // but `tier` is unrecognised → `handle_notify` bails with an
    // internal `String` that the issue #851 `bad_request_opaque` path
    // must collapse to the constant envelope.
    let body = json!({
        "target_agent_id": "bob",
        "title": "hello",
        "payload": "from leak test",
        "agent_id": "leak-probe",
        "tier": "not-a-real-tier"
    });
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/notify")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, body) = run_request(router, req).await;
    assert!(
        status.is_client_error() || status.is_server_error(),
        "expected non-2xx for invalid-tier notify; got {status} body={body}"
    );
    assert_error_envelope("notify_with_invalid_tier", &body);
    assert_no_leaks("notify_with_invalid_tier", &body);
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let msg = parsed["error"].as_str().unwrap_or_default();
    // The sanitized path collapses to the constant string — the raw
    // "invalid tier: not-a-real-tier" must NOT appear.
    assert_eq!(
        msg, "invalid request",
        "notify error must be the constant sanitized message, got {msg:?}"
    );
}

// -------------------------------------------------------------------------
// /api/v1/inbox — routed through `mcp::handle_inbox`. Probe with an
// invalid agent_id to force the inner error path.
// -------------------------------------------------------------------------

#[tokio::test]
async fn inbox_with_invalid_agent_id_returns_sanitized_error() {
    let router = build_router();
    // The validate_agent_id rule rejects whitespace + control chars.
    // Use a value that fails on the regex tier rather than the empty-string
    // tier so we exercise the handler's error path.
    let req = Request::builder()
        .method("GET")
        .uri("/api/v1/inbox?agent_id=bad%20id%00with%20null")
        .body(Body::empty())
        .unwrap();
    let (status, body) = run_request(router, req).await;
    assert!(
        status.is_client_error() || status.is_server_error(),
        "expected non-2xx for malformed inbox query; got {status} body={body}"
    );
    assert_error_envelope("inbox_with_invalid_agent_id", &body);
    assert_no_leaks("inbox_with_invalid_agent_id", &body);
}

// -------------------------------------------------------------------------
// /api/v1/memories/{id} — GET with an obviously-invalid id triggers
// validate::validate_id failure. The current handler returns the
// (template, safe) validate message; pin that it stays JSON-shaped and
// leak-free.
// -------------------------------------------------------------------------

#[tokio::test]
async fn get_memory_with_invalid_id_returns_sanitized_error() {
    let router = build_router();
    let req = Request::builder()
        .method("GET")
        // Path containing a null byte gets percent-decoded and then fails
        // validate_id (rejects control characters).
        .uri("/api/v1/memories/has%00null")
        .body(Body::empty())
        .unwrap();
    let (status, body) = run_request(router, req).await;
    assert!(
        status.is_client_error(),
        "expected 4xx for invalid memory id; got {status} body={body}"
    );
    assert_error_envelope("get_memory_with_invalid_id", &body);
    assert_no_leaks("get_memory_with_invalid_id", &body);
}

// -------------------------------------------------------------------------
// /api/v1/memories with malformed JSON body — exercise the axum
// JSON-rejection path. Body must stay sanitized.
// -------------------------------------------------------------------------

#[tokio::test]
async fn create_memory_with_malformed_json_returns_sanitized_error() {
    let router = build_router();
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/memories")
        .header("content-type", "application/json")
        // Truncated JSON triggers axum's `JsonRejection` path. The handler
        // never even runs; axum's default body must still not leak.
        .body(Body::from("{\"title\": \"oops"))
        .unwrap();
    let (status, body) = run_request(router, req).await;
    assert!(status.is_client_error(), "expected 4xx for malformed JSON");
    // axum's default JSON rejection text is "Failed to parse the request body as JSON".
    // That phrasing is fine — but we still pin the leak allowlist.
    assert_no_leaks("create_memory_with_malformed_json", &body);
}

// -------------------------------------------------------------------------
// Helper-level unit tests for the sanitizer itself. These don't go
// through the router but pin the classifier's allowlist directly so a
// future refactor that breaks the mapping shows up here too.
// -------------------------------------------------------------------------

#[test]
fn sanitize_bulk_row_error_maps_validate_to_validation_failed() {
    let raw = "title cannot be empty";
    assert_eq!(
        ai_memory::handlers::sanitize_bulk_row_error(raw),
        "validation failed"
    );
}

#[test]
fn sanitize_bulk_row_error_maps_unique_constraint_to_conflict() {
    let raw = "UNIQUE constraint failed: memories.id";
    assert_eq!(
        ai_memory::handlers::sanitize_bulk_row_error(raw),
        "conflict: already exists"
    );
}

#[test]
fn sanitize_bulk_row_error_maps_quorum_miss_to_replication_unavailable() {
    let raw = "quorum not met: 1/2 acks for peer http://10.0.0.1:9077";
    assert_eq!(
        ai_memory::handlers::sanitize_bulk_row_error(raw),
        "replication unavailable"
    );
}

#[test]
fn sanitize_bulk_row_error_falls_back_to_internal_for_unknown() {
    // A raw SQL syntax error must NOT pass through; it falls back to
    // the safe default.
    let raw = "near \"SELEC\": syntax error in SELECT id FROM memories";
    let mapped = ai_memory::handlers::sanitize_bulk_row_error(raw);
    assert_eq!(mapped, "internal error");
    // Pin that the sanitized form does NOT contain the original SQL.
    assert!(!mapped.contains("SELECT"));
    assert!(!mapped.contains("memories"));
}
