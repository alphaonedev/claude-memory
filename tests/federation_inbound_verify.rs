// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7 Track H3 — federation inbound link verification.
//!
//! These tests model the two-host federation flow without spawning the
//! HTTP daemon. The daemon-spawn integration tests in
//! `tests/integration.rs` are the right place for full HTTP wire
//! coverage, but they cost O(seconds) per test and serialise on a
//! single child process; the verification primitive is small enough to
//! exercise with a direct `db::create_link_inbound` call after running
//! the same `verify::verify` decision the `sync_push` handler runs.
//!
//! Scenarios covered:
//! 1. Happy path — peer A signs, peer B has A's pubkey enrolled →
//!    accepts as `peer_attested`.
//! 2. Tampered signature — flipped sig byte → `Tampered` → reject.
//! 3. Tampered link content — flipped relation byte → CBOR re-encoding
//!    diverges → `Tampered` → reject.
//! 4. No public key — peer A signs, peer B has no enrolled key for A
//!    → accepted with `attest_level = "unsigned"`.

use ai_memory::db;
use ai_memory::identity::keypair as kp_mod;
use ai_memory::identity::sign;
use ai_memory::identity::verify;
use ai_memory::models::{self, MemoryLink};
use chrono::Utc;
use tempfile::TempDir;

/// Two-host topology: each host has its own DB and (separately) its
/// own on-disk key directory. The signer (`alice`) lives on host A;
/// host B optionally enrols alice's public key under its own keys dir
/// to simulate `identity import` after a peer onboarding handshake.
struct TwoHosts {
    /// Host B's database — receives inbound links.
    receiver_db: rusqlite::Connection,
    /// Host B's key dir — what `verify::lookup_peer_public_key_in` reads.
    receiver_keys: TempDir,
    /// Host A's signing keypair (private + public).
    alice: kp_mod::AgentKeypair,
    /// IDs of two memories that exist on host B so link inserts pass
    /// the FK check inside `db::create_link_inbound`.
    src_id: String,
    dst_id: String,
    // Hold the receiver tempdir so the file path stays valid for the test.
    #[allow(dead_code)]
    receiver_tmp: TempDir,
}

fn setup() -> TwoHosts {
    let receiver_tmp = TempDir::new().expect("receiver tempdir");
    let receiver_keys = TempDir::new().expect("receiver keys tempdir");

    let db_path = receiver_tmp.path().join("ai-memory.db");
    let receiver_db = db::open(&db_path).expect("db::open");

    // Seed two memories on the receiver so the link's FK check passes.
    let src_id = seed(&receiver_db, "src");
    let dst_id = seed(&receiver_db, "dst");

    let alice = kp_mod::generate("alice").expect("generate alice");

    TwoHosts {
        receiver_db,
        receiver_keys,
        alice,
        src_id,
        dst_id,
        receiver_tmp,
    }
}

fn seed(conn: &rusqlite::Connection, title: &str) -> String {
    let now = Utc::now().to_rfc3339();
    let id = uuid::Uuid::new_v4().to_string();
    let mem = models::Memory {
        id: id.clone(),
        tier: models::Tier::Mid,
        namespace: "h3-test".to_string(),
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

/// Mimic the federation inbound decision logic from `handlers::sync_push`
/// using the explicit-key-dir variant of `lookup_peer_public_key`.
/// Returns the `attest_level` string the handler would persist (or
/// `Err` to mean the handler would skip this link with a warn log).
fn decide_attest_level(
    link: &MemoryLink,
    receiver_keys: &std::path::Path,
) -> Result<&'static str, verify::VerifyError> {
    match (link.signature.as_deref(), link.observed_by.as_deref()) {
        (Some(sig_bytes), Some(observed_by)) => {
            match verify::lookup_peer_public_key_in(observed_by, receiver_keys) {
                Some(pubkey) => {
                    let signable = sign::SignableLink {
                        src_id: &link.source_id,
                        dst_id: &link.target_id,
                        relation: &link.relation,
                        observed_by: Some(observed_by),
                        valid_from: link.valid_from.as_deref(),
                        valid_until: link.valid_until.as_deref(),
                    };
                    verify::verify(&pubkey, &signable, sig_bytes)?;
                    Ok("peer_attested")
                }
                None => Ok("unsigned"),
            }
        }
        _ => Ok("unsigned"),
    }
}

fn build_link(
    h: &TwoHosts,
    relation: &str,
    observed_by: Option<&str>,
    valid_from: Option<&str>,
) -> MemoryLink {
    MemoryLink {
        source_id: h.src_id.clone(),
        target_id: h.dst_id.clone(),
        relation: relation.to_string(),
        created_at: Utc::now().to_rfc3339(),
        signature: None,
        observed_by: observed_by.map(str::to_string),
        valid_from: valid_from.map(str::to_string),
        valid_until: None,
    }
}

fn read_attest_level(conn: &rusqlite::Connection, src: &str, dst: &str) -> Option<String> {
    conn.query_row(
        "SELECT attest_level FROM memory_links WHERE source_id = ?1 AND target_id = ?2",
        rusqlite::params![src, dst],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

#[test]
fn happy_path_peer_attested() {
    // Peer A signs, peer B has A's pubkey enrolled → accepts as
    // peer_attested.
    let h = setup();

    // Host B operator imports alice's public key.
    let alice_pub = kp_mod::AgentKeypair {
        agent_id: h.alice.agent_id.clone(),
        public: h.alice.public,
        private: None,
    };
    kp_mod::save_public_only(&alice_pub, h.receiver_keys.path()).unwrap();

    // Alice signs a link on host A.
    let valid_from = Utc::now().to_rfc3339();
    let mut link = build_link(&h, "related_to", Some(&h.alice.agent_id), Some(&valid_from));
    let signable = sign::SignableLink {
        src_id: &link.source_id,
        dst_id: &link.target_id,
        relation: &link.relation,
        observed_by: Some(&h.alice.agent_id),
        valid_from: Some(&valid_from),
        valid_until: None,
    };
    link.signature = Some(sign::sign(&h.alice, &signable).unwrap());

    // Run the decision + insert.
    let attest =
        decide_attest_level(&link, h.receiver_keys.path()).expect("happy path must not error out");
    assert_eq!(attest, "peer_attested");
    db::create_link_inbound(&h.receiver_db, &link, attest).expect("inbound insert");

    // Confirm the row landed with the right attest level.
    let stored = read_attest_level(&h.receiver_db, &h.src_id, &h.dst_id);
    assert_eq!(stored.as_deref(), Some("peer_attested"));
}

#[test]
fn tampered_signature_byte_is_rejected() {
    // Peer A signs; flip a single bit in the sig before B sees it.
    // Decision logic must return Tampered; row must not land.
    let h = setup();

    let alice_pub = kp_mod::AgentKeypair {
        agent_id: h.alice.agent_id.clone(),
        public: h.alice.public,
        private: None,
    };
    kp_mod::save_public_only(&alice_pub, h.receiver_keys.path()).unwrap();

    let valid_from = Utc::now().to_rfc3339();
    let mut link = build_link(&h, "related_to", Some(&h.alice.agent_id), Some(&valid_from));
    let signable = sign::SignableLink {
        src_id: &link.source_id,
        dst_id: &link.target_id,
        relation: &link.relation,
        observed_by: Some(&h.alice.agent_id),
        valid_from: Some(&valid_from),
        valid_until: None,
    };
    let mut sig = sign::sign(&h.alice, &signable).unwrap();
    sig[10] ^= 0xff;
    link.signature = Some(sig);

    let err = decide_attest_level(&link, h.receiver_keys.path()).unwrap_err();
    assert_eq!(err, verify::VerifyError::Tampered);

    // Receiver-side handler skips on Tampered; confirm by NOT inserting
    // and asserting the row is absent.
    let stored = read_attest_level(&h.receiver_db, &h.src_id, &h.dst_id);
    assert!(stored.is_none(), "tampered link must not land");
}

#[test]
fn tampered_link_content_is_rejected() {
    // Sign with relation=related_to but ship relation=supersedes.
    // CBOR re-encoding diverges → Ed25519 rejects.
    let h = setup();

    let alice_pub = kp_mod::AgentKeypair {
        agent_id: h.alice.agent_id.clone(),
        public: h.alice.public,
        private: None,
    };
    kp_mod::save_public_only(&alice_pub, h.receiver_keys.path()).unwrap();

    let valid_from = Utc::now().to_rfc3339();
    // Sign over relation=related_to.
    let signable_signed = sign::SignableLink {
        src_id: &h.src_id,
        dst_id: &h.dst_id,
        relation: "related_to",
        observed_by: Some(&h.alice.agent_id),
        valid_from: Some(&valid_from),
        valid_until: None,
    };
    let sig = sign::sign(&h.alice, &signable_signed).unwrap();

    // Ship relation=supersedes — verifier re-derives different bytes.
    let mut link = build_link(&h, "supersedes", Some(&h.alice.agent_id), Some(&valid_from));
    link.signature = Some(sig);

    let err = decide_attest_level(&link, h.receiver_keys.path()).unwrap_err();
    assert_eq!(err, verify::VerifyError::Tampered);

    let stored = read_attest_level(&h.receiver_db, &h.src_id, &h.dst_id);
    assert!(stored.is_none(), "mutated link content must not land");
}

#[test]
fn no_public_key_for_observed_by_lands_as_unsigned() {
    // Peer A signs; receiver B has NO enrolled key for alice. We don't
    // reject — federation back-compat is preserved by accept-and-flag.
    let h = setup();
    // NB: receiver_keys directory is empty; no `save_public_only` call.

    let valid_from = Utc::now().to_rfc3339();
    let mut link = build_link(&h, "related_to", Some(&h.alice.agent_id), Some(&valid_from));
    let signable = sign::SignableLink {
        src_id: &link.source_id,
        dst_id: &link.target_id,
        relation: &link.relation,
        observed_by: Some(&h.alice.agent_id),
        valid_from: Some(&valid_from),
        valid_until: None,
    };
    link.signature = Some(sign::sign(&h.alice, &signable).unwrap());

    let attest =
        decide_attest_level(&link, h.receiver_keys.path()).expect("no-key path must accept");
    assert_eq!(
        attest, "unsigned",
        "unknown observed_by must accept as unsigned, not reject"
    );

    db::create_link_inbound(&h.receiver_db, &link, attest).expect("inbound insert");
    let stored = read_attest_level(&h.receiver_db, &h.src_id, &h.dst_id);
    assert_eq!(stored.as_deref(), Some("unsigned"));
}

#[test]
fn legacy_unsigned_link_lands_as_unsigned() {
    // Pre-H3 peer ships a link with no signature / no observed_by →
    // `decide_attest_level` returns "unsigned" without invoking verify.
    let h = setup();
    let link = build_link(&h, "related_to", None, None);
    assert!(link.signature.is_none());

    let attest =
        decide_attest_level(&link, h.receiver_keys.path()).expect("legacy path must accept");
    assert_eq!(attest, "unsigned");

    db::create_link_inbound(&h.receiver_db, &link, attest).expect("inbound insert");
    let stored = read_attest_level(&h.receiver_db, &h.src_id, &h.dst_id);
    assert_eq!(stored.as_deref(), Some("unsigned"));
}
