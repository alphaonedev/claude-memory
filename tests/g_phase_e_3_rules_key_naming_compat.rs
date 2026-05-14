// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 G-PHASE-E-3 (issue #708) — `rules keygen` ↔ `rules enable
//! --sign` naming compatibility.
//!
//! Pre-#708 split: `ai-memory rules keygen` wrote
//! `<key-dir>/operator.key` (raw 32-byte private seed) + a sibling
//! `<key-dir>/operator.key.pub` (base64url no-pad encoded 32-byte
//! verifying key). But the mutation verbs `rules enable / disable /
//! remove --sign` loaded the operator key via `kp::load("operator",
//! <key-dir>)` which expects `<key-dir>/operator.priv` (raw 32-byte
//! seed) + `<key-dir>/operator.pub` (raw 32-byte verifying key). So
//! the documented one-liner `keygen → enable --sign` failed end-to-end
//! with a confusing "operator.priv missing" error.
//!
//! Fix: `load_operator_signing_key_from_dir` auto-detects which of the
//! two layouts is in use. Both are accepted. Tests below pin:
//!
//! 1. Layout 1 (legacy `operator.priv` + raw `operator.pub`) is
//!    accepted by `rules enable --sign`.
//! 2. Layout 2 (keygen-style `operator.key` + base64url
//!    `operator.key.pub`) is accepted by `rules enable --sign`.
//! 3. Neither layout present surfaces a typed error that names BOTH
//!    options so the operator can pick the right one.
//! 4. A `keygen → enable --sign` round-trip works end-to-end (the
//!    integration scenario the gap actually broke).

use ai_memory::cli::CliOutput;
use ai_memory::cli::rules as cli_rules;
use ai_memory::governance::rules_store::{self, Rule};
use base64::Engine;
use ed25519_dalek::SigningKey;
use std::fs;
use std::path::Path;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create the minimal `governance_rules` + `signed_events` schema in a
/// freshly-opened file DB. `cli_rules::run` re-opens the path, so the
/// schema must be on disk before we dispatch.
fn init_governance_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
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
             prev_hash BLOB,
             sequence INTEGER
         );",
    )
    .unwrap();
    // Seed one rule we can `enable` later.
    rules_store::insert(
        &conn,
        &Rule {
            id: "R-PHASE-E-3".into(),
            kind: "filesystem_write".into(),
            matcher: r#"{"glob":"/scratch/**"}"#.into(),
            severity: "refuse".into(),
            reason: "phase-e-3 test rule".into(),
            namespace: "_global".into(),
            created_by: "system:test".into(),
            created_at: 0,
            enabled: false,
            signature: None,
            attest_level: "unsigned".into(),
        },
    )
    .unwrap();
}

/// Stage layout (1): `<key_dir>/operator.priv` + `<key_dir>/operator.pub`
/// holding the raw 32-byte private seed and raw 32-byte verifying key.
/// `kp::load("operator", <dir>)` expects exactly this shape.
fn stage_layout_priv_pub(key_dir: &Path) -> SigningKey {
    let mut csprng = rand_core::OsRng;
    let signing = SigningKey::generate(&mut csprng);
    let priv_path = key_dir.join("operator.priv");
    let pub_path = key_dir.join("operator.pub");
    fs::write(&priv_path, signing.to_bytes()).unwrap();
    fs::write(&pub_path, signing.verifying_key().to_bytes()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&priv_path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    signing
}

/// Stage layout (2): `<key_dir>/operator.key` (raw 32B seed) +
/// `<key_dir>/operator.key.pub` (base64url no-pad 32B verifier). This
/// is the shape `ai-memory rules keygen` produces.
fn stage_layout_key_keypub(key_dir: &Path) -> SigningKey {
    let mut csprng = rand_core::OsRng;
    let signing = SigningKey::generate(&mut csprng);
    let priv_path = key_dir.join("operator.key");
    let pub_path = key_dir.join("operator.key.pub");
    fs::write(&priv_path, signing.to_bytes()).unwrap();
    let pub_b64 =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signing.verifying_key().to_bytes());
    fs::write(&pub_path, pub_b64).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&priv_path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    signing
}

/// Drive the `rules enable --sign` verb end-to-end. Returns Result so
/// callers can inspect failure messages.
fn run_enable(db_path: &Path, key_dir: &Path, rule_id: &str) -> anyhow::Result<()> {
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut out = CliOutput {
        stdout: &mut stdout,
        stderr: &mut stderr,
    };
    cli_rules::run(
        db_path,
        cli_rules::RulesArgs {
            key_dir: Some(key_dir.to_path_buf()),
            action: cli_rules::RulesAction::Enable {
                id: rule_id.to_string(),
                sign: true,
            },
        },
        false,
        &mut out,
    )
}

/// Read the rule back to verify the enable flag flipped to 1 + a
/// signature landed.
fn read_rule(db_path: &Path, id: &str) -> Rule {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    rules_store::get(&conn, id).unwrap().expect("rule exists")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn enable_accepts_legacy_priv_pub_layout() {
    let tdir = tempfile::tempdir().unwrap();
    let db_path = tdir.path().join("rules.db");
    init_governance_db(&db_path);
    let key_dir = tdir.path().join("keys-priv-pub");
    fs::create_dir_all(&key_dir).unwrap();
    let _signing = stage_layout_priv_pub(&key_dir);

    run_enable(&db_path, &key_dir, "R-PHASE-E-3").expect("enable with operator.priv layout");
    let rule = read_rule(&db_path, "R-PHASE-E-3");
    assert!(rule.enabled, "enable must flip the rule on");
    assert_eq!(rule.attest_level, "operator_signed");
    assert!(rule.signature.is_some());
}

#[test]
fn enable_accepts_keygen_key_keypub_layout() {
    let tdir = tempfile::tempdir().unwrap();
    let db_path = tdir.path().join("rules.db");
    init_governance_db(&db_path);
    let key_dir = tdir.path().join("keys-keygen");
    fs::create_dir_all(&key_dir).unwrap();
    let _signing = stage_layout_key_keypub(&key_dir);

    run_enable(&db_path, &key_dir, "R-PHASE-E-3").expect("enable with operator.key layout");
    let rule = read_rule(&db_path, "R-PHASE-E-3");
    assert!(rule.enabled, "enable must flip the rule on");
    assert_eq!(rule.attest_level, "operator_signed");
    assert!(rule.signature.is_some());
}

#[test]
fn enable_with_neither_layout_present_errors_mentions_both_options() {
    let tdir = tempfile::tempdir().unwrap();
    let db_path = tdir.path().join("rules.db");
    init_governance_db(&db_path);
    let key_dir = tdir.path().join("keys-empty");
    fs::create_dir_all(&key_dir).unwrap();
    // No staged files.

    let err = run_enable(&db_path, &key_dir, "R-PHASE-E-3").unwrap_err();
    let msg = format!("{err:#}");
    // Both naming options must appear in the error so the operator can
    // pick the right one to materialise.
    assert!(
        msg.contains("operator.priv") && msg.contains("operator.pub"),
        "error must mention operator.priv/operator.pub layout; got: {msg}"
    );
    assert!(
        msg.contains("operator.key") && msg.contains("operator.key.pub"),
        "error must mention operator.key/operator.key.pub layout; got: {msg}"
    );
    // And the `governance.no_operator_key` typed prefix must still
    // appear so downstream tooling (CI, audit scripts) can pattern-
    // match on the canonical error class.
    assert!(
        msg.contains("governance.no_operator_key"),
        "error must carry the governance.no_operator_key class; got: {msg}"
    );
}

#[test]
fn keygen_then_enable_roundtrip_works() {
    // The integration scenario the gap actually broke: an operator
    // runs `rules keygen` and immediately tries `rules enable --sign`.
    // Pre-#708, the latter failed because it expected `operator.priv`.
    let tdir = tempfile::tempdir().unwrap();
    let db_path = tdir.path().join("rules.db");
    init_governance_db(&db_path);
    let key_dir = tdir.path().join("keys-roundtrip");
    fs::create_dir_all(&key_dir).unwrap();
    let key_path = key_dir.join("operator.key");

    // 1. Run keygen via the CLI dispatch.
    {
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
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
    assert!(key_path.exists(), "keygen must write operator.key");
    assert!(
        key_dir.join("operator.key.pub").exists(),
        "keygen must write operator.key.pub"
    );

    // 2. Now drive enable --sign against the same key_dir. Without the
    // G-PHASE-E-3 fix, this errors with `operator.priv missing`.
    run_enable(&db_path, &key_dir, "R-PHASE-E-3")
        .expect("keygen→enable roundtrip must succeed post-#708");
    let rule = read_rule(&db_path, "R-PHASE-E-3");
    assert!(rule.enabled);
    assert_eq!(rule.attest_level, "operator_signed");
}

#[test]
fn enable_rejects_mismatched_key_keypub_pair() {
    // Defence-in-depth: if the public sidecar was tampered (e.g. swapped
    // with a different keypair's verifier), the load must refuse rather
    // than sign with a key the public side doesn't verify against.
    let tdir = tempfile::tempdir().unwrap();
    let db_path = tdir.path().join("rules.db");
    init_governance_db(&db_path);
    let key_dir = tdir.path().join("keys-mismatch");
    fs::create_dir_all(&key_dir).unwrap();
    let _signing = stage_layout_key_keypub(&key_dir);

    // Overwrite operator.key.pub with the verifier from a DIFFERENT keypair.
    let mut csprng = rand_core::OsRng;
    let attacker = SigningKey::generate(&mut csprng);
    let attacker_pub_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(attacker.verifying_key().to_bytes());
    fs::write(key_dir.join("operator.key.pub"), attacker_pub_b64).unwrap();

    let err = run_enable(&db_path, &key_dir, "R-PHASE-E-3").unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("does not match"),
        "mismatched pub must refuse; got: {msg}"
    );
}
