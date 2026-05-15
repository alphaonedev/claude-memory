// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7 Track H6 — identity end-to-end test.
//!
//! This file ties H1-H5 together into a single end-to-end assertion
//! that the OSS attestation chain is wired correctly from key
//! generation through audit-row append. H1-H5 each landed unit and
//! integration coverage for their own seam:
//!
//! - H1 ([`ai_memory::identity::keypair`]) — generate / save / load /
//!   list / export-pub.
//! - H2 ([`ai_memory::identity::sign`] +
//!   [`ai_memory::db::create_link_signed`]) — outbound canonical
//!   CBOR + Ed25519 over the six signable link fields, persisted to
//!   the previously-dead `signature` BLOB column.
//! - H3 ([`ai_memory::identity::verify`]) — inbound mirror; re-derive
//!   canonical bytes and verify against the peer's enrolled public key.
//! - H4 ([`ai_memory::mcp::handle_verify`] +
//!   [`ai_memory::models::AttestLevel`]) — the `memory_verify` MCP
//!   tool that callers invoke on demand.
//! - H5 ([`ai_memory::signed_events`]) — append-only audit table whose
//!   `payload_hash` commits to the same canonical CBOR bytes the H2
//!   signer hashed.
//!
//! H6 is the proof that **the chain composes**. Each scenario walks one
//! end-to-end path through the substrate and asserts the cross-seam
//! invariant, not just the local one a single H? test would.
//!
//! # Scenarios
//!
//! 1. `keypair_generated_via_library_round_trips` — H1 lifecycle smoke
//!    (generate + save + load) under the per-test key dir.
//! 2. `self_signed_link_persists_signature_column` — H2 happy path:
//!    `create_link_signed` with a keypair leaves the `signature`
//!    column non-NULL.
//! 3. `signed_events_payload_hash_matches_canonical_cbor` — H5
//!    invariant: an audit row appended for the same link has
//!    `payload_hash == SHA-256(canonical_cbor(signable))` AND the
//!    signature blob mirrors the row's `memory_links.signature`.
//! 4. `memory_verify_self_signed_happy_path` — H4 on-demand re-verify
//!    of the H2 row reports `signature_verified=true` +
//!    `attest_level=self_signed`.
//! 5. `memory_verify_tampered_content_returns_false` — negative test:
//!    flip the link content in the DB, re-call `memory_verify`, assert
//!    `signature_verified=false`.
//! 6. `peer_attested_inbound_link_verifies` — federation path: peer B
//!    has its public key enrolled, signs a link off-host, the local DB
//!    persists it via `create_link_inbound("peer_attested")`,
//!    `memory_verify` reports `signature_verified=true` +
//!    `attest_level=peer_attested`.
//! 7. `inbound_link_with_no_enrolled_pubkey_lands_unsigned` — peer C
//!    is not enrolled; the receiver-side decision drops to "unsigned"
//!    and `memory_verify` reflects that.
//! 8. `tampered_signature_byte_does_not_verify` — negative test on the
//!    signature blob itself: flip one byte, re-verify, assert false.
//!
//! # Hermeticity
//!
//! Each test sets `AI_MEMORY_KEY_DIR` to a per-test tempdir before
//! invoking any handler that loads a public key from disk. The env var
//! is consumed by [`ai_memory::identity::keypair::default_key_dir`]
//! (env-override added in H4) which the production
//! `mcp::handle_verify` calls through. Cross-test `env::set_var` is
//! racy; tests acquire a single-process [`Mutex`] before touching the
//! var so the suite runs serially even when `cargo test` parallelises
//! across test fns.

use std::sync::Mutex;

use ai_memory::db;
use ai_memory::identity::keypair as kp_mod;
use ai_memory::identity::sign;
use ai_memory::mcp;
use ai_memory::models::{self, MemoryLink};
use ai_memory::signed_events::{self, SignedEvent, payload_hash};
use chrono::Utc;
use rusqlite::params;
use serde_json::json;
use tempfile::TempDir;

/// Single-process gate against parallel `env::set_var` racing. Every
/// test below acquires this mutex before touching `AI_MEMORY_KEY_DIR`.
/// Pattern mirrors `tests/memory_verify.rs` (H4), which the H6 chain
/// directly composes on top of.
static ENV_GUARD: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// Test fixture: per-test DB, per-test key dir, and two seeded memories
/// so link inserts pass the FK guards inside `db::create_link_signed` /
/// `db::create_link_inbound`.
struct Fixture {
    conn: rusqlite::Connection,
    /// Holds the DB tempdir alive for the test's lifetime.
    #[allow(dead_code)]
    db_tmp: TempDir,
    /// Holds the key tempdir alive for the test's lifetime. Production
    /// code reads through `AI_MEMORY_KEY_DIR` set in `setup`.
    keys_tmp: TempDir,
    src_id: String,
    dst_id: String,
}

fn setup() -> Fixture {
    let db_tmp = TempDir::new().expect("db tempdir");
    let keys_tmp = TempDir::new().expect("keys tempdir");

    // Point the production-code lookup path at the per-test key dir.
    // SAFETY: caller acquired `ENV_GUARD` before invoking setup, so no
    // sibling test races on this env var write.
    unsafe {
        std::env::set_var("AI_MEMORY_KEY_DIR", keys_tmp.path());
    }

    let db_path = db_tmp.path().join("ai-memory.db");
    let conn = db::open(&db_path).expect("db::open");

    // H5's `signed_events` table ships as migration 0020 but is not yet
    // wired into `db::migrate`'s auto-apply ladder (the production path
    // applies it lazily; H6 binds the substrate without waiting on the
    // wiring). Apply the idempotent `CREATE TABLE IF NOT EXISTS`
    // migration directly so scenario 3 can append audit rows. This
    // mirrors `signed_events::tests::fresh_db`'s same `include_str!`
    // approach.
    conn.execute_batch(include_str!(
        "../migrations/sqlite/0020_v07_signed_events.sql"
    ))
    .expect("apply H5 signed_events migration");

    let src_id = seed(&conn, "h6-src");
    let dst_id = seed(&conn, "h6-dst");

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
        namespace: "h6-test".to_string(),
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

/// Read the persisted `signature` blob (Some/None mirrors NULL state).
fn read_signature(conn: &rusqlite::Connection, src: &str, dst: &str) -> Option<Vec<u8>> {
    conn.query_row(
        "SELECT signature FROM memory_links WHERE source_id = ?1 AND target_id = ?2",
        params![src, dst],
        |row| row.get::<_, Option<Vec<u8>>>(0),
    )
    .ok()
    .flatten()
}

/// Read the persisted `attest_level` column (`Some(level)` / `None`).
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
// 1. H1 lifecycle smoke — keypair via library API round-trips on disk
// ---------------------------------------------------------------------------
//
// H1 ships the four-verb keypair API and the `AI_MEMORY_KEY_DIR`
// env-override (added in H4). H6 starts here because every other
// scenario depends on a keypair being recoverable from the per-test
// key dir.
#[test]
fn keypair_generated_via_library_round_trips() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    // Generate via the H1 library API (the CLI surface
    // `ai-memory identity generate` calls the same `kp_mod::generate`).
    let alice = kp_mod::generate("alice").expect("H1 generate");
    assert!(alice.can_sign(), "freshly-generated keypair must sign");
    kp_mod::save(&alice, f.keys_tmp.path()).expect("H1 save");

    // Reload through the same dir the production verifier reads
    // (`AI_MEMORY_KEY_DIR` was set by `setup` to `f.keys_tmp.path()`).
    let dir = kp_mod::default_key_dir().expect("env override resolves");
    assert_eq!(
        dir,
        f.keys_tmp.path(),
        "AI_MEMORY_KEY_DIR override must steer the production lookup"
    );
    let loaded = kp_mod::load("alice", &dir).expect("H1 load");
    assert_eq!(loaded.public.to_bytes(), alice.public.to_bytes());
    assert!(loaded.can_sign(), "private key must round-trip on load");
}

// ---------------------------------------------------------------------------
// 2. H2 happy path — `create_link_signed` populates the signature column
// ---------------------------------------------------------------------------
//
// G12 audit finding: in v0.6.3 the `signature` column on `memory_links`
// existed but was dead — never populated. H2 wires it up; this scenario
// asserts the column is non-NULL after a signed write.
#[test]
fn self_signed_link_persists_signature_column() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    let alice = kp_mod::generate("alice").expect("generate");
    kp_mod::save(&alice, f.keys_tmp.path()).expect("save");

    let attest = db::create_link_signed(&f.conn, &f.src_id, &f.dst_id, "related_to", Some(&alice))
        .expect("create_link_signed (signed)");
    assert_eq!(
        attest, "self_signed",
        "H2 must land self_signed for keypair"
    );

    let sig = read_signature(&f.conn, &f.src_id, &f.dst_id)
        .expect("signature column must be non-NULL after signed write");
    assert_eq!(
        sig.len(),
        64,
        "Ed25519 signatures are 64 bytes, got {}",
        sig.len()
    );
    assert_eq!(
        read_attest_level(&f.conn, &f.src_id, &f.dst_id).as_deref(),
        Some("self_signed"),
        "attest_level column must mirror the chosen level"
    );
}

// ---------------------------------------------------------------------------
// 3. H5 audit chain — signed_events.payload_hash binds canonical CBOR
// ---------------------------------------------------------------------------
//
// H5's append-only `signed_events` table is the auditor's source of
// truth: each row carries the SHA-256 of the canonical CBOR bytes the
// H2 signer hashed, plus the 64-byte signature. H6 asserts the binding:
// re-deriving canonical CBOR from the same `SignableLink` and hashing
// it must match the audit row's `payload_hash`.
//
// As of v0.7.0 fix-campaign S4-INFO2 (#690), `db::create_link_signed`
// auto-appends a `memory_link.created` audit row whose `payload_hash`
// already binds the canonical CBOR — exactly the auditor invariant
// this test was originally protecting. We still construct an explicit
// row below to verify the helper API surface and to keep the
// regression assert pinned at the substrate boundary; the test now
// expects TWO rows (auto-emit + explicit append), and asserts both
// carry the same `expected_hash` so the binding holds for either
// emit-path a downstream auditor encounters.
#[test]
fn signed_events_payload_hash_matches_canonical_cbor() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    let alice = kp_mod::generate("alice").expect("generate");
    kp_mod::save(&alice, f.keys_tmp.path()).expect("save");

    // Persist a self-signed link via H2.
    db::create_link_signed(&f.conn, &f.src_id, &f.dst_id, "related_to", Some(&alice))
        .expect("create_link_signed");
    let sig_on_row =
        read_signature(&f.conn, &f.src_id, &f.dst_id).expect("H2 must populate signature");

    // Read the row back so the audit binding hashes the bytes the
    // signer actually committed to (same `valid_from` instant the
    // INSERT chose, same observed_by claim).
    let (valid_from, observed_by): (Option<String>, Option<String>) = f
        .conn
        .query_row(
            "SELECT valid_from, observed_by FROM memory_links \
             WHERE source_id = ?1 AND target_id = ?2",
            params![f.src_id, f.dst_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read row metadata back");

    let signable = sign::SignableLink {
        src_id: &f.src_id,
        dst_id: &f.dst_id,
        relation: "related_to",
        observed_by: observed_by.as_deref(),
        valid_from: valid_from.as_deref(),
        valid_until: None,
    };
    let cbor = sign::canonical_cbor(&signable).expect("canonical cbor");
    let expected_hash = payload_hash(&cbor);
    assert_eq!(
        expected_hash.len(),
        32,
        "SHA-256 digest is 32 bytes, got {}",
        expected_hash.len()
    );

    // Append the audit row using the same hash + signature the H2 path
    // produced. This is the exact shape a downstream auditor will see.
    let event = SignedEvent {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: alice.agent_id.clone(),
        event_type: "memory_link.created".to_string(),
        payload_hash: expected_hash.clone(),
        signature: Some(sig_on_row.clone()),
        attest_level: "self_signed".to_string(),
        timestamp: Utc::now().to_rfc3339(),
        ..SignedEvent::default()
    };
    signed_events::append_signed_event(&f.conn, &event).expect("append audit row");

    // Read it back through the public listing API and assert the
    // hash + signature bind to the same bytes a verifier would re-derive.
    //
    // Two rows are present post-S4-INFO2: the auto-emit from
    // create_link_signed + the explicit append above. Both must carry
    // the same payload_hash because they describe the same logical
    // event; the binding invariant the test protects holds for either.
    let listed = signed_events::list_signed_events(&f.conn, Some(&alice.agent_id), 10, 0)
        .expect("list audit rows");
    assert_eq!(
        listed.len(),
        2,
        "auto-emit + explicit append yield two audit rows"
    );
    for row in &listed {
        assert_eq!(
            row.payload_hash, expected_hash,
            "every audit row's payload_hash must equal SHA-256(canonical_cbor(signable))"
        );
        assert_eq!(
            row.signature.as_deref(),
            Some(sig_on_row.as_slice()),
            "every audit row's signature blob must mirror memory_links.signature"
        );
        assert_eq!(row.attest_level, "self_signed");
        assert_eq!(row.agent_id, alice.agent_id);
    }
}

// ---------------------------------------------------------------------------
// 4. H4 happy path — memory_verify reports verified + self_signed
// ---------------------------------------------------------------------------
//
// `memory_verify(link_id)` re-derives canonical CBOR from the stored
// row and re-checks the signature against the on-disk public key. The
// H2 row just inserted must round-trip cleanly through the verifier.
#[test]
fn memory_verify_self_signed_happy_path() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    let alice = kp_mod::generate("alice").expect("generate");
    kp_mod::save(&alice, f.keys_tmp.path()).expect("save");
    db::create_link_signed(&f.conn, &f.src_id, &f.dst_id, "related_to", Some(&alice))
        .expect("create_link_signed");

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
// 5. Negative — tampering with link content in the DB invalidates verify
// ---------------------------------------------------------------------------
//
// The signature commits to the canonical CBOR over `relation` (among
// other fields). Mutating `relation` directly in the row makes the
// verifier re-derive different bytes — Ed25519 must reject. This is the
// exact attack surface G12 flagged: the column existed but nothing
// re-checked it on read.
//
// The unique key on `memory_links` is `(source_id, target_id,
// relation)`, so a relation rewrite is the cleanest mutation that
// avoids collateral collisions.
#[test]
fn memory_verify_tampered_content_returns_false() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    let alice = kp_mod::generate("alice").expect("generate");
    kp_mod::save(&alice, f.keys_tmp.path()).expect("save");
    db::create_link_signed(&f.conn, &f.src_id, &f.dst_id, "related_to", Some(&alice))
        .expect("create_link_signed");

    // Sanity: pre-tamper, verify must succeed — otherwise the negative
    // assertion below would be vacuous.
    let pre = mcp::handle_verify(
        &f.conn,
        &json!({
            "source_id": f.src_id,
            "target_id": f.dst_id,
            "relation": "related_to",
        }),
    )
    .expect("pre-tamper handle_verify Ok");
    assert_eq!(pre["signature_verified"], json!(true));

    // Mutate the link content directly in the DB — flip `relation` to
    // a value the signer never committed to. The signature blob and
    // observed_by stay byte-identical, so the verifier re-derives a
    // different canonical CBOR and rejects.
    f.conn
        .execute(
            "UPDATE memory_links SET relation = 'supersedes' \
             WHERE source_id = ?1 AND target_id = ?2",
            params![f.src_id, f.dst_id],
        )
        .expect("tamper UPDATE");

    let post = mcp::handle_verify(
        &f.conn,
        &json!({
            "source_id": f.src_id,
            "target_id": f.dst_id,
            "relation": "supersedes",
        }),
    )
    .expect("post-tamper handle_verify Ok (rejection is data, not Err)");

    assert_eq!(
        post["signature_verified"],
        json!(false),
        "tampered link content must reject"
    );
    assert_eq!(
        post["attest_level"],
        json!("unsigned"),
        "on-demand verify reports unsigned regardless of stored column"
    );
    assert_eq!(post["signed_by"], json!(null));
    assert_eq!(post["signed_at"], json!(null));
}

// ---------------------------------------------------------------------------
// 6. Federation — peer B's pubkey enrolled → peer_attested + verified
// ---------------------------------------------------------------------------
//
// Two-host topology: peer B signs a link off-host (its private key
// lives on its host); the local receiver imports B's public key only
// (`save_public_only`) and persists the inbound link via
// `create_link_inbound("peer_attested")`. `memory_verify` must report
// `signature_verified=true` + `attest_level=peer_attested`.
#[test]
fn peer_attested_inbound_link_verifies() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    // Peer B generates its keypair off-host — we simulate by minting it
    // here, then enrolling only the public half on the local key dir
    // (the H3 inbound trust model: receiver-controlled allowlist).
    let bob = kp_mod::generate("bob").expect("generate bob");
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

    // Receiver-side: `handlers::sync_push` persists the inbound link
    // via `create_link_inbound` after running the same verify decision.
    // We invoke the DB call directly with `peer_attested` to isolate
    // the H4/H5 behaviour from the HTTP wire surface (the wire path
    // already has its own coverage in `tests/federation_inbound_verify.rs`).
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
    db::create_link_inbound(&f.conn, &inbound, "peer_attested").expect("inbound insert");
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
// 7. No-pubkey — inbound from peer C with no enrolled key lands unsigned
// ---------------------------------------------------------------------------
//
// H3's federation back-compat posture: when the receiver has no public
// key for `observed_by`, we accept the link with
// `attest_level="unsigned"` rather than reject. `memory_verify` then
// reflects that back: `signature_verified=false` (no key to check
// against) + `attest_level="unsigned"`.
#[test]
fn inbound_link_with_no_enrolled_pubkey_lands_unsigned() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    // Peer C exists in the wider federation but is NOT enrolled on
    // this receiver — the key dir has no `carol.pub`.
    let carol = kp_mod::generate("carol").expect("generate carol");

    let valid_from = Utc::now().to_rfc3339();
    let signable = sign::SignableLink {
        src_id: &f.src_id,
        dst_id: &f.dst_id,
        relation: "related_to",
        observed_by: Some("carol"),
        valid_from: Some(&valid_from),
        valid_until: None,
    };
    let sig = sign::sign(&carol, &signable).expect("carol signs off-host");

    // Mirror the federation handler's accept-and-flag-as-unsigned
    // posture: `decide_attest_level` returns "unsigned" when no public
    // key is enrolled (see `tests/federation_inbound_verify.rs::
    // no_public_key_for_observed_by_lands_as_unsigned`). The DB column
    // accordingly stores "unsigned".
    let inbound = MemoryLink {
        source_id: f.src_id.clone(),
        target_id: f.dst_id.clone(),
        relation: ai_memory::models::MemoryLinkRelation::RelatedTo,
        created_at: Utc::now().to_rfc3339(),
        signature: Some(sig),
        observed_by: Some("carol".to_string()),
        valid_from: Some(valid_from),
        valid_until: None,
    };
    db::create_link_inbound(&f.conn, &inbound, "unsigned").expect("inbound insert (no-key path)");
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

    assert_eq!(
        body["signature_verified"],
        json!(false),
        "no enrolled key → cannot verify"
    );
    assert_eq!(
        body["attest_level"],
        json!("unsigned"),
        "stored attest_level surfaces through verify"
    );
    assert_eq!(body["signed_by"], json!(null));
    assert_eq!(body["signed_at"], json!(null));
}

// ---------------------------------------------------------------------------
// 8. Negative — flipping a signature byte invalidates verify
// ---------------------------------------------------------------------------
//
// Twin to scenario 5 but at the signature byte layer rather than the
// content layer. Ed25519 has no malleability window: any altered byte
// in the 64-byte signature must reject.
#[test]
fn tampered_signature_byte_does_not_verify() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let f = setup();

    let alice = kp_mod::generate("alice").expect("generate");
    kp_mod::save(&alice, f.keys_tmp.path()).expect("save");
    db::create_link_signed(&f.conn, &f.src_id, &f.dst_id, "related_to", Some(&alice))
        .expect("create_link_signed");

    // Read the persisted signature, flip the first byte (XOR 0xFF —
    // guaranteed-different value), write it back. The on-disk
    // `attest_level` column still says "self_signed" (writes never go
    // back), but on-demand re-verification reports the truth.
    let original_sig =
        read_signature(&f.conn, &f.src_id, &f.dst_id).expect("signature must be present");
    assert_eq!(original_sig.len(), 64);
    let mut tampered = original_sig.clone();
    tampered[0] ^= 0xff;
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
    assert_eq!(body["attest_level"], json!("unsigned"));
    assert_eq!(body["signed_by"], json!(null));
    assert_eq!(body["signed_at"], json!(null));
}
