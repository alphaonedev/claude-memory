// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown)]
//! v0.7.0 #628 H2 (review blocker H5) — `invalidate_link` must not
//! silently corrupt a previously self-signed link.
//!
//! Pre-fix behaviour: `db::invalidate_link` mutated `valid_until` on
//! the row WITHOUT re-signing or clearing the signature. Because
//! `valid_until` is one of the six fields the H2 outbound signer
//! commits to (see [`ai_memory::identity::sign::SignableLink`]), every
//! previously self-signed link silently flipped to
//! `signature_verified=false / attest_level=unsigned` the moment it
//! was invalidated — legitimate supersession became indistinguishable
//! from tampering.
//!
//! Post-fix behaviour:
//!
//! 1. The `signature` column is NULLed and `attest_level` resets to
//!    `unsigned`. A future `memory_verify` honestly reports "no
//!    signature on this row" instead of "signature mismatch".
//! 2. A `memory_link.invalidated` row appears in `signed_events`,
//!    cryptographically attesting the supersession event. The
//!    `payload_hash` binds to the post-supersession canonical CBOR
//!    so an auditor can replay both the original `memory_link.created`
//!    row AND the matching `memory_link.invalidated` row to prove
//!    intent.

use std::sync::Mutex;

use ai_memory::db;
use ai_memory::identity::keypair as kp_mod;
use ai_memory::models;
use ai_memory::signed_events;
use chrono::Utc;
use rusqlite::params;
use tempfile::TempDir;

/// Keypair-loading helpers consult `AI_MEMORY_KEY_DIR`. The H6 e2e
/// suite already uses a process-wide mutex to serialise env writes
/// here; we mirror the same pattern.
static ENV_GUARD: Mutex<()> = Mutex::new(());

struct Fixture {
    conn: rusqlite::Connection,
    #[allow(dead_code)]
    db_tmp: TempDir,
    #[allow(dead_code)]
    keys_tmp: TempDir,
    src_id: String,
    dst_id: String,
}

fn setup() -> Fixture {
    let db_tmp = TempDir::new().expect("db tempdir");
    let keys_tmp = TempDir::new().expect("keys tempdir");

    // SAFETY: caller acquired ENV_GUARD before invoking setup.
    unsafe {
        std::env::set_var("AI_MEMORY_KEY_DIR", keys_tmp.path());
    }

    let db_path = db_tmp.path().join("ai-memory.db");
    let conn = db::open(&db_path).expect("db::open");

    let src_id = seed(&conn, "h2-src");
    let dst_id = seed(&conn, "h2-dst");

    Fixture {
        conn,
        db_tmp,
        keys_tmp,
        src_id,
        dst_id,
    }
}

fn seed(conn: &rusqlite::Connection, title: &str) -> String {
    let now = Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();
    let mem = models::Memory {
        id: id.clone(),
        tier: models::Tier::Mid,
        namespace: "h2-test".to_string(),
        title: title.to_string(),
        content: "x".to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "import".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: models::default_metadata(),
    };
    db::insert(conn, &mem).expect("db::insert")
}

fn read_signature_and_attest(
    conn: &rusqlite::Connection,
    src: &str,
    dst: &str,
) -> (Option<Vec<u8>>, Option<String>) {
    conn.query_row(
        "SELECT signature, attest_level FROM memory_links \
         WHERE source_id = ?1 AND target_id = ?2",
        params![src, dst],
        |row| {
            Ok((
                row.get::<_, Option<Vec<u8>>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        },
    )
    .expect("link row must exist")
}

#[test]
fn signed_link_invalidation_clears_signature_and_audits_event() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    // 1. Sign a link with Alice's keypair via the H2 outbound path.
    let alice = kp_mod::generate("alice").expect("generate");
    kp_mod::save(&alice, f.keys_tmp.path()).expect("save");

    let attest = db::create_link_signed(&f.conn, &f.src_id, &f.dst_id, "supersedes", Some(&alice))
        .expect("create_link_signed");
    assert_eq!(attest, "self_signed", "H2 must self-sign with a keypair");

    let (sig_before, attest_before) = read_signature_and_attest(&f.conn, &f.src_id, &f.dst_id);
    assert!(
        sig_before.as_ref().map(Vec::len) == Some(64),
        "Ed25519 signature must be 64 bytes pre-invalidate, got: {:?}",
        sig_before.as_ref().map(Vec::len)
    );
    assert_eq!(attest_before.as_deref(), Some("self_signed"));

    // Snapshot signed_events count BEFORE invalidate so we can assert
    // exactly one new audit row appears (the production
    // `db::create_link_signed` does not yet auto-append on the
    // create-side, so the table starts empty for this fixture).
    let before_audit: usize = signed_events::list_signed_events(&f.conn, Some("alice"), 1000, 0)
        .expect("list pre-invalidate")
        .len();

    // 2. Invalidate. Per the v0.7.0 #628 H5 fix, this must:
    //    - NULL the signature column
    //    - reset attest_level to "unsigned"
    //    - append a memory_link.invalidated row to signed_events
    let invalidated_at = "2026-05-06T12:00:00+00:00";
    let res = db::invalidate_link(
        &f.conn,
        &f.src_id,
        &f.dst_id,
        "supersedes",
        Some(invalidated_at),
    )
    .expect("invalidate_link Ok")
    .expect("link must exist");
    assert_eq!(res.valid_until, invalidated_at);

    // 3. Assert the signature column is now NULL and attest_level
    //    reflects "unsigned" — a future memory_verify will report
    //    "no signature on this row" rather than "signature mismatch".
    let (sig_after, attest_after) = read_signature_and_attest(&f.conn, &f.src_id, &f.dst_id);
    assert!(
        sig_after.is_none(),
        "signature column must be NULL after invalidating a signed link, got: {sig_after:?}"
    );
    assert_eq!(
        attest_after.as_deref(),
        Some("unsigned"),
        "attest_level must reset to 'unsigned' alongside the NULLed signature"
    );

    // 4. The audit chain MUST carry the supersession event.
    let listed = signed_events::list_signed_events(&f.conn, Some("alice"), 1000, 0)
        .expect("list post-invalidate");
    assert_eq!(
        listed.len(),
        before_audit + 1,
        "exactly one new signed_events row must be appended"
    );
    let invalidated = listed
        .iter()
        .find(|e| e.event_type == "memory_link.invalidated")
        .expect("memory_link.invalidated audit row must exist");
    assert_eq!(invalidated.agent_id, "alice");
    assert_eq!(
        invalidated.payload_hash.len(),
        32,
        "audit payload_hash must be SHA-256 (32 bytes)"
    );
    // The audit row preserves the PRIOR signature so an auditor can
    // bind it back to the original `memory_link.created` row.
    assert_eq!(
        invalidated.signature, sig_before,
        "audit signature blob must mirror the pre-invalidate signature \
         so the auditor can join back to the original signing event"
    );
}

#[test]
fn unsigned_link_invalidation_does_not_create_audit_row() {
    // Negative test: an unsigned link being invalidated must not
    // append a phantom audit row — the fix is scoped to PREVIOUSLY
    // SIGNED rows. Otherwise the audit table fills with empty
    // events from the v0.6.x unsigned-link backlog.
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    // No keypair on this path → unsigned link.
    let attest = db::create_link_signed(&f.conn, &f.src_id, &f.dst_id, "related_to", None)
        .expect("create unsigned");
    assert_eq!(attest, "unsigned");

    let res = db::invalidate_link(&f.conn, &f.src_id, &f.dst_id, "related_to", None)
        .expect("invalidate")
        .expect("found");
    assert!(!res.valid_until.is_empty());

    // No audit row should be appended for an unsigned-link
    // invalidation — the audit chain is signature-bearing only.
    let listed = signed_events::list_signed_events(&f.conn, None, 1000, 0).expect("list");
    assert!(
        listed
            .iter()
            .all(|e| e.event_type != "memory_link.invalidated"),
        "no memory_link.invalidated audit row should appear for an unsigned-link invalidation, \
         got: {listed:?}"
    );
}
