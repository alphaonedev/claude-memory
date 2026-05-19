// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7-polish #783 — COV-15: synthesis verdict diff matrix.
//!
//! The four baseline tests in `form_1_synthesis.rs` cover one verb
//! against a single seeded candidate. This matrix extends coverage
//! combinatorially: 4 verb classes × 5 candidate counts (1, 2, 3, 4, 5)
//! = 20 test cases, each asserting the substrate honours the verdict
//! exactly for every candidate in the batch.
//!
//! ## Candidate-count selection (1, 2, 3, 4, 5)
//!
//! The brief proposed 1, 2, 3, 5, 10. The pre-filter the substrate
//! actually uses — `storage::find_contradictions` — is hard-capped at
//! `LIMIT 5` (see src/storage/mod.rs:1995). Verdicts that reference
//! candidates beyond that pre-filter window are silently dropped by
//! design because the synthesiser never sees them. Picking n=10 would
//! test the pre-filter's cap, not the verdict-honouring contract, so
//! the matrix walks the actually-exercisable 1..=5 range instead. The
//! cap itself is a documented substrate property and not a bug; this
//! comment exists so a future widening of the LIMIT can grow the
//! matrix in lockstep.
//!
//! Tests against the already-shipped substrate; no source changes.

#![allow(
    clippy::too_many_lines,
    clippy::doc_markdown,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::similar_names,
    clippy::items_after_statements,
    clippy::let_and_return,
    clippy::wildcard_imports,
    clippy::needless_range_loop,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::map_unwrap_or,
    clippy::ptr_arg
)]

use std::path::PathBuf;

use ai_memory::config::ResolvedTtl;
use ai_memory::llm::OllamaClient;
use ai_memory::models::Memory;
use ai_memory::storage as db;

use chrono::Utc;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::sync::OnceLock;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Fixtures (mirrors form_1_synthesis.rs)
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
    root.join(format!("form-1-matrix-{}.db", uuid::Uuid::new_v4()))
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
        version: 1,
    };
    db::insert(conn, &mem).expect("seed insert")
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
        true,
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

const BASE_CONTENT: &str = "This is a substantial body so the AUTONOMY_MIN_CONTENT_LEN gate fires \
                            and the synthesis hook becomes eligible during the store call.";

/// Install a permissive synthesis-policy standard so the per-call delete
/// cap (default 1) does not block batches with N > 1 deletes. The other
/// substrate gates are left at their defaults.
fn install_permissive_synthesis_policy(conn: &Connection, ns: &str) {
    use ai_memory::models::{
        ApproverType, CorePolicy, GovernanceLevel, GovernancePolicy, Memory, MemoryKind,
        SynthesisPolicy, Tier, default_metadata,
    };
    let policy = GovernancePolicy {
        core: CorePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Any,
            approver: ApproverType::Human,
            inherit: true,
            max_reflection_depth: None,
        },
        synthesis: SynthesisPolicy {
            legacy_per_pair_classifier: None,
            synthesis_failure_mode: None,
            synthesis_max_deletes_per_call: Some(64),
            synthesis_max_candidate_chars: None,
        },
        ..Default::default()
    };
    let now = Utc::now().to_rfc3339();
    let mut metadata = default_metadata();
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert("agent_id".to_string(), json!("ai:test"));
        obj.insert(
            "governance".to_string(),
            serde_json::to_value(&policy).unwrap(),
        );
    }
    let standard = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: format!("_standards-{ns}"),
        title: format!("cov-15-std-{ns}"),
        content: "policy".to_string(),
        tags: vec![],
        priority: 9,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata,
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ai_memory::models::ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    };
    let sid = db::insert(conn, &standard).expect("insert std");
    db::set_namespace_standard(conn, ns, &sid, None).expect("set std");
}

/// Seed `n` candidates sharing a common token so the FTS pre-filter
/// surfaces all of them as synthesis candidates.
fn seed_n_candidates(conn: &Connection, n: usize, ns: &str) -> Vec<String> {
    (0..n)
        .map(|i| {
            seed_existing(
                conn,
                &format!("kubernetes deployment notes v{i}"),
                &format!("body for candidate {i}"),
                ns,
            )
        })
        .collect()
}

fn ns_count(conn: &Connection, ns: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE namespace = ?1",
        [ns],
        |r| r.get(0),
    )
    .unwrap()
}

fn row_exists(conn: &Connection, id: &str) -> bool {
    conn.query_row("SELECT 1 FROM memories WHERE id = ?1", [id], |_| Ok(true))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Matrix driver — one verb per fan-out.
// ---------------------------------------------------------------------------

/// Drive `add` against `n` candidates: substrate keeps all N candidates
/// and inserts the new row → total = N + 1.
fn matrix_add(n: usize) {
    let (conn, db_path) = open_db();
    let ns = format!("ns-matrix-add-{n}");
    install_permissive_synthesis_policy(&conn, &ns);
    let ids = seed_n_candidates(&conn, n, &ns);

    let verdicts: Vec<Value> = ids
        .iter()
        .map(|id| json!({"candidate_id": id, "verb": "add"}))
        .collect();
    let server = shared_mock_for_synthesis(json!({"verdicts": verdicts}));
    let llm = OllamaClient::new_with_url(&server.uri(), "test-model").expect("client");

    let resp = run_store(
        &conn,
        &db_path,
        &llm,
        json!({
            "title": "kubernetes rolling deploy strategy",
            "content": BASE_CONTENT,
            "namespace": ns,
            "on_conflict": "version",
        }),
    )
    .expect("store ok");

    assert!(resp["id"].is_string(), "add path returns id");
    let want = i64::try_from(n).unwrap() + 1;
    assert_eq!(
        ns_count(&conn, &ns),
        want,
        "add matrix n={n}: all N candidates kept + new row inserted",
    );
    for id in &ids {
        assert!(row_exists(&conn, id), "add: candidate {id} must survive");
    }
    assert_eq!(
        resp["synthesis_decisions"]["add"].as_u64(),
        Some(n as u64),
        "add: every verdict counted",
    );
}

/// Drive `update` against `n` candidates: substrate rewrites all N
/// candidates' content; SKIPs the new-row insert. Total = N.
fn matrix_update(n: usize) {
    let (conn, db_path) = open_db();
    let ns = format!("ns-matrix-update-{n}");
    install_permissive_synthesis_policy(&conn, &ns);
    let ids = seed_n_candidates(&conn, n, &ns);

    let verdicts: Vec<Value> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            json!({
                "candidate_id": id,
                "verb": "update",
                "merged_content": format!("merged-content-{i}"),
            })
        })
        .collect();
    let server = shared_mock_for_synthesis(json!({"verdicts": verdicts}));
    let llm = OllamaClient::new_with_url(&server.uri(), "test-model").expect("client");

    let resp = run_store(
        &conn,
        &db_path,
        &llm,
        json!({
            "title": "kubernetes rolling deploy strategy",
            "content": BASE_CONTENT,
            "namespace": ns,
            "on_conflict": "version",
        }),
    )
    .expect("store ok");

    // The response surfaces the PRIMARY (first) update's id.
    assert_eq!(
        resp["id"].as_str(),
        Some(ids[0].as_str()),
        "update: primary id is the first updated candidate",
    );
    assert_eq!(
        resp["duplicate"].as_bool(),
        Some(true),
        "update: dedup flag"
    );
    assert_eq!(
        ns_count(&conn, &ns),
        i64::try_from(n).unwrap(),
        "update matrix n={n}: SKIPs new-row insert; total stays at N",
    );
    // Every candidate's body must reflect its merged_content.
    for (i, id) in ids.iter().enumerate() {
        let body: String = conn
            .query_row("SELECT content FROM memories WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            body,
            format!("merged-content-{i}"),
            "update: candidate #{i} body must reflect merged_content",
        );
    }
    assert_eq!(
        resp["synthesis_decisions"]["update"].as_u64(),
        Some(n as u64),
    );
}

/// Drive `delete` against `n` candidates: substrate removes all N
/// candidates and inserts the new row. Total = 1.
fn matrix_delete(n: usize) {
    let (conn, db_path) = open_db();
    let ns = format!("ns-matrix-delete-{n}");
    install_permissive_synthesis_policy(&conn, &ns);
    let ids = seed_n_candidates(&conn, n, &ns);

    let verdicts: Vec<Value> = ids
        .iter()
        .map(|id| json!({"candidate_id": id, "verb": "delete"}))
        .collect();
    let server = shared_mock_for_synthesis(json!({"verdicts": verdicts}));
    let llm = OllamaClient::new_with_url(&server.uri(), "test-model").expect("client");

    let resp = run_store(
        &conn,
        &db_path,
        &llm,
        json!({
            "title": "kubernetes rolling deploy strategy",
            "content": BASE_CONTENT,
            "namespace": ns,
            "on_conflict": "version",
        }),
    )
    .expect("store ok");

    assert_eq!(
        ns_count(&conn, &ns),
        1,
        "delete matrix n={n}: all N candidates removed + 1 new row",
    );
    for id in &ids {
        assert!(
            !row_exists(&conn, id),
            "delete: candidate {id} must be gone",
        );
    }
    let new_id = resp["id"].as_str().expect("new id");
    assert!(row_exists(&conn, new_id), "delete: new row exists");
    assert_eq!(
        resp["synthesis_decisions"]["delete"].as_u64(),
        Some(n as u64),
    );
}

/// Drive `no_op` against `n` candidates: substrate keeps all N candidates
/// AND inserts the new row. Total = N + 1.
fn matrix_no_op(n: usize) {
    let (conn, db_path) = open_db();
    let ns = format!("ns-matrix-noop-{n}");
    install_permissive_synthesis_policy(&conn, &ns);
    let ids = seed_n_candidates(&conn, n, &ns);

    let verdicts: Vec<Value> = ids
        .iter()
        .map(|id| json!({"candidate_id": id, "verb": "no_op"}))
        .collect();
    let server = shared_mock_for_synthesis(json!({"verdicts": verdicts}));
    let llm = OllamaClient::new_with_url(&server.uri(), "test-model").expect("client");

    let resp = run_store(
        &conn,
        &db_path,
        &llm,
        json!({
            "title": "kubernetes rolling deploy strategy",
            "content": BASE_CONTENT,
            "namespace": ns,
            "on_conflict": "version",
        }),
    )
    .expect("store ok");

    assert!(resp["id"].is_string());
    let want = i64::try_from(n).unwrap() + 1;
    assert_eq!(
        ns_count(&conn, &ns),
        want,
        "no_op matrix n={n}: candidates kept + new row",
    );
    for id in &ids {
        assert!(row_exists(&conn, id), "no_op: candidate {id} kept");
    }
    assert_eq!(
        resp["synthesis_decisions"]["no_op"].as_u64(),
        Some(n as u64),
    );
}

// ---------------------------------------------------------------------------
// 4 verbs × 5 candidate counts (1, 2, 3, 4, 5) = 20 #[test] functions.
//
// One `#[test]` per cell so a failure pinpoints the exact (verb, n)
// pair. (`rstest` is not in the workspace's dependency set, so we
// fan out the matrix manually rather than pull in a new dep.)
// ---------------------------------------------------------------------------

#[test]
fn cov15_add_n1() {
    matrix_add(1);
}
#[test]
fn cov15_add_n2() {
    matrix_add(2);
}
#[test]
fn cov15_add_n3() {
    matrix_add(3);
}
#[test]
fn cov15_add_n4() {
    matrix_add(4);
}
#[test]
fn cov15_add_n5() {
    matrix_add(5);
}

#[test]
fn cov15_update_n1() {
    matrix_update(1);
}
#[test]
fn cov15_update_n2() {
    matrix_update(2);
}
#[test]
fn cov15_update_n3() {
    matrix_update(3);
}
#[test]
fn cov15_update_n4() {
    matrix_update(4);
}
#[test]
fn cov15_update_n5() {
    matrix_update(5);
}

#[test]
fn cov15_delete_n1() {
    matrix_delete(1);
}
#[test]
fn cov15_delete_n2() {
    matrix_delete(2);
}
#[test]
fn cov15_delete_n3() {
    matrix_delete(3);
}
#[test]
fn cov15_delete_n4() {
    matrix_delete(4);
}
#[test]
fn cov15_delete_n5() {
    matrix_delete(5);
}

#[test]
fn cov15_no_op_n1() {
    matrix_no_op(1);
}
#[test]
fn cov15_no_op_n2() {
    matrix_no_op(2);
}
#[test]
fn cov15_no_op_n3() {
    matrix_no_op(3);
}
#[test]
fn cov15_no_op_n4() {
    matrix_no_op(4);
}
#[test]
fn cov15_no_op_n5() {
    matrix_no_op(5);
}
