// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Round-2 F11 — `forget --pattern` / `forget --tier` without
//! `--namespace` requires `--confirm-global` to proceed.
//!
//! Pre-F11 the CLI happily globbed across every namespace in the
//! database when the operator omitted `--namespace`. A typo in
//! `--pattern` could wipe the working set with no confirmation.
//!
//! These tests invoke the forget handler programmatically and assert:
//! 1. `--pattern <p>` without `--namespace` and without
//!    `--confirm-global` errors with the documented message.
//! 2. `--tier <t>` without `--namespace` and without
//!    `--confirm-global` errors the same way.
//! 3. `--confirm-global` lets the same delete proceed.
//! 4. `--namespace <ns>` alone is accepted (bounded blast radius).
//! 5. `--namespace <ns>` + `--pattern` is accepted (also bounded).

use ai_memory::cli::CliOutput;
use ai_memory::cli::forget::{
    ForgetArgs, cmd_forget, global_scope_forget_error_message, requires_global_confirmation,
};
use ai_memory::models::ConfidenceSource;

fn args_blank() -> ForgetArgs {
    ForgetArgs {
        namespace: None,
        pattern: None,
        tier: None,
        confirm_global: false,
    }
}

#[test]
fn predicate_pattern_without_namespace_demands_confirmation() {
    let mut a = args_blank();
    a.pattern = Some("apple".into());
    assert!(requires_global_confirmation(&a));
}

#[test]
fn predicate_tier_without_namespace_demands_confirmation() {
    let mut a = args_blank();
    a.tier = Some("long".into());
    assert!(requires_global_confirmation(&a));
}

#[test]
fn predicate_namespace_alone_is_safe() {
    let mut a = args_blank();
    a.namespace = Some("ns".into());
    assert!(!requires_global_confirmation(&a));
}

#[test]
fn predicate_namespace_with_pattern_is_safe() {
    let mut a = args_blank();
    a.namespace = Some("ns".into());
    a.pattern = Some("apple".into());
    assert!(!requires_global_confirmation(&a));
}

#[test]
fn predicate_confirm_flag_lifts_safety_rail() {
    let mut a = args_blank();
    a.pattern = Some("apple".into());
    a.confirm_global = true;
    assert!(!requires_global_confirmation(&a));
}

#[test]
fn cmd_forget_errors_on_global_pattern_without_confirm() {
    // The handler must bail before touching the database, so we
    // intentionally point it at a path that won't be opened on the
    // refusal path. (The error fires before `db::open`.)
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut out = CliOutput::from_std(&mut stdout, &mut stderr);

    let mut a = args_blank();
    a.pattern = Some("apple".into());

    let res = cmd_forget(
        std::path::Path::new("/nonexistent/db.sqlite"),
        &a,
        true,
        &mut out,
    );
    let err = res.expect_err("expected error when --confirm-global is missing");
    let msg = err.to_string();
    assert!(msg.contains("--confirm-global"), "got: {msg}");
    assert!(msg.contains("--namespace"), "got: {msg}");
    // Pin the documented wording so a future refactor doesn't drift.
    assert_eq!(msg, global_scope_forget_error_message());
}

#[test]
fn cmd_forget_errors_on_global_tier_without_confirm() {
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut out = CliOutput::from_std(&mut stdout, &mut stderr);

    let mut a = args_blank();
    a.tier = Some("long".into());

    let res = cmd_forget(
        std::path::Path::new("/nonexistent/db.sqlite"),
        &a,
        true,
        &mut out,
    );
    let err = res.expect_err("expected error when --confirm-global is missing");
    assert!(err.to_string().contains("--confirm-global"));
}

#[test]
fn cmd_forget_proceeds_with_confirm_global() {
    // With --confirm-global set the safety rail steps aside; the
    // handler proceeds to the underlying delete. We seed three rows
    // across three namespaces and assert the global pattern delete
    // matches the two `apple-*` rows.
    use ai_memory::db;
    use ai_memory::models::{Memory, Tier};
    use chrono::Utc;
    use uuid::Uuid;

    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("test.db");
    let conn = db::open(&db_path).unwrap();
    for (ns, title) in [
        ("alpha", "apple-a"),
        ("beta", "apple-b"),
        ("gamma", "banana-c"),
    ] {
        let m = Memory {
            id: Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: ns.into(),
            title: title.into(),
            content: "x".into(),
            tags: vec![],
            priority: 1,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
            reflection_depth: 0,
            memory_kind: ai_memory::models::MemoryKind::Observation,
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
        db::insert(&conn, &m).unwrap();
    }
    drop(conn);

    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut out = CliOutput::from_std(&mut stdout, &mut stderr);

    let mut a = args_blank();
    a.pattern = Some("apple".into());
    a.confirm_global = true;

    cmd_forget(&db_path, &a, true, &mut out).expect("forget must proceed");
    let stdout_str = String::from_utf8(stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(stdout_str.trim()).unwrap();
    // Both apple rows match across namespaces.
    assert_eq!(v["deleted"].as_u64().unwrap(), 2);
}
