// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 issue #860 regression — `db::get_links` must surface the
//! temporal-validity columns (`valid_from`, `valid_until`, `observed_by`)
//! and the `attest_level` column the `memory_get_links` MCP tool's
//! docstring promises (`src/mcp/registry.rs:766`). Before the fix the
//! SELECT only pulled 4 columns and every optional field on the
//! `MemoryLink` struct was hard-coded to `None`, so a `memory_kg_invalidate`
//! call would write `valid_until` to the row but `memory_get_links`
//! would still return `valid_until = null` — silently dropping the
//! invalidation evidence callers depend on for audit + graph pruning.
//!
//! Scenario under test: store two memories → link them (signed via H2
//! self-signed) → invalidate the link → re-read with `get_links`. The
//! returned `MemoryLink` must carry the post-invalidation `valid_until`
//! AND the post-supersession `attest_level = "unsigned"` (H5 clears
//! the signing surface on invalidation, see `storage::invalidate_link`).

use ai_memory::db;
use ai_memory::models::{ConfidenceSource, Memory, MemoryKind, Tier};
use chrono::Utc;
use rusqlite::Connection;
use std::path::Path;

fn open_test_db() -> Connection {
    db::open(Path::new(":memory:")).expect("open in-memory db")
}

fn seed_memory(conn: &Connection, namespace: &str, title: &str) -> String {
    let now = Utc::now().to_rfc3339();
    let m = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("body for {title}"),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".into(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
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
    db::insert(conn, &m).expect("insert memory")
}

#[test]
fn get_links_surfaces_valid_until_after_invalidate() {
    let conn = open_test_db();
    let src = seed_memory(&conn, "issue-860", "source");
    let tgt = seed_memory(&conn, "issue-860", "target");

    // Create the link via the unsigned-default path. The row lands with
    // `attest_level = "unsigned"` and `valid_until = NULL`.
    db::create_link(&conn, &src, &tgt, "related_to").expect("create_link");

    // Sanity: the freshly-inserted row reads back without a
    // `valid_until` stamp.
    let before = db::get_links(&conn, &src).expect("get_links pre-invalidate");
    let matched_before = before
        .iter()
        .find(|l| l.source_id == src && l.target_id == tgt)
        .expect("pre-invalidate link must surface in get_links");
    assert!(
        matched_before.valid_until.is_none(),
        "pre-invalidate link must NOT carry valid_until; got {:?}",
        matched_before.valid_until
    );
    // Pre-fix would land None here too because the column was never
    // SELECTed; after the fix the column is real and `unsigned` is the
    // expected canonical default. Either NULL (legacy rows pre-H2) or
    // `"unsigned"` is acceptable as a "no signature attached" signal.
    assert!(
        matched_before
            .attest_level
            .as_deref()
            .is_none_or(|s| s == "unsigned"),
        "pre-invalidate attest_level must be unsigned or NULL; got {:?}",
        matched_before.attest_level
    );

    // Stamp the invalidation. `invalidate_link` writes `valid_until` to
    // the row (and also clears the signing surface if any, per H5).
    let stamp = "2026-05-18T12:34:56+00:00";
    let outcome = db::invalidate_link(&conn, &src, &tgt, "related_to", Some(stamp))
        .expect("invalidate_link must succeed");
    assert!(
        outcome.is_some(),
        "invalidate_link must return Some(InvalidateResult) for an existing row"
    );

    // The fix: get_links must now surface valid_until.
    let after = db::get_links(&conn, &src).expect("get_links post-invalidate");
    let matched_after = after
        .iter()
        .find(|l| l.source_id == src && l.target_id == tgt)
        .expect("post-invalidate link must still surface in get_links");
    assert_eq!(
        matched_after.valid_until.as_deref(),
        Some(stamp),
        "get_links must surface the post-invalidate valid_until stamp \
         (pre-#860 fix this was hard-coded to None)"
    );
    // attest_level must reflect the post-invalidation column value. The
    // create-default leaves the row at `unsigned`; H5 keeps it unsigned
    // because there was no prior signature to clear. Either way the
    // column MUST be surfaced as a real string, not silently dropped.
    assert_eq!(
        matched_after.attest_level.as_deref(),
        Some("unsigned"),
        "get_links must surface attest_level after invalidation; pre-#860 \
         fix this column was never SELECTed and the struct field was \
         hard-coded to None"
    );

    // Bidirectional surfacing — the inbound view from `tgt` must see the
    // same temporal-validity columns.
    let inbound = db::get_links(&conn, &tgt).expect("get_links inbound");
    let matched_inbound = inbound
        .iter()
        .find(|l| l.source_id == src && l.target_id == tgt)
        .expect("inbound view must include the now-invalidated link");
    assert_eq!(
        matched_inbound.valid_until.as_deref(),
        Some(stamp),
        "inbound get_links must also surface valid_until"
    );
}

#[test]
fn get_links_returns_observed_by_when_present() {
    // The promised columns include `observed_by`. Confirm a row whose
    // signing-surface observed_by claim is populated round-trips through
    // get_links. We poke the value into the row directly via SQL so we
    // don't have to spin up a full signing keypair in a regression test.
    let conn = open_test_db();
    let src = seed_memory(&conn, "issue-860", "obs-source");
    let tgt = seed_memory(&conn, "issue-860", "obs-target");
    db::create_link(&conn, &src, &tgt, "related_to").expect("create_link");

    // Stamp the row's observed_by + valid_from columns directly. This
    // mirrors what the inbound federation path does via
    // `db::create_link_inbound`; for the read-path regression we just
    // need the columns populated.
    let valid_from = "2026-05-17T10:00:00+00:00";
    conn.execute(
        "UPDATE memory_links \
            SET observed_by = ?1, valid_from = ?2 \
          WHERE source_id = ?3 AND target_id = ?4 AND relation = ?5",
        rusqlite::params!["peer:bob", valid_from, src, tgt, "related_to"],
    )
    .expect("update observed_by + valid_from");

    let links = db::get_links(&conn, &src).expect("get_links");
    let matched = links
        .iter()
        .find(|l| l.source_id == src && l.target_id == tgt)
        .expect("link must surface");
    assert_eq!(
        matched.observed_by.as_deref(),
        Some("peer:bob"),
        "observed_by must round-trip through get_links"
    );
    assert_eq!(
        matched.valid_from.as_deref(),
        Some(valid_from),
        "valid_from must round-trip through get_links"
    );
}
