// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_forget` migration. See `cli::store` for the design pattern.
//!
//! ## Round-2 F11 — global-scope safety rail
//!
//! `forget --pattern <p>` and `forget --tier <t>` without `--namespace`
//! delete across every namespace in the database. That has been the
//! contract since v0.6.x, but it is a sharp edge: a typo in `--pattern`
//! can wipe the operator's working set with no confirmation.
//!
//! v0.7.0 adds a `--confirm-global` flag. When `--namespace` is omitted
//! AND (`--pattern` or `--tier` is set) the handler refuses to proceed
//! unless `--confirm-global` is also present. `forget --id` is fine
//! because the id is unambiguous; `forget --namespace` is fine because
//! the blast radius is bounded.

use crate::cli::CliOutput;
use crate::{db, models};
use anyhow::{Result, bail};
use clap::Args;
use models::Tier;
use std::path::Path;

#[derive(Args)]
pub struct ForgetArgs {
    #[arg(long, short)]
    pub namespace: Option<String>,
    #[arg(long, short)]
    pub pattern: Option<String>,
    #[arg(long, short)]
    pub tier: Option<String>,
    /// Round-2 F11 — required when `--namespace` is omitted and either
    /// `--pattern` or `--tier` is set, since those flags then delete
    /// across every namespace in the database. Without `--namespace`
    /// the handler refuses to run without this confirmation.
    #[arg(long, default_value_t = false)]
    pub confirm_global: bool,
}

/// Round-2 F11 — return the safety-rail error string when the operator
/// invoked a global-scope `forget` without the `--confirm-global`
/// opt-in. Pulled out so the integration test in
/// `tests/round2_f11_forget_safety.rs` can assert on the exact
/// wording without coupling to handler-internal control flow.
#[must_use]
pub fn global_scope_forget_error_message() -> &'static str {
    "global-scope forget requires --confirm-global; restrict with --namespace=<ns> for safety"
}

/// Round-2 F11 — predicate used by both the CLI handler and the
/// integration test. Returns `true` when the args describe a
/// global-scope delete (no `--namespace`, but `--pattern` or `--tier`
/// set) and `--confirm-global` was NOT supplied.
#[must_use]
pub fn requires_global_confirmation(args: &ForgetArgs) -> bool {
    let no_namespace = args.namespace.is_none();
    let has_global_filter = args.pattern.is_some() || args.tier.is_some();
    no_namespace && has_global_filter && !args.confirm_global
}

/// `forget` handler. Deletes (and archives) memories matching at least
/// one of namespace/pattern/tier. CLI always passes `archive=true`.
pub fn cmd_forget(
    db_path: &Path,
    args: &ForgetArgs,
    json_out: bool,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    // Round-2 F11 — refuse global-scope deletes without explicit
    // confirmation. The error is propagated via `bail!` (not stderr +
    // process::exit) so test code can assert on the message without
    // killing the test process.
    if requires_global_confirmation(args) {
        bail!(global_scope_forget_error_message());
    }

    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let conn = db::open(db_path)?;
    match db::forget(
        &conn,
        args.namespace.as_deref(),
        args.pattern.as_deref(),
        tier.as_ref(),
        true, // always archive from CLI
    ) {
        Ok(n) => {
            if json_out {
                writeln!(out.stdout, "{}", serde_json::json!({"deleted": n}))?;
            } else {
                writeln!(out.stdout, "forgot {n} memories")?;
            }
        }
        Err(e) => {
            writeln!(out.stderr, "error: {e}")?;
            std::process::exit(1);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    fn args() -> ForgetArgs {
        ForgetArgs {
            namespace: None,
            pattern: None,
            tier: None,
            confirm_global: false,
        }
    }

    #[test]
    fn test_forget_by_namespace() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "alpha", "a", "ca");
        let _ = seed_memory(&db, "beta", "b", "cb");
        let mut a = args();
        a.namespace = Some("alpha".to_string());
        {
            let mut out = env.output();
            cmd_forget(&db, &a, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["deleted"].as_u64().unwrap(), 1);
        // beta still present.
        let conn = db::open(&db).unwrap();
        let still = db::list(
            &conn,
            Some("beta"),
            None,
            10,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(still.len(), 1);
    }

    #[test]
    fn test_forget_by_pattern() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "ns", "apple pie", "yum");
        let _ = seed_memory(&db, "ns", "banana split", "also yum");
        let mut a = args();
        a.pattern = Some("apple".to_string());
        // Round-2 F11 — `forget --pattern` without `--namespace` is a
        // global delete and now requires the operator opt-in.
        a.confirm_global = true;
        {
            let mut out = env.output();
            cmd_forget(&db, &a, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["deleted"].as_u64().unwrap(), 1);
    }

    #[test]
    fn test_forget_by_tier() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id_long = seed_memory(&db, "ns", "long-row", "x");
        let _ = seed_memory(&db, "ns", "mid-row", "y");
        {
            let conn = db::open(&db).unwrap();
            db::update(
                &conn,
                &id_long,
                None,
                None,
                Some(&Tier::Long),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        }
        let mut a = args();
        a.tier = Some("long".to_string());
        // Round-2 F11 — `forget --tier` without `--namespace` requires
        // the global confirmation flag.
        a.confirm_global = true;
        {
            let mut out = env.output();
            cmd_forget(&db, &a, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["deleted"].as_u64().unwrap(), 1);
    }

    #[test]
    fn test_forget_combined_filters() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "alpha", "apple-1", "x");
        let _ = seed_memory(&db, "beta", "apple-2", "y");
        let _ = seed_memory(&db, "alpha", "banana", "z");
        let mut a = args();
        a.namespace = Some("alpha".to_string());
        a.pattern = Some("apple".to_string());
        {
            let mut out = env.output();
            cmd_forget(&db, &a, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        // Only the alpha+apple row should be removed.
        assert_eq!(v["deleted"].as_u64().unwrap(), 1);
        let conn = db::open(&db).unwrap();
        let beta_apples = db::list(
            &conn,
            Some("beta"),
            None,
            10,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(beta_apples.len(), 1);
    }

    #[test]
    fn test_forget_no_filter_errors_or_no_op() {
        // db::forget bails when no filter is supplied. The handler turns
        // that into an stderr line + std::process::exit(1) — which we
        // can't observe in-process. Surface the bail by calling db::forget
        // directly so the test asserts the underlying contract.
        let env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "ns", "x", "y");
        let conn = db::open(&db).unwrap();
        let res = db::forget(&conn, None, None, None, false);
        assert!(res.is_err(), "no-filter forget must error");
        assert!(
            res.unwrap_err()
                .to_string()
                .contains("at least one of namespace, pattern, or tier")
        );
    }

    // ---- Round-2 F11 safety-rail unit tests ------------------------------

    #[test]
    fn requires_global_confirmation_pattern_no_namespace() {
        let mut a = args();
        a.pattern = Some("apple".into());
        assert!(requires_global_confirmation(&a));
    }

    #[test]
    fn requires_global_confirmation_tier_no_namespace() {
        let mut a = args();
        a.tier = Some("long".into());
        assert!(requires_global_confirmation(&a));
    }

    #[test]
    fn does_not_require_confirmation_when_namespace_present() {
        let mut a = args();
        a.namespace = Some("ns".into());
        a.pattern = Some("apple".into());
        assert!(!requires_global_confirmation(&a));
    }

    #[test]
    fn does_not_require_confirmation_when_only_namespace_set() {
        let mut a = args();
        a.namespace = Some("ns".into());
        // No pattern, no tier — `forget --namespace=ns` is bounded.
        assert!(!requires_global_confirmation(&a));
    }

    #[test]
    fn does_not_require_confirmation_when_confirm_flag_set() {
        let mut a = args();
        a.pattern = Some("apple".into());
        a.confirm_global = true;
        assert!(!requires_global_confirmation(&a));
    }

    #[test]
    fn cmd_forget_refuses_global_pattern_without_confirm() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "ns", "apple pie", "yum");
        let mut a = args();
        a.pattern = Some("apple".into());
        let mut out = env.output();
        let res = cmd_forget(&db, &a, true, &mut out);
        assert!(res.is_err(), "expected refusal");
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("--confirm-global"), "got: {msg}");
    }

    #[test]
    fn cmd_forget_proceeds_with_confirm_global() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "ns", "apple pie", "yum");
        let _ = seed_memory(&db, "other", "apple cake", "yum");
        let mut a = args();
        a.pattern = Some("apple".into());
        a.confirm_global = true;
        {
            let mut out = env.output();
            cmd_forget(&db, &a, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        // Both rows match — global delete succeeded under explicit
        // confirmation.
        assert_eq!(v["deleted"].as_u64().unwrap(), 2);
    }

    #[test]
    fn test_forget_text_output_count() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "ns", "a", "x");
        let _ = seed_memory(&db, "ns", "b", "y");
        let mut a = args();
        a.namespace = Some("ns".to_string());
        {
            let mut out = env.output();
            cmd_forget(&db, &a, false, &mut out).unwrap();
        }
        let stdout = env.stdout_str();
        assert!(stdout.contains("forgot 2 memories"), "got: {stdout}");
    }
}
