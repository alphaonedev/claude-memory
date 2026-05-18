// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 COV-8 (Cluster D, issue #767) — K9 governance gate on
//! `memory_kg_invalidate`.
//!
//! Pre-cluster-D regression: `kg_invalidate` consults
//! `Permissions::evaluate` (see `src/mcp/tools/kg_invalidate.rs:39+`),
//! but no named test pinned the refusal contract. A future refactor
//! that dropped the gate would NOT trip the test fleet.
//!
//! This file installs a deny-everything-for-`memory_link` rule
//! into the process-wide K9 registry, invokes `handle_kg_invalidate`
//! directly, and asserts the call surfaces the rule's refusal
//! verbatim in the error string. A complementary control case (no
//! rule installed) asserts the call would otherwise succeed against
//! the same source/target pair, isolating the refusal to the
//! permission decision rather than to a substrate-internal failure.
//!
//! # Why direct-handler-drive, not MCP stdio
//!
//! The MCP JSON-RPC dispatch path is exercised end-to-end by the
//! broader integration suite. This file is the narrowest possible
//! pin on the K9 ↔ `kg_invalidate` edge — directly calling
//! `handle_kg_invalidate` with a stubbed `PermissionContext` proves
//! the SQL handler honours the substrate's `[[permissions.rules]]`
//! contract. The same shape any future caller (HTTP, CLI shell-out,
//! webhook) would hit.
//!
//! # Registry isolation
//!
//! Tests in this file mutate the process-wide
//! `ACTIVE_PERMISSION_RULES` registry. `cargo test` runs tests in
//! parallel within a binary, so we serialise via a file-local mutex
//! and reset the registry around each scenario. The same pattern
//! the L1-6 activation integration tests use.

use std::sync::{Mutex, OnceLock};

use ai_memory::governance::{
    PermissionRule, RuleDecision, clear_active_permission_rules_for_test,
    set_active_permission_rules,
};
// Validation reject paths exercise the function-level error returns
// that the K9 gate guards. Imported here so the additional unit-style
// scenarios at the bottom of this file can drive them.
use ai_memory::mcp::tools::kg_invalidate::handle_kg_invalidate;
use ai_memory::models::{ConfidenceSource, Memory, MemoryKind, Tier};
use ai_memory::storage as db;
use chrono::Utc;
use rusqlite::Connection;
use serde_json::json;
use std::path::Path;

mod common;
use common::fresh_conn;

/// Process-wide mutex serialising registry mutation across the
/// scenarios below. Tests in this file MUST `let _g =
/// registry_lock().lock().unwrap();` at function entry and hold the
/// guard for the duration of the body — without it a concurrent test
/// reading `active_permission_rules()` mid-mutation would observe a
/// transient state.
fn registry_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Insert a memory in `namespace` and return its id.
fn insert_memory(conn: &Connection, namespace: &str, title: &str) -> String {
    let now = Utc::now().to_rfc3339();
    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("body for {title}"),
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
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    };
    db::insert(conn, &mem).expect("insert memory")
}

/// Stage a source + target pair AND a `related_to` link between them
/// so `kg_invalidate` has a row to act on (otherwise the handler's
/// "no such link" path would mask the gate verdict).
fn stage_linked_pair(conn: &Connection, ns: &str) -> (String, String) {
    let src = insert_memory(conn, ns, "source");
    let dst = insert_memory(conn, ns, "target");
    db::create_link(conn, &src, &dst, "related_to").expect("create link");
    (src, dst)
}

/// Build a deny-on-`memory_link` rule scoped to the namespace +
/// agent that the scenario uses. Matches the `[[permissions.rules]]`
/// TOML shape documented in `src/governance/mod.rs`.
fn deny_link_rule(ns_pattern: &str, agent_pattern: &str, reason: &str) -> PermissionRule {
    PermissionRule {
        namespace_pattern: ns_pattern.to_string(),
        op: "memory_link".to_string(),
        agent_pattern: agent_pattern.to_string(),
        decision: RuleDecision::Deny,
        reason: Some(reason.to_string()),
    }
}

/// Primary pin (COV-8): when the K9 registry holds a deny rule for
/// `memory_link` matching the call's namespace + agent, the
/// `kg_invalidate` handler MUST refuse with an error string that
/// surfaces the operator-authored reason. The substrate must NOT
/// proceed to call `db::invalidate_link` — the link row stays intact.
#[test]
fn handle_kg_invalidate_refuses_when_rule_denies() {
    let _g = registry_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_active_permission_rules_for_test();

    let conn = fresh_conn();
    let ns = "secrets/api";
    let (src, dst) = stage_linked_pair(&conn, ns);

    set_active_permission_rules(vec![deny_link_rule(
        "secrets/*",
        "ai:*",
        "ai agents may not mutate secrets-namespace links",
    )]);

    let params = json!({
        "source_id": src,
        "target_id": dst,
        "relation": "related_to",
        "agent_id": "ai:claude",
    });
    let err = handle_kg_invalidate(&conn, Path::new(":memory:"), &params)
        .expect_err("kg_invalidate must refuse under deny rule");
    assert!(
        err.contains("denied"),
        "refusal error must mention permission denial, got: {err}"
    );
    assert!(
        err.contains("ai agents may not mutate secrets-namespace links"),
        "refusal must surface operator-authored reason verbatim, got: {err}"
    );

    // The link row must still be present — invalidate_link was never
    // called, so `valid_until` stays NULL on the row.
    let valid_until: Option<String> = conn
        .query_row(
            "SELECT valid_until FROM memory_links
             WHERE source_id = ?1 AND target_id = ?2 AND relation = ?3",
            rusqlite::params![src, dst, "related_to"],
            |r| r.get(0),
        )
        .expect("link row");
    assert!(
        valid_until.is_none(),
        "denied kg_invalidate must NOT have stamped valid_until — got: {valid_until:?}"
    );

    clear_active_permission_rules_for_test();
}

/// Control case: with NO rules installed, the same call against the
/// same staged link succeeds and stamps `valid_until`. Isolates the
/// refusal above to the K9 verdict rather than to a substrate or
/// schema misconfiguration.
#[test]
fn handle_kg_invalidate_succeeds_when_no_rule_denies() {
    let _g = registry_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_active_permission_rules_for_test();

    let conn = fresh_conn();
    let ns = "public/blog";
    let (src, dst) = stage_linked_pair(&conn, ns);

    let params = json!({
        "source_id": src,
        "target_id": dst,
        "relation": "related_to",
        "agent_id": "ai:claude",
    });
    let res = handle_kg_invalidate(&conn, Path::new(":memory:"), &params)
        .expect("kg_invalidate must succeed when no rule denies");
    assert_eq!(res["found"], json!(true));
    assert!(res["valid_until"].is_string(), "expected valid_until stamp");

    clear_active_permission_rules_for_test();
}

// -----------------------------------------------------------------
// v0.7-polish coverage recovery (issue #767) — additional K9
// branches (Ask path), validation reject paths, not-found path,
// and `valid_until` override path.
// -----------------------------------------------------------------

fn ask_link_rule(ns_pattern: &str, agent_pattern: &str, prompt: &str) -> PermissionRule {
    PermissionRule {
        namespace_pattern: ns_pattern.to_string(),
        op: "memory_link".to_string(),
        agent_pattern: agent_pattern.to_string(),
        decision: RuleDecision::Ask,
        reason: Some(prompt.to_string()),
    }
}

#[test]
fn handle_kg_invalidate_returns_ask_envelope_when_rule_asks() {
    let _g = registry_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_active_permission_rules_for_test();

    let conn = fresh_conn();
    let ns = "review/queue";
    let (src, dst) = stage_linked_pair(&conn, ns);

    set_active_permission_rules(vec![ask_link_rule(
        "review/*",
        "ai:*",
        "please confirm this invalidation",
    )]);

    let params = json!({
        "source_id": src,
        "target_id": dst,
        "relation": "related_to",
        "agent_id": "ai:claude",
    });
    let res = handle_kg_invalidate(&conn, Path::new(":memory:"), &params)
        .expect("Ask decision returns Ok JSON envelope rather than Err");
    assert_eq!(res["status"], json!("ask"));
    assert_eq!(res["action"], json!("kg_invalidate"));
    assert_eq!(res["source_id"], json!(src));
    assert_eq!(res["target_id"], json!(dst));
    assert!(
        res["reason"]
            .as_str()
            .is_some_and(|s| s.contains("please confirm")),
        "expected operator prompt to surface, got: {res:?}"
    );

    // Ask must NOT have stamped the link row.
    let valid_until: Option<String> = conn
        .query_row(
            "SELECT valid_until FROM memory_links
             WHERE source_id = ?1 AND target_id = ?2 AND relation = ?3",
            rusqlite::params![src, dst, "related_to"],
            |r| r.get(0),
        )
        .expect("link row");
    assert!(
        valid_until.is_none(),
        "Ask path must NOT have stamped valid_until — got: {valid_until:?}"
    );

    clear_active_permission_rules_for_test();
}

#[test]
fn handle_kg_invalidate_rejects_missing_source_id() {
    let conn = fresh_conn();
    let params = json!({
        "target_id": "x",
        "relation": "related_to",
    });
    let err = handle_kg_invalidate(&conn, Path::new(":memory:"), &params).unwrap_err();
    assert!(err.contains("source_id"), "got: {err}");
}

#[test]
fn handle_kg_invalidate_rejects_missing_target_id() {
    let conn = fresh_conn();
    let params = json!({
        "source_id": "x",
        "relation": "related_to",
    });
    let err = handle_kg_invalidate(&conn, Path::new(":memory:"), &params).unwrap_err();
    assert!(err.contains("target_id"), "got: {err}");
}

#[test]
fn handle_kg_invalidate_rejects_missing_relation() {
    let conn = fresh_conn();
    let params = json!({
        "source_id": "x",
        "target_id": "y",
    });
    let err = handle_kg_invalidate(&conn, Path::new(":memory:"), &params).unwrap_err();
    assert!(err.contains("relation"), "got: {err}");
}

#[test]
fn handle_kg_invalidate_rejects_self_link() {
    // validate_link refuses source_id == target_id.
    let conn = fresh_conn();
    let params = json!({
        "source_id": "x",
        "target_id": "x",
        "relation": "related_to",
    });
    let res = handle_kg_invalidate(&conn, Path::new(":memory:"), &params);
    assert!(res.is_err(), "validate_link must reject self-link: {res:?}");
}

#[test]
fn handle_kg_invalidate_rejects_empty_source_id() {
    let conn = fresh_conn();
    let params = json!({
        "source_id": "",
        "target_id": "dst",
        "relation": "related_to",
    });
    let res = handle_kg_invalidate(&conn, Path::new(":memory:"), &params);
    assert!(
        res.is_err(),
        "validate_id must reject empty source: {res:?}"
    );
}

#[test]
fn handle_kg_invalidate_rejects_invalid_valid_until_format() {
    let _g = registry_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_active_permission_rules_for_test();

    let conn = fresh_conn();
    let ns = "public/blog";
    let (src, dst) = stage_linked_pair(&conn, ns);

    let params = json!({
        "source_id": src,
        "target_id": dst,
        "relation": "related_to",
        "valid_until": "not-an-rfc3339-date",
        "agent_id": "ai:claude",
    });
    let res = handle_kg_invalidate(&conn, Path::new(":memory:"), &params);
    assert!(res.is_err(), "valid_until format must be validated");
}

#[test]
fn handle_kg_invalidate_accepts_explicit_valid_until_override() {
    let _g = registry_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_active_permission_rules_for_test();

    let conn = fresh_conn();
    let ns = "public/blog";
    let (src, dst) = stage_linked_pair(&conn, ns);

    let stamp = "2099-01-01T00:00:00Z";
    let params = json!({
        "source_id": src,
        "target_id": dst,
        "relation": "related_to",
        "valid_until": stamp,
        "agent_id": "ai:claude",
    });
    let res = handle_kg_invalidate(&conn, Path::new(":memory:"), &params)
        .expect("kg_invalidate must succeed with explicit valid_until");
    assert_eq!(res["valid_until"].as_str(), Some(stamp));
    assert_eq!(res["found"], json!(true));
}

#[test]
fn handle_kg_invalidate_returns_found_false_when_link_absent() {
    let _g = registry_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_active_permission_rules_for_test();

    let conn = fresh_conn();
    // Insert source + target so the K9 gate's namespace lookup
    // resolves cleanly, but DON'T create a link between them. The
    // handler short-circuits to `found: false`.
    let src = insert_memory(&conn, "lonely/ns", "src");
    let dst = insert_memory(&conn, "lonely/ns", "dst");

    let params = json!({
        "source_id": src,
        "target_id": dst,
        "relation": "related_to",
        "agent_id": "ai:claude",
    });
    let res = handle_kg_invalidate(&conn, Path::new(":memory:"), &params)
        .expect("kg_invalidate must succeed with absent link");
    assert_eq!(res["found"], json!(false));
    assert_eq!(res["source_id"], json!(src));
    assert_eq!(res["target_id"], json!(dst));
    assert_eq!(res["relation"], json!("related_to"));
    // The 'found: false' envelope omits valid_until / previous_valid_until.
    assert!(res.get("valid_until").is_none());
}

#[test]
fn handle_kg_invalidate_returns_found_false_for_unknown_ids() {
    let _g = registry_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_active_permission_rules_for_test();

    let conn = fresh_conn();
    // Neither memory exists. The K9 namespace lookup falls back to
    // "global" (per the `_ => "global"` arm), then invalidate_link
    // returns None because there's no matching row.
    let params = json!({
        "source_id": "no-such-source",
        "target_id": "no-such-target",
        "relation": "related_to",
        "agent_id": "ai:claude",
    });
    let res = handle_kg_invalidate(&conn, Path::new(":memory:"), &params)
        .expect("kg_invalidate must succeed even for unknown ids (returns found=false)");
    assert_eq!(res["found"], json!(false));
}

/// Drives the `resolve_agent_id` failure path (line 49) by passing an
/// `agent_id` containing illegal control chars. The K9 evaluator never
/// reaches `Permissions::evaluate` because `?` short-circuits with the
/// validator error.
#[test]
fn handle_kg_invalidate_rejects_malformed_agent_id() {
    let _g = registry_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_active_permission_rules_for_test();

    let conn = fresh_conn();
    let ns = "public/blog";
    let (src, dst) = stage_linked_pair(&conn, ns);

    let params = json!({
        "source_id": src,
        "target_id": dst,
        "relation": "related_to",
        // Whitespace / control chars cause resolve_agent_id to reject.
        "agent_id": "agent with space",
    });
    let res = handle_kg_invalidate(&conn, Path::new(":memory:"), &params);
    assert!(
        res.is_err(),
        "malformed agent_id must be rejected by resolve_agent_id"
    );
}

/// Successful invalidation against a source memory that carries an
/// `agent_id` in metadata — exercises the `.as_str().map(...)` path in
/// the post-invalidation event-dispatch block. The control case in
/// `handle_kg_invalidate_succeeds_when_no_rule_denies` uses memories
/// without `metadata.agent_id` so the `event_agent_id` defaults to None;
/// this test populates the metadata field to drive the Some-branch.
#[test]
fn handle_kg_invalidate_dispatches_event_with_owner_agent_id_from_metadata() {
    let _g = registry_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_active_permission_rules_for_test();

    let conn = fresh_conn();
    let ns = "public/notes";
    // Insert source + target with agent_id metadata set on the source.
    let now = Utc::now().to_rfc3339();
    let src_mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: ns.to_string(),
        title: "src-with-owner".to_string(),
        content: "body".to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now.clone(),
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({"agent_id": "ai:owner"}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    };
    let src_id = db::insert(&conn, &src_mem).unwrap();
    let dst_id = insert_memory(&conn, ns, "dst");
    db::create_link(&conn, &src_id, &dst_id, "related_to").unwrap();

    let params = json!({
        "source_id": src_id,
        "target_id": dst_id,
        "relation": "related_to",
        "agent_id": "ai:caller",
    });
    let res = handle_kg_invalidate(&conn, Path::new(":memory:"), &params)
        .expect("invalidation must succeed");
    assert_eq!(res["found"], json!(true));
}

/// Allow path with the matching rule explicitly installed — exercises
/// the `Decision::Allow` arm rather than the no-rule fall-through.
#[test]
fn handle_kg_invalidate_succeeds_under_explicit_allow_rule() {
    let _g = registry_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_active_permission_rules_for_test();

    let conn = fresh_conn();
    let ns = "team/notes";
    let (src, dst) = stage_linked_pair(&conn, ns);

    set_active_permission_rules(vec![PermissionRule {
        namespace_pattern: "team/*".to_string(),
        op: "memory_link".to_string(),
        agent_pattern: "ai:*".to_string(),
        decision: RuleDecision::Allow,
        reason: None,
    }]);

    let params = json!({
        "source_id": src,
        "target_id": dst,
        "relation": "related_to",
        "agent_id": "ai:claude",
    });
    let res = handle_kg_invalidate(&conn, Path::new(":memory:"), &params)
        .expect("explicit Allow rule must permit the invalidation");
    assert_eq!(res["found"], json!(true));
    assert!(res["valid_until"].is_string());

    clear_active_permission_rules_for_test();
}
