// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #800 — Batman Mode activation regression suite.
//!
//! Covers the seven cracks closed by the install + acceptance work:
//!
//! 1. `ai-memory namespace {set,get,clear}-standard` CLI surface
//!    (replaces the MCP-stdio JSON-RPC dance for binding a
//!    `GovernancePolicy` to a namespace).
//! 2. `ai-memory namespace batman-policy` helper that prints the
//!    canonical Batman `GovernancePolicy` JSON blob.
//! 3. Critical security fix — `rules add` / `rules enable` /
//!    `rules disable` previously signed over `canonical_bytes`
//!    (without `enabled`) while `verify_rule_signature` reads
//!    `canonical_bytes_for_signing` (with `enabled`). Signatures
//!    NEVER validated; the L1-6 gate silently skipped every
//!    operator-signed rule; Form 7 enforcement returned `allow` for
//!    every action. This file pins the post-fix round-trip:
//!    sign-via-CLI ⇒ verify-from-disk ⇒ refuse-enforcement.
//!
//! Tests below drive the CLI handlers directly (no subprocess spawn)
//! against a tempdir-scoped operator key + an in-process SQLite
//! connection. Each test owns its own conn + key dir so they run in
//! parallel safely under `cargo test`.

use std::path::PathBuf;
use std::sync::Once;

use ai_memory::cli::namespace as ns_cli;
use ai_memory::cli::rules as rules_cli;
use ai_memory::cli::CliOutput;
use ai_memory::db;
use ai_memory::governance::agent_action::{AgentAction, Decision, check_agent_action};
use ai_memory::governance::rules_store;
use serde_json::{Value, json};
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

/// Create a fresh tempdir with an Ed25519 operator key pair (both the
/// `<dir>/operator.key` parent-dir form sign-seed expects AND the
/// `<dir>/keys/operator.{key,key.pub}` form `rules enable` expects).
fn setup_operator_key() -> (TempDir, PathBuf) {
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;

    let tmp = TempDir::new().unwrap();
    let key_dir = tmp.path().join("keys");
    std::fs::create_dir_all(&key_dir).unwrap();

    let mut rng = OsRng;
    let signing = SigningKey::generate(&mut rng);
    let verifying = signing.verifying_key();
    let pub_b64 = {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(verifying.to_bytes())
    };

    // The two locations the v0.7.0 binary checks:
    //   - sign-seed reads `<config-dir>/operator.key` (parent of keys/)
    //   - rules enable / disable / add read `<key-dir>/operator.key`
    //     (or `operator.priv` legacy form)
    for path in [
        tmp.path().join("operator.key"),
        key_dir.join("operator.key"),
    ] {
        std::fs::write(&path, signing.to_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    for path in [
        tmp.path().join("operator.key.pub"),
        key_dir.join("operator.key.pub"),
    ] {
        std::fs::write(&path, pub_b64.as_bytes()).unwrap();
    }

    (tmp, key_dir)
}

fn fresh_db() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("ai-memory.db");
    // Touch the DB so schema migrations land.
    let _conn = db::open(&path).unwrap();
    (tmp, path)
}

// ---------------------------------------------------------------------------
// CRACK 1 + 3 — CLI verb for namespace_set_standard / get / clear / policy
// ---------------------------------------------------------------------------

/// `ai-memory namespace batman-policy` prints a JSON blob with every
/// expected field at the documented defaults.
#[test]
fn namespace_batman_policy_emits_canonical_governance_blob() {
    init_tracing();
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput {
        stdout: &mut stdout,
        stderr: &mut stderr,
    };
    let args = ns_cli::NamespaceArgs {
        action: ns_cli::NamespaceAction::BatmanPolicy {
            atomise_threshold: 512,
            atom_max_tokens: 256,
            max_reflection_depth: 3,
            classify_mode: "regex_then_llm".into(),
        },
    };
    let (_db_dir, db_path) = fresh_db();
    ns_cli::run(&db_path, args, false, &mut out).expect("batman-policy must succeed");

    let s = String::from_utf8(stdout).unwrap();
    let parsed: Value = serde_json::from_str(s.trim()).unwrap();
    assert_eq!(parsed["auto_atomise"], json!(true));
    assert_eq!(parsed["auto_atomise_mode"], json!("synchronous"));
    assert_eq!(parsed["auto_atomise_threshold_cl100k"], json!(512));
    assert_eq!(parsed["auto_atomise_max_atom_tokens"], json!(256));
    assert_eq!(parsed["auto_classify_kind"], json!("regex_then_llm"));
    assert_eq!(parsed["max_reflection_depth"], json!(3));
    // Standard governance fields the v0.6.x policy validator requires.
    assert_eq!(parsed["write"], json!("owner"));
    assert_eq!(parsed["promote"], json!("any"));
    assert_eq!(parsed["delete"], json!("owner"));
    assert_eq!(parsed["approver"], json!("human"));
    assert_eq!(parsed["inherit"], json!(true));
}

/// `batman-policy` honors `--classify-mode regex_only` (cheaper Form 6 path).
#[test]
fn namespace_batman_policy_classify_mode_regex_only() {
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput {
        stdout: &mut stdout,
        stderr: &mut stderr,
    };
    let args = ns_cli::NamespaceArgs {
        action: ns_cli::NamespaceAction::BatmanPolicy {
            atomise_threshold: 1024,
            atom_max_tokens: 512,
            max_reflection_depth: 5,
            classify_mode: "regex_only".into(),
        },
    };
    let (_db_dir, db_path) = fresh_db();
    ns_cli::run(&db_path, args, false, &mut out).unwrap();
    let parsed: Value = serde_json::from_str(String::from_utf8(stdout).unwrap().trim()).unwrap();
    assert_eq!(parsed["auto_classify_kind"], json!("regex_only"));
    assert_eq!(parsed["auto_atomise_threshold_cl100k"], json!(1024));
    assert_eq!(parsed["max_reflection_depth"], json!(5));
}

/// `get-standard` on a namespace with no standard returns the empty
/// envelope (standard_id = null).
#[test]
fn namespace_get_standard_returns_null_when_unbound() {
    let (_db_dir, db_path) = fresh_db();
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput {
        stdout: &mut stdout,
        stderr: &mut stderr,
    };
    let args = ns_cli::NamespaceArgs {
        action: ns_cli::NamespaceAction::GetStandard {
            namespace: "nope".into(),
            inherit: false,
        },
    };
    ns_cli::run(&db_path, args, false, &mut out).unwrap();
    let s = String::from_utf8(stdout).unwrap();
    assert!(s.contains("has no standard set"), "got: {s}");
}

/// JSON mode on `get-standard --inherit` returns the chain envelope.
#[test]
fn namespace_get_standard_json_inherit_returns_chain() {
    let (_db_dir, db_path) = fresh_db();
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput {
        stdout: &mut stdout,
        stderr: &mut stderr,
    };
    let args = ns_cli::NamespaceArgs {
        action: ns_cli::NamespaceAction::GetStandard {
            namespace: "leaf/branch/root".into(),
            inherit: true,
        },
    };
    ns_cli::run(&db_path, args, true, &mut out).unwrap();
    let s = String::from_utf8(stdout).unwrap();
    let parsed: Value = serde_json::from_str(s.trim()).unwrap();
    assert_eq!(parsed["namespace"], json!("leaf/branch/root"));
    assert!(parsed["chain"].is_array());
}

/// `set-standard` then `get-standard` round-trips the binding +
/// surfaces the merged governance policy.
#[test]
fn namespace_set_then_get_standard_round_trip() {
    let (_db_dir, db_path) = fresh_db();
    let conn = db::open(&db_path).unwrap();

    // Create a standard memory directly via the DB layer so we don't
    // depend on the store CLI.
    use ai_memory::models::{Memory, Tier};
    use chrono::Utc;
    let std_id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let mem = Memory {
        id: std_id.clone(),
        tier: Tier::Long,
        namespace: "round-trip-ns".into(),
        title: "standard memory".into(),
        content: "Batman-active standard policy carrier".into(),
        tags: vec![],
        priority: 10,
        confidence: 1.0,
        source: "test".into(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({}),
        ..Memory::default()
    };
    db::insert(&conn, &mem).unwrap();
    drop(conn);

    // Bind the namespace to the memory + inject Batman governance.
    let governance = serde_json::to_string(&json!({
        "auto_atomise": true,
        "auto_atomise_mode": "synchronous",
        "auto_classify_kind": "regex_then_llm",
        "max_reflection_depth": 3,
        "write": "owner",
        "promote": "any",
        "delete": "owner",
        "approver": "human",
        "inherit": true,
    }))
    .unwrap();

    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput {
        stdout: &mut stdout,
        stderr: &mut stderr,
    };
    ns_cli::run(
        &db_path,
        ns_cli::NamespaceArgs {
            action: ns_cli::NamespaceAction::SetStandard {
                namespace: "round-trip-ns".into(),
                id: std_id.clone(),
                parent: None,
                governance: Some(governance),
            },
        },
        true,
        &mut out,
    )
    .expect("set-standard must succeed");
    let set_resp: Value = serde_json::from_str(String::from_utf8(stdout).unwrap().trim()).unwrap();
    assert_eq!(set_resp["set"], json!(true));
    assert_eq!(set_resp["standard_id"], json!(std_id));

    // Read it back.
    let mut stdout2: Vec<u8> = Vec::new();
    let mut stderr2: Vec<u8> = Vec::new();
    let mut out2 = CliOutput {
        stdout: &mut stdout2,
        stderr: &mut stderr2,
    };
    ns_cli::run(
        &db_path,
        ns_cli::NamespaceArgs {
            action: ns_cli::NamespaceAction::GetStandard {
                namespace: "round-trip-ns".into(),
                inherit: false,
            },
        },
        true,
        &mut out2,
    )
    .unwrap();
    let get_resp: Value =
        serde_json::from_str(String::from_utf8(stdout2).unwrap().trim()).unwrap();
    assert_eq!(get_resp["standard_id"], json!(std_id));
    assert_eq!(
        get_resp["governance"]["auto_classify_kind"],
        json!("regex_then_llm")
    );
    assert_eq!(get_resp["governance"]["auto_atomise"], json!(true));
    assert_eq!(get_resp["governance"]["max_reflection_depth"], json!(3));

    // Clear.
    let mut stdout3: Vec<u8> = Vec::new();
    let mut stderr3: Vec<u8> = Vec::new();
    let mut out3 = CliOutput {
        stdout: &mut stdout3,
        stderr: &mut stderr3,
    };
    ns_cli::run(
        &db_path,
        ns_cli::NamespaceArgs {
            action: ns_cli::NamespaceAction::ClearStandard {
                namespace: "round-trip-ns".into(),
            },
        },
        true,
        &mut out3,
    )
    .unwrap();
    let clr_resp: Value =
        serde_json::from_str(String::from_utf8(stdout3).unwrap().trim()).unwrap();
    assert_eq!(clr_resp["cleared"], json!(true));
    assert_eq!(clr_resp["namespace"], json!("round-trip-ns"));
}

/// `set-standard --governance` rejects malformed JSON without writing
/// anything to the DB.
#[test]
fn namespace_set_standard_invalid_governance_errors() {
    let (_db_dir, db_path) = fresh_db();
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput {
        stdout: &mut stdout,
        stderr: &mut stderr,
    };
    let err = ns_cli::run(
        &db_path,
        ns_cli::NamespaceArgs {
            action: ns_cli::NamespaceAction::SetStandard {
                namespace: "x".into(),
                id: "nonexistent".into(),
                parent: None,
                governance: Some("not-json-at-all".into()),
            },
        },
        false,
        &mut out,
    )
    .expect_err("must reject invalid governance JSON");
    assert!(
        format!("{err:#}").contains("--governance must be a valid JSON object"),
        "expected a governance-JSON parse error, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// CRACK — rules add / enable / disable signature-bytes mismatch fix
// ---------------------------------------------------------------------------

/// Pin the canonical-bytes fix: `rules enable --sign` must produce a
/// signature that `verify_rule_signature` accepts. Pre-fix, the signer
/// used `canonical_bytes` (without `enabled`) while the verifier used
/// `canonical_bytes_for_signing` (with `enabled`), so every
/// operator-signed rule was silently skipped by the L1-6 gate.
#[test]
fn rules_enable_signed_signature_verifies_against_operator_pubkey() {
    init_tracing();
    let (_key_tmp, key_dir) = setup_operator_key();
    let (_db_tmp, db_path) = fresh_db();
    let conn = db::open(&db_path).unwrap();

    // Seed a single rule via the rules_store API (mirrors what the
    // seed-migration would do, but lets us pick the matcher).
    let rule = rules_store::Rule {
        id: "R-test".into(),
        kind: "filesystem_write".into(),
        matcher: r#"{"glob":"/tmp/**"}"#.into(),
        severity: "refuse".into(),
        reason: "test rule".into(),
        namespace: "_global".into(),
        created_by: "test".into(),
        created_at: 0,
        enabled: false,
        signature: None,
        attest_level: "unsigned".into(),
    };
    rules_store::insert(&conn, &rule).unwrap();
    drop(conn);

    // Drive `ai-memory rules enable --id R-test --sign` directly.
    let args = rules_cli::RulesArgs {
        key_dir: Some(key_dir.clone()),
        action: rules_cli::RulesAction::Enable {
            id: "R-test".into(),
            sign: true,
        },
    };
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput {
        stdout: &mut stdout,
        stderr: &mut stderr,
    };
    rules_cli::run(&db_path, args, false, &mut out).expect("enable --sign must succeed");

    // Now load the rule back + verify the signature against the
    // operator pubkey using the same code path the L1-6 gate uses.
    let conn2 = db::open(&db_path).unwrap();
    let updated = rules_store::get(&conn2, "R-test").unwrap().unwrap();
    assert!(updated.enabled);
    assert_eq!(updated.attest_level, "operator_signed");
    let sig = updated.signature.as_ref().expect("signature must be set");
    assert_eq!(sig.len(), 64, "Ed25519 signatures are 64 bytes");

    // Load the operator pubkey from disk (the same path
    // `resolve_operator_pubkey` reads).
    let pubkey = {
        use base64::Engine;
        let pub_path = key_dir.join("operator.key.pub");
        let pub_b64 = std::fs::read_to_string(&pub_path).unwrap();
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(pub_b64.trim())
            .unwrap();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&raw);
        ed25519_dalek::VerifyingKey::from_bytes(&arr).unwrap()
    };

    rules_store::verify_rule_signature(&updated, &pubkey)
        .expect("PR #800 fix: enable-produced signature must verify against the operator pubkey");
}

/// Parity test for the Disable verb — also signed, also must verify.
#[test]
fn rules_disable_signed_signature_verifies_against_operator_pubkey() {
    let (_key_tmp, key_dir) = setup_operator_key();
    let (_db_tmp, db_path) = fresh_db();
    let conn = db::open(&db_path).unwrap();
    let rule = rules_store::Rule {
        id: "R-dis".into(),
        kind: "filesystem_write".into(),
        matcher: r#"{"glob":"/var/tmp/**"}"#.into(),
        severity: "refuse".into(),
        reason: "disable test".into(),
        namespace: "_global".into(),
        created_by: "test".into(),
        created_at: 0,
        enabled: true,
        signature: None,
        attest_level: "unsigned".into(),
    };
    rules_store::insert(&conn, &rule).unwrap();
    drop(conn);

    let args = rules_cli::RulesArgs {
        key_dir: Some(key_dir.clone()),
        action: rules_cli::RulesAction::Disable {
            id: "R-dis".into(),
            sign: true,
        },
    };
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput {
        stdout: &mut stdout,
        stderr: &mut stderr,
    };
    rules_cli::run(&db_path, args, false, &mut out).expect("disable --sign must succeed");

    let conn2 = db::open(&db_path).unwrap();
    let updated = rules_store::get(&conn2, "R-dis").unwrap().unwrap();
    assert!(!updated.enabled);
    let pubkey = {
        use base64::Engine;
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(std::fs::read_to_string(key_dir.join("operator.key.pub")).unwrap().trim())
            .unwrap();
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&raw);
        ed25519_dalek::VerifyingKey::from_bytes(&arr).unwrap()
    };
    rules_store::verify_rule_signature(&updated, &pubkey)
        .expect("disable signature must verify");
}

/// End-to-end: enable a rule via the CLI, then `check_agent_action`
/// must return `Refuse` (proves the L1-6 gate doesn't skip our
/// freshly-signed rule). Pre-fix this returned `Allow` because the
/// signature never validated.
#[test]
fn rules_enable_then_check_agent_action_refuses_matching_path() {
    let (_key_tmp, key_dir) = setup_operator_key();
    let (_db_tmp, db_path) = fresh_db();
    let conn = db::open(&db_path).unwrap();

    // Use a fresh rule id (R001..R004 are seeded by migrations on
    // db::open() and would collide on insert).
    rules_store::insert(
        &conn,
        &rules_store::Rule {
            id: "R-test-tmp".into(),
            kind: "filesystem_write".into(),
            matcher: r#"{"glob":"/tmp/**"}"#.into(),
            severity: "refuse".into(),
            reason: "no /tmp writes".into(),
            namespace: "_global".into(),
            created_by: "seed".into(),
            created_at: 0,
            enabled: false,
            signature: None,
            attest_level: "unsigned".into(),
        },
    )
    .unwrap();
    drop(conn);

    // Enable + sign via CLI.
    rules_cli::run(
        &db_path,
        rules_cli::RulesArgs {
            key_dir: Some(key_dir.clone()),
            action: rules_cli::RulesAction::Enable {
                id: "R-test-tmp".into(),
                sign: true,
            },
        },
        false,
        &mut CliOutput {
            stdout: &mut Vec::new(),
            stderr: &mut Vec::new(),
        },
    )
    .unwrap();

    // Point `resolve_operator_pubkey` at our test key dir via the
    // AI_MEMORY_OPERATOR_PUBKEY env var (no global state).
    let pub_b64 =
        std::fs::read_to_string(key_dir.join("operator.key.pub")).unwrap();
    // SAFETY: This test owns the env var read by the L1-6 gate; we
    // restore it after the assertion below. cargo test runs tests in
    // multiple threads inside the same process, so we keep the var
    // scoped to a critical section guarded by a mutex.
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap();
    let prev = std::env::var("AI_MEMORY_OPERATOR_PUBKEY").ok();
    unsafe {
        std::env::set_var("AI_MEMORY_OPERATOR_PUBKEY", pub_b64.trim());
    }

    let conn2 = db::open(&db_path).unwrap();
    let decision = check_agent_action(
        &conn2,
        "test-agent",
        &AgentAction::FilesystemWrite {
            path: PathBuf::from("/tmp/foo.txt"),
            byte_estimate: None,
        },
    )
    .unwrap();

    // Restore env var before any assertion that might panic.
    unsafe {
        match prev {
            Some(v) => std::env::set_var("AI_MEMORY_OPERATOR_PUBKEY", v),
            None => std::env::remove_var("AI_MEMORY_OPERATOR_PUBKEY"),
        }
    }

    match decision {
        Decision::Refuse { rule_id, .. } => {
            // R001 OR R-test-tmp both match /tmp/**; the seeded R001
            // wins first (alphabetical). The point of the test is
            // that *some* signed rule fires — not that our specific
            // id wins. Pre-fix this returned Allow entirely.
            assert!(
                rule_id == "R001" || rule_id == "R-test-tmp",
                "expected refuse from R001 or R-test-tmp, got rule_id={rule_id}"
            );
        }
        other => panic!(
            "expected Refuse, got {other:?} — the L1-6 gate is skipping the rule, \
             which means the canonical-bytes fix did not land"
        ),
    }
}
