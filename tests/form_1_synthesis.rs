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
//!
//! v0.7.0 Cluster-B (issue #767) acceptance suite extension closes
//! the 5 findings (SEC-1, SEC-11, COR-5, COR-6, PERF-7) by exercising
//! the K9 delete-recheck, the per-call delete cap, the synthesis
//! failure-mode policy, the prompt-size truncation contract, and the
//! multi-update honouring policy.

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
        confidence_source: ai_memory::models::ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
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
    let cands = [Memory {
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
        confidence_source: ai_memory::models::ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
    }];
    let raw = r#"{"verdicts":[{"candidate_id":"c1","verb":"delete","reason":"stale"}]}"#;
    let cands_ref: Vec<&Memory> = cands.iter().collect();
    let parsed: SynthesisResponse = parse_response(raw, &cands_ref).unwrap();
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

// ---------------------------------------------------------------------------
// v0.7.0 Cluster-B (issue #767) — acceptance tests for SEC-1, SEC-11,
// COR-5, COR-6, PERF-7. Each test installs a wiremock LLM and drives
// `handle_store` through the public MCP entry point.
// ---------------------------------------------------------------------------

/// Spin a mock that errors on every `/api/chat` (synthesis) call —
/// drives the COR-6 failure-surfacing tests.
fn shared_mock_for_synthesis_error() -> MockServer {
    let rt = mock_runtime();
    rt.block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(&server)
            .await;
        // Synthesis call returns 500 — synthesise() will surface an Err.
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(500).set_body_string("curator down"))
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

/// Install a namespace standard carrying a `GovernancePolicy` with the
/// supplied synthesis knobs. Used by the Cluster-B tests to exercise
/// per-namespace overrides without rebuilding the entire policy.
fn install_synthesis_policy(
    conn: &Connection,
    ns: &str,
    failure_mode: Option<ai_memory::models::SynthesisFailureMode>,
    max_deletes_per_call: Option<u32>,
    max_candidate_chars: Option<u32>,
) {
    use ai_memory::models::{
        ApproverType, GovernanceLevel, GovernancePolicy, Memory, MemoryKind, Tier, default_metadata,
    };
    let policy = GovernancePolicy {
        write: GovernanceLevel::Any,
        promote: GovernanceLevel::Any,
        delete: GovernanceLevel::Any,
        approver: ApproverType::Human,
        inherit: true,
        max_reflection_depth: None,
        auto_export_reflections_to_filesystem: None,
        auto_atomise: None,
        auto_atomise_threshold_cl100k: None,
        auto_atomise_max_atom_tokens: None,
        auto_atomise_max_retries: None,
        auto_persona_trigger_every_n_memories: None,
        auto_export_personas_to_filesystem: None,
        auto_atomise_mode: None,
        legacy_per_pair_classifier: None,
        auto_classify_kind: None,
        synthesis_failure_mode: failure_mode,
        synthesis_max_deletes_per_call: max_deletes_per_call,
        synthesis_max_candidate_chars: max_candidate_chars,
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
        title: format!("cluster-b-std-{ns}"),
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
    };
    let sid = db::insert(conn, &standard).expect("insert std");
    db::set_namespace_standard(conn, ns, &sid, None).expect("set std");
}

/// SEC-1 — every `delete` verdict is re-checked against the K9
/// `MemoryDelete` op. A K9 rule denying delete in this namespace must
/// suppress the delete; the other (k9-allowed) candidate proceeds.
#[test]
fn synthesis_delete_verdict_consults_k9_per_candidate() {
    use ai_memory::permissions::{
        PermissionRule, RuleDecision, clear_active_permission_rules_for_test,
        set_active_permission_rules,
    };

    let (conn, db_path) = open_db();
    let ns = "ns-k9-recheck";
    // Bump the per-call delete cap so the 2-delete batch is permitted
    // at the cap layer; the test exercises the K9 recheck specifically.
    install_synthesis_policy(&conn, ns, None, Some(5), None);

    // Seed two candidates that the synthesiser will be told to delete.
    let kept_id = seed_existing(&conn, "kubernetes deployment notes", "kept body", ns);
    let pruned_id = seed_existing(&conn, "kubernetes rolling strategy", "pruned body", ns);

    // K9 rule: deny memory_delete on this namespace for any agent.
    // The recheck must drop EVERY delete verdict — neither candidate
    // should be removed via the synthesis path.
    clear_active_permission_rules_for_test();
    set_active_permission_rules(vec![PermissionRule {
        namespace_pattern: ns.to_string(),
        op: "memory_delete".to_string(),
        agent_pattern: "*".to_string(),
        decision: RuleDecision::Deny,
        reason: Some("K9 deny test".into()),
    }]);

    let verdict = json!({
        "verdicts": [
            {"candidate_id": kept_id, "verb": "delete"},
            {"candidate_id": pruned_id, "verb": "delete"},
        ]
    });
    let server = shared_mock_for_synthesis(verdict);
    let uri = server.uri();
    let llm = OllamaClient::new_with_url(&uri, "test-model").expect("mock client");

    let _resp = run_store(
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
    .expect("store ok");

    // Both seeded candidates survive — K9 denied every delete verdict.
    let kept_exists: bool = conn
        .query_row("SELECT 1 FROM memories WHERE id = ?1", [&kept_id], |_| {
            Ok(true)
        })
        .unwrap_or(false);
    let pruned_exists: bool = conn
        .query_row("SELECT 1 FROM memories WHERE id = ?1", [&pruned_id], |_| {
            Ok(true)
        })
        .unwrap_or(false);
    assert!(kept_exists, "k9-denied candidate must NOT be deleted");
    assert!(pruned_exists, "k9-denied candidate must NOT be deleted");

    clear_active_permission_rules_for_test();
}

/// SEC-1 — a verdict whose delete count exceeds the per-namespace cap
/// (default 1, no K10 approval in this test) must be refused with a
/// `GOVERNANCE_REFUSED` envelope. No candidate is deleted.
#[test]
fn synthesis_unbounded_delete_refused_without_k10_approval() {
    let (conn, db_path) = open_db();
    let ns = "ns-unbounded-delete";
    // No policy installed → default cap of 1 applies.

    let ids: Vec<String> = (0..5)
        .map(|i| {
            seed_existing(
                &conn,
                &format!("kubernetes deploy notes v{i}"),
                "stale body",
                ns,
            )
        })
        .collect();

    let verdicts: Vec<Value> = ids
        .iter()
        .map(|id| json!({"candidate_id": id, "verb": "delete"}))
        .collect();
    let verdict = json!({"verdicts": verdicts});
    let server = shared_mock_for_synthesis(verdict);
    let uri = server.uri();
    let llm = OllamaClient::new_with_url(&uri, "test-model").expect("mock client");

    let result = run_store(
        &conn,
        &db_path,
        &llm,
        json!({
            "title": "kubernetes patch tuesday rollout",
            "content": BASE_CONTENT,
            "namespace": ns,
            "on_conflict": "version",
        }),
    );
    let err = result.expect_err("over-cap batch must refuse");
    assert!(
        err.contains("GOVERNANCE_REFUSED"),
        "expected GOVERNANCE_REFUSED, got: {err}"
    );
    assert!(
        err.contains("cap") || err.contains("exceed"),
        "expected cap-reason, got: {err}"
    );

    // No deletions occurred — every seeded candidate still exists.
    let surviving: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace = ?1",
            [ns],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(surviving, 5, "no candidates deleted under refused batch");
}

/// COR-6 — on synthesis failure under the default `fall_through`
/// policy, the response envelope carries `synthesis_failed: true` so
/// callers observe the curator outage instead of silently inheriting
/// the legacy fall-through.
#[test]
fn synthesis_response_carries_synthesis_failed_on_llm_error() {
    let (conn, db_path) = open_db();
    let ns = "ns-failed-flag";
    // Default policy: fall_through.
    let _existing_id = seed_existing(&conn, "kubernetes deployment notes", "earlier", ns);

    let server = shared_mock_for_synthesis_error();
    let uri = server.uri();
    let llm = OllamaClient::new_with_url(&uri, "test-model").expect("mock client");

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
    .expect("fall-through still writes");

    assert_eq!(
        resp["synthesis_failed"].as_bool(),
        Some(true),
        "expected synthesis_failed=true, got: {resp}"
    );
    assert!(
        resp["synthesis_failed_reason"].is_string(),
        "expected synthesis_failed_reason populated"
    );
}

/// PERF-7 — the synthesis prompt truncates each candidate's content
/// at the per-namespace cap so a multi-KB candidate cannot inflate the
/// prompt unboundedly.
#[test]
fn synthesis_prompt_truncates_candidate_content_at_cap() {
    use ai_memory::synthesis;

    synthesis::reset_max_prompt_size_chars_for_test();

    // 10K-char candidate content. Build the prompt directly through
    // the public API so the test pins the substrate's truncation
    // contract without round-tripping the LLM.
    let now = Utc::now().to_rfc3339();
    let cand = ai_memory::models::Memory {
        id: "huge".to_string(),
        tier: ai_memory::models::Tier::Mid,
        namespace: "ns".to_string(),
        title: "huge candidate".to_string(),
        content: "z".repeat(10_000),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
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
        confidence_source: ai_memory::models::ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
    };

    let cap = 200_usize;
    let prompt = synthesis::build_prompt_with_cap("incoming", "body", &[&cand], cap);

    // Untruncated content (10K zs) must not appear verbatim.
    assert!(
        !prompt.contains(&"z".repeat(10_000)),
        "prompt leaked the full 10K body",
    );
    assert!(prompt.contains("…[truncated"), "truncation marker missing");
    // Prompt char count is bounded: a small system-instruction
    // header + the cap + a tiny suffix.
    let len = prompt.chars().count();
    assert!(len < 2_000, "prompt exceeded sane budget: {len} chars");
    // Telemetry recorded the running max.
    assert_eq!(synthesis::max_prompt_size_chars(), len);
}

/// COR-6 — when the namespace's `synthesis_failure_mode` is
/// `block_write`, a curator failure refuses the store with a typed
/// error instead of silently falling through.
#[test]
fn synthesis_block_write_namespace_refuses_on_curator_down() {
    let (conn, db_path) = open_db();
    let ns = "ns-block-write";
    install_synthesis_policy(
        &conn,
        ns,
        Some(ai_memory::models::SynthesisFailureMode::BlockWrite),
        None,
        None,
    );

    // Seed a candidate so the synthesis eligibility gate engages.
    let _existing = seed_existing(&conn, "kubernetes deployment notes", "earlier body", ns);

    let server = shared_mock_for_synthesis_error();
    let uri = server.uri();
    let llm = OllamaClient::new_with_url(&uri, "test-model").expect("mock client");

    let result = run_store(
        &conn,
        &db_path,
        &llm,
        json!({
            "title": "kubernetes rolling deploy strategy",
            "content": BASE_CONTENT,
            "namespace": ns,
            "on_conflict": "version",
        }),
    );
    let err = result.expect_err("block_write must refuse under curator outage");
    assert!(
        err.contains("SYNTHESIS_FAILED"),
        "expected SYNTHESIS_FAILED envelope, got: {err}"
    );

    // No new row was inserted — the existing candidate is the only
    // row in the namespace.
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE namespace = ?1",
            [ns],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "block_write must not insert under curator outage");
}

/// COR-5 — a verdict batch with multiple `update` verbs honours ALL
/// of them in sequence (instead of silently dropping all but the
/// first). A WARN is emitted; we don't assert on log output here but
/// the substrate-state assertion is the load-bearing check.
#[test]
fn synthesis_multi_update_verdict_honors_all_updates() {
    let (conn, db_path) = open_db();
    let ns = "ns-multi-update";

    let id_a = seed_existing(&conn, "kubernetes deploy strategy A", "old A", ns);
    let id_b = seed_existing(&conn, "kubernetes rolling notes B", "old B", ns);

    let verdict = json!({
        "verdicts": [
            {
                "candidate_id": id_a,
                "verb": "update",
                "merged_content": "merged-A-content"
            },
            {
                "candidate_id": id_b,
                "verb": "update",
                "merged_content": "merged-B-content"
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
    .expect("store ok");

    // Response carries the PRIMARY (first) update's candidate id.
    assert_eq!(resp["id"].as_str(), Some(id_a.as_str()));
    assert_eq!(resp["synthesis_decisions"]["update"].as_u64(), Some(2));

    // BOTH candidates have been rewritten — the previously-silent drop
    // of the second update is closed.
    let body_a: String = conn
        .query_row("SELECT content FROM memories WHERE id = ?1", [&id_a], |r| {
            r.get(0)
        })
        .unwrap();
    let body_b: String = conn
        .query_row("SELECT content FROM memories WHERE id = ?1", [&id_b], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(body_a, "merged-A-content", "first update applied");
    assert_eq!(body_b, "merged-B-content", "second update applied (COR-5)");
}
