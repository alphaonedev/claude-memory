// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #911 — admin-class action forensic-audit emission regression pin.
//!
//! Pre-#911 the two admin-class state-changing handlers
//! `POST /api/v1/agents` (`register_agent`) and `DELETE /api/v1/archive`
//! (`purge_archive`) landed their state changes WITHOUT emitting a row
//! to the signed forensic chain. That left SOC2-regulated deployments
//! unable to prove "who deleted what when" via the audit trail alone.
//!
//! The fix landed in `e40c81791` mirrors the established
//! `governance::audit::record_decision` call pattern used by
//! `governance::agent_action::emit_forensic_decision` for governance
//! check decisions. Each handler resolves the caller via the
//! `X-Agent-Id` header (falling back to `anonymous:…` for unattested
//! callers) and records a row BEFORE the storage write so the chain
//! entry survives even if the downstream write errors.
//!
//! These two tests prove the fix mechanically:
//!
//! 1. `register_agent_emits_forensic_audit_entry` — POST to
//!    `/api/v1/agents` lands a `kind=register_agent` row in the chain
//!    with `actor` matching the authenticated caller, `decision=allow`,
//!    and `payload.new_agent_id` matching the request body.
//! 2. `purge_archive_emits_forensic_audit_entry` — DELETE to
//!    `/api/v1/archive?older_than_days=0` lands a `kind=archive_purge`
//!    row with `payload.older_than_days = 0`, even when the query
//!    yields zero rows.

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
// `await_holding_lock` mirrors the discipline used in `tests/
// g3_postgres_verify_link_signature_roundtrip.rs` and the existing
// forensic-sink suite — the std::sync::Mutex protects the
// process-global forensic SINK from cross-test races, never awaiting
// while the in-process governance::audit code path holds an internal
// lock. Holding our test-level guard across `oneshot(...)` is the
// load-bearing serialisation.
#![allow(clippy::missing_panics_doc, clippy::await_holding_lock)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::governance::audit as forensic;
use ai_memory::handlers::{ApiKeyState, AppState, Db};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ed25519_dalek::SigningKey;
use rand_core::OsRng;
use tempfile::{NamedTempFile, TempDir};
use tower::ServiceExt as _;

/// Forensic sink is process-global; serialise across the two tests in
/// this file so they cannot race the OnceLock-backed `SINK`.
fn forensic_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn local_runs_root() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".local-runs")
        .join("issue-911-admin-audit-test")
}

fn fresh_dir() -> TempDir {
    let root = local_runs_root();
    std::fs::create_dir_all(&root).ok();
    tempfile::tempdir_in(&root).expect("tempdir under .local-runs")
}

/// Build an HTTP router via the production `build_router` entry point
/// over a fresh `SQLite` tempfile. Mirrors the pattern used by the
/// `handler_postgres_branches_fake_pg` family of tests for the
/// in-process Axum dispatch.
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
        federation_nonce_cache: std::sync::Arc::new(
            ai_memory::identity::replay::FederationNonceCache::default(),
        ),
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

/// Scan every `forensic-YYYY-MM-DD.jsonl` under `dir` and collect each
/// row whose `kind` field matches `kind`. The forensic chain rotates
/// daily so the directory may contain multiple files even within a
/// single test (UTC midnight crossing).
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

#[tokio::test]
async fn register_agent_emits_forensic_audit_entry() {
    let _g = forensic_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = fresh_dir();
    // Per task brief: `key=None` since signing isn't load-bearing for
    // the emission assertion. The fix path emits the row regardless of
    // whether the sink carries a signing key.
    forensic::shutdown();
    forensic::init(dir.path(), None).expect("init forensic sink");

    let (router, _f) = build_router_fixture();
    let new_agent_id = "ai:new-tenant-001";
    let caller = "operator-alice";
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/agents")
        .header("content-type", "application/json")
        .header("x-agent-id", caller)
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "agent_id": new_agent_id,
                "agent_type": "human",
                "capabilities": ["read", "write"],
            }))
            .unwrap(),
        ))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    assert!(
        status == StatusCode::CREATED || status == StatusCode::OK,
        "register_agent must succeed, got {status}"
    );

    // Shutdown flushes + closes the sink so the file is fully written
    // before we scan.
    forensic::shutdown();
    let rows = collect_kind_rows(dir.path(), "register_agent");
    assert!(
        !rows.is_empty(),
        "#911 regression: register_agent must emit a `kind=register_agent` \
         row to the forensic chain, found 0 rows under {}",
        dir.path().display()
    );
    // Pin the row shape against the contract documented in the fix
    // commit: actor = authenticated caller, decision = allow,
    // payload.new_agent_id = request body's `agent_id`.
    let row = &rows[0];
    assert_eq!(
        row.get("actor").and_then(|v| v.as_str()),
        Some(caller),
        "actor must be the X-Agent-Id caller; got row={row}"
    );
    assert_eq!(
        row.get("decision").and_then(|v| v.as_str()),
        Some("allow"),
        "decision must be `allow`; got row={row}"
    );
    assert_eq!(
        row.get("kind").and_then(|v| v.as_str()),
        Some("register_agent"),
        "kind must be `register_agent`; got row={row}"
    );
    assert_eq!(
        row.get("payload")
            .and_then(|p| p.get("new_agent_id"))
            .and_then(|v| v.as_str()),
        Some(new_agent_id),
        "payload.new_agent_id must match request body; got row={row}"
    );
}

#[tokio::test]
async fn purge_archive_emits_forensic_audit_entry() {
    let _g = forensic_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = fresh_dir();
    // Same `key=None` rationale as the register_agent test.
    forensic::shutdown();
    // Also exercise the signed path here so the emission assertion
    // covers both signed + unsigned sinks. Either is fine for the
    // fix-pin contract; we use a fresh SigningKey to avoid sharing
    // chain heads across tests.
    let key = SigningKey::generate(&mut OsRng);
    forensic::init(dir.path(), Some(key)).expect("init forensic sink");

    let (router, _f) = build_router_fixture();
    // The HTTP route is `DELETE /api/v1/archive` (the handler reads
    // `older_than_days` from the query string). Task brief mentioned
    // `/api/v1/archive/purge` as shorthand; the production route is
    // the one wired in `src/lib.rs:352`.
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/v1/archive?older_than_days=0")
        .header("x-agent-id", "operator-alice")
        .body(Body::empty())
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "archive purge must succeed even on empty archive, got {status}"
    );

    forensic::shutdown();
    let rows = collect_kind_rows(dir.path(), "archive_purge");
    assert!(
        !rows.is_empty(),
        "#911 regression: archive_purge must emit a `kind=archive_purge` \
         row to the forensic chain, found 0 rows under {}",
        dir.path().display()
    );
    let row = &rows[0];
    assert_eq!(
        row.get("actor").and_then(|v| v.as_str()),
        Some("operator-alice"),
        "actor must be the X-Agent-Id caller; got row={row}"
    );
    assert_eq!(
        row.get("decision").and_then(|v| v.as_str()),
        Some("allow"),
        "decision must be `allow`; got row={row}"
    );
    assert_eq!(
        row.get("kind").and_then(|v| v.as_str()),
        Some("archive_purge"),
        "kind must be `archive_purge`; got row={row}"
    );
    assert_eq!(
        row.get("payload")
            .and_then(|p| p.get("older_than_days"))
            .and_then(serde_json::Value::as_i64),
        Some(0),
        "payload.older_than_days must equal the query param (0); got row={row}"
    );
}
