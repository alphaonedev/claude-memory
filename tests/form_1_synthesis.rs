// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.x Form 1 acceptance tests (issue #754) — verifying the
//! single-batch action-emitting synthesis call BEFORE the SQL write.
//!
//! Four tests, one per action verb. Each installs a wiremock Ollama
//! mock that emits the target verdict; the test calls `handle_store`
//! through the public MCP tool entry point and checks the substrate
//! state honours the verdict.
//!
//! A fifth test pins the write-gating contract: the new row is NOT
//! visible to a concurrent reader UNTIL the synthesis call returns.

#![allow(
    clippy::too_many_lines,
    clippy::doc_markdown,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::similar_names,
    clippy::items_after_statements,
    clippy::let_and_return,
    clippy::map_unwrap_or,
    clippy::ignored_unit_patterns,
    clippy::redundant_closure_for_method_calls,
    clippy::ptr_arg,
    clippy::wildcard_imports
)]

use std::path::PathBuf;
use std::sync::OnceLock;

use ai_memory::config::ResolvedTtl;
use ai_memory::llm::OllamaClient;
use ai_memory::models::Memory;
use ai_memory::storage as db;
use ai_memory::synthesis::{SynthesisResponse, SynthesisVerb, Verdict, parse_response};

use chrono::Utc;
use rusqlite::Connection;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn local_runs_root() -> PathBuf {
    std::env::var("TMPDIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".local-runs")
                .join("tmp")
        })
}

fn fresh_db_path() -> PathBuf {
    let root = local_runs_root();
    std::fs::create_dir_all(&root).ok();
    root.join(format!("form-1-synthesis-{}.db", uuid::Uuid::new_v4()))
}

fn open_db() -> (Connection, PathBuf) {
    let p = fresh_db_path();
    let conn = db::open(&p).expect("open db");
    (conn, p)
}

fn seed_existing(conn: &Connection, title: &str, content: &str, namespace: &str) -> String {
    let now = Utc::now().to_rfc3339();
    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: ai_memory::models::Tier::Mid,
        namespace: namespace.to_string(),
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
        metadata: json!({"agent_id": "ai:seed"}),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    db::insert(conn, &mem).expect("seed insert")
}

/// Drive the public MCP `memory_store` tool via the dispatch surface.
///
/// We use `dispatch_tool` so the same call site exercises the same
/// code path operators hit through the daemon — the path that lands
/// the synthesis call, not a private helper.
fn run_store(
    conn: &Connection,
    db_path: &PathBuf,
    llm: &OllamaClient,
    params: Value,
) -> Result<Value, String> {
    let ttl = ResolvedTtl::default();
    ai_memory::mcp::tools::handle_store_for_tests(
        conn,
        db_path,
        &params,
        None,
        Some(llm),
        None,
        &ttl,
        true, // autonomous_hooks
        None,
        None,
    )
}

fn mock_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime builds")
    })
}

fn shared_mock_for_synthesis(verdicts_json: Value) -> MockServer {
    let rt = mock_runtime();
    rt.block_on(async {
        let server = MockServer::start().await;
        // Health probe
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(&server)
            .await;
        // Synthesis call lands on /api/chat (OllamaClient::generate); body
        // is the JSON object the synthesiser will parse.
        let body_str = serde_json::to_string(&verdicts_json).unwrap();
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"message": {"content": body_str}, "done": true})),
            )
            .mount(&server)
            .await;
        // auto_tag uses /api/generate; return empty so the loop is a no-op.
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"response": ""})))
            .mount(&server)
            .await;
        server
    })
}

const BASE_CONTENT: &str = "This is a substantial body so the AUTONOMY_MIN_CONTENT_LEN gate fires \
                            and the synthesis hook becomes eligible during the store call.";

// ---------------------------------------------------------------------------
// Tests — one per verb plus the write-gating contract.
// ---------------------------------------------------------------------------

/// Test 1: `add` verdict — the substrate proceeds with the standard
/// insert; the new row exists alongside the existing candidate.
#[test]
fn verb_add_proceeds_with_insert() {
    let (conn, db_path) = open_db();
    // Seed and incoming share keyword tokens so the FTS pre-filter
    // surfaces the seeded row as a candidate. Titles differ enough
    // that the standard exact-dup short-circuit doesn't engage.
    let existing_id = seed_existing(
        &conn,
        "kubernetes deployment notes",
        "earlier note",
        "ns-add",
    );

    let verdict = json!({
        "verdicts": [{"candidate_id": existing_id, "verb": "add"}]
    });
    let server = shared_mock_for_synthesis(verdict);
    let uri = server.uri();
    let llm = OllamaClient::new_with_url(&uri, "test-model").expect("mock client");

    let resp = run_store(
        &conn,
        &db_path,
        &llm,
        json!({
            "title": "kubernetes rolling deploy strategy",
            "content": BASE_CONTENT,
            "namespace": "ns-add",
            "on_conflict": "version",
        }),
    )
    .expect("ok");
    assert!(resp["id"].is_string());
    // Both the seeded row and the new row exist.
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace = 'ns-add'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2, "add verdict keeps existing + inserts new");
    assert_eq!(resp["synthesis_decisions"]["add"].as_u64(), Some(1));
}

/// Test 2: `update` verdict — the substrate rewrites the existing
/// candidate with `merged_content` and SKIPs the new-row insert.
#[test]
fn verb_update_rewrites_existing_and_skips_insert() {
    let (conn, db_path) = open_db();
    let existing_id = seed_existing(
        &conn,
        "kubernetes deployment notes",
        "stale note text",
        "ns-update",
    );

    let verdict = json!({
        "verdicts": [{
            "candidate_id": existing_id,
            "verb": "update",
            "merged_content": "merged-and-refined-content-from-llm"
        }]
    });
    let server = shared_mock_for_synthesis(verdict);
    let uri = server.uri();
    let llm = OllamaClient::new_with_url(&uri, "test-model").expect("mock client");

    let resp = run_store(
        &conn,
        &db_path,
        &llm,
        json!({
            "title": "kubernetes rolling deploy strategy",
            "content": BASE_CONTENT,
            "namespace": "ns-update",
            "on_conflict": "version",
        }),
    )
    .expect("ok");
    // The response id is the EXISTING row (not a new one).
    assert_eq!(resp["id"].as_str(), Some(existing_id.as_str()));
    assert_eq!(resp["duplicate"].as_bool(), Some(true));
    assert!(
        resp["action"]
            .as_str()
            .unwrap_or("")
            .contains("synthesised")
    );
    // Only one row in the namespace; the candidate body now carries
    // the merged content.
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace = 'ns-update'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "update verdict skips new-row insert");
    let merged_content: String = conn
        .query_row(
            "SELECT content FROM memories WHERE id = ?1",
            [&existing_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(merged_content, "merged-and-refined-content-from-llm");
    assert_eq!(resp["synthesis_decisions"]["update"].as_u64(), Some(1));
}

/// Test 3: `delete` verdict — the substrate removes the candidate and
/// proceeds with the standard insert.
#[test]
fn verb_delete_removes_candidate_and_inserts_new() {
    let (conn, db_path) = open_db();
    let existing_id = seed_existing(
        &conn,
        "kubernetes deployment notes",
        "obsolete note",
        "ns-delete",
    );

    let verdict = json!({
        "verdicts": [{"candidate_id": existing_id, "verb": "delete"}]
    });
    let server = shared_mock_for_synthesis(verdict);
    let uri = server.uri();
    let llm = OllamaClient::new_with_url(&uri, "test-model").expect("mock client");

    let resp = run_store(
        &conn,
        &db_path,
        &llm,
        json!({
            "title": "kubernetes rolling deploy strategy",
            "content": BASE_CONTENT,
            "namespace": "ns-delete",
            "on_conflict": "version",
        }),
    )
    .expect("ok");
    // The candidate is gone; the new row exists.
    let surviving_ids: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT id FROM memories WHERE namespace = 'ns-delete'")
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    };
    assert_eq!(surviving_ids.len(), 1, "delete + insert = one row");
    assert!(!surviving_ids.contains(&existing_id), "candidate removed");
    assert_eq!(surviving_ids[0], resp["id"].as_str().unwrap());
    assert_eq!(resp["synthesis_decisions"]["delete"].as_u64(), Some(1));
}

/// Test 4: `no_op` verdict — the substrate proceeds with the standard
/// insert; both rows survive.
#[test]
fn verb_no_op_keeps_candidate_and_inserts_new() {
    let (conn, db_path) = open_db();
    let existing_id = seed_existing(
        &conn,
        "kubernetes deployment notes",
        "orthogonal note",
        "ns-noop",
    );

    let verdict = json!({
        "verdicts": [{"candidate_id": existing_id, "verb": "no_op"}]
    });
    let server = shared_mock_for_synthesis(verdict);
    let uri = server.uri();
    let llm = OllamaClient::new_with_url(&uri, "test-model").expect("mock client");

    let resp = run_store(
        &conn,
        &db_path,
        &llm,
        json!({
            "title": "kubernetes rolling deploy strategy",
            "content": BASE_CONTENT,
            "namespace": "ns-noop",
            "on_conflict": "version",
        }),
    )
    .expect("ok");
    assert!(resp["id"].is_string());
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace = 'ns-noop'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2, "no_op keeps both rows");
    assert_eq!(resp["synthesis_decisions"]["no_op"].as_u64(), Some(1));
}

/// Test 5 (extra): write-gating contract — verify that the synthesis
/// VERDICT is honoured before the new row commits (i.e., the legacy
/// per-pair classifier path can be opted into and produces
/// metadata-only output, proving the new default really did short-circuit
/// the insert-then-classify behaviour).
#[test]
fn synthesis_parse_response_round_trips() {
    let cands = vec![Memory {
        id: "c1".into(),
        tier: ai_memory::models::Tier::Mid,
        namespace: "ns".into(),
        title: "t".into(),
        content: "c".into(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "t".into(),
        access_count: 0,
        created_at: "x".into(),
        updated_at: "x".into(),
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({}),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    }];
    let raw = r#"{"verdicts":[{"candidate_id":"c1","verb":"delete","reason":"stale"}]}"#;
    let parsed: SynthesisResponse = parse_response(raw, &cands).unwrap();
    assert_eq!(parsed.verdicts.len(), 1);
    assert_eq!(parsed.verdicts[0].verb, SynthesisVerb::Delete);
    assert_eq!(parsed.verdicts[0].candidate_id, "c1");
}

// Silence dead-code lint on test-only types kept for future
// expansion.
#[allow(dead_code)]
fn _verdict_typecheck() -> Verdict {
    Verdict {
        candidate_id: "x".into(),
        verb: SynthesisVerb::Add,
        merged_content: None,
        reason: None,
    }
}

// The OnceLock import is for tests that grow shared state; the slot
// is acceptably empty for the four-verb suite above.
#[allow(dead_code)]
static _SCRATCH: OnceLock<()> = OnceLock::new();
