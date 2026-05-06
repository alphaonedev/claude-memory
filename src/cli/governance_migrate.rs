// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 K11 — `ai-memory governance migrate-to-permissions` CLI.
//!
//! Backward-compatibility shim during the v0.6 → v0.7 transition.
//! Operators with mature `[governance]` rulesets in `config.toml` get an
//! automated path to the K9 `[[permissions.rules]]` schema. The
//! translator is intentionally a thin TOML-to-TOML mapper: it does NOT
//! interact with the runtime `db::enforce_governance` gate, never
//! touches the SQLite database, and never mutates the loaded
//! `AppConfig`. Operators stay in control of when (or whether) the
//! emitted rules get pasted into their live config.
//!
//! ## Field mapping (`[governance.policy]` → `[[permissions.rules]]`)
//!
//! ```text
//!   policy.scope     → rule.namespace_pattern
//!   policy.action    → rule.op
//!   policy.role      → rule.agent_pattern   (preferred)
//!   policy.agent_id  → rule.agent_pattern   (fallback when role absent)
//!   policy.decision  → rule.decision
//! ```
//!
//! Unknown fields on a policy are dropped silently — the migrator's
//! contract is "translate the documented K11 mapping, nothing more". A
//! follow-up release can extend the field set without breaking existing
//! `[governance]` files because TOML deserialization is forgiving.
//!
//! ## Modes
//!
//! - **Dry-run (default).** Render the proposed `[[permissions.rules]]`
//!   block to stdout as TOML text. Nothing on disk is modified. Safe to
//!   pipe into `diff` against an existing `[permissions]` block.
//! - **`--config-out PATH`.** Write the rendered TOML to `PATH`. When
//!   `PATH` matches the loaded config file, the migrator does an
//!   in-place merge: every non-`[governance]` section of the original
//!   file is preserved verbatim, and the new `[[permissions.rules]]`
//!   array is appended (existing `[[permissions.rules]]` entries are
//!   preserved as well — this is an additive append, NOT a replace).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use serde::{Deserialize, Serialize};

use crate::cli::CliOutput;

// ---------------------------------------------------------------------------
// CLI arg surface
// ---------------------------------------------------------------------------

/// `ai-memory governance migrate-to-permissions` arguments.
#[derive(Args, Debug, Clone)]
pub struct MigrateToPermissionsArgs {
    /// Print the rendered `[[permissions.rules]]` block to stdout
    /// without writing anywhere. This is the default behaviour when
    /// `--config-out` is omitted; passing `--dry-run` explicitly is
    /// supported for callers who want the intent to be obvious.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Write the rendered `[[permissions.rules]]` block to this path.
    /// When the path matches the loaded config file, the migrator
    /// performs an in-place merge that preserves every other section.
    /// When the path is different, the rendered block is written
    /// standalone (overwriting any existing file at that path).
    #[arg(long, value_name = "PATH")]
    pub config_out: Option<PathBuf>,

    /// Override the loaded config file path. Defaults to
    /// `~/.config/ai-memory/config.toml` (the path
    /// [`crate::config::AppConfig::config_path`] returns).
    #[arg(long, value_name = "PATH")]
    pub config_in: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Wire format — `[governance]` (v0.6.x legacy)
// ---------------------------------------------------------------------------

/// Top-level `[governance]` section. The only field today is the
/// `policy` array; the wrapper exists so the deserializer can ignore
/// other unknown sub-keys an operator might have added.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyGovernance {
    /// Array of `[[governance.policy]]` entries in the loaded config.
    #[serde(default)]
    pub policy: Vec<LegacyGovernancePolicy>,
}

/// A single legacy governance policy. Mirrors the documented v0.6.x
/// field set; every field is optional so partial entries round-trip.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyGovernancePolicy {
    /// Namespace selector (glob-shaped string, e.g. `team/*`).
    #[serde(default)]
    pub scope: Option<String>,
    /// Operation gated by this policy: `write`, `delete`, `promote`,
    /// `recall`, etc. Translated 1:1 into `rule.op`.
    #[serde(default)]
    pub action: Option<String>,
    /// Role-based agent selector. When present, takes precedence over
    /// `agent_id` for `rule.agent_pattern`.
    #[serde(default)]
    pub role: Option<String>,
    /// Agent-id selector. Used as a fallback when `role` is absent.
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Decision returned when the policy matches: `allow`, `deny`,
    /// `ask`, etc. Forwarded verbatim to `rule.decision`.
    #[serde(default)]
    pub decision: Option<String>,
}

// ---------------------------------------------------------------------------
// Wire format — `[[permissions.rules]]` (v0.7.0 K9)
// ---------------------------------------------------------------------------

/// Container for `[[permissions.rules]]`. Used only by the migrator's
/// rendering path so the K9 module can keep its richer in-memory shape
/// without forcing the migrator to depend on it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionsBlock {
    /// `[[permissions.rules]]` array.
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
}

/// One rule in the K9 `[[permissions.rules]]` array.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionRule {
    /// Namespace glob the rule applies to.
    pub namespace_pattern: String,
    /// Operation the rule applies to (`write`, `delete`, …).
    pub op: String,
    /// Agent-id or role glob the rule applies to. Empty string when
    /// the source policy carried neither `role` nor `agent_id`.
    pub agent_pattern: String,
    /// Decision the rule returns when matched (`allow`, `deny`, `ask`).
    pub decision: String,
}

// ---------------------------------------------------------------------------
// Translation
// ---------------------------------------------------------------------------

/// Translate one [`LegacyGovernancePolicy`] into the K9
/// [`PermissionRule`] shape. Missing fields are filled with `"*"` for
/// pattern-shaped fields (the deny-first matcher treats `"*"` as
/// "match anything") and `"ask"` for the decision (matches the K9
/// "ask-by-default for ambiguous cases" default).
#[must_use]
pub fn translate_policy(p: &LegacyGovernancePolicy) -> PermissionRule {
    let agent_pattern = p
        .role
        .clone()
        .or_else(|| p.agent_id.clone())
        .unwrap_or_else(|| "*".to_string());
    PermissionRule {
        namespace_pattern: p.scope.clone().unwrap_or_else(|| "*".to_string()),
        op: p.action.clone().unwrap_or_else(|| "*".to_string()),
        agent_pattern,
        decision: p.decision.clone().unwrap_or_else(|| "ask".to_string()),
    }
}

/// Translate a [`LegacyGovernance`] section into a [`PermissionsBlock`].
#[must_use]
pub fn translate(legacy: &LegacyGovernance) -> PermissionsBlock {
    PermissionsBlock {
        rules: legacy.policy.iter().map(translate_policy).collect(),
    }
}

// ---------------------------------------------------------------------------
// Parse + render
// ---------------------------------------------------------------------------

/// Parse the `[governance]` section out of a raw config-toml string.
/// Returns an empty [`LegacyGovernance`] when the section is missing —
/// callers can detect "nothing to migrate" by checking
/// `result.policy.is_empty()`.
pub fn parse_legacy_governance(raw: &str) -> Result<LegacyGovernance> {
    let value: toml::Value = toml::from_str(raw).context("parse config.toml")?;
    let Some(gov) = value.get("governance") else {
        return Ok(LegacyGovernance::default());
    };
    let parsed: LegacyGovernance = gov.clone().try_into().context("parse [governance] block")?;
    Ok(parsed)
}

/// Render a [`PermissionsBlock`] as a `[[permissions.rules]]` TOML
/// fragment. The output is a standalone snippet — no `[permissions]`
/// table header, just the array entries in source order. Operators can
/// paste it into an existing `[permissions]` table or feed it into
/// `--config-out`.
#[must_use]
pub fn render_permissions_block(block: &PermissionsBlock) -> String {
    if block.rules.is_empty() {
        return "# v0.7.0 K11: no [governance] policies found — nothing to migrate.\n".to_string();
    }
    let mut out = String::new();
    out.push_str("# v0.7.0 K11: translated from legacy [[governance.policy]] entries.\n");
    out.push_str("# Mapping: scope→namespace_pattern, action→op,\n");
    out.push_str("#          role|agent_id→agent_pattern, decision→decision.\n");
    for rule in &block.rules {
        out.push_str("\n[[permissions.rules]]\n");
        out.push_str(&format!(
            "namespace_pattern = {}\n",
            toml_str(&rule.namespace_pattern)
        ));
        out.push_str(&format!("op = {}\n", toml_str(&rule.op)));
        out.push_str(&format!(
            "agent_pattern = {}\n",
            toml_str(&rule.agent_pattern)
        ));
        out.push_str(&format!("decision = {}\n", toml_str(&rule.decision)));
    }
    out
}

/// Quote a string the way TOML expects: basic-string with escaped
/// backslashes and quotes. Avoids pulling in `toml::ser` for a
/// four-line helper.
fn toml_str(s: &str) -> String {
    let escaped: String = s
        .chars()
        .flat_map(|c| match c {
            '\\' => vec!['\\', '\\'],
            '"' => vec!['\\', '"'],
            '\n' => vec!['\\', 'n'],
            '\r' => vec!['\\', 'r'],
            '\t' => vec!['\\', 't'],
            c => vec![c],
        })
        .collect();
    format!("\"{escaped}\"")
}

// ---------------------------------------------------------------------------
// In-place merge
// ---------------------------------------------------------------------------

/// Append the rendered `[[permissions.rules]]` block to an existing
/// config file's contents. The merge strategy is intentionally
/// conservative:
///
/// - Every section of the existing file is preserved verbatim
///   (including any pre-existing `[[permissions.rules]]` entries).
/// - The migrator block is appended at the end with a leading
///   `# --- migrated from [governance] (K11) ---` separator so a human
///   reader can see exactly which entries the migrator wrote.
///
/// This sidesteps the messy task of editing TOML in place (which would
/// strip comments and reorder keys) while still meeting the K11
/// "preserve other sections" contract.
#[must_use]
pub fn merge_in_place(existing: &str, rendered: &str) -> String {
    let mut out = String::with_capacity(existing.len() + rendered.len() + 64);
    out.push_str(existing);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\n# --- migrated from [governance] (v0.7.0 K11) ---\n");
    out.push_str(rendered);
    out
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// `ai-memory governance migrate-to-permissions` entry point.
///
/// Returns `Ok(())` after a successful dry-run / write. Errors propagate
/// for missing input files, parse failures, and IO write failures — the
/// caller exits non-zero in the standard way.
pub fn run(args: MigrateToPermissionsArgs, out: &mut CliOutput<'_>) -> Result<()> {
    let in_path = match args.config_in.clone() {
        Some(p) => p,
        None => crate::config::AppConfig::config_path()
            .context("no HOME — cannot resolve default config path; pass --config-in")?,
    };
    let raw = std::fs::read_to_string(&in_path)
        .with_context(|| format!("read config from {}", in_path.display()))?;
    let legacy = parse_legacy_governance(&raw)?;
    let block = translate(&legacy);
    let rendered = render_permissions_block(&block);

    // Dry-run is the default. We treat "no --config-out AND no
    // --dry-run" as dry-run too, matching the K11 spec.
    let dry_run = args.dry_run || args.config_out.is_none();
    if dry_run {
        // Print to stdout. The rendered block already ends in a
        // newline, so no extra `\n` here.
        write!(out.stdout, "{rendered}")?;
        return Ok(());
    }

    // Write path. Either standalone (different file) or in-place merge
    // (same file as the input). Compare canonical paths so a relative
    // and absolute reference to the same file still take the merge
    // branch.
    let out_path = args.config_out.clone().expect("checked above");
    let same_file = same_path(&in_path, &out_path);
    if same_file {
        let merged = merge_in_place(&raw, &rendered);
        std::fs::write(&out_path, merged)
            .with_context(|| format!("write merged config to {}", out_path.display()))?;
        writeln!(
            out.stdout,
            "merged {} migrated rule(s) into {}",
            block.rules.len(),
            out_path.display()
        )?;
    } else {
        std::fs::write(&out_path, &rendered)
            .with_context(|| format!("write rendered block to {}", out_path.display()))?;
        writeln!(
            out.stdout,
            "wrote {} migrated rule(s) to {}",
            block.rules.len(),
            out_path.display()
        )?;
    }

    if block.rules.is_empty() {
        // Surface the no-op as a non-fatal warning so operators don't
        // mistakenly assume the migration ran successfully when their
        // legacy config never had a `[governance]` block to begin with.
        writeln!(
            out.stderr,
            "warning: no [governance] policies found in {} — nothing migrated",
            in_path.display()
        )?;
    }

    Ok(())
}

/// Compare two paths for equality after canonicalization, falling back
/// to a literal-component compare when canonicalization fails (e.g. the
/// output file does not exist yet — that's still "same path" if the
/// strings agree).
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// Internal helper exposed for the integration tests so they can drive
/// the migrator with an explicit `--config-out` path without round-
/// tripping through clap. Returns the rendered block as a string for
/// post-write asserts.
#[doc(hidden)]
#[allow(dead_code)]
pub fn run_with_paths(
    in_path: &Path,
    config_out: Option<&Path>,
    dry_run: bool,
    out: &mut CliOutput<'_>,
) -> Result<String> {
    let raw = std::fs::read_to_string(in_path)
        .with_context(|| format!("read config from {}", in_path.display()))?;
    let legacy = parse_legacy_governance(&raw)?;
    let block = translate(&legacy);
    let rendered = render_permissions_block(&block);

    let dry = dry_run || config_out.is_none();
    if dry {
        write!(out.stdout, "{rendered}")?;
        return Ok(rendered);
    }

    let out_path = config_out.expect("checked above");
    if same_path(in_path, out_path) {
        let merged = merge_in_place(&raw, &rendered);
        std::fs::write(out_path, merged)
            .with_context(|| format!("write merged to {}", out_path.display()))?;
    } else if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent of {}", out_path.display()))?;
        std::fs::write(out_path, &rendered)
            .with_context(|| format!("write rendered to {}", out_path.display()))?;
    } else {
        std::fs::write(out_path, &rendered)
            .with_context(|| format!("write rendered to {}", out_path.display()))?;
    }
    writeln!(
        out.stdout,
        "wrote {} migrated rule(s) to {}",
        block.rules.len(),
        out_path.display()
    )?;
    Ok(rendered)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::TestEnv;

    fn sample_legacy_config() -> &'static str {
        r#"
# user config with a mature governance ruleset

[governance]

[[governance.policy]]
scope = "team/eng/*"
action = "write"
role = "engineer"
decision = "allow"

[[governance.policy]]
scope = "team/finance/*"
action = "delete"
agent_id = "alice"
decision = "ask"

[[governance.policy]]
scope = "*"
action = "promote"
decision = "deny"
"#
    }

    #[test]
    fn parse_three_policies() {
        let parsed = parse_legacy_governance(sample_legacy_config()).unwrap();
        assert_eq!(parsed.policy.len(), 3);
        assert_eq!(parsed.policy[0].scope.as_deref(), Some("team/eng/*"));
        assert_eq!(parsed.policy[0].role.as_deref(), Some("engineer"));
        assert_eq!(parsed.policy[1].agent_id.as_deref(), Some("alice"));
        assert_eq!(parsed.policy[2].decision.as_deref(), Some("deny"));
    }

    #[test]
    fn translate_role_wins_over_agent_id() {
        let p = LegacyGovernancePolicy {
            scope: Some("ns".into()),
            action: Some("write".into()),
            role: Some("ops".into()),
            agent_id: Some("alice".into()),
            decision: Some("allow".into()),
        };
        let r = translate_policy(&p);
        assert_eq!(r.namespace_pattern, "ns");
        assert_eq!(r.op, "write");
        assert_eq!(r.agent_pattern, "ops");
        assert_eq!(r.decision, "allow");
    }

    #[test]
    fn translate_falls_back_to_agent_id_when_role_absent() {
        let p = LegacyGovernancePolicy {
            scope: Some("ns".into()),
            action: Some("write".into()),
            role: None,
            agent_id: Some("alice".into()),
            decision: Some("allow".into()),
        };
        let r = translate_policy(&p);
        assert_eq!(r.agent_pattern, "alice");
    }

    #[test]
    fn translate_uses_safe_defaults_when_fields_missing() {
        let p = LegacyGovernancePolicy::default();
        let r = translate_policy(&p);
        assert_eq!(r.namespace_pattern, "*");
        assert_eq!(r.op, "*");
        assert_eq!(r.agent_pattern, "*");
        assert_eq!(r.decision, "ask");
    }

    #[test]
    fn render_emits_one_block_per_rule() {
        let parsed = parse_legacy_governance(sample_legacy_config()).unwrap();
        let block = translate(&parsed);
        let rendered = render_permissions_block(&block);
        assert_eq!(rendered.matches("[[permissions.rules]]").count(), 3);
        assert!(rendered.contains("namespace_pattern = \"team/eng/*\""));
        assert!(rendered.contains("agent_pattern = \"engineer\""));
        assert!(rendered.contains("agent_pattern = \"alice\""));
        assert!(rendered.contains("decision = \"deny\""));
    }

    #[test]
    fn render_empty_block_emits_comment() {
        let block = PermissionsBlock::default();
        let s = render_permissions_block(&block);
        assert!(s.contains("nothing to migrate"));
    }

    #[test]
    fn missing_governance_section_yields_empty() {
        let raw = "tier = \"semantic\"\n";
        let parsed = parse_legacy_governance(raw).unwrap();
        assert!(parsed.policy.is_empty());
    }

    #[test]
    fn merge_in_place_preserves_existing_then_appends() {
        let existing = "tier = \"semantic\"\n[scoring]\nlegacy_scoring = false\n";
        let rendered = "[[permissions.rules]]\nnamespace_pattern = \"a\"\n";
        let merged = merge_in_place(existing, rendered);
        assert!(merged.starts_with("tier = \"semantic\""));
        assert!(merged.contains("[scoring]"));
        assert!(merged.contains("[[permissions.rules]]"));
        assert!(merged.contains("--- migrated from [governance] (v0.7.0 K11) ---"));
    }

    #[test]
    fn run_with_paths_dry_run_writes_to_stdout() {
        let mut env = TestEnv::fresh();
        let cfg_path = env.db_path.parent().unwrap().join("config.toml");
        std::fs::write(&cfg_path, sample_legacy_config()).unwrap();
        let _ = {
            let mut o = env.output();
            run_with_paths(&cfg_path, None, true, &mut o).unwrap()
        };
        let stdout = env.stdout_str();
        assert_eq!(stdout.matches("[[permissions.rules]]").count(), 3);
    }

    #[test]
    fn run_with_paths_writes_to_named_file() {
        let mut env = TestEnv::fresh();
        let in_path = env.db_path.parent().unwrap().join("in.toml");
        let out_path = env.db_path.parent().unwrap().join("out.toml");
        std::fs::write(&in_path, sample_legacy_config()).unwrap();
        let _ = {
            let mut o = env.output();
            run_with_paths(&in_path, Some(&out_path), false, &mut o).unwrap()
        };
        let written = std::fs::read_to_string(&out_path).unwrap();
        assert_eq!(written.matches("[[permissions.rules]]").count(), 3);
        let parsed: toml::Value = toml::from_str(&written).unwrap();
        let rules = parsed["permissions"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 3);
    }

    #[test]
    fn run_with_paths_in_place_merge_preserves_other_sections() {
        let mut env = TestEnv::fresh();
        let cfg_path = env.db_path.parent().unwrap().join("cfg.toml");
        let mut original = String::from(sample_legacy_config());
        original.push_str("\n[scoring]\nlegacy_scoring = false\n");
        std::fs::write(&cfg_path, &original).unwrap();
        let _ = {
            let mut o = env.output();
            run_with_paths(&cfg_path, Some(&cfg_path), false, &mut o).unwrap()
        };
        let after = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(after.contains("[scoring]"));
        assert!(after.contains("legacy_scoring = false"));
        assert!(after.contains("[governance]"));
        assert_eq!(after.matches("[[permissions.rules]]").count(), 3);
    }
}
