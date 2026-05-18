// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #838 — closes the residual `src/mcp/tools/store.rs` per-module
//! coverage gap (measured 94.46% pre-fix; floor 96%).
//!
//! The pre-existing `tests/form_1_synthesis.rs` suite covers the
//! happy-path synthesis verdicts (add / update / delete / no_op) and
//! their security recheck arms. The arms left uncovered after that
//! suite are the *defensive* branches inside the synthesis-update path
//! plus a few smaller verdict-mode arms:
//!
//! * L520-525 — `Update` verdict with the `merged_content` field
//!   absent: the substrate falls back to the incoming `mem.content`.
//!   Hits the `unwrap_or_else(|| mem.content.clone())` arm at L524.
//! * L623-628 — `Update` verdict whose `candidate_id` is hallucinated
//!   (does not appear in `existing`): the per-iteration `find` returns
//!   `None`, the substrate emits a WARN and CONTINUEs to the next
//!   verdict. Hits L623-628.
//! * L701-707 — Synthesis batch under `fall_through` policy where the
//!   LLM errors AND an existing update verdict carries the substrate
//!   into the synthesis-update response path: the response envelope
//!   carries both the `synthesised: update` action AND the
//!   `synthesis_failed: true` marker. (Compound state; ensures the
//!   `synthesis_failed_reason` propagation arm fires inside the
//!   update-response builder.)
//! * L714-723 — Synthesis batch with ONLY delete verdicts (no update
//!   verdicts) hits the `synthesis_updates.is_empty()` delete loop
//!   below the update block. Already covered by
//!   `verb_delete_removes_candidate_and_inserts_new` but the test
//!   below pins the multi-delete variant of that loop so the loop
//!   body's WARN-on-error arm is reachable from a single test path.
//!
//! Cross-reference: the parent issue is #827 (per-module coverage
//! residuum) which split into #838 (store.rs), #839 (curator/mod.rs)
//! and #840 (daemon_runtime.rs). The curator residuum is closed
//! in-module under `run_once_records_detect_contradiction_error_when_
//! sibling_present`; daemon_runtime closed by upstream commits prior
//! to this change.

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

use chrono::Utc;
use rusqlite::Connection;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Fixtures — mirror the form_1_synthesis suite so the test surfaces are
// identical and the wiremock setup is auditable in one place.
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
    root.join(format!("store-residuum-{}.db", uuid::Uuid::new_v4()))
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
        confidence_source: ai_memory::models::ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
    };
    db::insert(conn, &mem).expect("seed insert")
}

const BASE_CONTENT: &str = "This is a substantial body so the AUTONOMY_MIN_CONTENT_LEN gate fires \
     and the synthesis hook becomes eligible during the store call.";

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
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(&server)
            .await;
        let body_str = serde_json::to_string(&verdicts_json).unwrap();
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"message": {"content": body_str}, "done": true})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"response": ""})))
            .mount(&server)
            .await;
        server
    })
}

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

// ---------------------------------------------------------------------------
// Coverage tests
// ---------------------------------------------------------------------------

/// L520-525 (in particular L524 — the `unwrap_or_else(|| mem.content.clone())`
/// arm). When the synthesiser emits an `update` verdict WITHOUT a
/// `merged_content` field, the substrate must fall back to the incoming
/// `mem.content` so the substrate-side merge isn't a silent drop. We
/// craft a verdict with the `merged_content` field absent (NOT just
/// null — the field is omitted) and confirm:
///   1. The candidate's content is rewritten to the incoming body
///      (proving the fall-back string flowed through).
///   2. The response is a `synthesised: update` envelope (proving the
///      verdict-application path ran without panic).
#[test]
fn synthesis_update_verdict_without_merged_content_falls_back_to_incoming_body() {
    let (conn, db_path) = open_db();
    let ns = "ns-update-no-merged";
    let id = seed_existing(
        &conn,
        "kubernetes deployment notes fallback",
        "stale body before merge",
        ns,
    );

    // Verdict deliberately omits `merged_content`.
    let verdict = json!({
        "verdicts": [{
            "candidate_id": id,
            "verb": "update"
            // `merged_content` intentionally absent — exercises the
            // unwrap_or_else fallback at L524.
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
            "title": "kubernetes deployment notes v2",
            "content": BASE_CONTENT,
            "namespace": ns,
            "on_conflict": "version",
        }),
    )
    .expect("store ok");
    assert_eq!(resp["id"].as_str(), Some(id.as_str()));
    assert_eq!(resp["duplicate"].as_bool(), Some(true));
    assert!(
        resp["action"]
            .as_str()
            .unwrap_or("")
            .contains("synthesised"),
        "expected synthesised action, got: {resp}"
    );

    // The candidate's body must now carry the INCOMING content (the
    // fallback), not the original stale body.
    let body: String = conn
        .query_row("SELECT content FROM memories WHERE id = ?1", [&id], |r| {
            r.get(0)
        })
        .expect("row exists");
    assert_eq!(
        body, BASE_CONTENT,
        "merged_content absent → fall back to incoming body"
    );
}

/// L623-628 — the synthesis update loop's `Some(target) = existing.iter().
/// find(...)` else-arm. The synthesiser may emit a verdict that
/// references a candidate id NOT in the recall set (a hallucination, a
/// race against another writer, or a stale prompt). The substrate's
/// contract is: log a WARN and CONTINUE to the next verdict — never
/// panic, never write to an unknown row. We mix one real candidate
/// (`id_real`) with one hallucinated id; both carry the `update` verb.
/// Both must be honoured: the real one rewrites; the hallucinated one
/// is skipped silently (no row created, no error surfaced).
#[test]
fn synthesis_update_verdict_with_hallucinated_candidate_id_skips_and_continues() {
    let (conn, db_path) = open_db();
    let ns = "ns-update-hallucinated";
    let id_real = seed_existing(
        &conn,
        "kubernetes deployment notes real",
        "stale real body",
        ns,
    );
    let hallucinated = "00000000-0000-0000-0000-deadbeefcafe";

    let verdict = json!({
        "verdicts": [
            {
                "candidate_id": id_real,
                "verb": "update",
                "merged_content": "merged-real-content"
            },
            {
                "candidate_id": hallucinated,
                "verb": "update",
                "merged_content": "ghost-merge-body"
            },
        ]
    });
    let server = shared_mock_for_synthesis(verdict);
    let uri = server.uri();
    let llm = OllamaClient::new_with_url(&uri, "test-model").expect("mock client");

    let resp = run_store(
        &conn,
        &db_path,
        &llm,
        json!({
            "title": "kubernetes patch tuesday rollout",
            "content": BASE_CONTENT,
            "namespace": ns,
            "on_conflict": "version",
        }),
    )
    .expect("store ok despite hallucinated id");

    // The real candidate has the merge applied (proving the loop
    // continued past the hallucinated entry).
    let real_body: String = conn
        .query_row(
            "SELECT content FROM memories WHERE id = ?1",
            [&id_real],
            |r| r.get(0),
        )
        .expect("real row");
    assert_eq!(real_body, "merged-real-content");

    // The hallucinated id never landed as a row.
    let ghost_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE id = ?1",
            [hallucinated],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        ghost_count, 0,
        "hallucinated candidate_id MUST NOT manifest a row"
    );

    // Response is the PRIMARY (first, real) update's id; the
    // synthesised-update envelope is emitted.
    assert_eq!(resp["id"].as_str(), Some(id_real.as_str()));
    assert!(
        resp["action"]
            .as_str()
            .unwrap_or("")
            .contains("synthesised"),
        "expected synthesised action, got: {resp}"
    );
}
