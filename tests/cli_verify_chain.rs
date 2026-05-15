// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L1-3 — `ai-memory verify-reflection-chain` CLI integration tests.
//!
//! Scenario coverage (per issue #660):
//!   1. Synthetic chain depth 0-3, all edges unsigned → exit 0, reports
//!      unsigned (no failures).
//!   2. Synthetic chain depth 0-3, all edges signed + valid → exit 0.
//!   3. Tampered signature (manually corrupt one `signature` blob) →
//!      exit non-zero with a FAIL marker in the output.
//!   4. Chain with no signatures (unsigned links) → exit 0.
//!   5. Out-of-bound chain (memory `reflection_depth` > governance cap)
//!      → exit non-zero, output contains `exceeded_cap`.
//!   6. `--format json` produces a parseable payload with the documented
//!      schema fields.
//!   7. `--include-signed-events` flag is accepted without error.
//!
//! All tests set `AI_MEMORY_NO_CONFIG=1` per the standard CLI test
//! convention. Scratch lives in per-test `tempfile::TempDir`s —
//! never `/tmp` (project hard rule).

use ai_memory::db;
use ai_memory::identity::keypair as kp_mod;
use ai_memory::identity::sign;
use ai_memory::models::{Memory, Tier};
use assert_cmd::Command;
use chrono::Utc;
use predicates::prelude::*;
use rusqlite::params;
use tempfile::TempDir;

// ─────────────────────────────────────────────────────────────────────
// Fixture helpers
// ─────────────────────────────────────────────────────────────────────

/// Build the `ai-memory --db <path>` command with `AI_MEMORY_NO_CONFIG=1`.
fn ai_memory_cmd(db_path: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("ai-memory").unwrap();
    cmd.env("AI_MEMORY_NO_CONFIG", "1")
        .args(["--db", db_path.to_str().unwrap()]);
    cmd
}

/// Open the DB (triggering migrations) and return the connection.
fn open_db(db_path: &std::path::Path) -> rusqlite::Connection {
    db::open(db_path).expect("db::open")
}

/// Insert a memory at the given `reflection_depth` and return its id.
fn insert_memory(conn: &rusqlite::Connection, ns: &str, depth: i32) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let mem = Memory {
        id: id.clone(),
        tier: Tier::Mid,
        namespace: ns.to_string(),
        title: format!("depth-{depth}"),
        content: format!("content at depth {depth}"),
        reflection_depth: depth,
        created_at: now.clone(),
        updated_at: now,
        ..Default::default()
    };
    db::insert(conn, &mem).expect("insert memory");
    id
}

/// Insert a `reflects_on` link without a signature.
fn insert_unsigned_reflects_on(conn: &rusqlite::Connection, source_id: &str, target_id: &str) {
    conn.execute(
        "INSERT OR IGNORE INTO memory_links \
         (source_id, target_id, relation, created_at, attest_level) \
         VALUES (?1, ?2, 'reflects_on', ?3, 'unsigned')",
        params![source_id, target_id, Utc::now().to_rfc3339()],
    )
    .expect("insert unsigned link");
}

/// Insert a `reflects_on` link signed with `agent`'s keypair.
/// Returns the 64-byte signature for potential tampering.
fn insert_signed_reflects_on(
    conn: &rusqlite::Connection,
    source_id: &str,
    target_id: &str,
    keypair: &kp_mod::AgentKeypair,
) -> Vec<u8> {
    let now = Utc::now().to_rfc3339();
    let link = sign::SignableLink {
        src_id: source_id,
        dst_id: target_id,
        relation: "reflects_on",
        observed_by: Some(&keypair.agent_id),
        valid_from: Some(&now),
        valid_until: None,
    };
    let sig = sign::sign(keypair, &link).expect("sign link");
    // Mirror create_link_signed: valid_from = created_at = now.
    // The verifier reads valid_from from the DB and must re-derive
    // the same canonical CBOR bytes the signer committed to.
    conn.execute(
        "INSERT OR IGNORE INTO memory_links \
         (source_id, target_id, relation, created_at, valid_from, \
          signature, observed_by, attest_level) \
         VALUES (?1, ?2, 'reflects_on', ?3, ?3, ?4, ?5, 'self_signed')",
        params![source_id, target_id, now, sig, keypair.agent_id],
    )
    .expect("insert signed link");
    sig
}

/// Attach a `max_reflection_depth` governance policy to `ns` by
/// inserting a namespace standard memory. Mirrors the `seed_policy`
/// helper in `tests/governance_inheritance.rs`.
fn set_governance_cap(conn: &rusqlite::Connection, ns: &str, cap: u32) {
    use ai_memory::models::{GovernancePolicy, default_metadata};
    let now = Utc::now().to_rfc3339();
    let policy = GovernancePolicy {
        max_reflection_depth: Some(cap),
        auto_export_reflections_to_filesystem: None,
        auto_atomise: None,
        auto_atomise_threshold_cl100k: None,
        auto_atomise_max_atom_tokens: None,
        auto_persona_trigger_every_n_memories: None,
        auto_export_personas_to_filesystem: None,
        auto_atomise_mode: None,
        legacy_per_pair_classifier: None,
        auto_classify_kind: None,
        ..GovernancePolicy::default()
    };
    let mut metadata = default_metadata();
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert("agent_id".into(), serde_json::Value::String("test".into()));
        obj.insert("governance".into(), serde_json::to_value(&policy).unwrap());
    }
    let standard = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: format!("_standards-{ns}"),
        title: format!("standard for {ns}"),
        content: "policy".into(),
        created_at: now.clone(),
        updated_at: now,
        metadata,
        ..Default::default()
    };
    let sid = db::insert(conn, &standard).expect("insert standard");
    db::set_namespace_standard(conn, ns, &sid, None).expect("set_namespace_standard");
}

// ─────────────────────────────────────────────────────────────────────
// Scenario 1 + 4: unsigned chain depth 0-3 → exit 0, reports unsigned
// ─────────────────────────────────────────────────────────────────────

#[test]
fn unsigned_chain_depth_0_to_3_exits_0() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("ai-memory.db");
    let conn = open_db(&db_path);

    let d0 = insert_memory(&conn, "v-chain", 0);
    let d1 = insert_memory(&conn, "v-chain", 1);
    let d2 = insert_memory(&conn, "v-chain", 2);
    let d3 = insert_memory(&conn, "v-chain", 3);
    insert_unsigned_reflects_on(&conn, &d3, &d2);
    insert_unsigned_reflects_on(&conn, &d2, &d1);
    insert_unsigned_reflects_on(&conn, &d1, &d0);
    drop(conn);

    ai_memory_cmd(&db_path)
        .args(["verify-reflection-chain", &d3])
        .assert()
        .success()
        .stdout(predicate::str::contains("memories=4"))
        .stdout(predicate::str::contains("depth=3"))
        .stdout(predicate::str::contains("failed=0"));
}

// ─────────────────────────────────────────────────────────────────────
// Scenario 2: signed + valid chain → exit 0
// ─────────────────────────────────────────────────────────────────────

#[test]
fn signed_valid_chain_exits_0() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("ai-memory.db");
    let keys_tmp = TempDir::new().unwrap();
    let conn = open_db(&db_path);

    let agent = kp_mod::generate("alice-l13").expect("gen keypair");
    kp_mod::save(&agent, keys_tmp.path()).expect("save keypair");

    let d0 = insert_memory(&conn, "v-signed", 0);
    let d1 = insert_memory(&conn, "v-signed", 1);
    insert_signed_reflects_on(&conn, &d1, &d0, &agent);
    drop(conn);

    ai_memory_cmd(&db_path)
        .args(["verify-reflection-chain", &d1])
        .env("AI_MEMORY_KEY_DIR", keys_tmp.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("failed=0"));
}

// ─────────────────────────────────────────────────────────────────────
// Scenario 3: tampered signature → exit non-zero
// ─────────────────────────────────────────────────────────────────────

#[test]
fn tampered_signature_exits_nonzero() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("ai-memory.db");
    let keys_tmp = TempDir::new().unwrap();
    let conn = open_db(&db_path);

    let agent = kp_mod::generate("bob-l13").expect("gen keypair");
    kp_mod::save(&agent, keys_tmp.path()).expect("save keypair");

    let d0 = insert_memory(&conn, "v-tamper", 0);
    let d1 = insert_memory(&conn, "v-tamper", 1);
    let mut sig = insert_signed_reflects_on(&conn, &d1, &d0, &agent);

    // Flip the first byte — invalidates the Ed25519 signature.
    sig[0] ^= 0x01;
    conn.execute(
        "UPDATE memory_links SET signature = ?1 \
         WHERE source_id = ?2 AND target_id = ?3 AND relation = 'reflects_on'",
        params![sig, d1, d0],
    )
    .expect("corrupt sig");
    drop(conn);

    ai_memory_cmd(&db_path)
        .args(["verify-reflection-chain", &d1])
        .env("AI_MEMORY_KEY_DIR", keys_tmp.path())
        .assert()
        .failure()
        .stdout(predicate::str::contains("FAIL"));
}

// ─────────────────────────────────────────────────────────────────────
// Scenario 5: out-of-bound chain → exit non-zero
// ─────────────────────────────────────────────────────────────────────

#[test]
fn out_of_bound_chain_exits_nonzero() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("ai-memory.db");
    let conn = open_db(&db_path);

    // Cap = 1, memory at depth 2 violates it.
    set_governance_cap(&conn, "v-oob", 1);
    let d0 = insert_memory(&conn, "v-oob", 0);
    let d1 = insert_memory(&conn, "v-oob", 1);
    let d2 = insert_memory(&conn, "v-oob", 2);
    insert_unsigned_reflects_on(&conn, &d2, &d1);
    insert_unsigned_reflects_on(&conn, &d1, &d0);
    drop(conn);

    ai_memory_cmd(&db_path)
        .args(["verify-reflection-chain", &d2])
        .assert()
        .failure()
        .stdout(predicate::str::contains("exceeded_cap"));
}

// ─────────────────────────────────────────────────────────────────────
// Scenario 6: --format json produces parseable payload
// ─────────────────────────────────────────────────────────────────────

#[test]
fn json_output_is_parseable() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("ai-memory.db");
    let conn = open_db(&db_path);

    let d0 = insert_memory(&conn, "v-json", 0);
    let d1 = insert_memory(&conn, "v-json", 1);
    insert_unsigned_reflects_on(&conn, &d1, &d0);
    drop(conn);

    let output = ai_memory_cmd(&db_path)
        .args(["verify-reflection-chain", &d1, "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let text = std::str::from_utf8(&output).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("JSON parse failed: {e}\noutput: {text}"));

    // Required top-level fields per issue #660 (AgenticMem Attest schema).
    assert!(parsed.get("root_id").is_some(), "missing root_id");
    assert!(parsed.get("n_memories").is_some(), "missing n_memories");
    assert!(parsed.get("chain_depth").is_some(), "missing chain_depth");
    assert!(
        parsed.get("edges_verified").is_some(),
        "missing edges_verified"
    );
    assert!(parsed.get("edges_failed").is_some(), "missing edges_failed");
    assert!(parsed.get("edges").is_some(), "missing edges array");
    assert!(
        parsed.get("bounded_status").is_some(),
        "missing bounded_status"
    );
    assert!(parsed.get("generated_at").is_some(), "missing generated_at");
    assert_eq!(parsed["root_id"].as_str().unwrap(), d1);
    assert_eq!(parsed["n_memories"].as_u64().unwrap(), 2);
}

// ─────────────────────────────────────────────────────────────────────
// Scenario 7: --include-signed-events flag is accepted
// ─────────────────────────────────────────────────────────────────────

#[test]
fn include_signed_events_flag_accepted() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("ai-memory.db");
    let conn = open_db(&db_path);
    let d0 = insert_memory(&conn, "v-se", 0);
    drop(conn);

    // Flag must be accepted without error (signed_events may be empty).
    ai_memory_cmd(&db_path)
        .args([
            "verify-reflection-chain",
            &d0,
            "--include-signed-events",
            "--format",
            "json",
        ])
        .assert()
        .success();
}
