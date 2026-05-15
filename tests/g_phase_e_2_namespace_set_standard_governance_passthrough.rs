// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 G-PHASE-E-2 (issue #707) — `memory_namespace_set_standard`
//! governance pass-through.
//!
//! Pre-#707 regression: when the caller passed a `governance` payload
//! to `memory_namespace_set_standard`, the handler round-tripped it
//! through the typed `GovernancePolicy` struct (write / promote /
//! delete / approver / inherit / `max_reflection_depth`). Any other key
//! — most notably `require_approval_above_depth`, which is read by the
//! free function `storage::resolve_require_approval_above_depth` and
//! never lives on the typed struct — was silently dropped during the
//! re-serialise step.
//!
//! Effect on the operator: setting `require_approval_above_depth: 1`
//! on a memory's `metadata.governance` and later calling
//! `memory_namespace_set_standard` (for any reason, e.g. promoting it
//! to standard or attaching a parent) removed the gate without any
//! error or log.
//!
//! The fix merges the incoming governance JSON onto the existing
//! `metadata.governance` blob key-by-key, so extra fields on EITHER
//! side survive the round-trip. This regression file pins:
//!
//! 1. Existing-side `require_approval_above_depth` survives a
//!    set-standard call that supplies only the typed governance
//!    whitelist (write / promote / delete / approver / inherit).
//! 2. Incoming-side `require_approval_above_depth` lands on the
//!    standard memory and is readable by
//!    `resolve_require_approval_above_depth` afterwards.
//! 3. Other "extra" fields (e.g. `skill_promotion_min_depth`) are
//!    likewise preserved.

use ai_memory::models::{Memory, MemoryKind, Tier};
use ai_memory::storage as db;
use chrono::Utc;
use rusqlite::Connection;
use serde_json::json;
use std::path::Path;

fn fresh_conn() -> Connection {
    db::open(Path::new(":memory:")).expect("open in-memory DB")
}

/// Insert a memory pre-populated with the given metadata; returns the id.
fn insert_with_metadata(
    conn: &Connection,
    namespace: &str,
    title: &str,
    metadata: serde_json::Value,
) -> String {
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
        metadata,
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    db::insert(conn, &mem).expect("insert memory")
}

/// Call the MCP `memory_namespace_set_standard` handler. The handler
/// is `pub` (promoted from `pub(crate)` by G-PHASE-E-2) so this
/// regression file pins the substrate behaviour without going through
/// stdio JSON-RPC.
fn call_set_standard(
    conn: &Connection,
    params: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    ai_memory::mcp::handle_namespace_set_standard(conn, params)
}

/// Re-read a memory's metadata.governance blob.
fn read_governance(conn: &Connection, id: &str) -> serde_json::Value {
    let mem = db::get(conn, id).expect("db::get").expect("memory exists");
    mem.metadata
        .get("governance")
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

#[test]
fn existing_require_approval_above_depth_survives_set_standard() {
    let conn = fresh_conn();
    // Memory pre-populated with the full operator-intended governance,
    // including `require_approval_above_depth: 1`.
    let id = insert_with_metadata(
        &conn,
        "phase-e-2",
        "standard",
        json!({
            "governance": {
                "approver": "human",
                "require_approval_above_depth": 1,
                "write": "any",
                "delete": "owner",
                "promote": "any",
                "inherit": true,
            }
        }),
    );
    // Call set-standard with only the typed whitelist fields — exactly
    // the pre-#707 caller shape that triggered the silent drop.
    let resp = call_set_standard(
        &conn,
        &json!({
            "namespace": "phase-e-2",
            "id": id,
            "governance": {
                "write": "any",
                "promote": "any",
                "delete": "owner",
                "approver": "human",
                "inherit": true,
            },
        }),
    )
    .expect("set-standard succeeds");
    assert_eq!(resp["set"], true);

    // The merged governance MUST still carry require_approval_above_depth.
    let gov = read_governance(&conn, &id);
    assert_eq!(
        gov["require_approval_above_depth"], 1,
        "require_approval_above_depth must survive set-standard; got governance={gov}"
    );
    // The free-function lookup that the live reflection-gate uses must
    // also resolve to the same value.
    let resolved = db::resolve_require_approval_above_depth(&conn, "phase-e-2")
        .expect("resolver returns Some after merge");
    assert_eq!(
        resolved, 1,
        "resolver must surface require_approval_above_depth"
    );
}

#[test]
fn incoming_require_approval_above_depth_lands_on_standard() {
    let conn = fresh_conn();
    // Memory starts WITHOUT a governance blob; the operator supplies
    // require_approval_above_depth on the set-standard call directly.
    let id = insert_with_metadata(&conn, "phase-e-2-incoming", "standard", json!({}));
    let resp = call_set_standard(
        &conn,
        &json!({
            "namespace": "phase-e-2-incoming",
            "id": id,
            "governance": {
                "write": "owner",
                "require_approval_above_depth": 2,
            },
        }),
    )
    .expect("set-standard succeeds");
    assert_eq!(resp["set"], true);

    let gov = read_governance(&conn, &id);
    assert_eq!(
        gov["require_approval_above_depth"], 2,
        "incoming require_approval_above_depth must land; got governance={gov}"
    );
    let resolved = db::resolve_require_approval_above_depth(&conn, "phase-e-2-incoming")
        .expect("resolver returns Some after incoming set");
    assert_eq!(resolved, 2);
}

#[test]
fn extra_fields_on_existing_blob_survive_set_standard() {
    let conn = fresh_conn();
    // Memory pre-populated with both require_approval_above_depth AND
    // skill_promotion_min_depth — both are free-function lookups that
    // live outside GovernancePolicy.
    let id = insert_with_metadata(
        &conn,
        "phase-e-2-extras",
        "standard",
        json!({
            "governance": {
                "write": "any",
                "require_approval_above_depth": 3,
                "skill_promotion_min_depth": 2,
                "an_arbitrary_marker": "audit-rev-2026-05-14",
            }
        }),
    );
    let resp = call_set_standard(
        &conn,
        &json!({
            "namespace": "phase-e-2-extras",
            "id": id,
            "governance": {
                "write": "owner",
            },
        }),
    )
    .expect("set-standard succeeds");
    assert_eq!(resp["set"], true);

    let gov = read_governance(&conn, &id);
    // Incoming override applied:
    assert_eq!(
        gov["write"], "owner",
        "incoming write must override; governance={gov}"
    );
    // Existing extras preserved:
    assert_eq!(gov["require_approval_above_depth"], 3, "governance={gov}");
    assert_eq!(gov["skill_promotion_min_depth"], 2, "governance={gov}");
    assert_eq!(
        gov["an_arbitrary_marker"], "audit-rev-2026-05-14",
        "even unknown free-form keys must survive; governance={gov}"
    );
}

#[test]
fn incoming_overrides_existing_for_overlapping_keys() {
    let conn = fresh_conn();
    let id = insert_with_metadata(
        &conn,
        "phase-e-2-override",
        "standard",
        json!({
            "governance": {
                "write": "any",
                "require_approval_above_depth": 1,
            }
        }),
    );
    let resp = call_set_standard(
        &conn,
        &json!({
            "namespace": "phase-e-2-override",
            "id": id,
            "governance": {
                "write": "owner",
                "require_approval_above_depth": 9,
            },
        }),
    )
    .expect("set-standard succeeds");
    assert_eq!(resp["set"], true);
    let gov = read_governance(&conn, &id);
    assert_eq!(gov["write"], "owner");
    // Incoming side wins on key conflict.
    assert_eq!(gov["require_approval_above_depth"], 9, "governance={gov}");
}
