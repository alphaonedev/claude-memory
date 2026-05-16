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
use ai_memory::mcp::tools::kg_invalidate::handle_kg_invalidate;
use ai_memory::models::{ConfidenceSource, Memory, MemoryKind, Tier};
use ai_memory::storage as db;
use chrono::Utc;
use rusqlite::Connection;
use serde_json::json;
use std::path::Path;

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

fn fresh_conn() -> Connection {
    db::open(Path::new(":memory:")).expect("open in-memory DB")
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
