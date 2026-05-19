// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 7th-form closeout (issue #760) — `ai-memory governance
//! install-defaults` CLI subcommand.
//!
//! Bulk-activates the four seeded operator hard rules (R001-R004) that
//! migration `0024_v07_governance_rules.sql` lands at `enabled = 0`:
//!
//! | Rule | Kind             | Matcher                                       | Reason                                              |
//! |------|------------------|-----------------------------------------------|-----------------------------------------------------|
//! | R001 | filesystem_write | `{"glob":"/tmp/**"}`                          | No `/tmp` writes (project hard rule, #691).         |
//! | R002 | filesystem_write | `{"glob":"/var/tmp/**"}`                      | No `/var/tmp` writes.                                |
//! | R003 | filesystem_write | `{"glob":"/private/tmp/**"}`                  | No `/private/tmp` writes (macOS realpath of `/tmp`).|
//! | R004 | process_spawn    | `{"binary":"cargo","disk_free_min_gib":20}`   | Refuse `cargo` on low-disk (<20 GiB) host.          |
//!
//! ## Operator flow
//!
//! ```text
//!   $ ai-memory governance install-defaults
//!   The following seed rules will be enabled (R001-R004):
//!     R001  filesystem_write  /tmp/**           refuse
//!     R002  filesystem_write  /var/tmp/**       refuse
//!     R003  filesystem_write  /private/tmp/**   refuse
//!     R004  process_spawn     cargo (<20 GiB)   refuse
//!   Proceed? [y/N]: y
//!   Activated 4 rule(s).
//! ```
//!
//! ## Why not `rules enable` per-id?
//!
//! `ai-memory rules enable <id> --sign` is the per-rule path; it
//! requires the operator's Ed25519 key on disk and re-signs each row.
//! For the bootstrap step where the operator just wants the seeded
//! hard rules ON, `install-defaults` is a single confirmed batch.
//! It does NOT touch the signature column — the seeded rows ship
//! `attest_level = 'unsigned'` and the operator may pair this verb
//! with a separate `ai-memory rules sign-seed --key …` to upgrade the
//! attestation level.
//!
//! ## Audit honesty
//!
//! Activating the rule is **mechanical at the harness hook boundary**
//! (per `src/governance/agent_action.rs` module docs). It is not a
//! "100% can't be bypassed" claim — see the audit-honest wording in
//! the agent_action module and `docs/governance/agent-action-rules.md`.

use anyhow::{Context, Result};
use clap::Args;
use rusqlite::params;

use crate::cli::CliOutput;

/// The four seed rule ids defined in migration `0024_v07_governance_rules.sql`.
/// Kept here as a typed constant so unit tests can iterate without
/// relying on the migration text.
pub const SEED_RULE_IDS: &[&str] = &["R001", "R002", "R003", "R004"];

/// CLI args for `ai-memory governance install-defaults`.
#[derive(Args, Debug, Clone)]
pub struct InstallDefaultsArgs {
    /// Skip the interactive `Proceed? [y/N]:` confirmation prompt.
    /// Required for non-interactive contexts (CI, scripts).
    #[arg(long)]
    pub yes: bool,

    /// Emit a JSON envelope instead of the human-readable summary.
    /// Stable wire shape: `{ "verb": "governance.install-defaults",
    /// "result": { "activated": [...], "missing": [...], "already_enabled": [...] } }`.
    #[arg(long)]
    pub json: bool,
}

/// Outcome of the install-defaults run; surfaced both to the JSON
/// envelope and to the human summary line.
#[derive(Debug, Default, serde::Serialize)]
pub struct InstallDefaultsReport {
    /// Rule ids that flipped from `enabled = 0` to `enabled = 1`.
    pub activated: Vec<String>,
    /// Rule ids that were already enabled at the start.
    pub already_enabled: Vec<String>,
    /// Rule ids that were not present in the DB (migration skipped or
    /// row hand-deleted). Surfaced so the operator can investigate.
    pub missing: Vec<String>,
}

/// Dispatch entry called from the daemon-runtime `GovernanceAction`
/// match arm.
///
/// # Errors
///
/// Returns an error if the DB cannot be opened, the SELECT/UPDATE
/// queries fail, or the operator declines the prompt and the JSON
/// envelope cannot be serialised. Declining the prompt is NOT an error
/// — it returns `Ok(())` after writing `aborted: true` to stdout.
pub fn run(
    db_path: &std::path::Path,
    args: InstallDefaultsArgs,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let conn = rusqlite::Connection::open(db_path).with_context(|| {
        format!(
            "governance install-defaults: open db at {}",
            db_path.display()
        )
    })?;

    // Confirm the four rules exist + grab their current state so we
    // can render the preview block and decide what to activate.
    let mut preview: Vec<SeedRuleRow> = Vec::with_capacity(SEED_RULE_IDS.len());
    let mut missing: Vec<String> = Vec::new();
    for id in SEED_RULE_IDS {
        match load_seed_row(&conn, id)? {
            Some(row) => preview.push(row),
            None => missing.push((*id).to_string()),
        }
    }

    // Interactive prompt unless --yes / --json was supplied.
    if !args.yes {
        // JSON-mode callers MUST pass --yes; an interactive prompt on
        // a JSON path would corrupt the envelope. Refuse early.
        if args.json {
            anyhow::bail!("governance install-defaults: --json requires --yes (non-interactive)");
        }
        render_preview(out, &preview, &missing)?;
        if !confirm_proceed(out)? {
            writeln!(out.stdout, "Aborted. No rules were activated.")?;
            return Ok(());
        }
    }

    // Flip enabled = 1 on every row whose enabled = 0.
    let mut report = InstallDefaultsReport {
        missing: missing.clone(),
        ..Default::default()
    };
    for row in &preview {
        if row.enabled {
            report.already_enabled.push(row.id.clone());
            continue;
        }
        let affected = conn
            .execute(
                "UPDATE governance_rules SET enabled = 1 WHERE id = ?1 AND enabled = 0",
                params![row.id],
            )
            .with_context(|| format!("install-defaults: UPDATE enabled=1 for {}", row.id))?;
        if affected > 0 {
            report.activated.push(row.id.clone());
        }
    }

    if args.json {
        let envelope = serde_json::json!({
            "verb": "governance.install-defaults",
            "result": &report,
        });
        writeln!(
            out.stdout,
            "{}",
            serde_json::to_string(&envelope)
                .context("install-defaults: serialise JSON envelope")?
        )?;
    } else {
        writeln!(
            out.stdout,
            "Activated {} rule(s); {} already-enabled; {} missing.",
            report.activated.len(),
            report.already_enabled.len(),
            report.missing.len(),
        )?;
        if !report.activated.is_empty() {
            writeln!(out.stdout, "  activated: {}", report.activated.join(", "))?;
        }
        if !report.missing.is_empty() {
            writeln!(out.stdout, "  missing:   {}", report.missing.join(", "))?;
        }
    }
    Ok(())
}

/// Snapshot of one row from `governance_rules` for the preview block.
struct SeedRuleRow {
    id: String,
    kind: String,
    matcher: String,
    severity: String,
    enabled: bool,
}

fn load_seed_row(conn: &rusqlite::Connection, id: &str) -> Result<Option<SeedRuleRow>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT id, kind, matcher, severity, enabled \
         FROM governance_rules WHERE id = ?1",
        params![id],
        |r| {
            Ok(SeedRuleRow {
                id: r.get::<_, String>(0)?,
                kind: r.get::<_, String>(1)?,
                matcher: r.get::<_, String>(2)?,
                severity: r.get::<_, String>(3)?,
                enabled: r.get::<_, i64>(4)? != 0,
            })
        },
    )
    .optional()
    .with_context(|| format!("install-defaults: SELECT governance_rules id={id}"))
}

fn render_preview(
    out: &mut CliOutput<'_>,
    preview: &[SeedRuleRow],
    missing: &[String],
) -> Result<()> {
    writeln!(
        out.stdout,
        "The following seed rules will be enabled (R001-R004):"
    )?;
    for row in preview {
        let state = if row.enabled {
            "already-on"
        } else {
            "will-enable"
        };
        writeln!(
            out.stdout,
            "  {:<5} {:<17} {:<32} {:<8} [{}]",
            row.id, row.kind, row.matcher, row.severity, state,
        )?;
    }
    if !missing.is_empty() {
        writeln!(
            out.stdout,
            "Warning: the following seed rule ids were not found in the DB: {}",
            missing.join(", ")
        )?;
        writeln!(
            out.stdout,
            "  (re-run `ai-memory schema-init` or check migration 0024 applied)"
        )?;
    }
    Ok(())
}

fn confirm_proceed(out: &mut CliOutput<'_>) -> Result<bool> {
    write!(out.stdout, "Proceed? [y/N]: ")?;
    out.stdout.flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("install-defaults: read stdin")?;
    let trimmed = answer.trim().to_ascii_lowercase();
    Ok(matches!(trimmed.as_str(), "y" | "yes"))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Seed `db_path` with the `governance_rules` table + the four
    /// seeded rows at `enabled = 0`. Avoids pulling in the full
    /// migration ladder (which would also drag in fts5 / hnsw).
    fn seed_db_at(db_path: &std::path::Path) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS governance_rules (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 matcher TEXT NOT NULL,
                 severity TEXT NOT NULL,
                 reason TEXT NOT NULL,
                 namespace TEXT NOT NULL DEFAULT '_global',
                 created_by TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 enabled INTEGER NOT NULL DEFAULT 1,
                 signature BLOB,
                 attest_level TEXT NOT NULL DEFAULT 'unsigned'
             );",
        )
        .unwrap();
        for (id, kind, matcher) in [
            ("R001", "filesystem_write", r#"{"glob":"/tmp/**"}"#),
            ("R002", "filesystem_write", r#"{"glob":"/var/tmp/**"}"#),
            ("R003", "filesystem_write", r#"{"glob":"/private/tmp/**"}"#),
            (
                "R004",
                "process_spawn",
                r#"{"binary":"cargo","disk_free_min_gib":20}"#,
            ),
        ] {
            conn.execute(
                "INSERT INTO governance_rules (id, kind, matcher, severity, reason, \
                 namespace, created_by, created_at, enabled, signature, attest_level) \
                 VALUES (?1, ?2, ?3, 'refuse', 'seed', '_global', 'system:seed', 0, 0, NULL, 'unsigned')",
                params![id, kind, matcher],
            )
            .unwrap();
        }
    }

    /// Build an `InstallDefaultsArgs` with `--yes` set so the prompt
    /// is skipped.
    fn yes_args() -> InstallDefaultsArgs {
        InstallDefaultsArgs {
            yes: true,
            json: false,
        }
    }

    #[test]
    fn seed_rule_ids_is_the_canonical_four() {
        assert_eq!(SEED_RULE_IDS, &["R001", "R002", "R003", "R004"]);
    }

    /// Build a fresh on-disk DB in a scoped tempdir and seed it.
    fn fresh_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("governance.db");
        seed_db_at(&db_path);
        (dir, db_path)
    }

    #[test]
    fn install_defaults_flips_enabled_on_seeded_rows() {
        let (_dir, db_path) = fresh_db();
        // Sanity: confirm all four start disabled.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            for id in SEED_RULE_IDS {
                let enabled: i64 = conn
                    .query_row(
                        "SELECT enabled FROM governance_rules WHERE id = ?1",
                        params![id],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(enabled, 0, "rule {id} must start disabled");
            }
        }

        let mut so = Vec::<u8>::new();
        let mut se = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut so, &mut se);
        run(&db_path, yes_args(), &mut out).unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        for id in SEED_RULE_IDS {
            let enabled: i64 = conn
                .query_row(
                    "SELECT enabled FROM governance_rules WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(enabled, 1, "rule {id} must be activated");
        }
        let stdout = String::from_utf8(so).unwrap();
        assert!(stdout.contains("Activated 4 rule(s)"));
    }

    #[test]
    fn install_defaults_idempotent_when_already_enabled() {
        let (_dir, db_path) = fresh_db();
        // Pre-flip all rows to enabled = 1.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "UPDATE governance_rules SET enabled = 1 WHERE id IN ('R001','R002','R003','R004')",
                [],
            )
            .unwrap();
        }

        let mut so = Vec::<u8>::new();
        let mut se = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut so, &mut se);
        run(&db_path, yes_args(), &mut out).unwrap();

        let stdout = String::from_utf8(so).unwrap();
        assert!(stdout.contains("Activated 0 rule(s)"));
        assert!(stdout.contains("4 already-enabled"));
    }

    #[test]
    fn install_defaults_reports_missing_rows() {
        let (_dir, db_path) = fresh_db();
        // Hand-delete R003.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute("DELETE FROM governance_rules WHERE id = 'R003'", [])
                .unwrap();
        }

        let mut so = Vec::<u8>::new();
        let mut se = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut so, &mut se);
        run(&db_path, yes_args(), &mut out).unwrap();

        let stdout = String::from_utf8(so).unwrap();
        assert!(
            stdout.contains("1 missing") || stdout.contains("missing:   R003"),
            "stdout was: {stdout}",
        );
    }

    #[test]
    fn json_mode_emits_envelope() {
        let (_dir, db_path) = fresh_db();
        let mut so = Vec::<u8>::new();
        let mut se = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut so, &mut se);
        run(
            &db_path,
            InstallDefaultsArgs {
                yes: true,
                json: true,
            },
            &mut out,
        )
        .unwrap();
        let stdout = String::from_utf8(so).unwrap();
        let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
        assert_eq!(v["verb"], "governance.install-defaults");
        assert_eq!(v["result"]["activated"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn json_without_yes_refuses() {
        let (_dir, db_path) = fresh_db();
        let mut so = Vec::<u8>::new();
        let mut se = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut so, &mut se);
        let err = run(
            &db_path,
            InstallDefaultsArgs {
                yes: false,
                json: true,
            },
            &mut out,
        )
        .expect_err("expected refusal");
        assert!(
            err.to_string().contains("--json requires --yes"),
            "got: {err}"
        );
    }

    // ------------------------------------------------------------------
    // Coverage-uplift block (2026-05-19): exercise helper functions
    // (render_preview, load_seed_row) and additional run() branches that
    // the original 6 tests did not cover.
    // ------------------------------------------------------------------

    #[test]
    fn render_preview_emits_one_row_per_seeded_rule() {
        let preview = vec![
            SeedRuleRow {
                id: "R001".into(),
                kind: "filesystem_write".into(),
                matcher: r#"{"glob":"/tmp/**"}"#.into(),
                severity: "refuse".into(),
                enabled: false,
            },
            SeedRuleRow {
                id: "R002".into(),
                kind: "filesystem_write".into(),
                matcher: r#"{"glob":"/var/tmp/**"}"#.into(),
                severity: "refuse".into(),
                enabled: true,
            },
        ];
        let missing: Vec<String> = vec![];

        let mut so = Vec::<u8>::new();
        let mut se = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut so, &mut se);
        render_preview(&mut out, &preview, &missing).unwrap();
        drop(out);
        let stdout = String::from_utf8(so).unwrap();
        // Header line is present.
        assert!(stdout.contains("The following seed rules will be enabled"));
        // Both rule ids appear in the preview.
        assert!(stdout.contains("R001"));
        assert!(stdout.contains("R002"));
        // Disabled row prints "will-enable"; enabled row prints
        // "already-on" — both arms exercised.
        assert!(stdout.contains("will-enable"));
        assert!(stdout.contains("already-on"));
        // No "Warning" line — the missing list is empty.
        assert!(!stdout.contains("Warning"));
    }

    #[test]
    fn render_preview_emits_warning_block_when_missing_present() {
        let preview: Vec<SeedRuleRow> = vec![];
        let missing = vec!["R003".to_string(), "R004".to_string()];

        let mut so = Vec::<u8>::new();
        let mut se = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut so, &mut se);
        render_preview(&mut out, &preview, &missing).unwrap();
        drop(out);
        let stdout = String::from_utf8(so).unwrap();
        // Warning + remediation lines fire.
        assert!(stdout.contains("Warning"));
        assert!(stdout.contains("R003"));
        assert!(stdout.contains("R004"));
        assert!(stdout.contains("re-run `ai-memory schema-init`"));
    }

    #[test]
    fn load_seed_row_returns_none_for_unknown_id() {
        let (_dir, db_path) = fresh_db();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let row = load_seed_row(&conn, "R999-nonexistent").unwrap();
        assert!(row.is_none());
    }

    #[test]
    fn load_seed_row_returns_typed_row_with_disabled_default() {
        let (_dir, db_path) = fresh_db();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let row = load_seed_row(&conn, "R001").unwrap();
        let row = row.expect("R001 seeded");
        assert_eq!(row.id, "R001");
        assert_eq!(row.kind, "filesystem_write");
        assert_eq!(row.severity, "refuse");
        assert!(!row.enabled, "seeded rows ship at enabled = 0");
    }

    #[test]
    fn install_defaults_human_render_emits_activated_and_missing_lines() {
        // Drives both `if !report.activated.is_empty()` and
        // `if !report.missing.is_empty()` writeln arms (lines ~173-178)
        // in a single run by hand-deleting one row before invoking run.
        let (_dir, db_path) = fresh_db();
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute("DELETE FROM governance_rules WHERE id = 'R002'", [])
                .unwrap();
        }
        let mut so = Vec::<u8>::new();
        let mut se = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut so, &mut se);
        run(&db_path, yes_args(), &mut out).unwrap();
        drop(out);
        let stdout = String::from_utf8(so).unwrap();
        // Summary header with non-zero counts.
        assert!(stdout.contains("Activated 3 rule(s)"));
        assert!(stdout.contains("1 missing"));
        // Per-id "activated:" line fires when activated is non-empty.
        assert!(stdout.contains("  activated:"));
        // Per-id "missing:" line fires when missing is non-empty.
        assert!(stdout.contains("  missing:"));
        assert!(stdout.contains("R002"));
    }

    #[test]
    fn install_defaults_json_envelope_pins_wire_shape_when_partial_missing() {
        // Hand-delete two rows, run with --json --yes, parse envelope.
        let (_dir, db_path) = fresh_db();
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute(
                "DELETE FROM governance_rules WHERE id IN ('R003','R004')",
                [],
            )
            .unwrap();
        }
        let mut so = Vec::<u8>::new();
        let mut se = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut so, &mut se);
        run(
            &db_path,
            InstallDefaultsArgs {
                yes: true,
                json: true,
            },
            &mut out,
        )
        .unwrap();
        drop(out);
        let stdout = String::from_utf8(so).unwrap();
        let v: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
        assert_eq!(v["verb"], "governance.install-defaults");
        let result = &v["result"];
        // R001 + R002 activated; R003 + R004 missing.
        let activated = result["activated"].as_array().unwrap();
        assert_eq!(activated.len(), 2);
        let missing = result["missing"].as_array().unwrap();
        assert_eq!(missing.len(), 2);
        assert!(missing.iter().any(|x| x == "R003"));
        assert!(missing.iter().any(|x| x == "R004"));
    }

    #[test]
    fn run_propagates_open_error_for_non_existent_db_with_unwritable_parent() {
        // db path under a non-existent directory cannot be opened —
        // exercises the with_context closure on Connection::open (lines
        // 101-106). The closure body fires only on the error path.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("nonexistent-dir/missing.db");
        let mut so = Vec::<u8>::new();
        let mut se = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut so, &mut se);
        let err = run(&db_path, yes_args(), &mut out).expect_err("must fail");
        // The with_context closure runs and the formatted context is
        // attached to the error chain.
        let chain = format!("{err:#}");
        assert!(
            chain.contains("governance install-defaults: open db at"),
            "expected context, got: {chain}"
        );
    }
}
