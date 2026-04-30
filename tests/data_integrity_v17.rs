// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::doc_markdown)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::single_char_pattern)]

//! v0.6.3.1 Phase P2 — Data-integrity hardening acceptance tests.
//!
//! Per `REMEDIATIONv0631` §"Phase P2", the migration v17 + handler updates
//! must close G4 (mixed embedding dims silently tolerated), G5 (archive
//! lossy + restore resets), G6 (UNIQUE(title,namespace) silent merge), and
//! G13 (f32 endianness — magic byte header). Every acceptance test the
//! charter calls out is implemented here and runs against an in-memory
//! `SQLite` DB so the suite stays hermetic.

use ai_memory::db;
use ai_memory::embeddings::{
    EMBEDDING_HEADER_BE_F32, EMBEDDING_HEADER_LE_F32, decode_embedding_blob, encode_embedding_blob,
};
use ai_memory::models::{Memory, Tier};
use rusqlite::params;
use std::path::Path;

fn open_test_db() -> rusqlite::Connection {
    db::open(Path::new(":memory:")).expect("open in-memory DB")
}

fn make_memory(title: &str, ns: &str, tier: Tier) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: tier.clone(),
        namespace: ns.to_string(),
        title: title.to_string(),
        content: format!("Content for {title}"),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: tier
            .default_ttl_secs()
            .map(|s| (chrono::Utc::now() + chrono::Duration::seconds(s)).to_rfc3339()),
        metadata: serde_json::json!({}),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// G5 — archive_preserves_embedding_and_tier_on_restore
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn archive_preserves_embedding_and_tier_on_restore() {
    let conn = open_test_db();
    let mut mem = make_memory("integrity-G5", "p2/test", Tier::Mid);
    let stored_expires_at = mem.expires_at.clone();
    db::insert(&conn, &mem).expect("insert");

    // Set an embedding so we can verify it round-trips through the archive.
    let embedding: Vec<f32> = (0..8).map(|i| (i as f32) * 0.125).collect();
    db::set_embedding(&conn, &mem.id, &embedding).expect("set embedding");

    // Archive it.
    let moved = db::archive_memory(&conn, &mem.id, Some("p2-test")).expect("archive");
    assert!(moved, "expected the row to move into archive");

    // Restore it.
    let restored = db::restore_archived(&conn, &mem.id).expect("restore");
    assert!(restored, "expected the row to restore");

    // The restored row must keep its original tier (Mid, not Long), original
    // expires_at, AND embedding.
    let after = db::get(&conn, &mem.id)
        .expect("get after restore")
        .expect("row present");
    assert_eq!(
        after.tier.as_str(),
        "mid",
        "restore must preserve original tier (was Mid; pre-v17 reset to Long)"
    );
    assert_eq!(
        after.expires_at, stored_expires_at,
        "restore must preserve original expires_at (pre-v17 reset to NULL)"
    );

    let emb_back = db::get_embedding(&conn, &mem.id)
        .expect("get embedding")
        .expect("embedding round-trips");
    assert_eq!(
        emb_back.len(),
        embedding.len(),
        "embedding length preserved"
    );
    for (i, (a, b)) in emb_back.iter().zip(embedding.iter()).enumerate() {
        assert!(
            (a - b).abs() < 1e-6,
            "embedding[{i}]: restored {a} != original {b}"
        );
    }

    // Avoid unused-mut warning under clippy::pedantic.
    mem.title = String::new();
}

// ─────────────────────────────────────────────────────────────────────────
// G4 — mixed_dim_write_rejected_after_first_dim_established
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn mixed_dim_write_rejected_after_first_dim_established() {
    let conn = open_test_db();
    let m_a = make_memory("dim-a", "g4/test", Tier::Long);
    let m_b = make_memory("dim-b", "g4/test", Tier::Long);
    db::insert(&conn, &m_a).expect("insert a");
    db::insert(&conn, &m_b).expect("insert b");

    // First write establishes the namespace dim at 4.
    let v4 = vec![0.1_f32, 0.2, 0.3, 0.4];
    db::set_embedding(&conn, &m_a.id, &v4).expect("first write succeeds");

    // Second write into the SAME namespace at a different dim must fail
    // with the typed EmbeddingDimMismatch error (G4).
    let v8: Vec<f32> = (0..8).map(|i| (i as f32) * 0.1).collect();
    let err = db::set_embedding(&conn, &m_b.id, &v8)
        .expect_err("second write at different dim must error");
    let msg = err.to_string();
    assert!(
        msg.contains("dim mismatch")
            || msg.contains("dim_mismatch")
            || msg.contains("4")
            || msg.contains("8"),
        "expected typed dim-mismatch error, got: {msg}"
    );

    // Same dim should still succeed.
    db::set_embedding(&conn, &m_b.id, &v4).expect("matching dim succeeds");

    // dim_violations stat reports zero on a clean v17 store.
    assert_eq!(
        db::dim_violations(&conn).expect("dim_violations"),
        0,
        "no violations expected after only valid writes"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// G13 — legacy_no_header_embedding_still_readable
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn legacy_no_header_embedding_still_readable() {
    let conn = open_test_db();
    let m = make_memory("legacy-g13", "g13/test", Tier::Long);
    db::insert(&conn, &m).expect("insert");

    // Simulate a pre-v17 row by writing the BLOB manually with the legacy
    // unheaded LE-f32 layout, bypassing set_embedding.
    let v = vec![1.0_f32, -0.5, 0.25];
    let raw: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
    conn.execute(
        "UPDATE memories SET embedding = ?1 WHERE id = ?2",
        params![raw, m.id],
    )
    .expect("write legacy blob");

    // The reader must tolerate the missing header.
    let back = db::get_embedding(&conn, &m.id)
        .expect("read legacy embedding")
        .expect("present");
    assert_eq!(back, v, "legacy LE-f32 BLOB decodes verbatim");

    // The codec should also decode it directly.
    let direct = decode_embedding_blob(&raw).expect("legacy decode");
    assert_eq!(direct, v);
}

// ─────────────────────────────────────────────────────────────────────────
// G13 — endianness_corruption_detected_on_be_byte
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn endianness_corruption_detected_on_be_byte() {
    let conn = open_test_db();
    let m = make_memory("endian-g13", "g13/test", Tier::Long);
    db::insert(&conn, &m).expect("insert");

    // Hand-craft a BE-headed payload (simulating a payload from a
    // future BE-arch peer). The reader must reject it with a typed
    // error rather than silently producing a wrong cosine score.
    let v = 1.0_f32;
    let mut blob = vec![EMBEDDING_HEADER_BE_F32];
    blob.extend_from_slice(&v.to_be_bytes());
    conn.execute(
        "UPDATE memories SET embedding = ?1 WHERE id = ?2",
        params![blob, m.id],
    )
    .expect("write BE-headed blob");

    let err = db::get_embedding(&conn, &m.id).expect_err("BE header must error on read");
    let msg = err.to_string();
    assert!(
        msg.contains("big-endian") || msg.contains("0x02"),
        "expected typed BE-unsupported error, got: {msg}"
    );

    // The codec should also reject it in isolation.
    let direct_err = decode_embedding_blob(&blob).expect_err("codec rejects BE");
    assert!(format!("{direct_err}").contains("big-endian"));

    // Confirm a properly-headed LE payload at the same row still works.
    let good = vec![1.0_f32, 2.0, 3.0];
    let good_blob = encode_embedding_blob(&good);
    assert_eq!(good_blob[0], EMBEDDING_HEADER_LE_F32);
    conn.execute(
        "UPDATE memories SET embedding = ?1 WHERE id = ?2",
        params![good_blob, m.id],
    )
    .expect("overwrite with good blob");
    let back = db::get_embedding(&conn, &m.id)
        .expect("read good")
        .expect("present");
    assert_eq!(back, good);
}

// ─────────────────────────────────────────────────────────────────────────
// G6 — store_on_conflict_error_returns_409
//
// We exercise G6 at the database helper level (find_by_title_namespace
// + next_versioned_title) since the MCP/HTTP surfaces sit on top of these.
// The MCP-side capability negotiation is unit-tested in src/mcp.rs.
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn store_on_conflict_error_returns_409() {
    // Simulate the on_conflict='error' path: a second write at the same
    // (title, namespace) must be detected before any INSERT runs so the
    // handler can map it to a 409 Conflict.
    let conn = open_test_db();
    let m = make_memory("conflict-g6", "g6/test", Tier::Long);
    db::insert(&conn, &m).expect("first insert");

    let existing = db::find_by_title_namespace(&conn, &m.title, &m.namespace)
        .expect("lookup")
        .expect("first row exists");
    assert_eq!(existing, m.id);

    // Negative case — different title returns None.
    let not_found =
        db::find_by_title_namespace(&conn, "no-such-title", &m.namespace).expect("lookup");
    assert!(not_found.is_none());

    // version mode helper picks a free suffix.
    let next = db::next_versioned_title(&conn, &m.title, &m.namespace).expect("next");
    assert_eq!(next, format!("{} (2)", m.title));

    // Insert that suffix, then ask again — should jump to (3).
    let mut m2 = make_memory(&next, &m.namespace, Tier::Long);
    m2.id = uuid::Uuid::new_v4().to_string();
    db::insert(&conn, &m2).expect("second insert at (2)");
    let next2 = db::next_versioned_title(&conn, &m.title, &m.namespace).expect("next2");
    assert_eq!(next2, format!("{} (3)", m.title));
}

// ─────────────────────────────────────────────────────────────────────────
// G6 — store_on_conflict_merge_preserves_v063_behavior
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn store_on_conflict_merge_preserves_v063_behavior() {
    // Merge mode is the legacy behaviour: db::insert with the same
    // (title, namespace) updates the existing row in place. Verify the
    // upsert keeps the original id, applies max(priority), max(confidence),
    // and never downgrades the tier.
    let conn = open_test_db();
    let mut a = make_memory("merge-g6", "g6/merge", Tier::Mid);
    a.priority = 3;
    a.confidence = 0.5;
    let id_a = db::insert(&conn, &a).expect("first insert");
    assert_eq!(id_a, a.id);

    // Second write — same (title, namespace), bumped priority + tier=long.
    let mut b = make_memory("merge-g6", "g6/merge", Tier::Long);
    b.id = uuid::Uuid::new_v4().to_string(); // new id, but upsert ignores it
    b.priority = 9;
    b.confidence = 0.9;
    let id_b = db::insert(&conn, &b).expect("second insert (merge)");
    assert_eq!(id_b, id_a, "merge must reuse the original id");

    let after = db::get(&conn, &id_a).expect("get").expect("present");
    assert_eq!(after.tier.as_str(), "long", "tier promoted to long");
    assert_eq!(after.priority, 9, "max(priority) applied");
    assert!(
        (after.confidence - 0.9).abs() < 1e-6,
        "max(confidence) applied"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Bonus: the migration leaves dim_violations==0 on a fresh DB.
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn fresh_db_reports_zero_dim_violations() {
    let conn = open_test_db();
    let m = make_memory("clean", "stats/test", Tier::Long);
    db::insert(&conn, &m).expect("insert");
    let v = vec![0.0_f32; 16];
    db::set_embedding(&conn, &m.id, &v).expect("set embedding");

    let stats = db::stats(&conn, Path::new(":memory:")).expect("stats");
    assert_eq!(
        stats.dim_violations, 0,
        "no violations on a clean v17 store"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Bonus: stats surfaces dim_violations when a legacy row is corrupt-by-design.
// (The doctor/P7 surface is downstream of this.)
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn stats_surfaces_dim_violations_for_corrupt_row() {
    let conn = open_test_db();
    let m = make_memory("corrupt", "stats/test", Tier::Long);
    db::insert(&conn, &m).expect("insert");

    // Hand-craft a legacy 4n-byte BLOB whose declared dim disagrees.
    let raw: Vec<u8> = (0..32_u8).collect(); // 32 bytes -> 8 floats by length
    conn.execute(
        "UPDATE memories SET embedding = ?1, embedding_dim = 4 WHERE id = ?2",
        params![raw, m.id],
    )
    .expect("write mismatched row");

    let stats = db::stats(&conn, Path::new(":memory:")).expect("stats");
    assert!(
        stats.dim_violations >= 1,
        "expected at least 1 violation, got {}",
        stats.dim_violations
    );
}
