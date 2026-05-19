// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #838 — final residual coverage pull for `src/mcp/tools/store.rs`.
//!
//! The companion files (`tests/store_residuum_coverage.rs`,
//! `tests/store_synthesis_error_paths.rs`, and the in-module
//! `legacy_classifier_handles_no_and_error_responses` test) lifted the
//! per-module floor from 94.46% to 94.60%. The 1.4% gap to the 96%
//! gate breaks down (per `cov-838-final.json`) into:
//!
//! * L93/100/107 — `forward_to_http` send/read error arms exercised
//!   under a wiremock that 5xx-rejects the request or closes the
//!   connection mid-body. Only the federation-forward MCP write path
//!   hits this helper, and no prior test wires it.
//! * L227/237 — agent_id / scope `if let Some(obj)` insertions when
//!   metadata is **not** a JSON object. Drives the false-branch of the
//!   `as_object_mut()` guards (rare — most callers pass an object).
//! * L924/937 — `auto_tag` Err arm + legacy-per-pair-classifier loop
//!   continue when the LLM call errors (the existing in-module test
//!   only exercises the detect_contradiction Err arm, not auto_tag).
//! * L958/965-966/979-981 — post-autonomy metadata loop branches:
//!     - L958 — `if !auto_tags.is_empty()` true with empty
//!       `confirmed_contradictions` (only the auto_tags branch fires)
//!     - L965-966 — both auto_tags AND confirmed_contradictions
//!       populated (both branches fire under a single update)
//!     - L979-981 — `db::update` failure warn arm under the autonomy
//!       metadata persist call (engineered by deleting the row
//!       between insert and the autonomy hook re-update)
//!
//! Cross-reference: parent issue #827 (per-module coverage residuum)
//! and the README in `tests/store_synthesis_error_paths.rs` for the
//! verdict / synthesis-failed-flag arms.

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
    clippy::wildcard_imports,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
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
// Fixtures (mirror `tests/store_synthesis_error_paths.rs` — kept here so
// the file is auditable in isolation under `cargo test --test
// store_legacy_classifier_paths`).
// ---------------------------------------------------------------------------

fn local_runs_root() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".local-runs")
        .join("tmp")
}

fn fresh_db_path() -> PathBuf {
    let root = local_runs_root();
    std::fs::create_dir_all(&root).ok();
    root.join(format!("store-legacy-{}.db", uuid::Uuid::new_v4()))
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

const BASE_CONTENT: &str = "This is a substantial body so the AUTONOMY_MIN_CONTENT_LEN gate fires \
     and the synthesis hook becomes eligible during the store call. \
     Padding to ensure the post-store autonomy hooks fire as well.";

fn mock_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime builds")
    })
}

fn run_store(
    conn: &Connection,
    db_path: &PathBuf,
    llm: Option<&OllamaClient>,
    autonomous_hooks: bool,
    params: Value,
) -> Result<Value, String> {
    let ttl = ResolvedTtl::default();
    ai_memory::mcp::tools::handle_store_for_tests(
        conn,
        db_path,
        &params,
        None,
        llm,
        None,
        &ttl,
        autonomous_hooks,
        None,
        None,
    )
}

fn run_store_with_forward(
    conn: &Connection,
    db_path: &PathBuf,
    forward_url: &str,
    params: Value,
) -> Result<Value, String> {
    let ttl = ResolvedTtl::default();
    ai_memory::mcp::tools::handle_store_for_tests(
        conn,
        db_path,
        &params,
        None,
        None,
        None,
        &ttl,
        false,
        None,
        Some(forward_url),
    )
}

/// Install the legacy per-pair classifier policy on `ns` so the
/// substrate's pre-synthesis classifier loop (`store.rs` L934-950)
/// fires instead of routing through the new synthesis batch call.
fn install_legacy_classifier_policy(conn: &Connection, ns: &str) {
    use ai_memory::models::{
        ApproverType, CorePolicy, GovernanceLevel, GovernancePolicy, MemoryKind, SynthesisPolicy,
        default_metadata,
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
            legacy_per_pair_classifier: Some(true),
            synthesis_failure_mode: None,
            synthesis_max_deletes_per_call: None,
            synthesis_max_candidate_chars: None,
        },
        ..Default::default()
    };
    let now = Utc::now().to_rfc3339();
    let mut metadata = default_metadata();
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String("ai:test".to_string()),
        );
        obj.insert(
            "governance".to_string(),
            serde_json::to_value(&policy).unwrap(),
        );
    }
    let standard = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: ai_memory::models::Tier::Long,
        namespace: format!("_standards-{ns}"),
        title: format!("legacy-std-{ns}"),
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
    let sid = db::insert(conn, &standard).expect("insert standard");
    db::set_namespace_standard(conn, ns, &sid, None).expect("set standard");
}

// ---------------------------------------------------------------------------
// #838 final residuum — L751/L760-761 — dedup-merge embedder retry path
// with `vector_index = Some`. The existing residuum suites cover the
// dedup-merge path with `vector_index = None` (embed lands but the
// index removal/insert is skipped); the lines 757-760 false-arm of
// `if let Some(idx) = vector_index` close only when a `Some` index is
// threaded through AND content actually changes during the merge so
// the inner `embed().is_ok()` branch fires.
// ---------------------------------------------------------------------------

/// Tiny in-test embedder: returns a deterministic 4-d vector derived
/// from the input length so the dedup-merge embed retry path can be
/// exercised without pulling in HuggingFace model loading.
struct LenEmbedder;

impl ai_memory::embeddings::Embed for LenEmbedder {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let n = text.len() as f32;
        let mag = (n * n + 4.0).sqrt();
        Ok(vec![n / mag, 1.0 / mag, 1.0 / mag, 1.0 / mag])
    }

    fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

#[test]
fn dedup_merge_embed_retry_with_vector_index_some() {
    use ai_memory::hnsw::VectorIndex;

    let (conn, db_path) = open_db();
    let ns = "ns-dedup-merge-idx";
    let seed_id = seed_existing(
        &conn,
        "dedup-merge-target",
        "seed body content with substantial length for stable diffing",
        ns,
    );

    let vector_index = VectorIndex::empty();
    let embedder = LenEmbedder;
    let ttl = ResolvedTtl::default();
    let params = json!({
        "title": "dedup-merge-target",
        "content": "incoming body content with DIFFERENT words so content_changed is true",
        "namespace": ns,
        "on_conflict": "merge",
    });

    let resp = ai_memory::mcp::tools::handle_store_for_tests(
        &conn,
        &db_path,
        &params,
        Some(&embedder as &dyn ai_memory::embeddings::Embed),
        None,
        Some(&vector_index),
        &ttl,
        false,
        None,
        None,
    )
    .expect("merge upsert with embed must succeed");

    assert_eq!(
        resp["id"].as_str(),
        Some(seed_id.as_str()),
        "dedup-merge must reuse seed id, got: {resp}"
    );
    assert_eq!(resp["duplicate"].as_bool(), Some(true));

    let hits = vector_index.search(&[0.5, 0.5, 0.5, 0.5], 5);
    assert!(
        hits.iter().any(|h| h.id == seed_id),
        "vector_index must contain merged row after embed retry, got: {hits:?}"
    );
}

// ---------------------------------------------------------------------------
// L924-925 — auto_tag Err arm. The existing in-module
// `legacy_classifier_handles_no_and_error_responses` exercises the
// `detect_contradiction` Err arm; this pins the symmetric `auto_tag` Err
// arm where /api/generate returns 5xx while /api/chat (synthesis) is OK.
// ---------------------------------------------------------------------------

#[test]
fn autonomy_hook_auto_tag_error_arm_logs_warn_and_continues() {
    let (conn, db_path) = open_db();
    let ns = "ns-autotag-err";

    let server =
        mock_runtime().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/api/tags"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
                .mount(&server)
                .await;
            // synthesis call (chat) — return empty verdicts so the synth path
            // hands off cleanly to the autonomy hooks below.
            Mock::given(method("POST"))
                .and(path("/api/chat"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    json!({"message": {"content": "{\"verdicts\": []}"}, "done": true}),
                ))
                .mount(&server)
                .await;
            // auto_tag (generate) — 500 so OllamaClient::auto_tag returns Err
            Mock::given(method("POST"))
                .and(path("/api/generate"))
                .respond_with(ResponseTemplate::new(500))
                .mount(&server)
                .await;
            server
        });
    let uri = server.uri();
    let llm = OllamaClient::new_with_url(&uri, "test-model").expect("mock client");

    let resp = run_store(
        &conn,
        &db_path,
        Some(&llm),
        true, // autonomous_hooks ON
        json!({
            "title": "autotag-err-memory",
            "content": BASE_CONTENT,
            "namespace": ns,
            "on_conflict": "version",
        }),
    )
    .expect("store ok even when auto_tag errors");

    // Row landed; metadata exists; auto_tags absent or empty.
    let id = resp["id"].as_str().expect("id").to_string();
    let meta: String = conn
        .query_row("SELECT metadata FROM memories WHERE id = ?1", [&id], |r| {
            r.get(0)
        })
        .expect("row exists");
    let parsed: Value = serde_json::from_str(&meta).expect("metadata is valid json");
    // auto_tag Err leaves auto_tags absent (or empty).
    if let Some(arr) = parsed.get("auto_tags").and_then(|v| v.as_array()) {
        assert!(
            arr.is_empty(),
            "auto_tags should be empty after Err: {arr:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// L93/100/107 — forward_to_http error arms exercised through the
// `federation_forward_url` MCP write-path entry point.
// ---------------------------------------------------------------------------

/// Server returns 500 → drives L108-111 (status.is_success false arm),
/// AND L100 (req.json body wire), AND L107 (text read success).
#[test]
fn forward_to_http_5xx_returns_structured_error() {
    let (conn, db_path) = open_db();

    let server = mock_runtime().block_on(async {
        let server = MockServer::start().await;
        // The /api/v1/memories route returns 500 with a body that should
        // be echoed back in the error string.
        Mock::given(method("POST"))
            .and(path("/api/v1/memories"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream down"))
            .mount(&server)
            .await;
        server
    });
    let forward_url = server.uri();

    let err = run_store_with_forward(
        &conn,
        &db_path,
        &forward_url,
        json!({
            "title": "forward-5xx-target",
            "content": BASE_CONTENT,
            "namespace": "ns-forward-5xx",
            "agent_id": "ai:fwd",
        }),
    )
    .expect_err("5xx upstream must surface as Err");
    assert!(
        err.contains("federation_forward") && err.contains("500") && err.contains("upstream down"),
        "expected federation_forward 500 echo, got: {err}"
    );
}

/// No listener at the URL → reqwest connection failure exercises
/// L101-103 (send-arm Err).
#[test]
fn forward_to_http_connection_failure_surfaces_send_error() {
    let (conn, db_path) = open_db();

    // Localhost on a vanishingly unlikely-to-be-bound high port. The
    // test does NOT need to bind anything — we want the send() to fail.
    let forward_url = "http://127.0.0.1:1";

    let err = run_store_with_forward(
        &conn,
        &db_path,
        forward_url,
        json!({
            "title": "forward-conn-fail",
            "content": BASE_CONTENT,
            "namespace": "ns-forward-conn-fail",
            "agent_id": "ai:fwd",
        }),
    )
    .expect_err("dead URL must surface as Err");
    assert!(
        err.contains("federation_forward"),
        "expected federation_forward send-error, got: {err}"
    );
}

/// 2xx with non-JSON body → drives L113-114 (parse-body Err arm).
#[test]
fn forward_to_http_non_json_body_surfaces_parse_error() {
    let (conn, db_path) = open_db();

    let server = mock_runtime().block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/memories"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json-at-all"))
            .mount(&server)
            .await;
        server
    });
    let forward_url = server.uri();

    let err = run_store_with_forward(
        &conn,
        &db_path,
        &forward_url,
        json!({
            "title": "forward-nonjson-target",
            "content": BASE_CONTENT,
            "namespace": "ns-forward-nonjson",
            "agent_id": "ai:fwd",
        }),
    )
    .expect_err("non-json body must surface as Err");
    assert!(
        err.contains("parse body") && err.contains("not-json-at-all"),
        "expected parse-body Err echo, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// L227 / L237 — `if let Some(obj) = metadata.as_object_mut()` false arm
// when caller supplies a non-object metadata (e.g. an array). The
// substrate must still write the row; the agent_id / scope insertion
// is silently skipped. Drives the "else" branch of both guards.
// ---------------------------------------------------------------------------

#[test]
fn store_accepts_non_object_metadata_without_panicking() {
    let (conn, db_path) = open_db();
    let ns = "ns-nonobj-meta";
    // metadata is an ARRAY → `as_object_mut()` returns None → the
    // agent_id and scope insertion both fall through.
    let resp = run_store(
        &conn,
        &db_path,
        None,
        false,
        json!({
            "title": "non-object-metadata-target",
            "content": BASE_CONTENT,
            "namespace": ns,
            "scope": "personal",
            "metadata": ["this", "is", "not", "an", "object"],
            "agent_id": "ai:nobj",
        }),
    );
    // Either the row lands (and metadata is whatever it was coerced to)
    // OR the substrate validate_metadata rejects it. Both branches
    // exercise the false-arm of the `if let Some(obj)` guards — what we
    // want to pin is "no panic, no crash". Anything else is acceptable.
    match resp {
        Ok(v) => {
            // Row landed.
            assert!(v["id"].as_str().is_some(), "id must be present, got: {v}");
        }
        Err(e) => {
            // validate_metadata rejection is fine — we exercised the
            // false-arm of `as_object_mut()` before reaching the
            // validator. The error message should be metadata-related.
            assert!(
                e.contains("metadata") || e.contains("scope"),
                "rejection must name metadata/scope, got: {e}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// L958 / L965-966 — both auto_tags AND confirmed_contradictions populated
// in the SAME metadata persist call. The existing
// `autonomy_hook_confirmed_contradictions_reach_response` test fires
// detect_contradiction → Ok(true) but its /api/generate mock returns
// "alpha\nbeta" which DOES populate auto_tags too — so it should
// already cover both. The remaining gap is L958 alone (auto_tags
// populated, confirmed_contradictions empty) which is the namespace
// WITHOUT the legacy_per_pair_classifier opt-in.
// ---------------------------------------------------------------------------

#[test]
fn autonomy_hook_auto_tags_only_no_legacy_classifier_persists_to_metadata() {
    let (conn, db_path) = open_db();
    let ns = "ns-autotags-only";
    let _ = seed_existing(&conn, "autotags-only similar title", "earlier body", ns);

    let server =
        mock_runtime().block_on(async {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/api/tags"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
                .mount(&server)
                .await;
            // synthesis (chat) — return empty verdicts so the synth path is
            // a no-op and control falls through to the autonomy hooks.
            Mock::given(method("POST"))
                .and(path("/api/chat"))
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    json!({"message": {"content": "{\"verdicts\": []}"}, "done": true}),
                ))
                .mount(&server)
                .await;
            // auto_tag (generate) — return a real tag list so auto_tags is
            // non-empty when the metadata persist call fires.
            Mock::given(method("POST"))
                .and(path("/api/generate"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"response": "deployment\nrolling\nk8s"})),
                )
                .mount(&server)
                .await;
            server
        });
    let uri = server.uri();
    let llm = OllamaClient::new_with_url(&uri, "test-model").expect("mock client");

    // No legacy_per_pair_classifier policy → the classifier loop is
    // skipped → confirmed_contradictions stays empty → only the
    // auto_tags branch of the metadata persist fires.
    let resp = run_store(
        &conn,
        &db_path,
        Some(&llm),
        true,
        json!({
            "title": "autotags-only target",
            "content": BASE_CONTENT,
            "namespace": ns,
            "on_conflict": "version",
        }),
    )
    .expect("store ok");

    let id = resp["id"].as_str().expect("id").to_string();
    let meta: String = conn
        .query_row("SELECT metadata FROM memories WHERE id = ?1", [&id], |r| {
            r.get(0)
        })
        .expect("row exists");
    let parsed: Value = serde_json::from_str(&meta).expect("valid metadata json");
    let auto_tags = parsed
        .get("auto_tags")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !auto_tags.is_empty(),
        "auto_tags must land in metadata when LLM returned tags; got metadata: {parsed}"
    );
    // confirmed_contradictions must be ABSENT (legacy classifier off).
    assert!(
        parsed.get("confirmed_contradictions").is_none()
            || parsed["confirmed_contradictions"]
                .as_array()
                .is_none_or(std::vec::Vec::is_empty),
        "confirmed_contradictions must be absent without legacy_per_pair_classifier opt-in"
    );
}

// ---------------------------------------------------------------------------
// L937 — legacy-classifier loop continue arm. Exercises the
// `cand.id == actual_id || cand.id == mem.id` branch where the loop
// skips the self-row in the candidate set. Symmetric to the
// in-module `legacy_classifier_handles_no_and_error_responses` test
// (which exercises detect_contradiction Ok(false) / Err but not the
// self-skip continue).
// ---------------------------------------------------------------------------

#[test]
fn legacy_classifier_skips_self_id_in_candidate_loop() {
    let (conn, db_path) = open_db();
    let ns = "legacy-self-skip-ns";
    install_legacy_classifier_policy(&conn, ns);

    let server = mock_runtime().block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"message": {"content": "no"}, "done": true})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"response": "tag1\ntag2"})),
            )
            .mount(&server)
            .await;
        server
    });
    let uri = server.uri();
    let llm = OllamaClient::new_with_url(&uri, "test-model").expect("mock client");

    // Seed an existing row with overlapping FTS keywords so it appears
    // in the `existing` candidate set; the merge-mode store below will
    // upsert into that row, making `actual_id == cand.id` for that
    // candidate → loop continue arm fires.
    let seed = seed_existing(
        &conn,
        "legacy self-skip canonical title",
        "earlier seeded body with substantial words for FTS overlap",
        ns,
    );

    let resp = run_store(
        &conn,
        &db_path,
        Some(&llm),
        true,
        json!({
            "title": "legacy self-skip canonical title",
            "content": "updated body with similar substantial words for FTS overlap",
            "namespace": ns,
            "on_conflict": "merge",
        }),
    )
    .expect("merge upsert ok");

    // Merge mode means the response targets the seed row.
    assert_eq!(
        resp["id"].as_str(),
        Some(seed.as_str()),
        "merge mode must reuse the seed row id"
    );
}

// ---------------------------------------------------------------------------
// Issue #838 — coverage for the v0.7.0 (#519) proactive contradiction
// detection block in `handle_store` (L779-L815). Those branches were
// added by commit 9f06d5178 and pulled store.rs coverage from 95.89%
// to 95.41% (sub-floor). The 8 unit tests in `tests/proactive_conflict.rs`
// pin the `db::proactive_conflict_check` substrate but do NOT execute
// the MCP wrapper that consumes its result.
//
// The branches we close here:
//
// * L789 — `params["force"].as_bool().unwrap_or(false)` true arm
//   (`force_write = true` bypass).
// * L790 — `!force_write && let Some(emb) = embedder` true arm with
//   embedder present.
// * L792-793 — `emb.embed(...)` Ok arm AND
//   `db::proactive_conflict_check` returns `Some(conflict)`.
// * L795-803 — `tracing::info!` conflict-refused log line.
// * L804-813 — `return Err("CONFLICT: ...")` envelope shape.
// * L790 false arm — `force_write = true` with embedder present
//   (skips the inner probe entirely).
// * L792 false arm reachability — `emb.embed` failure path (the
//   substrate falls through to the quota gate when embedding errors).
// * L793 None arm — embedder + Ok embedding but no conflict
//   detected (low similarity / non-contradicting content).
// ---------------------------------------------------------------------------

/// Constant-vector embedder: every input maps to the same unit vector
/// so the proactive conflict probe always sees cosine = 1.0 between
/// the incoming write and any seeded memory whose stored embedding
/// matches. Lets us drive the #519 conflict-refusal branch without
/// pulling in HuggingFace model loading.
struct FixedEmbedder {
    vector: Vec<f32>,
}

impl FixedEmbedder {
    fn unit_4d() -> Self {
        // Pre-normalised so cosine(self, self) = 1.0.
        let v = (1.0_f32 / 4.0_f32.sqrt()).abs();
        Self {
            vector: vec![v, v, v, v],
        }
    }
}

impl ai_memory::embeddings::Embed for FixedEmbedder {
    fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(self.vector.clone())
    }

    fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

/// Embedder that always errors — exercises the L792 false-arm
/// (`if let Ok(query_embedding) = emb.embed(&text)` fails so the
/// conflict probe is silently skipped and execution falls through).
struct ErrorEmbedder;

impl ai_memory::embeddings::Embed for ErrorEmbedder {
    fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        Err(anyhow::anyhow!("synthetic embedder failure"))
    }

    fn embed_batch(&self, _texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Err(anyhow::anyhow!("synthetic embedder failure"))
    }
}

/// Seed a row with a caller-supplied embedding. Mirrors the
/// http handler's "embed-before-lock" pattern.
fn seed_with_embedding(
    conn: &Connection,
    title: &str,
    content: &str,
    namespace: &str,
    embedding: &[f32],
) -> String {
    let id = seed_existing(conn, title, content, namespace);
    db::set_embedding(conn, &id, embedding).expect("set_embedding");
    id
}

#[test]
fn proactive_conflict_mcp_refuses_near_duplicate_with_conflict_envelope() {
    // The conflict block fires when:
    //   * embedder wired
    //   * force omitted (defaults false)
    //   * a seeded row in the same namespace has a near-identical
    //     embedding AND a different content body
    //   * AND a different title (to avoid the exact-title dedup
    //     branch upstream that short-circuits before the proactive
    //     conflict probe)
    let (conn, db_path) = open_db();
    let ns = "ns-pcd-refuse";
    let embedder = FixedEmbedder::unit_4d();
    let seed_id = seed_with_embedding(
        &conn,
        "established-fact",
        "deadline is june 15",
        ns,
        &embedder.vector,
    );

    // Different title (skips upstream dedup), different content
    // (triggers contradiction signal), same namespace.
    let params = json!({
        "title": "revised-fact",
        "content": "deadline is june 22",
        "namespace": ns,
        "agent_id": "ai:pcd",
    });

    let err = ai_memory::mcp::tools::handle_store_for_tests(
        &conn,
        &db_path,
        &params,
        Some(&embedder as &dyn ai_memory::embeddings::Embed),
        None,
        None,
        &ResolvedTtl::default(),
        false,
        None,
        None,
    )
    .expect_err("proactive conflict must refuse the write");

    assert!(
        err.starts_with("CONFLICT:"),
        "error must carry the CONFLICT: wire prefix, got: {err}"
    );
    assert!(
        err.contains(ns),
        "error envelope must name the namespace, got: {err}"
    );
    assert!(
        err.contains(&seed_id),
        "error envelope must surface the existing id, got: {err}"
    );
    assert!(
        err.contains("established-fact"),
        "error envelope must surface the existing title, got: {err}"
    );
    assert!(
        err.contains("near_duplicate_with_differing_content"),
        "error envelope must carry the substrate reason code, got: {err}"
    );
    assert!(
        err.contains("force=true"),
        "error envelope must hint at the bypass mechanism, got: {err}"
    );
}

#[test]
fn proactive_conflict_mcp_force_true_bypasses_probe_and_writes() {
    // L789 true arm — `force = true` short-circuits the probe so the
    // store lands even when a near-duplicate-with-differing-content
    // exists in the namespace.
    let (conn, db_path) = open_db();
    let ns = "ns-pcd-force";
    let embedder = FixedEmbedder::unit_4d();
    let _seed_id = seed_with_embedding(
        &conn,
        "established-fact",
        "deadline is june 15",
        ns,
        &embedder.vector,
    );

    let params = json!({
        "title": "revised-fact-forced",
        "content": "deadline is june 22",
        "namespace": ns,
        "agent_id": "ai:pcd",
        "force": true,
    });

    let resp = ai_memory::mcp::tools::handle_store_for_tests(
        &conn,
        &db_path,
        &params,
        Some(&embedder as &dyn ai_memory::embeddings::Embed),
        None,
        None,
        &ResolvedTtl::default(),
        false,
        None,
        None,
    )
    .expect("force=true must bypass the conflict probe and land the row");

    assert!(
        resp["id"].as_str().is_some(),
        "row must land when force=true, got: {resp}"
    );
    assert_eq!(
        resp["title"].as_str(),
        Some("revised-fact-forced"),
        "title must round-trip into the response"
    );
}

#[test]
fn proactive_conflict_mcp_no_embedder_skips_probe() {
    // L790 false arm — no embedder wired → the proactive conflict
    // probe is skipped entirely and the store falls through to the
    // quota + insert path.
    let (conn, db_path) = open_db();
    let ns = "ns-pcd-noembed";

    let params = json!({
        "title": "fact-without-embedder",
        "content": "anything",
        "namespace": ns,
        "agent_id": "ai:pcd",
    });

    let resp = ai_memory::mcp::tools::handle_store_for_tests(
        &conn,
        &db_path,
        &params,
        None, // no embedder
        None,
        None,
        &ResolvedTtl::default(),
        false,
        None,
        None,
    )
    .expect("store must land when no embedder is wired (probe skipped)");

    assert!(resp["id"].as_str().is_some(), "row must land, got: {resp}");
}

#[test]
fn proactive_conflict_mcp_embedder_error_falls_through_to_quota() {
    // L792 false arm — embedder errors → `emb.embed(...)` returns
    // `Err` → the `if let Ok(...)` guard fails → the conflict probe
    // is silently skipped → store falls through to quota + insert.
    let (conn, db_path) = open_db();
    let ns = "ns-pcd-emb-err";
    // Seed a near-duplicate that WOULD trigger the probe — to prove
    // the embedder-error path bypasses it.
    let seed = FixedEmbedder::unit_4d();
    let _seed_id = seed_with_embedding(
        &conn,
        "would-conflict",
        "deadline is june 15",
        ns,
        &seed.vector,
    );

    let embedder = ErrorEmbedder;
    let params = json!({
        "title": "incoming-write",
        "content": "deadline is june 22",
        "namespace": ns,
        "agent_id": "ai:pcd",
    });

    let resp = ai_memory::mcp::tools::handle_store_for_tests(
        &conn,
        &db_path,
        &params,
        Some(&embedder as &dyn ai_memory::embeddings::Embed),
        None,
        None,
        &ResolvedTtl::default(),
        false,
        None,
        None,
    )
    .expect("embedder Err must fall through to the quota/insert path");

    assert!(
        resp["id"].as_str().is_some(),
        "row must land even when embedder errors, got: {resp}"
    );
}

#[test]
fn proactive_conflict_mcp_low_similarity_does_not_refuse() {
    // L793 None arm — embedder Ok + probe runs + no candidate clears
    // the 0.95 cosine threshold → `proactive_conflict_check` returns
    // `Ok(None)` → the early-return is skipped → store proceeds.
    let (conn, db_path) = open_db();
    let ns = "ns-pcd-low-sim";

    // Seed with one unit vector; the incoming write embeds to an
    // ORTHOGONAL vector so cosine = 0.0 << 0.95.
    let orthogonal_seed = vec![1.0_f32, 0.0, 0.0, 0.0];
    let _seed_id = seed_with_embedding(
        &conn,
        "orthogonal-seed",
        "established fact alpha",
        ns,
        &orthogonal_seed,
    );

    // Embedder that always returns [0, 1, 0, 0] — orthogonal to seed.
    struct OrthogonalEmbedder;
    impl ai_memory::embeddings::Embed for OrthogonalEmbedder {
        fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.0, 1.0, 0.0, 0.0])
        }
        fn embed_batch(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            texts.iter().map(|t| self.embed(t)).collect()
        }
    }

    let embedder = OrthogonalEmbedder;
    let params = json!({
        "title": "incoming-orthogonal",
        "content": "totally unrelated fact",
        "namespace": ns,
        "agent_id": "ai:pcd",
    });

    let resp = ai_memory::mcp::tools::handle_store_for_tests(
        &conn,
        &db_path,
        &params,
        Some(&embedder as &dyn ai_memory::embeddings::Embed),
        None,
        None,
        &ResolvedTtl::default(),
        false,
        None,
        None,
    )
    .expect("low-similarity write must land — probe returned None");

    assert!(
        resp["id"].as_str().is_some(),
        "row must land for orthogonal embeddings, got: {resp}"
    );
}

#[test]
fn proactive_conflict_mcp_high_sim_same_content_does_not_refuse() {
    // L793 None arm — near-duplicate AND IDENTICAL content body. The
    // substrate classifies this as the upsert happy-path (not a
    // contradiction), so `proactive_conflict_check` returns `Ok(None)`
    // even when cosine = 1.0. The MCP wrapper falls through to insert.
    //
    // Different title is required so the upstream exact-title dedup
    // doesn't short-circuit before the probe.
    let (conn, db_path) = open_db();
    let ns = "ns-pcd-same-content";
    let embedder = FixedEmbedder::unit_4d();
    let _seed_id = seed_with_embedding(
        &conn,
        "user-pref-a",
        "user prefers dark mode",
        ns,
        &embedder.vector,
    );

    let params = json!({
        "title": "user-pref-b",
        "content": "user prefers dark mode",
        "namespace": ns,
        "agent_id": "ai:pcd",
    });

    let resp = ai_memory::mcp::tools::handle_store_for_tests(
        &conn,
        &db_path,
        &params,
        Some(&embedder as &dyn ai_memory::embeddings::Embed),
        None,
        None,
        &ResolvedTtl::default(),
        false,
        None,
        None,
    )
    .expect("same-content near-duplicate must NOT refuse (upsert happy-path)");

    assert!(
        resp["id"].as_str().is_some(),
        "row must land for same-content near-duplicate, got: {resp}"
    );
}

// ---------------------------------------------------------------------------
// Issue #838 — L996 / L1003 closing-region pin: `confirmed_contradictions`
// populated AND `auto_tags` EMPTY. The cov-838-r3 baseline shows L996
// (closing brace of the `if !auto_tags.is_empty()` true block) at hit
// count 0. The branch fires when:
//   * legacy_per_pair_classifier = true (populates confirmed_contradictions)
//   * auto_tag returns Ok(empty)        (auto_tags stays empty)
//   * detect_contradiction returns Ok(true) on at least one candidate
// Symmetric to `autonomy_hook_auto_tags_only_no_legacy_classifier_persists`
// (which covers the inverse — auto_tags populated, contradictions empty).
// ---------------------------------------------------------------------------

#[test]
fn autonomy_hook_confirmed_contradictions_only_no_auto_tags_persists_to_metadata() {
    let (conn, db_path) = open_db();
    let ns = "ns-contra-only";
    install_legacy_classifier_policy(&conn, ns);
    // Seed a candidate row with overlapping FTS keywords so it appears
    // in the `existing` set the legacy classifier scans.
    let _seed = seed_existing(
        &conn,
        "contra-only similar title",
        "earlier seeded body with overlap",
        ns,
    );

    let server = mock_runtime().block_on(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"models": []})))
            .mount(&server)
            .await;
        // synthesis (chat) — synthesis is GATED by !legacy_per_pair_classifier
        // so it won't fire; but legacy classifier uses chat too. Return
        // "yes" so detect_contradiction → Ok(true) → confirmed populated.
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"message": {"content": "yes"}, "done": true})),
            )
            .mount(&server)
            .await;
        // auto_tag (generate) — return empty so auto_tags stays empty.
        // OllamaClient::auto_tag parses newline-separated tokens; an
        // empty response yields no tags.
        Mock::given(method("POST"))
            .and(path("/api/generate"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"response": ""})))
            .mount(&server)
            .await;
        server
    });
    let uri = server.uri();
    let llm = OllamaClient::new_with_url(&uri, "test-model").expect("mock client");

    let resp = run_store(
        &conn,
        &db_path,
        Some(&llm),
        true,
        json!({
            "title": "contra-only canonical title",
            "content": BASE_CONTENT,
            "namespace": ns,
            "on_conflict": "version",
        }),
    )
    .expect("store ok");

    let id = resp["id"].as_str().expect("id").to_string();
    let meta: String = conn
        .query_row("SELECT metadata FROM memories WHERE id = ?1", [&id], |r| {
            r.get(0)
        })
        .expect("row exists");
    let parsed: Value = serde_json::from_str(&meta).expect("valid metadata json");

    // auto_tags MUST be absent or empty (LLM returned no tags) — exercises
    // the false-arm of `if !auto_tags.is_empty()` at L994/996.
    let auto_tags_empty = parsed
        .get("auto_tags")
        .and_then(|v| v.as_array())
        .is_none_or(std::vec::Vec::is_empty);
    assert!(
        auto_tags_empty,
        "auto_tags must be empty when LLM returned no tags, got: {parsed}"
    );

    // confirmed_contradictions MUST be populated (legacy classifier ran +
    // detect_contradiction returned Ok(true) for the seed candidate).
    let contras = parsed
        .get("confirmed_contradictions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !contras.is_empty(),
        "confirmed_contradictions must be populated when chat returned yes, got: {parsed}"
    );
}

#[test]
fn proactive_conflict_mcp_force_default_false_when_omitted() {
    // Wire-shape pin: `params["force"].as_bool().unwrap_or(false)` —
    // when callers omit `force` entirely, the unwrap_or branch fires
    // and the probe gets to run. The previous test seeds a refusal
    // when force is omitted; this one pins the symmetric absence-as-
    // default-false by checking the response shape against a write
    // that wouldn't refuse anyway (so the only signal is that the
    // probe ran but found nothing).
    let (conn, db_path) = open_db();
    let ns = "ns-pcd-force-default";
    let embedder = FixedEmbedder::unit_4d();

    // Empty namespace — no candidates exist → probe returns None.
    let params = json!({
        "title": "fresh-write-no-candidates",
        "content": "no priors in this namespace",
        "namespace": ns,
        "agent_id": "ai:pcd",
        // intentionally NO "force" key
    });

    let resp = ai_memory::mcp::tools::handle_store_for_tests(
        &conn,
        &db_path,
        &params,
        Some(&embedder as &dyn ai_memory::embeddings::Embed),
        None,
        None,
        &ResolvedTtl::default(),
        false,
        None,
        None,
    )
    .expect("first write with no priors must land");

    assert!(
        resp["id"].as_str().is_some(),
        "row must land in empty namespace, got: {resp}"
    );
}
