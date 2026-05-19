// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 epic #813 — end-to-end persona signing pipeline regression
//! coverage.
//!
//! Pins the three behaviours closed by issues #810, #811, and #812:
//!
//! - **#810 (BUG-A)** — a hand-rolled INSERT that asserts
//!   `attest_level = 'self_signed'` with a NULL signature must be
//!   refused at the `SQLite` trigger layer. The `CHECK` trigger
//!   `memory_links_ck_attest_signature_ins` blocks the write.
//! - **#811 (BUG-B)** — calling `memory_persona_generate` with a
//!   keypair wired into the MCP dispatch must produce signed
//!   `derived_from` links. The pre-#811 dispatch passed `None`
//!   unconditionally; the fix routes `active_keypair` through.
//!   Regeneration (v2) must ALSO sign, not just v1.
//! - **#812 (BUG-C)** — the Persona artifact itself gains an Ed25519
//!   signature: `metadata.persona.signature` carries the base64 of
//!   the 64-byte bytes, `metadata.persona.attest_level = "self_signed"`,
//!   and `signed_events` has a `persona_generated` row with the same
//!   signature. The signature verifies against the daemon keypair's
//!   public key under the seven-field `SignablePersona` envelope.
//!
//! The test stands up a fresh tempdir DB (migrations run to v43, so
//! the CHECK trigger is installed), generates an Ed25519 keypair, and
//! drives `handle_persona_generate` with the keypair via the
//! pub-exposed `ai_memory::mcp::persona_generate_call` shim. The
//! migration / trigger / wire path are all exercised in one shot.

use std::sync::Once;

use ai_memory::autonomy::AutonomyLlm;
use ai_memory::config::FeatureTier;
use ai_memory::db;
use ai_memory::models::{ConfidenceSource, Memory, MemoryKind, Tier};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::Utc;
use ed25519_dalek::Verifier;
use rusqlite::{Connection, params};
use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

static INIT_TRACING: Once = Once::new();

fn init_tracing() {
    INIT_TRACING.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("warn")
            .with_test_writer()
            .try_init();
    });
}

/// `AutonomyLlm` stub returning a canned persona body so the test
/// stays hermetic. The substrate hashes the body before signing so
/// the actual prose is irrelevant — only its bytes flow through the
/// signing pipeline.
struct StubLlm;
impl AutonomyLlm for StubLlm {
    fn auto_tag(&self, _t: &str, _c: &str) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
    fn detect_contradiction(&self, _a: &str, _b: &str) -> anyhow::Result<bool> {
        Ok(false)
    }
    fn summarize_memories(&self, mems: &[(String, String)]) -> anyhow::Result<String> {
        Ok(format!("Persona body covering {} sources.", mems.len()))
    }
}

/// Seed two reflections tagged with `entity_id = alice` so the
/// PERF-8 `mentioned_entity_id` indexed lookup matches.
fn seed_two_alice_reflections(conn: &Connection, namespace: &str) -> Vec<String> {
    let mut ids = Vec::new();
    for i in 0..2 {
        let now = Utc::now().to_rfc3339();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: namespace.to_string(),
            title: format!("obs-{i} about alice"),
            content: format!("alice did thing {i}"),
            tags: vec!["reflection".into()],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id": "ai:test", "entity_id": "alice"}),
            reflection_depth: 1,
            memory_kind: MemoryKind::Reflection,
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
        ids.push(db::insert(conn, &mem).unwrap());
    }
    ids
}

fn fresh_db() -> (Connection, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("ai-memory.db");
    let conn = db::open(&path).unwrap();
    (conn, dir)
}

// ============================================================================
// BUG-A (#810) — CHECK trigger enforces atomic (attest_level, signature)
// ============================================================================

#[test]
fn issue_810_check_trigger_refuses_phantom_self_signed_insert() {
    init_tracing();
    let (conn, _dir) = fresh_db();
    // Two real memories so the FK targets exist.
    let now = Utc::now().to_rfc3339();
    let mk = |id: &str| Memory {
        id: id.to_string(),
        tier: Tier::Long,
        namespace: "global".into(),
        title: format!("memo-{id}"),
        content: "content".into(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".into(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now.clone(),
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "ai:test"}),
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
    db::insert(&conn, &mk("813-a-src")).unwrap();
    db::insert(&conn, &mk("813-a-tgt")).unwrap();

    let res = conn.execute(
        "INSERT INTO memory_links \
            (source_id, target_id, relation, created_at, valid_from, signature, attest_level) \
         VALUES (?1, ?2, 'related_to', ?3, ?3, NULL, 'self_signed')",
        params!["813-a-src", "813-a-tgt", &now],
    );
    let err = res.expect_err(
        "CHECK trigger memory_links_ck_attest_signature_ins must reject the phantom row",
    );
    let msg = format!("{err}");
    assert!(
        msg.contains("CHECK constraint failed") || msg.contains("64-byte signature"),
        "trigger error must explain itself, got: {msg}"
    );
}

// ============================================================================
// BUG-B (#811) + BUG-C (#812) — full e2e signing pipeline through MCP
// ============================================================================

// One end-to-end orchestration test legitimately needs every stage:
// MCP-dispatch invocation → DB-side link assertion → metadata
// assertion → signature verification → audit-row cross-check. Splitting
// it into shorter helpers would obscure the linear narrative the issue
// closure relies on, so opt out of `too_many_lines` here.
#[allow(clippy::too_many_lines)]
#[test]
fn issue_811_812_persona_generate_signs_links_and_artifact_end_to_end() {
    init_tracing();
    let (conn, _dir) = fresh_db();
    let sources = seed_two_alice_reflections(&conn, "team/alpha");
    assert_eq!(sources.len(), 2);

    // Fresh daemon keypair — the test owns the bytes, no env-var or
    // filesystem touched (verification done by hand against the
    // returned signature bytes).
    let kp = ai_memory::identity::keypair::generate("ai:curator").unwrap();
    let llm: Box<dyn AutonomyLlm> = Box::new(StubLlm);

    // The MCP dispatch surface — `persona_generate_call` is the
    // pub-re-exported `handle_persona_generate` (re-exported at the
    // module level by #809). The fifth parameter is the threaded
    // `active_keypair` — the regression is that the dispatch in
    // `src/mcp/mod.rs` now forwards this through.
    let resp = ai_memory::mcp::persona_generate_call(
        &conn,
        &json!({"entity_id": "alice", "namespace": "team/alpha"}),
        Some(llm.as_ref()),
        FeatureTier::Autonomous,
        Some(&kp),
    )
    .expect("memory_persona_generate must succeed");

    let persona = &resp["persona"];
    assert_eq!(persona["entity_id"], "alice");
    assert_eq!(persona["version"], 1);
    // BUG-C — the wire shape carries the signed attest_level.
    assert_eq!(persona["attest_level"], "self_signed");

    let persona_id = persona["id"].as_str().expect("persona.id").to_string();

    // --- BUG-B closing assertion: every derived_from link is signed
    let links: Vec<(Option<Vec<u8>>, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT signature, attest_level FROM memory_links \
                 WHERE source_id = ?1 AND relation = 'derived_from'",
            )
            .unwrap();
        stmt.query_map(params![&persona_id], |r| {
            Ok((r.get::<_, Option<Vec<u8>>>(0)?, r.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    };
    assert_eq!(links.len(), 2, "must write one derived_from per source");
    for (sig, level) in &links {
        assert_eq!(level, "self_signed", "every link must be self_signed");
        let s = sig.as_ref().expect("signed link must have signature bytes");
        assert_eq!(s.len(), 64, "Ed25519 signatures are 64 bytes");
    }

    // --- BUG-C closing assertion: persona metadata carries the sig
    let meta_str: String = conn
        .query_row(
            "SELECT metadata FROM memories WHERE id = ?1",
            params![&persona_id],
            |r| r.get(0),
        )
        .unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_str).unwrap();
    assert_eq!(meta["agent_id"], "ai:curator");
    assert_eq!(meta["persona"]["attest_level"], "self_signed");
    let sig_b64 = meta["persona"]["signature"]
        .as_str()
        .expect("metadata.persona.signature missing");
    let sig_bytes = BASE64_STANDARD
        .decode(sig_b64)
        .expect("metadata.persona.signature is not valid base64");
    assert_eq!(sig_bytes.len(), 64);

    // --- BUG-C closing assertion: verify the signature against the
    // keypair's public key under the seven-field SignablePersona.
    let body_md: String = conn
        .query_row(
            "SELECT content FROM memories WHERE id = ?1",
            params![&persona_id],
            |r| r.get(0),
        )
        .unwrap();
    let mut hasher = Sha256::new();
    hasher.update(body_md.as_bytes());
    let mut body_hash = [0u8; 32];
    body_hash.copy_from_slice(&hasher.finalize());

    let generated_at = persona["generated_at"]
        .as_str()
        .expect("persona.generated_at")
        .to_string();
    let source_ids: Vec<String> = persona["sources"]
        .as_array()
        .expect("persona.sources")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        source_ids.len(),
        2,
        "persona.sources must mirror the link rows"
    );
    let signable = ai_memory::identity::sign::SignablePersona {
        persona_id: persona_id.as_str(),
        entity_id: "alice",
        namespace: "team/alpha",
        version: 1,
        generated_at: generated_at.as_str(),
        sources: &source_ids,
        body_md_sha256: &body_hash,
    };
    let payload = ai_memory::identity::sign::canonical_cbor_persona(&signable).unwrap();
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&sig_bytes);
    let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
    kp.public
        .verify(&payload, &sig)
        .expect("metadata.persona.signature must verify under the curator keypair");

    // --- BUG-C closing assertion: signed_events row carries the same sig
    let (audit_sig, audit_attest): (Option<Vec<u8>>, String) = conn
        .query_row(
            "SELECT signature, attest_level FROM signed_events \
             WHERE event_type = 'persona_generated' \
             ORDER BY sequence DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(audit_attest, "self_signed");
    assert_eq!(
        audit_sig.expect("audit row must have signature bytes"),
        sig_bytes,
        "audit row signature must match metadata.persona.signature"
    );
}

// ----------------------------------------------------------------------------
// BUG-B (#811) — regeneration (v2) must ALSO sign end-to-end. Pins that the
// signer flows through every call to generate(), not just the first.
// ----------------------------------------------------------------------------

#[test]
fn issue_811_regeneration_v2_is_also_signed() {
    init_tracing();
    let (conn, _dir) = fresh_db();
    seed_two_alice_reflections(&conn, "team/alpha");
    let kp = ai_memory::identity::keypair::generate("ai:curator").unwrap();
    let llm: Box<dyn AutonomyLlm> = Box::new(StubLlm);

    // v1.
    let r1 = ai_memory::mcp::persona_generate_call(
        &conn,
        &json!({"entity_id": "alice", "namespace": "team/alpha"}),
        Some(llm.as_ref()),
        FeatureTier::Autonomous,
        Some(&kp),
    )
    .expect("v1 must succeed");
    assert_eq!(r1["persona"]["version"], 1);
    assert_eq!(r1["persona"]["attest_level"], "self_signed");

    // v2 — same kepyair, same dispatch.
    let r2 = ai_memory::mcp::persona_generate_call(
        &conn,
        &json!({"entity_id": "alice", "namespace": "team/alpha"}),
        Some(llm.as_ref()),
        FeatureTier::Autonomous,
        Some(&kp),
    )
    .expect("v2 must succeed");
    assert_eq!(r2["persona"]["version"], 2);
    assert_eq!(
        r2["persona"]["attest_level"], "self_signed",
        "regeneration (v2) MUST sign — the regression #811 closed"
    );

    // v2's derived_from links also signed.
    let v2_id = r2["persona"]["id"].as_str().unwrap();
    let signed_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_links \
             WHERE source_id = ?1 AND relation = 'derived_from' \
               AND attest_level = 'self_signed' AND length(signature) = 64",
            params![v2_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        signed_count, 2,
        "v2 must produce 2 signed derived_from links"
    );
}

// ----------------------------------------------------------------------------
// Negative-control: BUG-B is NOT fixed by "always sign". Without a keypair the
// dispatch must STILL produce an unsigned persona (no phantom labels).
// ----------------------------------------------------------------------------

#[test]
fn issue_812_persona_generate_without_keypair_stays_unsigned() {
    init_tracing();
    let (conn, _dir) = fresh_db();
    seed_two_alice_reflections(&conn, "team/alpha");
    let llm: Box<dyn AutonomyLlm> = Box::new(StubLlm);

    let resp = ai_memory::mcp::persona_generate_call(
        &conn,
        &json!({"entity_id": "alice", "namespace": "team/alpha"}),
        Some(llm.as_ref()),
        FeatureTier::Autonomous,
        None,
    )
    .expect("persona generation must succeed without a keypair");
    assert_eq!(resp["persona"]["attest_level"], "unsigned");

    let persona_id = resp["persona"]["id"].as_str().unwrap();
    let meta_str: String = conn
        .query_row(
            "SELECT metadata FROM memories WHERE id = ?1",
            params![persona_id],
            |r| r.get(0),
        )
        .unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_str).unwrap();
    // The unsigned path MUST NOT carry a signature field — false
    // signing would be the inverse defect of #810/#811/#812.
    assert!(
        meta["persona"].get("signature").is_none() || meta["persona"]["signature"].is_null(),
        "metadata.persona.signature must be absent on the unsigned path; got: {meta:#?}"
    );

    // And every derived_from link is unsigned (NULL signature column).
    let null_sig_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_links \
             WHERE source_id = ?1 AND relation = 'derived_from' \
               AND signature IS NULL AND attest_level = 'unsigned'",
            params![persona_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        null_sig_count, 2,
        "unsigned path: 2 links with NULL signature + unsigned attest_level"
    );
}
