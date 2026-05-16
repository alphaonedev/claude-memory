// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L1-6 — substrate-rules activation integration tests.
//!
//! The audit-critical bypass-impossibility properties:
//!
//! 1. `enforced_rule_must_be_operator_signed` — seed an unsigned
//!    `enabled = 1` rule. `check_agent_action` returns Allow (the
//!    rule is SKIPPED with a `warn!` because the L1-6 pubkey is
//!    configured but the row carries no operator signature). Then
//!    sign it via the `sign-seed` pipeline. The same action now
//!    returns Refuse.
//! 2. `tampered_signature_rejects_at_load` — sign a rule. Then
//!    directly UPDATE the row's `matcher` field via raw rusqlite
//!    (no re-sign). Load + check: the rule is SKIPPED. The agent
//!    action proceeds (Allow).
//! 3. `direct_enabled_flip_bypass_attempt_fails` — seed signed rule
//!    with `enabled = 0`. Direct SQL `UPDATE governance_rules SET
//!    enabled = 1 WHERE id = ?`. Load + check: rule is SKIPPED
//!    because the signature commits to `enabled = 0`.
//! 4. `keygen_writes_0600_and_load_refuses_open_permissions` — call
//!    `keygen`, verify perms are `0o600`. Then `chmod 0o644` and call
//!    `load_operator_signing_key` — must return a clear error
//!    mentioning `0600`.
//! 5. `sign_seed_idempotent` — call `sign-seed` twice. Second call is
//!    a no-op (same canonical bytes → same signature; UPDATEs are
//!    no-ops). Verify no errors + row signatures unchanged.
//!
//! # Env-var serialization
//!
//! Several tests mutate the process-wide `AI_MEMORY_OPERATOR_PUBKEY`
//! env var to inject a per-test verifying key. `cargo test` runs
//! tests in parallel within a file, so env mutation MUST be
//! serialized — otherwise a concurrent test reading the env mid-
//! mutation would observe a transient value. A process-wide
//! `OnceLock<Mutex<()>>` guard fences the env reads/writes; tests
//! that touch the env must `let _g = env_lock().lock().unwrap();`
//! at function entry and hold the guard for the duration of the
//! test body.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use ai_memory::cli::CliOutput;
use ai_memory::cli::rules as cli_rules;
use ai_memory::governance::agent_action::{
    AgentAction, Decision, GOVERNANCE_CHECK_EVENT_TYPE, check_agent_action,
};
use ai_memory::governance::rules_store::{self, Rule};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Process-wide mutex serializing env-var mutation across L1-6 tests.
/// `cargo test` runs tests in parallel within a single test binary;
/// any test that flips `AI_MEMORY_OPERATOR_PUBKEY` must hold this lock
/// for the whole duration of its body. Poisoned-lock recovery returns
/// the inner guard so a panicking prior test cannot wedge the suite.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Build a fresh in-memory rules + `signed_events` schema. Mirrors the
/// `governance_agent_action.rs` helper so both integration files
/// share the same idea of "minimal substrate".
fn fresh_conn() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE governance_rules (
             id TEXT PRIMARY KEY,
             kind TEXT NOT NULL,
             matcher TEXT NOT NULL,
             severity TEXT NOT NULL CHECK (severity IN ('refuse','warn','log')),
             reason TEXT NOT NULL,
             namespace TEXT NOT NULL DEFAULT '_global',
             created_by TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             enabled INTEGER NOT NULL DEFAULT 1,
             signature BLOB,
             attest_level TEXT NOT NULL DEFAULT 'unsigned'
         );
         CREATE TABLE signed_events (
             id TEXT PRIMARY KEY,
             agent_id TEXT NOT NULL,
             event_type TEXT NOT NULL,
             payload_hash BLOB NOT NULL,
             signature BLOB,
             attest_level TEXT NOT NULL DEFAULT 'unsigned',
             timestamp TEXT NOT NULL,
             -- v34 (V-4 closeout, #698) — cross-row chain columns.
             prev_hash BLOB,
             sequence INTEGER
         );",
    )
    .unwrap();
    conn
}

/// Insert an unsigned, enabled `filesystem_write` rule covering
/// `/tmp/**` — same shape as the L1-6 seed R001 in
/// `migrations/sqlite/0024_v07_governance_rules.sql`.
fn insert_seed_rule(conn: &rusqlite::Connection, id: &str, enabled: bool) {
    rules_store::insert(
        conn,
        &Rule {
            id: id.to_string(),
            kind: "filesystem_write".into(),
            matcher: r#"{"glob":"/tmp/**"}"#.into(),
            severity: "refuse".into(),
            reason: format!("{id}: no /tmp writes"),
            namespace: "_global".into(),
            created_by: "system:seed".into(),
            created_at: 0,
            enabled,
            signature: None,
            attest_level: "unsigned".into(),
        },
    )
    .unwrap();
}

/// Sign every row in `governance_rules` under `signing` using
/// `rules_store::canonical_bytes_for_signing`. Mirrors what the
/// `ai-memory rules sign-seed` CLI does, but without going through
/// the `CliOutput` plumbing — keeps the integration test concise.
fn sign_all_rules(conn: &rusqlite::Connection, signing: &SigningKey) {
    for rule in rules_store::list(conn).unwrap() {
        let canonical = rules_store::canonical_bytes_for_signing(&rule).unwrap();
        let sig = signing.sign(&canonical);
        rules_store::update_signature(conn, &rule.id, &sig.to_bytes(), "operator_signed").unwrap();
    }
}

/// Install `signing.verifying_key()` as the operator pubkey via
/// `AI_MEMORY_OPERATOR_PUBKEY`. The caller MUST already hold the
/// `env_lock()` mutex.
fn install_operator_pubkey(signing: &SigningKey) {
    let pk_b64 =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signing.verifying_key().to_bytes());
    // SAFETY: env mutation serialised by `env_lock` for the duration
    // of the calling test. No other process touches this var.
    unsafe { std::env::set_var("AI_MEMORY_OPERATOR_PUBKEY", pk_b64) };
}

/// Scrub the operator pubkey env var. Caller MUST hold `env_lock()`.
fn uninstall_operator_pubkey() {
    // SAFETY: env mutation serialised by `env_lock` for the duration
    // of the calling test.
    unsafe { std::env::remove_var("AI_MEMORY_OPERATOR_PUBKEY") };
}

/// Run `check_agent_action` against a `/tmp/x` filesystem write — the
/// canonical "would the substrate refuse this?" probe used by every
/// bypass-impossibility test.
fn probe_tmp_write(conn: &rusqlite::Connection) -> Decision {
    let action = AgentAction::FilesystemWrite {
        path: "/tmp/x".into(),
        byte_estimate: None,
    };
    check_agent_action(conn, "agent:l16-test", &action).unwrap()
}

// ---------------------------------------------------------------------------
// 1. enforced_rule_must_be_operator_signed
// ---------------------------------------------------------------------------

#[test]
fn enforced_rule_must_be_operator_signed() {
    let _g = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut csprng = rand_core::OsRng;
    let signing = SigningKey::generate(&mut csprng);

    install_operator_pubkey(&signing);

    let conn = fresh_conn();
    // Seed unsigned + enabled rule. The L1-6 verifier sees the
    // pubkey is configured AND the row is `unsigned` → SKIP. The
    // `/tmp/x` write must Allow because no rule is enforced.
    insert_seed_rule(&conn, "R001", true);
    let decision_before = probe_tmp_write(&conn);
    assert_eq!(
        decision_before,
        Decision::Allow,
        "unsigned-enabled rule must be skipped when L1-6 pubkey is configured"
    );

    // Sign every row. Now the rule has `attest_level=operator_signed`
    // and `signature` over (id,kind,matcher,severity,reason,
    // namespace,created_by,enabled). The same `/tmp/x` action must
    // refuse.
    sign_all_rules(&conn, &signing);
    let decision_after = probe_tmp_write(&conn);
    match decision_after {
        Decision::Refuse { rule_id, .. } => assert_eq!(rule_id, "R001"),
        other => panic!("expected Refuse after signing; got {other:?}"),
    }

    uninstall_operator_pubkey();
}

// ---------------------------------------------------------------------------
// 2. tampered_signature_rejects_at_load
// ---------------------------------------------------------------------------

#[test]
fn tampered_signature_rejects_at_load() {
    let _g = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut csprng = rand_core::OsRng;
    let signing = SigningKey::generate(&mut csprng);

    install_operator_pubkey(&signing);

    let conn = fresh_conn();
    insert_seed_rule(&conn, "R001", true);
    sign_all_rules(&conn, &signing);
    // Signed enabled rule → refuse on /tmp.
    assert!(matches!(probe_tmp_write(&conn), Decision::Refuse { .. }));

    // Direct SQL tampering: change the matcher to point at /var/tmp.
    // The signature was computed over the original matcher; the row
    // no longer verifies.
    conn.execute(
        "UPDATE governance_rules SET matcher = ?1 WHERE id = ?2",
        rusqlite::params![r#"{"glob":"/var/tmp/**"}"#, "R001"],
    )
    .unwrap();
    let decision = probe_tmp_write(&conn);
    assert_eq!(
        decision,
        Decision::Allow,
        "tampered row must be skipped; /tmp/x is no longer covered by anything"
    );

    uninstall_operator_pubkey();
}

// ---------------------------------------------------------------------------
// 3. direct_enabled_flip_bypass_attempt_fails
// ---------------------------------------------------------------------------

#[test]
fn direct_enabled_flip_bypass_attempt_fails() {
    let _g = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut csprng = rand_core::OsRng;
    let signing = SigningKey::generate(&mut csprng);

    install_operator_pubkey(&signing);

    let conn = fresh_conn();
    // Sign a row with enabled = 0 (the seed-state shape).
    insert_seed_rule(&conn, "R001", false);
    sign_all_rules(&conn, &signing);

    // Direct SQL flip: enabled = 1. The signature was computed over
    // canonical_bytes_for_signing which INCLUDES `enabled`; flipping
    // it changes the canonical bytes, so verify fails, so the rule
    // is skipped, so `/tmp/x` is allowed.
    conn.execute(
        "UPDATE governance_rules SET enabled = 1 WHERE id = ?1",
        rusqlite::params!["R001"],
    )
    .unwrap();
    let decision = probe_tmp_write(&conn);
    assert_eq!(
        decision,
        Decision::Allow,
        "post-sign `UPDATE SET enabled = 1` must NOT bypass the substrate"
    );

    // Audit row was still emitted (the engine always records an
    // event even when the matched rule was filtered out at load).
    let audit_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM signed_events WHERE event_type = ?1",
            rusqlite::params![GOVERNANCE_CHECK_EVENT_TYPE],
            |r| r.get(0),
        )
        .unwrap();
    assert!(audit_count > 0, "audit row must be emitted regardless");

    uninstall_operator_pubkey();
}

// ---------------------------------------------------------------------------
// 4. keygen_writes_0600_and_load_refuses_open_permissions
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn keygen_writes_0600_and_load_refuses_open_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let tdir = tempfile::tempdir().unwrap();
    let key_path = tdir.path().join("operator.key");

    // Drive the CLI keygen via its public dispatch entry point. We
    // route stdout/stderr into Vec buffers so the test stays hermetic
    // (no actual file output on the test runner).
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    {
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        // We don't need a real DB for keygen — but the CLI dispatch
        // opens one. Use a scratch tempfile DB.
        let db_path = tdir.path().join("scratch.db");
        cli_rules::run(
            &db_path,
            cli_rules::RulesArgs {
                key_dir: None,
                action: cli_rules::RulesAction::Keygen {
                    out: Some(key_path.clone()),
                    force: false,
                },
            },
            false,
            &mut out,
        )
        .expect("keygen");
    }

    // Mode verification: 0600 on the private seed.
    let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "keygen must write 0o600, got {mode:o}");

    // Load round-trips.
    let _ = cli_rules::load_operator_signing_key(&key_path).expect("load 0600");

    // Loosen to 0644 — load must refuse with a clear `0600` mention.
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = cli_rules::load_operator_signing_key(&key_path).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("0600"), "error must mention 0600, got: {msg}");

    // Restore for tempdir cleanup.
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

// ---------------------------------------------------------------------------
// 5. sign_seed_idempotent
// ---------------------------------------------------------------------------

#[test]
fn sign_seed_idempotent() {
    let _g = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut csprng = rand_core::OsRng;
    let signing = SigningKey::generate(&mut csprng);

    let conn = fresh_conn();
    insert_seed_rule(&conn, "R001", false);
    insert_seed_rule(&conn, "R002", false);

    sign_all_rules(&conn, &signing);
    let sigs_first: Vec<_> = rules_store::list(&conn)
        .unwrap()
        .into_iter()
        .map(|r| (r.id, r.signature, r.attest_level))
        .collect();

    sign_all_rules(&conn, &signing);
    let sigs_second: Vec<_> = rules_store::list(&conn)
        .unwrap()
        .into_iter()
        .map(|r| (r.id, r.signature, r.attest_level))
        .collect();

    assert_eq!(
        sigs_first, sigs_second,
        "idempotent re-sign must preserve the existing signature bytes"
    );

    // Ensure operator_signed attest_level + 64-byte signature.
    for (id, sig, attest) in &sigs_second {
        assert_eq!(attest, "operator_signed", "rule {id} attest");
        assert_eq!(
            sig.as_ref().map(Vec::len),
            Some(ed25519_dalek::SIGNATURE_LENGTH),
            "rule {id} sig length"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Bonus — wrong-key scenario (the audit-grade "different operator key"
//    attempt). Mirrors test 2 but at the key-rotation surface instead of
//    the row-mutation surface. Explicitly documented so the audit can
//    point at this test for the "rotated key invalidates prior rules"
//    property.
// ---------------------------------------------------------------------------

#[test]
fn rotated_operator_key_invalidates_prior_signatures() {
    let _g = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut csprng = rand_core::OsRng;
    let original = SigningKey::generate(&mut csprng);
    let rotated = SigningKey::generate(&mut csprng);

    let conn = fresh_conn();
    insert_seed_rule(&conn, "R001", true);

    // Sign with the original key; install the original pubkey; rule
    // is enforced.
    sign_all_rules(&conn, &original);
    install_operator_pubkey(&original);
    assert!(matches!(probe_tmp_write(&conn), Decision::Refuse { .. }));

    // Operator rotates the pubkey (without re-signing). All prior
    // signatures become invalid; the rule is skipped; /tmp/x is
    // allowed. This is the same property `keygen --force` warns
    // about.
    install_operator_pubkey(&rotated);
    assert_eq!(
        probe_tmp_write(&conn),
        Decision::Allow,
        "rotated key must invalidate prior signatures"
    );

    uninstall_operator_pubkey();
}

// ---------------------------------------------------------------------------
// Compile-time anchor: we re-export the `Path` import to silence
// dead-code on platforms where the unix-only test is gated out.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn _path_use(p: &Path) -> &Path {
    p
}
