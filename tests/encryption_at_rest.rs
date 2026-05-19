// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// Test scaffolding: keep pedantic lints quiet where they add no value.
#![allow(clippy::doc_markdown)]

//! v0.7.0 (issue #228) — E2E memory content encryption at rest.
//!
//! This regression suite pins the substrate-level encryption contract
//! end-to-end:
//!
//! 1. Schema v44 migration applies cleanly on a fresh database —
//!    `memories.encrypted_envelope BLOB NULL` column is present, and
//!    `schema_version` reaches the binary's `CURRENT_SCHEMA_VERSION`.
//!
//! 2. Round-trip: encrypt plaintext → persist the envelope in the
//!    `encrypted_envelope` column → re-read the envelope from the
//!    DB → decrypt → recover the original plaintext byte-for-byte.
//!
//! 3. Non-encrypted memories are unchanged: the new column is NULL,
//!    the `content` column carries plaintext, and `db::get` returns
//!    the memory with the original `content` intact (zero behaviour
//!    drift for callers that haven't opted into encryption).
//!
//! 4. AEAD tamper detection: flipping a bit in the persisted envelope
//!    causes `decrypt` to fail (no silent plaintext fallback).
//!
//! 5. The encryption gate (`encryption_enabled`) respects both the
//!    config flag (`Some(true)`) and the `AI_MEMORY_ENCRYPT_AT_REST`
//!    env var, and defaults to off when neither source opts in.
//!
//! Runs under `AI_MEMORY_NO_CONFIG=1` — no embedder, no LLM, no
//! network dependencies.

use ai_memory::encryption::{
    Envelope, decrypt, encrypt, encryption_enabled, get_or_create_keypair,
};
use ai_memory::models::{ConfidenceSource, Memory, MemoryKind, Tier};
use ai_memory::storage as db;
use rusqlite::params;

fn fresh_conn() -> rusqlite::Connection {
    db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

fn make_mem(title: &str, content: &str, ns: &str) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: ns.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "api".to_string(),
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
        citations: vec![],
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    }
}

#[test]
fn schema_v44_migration_applies_cleanly_and_adds_encrypted_envelope_column() {
    // Opening a fresh in-memory database runs migrate() implicitly via
    // `db::open`. The terminal version must equal the binary's
    // CURRENT_SCHEMA_VERSION, and the new column must be present on
    // the `memories` table.
    let conn = fresh_conn();

    let version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .expect("read schema_version");
    let expected = db::current_schema_version_for_tests();
    assert_eq!(
        version, expected,
        "schema_version must reach the binary's CURRENT_SCHEMA_VERSION ({expected}); \
         got {version}"
    );
    assert!(
        version >= 44,
        "v44 substrate must be installed (issue #228); got {version}"
    );

    // The encrypted_envelope BLOB column must be present.
    let mut stmt = conn
        .prepare("PRAGMA table_info(memories)")
        .expect("prepare PRAGMA");
    let cols: Vec<(String, String)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })
        .expect("query_map")
        .filter_map(Result::ok)
        .collect();
    let found = cols
        .iter()
        .find(|(name, _)| name == "encrypted_envelope")
        .expect("encrypted_envelope column must be present on memories after v44");
    assert!(
        found.1.eq_ignore_ascii_case("BLOB"),
        "encrypted_envelope must be BLOB-typed; got {}",
        found.1
    );
}

#[test]
fn encrypt_store_recall_decrypt_recovers_plaintext() {
    let conn = fresh_conn();

    // Caller-side: encrypt the plaintext content to the agent's
    // X25519 pubkey, then persist the envelope bytes on the new
    // schema-v44 column. The `content` column carries a placeholder
    // marker so the row is still a valid memory shape for downstream
    // callers that don't decrypt.
    let agent = "test-agent-228";
    let plaintext = "the launch codes are 1138, 0451, 4815";
    let kp = get_or_create_keypair(agent).expect("keypair");
    let envelope = encrypt(plaintext, &kp.public).expect("encrypt");
    let envelope_bytes = envelope.to_bytes();

    // Insert the memory with the placeholder content. (The MVP wiring
    // pattern: handler swaps plaintext content with a sentinel before
    // calling `db::insert`, then persists the envelope blob via a
    // separate UPDATE. The test exercises both writes back-to-back to
    // mirror that pattern without dragging the handler into scope.)
    let mut mem = make_mem("issue-228-roundtrip", "[ENCRYPTED]", "global");
    let mem_id = mem.id.clone();
    let actual_id = db::insert(&conn, &mem).expect("insert");
    assert_eq!(actual_id, mem_id);

    conn.execute(
        "UPDATE memories SET encrypted_envelope = ?1 WHERE id = ?2",
        params![envelope_bytes, &actual_id],
    )
    .expect("persist envelope");

    // Reader-side: pull the blob back out, parse the envelope, and
    // decrypt with the recipient's secret key. Must recover the
    // original plaintext byte-for-byte.
    let stored: Vec<u8> = conn
        .query_row(
            "SELECT encrypted_envelope FROM memories WHERE id = ?1",
            params![&actual_id],
            |r| r.get(0),
        )
        .expect("read envelope back");
    assert_eq!(stored, envelope_bytes, "envelope bytes must round-trip");

    let parsed = Envelope::from_bytes(&stored).expect("parse envelope");
    let recovered = decrypt(&parsed, &kp.secret).expect("decrypt");
    assert_eq!(
        recovered, plaintext,
        "decrypt must recover the original plaintext byte-for-byte"
    );

    // And `db::get` continues to return the memory shape — the
    // `content` column still carries the placeholder, no panic on
    // the BLOB column being present.
    let fetched = db::get(&conn, &actual_id)
        .expect("db::get")
        .expect("memory must exist");
    assert_eq!(fetched.id, actual_id);
    assert_eq!(fetched.title, "issue-228-roundtrip");
    assert_eq!(
        fetched.content, "[ENCRYPTED]",
        "non-decrypting reader sees the placeholder, not the plaintext"
    );

    // Silence unused-mut on mem — kept mutable so the make_mem helper
    // signature stays compatible with mutation patterns in sibling
    // tests.
    let _ = &mut mem;
}

#[test]
fn non_encrypted_memories_unchanged_after_v44() {
    // A memory written without the encryption opt-in carries
    // plaintext in `content` and NULL in `encrypted_envelope`. The
    // public `Memory` shape and the existing read paths must behave
    // exactly as they did pre-v44.
    let conn = fresh_conn();
    let plaintext = "this is plaintext, encryption gate off";
    let mem = make_mem("issue-228-non-encrypted", plaintext, "global");
    let actual_id = db::insert(&conn, &mem).expect("insert");

    let envelope: Option<Vec<u8>> = conn
        .query_row(
            "SELECT encrypted_envelope FROM memories WHERE id = ?1",
            params![&actual_id],
            |r| r.get(0),
        )
        .expect("query");
    assert!(
        envelope.is_none(),
        "non-encrypted memories must leave encrypted_envelope NULL"
    );

    let fetched = db::get(&conn, &actual_id)
        .expect("db::get")
        .expect("memory must exist");
    assert_eq!(fetched.content, plaintext);
}

#[test]
fn tampered_envelope_bytes_fail_aead_authentication() {
    // The AEAD tag in ChaCha20-Poly1305 is non-malleable: any change
    // to the on-disk bytes — ephemeral pubkey, nonce, or ciphertext —
    // must cause decrypt to fail. No silent plaintext leak.
    let conn = fresh_conn();
    let agent = "test-agent-228-tamper";
    let kp = get_or_create_keypair(agent).expect("keypair");
    let envelope = encrypt("tamper-detect-payload", &kp.public).expect("encrypt");
    let mut bytes = envelope.to_bytes();
    // Flip a bit deep in the ciphertext (after the 1-byte version +
    // 32-byte pubkey + 12-byte nonce header).
    let cipher_idx = 1 + 32 + 12 + 4;
    bytes[cipher_idx] ^= 0x55;

    let mem = make_mem("issue-228-tamper", "[ENCRYPTED]", "global");
    let id = db::insert(&conn, &mem).expect("insert");
    conn.execute(
        "UPDATE memories SET encrypted_envelope = ?1 WHERE id = ?2",
        params![bytes, &id],
    )
    .expect("persist tampered envelope");

    let stored: Vec<u8> = conn
        .query_row(
            "SELECT encrypted_envelope FROM memories WHERE id = ?1",
            params![&id],
            |r| r.get(0),
        )
        .expect("read");
    let parsed = Envelope::from_bytes(&stored).expect("parse tampered envelope");
    assert!(
        decrypt(&parsed, &kp.secret).is_err(),
        "AEAD must refuse to decrypt a tampered envelope"
    );
}

#[test]
fn encryption_gate_consults_config_flag_and_env_var() {
    // Save + restore the env var so this test doesn't perturb the
    // process state for sibling tests.
    let prev = std::env::var("AI_MEMORY_ENCRYPT_AT_REST").ok();
    // SAFETY: env-var reads/writes are inherently process-global;
    // tests in this file are not parallelised within a single
    // integration binary, so the read/restore is safe.
    unsafe { std::env::remove_var("AI_MEMORY_ENCRYPT_AT_REST") };
    assert!(!encryption_enabled(None), "default off");
    assert!(encryption_enabled(Some(true)), "config flag opts in");
    assert!(!encryption_enabled(Some(false)), "explicit false stays off");
    unsafe { std::env::set_var("AI_MEMORY_ENCRYPT_AT_REST", "1") };
    assert!(encryption_enabled(None), "env var opts in");
    unsafe { std::env::set_var("AI_MEMORY_ENCRYPT_AT_REST", "no") };
    assert!(!encryption_enabled(None), "falsy env stays off");

    // Restore.
    if let Some(v) = prev {
        unsafe { std::env::set_var("AI_MEMORY_ENCRYPT_AT_REST", v) };
    } else {
        unsafe { std::env::remove_var("AI_MEMORY_ENCRYPT_AT_REST") };
    }
}
