// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `ai-memory boot` — universal session-boot context primitive (issue #487).
//!
//! Every AI-agent integration recipe (Claude Code SessionStart hook, Cursor
//! `.cursorrules` boot directive, Cline / Continue / Windsurf system-message,
//! Codex CLI / Claude Agent SDK / OpenAI Apps SDK programmatic prepend,
//! OpenClaw built-in, local models via LM Studio / Ollama / vLLM) calls this
//! same subcommand and consumes its stdout as the agent's first-turn context.
//!
//! Boot deliberately does **not** load the embedder. It returns the
//! most-recently-accessed memories in the inferred namespace, falls back to
//! the most-recently-accessed memories globally if the namespace is empty,
//! and clamps output to a token budget so a misconfigured agent can't bloat
//! its first turn.
//!
//! Failure modes are graceful by default:
//! - DB unavailable + `--quiet`: exit 0, empty stdout (a hook that fails
//!   here would otherwise wedge the agent's session).
//! - DB unavailable + no `--quiet`: write the error to stderr, emit only
//!   the header on stdout, still exit 0.
//! - No memories found: emit the header (or nothing with `--no-header`),
//!   exit 0.

use crate::cli::CliOutput;
use crate::cli::helpers::{auto_namespace, human_age, id_short};
use crate::{db, models, toon};
use anyhow::Result;
use clap::Args;
use models::Tier;
use std::path::Path;

/// Default budget — large enough for ~10 toon-compact rows, small enough that
/// a misconfigured hook can't wedge the first turn with megabytes of context.
const DEFAULT_BUDGET_TOKENS: usize = 4096;

/// Approximate tokens-per-character for cl100k_base / English text. Used for
/// the cheap budget clamp. Real tokenization happens elsewhere (recall_hybrid);
/// boot's budget is advisory and only needs to be in the right order of
/// magnitude to bound output cost.
const TOKENS_PER_CHAR: f32 = 0.25;

/// Output formats supported by `ai-memory boot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootFormat {
    /// Human-readable bulleted list (the default — works in any agent's
    /// system message and is easiest to scan).
    Text,
    /// JSON object: `{namespace, count, memories: [...]}`. For programmatic
    /// integrations (Claude Agent SDK, Apps SDK, Codex CLI prepend).
    Json,
    /// TOON-compact (the canonical token-efficient memory format).
    /// Mirrors the wire shape `memory_recall` returns over MCP.
    Toon,
}

impl BootFormat {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "toon" | "toon-compact" | "toon_compact" => Ok(Self::Toon),
            other => Err(anyhow::anyhow!(
                "unknown --format value: {other} (expected: text | json | toon)"
            )),
        }
    }
}

/// Args for `ai-memory boot`. Every field has a defaulted value so the
/// subcommand is safe to invoke with no arguments — that is the contract
/// every integration recipe relies on.
#[derive(Args, Debug)]
pub struct BootArgs {
    /// Override the inferred namespace. Default: derived from the current
    /// working directory via the same `auto_namespace` helper used by
    /// `ai-memory store` (git remote name → cwd basename → "global").
    #[arg(long)]
    pub namespace: Option<String>,
    /// Maximum number of memories to return. Clamped to `[1, 50]`.
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
    /// Approximate token budget for the rendered output. Cumulative
    /// character count divided by 4 ≈ tokens; boot stops adding rows
    /// when the next row would exceed the budget. Set to 0 to disable.
    #[arg(long, default_value_t = DEFAULT_BUDGET_TOKENS)]
    pub budget_tokens: usize,
    /// Output format: `text` (default), `json`, or `toon`.
    #[arg(long, default_value = "text")]
    pub format: String,
    /// Suppress the `# ai-memory boot context (...)` header line.
    /// Useful when the integration recipe wraps boot output inside
    /// its own framing.
    #[arg(long, default_value_t = false)]
    pub no_header: bool,
    /// Exit 0 with empty stdout if the DB is unavailable or no memories
    /// are found. Without this flag, errors land on stderr and stdout
    /// gets the header only. Hooks should pass `--quiet` so a failed
    /// boot never wedges the agent's first turn.
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
    /// Override `auto_namespace`'s working-directory inference. Useful
    /// when the hook fires before the agent has chdir'd into the
    /// project root.
    #[arg(long, value_name = "PATH")]
    pub cwd: Option<std::path::PathBuf>,
}

/// Resolve the boot namespace. Explicit `--namespace` wins; otherwise
/// `auto_namespace` runs against the optional `--cwd` (or the current
/// process's CWD if unset).
fn resolve_namespace(args: &BootArgs) -> String {
    if let Some(ref ns) = args.namespace {
        return ns.clone();
    }
    if let Some(ref cwd) = args.cwd {
        let _ = std::env::set_current_dir(cwd);
    }
    auto_namespace()
}

/// Pull the boot set from the DB. Two-stage:
///   1. List most-recently-accessed memories in the resolved namespace.
///   2. If empty, fall back to the most-recently-accessed memories at
///      tier=Long globally (cross-project context for greenfield checkouts).
fn fetch_boot_memories(
    conn: &rusqlite::Connection,
    namespace: &str,
    limit: usize,
) -> Result<(Vec<models::Memory>, String)> {
    // Stage 1: namespace-scoped list.
    let primary = db::list(
        conn,
        Some(namespace),
        None,
        limit,
        0,
        None,
        None,
        None,
        None,
        None,
    )?;
    if !primary.is_empty() {
        return Ok((primary, namespace.to_string()));
    }
    // Stage 2: global tier=Long fallback. The "" sentinel signals
    // "no namespace match found; surfacing global context" to the
    // formatter so it can flag the divergence in the header.
    let fallback = db::list(
        conn,
        None,
        Some(&Tier::Long),
        limit,
        0,
        None,
        None,
        None,
        None,
        None,
    )?;
    Ok((fallback, String::new()))
}

/// Cumulative character → approximate-tokens budget clamp. Returns the
/// prefix of `mems` that fits in the budget. Always keeps the first
/// memory (R1 always-return-at-least-one parity with `memory_recall`).
fn clamp_to_budget(mems: Vec<models::Memory>, budget_tokens: usize) -> Vec<models::Memory> {
    if budget_tokens == 0 || mems.is_empty() {
        return mems;
    }
    let mut chars_so_far: usize = 0;
    let mut out = Vec::with_capacity(mems.len());
    for (idx, mem) in mems.into_iter().enumerate() {
        // Conservative per-row width: title + namespace + tier label +
        // age + ~20 chars of decorations. Real toon/text rows are ~150
        // chars; we round up to bound risk.
        let row_chars = mem.title.len() + mem.namespace.len() + 80;
        let projected_tokens =
            ((chars_so_far + row_chars) as f32 * TOKENS_PER_CHAR).ceil() as usize;
        if idx > 0 && projected_tokens > budget_tokens {
            break;
        }
        chars_so_far += row_chars;
        out.push(mem);
    }
    out
}

/// Boot status — encodes the diagnostic the agent (and the human running
/// it) sees on every invocation. End users asked for an always-visible
/// signal so a missing memory context is a known state, not a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootStatus {
    /// Found memories in the requested namespace. Normal happy path.
    OkLoaded,
    /// Requested namespace had no memories; falling back to global Long tier.
    InfoFallback,
    /// Both the requested namespace and the global fallback were empty.
    /// First-run condition for greenfield checkouts. Not an error.
    InfoEmpty,
    /// Requested DB path does not exist or could not be opened. With
    /// `--quiet` we still exit 0, but the header surfaces the warning so
    /// the agent can say "I would have loaded context but couldn't" rather
    /// than silently appearing memory-less.
    WarnDbUnavailable,
}

/// `ai-memory boot` entry point.
#[allow(clippy::too_many_lines)]
pub fn run(db_path: &Path, args: &BootArgs, out: &mut CliOutput<'_>) -> Result<()> {
    let format = BootFormat::parse(&args.format)?;
    let limit = args.limit.clamp(1, 50);
    let namespace = resolve_namespace(args);

    // Open the DB. On failure, honor `--quiet` (exit 0 with empty stdout
    // when `--no-header` is also set; otherwise emit a warning header so
    // the agent always sees that boot ran, even on failure).
    let conn = match db::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            if !args.quiet {
                writeln!(
                    out.stderr,
                    "ai-memory boot: db unavailable at {}: {e}",
                    db_path.display()
                )?;
            }
            if !args.no_header {
                emit_status_header(
                    out,
                    BootStatus::WarnDbUnavailable,
                    &namespace,
                    0,
                    db_path,
                    format,
                )?;
            }
            return Ok(());
        }
    };

    let (mems, used_namespace) = fetch_boot_memories(&conn, &namespace, limit)?;
    let mems = clamp_to_budget(mems, args.budget_tokens);
    let fell_back = !mems.is_empty() && used_namespace.is_empty();

    if mems.is_empty() {
        if !args.no_header {
            emit_status_header(out, BootStatus::InfoEmpty, &namespace, 0, db_path, format)?;
        }
        return Ok(());
    }

    let displayed_ns = if fell_back { "global" } else { &namespace };
    let status = if fell_back {
        BootStatus::InfoFallback
    } else {
        BootStatus::OkLoaded
    };

    match format {
        BootFormat::Json => {
            // JSON output is one object: header fields + memories together.
            // `--no-header` is meaningless for JSON (the JSON IS the
            // boundary) but we honor it as "skip the diagnostic JSON" by
            // emitting only the memories array, for advanced wrappers.
            if args.no_header {
                writeln!(
                    out.stdout,
                    "{}",
                    serde_json::to_string(&serde_json::json!({"memories": mems}))?
                )?;
            } else {
                emit_json_with_status(out, status, displayed_ns, &mems, fell_back)?;
            }
        }
        BootFormat::Text => {
            if !args.no_header {
                emit_status_header(out, status, displayed_ns, mems.len(), db_path, format)?;
            }
            emit_text(out, &mems)?;
        }
        BootFormat::Toon => {
            if !args.no_header {
                emit_status_header(out, status, displayed_ns, mems.len(), db_path, format)?;
            }
            emit_toon(out, &mems)?;
        }
    }

    Ok(())
}

/// Always-visible diagnostic header. Agents see this in their session log
/// even when the body is empty, so the absence of memory context is a
/// surfaced signal rather than a silent failure. Format:
///   text/toon: `# ai-memory boot: <STATUS> — <human readable reason>`
///   json:      single JSON object with `status`, `namespace`, `count`,
///              optional `memories` and `note`.
fn emit_status_header(
    out: &mut CliOutput<'_>,
    status: BootStatus,
    namespace: &str,
    count: usize,
    db_path: &Path,
    format: BootFormat,
) -> Result<()> {
    let (label, note) = match status {
        BootStatus::OkLoaded => (
            "ok",
            format!(
                "loaded {count} memor{plural} from ns={namespace}",
                plural = if count == 1 { "y" } else { "ies" }
            ),
        ),
        BootStatus::InfoFallback => (
            "info",
            format!(
                "namespace empty; loaded {count} memor{plural} from global Long tier fallback",
                plural = if count == 1 { "y" } else { "ies" }
            ),
        ),
        BootStatus::InfoEmpty => (
            "info",
            format!(
                "namespace '{namespace}' is empty and no global Long-tier fallback found — \
                 nothing to load (this is normal on a fresh install)"
            ),
        ),
        BootStatus::WarnDbUnavailable => (
            "warn",
            format!(
                "db unavailable at {} — proceeding without memory context. \
                 Run `ai-memory doctor` to diagnose. \
                 See https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/integrations/README.md",
                db_path.display()
            ),
        ),
    };

    match format {
        BootFormat::Json => {
            writeln!(
                out.stdout,
                "{}",
                serde_json::json!({
                    "status": label,
                    "namespace": namespace,
                    "count": count,
                    "note": note,
                })
            )?;
        }
        _ => {
            writeln!(out.stdout, "# ai-memory boot: {label} — {note}")?;
        }
    }
    Ok(())
}

fn emit_text(out: &mut CliOutput<'_>, mems: &[models::Memory]) -> Result<()> {
    for mem in mems {
        let age = human_age(&mem.updated_at);
        writeln!(
            out.stdout,
            "- [{}/{}] {} (ns={}, p={}, {})",
            mem.tier,
            id_short(&mem.id),
            mem.title,
            mem.namespace,
            mem.priority,
            age
        )?;
    }
    Ok(())
}

fn emit_json_with_status(
    out: &mut CliOutput<'_>,
    status: BootStatus,
    namespace: &str,
    mems: &[models::Memory],
    fell_back: bool,
) -> Result<()> {
    let label = match status {
        BootStatus::OkLoaded => "ok",
        BootStatus::InfoFallback | BootStatus::InfoEmpty => "info",
        BootStatus::WarnDbUnavailable => "warn",
    };
    let body = serde_json::json!({
        "status": label,
        "namespace": namespace,
        "fell_back_to_global": fell_back,
        "count": mems.len(),
        "memories": mems,
    });
    writeln!(out.stdout, "{}", serde_json::to_string(&body)?)?;
    Ok(())
}

fn emit_toon(out: &mut CliOutput<'_>, mems: &[models::Memory]) -> Result<()> {
    // Reuse the canonical TOON serializer used by `memory_recall` so boot
    // output is byte-identical to a recall response on the wire format.
    // `memories_to_toon` takes the `{memories: [...], count: N}` shape.
    let body = serde_json::json!({
        "memories": mems,
        "count": mems.len(),
    });
    let toon_str = toon::memories_to_toon(&body, true);
    writeln!(out.stdout, "{toon_str}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    fn default_args() -> BootArgs {
        BootArgs {
            namespace: None,
            limit: 10,
            budget_tokens: DEFAULT_BUDGET_TOKENS,
            format: "text".to_string(),
            no_header: false,
            quiet: false,
            cwd: None,
        }
    }

    #[test]
    fn boot_format_parse_accepts_aliases() {
        assert_eq!(BootFormat::parse("text").unwrap(), BootFormat::Text);
        assert_eq!(BootFormat::parse("json").unwrap(), BootFormat::Json);
        assert_eq!(BootFormat::parse("toon").unwrap(), BootFormat::Toon);
        assert_eq!(BootFormat::parse("toon-compact").unwrap(), BootFormat::Toon);
        assert_eq!(BootFormat::parse("toon_compact").unwrap(), BootFormat::Toon);
        assert!(BootFormat::parse("yaml").is_err());
    }

    #[test]
    fn boot_emits_ok_header_with_loaded_memories() {
        let mut env = TestEnv::fresh();
        seed_memory(&env.db_path, "ns-x", "first", "content one");
        seed_memory(&env.db_path, "ns-x", "second", "content two");
        seed_memory(&env.db_path, "ns-y", "elsewhere", "content three");
        let db_path = env.db_path.clone();
        let mut args = default_args();
        args.namespace = Some("ns-x".to_string());
        let mut out = env.output();
        run(&db_path, &args, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(
            stdout.contains("# ai-memory boot: ok") && stdout.contains("ns=ns-x"),
            "expected ok status header, got: {stdout}"
        );
        assert!(stdout.contains("first"));
        assert!(stdout.contains("second"));
        assert!(!stdout.contains("elsewhere"));
    }

    #[test]
    fn boot_respects_limit() {
        let mut env = TestEnv::fresh();
        for i in 0..5 {
            seed_memory(&env.db_path, "ns-l", &format!("m{i}"), "x");
        }
        let db_path = env.db_path.clone();
        let mut args = default_args();
        args.namespace = Some("ns-l".to_string());
        args.limit = 2;
        let mut out = env.output();
        run(&db_path, &args, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(stdout.contains("loaded 2 memories"));
        let row_count = stdout.lines().filter(|l| l.starts_with("- [")).count();
        assert_eq!(row_count, 2, "expected 2 rows, got {row_count}: {stdout}");
    }

    #[test]
    fn boot_no_header_with_flag_suppresses_status() {
        let mut env = TestEnv::fresh();
        seed_memory(&env.db_path, "ns-h", "row-one", "x");
        let db_path = env.db_path.clone();
        let mut args = default_args();
        args.namespace = Some("ns-h".to_string());
        args.no_header = true;
        let mut out = env.output();
        run(&db_path, &args, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(!stdout.contains("# ai-memory boot"));
        assert!(stdout.contains("row-one"));
    }

    #[test]
    fn boot_json_format_emits_status_and_memories() {
        let mut env = TestEnv::fresh();
        seed_memory(&env.db_path, "ns-j", "row", "x");
        let db_path = env.db_path.clone();
        let mut args = default_args();
        args.namespace = Some("ns-j".to_string());
        args.format = "json".to_string();
        let mut out = env.output();
        run(&db_path, &args, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["namespace"], "ns-j");
        assert_eq!(parsed["count"], 1);
        assert_eq!(parsed["fell_back_to_global"], false);
        assert!(parsed["memories"].is_array());
    }

    #[test]
    fn boot_quiet_with_unreachable_db_emits_warn_header_no_stderr() {
        // The user-facing diagnostic header MUST appear so the agent (and
        // a human looking at the agent log) sees that boot ran but
        // couldn't load context. --quiet suppresses *only* stderr.
        let mut env = TestEnv::fresh();
        let bad_path = env
            .db_path
            .parent()
            .unwrap()
            .join("subdir/that/does/not/exist/db.sqlite");
        let mut args = default_args();
        args.quiet = true;
        let mut out = env.output();
        run(&bad_path, &args, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(
            stdout.contains("# ai-memory boot: warn"),
            "warn header should always appear under --quiet: {stdout}"
        );
        assert!(
            stdout.contains("db unavailable"),
            "header should explain the warning cause: {stdout}"
        );
        assert!(
            env.stderr.is_empty(),
            "stderr should be silent under --quiet"
        );
    }

    #[test]
    fn boot_db_unavailable_without_quiet_writes_to_stderr() {
        let mut env = TestEnv::fresh();
        let bad_path = env
            .db_path
            .parent()
            .unwrap()
            .join("subdir/that/does/not/exist/db.sqlite");
        let mut args = default_args();
        // quiet = false (default) — error goes to stderr too.
        let mut out = env.output();
        run(&bad_path, &args, &mut out).unwrap();
        let stderr = std::str::from_utf8(&env.stderr).unwrap();
        assert!(
            stderr.contains("ai-memory boot: db unavailable"),
            "stderr should carry the diagnostic without --quiet: {stderr}"
        );
    }

    #[test]
    fn boot_quiet_with_no_header_silent_for_legacy_wrappers() {
        // Wrappers that frame their own context can opt out of both the
        // diagnostic header AND any error output by combining flags.
        let mut env = TestEnv::fresh();
        let bad_path = env
            .db_path
            .parent()
            .unwrap()
            .join("subdir/that/does/not/exist/db.sqlite");
        let mut args = default_args();
        args.quiet = true;
        args.no_header = true;
        let mut out = env.output();
        run(&bad_path, &args, &mut out).unwrap();
        assert!(env.stdout.is_empty());
        assert!(env.stderr.is_empty());
    }

    #[test]
    fn boot_falls_back_to_long_tier_when_namespace_empty() {
        let mut env = TestEnv::fresh();
        let id = seed_memory(&env.db_path, "other", "long-tier-row", "x");
        let conn = db::open(&env.db_path).unwrap();
        conn.execute(
            "UPDATE memories SET tier='long' WHERE id=?1",
            rusqlite::params![id],
        )
        .unwrap();
        drop(conn);
        let db_path = env.db_path.clone();
        let mut args = default_args();
        args.namespace = Some("nonexistent-ns".to_string());
        let mut out = env.output();
        run(&db_path, &args, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(
            stdout.contains("# ai-memory boot: info") && stdout.contains("fallback"),
            "expected info/fallback status: {stdout}"
        );
        assert!(stdout.contains("long-tier-row"));
    }

    #[test]
    fn boot_empty_namespace_emits_info_empty_status() {
        let mut env = TestEnv::fresh();
        let db_path = env.db_path.clone();
        let mut args = default_args();
        args.namespace = Some("nothing-here".to_string());
        let mut out = env.output();
        run(&db_path, &args, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(
            stdout.contains("# ai-memory boot: info")
                && stdout.contains("nothing-here")
                && stdout.contains("empty"),
            "info/empty header expected: {stdout}"
        );
    }

    #[test]
    fn boot_budget_tokens_clamps_output() {
        let mut env = TestEnv::fresh();
        for i in 0..20 {
            seed_memory(
                &env.db_path,
                "ns-budget",
                &format!("memory number {i} with a moderate-length title"),
                "x",
            );
        }
        let db_path = env.db_path.clone();
        let mut args = default_args();
        args.namespace = Some("ns-budget".to_string());
        args.limit = 50;
        args.budget_tokens = 100;
        let mut out = env.output();
        run(&db_path, &args, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        let row_count = stdout.lines().filter(|l| l.starts_with("- [")).count();
        assert!(
            row_count >= 1 && row_count < 20,
            "budget_tokens=100 should clamp to fewer than 20 rows; got {row_count}\noutput:\n{stdout}"
        );
    }

    #[test]
    fn boot_json_warn_status_when_db_unavailable() {
        let mut env = TestEnv::fresh();
        let bad_path = env
            .db_path
            .parent()
            .unwrap()
            .join("subdir/that/does/not/exist/db.sqlite");
        let mut args = default_args();
        args.format = "json".to_string();
        args.quiet = true;
        let mut out = env.output();
        run(&bad_path, &args, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
        assert_eq!(parsed["status"], "warn");
        assert_eq!(parsed["count"], 0);
        assert!(parsed["note"].as_str().unwrap().contains("db unavailable"));
    }
}
