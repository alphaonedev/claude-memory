// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7 Track H4 — `memory_verify` MCP tool integration tests.
//!
//! H4 formalises the `attest_level` enum (Unsigned / `SelfSigned` /
//! `PeerAttested`) that H2 (#566) and H3 (#572) already write as raw
//! strings to the `memory_links.attest_level` column. The new
//! `memory_verify(link_id)` MCP tool re-derives the canonical CBOR
//! payload from the stored row and re-checks the signature on demand,
//! returning `{signature_verified, attest_level, signed_by, signed_at}`.
//!
//! Scenarios pinned here:
//! 1. Unsigned link (no signature blob) → `signature_verified=false`,
//!    `attest_level="unsigned"`, `signed_by`/`signed_at` both `null`.
//! 2. Self-signed link (H2 outbound path) → `signature_verified=true`,
//!    `attest_level="self_signed"`, `signed_by`/`signed_at` populated.
//! 3. Tampered signature byte → `signature_verified=false`,
//!    `attest_level="unsigned"`.
//! 4. Peer-attested link (simulating H3's inbound path) →
//!    `signature_verified=true`, `attest_level="peer_attested"`.
//! 5. `link_id` composite form parses identically to explicit-args.
//! 6. Missing link tuple → handler returns Err.
//!
//! Hermeticity: each test sets `AI_MEMORY_KEY_DIR` to a per-test
//! tempdir before invoking the handler so the on-disk lookup never
//! touches the operator's real `~/.config/ai-memory/keys/`. The env
//! var is consumed by `crate::identity::keypair::default_key_dir`
//! (added in H4) which the production handler calls through.
//!
//! Concurrency: cross-test `env::set_var` is racy. The cases below
//! gate through a single `Mutex` so the suite runs serially even
//! when `cargo test` parallelises across test fns.

use std::sync::Mutex;

use ai_memory::db;
use ai_memory::identity::keypair as kp_mod;
use ai_memory::identity::sign;
use ai_memory::mcp;
use ai_memory::models::{self, MemoryLink};
use chrono::Utc;
use rusqlite::params;
use serde_json::json;
use tempfile::TempDir;

/// Single-process gate against parallel `env::set_var` racing. Every
/// test below acquires this mutex before touching `AI_MEMORY_KEY_DIR`.
static ENV_GUARD: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    conn: rusqlite::Connection,
    /// Dropped at the end of the test; keeps the DB file alive.
    #[allow(dead_code)]
    db_tmp: TempDir,
    /// Dropped at the end of the test; keeps the key dir alive.
    keys_tmp: TempDir,
    src_id: String,
    dst_id: String,
}

fn setup() -> Fixture {
    let db_tmp = TempDir::new().expect("db tempdir");
    let keys_tmp = TempDir::new().expect("keys tempdir");

    // Point the production-code lookup path at the per-test key dir.
    // SAFETY: caller acquired `ENV_GUARD` before invoking setup.
    unsafe {
        std::env::set_var("AI_MEMORY_KEY_DIR", keys_tmp.path());
    }

    let db_path = db_tmp.path().join("ai-memory.db");
    let conn = db::open(&db_path).expect("db::open");

    let src_id = seed(&conn, "h4-src");
    let dst_id = seed(&conn, "h4-dst");

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
        namespace: "h4-test".to_string(),
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
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    db::insert(conn, &mem).expect("db::insert")
}

fn read_attest_level(conn: &rusqlite::Connection, src: &str, dst: &str) -> Option<String> {
    conn.query_row(
        "SELECT attest_level FROM memory_links WHERE source_id = ?1 AND target_id = ?2",
        params![src, dst],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

// ---------------------------------------------------------------------------
// 1. Unsigned link → signature_verified=false, attest_level=unsigned
// ---------------------------------------------------------------------------
#[test]
fn unsigned_link_reports_unsigned_and_not_verified() {
    // PoisonError-tolerant lock: a panic in a sibling test would
    // otherwise cascade-fail every other test in the file.
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    // Insert an unsigned link via the H2 helper with `keypair=None` —
    // exactly what callers without a generated keypair already produce.
    let attest = db::create_link_signed(&f.conn, &f.src_id, &f.dst_id, "related_to", None)
        .expect("create_link_signed (unsigned)");
    assert_eq!(attest, "unsigned", "H2 must land an unsigned row");
    assert_eq!(
        read_attest_level(&f.conn, &f.src_id, &f.dst_id).as_deref(),
        Some("unsigned")
    );

    let body = mcp::handle_verify(
        &f.conn,
        &json!({
            "source_id": f.src_id,
            "target_id": f.dst_id,
            "relation": "related_to",
        }),
    )
    .expect("handle_verify Ok");

    assert_eq!(body["signature_verified"], json!(false));
    assert_eq!(body["attest_level"], json!("unsigned"));
    assert_eq!(body["signed_by"], json!(null));
    assert_eq!(body["signed_at"], json!(null));
}

// ---------------------------------------------------------------------------
// 2. Self-signed link → signature_verified=true, attest_level=self_signed
// ---------------------------------------------------------------------------
#[test]
fn self_signed_link_verifies_and_reports_self_signed() {
    // PoisonError-tolerant lock: a panic in a sibling test would
    // otherwise cascade-fail every other test in the file.
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    // Generate alice's keypair under the per-test key dir so the
    // production lookup path can find alice's public key on the verify
    // round-trip.
    let alice = kp_mod::generate("alice").unwrap();
    kp_mod::save(&alice, f.keys_tmp.path()).expect("save alice");

    // H2 outbound path: signs the link before insert. Observed_by is
    // alice (from the keypair); valid_from is "now".
    let attest = db::create_link_signed(&f.conn, &f.src_id, &f.dst_id, "related_to", Some(&alice))
        .expect("create_link_signed (self-signed)");
    assert_eq!(attest, "self_signed");

    let body = mcp::handle_verify(
        &f.conn,
        &json!({
            "source_id": f.src_id,
            "target_id": f.dst_id,
            "relation": "related_to",
        }),
    )
    .expect("handle_verify Ok");

    assert_eq!(body["signature_verified"], json!(true));
    assert_eq!(body["attest_level"], json!("self_signed"));
    assert_eq!(body["signed_by"], json!("alice"));
    assert!(
        body["signed_at"].is_string(),
        "signed_at must be RFC3339 string when verified, got: {:?}",
        body["signed_at"]
    );
}

// ---------------------------------------------------------------------------
// 3. Tampered signature byte → signature_verified=false
// ---------------------------------------------------------------------------
#[test]
fn tampered_signature_byte_does_not_verify() {
    // PoisonError-tolerant lock: a panic in a sibling test would
    // otherwise cascade-fail every other test in the file.
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    let alice = kp_mod::generate("alice").unwrap();
    kp_mod::save(&alice, f.keys_tmp.path()).expect("save alice");

    // Land a self-signed row first via the H2 path so the column
    // shape (observed_by, valid_from, attest_level) matches a real
    // signed insert.
    db::create_link_signed(&f.conn, &f.src_id, &f.dst_id, "related_to", Some(&alice))
        .expect("create_link_signed");

    // Now flip the first byte of the persisted signature blob (XOR
    // 0xFF — guaranteed-different value). The verifier must reject;
    // the `attest_level` column still says "self_signed" (writes never
    // go back) but the on-demand re-verification reports the truth.
    let original_sig: Vec<u8> = f
        .conn
        .query_row(
            "SELECT signature FROM memory_links \
             WHERE source_id = ?1 AND target_id = ?2",
            params![f.src_id, f.dst_id],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .expect("read signature back");
    assert_eq!(
        original_sig.len(),
        64,
        "Ed25519 signature must be 64 bytes, got {}",
        original_sig.len()
    );
    let mut tampered = original_sig.clone();
    tampered[0] ^= 0xFF;
    f.conn
        .execute(
            "UPDATE memory_links SET signature = ?3 \
             WHERE source_id = ?1 AND target_id = ?2",
            params![f.src_id, f.dst_id, tampered],
        )
        .expect("write tampered signature");

    let body = mcp::handle_verify(
        &f.conn,
        &json!({
            "source_id": f.src_id,
            "target_id": f.dst_id,
            "relation": "related_to",
        }),
    )
    .expect("handle_verify Ok (tampered → false, not Err)");

    assert_eq!(body["signature_verified"], json!(false));
    assert_eq!(
        body["attest_level"],
        json!("unsigned"),
        "tampered → on-demand verify reports unsigned regardless of stored column"
    );
    assert_eq!(body["signed_by"], json!(null));
    assert_eq!(body["signed_at"], json!(null));
}

// ---------------------------------------------------------------------------
// 4. Peer-attested link → signature_verified=true, attest_level=peer_attested
// ---------------------------------------------------------------------------
#[test]
fn peer_attested_link_verifies_and_reports_peer_attested() {
    // PoisonError-tolerant lock: a panic in a sibling test would
    // otherwise cascade-fail every other test in the file.
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    // Peer "bob" exists on a different host. We import only bob's
    // public key on this host — the H3 inbound path's enrolled-key
    // pattern.
    let bob = kp_mod::generate("bob").unwrap();
    let bob_pub = kp_mod::AgentKeypair {
        agent_id: "bob".to_string(),
        public: bob.public,
        private: None,
    };
    kp_mod::save_public_only(&bob_pub, f.keys_tmp.path()).expect("import bob.pub");

    // Bob signs a link off-host.
    let valid_from = Utc::now().to_rfc3339();
    let signable = sign::SignableLink {
        src_id: &f.src_id,
        dst_id: &f.dst_id,
        relation: "related_to",
        observed_by: Some("bob"),
        valid_from: Some(&valid_from),
        valid_until: None,
    };
    let sig = sign::sign(&bob, &signable).expect("bob signs");

    // The link arrives over federation; H3's inbound path persists it
    // with attest_level="peer_attested" after running verify(). We
    // call `create_link_inbound` directly with the matching attest
    // level — the same shape `handlers::sync_push` produces.
    let inbound = MemoryLink {
        source_id: f.src_id.clone(),
        target_id: f.dst_id.clone(),
        relation: ai_memory::models::MemoryLinkRelation::RelatedTo,
        created_at: Utc::now().to_rfc3339(),
        signature: Some(sig),
        observed_by: Some("bob".to_string()),
        valid_from: Some(valid_from.clone()),
        valid_until: None,
    };
    db::create_link_inbound(&f.conn, &inbound, "peer_attested").expect("create_link_inbound");
    assert_eq!(
        read_attest_level(&f.conn, &f.src_id, &f.dst_id).as_deref(),
        Some("peer_attested")
    );

    let body = mcp::handle_verify(
        &f.conn,
        &json!({
            "source_id": f.src_id,
            "target_id": f.dst_id,
            "relation": "related_to",
        }),
    )
    .expect("handle_verify Ok");

    assert_eq!(body["signature_verified"], json!(true));
    assert_eq!(body["attest_level"], json!("peer_attested"));
    assert_eq!(body["signed_by"], json!("bob"));
    assert_eq!(body["signed_at"], json!(valid_from));
}

// ---------------------------------------------------------------------------
// 5. link_id composite form parses identically to explicit-args form
// ---------------------------------------------------------------------------
#[test]
fn link_id_composite_form_resolves_same_link() {
    // PoisonError-tolerant lock: a panic in a sibling test would
    // otherwise cascade-fail every other test in the file.
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    db::create_link_signed(&f.conn, &f.src_id, &f.dst_id, "related_to", None)
        .expect("create_link_signed");

    let composite = format!("{}--related_to-->{}", f.src_id, f.dst_id);
    let body =
        mcp::handle_verify(&f.conn, &json!({ "link_id": composite })).expect("handle_verify Ok");

    assert_eq!(body["signature_verified"], json!(false));
    assert_eq!(body["attest_level"], json!("unsigned"));
}

// ---------------------------------------------------------------------------
// 6. Missing link tuple → handler returns Err
// ---------------------------------------------------------------------------
#[test]
fn missing_link_returns_err() {
    // PoisonError-tolerant lock: a panic in a sibling test would
    // otherwise cascade-fail every other test in the file.
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    // Look up a relation we never inserted on this fixture.
    let err = mcp::handle_verify(
        &f.conn,
        &json!({
            "source_id": f.src_id,
            "target_id": f.dst_id,
            "relation": "supersedes",
        }),
    )
    .unwrap_err();
    assert!(
        err.contains("link not found"),
        "err message should name the failure mode: {err}"
    );
}
