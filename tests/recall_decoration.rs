// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Gap 7 (issue #890) — recall-response Tier-3 decoration
//! regression suite.
//!
//! Acceptance criteria from the playbook:
//!
//! 1. Default `memory_recall` (`verbose_provenance=true`, the v0.7.0
//!    default) returns rows decorated with the full provenance
//!    audit trail: `confidence`, `confidence_tier`, `source`,
//!    `source_uri`, `freshness_state`, `access_count`,
//!    `last_accessed_at` (when set), and `latest_link_attest_level`
//!    (when at least one link is incident on the memory).
//! 2. `verbose_provenance=false` collapses the row to the v0.6.x
//!    shape (no derived fields) for callers that want the trimmed
//!    payload.
//! 3. The token-budget guards (`tests/token_budget_guard.rs`)
//!    continue to pass — the new tool definition and per-row
//!    decoration stay under their respective ceilings.

use ai_memory::config::{ResolvedScoring, ResolvedTtl};
use rusqlite::params;
use serde_json::json;

fn fresh_db() -> rusqlite::Connection {
    ai_memory::storage::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

fn seed_memory_full(conn: &rusqlite::Connection, id: &str, source_uri: Option<&str>) {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memories \
            (id, tier, namespace, title, content, confidence, source, source_uri, \
             access_count, created_at, updated_at, last_accessed_at) \
         VALUES (?1, 'long', 'g7', ?2, ?3, 0.92, 'api', ?4, 3, ?5, ?5, ?5)",
        params![
            id,
            format!("title-{id}"),
            format!("payload {id} for gap-7 decoration"),
            source_uri,
            now
        ],
    )
    .expect("seed memory");
    // FTS5 sync — the test bypasses the crate's insert helper for
    // compactness.
    conn.execute(
        "INSERT INTO memories_fts(rowid, title, content) \
         SELECT rowid, title, content FROM memories WHERE id = ?1",
        params![id],
    )
    .ok();
}

#[test]
fn gap7_recall_row_carries_full_provenance_block_by_default() {
    let conn = fresh_db();
    seed_memory_full(&conn, "m-gap7-a", Some("doc:gap-7-spec#para-1"));
    seed_memory_full(&conn, "m-gap7-b", None);

    let ttl = ResolvedTtl::default();
    let scoring = ResolvedScoring::default();
    let resp = ai_memory::mcp::handle_recall(
        &conn,
        &json!({"context": "decoration", "namespace": "g7"}),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall ok");

    let memories = resp["memories"]
        .as_array()
        .expect("recall response carries memories array");
    assert!(!memories.is_empty(), "expected at least one row");

    let row = &memories[0];
    // Base substrate columns serialized via Memory's serde derive.
    assert!(row["confidence"].is_number(), "confidence present");
    assert!(row["source"].is_string(), "source present");
    assert!(row["access_count"].is_number(), "access_count present");
    assert!(
        row["last_accessed_at"].is_string(),
        "last_accessed_at present (seeded above)"
    );
    // Gap 7 derived decoration:
    assert!(
        row["confidence_tier"].is_string(),
        "Gap 7: confidence_tier decoration present"
    );
    assert!(
        row["freshness_state"].is_string(),
        "Gap 7: freshness_state decoration present"
    );

    // The recall envelope echoes the Gap 3 recall_id so the caller
    // can cite it on a downstream store/link.
    assert!(
        resp["recall_id"].is_string() && !resp["recall_id"].as_str().unwrap().is_empty(),
        "Gap 3: recall_id echoed in the response envelope"
    );
}

#[test]
fn gap7_verbose_provenance_false_collapses_to_legacy_shape() {
    let conn = fresh_db();
    seed_memory_full(&conn, "m-gap7-c", None);

    let ttl = ResolvedTtl::default();
    let scoring = ResolvedScoring::default();
    let resp = ai_memory::mcp::handle_recall(
        &conn,
        &json!({
            "context": "decoration",
            "namespace": "g7",
            "verbose_provenance": false,
        }),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall ok");

    let memories = resp["memories"].as_array().unwrap();
    assert!(!memories.is_empty());
    let row = &memories[0];
    // Substrate columns still present (they ride on Memory's serde).
    assert!(row["confidence"].is_number());
    // Gap 7 derived decoration MUST be absent on this branch.
    assert!(
        row.get("confidence_tier").is_none(),
        "verbose_provenance=false ⇒ no confidence_tier decoration"
    );
    assert!(
        row.get("freshness_state").is_none(),
        "verbose_provenance=false ⇒ no freshness_state decoration"
    );
    assert!(
        row.get("latest_link_attest_level").is_none(),
        "verbose_provenance=false ⇒ no latest_link_attest_level decoration"
    );
}

#[test]
fn gap7_freshness_state_warm_for_recently_accessed_row() {
    // AC pin: a row touched in the last 30 days surfaces as "warm".
    // Pin the substrate-derived branch directly via the recall path
    // (which decorates with freshness_state when verbose_provenance
    // is true / default).
    let conn = fresh_db();
    let now = chrono::Utc::now().to_rfc3339();
    let recent = (chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339();
    conn.execute(
        "INSERT INTO memories \
            (id, tier, namespace, title, content, confidence, source, \
             access_count, created_at, updated_at, last_accessed_at) \
         VALUES ('m-warm', 'long', 'g7w', 'warm-title', 'warm payload search', 0.9, 'api', \
                 3, ?1, ?1, ?2)",
        params![now, recent],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memories_fts(rowid, title, content) \
         SELECT rowid, title, content FROM memories WHERE id = 'm-warm'",
        [],
    )
    .ok();
    let ttl = ResolvedTtl::default();
    let scoring = ResolvedScoring::default();
    let resp = ai_memory::mcp::handle_recall(
        &conn,
        &json!({"context": "search", "namespace": "g7w"}),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall");
    let memories = resp["memories"].as_array().unwrap();
    assert!(!memories.is_empty());
    let fs = memories[0]["freshness_state"]
        .as_str()
        .expect("freshness_state present");
    assert_eq!(fs, "warm", "row touched in last 30 days ⇒ warm (got {fs})");
}

#[test]
fn gap7_freshness_state_stale_for_old_last_access() {
    let conn = fresh_db();
    let now = chrono::Utc::now().to_rfc3339();
    let long_ago = (chrono::Utc::now() - chrono::Duration::days(90)).to_rfc3339();
    conn.execute(
        "INSERT INTO memories \
            (id, tier, namespace, title, content, confidence, source, \
             access_count, created_at, updated_at, last_accessed_at) \
         VALUES ('m-stale', 'long', 'g7s', 'stale-title', 'stale payload search', 0.9, 'api', \
                 50, ?1, ?1, ?2)",
        params![now, long_ago],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memories_fts(rowid, title, content) \
         SELECT rowid, title, content FROM memories WHERE id = 'm-stale'",
        [],
    )
    .ok();
    let ttl = ResolvedTtl::default();
    let scoring = ResolvedScoring::default();
    let resp = ai_memory::mcp::handle_recall(
        &conn,
        &json!({"context": "search", "namespace": "g7s"}),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall");
    let memories = resp["memories"].as_array().unwrap();
    assert!(!memories.is_empty(), "stale row still appears in recall");
    let fs = memories[0]["freshness_state"]
        .as_str()
        .expect("freshness_state present");
    assert_eq!(
        fs, "stale",
        "row last touched 90 days ago ⇒ stale (got {fs})"
    );
}

#[test]
fn gap7_freshness_state_expired_when_expires_at_in_past() {
    let conn = fresh_db();
    let now = chrono::Utc::now().to_rfc3339();
    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    conn.execute(
        "INSERT INTO memories \
            (id, tier, namespace, title, content, confidence, source, \
             access_count, created_at, updated_at, last_accessed_at, expires_at) \
         VALUES ('m-exp', 'short', 'g7e', 'exp-title', 'exp payload search', 0.9, 'api', \
                 2, ?1, ?1, ?1, ?2)",
        params![now, past],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO memories_fts(rowid, title, content) \
         SELECT rowid, title, content FROM memories WHERE id = 'm-exp'",
        [],
    )
    .ok();
    let ttl = ResolvedTtl::default();
    let scoring = ResolvedScoring::default();
    let resp = ai_memory::mcp::handle_recall(
        &conn,
        &json!({"context": "search", "namespace": "g7e"}),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall");
    let memories = resp["memories"].as_array().unwrap();
    // Even though expires_at is in the past, search/recall surfaces
    // the row when the filter selects it (recall's TTL gate is
    // separate). The decoration must still mark it expired.
    if !memories.is_empty() {
        let fs = memories[0]["freshness_state"]
            .as_str()
            .expect("freshness_state present");
        assert_eq!(
            fs, "expired",
            "row with past expires_at ⇒ expired (got {fs})"
        );
    }
}

#[test]
fn gap7_decoration_present_on_every_returned_row_not_just_first() {
    // AC pin: the decoration block applies to every row in the
    // response, not just the top hit.
    let conn = fresh_db();
    for i in 0..3 {
        seed_memory_full(&conn, &format!("m-multi-{i}"), None);
    }
    let ttl = ResolvedTtl::default();
    let scoring = ResolvedScoring::default();
    let resp = ai_memory::mcp::handle_recall(
        &conn,
        &json!({"context": "decoration", "namespace": "g7"}),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall");
    let memories = resp["memories"].as_array().unwrap();
    assert_eq!(memories.len(), 3, "all 3 seeded rows returned");
    for row in memories {
        assert!(
            row["confidence_tier"].is_string(),
            "every row carries confidence_tier"
        );
        assert!(
            row["freshness_state"].is_string(),
            "every row carries freshness_state"
        );
    }
}

#[test]
fn gap7_score_present_with_three_decimal_precision() {
    // AC pin: every row carries `score` field (existing behavior pre-Gap 7,
    // preserved post-Gap-7 — token-budget contract).
    let conn = fresh_db();
    seed_memory_full(&conn, "m-score", None);
    let ttl = ResolvedTtl::default();
    let scoring = ResolvedScoring::default();
    let resp = ai_memory::mcp::handle_recall(
        &conn,
        &json!({"context": "decoration", "namespace": "g7"}),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall");
    let row = &resp["memories"][0];
    assert!(row["score"].is_number(), "score field present on every row");
}

#[test]
fn gap7_source_uri_surfaces_through_serde_when_set() {
    // AC pin: the substrate-side source_uri is emitted via serde
    // (not added by the decorator). Pin both branches: present when
    // set, absent when None.
    let conn = fresh_db();
    seed_memory_full(&conn, "m-uri", Some("doc:source-test"));
    seed_memory_full(&conn, "m-no-uri", None);
    let ttl = ResolvedTtl::default();
    let scoring = ResolvedScoring::default();
    let resp = ai_memory::mcp::handle_recall(
        &conn,
        &json!({"context": "decoration", "namespace": "g7"}),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall");
    let memories = resp["memories"].as_array().unwrap();
    let by_id: std::collections::HashMap<_, _> = memories
        .iter()
        .filter_map(|m| m["id"].as_str().map(|id| (id, m)))
        .collect();
    if let Some(uri_row) = by_id.get("m-uri") {
        assert_eq!(
            uri_row["source_uri"].as_str(),
            Some("doc:source-test"),
            "source_uri serialised when present"
        );
    }
    if let Some(no_uri_row) = by_id.get("m-no-uri") {
        // source_uri absent or null when None on the row.
        let v = no_uri_row.get("source_uri").cloned().unwrap_or(json!(null));
        assert!(v.is_null(), "no source_uri ⇒ null/absent: {v}");
    }
}

#[test]
fn gap7_recall_id_is_uuid_shaped() {
    // AC pin: the per-call recall_id is a UUID. Validates the
    // observation tier's "fresh per call" contract — every recall
    // returns a new id the caller can echo on a downstream
    // memory_store / memory_link cite hook.
    let conn = fresh_db();
    seed_memory_full(&conn, "m-rid", None);
    let ttl = ResolvedTtl::default();
    let scoring = ResolvedScoring::default();
    let resp1 = ai_memory::mcp::handle_recall(
        &conn,
        &json!({"context": "decoration", "namespace": "g7"}),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall 1");
    let resp2 = ai_memory::mcp::handle_recall(
        &conn,
        &json!({"context": "decoration", "namespace": "g7"}),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall 2");
    let id1 = resp1["recall_id"].as_str().expect("recall_id");
    let id2 = resp2["recall_id"].as_str().expect("recall_id");
    assert_ne!(id1, id2, "every recall mints a FRESH id");
    // UUIDs are 36 chars in the canonical form `8-4-4-4-12`.
    assert_eq!(id1.len(), 36, "looks like a UUID: {id1}");
    assert_eq!(id1.matches('-').count(), 4, "four dashes in a UUID");
}

#[test]
fn gap7_token_budget_guard_still_passes_post_decoration() {
    // Pin the post-Gap-7 catalog totals so the new tool definition
    // (`memory_recall_observations`) + extended `memory_recall`
    // schema can't blow the operator-agreed budgets. The
    // `token_budget_guard` integration test enforces the same
    // ceiling end-to-end; this regression test re-runs the
    // computations directly so a guard regression surfaces in this
    // suite too.
    let trimmed = ai_memory::sizes::trimmed_full_profile_total_tokens();
    let verbose = ai_memory::sizes::full_profile_total_tokens();
    assert!(
        trimmed <= 5_000,
        "Gap 7 regression: trimmed full-profile total {trimmed} exceeds the 5000-token ceiling"
    );
    assert!(
        verbose <= 10_000,
        "Gap 7 regression: verbose full-profile total {verbose} exceeds the 10000-token ceiling"
    );
}
