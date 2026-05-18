// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #831 — regression test pinning the `memory_promote` MCP
//! tool's tier-jump semantics + the new optional `target_tier`
//! parameter.
//!
//! `memory_promote` advances to the highest reachable tier (long) in
//! a single call by default — there is no implicit short→mid→long
//! step ladder. The MCP schema now accepts an optional `target_tier`
//! parameter ("mid" or "long") so callers that want stepwise control
//! can land on `mid` without going all the way to `long`. Omitting
//! `target_tier` preserves the historical "jump to long" behaviour.

use ai_memory::db;
use ai_memory::mcp::tools::handle_promote_for_tests;
use ai_memory::models::{ConfidenceSource, Memory, MemoryKind, Tier, default_metadata};
use chrono::{Duration, Utc};
use serde_json::{Value, json};

fn make_short_memory(expires_at: Option<String>) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Short,
        namespace: "lifecycle-test".to_string(),
        title: format!("lifecycle-row-{}", uuid::Uuid::new_v4()),
        content: "lifecycle test content".to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at,
        metadata: default_metadata(),
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
    }
}

/// **Issue #831** — pin both behaviours of `memory_promote`:
///
/// 1. Default (no `target_tier`) jumps a short-tier memory directly to
///    `long` in a single call — there is no implicit step ladder.
/// 2. `target_tier: "mid"` lands the memory on `mid` and stops there
///    (the historical jump-to-long is NOT applied when an intermediate
///    target is explicitly requested).
///
/// Both branches go through the MCP substrate handler so the test also
/// covers the schema-level surface (the `target_tier` parameter must
/// be accepted by the input schema and routed through to the
/// substrate).
#[test]
fn lifecycle_promote_target_tier_param_honored() {
    let conn = db::open(std::path::Path::new(":memory:")).expect("open in-memory db");

    // ----- (a) default: short → long single jump -----
    let mem_a = make_short_memory(Some((Utc::now() + Duration::hours(6)).to_rfc3339()));
    let id_a = db::insert(&conn, &mem_a).expect("insert short-tier memory A");

    let val_a: Value = handle_promote_for_tests(
        &conn,
        std::path::Path::new(":memory:"),
        &json!({"id": id_a}),
        None,
    )
    .expect("default promote should succeed");
    assert_eq!(val_a["promoted"], true);
    assert_eq!(val_a["mode"], "tier");
    assert_eq!(
        val_a["tier"], "long",
        "default promote on a short-tier memory must jump straight to long \
         (no implicit short→mid step ladder); got: {val_a}"
    );
    let after_a = db::get(&conn, &id_a).unwrap().unwrap();
    assert_eq!(after_a.tier, Tier::Long, "row tier after default promote");
    assert!(
        after_a.expires_at.is_none(),
        "long-tier rows must have expires_at cleared; got {:?}",
        after_a.expires_at
    );

    // ----- (b) target_tier=mid: short → mid (and stops) -----
    let mem_b = make_short_memory(Some((Utc::now() + Duration::hours(6)).to_rfc3339()));
    let id_b = db::insert(&conn, &mem_b).expect("insert short-tier memory B");

    let val_b: Value = handle_promote_for_tests(
        &conn,
        std::path::Path::new(":memory:"),
        &json!({"id": id_b, "target_tier": "mid"}),
        None,
    )
    .expect("promote target_tier=mid should succeed");
    assert_eq!(val_b["promoted"], true);
    assert_eq!(val_b["mode"], "tier");
    assert_eq!(
        val_b["tier"], "mid",
        "explicit target_tier=mid must land on mid (NOT jump through to long); got: {val_b}"
    );
    let after_b = db::get(&conn, &id_b).unwrap().unwrap();
    assert_eq!(
        after_b.tier,
        Tier::Mid,
        "row tier after target_tier=mid promote"
    );
}

/// **Issue #831 / Per-Module Coverage (#827)** — pin the explicit
/// `target_tier="long"` branch separately from the default-None
/// branch. The two arms (`None => Tier::Long` and `Some("long") =>
/// Tier::Long`) are distinct compiled branches in the
/// `params["target_tier"].as_str()` match; the integration test for
/// the default jump-to-long covers only the `None` arm, leaving
/// `Some("long")` uncovered and dragging `mcp/tools/promote.rs`
/// below its 94% per-module floor (commit 1c14957 added the
/// `target_tier` match without exhaustive test coverage).
///
/// Behavioural assertion: explicit `target_tier="long"` produces the
/// same observable outcome as the default (tier=long, `expires_at`
/// cleared) — operators using the explicit form to be defensive
/// against future default flips must get identical semantics today.
#[test]
fn lifecycle_promote_target_tier_long_explicit_lands_on_long() {
    let conn = db::open(std::path::Path::new(":memory:")).expect("open in-memory db");
    let mem = make_short_memory(Some((Utc::now() + Duration::hours(6)).to_rfc3339()));
    let id = db::insert(&conn, &mem).expect("insert short-tier memory");

    let val: Value = handle_promote_for_tests(
        &conn,
        std::path::Path::new(":memory:"),
        &json!({"id": id, "target_tier": "long"}),
        None,
    )
    .expect("explicit target_tier=long should succeed");
    assert_eq!(val["promoted"], true);
    assert_eq!(val["mode"], "tier");
    assert_eq!(
        val["tier"], "long",
        "explicit target_tier=long must land on long; got: {val}"
    );
    let after = db::get(&conn, &id).unwrap().unwrap();
    assert_eq!(after.tier, Tier::Long, "row tier after explicit long");
    assert!(
        after.expires_at.is_none(),
        "long-tier rows must have expires_at cleared (parity with default \
         None path); got {:?}",
        after.expires_at
    );
}

/// **Issue #831 / Per-Module Coverage (#827)** — pin the
/// `target_tier="short"` rejection branch. `short` is a downgrade
/// from any tier the promote path could be invoked on (memories
/// already on short never need to "promote" to short; memories on
/// mid/long would regress). The match arm returns a clear error
/// rather than silently no-op or attempting the downgrade.
///
/// Pre-fix the only error coverage on this branch was the
/// `#[cfg(test)] mod tests` unit tests which validate the early
/// id/validator paths; the post-validate `target_tier` match was
/// uncovered for this arm, contributing to the coverage gap.
#[test]
fn lifecycle_promote_target_tier_short_rejected_with_downgrade_message() {
    let conn = db::open(std::path::Path::new(":memory:")).expect("open in-memory db");
    let mem = make_short_memory(Some((Utc::now() + Duration::hours(6)).to_rfc3339()));
    let id = db::insert(&conn, &mem).expect("insert short-tier memory");

    let err = handle_promote_for_tests(
        &conn,
        std::path::Path::new(":memory:"),
        &json!({"id": id, "target_tier": "short"}),
        None,
    )
    .expect_err("target_tier=short must be rejected");
    assert!(
        err.contains("short") && err.contains("downgrade"),
        "error must cite the rejected tier + name the failure mode; \
         got: {err}"
    );
    // Row must be unchanged — rejection is pre-update.
    let after = db::get(&conn, &id).unwrap().unwrap();
    assert_eq!(
        after.tier,
        Tier::Short,
        "row tier must be unchanged after rejection; got: {:?}",
        after.tier
    );
}

/// **Issue #831 / Per-Module Coverage (#827)** — pin the
/// `target_tier=<unknown>` catch-all rejection branch. Any value
/// other than `"long"`, `"mid"`, or `"short"` must surface an error
/// that names BOTH the legal values AND the rejected token, so
/// operators typing `target_tier: "permanent"` (a plausible alias)
/// get an immediate corrective message rather than a silent
/// fallthrough.
#[test]
fn lifecycle_promote_target_tier_bogus_value_rejected_with_legal_set() {
    let conn = db::open(std::path::Path::new(":memory:")).expect("open in-memory db");
    let mem = make_short_memory(Some((Utc::now() + Duration::hours(6)).to_rfc3339()));
    let id = db::insert(&conn, &mem).expect("insert short-tier memory");

    let err = handle_promote_for_tests(
        &conn,
        std::path::Path::new(":memory:"),
        &json!({"id": id, "target_tier": "permanent"}),
        None,
    )
    .expect_err("unknown target_tier value must be rejected");
    assert!(
        err.contains("mid") && err.contains("long") && err.contains("permanent"),
        "error must enumerate the legal values AND echo the rejected \
         token; got: {err}"
    );
    // Row must be unchanged — rejection is pre-update.
    let after = db::get(&conn, &id).unwrap().unwrap();
    assert_eq!(after.tier, Tier::Short, "row tier must be unchanged");
}
