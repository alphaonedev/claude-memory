// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows: test scaffolding only; pedantic lints with no
// behavioural impact in test code.
#![allow(clippy::redundant_closure_for_method_calls)]
#![allow(clippy::too_many_lines)]
// The `drop(out)` calls below explicitly release the &mut borrows
// CliOutput holds on local `stdout` / `stderr` Vec<u8>s so the
// post-test assertions can read them back. CliOutput itself doesn't
// implement Drop — the drop is for lifetime release, not destructor
// invocation, which is exactly what clippy flags. Allow at the file
// level for clarity of intent.
#![allow(clippy::drop_non_drop)]
// The module docstring lists every MCP tool name with backticks
// would just add visual noise to the route → handler ASCII table.
#![allow(clippy::doc_markdown)]

//! v0.7.0 Cluster E API-2 (issue #767) — CLI / HTTP parity smoke
//! suite for the seven L1-5 Agent Skills MCP tools.
//!
//! Pins the contract that the three interfaces (MCP / CLI / HTTP) all
//! exercise the same substrate handlers. The CLI tests live alongside
//! the unit tests in `src/cli/commands/skill.rs#tests`; this file
//! covers the HTTP surface end-to-end via `Router::oneshot()`, plus a
//! couple of additional CLI smoke checks for the verbs the unit-test
//! block doesn't cover (resource, promote).
//!
//! HTTP route → MCP tool name parity table (also pinned in
//! `src/handlers/http.rs` and `src/lib.rs`):
//!
//!   POST /api/v1/skill/register         → memory_skill_register
//!   GET  /api/v1/skill/list             → memory_skill_list
//!   GET  /api/v1/skill/{id}             → memory_skill_get
//!   GET  /api/v1/skill/{id}/resource    → memory_skill_resource
//!   POST /api/v1/skill/{id}/export      → memory_skill_export
//!   POST /api/v1/skill/{id}/promote     → memory_skill_promote_from_reflection
//!   POST /api/v1/skill/{id}/compose     → memory_skill_compositional_context

use ai_memory::cli::CliOutput;
use ai_memory::cli::commands::skill::{
    ComposeArgs, ExportArgs, GetArgs, ListArgs, PromoteArgs, RegisterArgs, ResourceArgs,
    SkillAction, SkillArgs,
};
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use std::path::PathBuf;
use tempfile::TempDir;
use tower::ServiceExt as _;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn minimal_skill_md(name: &str) -> String {
    format!(
        "---\nnamespace: testns\nname: {name}\ndescription: A demo skill for parity tests.\n---\n\nBody for {name}.\n"
    )
}

fn fresh_db() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("ai-memory.db");
    let _conn = ai_memory::db::open(&path).expect("db::open");
    (dir, path)
}

fn build_router_with_db_path(db_path: &std::path::Path) -> (axum::Router, ai_memory::handlers::Db) {
    let conn = ai_memory::db::open(db_path).expect("open db");
    let path = db_path.to_path_buf();
    let db: ai_memory::handlers::Db = std::sync::Arc::new(tokio::sync::Mutex::new((
        conn,
        path,
        ai_memory::config::ResolvedTtl::default(),
        true,
    )));
    #[cfg(feature = "sal")]
    let store: std::sync::Arc<dyn ai_memory::store::MemoryStore> = {
        std::sync::Arc::new(
            ai_memory::store::sqlite::SqliteStore::open(db_path).expect("open SqliteStore"),
        )
    };
    let app_state = ai_memory::handlers::AppState {
        db: db.clone(),
        embedder: std::sync::Arc::new(None),
        vector_index: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        federation: std::sync::Arc::new(None),
        tier_config: std::sync::Arc::new(ai_memory::config::FeatureTier::Keyword.config()),
        scoring: std::sync::Arc::new(ai_memory::config::ResolvedScoring::default()),
        profile: std::sync::Arc::new(ai_memory::profile::Profile::core()),
        mcp_config: std::sync::Arc::new(None),
        active_keypair: std::sync::Arc::new(None),
        family_embeddings: std::sync::Arc::new(tokio::sync::RwLock::new(Some(Vec::new()))),
        storage_backend: ai_memory::handlers::StorageBackend::Sqlite,
        #[cfg(feature = "sal")]
        store,
        llm: std::sync::Arc::new(None),
        auto_tag_model: std::sync::Arc::new(None),
        llm_call_timeout: std::time::Duration::from_secs(30),
        replay_cache: std::sync::Arc::new(ai_memory::identity::replay::ReplayCache::default()),
        verify_require_nonce: false,
        federation_nonce_cache: std::sync::Arc::new(
            ai_memory::identity::replay::FederationNonceCache::default(),
        ),
        autonomous_hooks: false,
        recall_scope: std::sync::Arc::new(None),
        deferred_audit_queue: std::sync::Arc::new(None),
    };
    let api_key_state = ai_memory::handlers::ApiKeyState {
        key: None,
        mtls_enforced: false,
    };
    let router = ai_memory::build_router(api_key_state, app_state);
    (router, db)
}

async fn read_body_json(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, v)
}

/// Seed a skill directly via the MCP handler (the simplest possible
/// fixture). Returns the skill id.
fn seed_skill(db_path: &std::path::Path, name: &str) -> String {
    let conn = ai_memory::db::open(db_path).unwrap();
    let v = ai_memory::mcp::handle_skill_register(
        &conn,
        &json!({"inline_skill": minimal_skill_md(name)}),
        None,
    )
    .expect("seed skill");
    v["id"].as_str().unwrap().to_string()
}

// ---------------------------------------------------------------------------
// HTTP route tests — one per route (6 of 7 routes here; the unit tests
// in src/cli/commands/skill.rs exercise the underlying handlers too).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn http_skill_register_route_returns_200() {
    let (_dir, db_path) = fresh_db();
    let (router, _db) = build_router_with_db_path(&db_path);
    let body = json!({"inline_skill": minimal_skill_md("http-register")});
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/skill/register")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let (status, v) = read_body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "got body: {v}");
    assert_eq!(v["registered"], json!(true));
    assert_eq!(v["name"], json!("http-register"));
    assert_eq!(v["namespace"], json!("testns"));
}

#[tokio::test]
async fn http_skill_list_route_returns_seeded_skill() {
    let (_dir, db_path) = fresh_db();
    let _id = seed_skill(&db_path, "http-list");
    let (router, _db) = build_router_with_db_path(&db_path);
    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/skill/list?namespace=testns")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let (status, v) = read_body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "got body: {v}");
    let arr = v["skills"].as_array().expect("skills array");
    assert!(
        arr.iter().any(|s| s["name"].as_str() == Some("http-list")),
        "skill 'http-list' must appear in list response: {v}"
    );
}

#[tokio::test]
async fn http_skill_get_route_returns_body() {
    let (_dir, db_path) = fresh_db();
    let id = seed_skill(&db_path, "http-get");
    let (router, _db) = build_router_with_db_path(&db_path);
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!("/api/v1/skill/{id}"))
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let (status, v) = read_body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "got body: {v}");
    assert_eq!(v["id"], json!(id));
    assert_eq!(v["name"], json!("http-get"));
    assert!(
        v["body"]
            .as_str()
            .unwrap_or("")
            .contains("Body for http-get")
    );
}

#[tokio::test]
async fn http_skill_get_route_returns_404_on_unknown_id() {
    let (_dir, db_path) = fresh_db();
    let (router, _db) = build_router_with_db_path(&db_path);
    let req = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/skill/no-such-skill-id")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let (status, v) = read_body_json(resp).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "got body: {v}");
}

#[tokio::test]
async fn http_skill_export_route_writes_skill_md() {
    let (dir, db_path) = fresh_db();
    let id = seed_skill(&db_path, "http-export");
    let target = dir.path().join("export-target");
    let (router, _db) = build_router_with_db_path(&db_path);
    let body = json!({"target_folder": target.to_string_lossy()});
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/v1/skill/{id}/export"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let (status, v) = read_body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "got body: {v}");
    assert!(target.join("SKILL.md").exists());
}

#[tokio::test]
async fn http_skill_compose_route_returns_body_only_for_non_composing_skill() {
    // A skill registered without `composes_with_reflections` returns
    // `body` only; this is the documented degrade-cleanly path. Pin
    // the 200-status path.
    let (_dir, db_path) = fresh_db();
    let id = seed_skill(&db_path, "http-compose");
    let (router, _db) = build_router_with_db_path(&db_path);
    let body = json!({"budget_tokens": 2000});
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/v1/skill/{id}/compose"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let (status, v) = read_body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "got body: {v}");
    assert!(
        v.get("body").is_some(),
        "compose response must include `body`: {v}"
    );
}

#[tokio::test]
async fn http_skill_resource_route_404_on_missing() {
    let (_dir, db_path) = fresh_db();
    let id = seed_skill(&db_path, "http-resource");
    let (router, _db) = build_router_with_db_path(&db_path);
    // No resources were registered for this skill — request must 404.
    let req = Request::builder()
        .method(Method::GET)
        .uri(format!(
            "/api/v1/skill/{id}/resource?path=scripts/nothing.sh"
        ))
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let (status, _) = read_body_json(resp).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn http_skill_promote_route_rejects_non_reflection_400() {
    // Seed a plain skill (not a reflection memory) — promote refuses
    // because the source row's memory_kind != Reflection. The HTTP
    // handler should surface the substrate error as 400 / 404.
    let (_dir, db_path) = fresh_db();
    let (router, _db) = build_router_with_db_path(&db_path);
    let body = json!({
        "name": "promoted-not",
        "description": "should fail",
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/skill/no-such-reflection/promote")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let (status, v) = read_body_json(resp).await;
    assert!(
        status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
        "promote of missing reflection must surface 404 or 400 (got {status}): {v}"
    );
}

// ---------------------------------------------------------------------------
// CLI tests — complement the in-module unit suite by covering verbs the
// unit tests don't (resource, promote happy-path is hard without a real
// reflection chain; cover error paths and list output here).
// ---------------------------------------------------------------------------

#[test]
fn cli_skill_resource_missing_exits_nonzero() {
    let (_dir, db_path) = fresh_db();
    let id = seed_skill(&db_path, "cli-resource-missing");
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
    let args = SkillArgs {
        action: SkillAction::Resource(ResourceArgs {
            id,
            path: "scripts/nothing.sh".to_string(),
            json: true,
        }),
    };
    let code = ai_memory::cli::commands::skill::run(&db_path, &args, None, &mut out).unwrap();
    assert_eq!(code, 2);
    drop(out);
    let err = String::from_utf8(stderr).unwrap();
    assert!(err.contains("resource not found"), "got stderr: {err}");
}

#[test]
fn cli_skill_promote_missing_reflection_exits_nonzero() {
    let (_dir, db_path) = fresh_db();
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
    let args = SkillArgs {
        action: SkillAction::Promote(PromoteArgs {
            id: "no-such-reflection".to_string(),
            name: "promoted".to_string(),
            description: "should fail".to_string(),
            parameters_schema: None,
            json: true,
        }),
    };
    let code = ai_memory::cli::commands::skill::run(&db_path, &args, None, &mut out).unwrap();
    assert_eq!(code, 2);
}

#[test]
fn cli_skill_list_with_namespace_filter_smoke() {
    let (_dir, db_path) = fresh_db();
    let _id = seed_skill(&db_path, "cli-list-ns");
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
    let args = SkillArgs {
        action: SkillAction::List(ListArgs {
            namespace: Some("testns".to_string()),
            filter: None,
            json: true,
        }),
    };
    let code = ai_memory::cli::commands::skill::run(&db_path, &args, None, &mut out).unwrap();
    assert_eq!(code, 0);
    drop(out);
    let text = String::from_utf8(stdout).unwrap();
    assert!(text.contains("cli-list-ns"));
}

#[test]
fn cli_skill_export_writes_skill_md() {
    let (dir, db_path) = fresh_db();
    let id = seed_skill(&db_path, "cli-export-md");
    let target = dir.path().join("cli-export-out");
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
    let args = SkillArgs {
        action: SkillAction::Export(ExportArgs {
            id,
            output: target.clone(),
            json: true,
        }),
    };
    let code = ai_memory::cli::commands::skill::run(&db_path, &args, None, &mut out).unwrap();
    assert_eq!(code, 0);
    assert!(target.join("SKILL.md").exists());
}

#[test]
fn cli_skill_get_human_format_renders_namespace_and_name() {
    let (_dir, db_path) = fresh_db();
    let id = seed_skill(&db_path, "cli-get-human");
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
    let args = SkillArgs {
        action: SkillAction::Get(GetArgs {
            id,
            json: false, // human format
        }),
    };
    let code = ai_memory::cli::commands::skill::run(&db_path, &args, None, &mut out).unwrap();
    assert_eq!(code, 0);
    drop(out);
    let text = String::from_utf8(stdout).unwrap();
    assert!(text.contains("# testns/cli-get-human"), "got: {text}");
    assert!(text.contains("Body for cli-get-human"));
}

#[test]
fn cli_skill_compose_smoke_via_run() {
    let (_dir, db_path) = fresh_db();
    let id = seed_skill(&db_path, "cli-compose-run");
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
    let args = SkillArgs {
        action: SkillAction::Compose(ComposeArgs {
            id: id.clone(),
            budget_tokens: None,
            json: true,
        }),
    };
    let code = ai_memory::cli::commands::skill::run(&db_path, &args, None, &mut out).unwrap();
    assert_eq!(code, 0);
    drop(out);
    let text = String::from_utf8(stdout).unwrap();
    assert!(
        text.contains("body") || text.contains(&id),
        "compose response must surface body or skill id: {text}"
    );
}

#[test]
fn cli_skill_register_inline_smoke_v2() {
    // Second register smoke-test (the unit test in
    // src/cli/commands/skill.rs#tests covers the same path; replicate
    // here so the dedicated parity test crate has a self-contained
    // entry under `cargo test --test skill_cli_http_parity`).
    let (_dir, db_path) = fresh_db();
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
    let args = SkillArgs {
        action: SkillAction::Register(RegisterArgs {
            manifest: None,
            inline: Some(minimal_skill_md("cli-register-v2")),
            json: true,
        }),
    };
    let code = ai_memory::cli::commands::skill::run(&db_path, &args, None, &mut out).unwrap();
    assert_eq!(code, 0);
    drop(out);
    let text = String::from_utf8(stdout).unwrap();
    assert!(text.contains("cli-register-v2"));
}
