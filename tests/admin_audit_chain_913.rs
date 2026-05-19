// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #913 — admin-action forensic-audit emission audit sweep.
//!
//! Follow-on to #911: #911 closed only `register_agent` + `archive_purge`.
//! Per the operator's "no half-measures" mandate the full audit-pass
//! enumerated EVERY admin / sensitive state-changing surface across
//! HTTP, MCP, and CLI and threaded `governance::audit::record_decision`
//! through each one BEFORE the storage write so the audit trail captures
//! intent regardless of downstream success.
//!
//! These tests pin the four highest-value surfaces:
//!
//! 1. `namespace_set_standard_emits_forensic_audit_entry` — HTTP
//!    `POST /api/v1/namespaces/{ns}/standard` lands a
//!    `kind=namespace_set_standard` row.
//! 2. `memory_delete_emits_forensic_audit_entry` — HTTP
//!    `DELETE /api/v1/memories/{id}` lands a `kind=memory_delete` row.
//! 3. `pending_approve_emits_forensic_audit_entry` — MCP
//!    `memory_pending_approve` lands a `kind=pending_approve` row.
//! 4. `archive_purge_emits_forensic_audit_entry_mcp` — MCP
//!    `memory_archive_purge` lands a `kind=archive_purge` row (the HTTP
//!    side is already pinned by #911's test; this is the MCP-surface
//!    twin).
//!
//! Forensic sink is process-global so these tests serialise via the
//! same lock pattern used by `admin_action_forensic_audit.rs`.

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
// `await_holding_lock` mirrors the discipline used in the sibling
// `tests/admin_action_forensic_audit.rs` suite — the std::sync::Mutex
// protects the process-global forensic SINK from cross-test races,
// never awaiting on a non-async lock. Holding our test-level guard
// across `oneshot(...)` is the load-bearing serialisation.
#![allow(clippy::missing_panics_doc, clippy::await_holding_lock)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::governance::audit as forensic;
use ai_memory::handlers::{ApiKeyState, AppState, Db};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tempfile::{NamedTempFile, TempDir};
use tower::ServiceExt as _;

fn forensic_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn local_runs_root() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".local-runs")
        .join("issue-913-admin-audit-test")
}

fn fresh_dir() -> TempDir {
    let root = local_runs_root();
    std::fs::create_dir_all(&root).ok();
    tempfile::tempdir_in(&root).expect("tempdir under .local-runs")
}

fn build_router_fixture() -> (axum::Router, NamedTempFile) {
    let f = NamedTempFile::new().expect("tempfile");
    let db_path = f.path().to_path_buf();
    let _ = ai_memory::db::open(&db_path).expect("db::open");
    let conn = ai_memory::db::open(&db_path).expect("reopen for AppState");
    let db: Db = Arc::new(tokio::sync::Mutex::new((
        conn,
        db_path.clone(),
        ResolvedTtl::default(),
        true,
    )));
    #[cfg(feature = "sal")]
    let store: Arc<dyn ai_memory::store::MemoryStore> =
        Arc::new(ai_memory::store::sqlite::SqliteStore::open(&db_path).expect("open SqliteStore"));
    let app_state = AppState {
        db,
        embedder: Arc::new(None),
        vector_index: Arc::new(tokio::sync::Mutex::new(None)),
        federation: Arc::new(None),
        tier_config: Arc::new(FeatureTier::Keyword.config()),
        scoring: Arc::new(ResolvedScoring::default()),
        profile: Arc::new(ai_memory::profile::Profile::core()),
        mcp_config: Arc::new(None),
        active_keypair: Arc::new(None),
        family_embeddings: Arc::new(tokio::sync::RwLock::new(Some(Vec::new()))),
        storage_backend: ai_memory::handlers::StorageBackend::Sqlite,
        #[cfg(feature = "sal")]
        store,
        llm: Arc::new(None),
        auto_tag_model: Arc::new(None),
        llm_call_timeout: std::time::Duration::from_secs(30),
        replay_cache: Arc::new(ai_memory::identity::replay::ReplayCache::default()),
        verify_require_nonce: false,
        autonomous_hooks: false,
        recall_scope: Arc::new(None),
        deferred_audit_queue: Arc::new(None),
    };
    let api_key_state = ApiKeyState {
        key: None,
        mtls_enforced: false,
    };
    let router = ai_memory::build_router(api_key_state, app_state);
    (router, f)
}

fn collect_kind_rows(dir: &std::path::Path, kind: &str) -> Vec<serde_json::Value> {
    use std::io::{BufRead, BufReader};
    let mut hits = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(n) = name.to_str() else { continue };
        if !n.starts_with("forensic-") || !n.to_ascii_lowercase().ends_with(".jsonl") {
            continue;
        }
        let Ok(f) = std::fs::File::open(entry.path()) else {
            continue;
        };
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            if v.get("kind").and_then(|k| k.as_str()) == Some(kind) {
                hits.push(v);
            }
        }
    }
    hits
}

/// #913 surface 1 — HTTP `POST /api/v1/namespaces/{ns}/standard` must
/// emit a `kind=namespace_set_standard` row to the forensic chain.
/// Pre-#913 the handler mutated the governance policy gating EVERY
/// downstream write into the namespace WITHOUT producing a chain entry.
#[tokio::test]
async fn namespace_set_standard_emits_forensic_audit_entry() {
    let _g = forensic_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = fresh_dir();
    forensic::shutdown();
    forensic::init(dir.path(), None).expect("init forensic sink");

    let (router, _f) = build_router_fixture();

    // Seed a memory to point the standard at — the handler requires a
    // valid standard_id even when the namespace doesn't yet exist.
    let seed_req = Request::builder()
        .method("POST")
        .uri("/api/v1/memories")
        .header("content-type", "application/json")
        .header("x-agent-id", "operator-alice")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "tier": "long",
                "namespace": "audit-913-ns",
                "title": "_standard:audit-913-ns",
                "content": "policy",
                "tags": ["_namespace_standard"],
                "priority": 5,
            }))
            .unwrap(),
        ))
        .unwrap();
    let seed_resp = router.clone().oneshot(seed_req).await.unwrap();
    assert!(
        seed_resp.status().is_success(),
        "seed memory must store; got {}",
        seed_resp.status()
    );
    let seed_body = axum::body::to_bytes(seed_resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let seed_v: serde_json::Value = serde_json::from_slice(&seed_body).unwrap();
    let standard_id = seed_v["id"].as_str().expect("seed id").to_string();

    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/namespaces/audit-913-ns/standard")
        .header("content-type", "application/json")
        .header("x-agent-id", "operator-alice")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "id": standard_id })).unwrap(),
        ))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    assert!(
        status.is_success(),
        "set_namespace_standard must succeed, got {status}"
    );

    forensic::shutdown();
    let rows = collect_kind_rows(dir.path(), "namespace_set_standard");
    assert!(
        !rows.is_empty(),
        "#913 regression: namespace_set_standard must emit a forensic row, found 0 under {}",
        dir.path().display()
    );
    let row = &rows[0];
    assert_eq!(
        row.get("actor").and_then(|v| v.as_str()),
        Some("operator-alice"),
        "actor must be X-Agent-Id caller; got row={row}"
    );
    assert_eq!(
        row.get("payload")
            .and_then(|p| p.get("namespace"))
            .and_then(|v| v.as_str()),
        Some("audit-913-ns"),
        "payload.namespace must echo the path segment; got row={row}"
    );
}

/// #913 surface 2 — HTTP `DELETE /api/v1/memories/{id}` must emit a
/// `kind=memory_delete` forensic-chain row.
#[tokio::test]
async fn memory_delete_emits_forensic_audit_entry() {
    let _g = forensic_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = fresh_dir();
    forensic::shutdown();
    forensic::init(dir.path(), None).expect("init forensic sink");

    let (router, _f) = build_router_fixture();

    // Seed a memory to delete.
    let seed_req = Request::builder()
        .method("POST")
        .uri("/api/v1/memories")
        .header("content-type", "application/json")
        .header("x-agent-id", "operator-alice")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "tier": "long",
                "namespace": "audit-913-del-ns",
                "title": "to delete",
                "content": "payload",
                "tags": [],
                "priority": 5,
            }))
            .unwrap(),
        ))
        .unwrap();
    let seed_resp = router.clone().oneshot(seed_req).await.unwrap();
    assert!(seed_resp.status().is_success());
    let seed_body = axum::body::to_bytes(seed_resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let seed_v: serde_json::Value = serde_json::from_slice(&seed_body).unwrap();
    let mem_id = seed_v["id"].as_str().expect("seed id").to_string();

    let req = Request::builder()
        .method("DELETE")
        .uri(format!("/api/v1/memories/{mem_id}"))
        .header("x-agent-id", "operator-alice")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "delete must succeed, got {status}"
    );

    forensic::shutdown();
    let rows = collect_kind_rows(dir.path(), "memory_delete");
    assert!(
        !rows.is_empty(),
        "#913 regression: memory_delete must emit a forensic row, found 0 under {}",
        dir.path().display()
    );
    let row = &rows[0];
    assert_eq!(
        row.get("actor").and_then(|v| v.as_str()),
        Some("operator-alice"),
        "actor must be X-Agent-Id caller; got row={row}"
    );
    assert_eq!(
        row.get("decision").and_then(|v| v.as_str()),
        Some("allow"),
        "decision must be `allow`; got row={row}"
    );
    assert_eq!(
        row.get("payload")
            .and_then(|p| p.get("id"))
            .and_then(|v| v.as_str()),
        Some(mem_id.as_str()),
        "payload.id must echo the path segment; got row={row}"
    );
}

/// #913 surface 3 — MCP `memory_pending_approve` must emit a
/// `kind=pending_approve` forensic-chain row. Exercised directly via
/// the MCP handler (the HTTP `approve_pending` handler is HMAC-gated
/// and harder to exercise without seeding the secret; the MCP handler
/// covers the same audit-emission contract).
#[tokio::test]
async fn pending_approve_emits_forensic_audit_entry_mcp() {
    let _g = forensic_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = fresh_dir();
    forensic::shutdown();
    forensic::init(dir.path(), None).expect("init forensic sink");

    let f = NamedTempFile::new().expect("tempfile");
    let conn = ai_memory::db::open(f.path()).expect("db::open");

    // Queue a pending action so approve has something to flip. Use the
    // `Promote` action which requires only a memory_id; the actual
    // execution may fail (target row absent) but record_decision lands
    // BEFORE that, which is exactly what we test.
    let pending_id = ai_memory::db::queue_pending_action(
        &conn,
        ai_memory::models::GovernedAction::Promote,
        "audit-913-pa",
        Some("11111111-2222-3333-4444-555555555555"),
        "ai:tester",
        &serde_json::json!({"target_tier": "long"}),
    )
    .expect("queue pending action");

    let _ = ai_memory::mcp::handle_pending_approve(
        &conn,
        &serde_json::json!({
            "id": pending_id,
            "agent_id": "ai:approver",
        }),
        None,
    );

    forensic::shutdown();
    let rows = collect_kind_rows(dir.path(), "pending_approve");
    assert!(
        !rows.is_empty(),
        "#913 regression: pending_approve must emit a forensic row, found 0 under {}",
        dir.path().display()
    );
    let row = &rows[0];
    assert_eq!(
        row.get("actor").and_then(|v| v.as_str()),
        Some("ai:approver"),
        "actor must be the supplied agent_id; got row={row}"
    );
    assert_eq!(
        row.get("decision").and_then(|v| v.as_str()),
        Some("allow"),
        "decision must be `allow` for approve; got row={row}"
    );
}

/// #913 surface 4 — MCP `memory_archive_purge` must emit a
/// `kind=archive_purge` forensic-chain row. The HTTP-side twin is
/// pinned by #911's `purge_archive_emits_forensic_audit_entry`; this
/// pins the MCP-surface companion that the audit-pass closed inline.
#[tokio::test]
async fn archive_purge_emits_forensic_audit_entry_mcp() {
    let _g = forensic_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = fresh_dir();
    forensic::shutdown();
    forensic::init(dir.path(), None).expect("init forensic sink");

    let f = NamedTempFile::new().expect("tempfile");
    let conn = ai_memory::db::open(f.path()).expect("db::open");

    let _ = ai_memory::mcp::handle_archive_purge_for_test(
        &conn,
        &serde_json::json!({
            "older_than_days": 7,
            "agent_id": "ai:tester",
        }),
    );

    forensic::shutdown();
    let rows = collect_kind_rows(dir.path(), "archive_purge");
    assert!(
        !rows.is_empty(),
        "#913 regression: archive_purge (MCP) must emit a forensic row, found 0 under {}",
        dir.path().display()
    );
    let row = &rows[0];
    assert_eq!(
        row.get("actor").and_then(|v| v.as_str()),
        Some("ai:tester"),
        "actor must be the supplied agent_id; got row={row}"
    );
    assert_eq!(
        row.get("payload")
            .and_then(|p| p.get("older_than_days"))
            .and_then(serde_json::Value::as_i64),
        Some(7),
        "payload.older_than_days must echo the param; got row={row}"
    );
}
