// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Cluster E API-2 (issue #767) — `ai-memory skill <subcommand>`
//! CLI surface.
//!
//! Closes the CLI/HTTP parity gap surfaced by the v0.7.0 6-reviewer audit:
//! the L1-5 Agent Skills substrate landed with seven MCP tools
//! (`memory_skill_*`) but zero CLI subcommands and zero HTTP routes, so
//! HTTP-daemon operators and shell-driven workflows could not interact
//! with skills at all. This module adds the CLI surface; the matching
//! HTTP routes live in `src/handlers/http.rs`.
//!
//! Each subcommand delegates to the **same** substrate handler the MCP
//! dispatch already uses (re-exported as `crate::mcp::handle_skill_*`).
//! No business logic is re-implemented here — the CLI is a clap-shaped
//! thin client over the existing handlers, so MCP / CLI / HTTP share a
//! single source of truth for skill semantics.
//!
//! Verb mapping (CLI → MCP tool name):
//!
//!   * `ai-memory skill register`    → `memory_skill_register`
//!   * `ai-memory skill list`        → `memory_skill_list`
//!   * `ai-memory skill get`         → `memory_skill_get`
//!   * `ai-memory skill resource`    → `memory_skill_resource`
//!   * `ai-memory skill export`      → `memory_skill_export`
//!   * `ai-memory skill promote`     → `memory_skill_promote_from_reflection`
//!   * `ai-memory skill compose`     → `memory_skill_compositional_context`
//!
//! All seven mirror the MCP tool surface 1:1. No new MCP tools land —
//! the tool count stays at 71/70/Power 22.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::cli::CliOutput;
use crate::db;

/// Top-level `ai-memory skill <subcommand>` argument struct.
#[derive(Args, Debug, Clone)]
pub struct SkillArgs {
    #[command(subcommand)]
    pub action: SkillAction,
}

/// `ai-memory skill ...` sub-subcommands. One per MCP `memory_skill_*`
/// tool so the CLI surface is parity-checkable by name.
#[derive(Subcommand, Debug, Clone)]
pub enum SkillAction {
    /// Register a SKILL.md skill from a folder or inline manifest text.
    Register(RegisterArgs),
    /// List current (non-superseded) skills — discovery payload only.
    List(ListArgs),
    /// Fetch the full activation payload for a skill (body included).
    Get(GetArgs),
    /// Fetch the decompressed content of a single skill resource and
    /// verify its SHA-256 digest.
    Resource(ResourceArgs),
    /// Export a skill back to a folder as a round-trip-stable SKILL.md.
    Export(ExportArgs),
    /// Promote a Reflection-kind memory into a reusable Agent Skill.
    Promote(PromoteArgs),
    /// Load a skill body together with the reflections declared in its
    /// `composes_with_reflections` frontmatter list.
    Compose(ComposeArgs),
}

// ---------------------------------------------------------------------------
// Per-verb argument structs
// ---------------------------------------------------------------------------

/// `ai-memory skill register` — accepts EITHER `--manifest <folder-or-file>`
/// OR `--inline <text>`.
#[derive(Args, Debug, Clone)]
pub struct RegisterArgs {
    /// Path to a directory containing `SKILL.md` (and an optional
    /// `resources/` sub-directory), OR a path to a SKILL.md file
    /// directly (the parent directory is then treated as the folder).
    /// Mirrors the MCP `folder_path` parameter.
    #[arg(long, value_name = "PATH")]
    pub manifest: Option<PathBuf>,

    /// Raw SKILL.md text including YAML frontmatter and markdown body.
    /// Mirrors the MCP `inline_skill` parameter. Mutually exclusive
    /// with `--manifest` at the substrate level — passing both surfaces
    /// the substrate's "either / or" error.
    #[arg(long, value_name = "TEXT", conflicts_with = "manifest")]
    pub inline: Option<String>,

    /// Emit a structured JSON envelope instead of a human summary line.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `ai-memory skill list`
#[derive(Args, Debug, Clone)]
pub struct ListArgs {
    /// Filter to this namespace. Omit (or pass `%`) for all namespaces.
    #[arg(long, value_name = "NS")]
    pub namespace: Option<String>,

    /// Optional substring filter applied to name and description.
    #[arg(long, value_name = "TEXT")]
    pub filter: Option<String>,

    /// Emit a structured JSON envelope instead of a human table.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `ai-memory skill get`
#[derive(Args, Debug, Clone)]
pub struct GetArgs {
    /// The UUID of the skill to retrieve. Mirrors MCP `skill_id`.
    #[arg(long, value_name = "ID")]
    pub id: String,

    /// Emit a structured JSON envelope; default is a brief summary.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `ai-memory skill resource`
#[derive(Args, Debug, Clone)]
pub struct ResourceArgs {
    /// The UUID of the parent skill.
    #[arg(long, value_name = "ID")]
    pub id: String,

    /// Relative path of the resource (e.g. `scripts/run.sh`).
    #[arg(long, value_name = "PATH")]
    pub path: String,

    /// Emit a structured JSON envelope.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `ai-memory skill export`
#[derive(Args, Debug, Clone)]
pub struct ExportArgs {
    /// The UUID of the skill to export.
    #[arg(long, value_name = "ID")]
    pub id: String,

    /// Destination directory. Created if absent. Re-registering the
    /// exported folder via `ai-memory skill register --manifest <dir>`
    /// produces the IDENTICAL SHA-256 digest (round-trip guarantee).
    #[arg(long, value_name = "PATH")]
    pub output: PathBuf,

    /// Emit a structured JSON envelope.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `ai-memory skill promote`
#[derive(Args, Debug, Clone)]
pub struct PromoteArgs {
    /// The UUID of a Reflection-kind memory (created via `memory_reflect`).
    /// Mirrors MCP `reflection_id`.
    #[arg(long, value_name = "ID")]
    pub id: String,

    /// agentskills.io §3.1-compliant skill name (`^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$`,
    /// 1–64 chars).
    #[arg(long, value_name = "NAME")]
    pub name: String,

    /// 1–1024 char description for the promoted skill.
    #[arg(long, value_name = "TEXT")]
    pub description: String,

    /// Optional path to a JSON file containing the skill's parameters
    /// JSON-schema. Spliced into the SKILL.md body verbatim.
    #[arg(long, value_name = "PATH")]
    pub parameters_schema: Option<PathBuf>,

    /// Emit a structured JSON envelope.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// `ai-memory skill compose`
#[derive(Args, Debug, Clone)]
pub struct ComposeArgs {
    /// The UUID of the skill to load with composed reflections.
    #[arg(long, value_name = "ID")]
    pub id: String,

    /// Optional token cap on the cumulative reflection content (skill
    /// body is NOT counted). Default 4000 inside the substrate; hard-
    /// clamped to 32000.
    #[arg(long, value_name = "N")]
    pub budget_tokens: Option<u64>,

    /// Emit a structured JSON envelope (default).
    #[arg(long, default_value_t = true)]
    pub json: bool,
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Dispatch entry-point called from `daemon_runtime::run`.
///
/// Loads the active keypair (when configured) so register / export /
/// promote can sign their output, matching the MCP-side wiring.
///
/// # Errors
///
/// Surfaces DB-open errors verbatim. Substrate-handler failures bubble
/// up as exit codes (non-zero) with the substrate's error string
/// printed to stderr.
pub fn run(
    db_path: &Path,
    args: &SkillArgs,
    active_keypair: Option<&crate::identity::keypair::AgentKeypair>,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    let conn = db::open(db_path)?;
    match &args.action {
        SkillAction::Register(a) => run_register(&conn, a, active_keypair, out),
        SkillAction::List(a) => run_list(&conn, a, out),
        SkillAction::Get(a) => run_get(&conn, a, out),
        SkillAction::Resource(a) => run_resource(&conn, a, out),
        SkillAction::Export(a) => run_export(&conn, a, active_keypair, out),
        SkillAction::Promote(a) => run_promote(&conn, a, active_keypair, out),
        SkillAction::Compose(a) => run_compose(&conn, a, out),
    }
}

fn handler_err_exit(out: &mut CliOutput<'_>, verb: &str, e: &str) -> Result<i32> {
    writeln!(out.stderr, "ai-memory skill {verb}: {e}")?;
    Ok(2)
}

fn emit_json(out: &mut CliOutput<'_>, v: &Value) -> Result<()> {
    let s = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    writeln!(out.stdout, "{s}")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// register
// ---------------------------------------------------------------------------

fn run_register(
    conn: &rusqlite::Connection,
    args: &RegisterArgs,
    active_keypair: Option<&crate::identity::keypair::AgentKeypair>,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    // Normalise --manifest: accept either a directory OR a SKILL.md
    // file path (in which case we hand the substrate the parent dir).
    let folder_path: Option<String> = args.manifest.as_ref().map(|p| {
        if p.is_file() {
            p.parent().map_or_else(
                || p.to_string_lossy().into_owned(),
                |d| d.to_string_lossy().into_owned(),
            )
        } else {
            p.to_string_lossy().into_owned()
        }
    });

    let mut params = json!({});
    if let Some(ref fp) = folder_path {
        params["folder_path"] = json!(fp);
    }
    if let Some(ref inl) = args.inline {
        params["inline_skill"] = json!(inl);
    }

    match crate::mcp::handle_skill_register(conn, &params, active_keypair) {
        Ok(v) => {
            if args.json {
                emit_json(out, &v)?;
            } else {
                let id = v["id"].as_str().unwrap_or("");
                let ns = v["namespace"].as_str().unwrap_or("");
                let name = v["name"].as_str().unwrap_or("");
                let digest = v["digest"].as_str().unwrap_or("");
                let signed = v["signed"].as_bool().unwrap_or(false);
                writeln!(
                    out.stdout,
                    "registered skill {ns}/{name} id={id} digest={} signed={signed}",
                    &digest[..digest.len().min(16)],
                )?;
                if let Some(prev) = v.get("superseded_id").and_then(Value::as_str) {
                    writeln!(out.stdout, "  superseded previous id={prev}")?;
                }
            }
            Ok(0)
        }
        Err(e) => handler_err_exit(out, "register", &e),
    }
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

fn run_list(conn: &rusqlite::Connection, args: &ListArgs, out: &mut CliOutput<'_>) -> Result<i32> {
    let mut params = json!({});
    if let Some(ref ns) = args.namespace {
        params["namespace"] = json!(ns);
    }
    if let Some(ref f) = args.filter {
        params["filter"] = json!(f);
    }
    match crate::mcp::handle_skill_list(conn, &params) {
        Ok(v) => {
            if args.json {
                emit_json(out, &v)?;
            } else {
                let empty: Vec<Value> = Vec::new();
                let arr = v["skills"].as_array().unwrap_or(&empty);
                writeln!(out.stdout, "{} skills", arr.len())?;
                for s in arr {
                    let ns = s["namespace"].as_str().unwrap_or("");
                    let name = s["name"].as_str().unwrap_or("");
                    let id = s["id"].as_str().unwrap_or("");
                    let desc = s["description"].as_str().unwrap_or("");
                    writeln!(out.stdout, "  {ns}/{name} ({id})\n    {desc}")?;
                }
            }
            Ok(0)
        }
        Err(e) => handler_err_exit(out, "list", &e),
    }
}

// ---------------------------------------------------------------------------
// get
// ---------------------------------------------------------------------------

fn run_get(conn: &rusqlite::Connection, args: &GetArgs, out: &mut CliOutput<'_>) -> Result<i32> {
    let params = json!({ "skill_id": args.id });
    match crate::mcp::handle_skill_get(conn, &params) {
        Ok(v) => {
            if args.json {
                emit_json(out, &v)?;
            } else {
                let ns = v["namespace"].as_str().unwrap_or("");
                let name = v["name"].as_str().unwrap_or("");
                let body = v["body"].as_str().unwrap_or("");
                writeln!(out.stdout, "# {ns}/{name}\n\n{body}")?;
            }
            Ok(0)
        }
        Err(e) => handler_err_exit(out, "get", &e),
    }
}

// ---------------------------------------------------------------------------
// resource
// ---------------------------------------------------------------------------

fn run_resource(
    conn: &rusqlite::Connection,
    args: &ResourceArgs,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    let params = json!({
        "skill_id": args.id,
        "resource_path": args.path,
    });
    match crate::mcp::handle_skill_resource(conn, &params) {
        Ok(v) => {
            if args.json {
                emit_json(out, &v)?;
            } else if let Some(content) = v["content"].as_str() {
                writeln!(out.stdout, "{content}")?;
            } else {
                emit_json(out, &v)?;
            }
            Ok(0)
        }
        Err(e) => handler_err_exit(out, "resource", &e),
    }
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

fn run_export(
    conn: &rusqlite::Connection,
    args: &ExportArgs,
    active_keypair: Option<&crate::identity::keypair::AgentKeypair>,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    let params = json!({
        "skill_id": args.id,
        "target_folder": args.output.to_string_lossy(),
    });
    match crate::mcp::handle_skill_export(conn, &params, active_keypair) {
        Ok(v) => {
            if args.json {
                emit_json(out, &v)?;
            } else {
                let fallback_folder = args.output.to_string_lossy();
                let folder = v["target_folder"].as_str().unwrap_or(&fallback_folder);
                writeln!(out.stdout, "exported skill {} → {folder}", args.id)?;
            }
            Ok(0)
        }
        Err(e) => handler_err_exit(out, "export", &e),
    }
}

// ---------------------------------------------------------------------------
// promote
// ---------------------------------------------------------------------------

fn run_promote(
    conn: &rusqlite::Connection,
    args: &PromoteArgs,
    active_keypair: Option<&crate::identity::keypair::AgentKeypair>,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    let mut params = json!({
        "reflection_id": args.id,
        "skill_name": args.name,
        "skill_description": args.description,
    });
    if let Some(ref p) = args.parameters_schema {
        let raw = std::fs::read_to_string(p)
            .map_err(|e| anyhow::anyhow!("read parameters_schema {}: {e}", p.display()))?;
        let v: Value = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parse parameters_schema {}: {e}", p.display()))?;
        params["parameters_schema"] = v;
    }
    match crate::mcp::handle_skill_promote_from_reflection(conn, &params, active_keypair) {
        Ok(v) => {
            if args.json {
                emit_json(out, &v)?;
            } else {
                let id = v["skill_id"]
                    .as_str()
                    .or_else(|| v["id"].as_str())
                    .unwrap_or("");
                writeln!(out.stdout, "promoted reflection {} → skill {id}", args.id)?;
            }
            Ok(0)
        }
        Err(e) => handler_err_exit(out, "promote", &e),
    }
}

// ---------------------------------------------------------------------------
// compose
// ---------------------------------------------------------------------------

fn run_compose(
    conn: &rusqlite::Connection,
    args: &ComposeArgs,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    let mut params = json!({ "skill_id": args.id });
    if let Some(b) = args.budget_tokens {
        params["budget_tokens"] = json!(b);
    }
    match crate::mcp::handle_skill_compositional_context(conn, &params) {
        Ok(v) => {
            emit_json(out, &v)?;
            Ok(0)
        }
        Err(e) => handler_err_exit(out, "compose", &e),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::drop_non_drop)] // explicit borrow-release of CliOutput; see file-level note above.
mod tests {
    use super::*;
    use crate::cli::CliOutput;
    use tempfile::TempDir;

    fn fresh_db() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ai-memory.db");
        let _conn = db::open(&path).unwrap();
        (dir, path)
    }

    fn minimal_skill_md(name: &str) -> String {
        format!("---\nnamespace: testns\nname: {name}\ndescription: A demo skill.\n---\n\nBody.\n")
    }

    #[test]
    fn cli_skill_register_inline_smoke() {
        let (_dir, db_path) = fresh_db();
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = SkillArgs {
            action: SkillAction::Register(RegisterArgs {
                manifest: None,
                inline: Some(minimal_skill_md("cli-register")),
                json: true,
            }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 0);
        drop(out);
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.contains("\"registered\""));
        assert!(text.contains("cli-register"));
    }

    #[test]
    fn cli_skill_list_smoke() {
        let (_dir, db_path) = fresh_db();
        // Seed a skill via the register handler so list has something to find.
        let conn = db::open(&db_path).unwrap();
        let _ = crate::mcp::handle_skill_register(
            &conn,
            &json!({"inline_skill": minimal_skill_md("cli-list")}),
            None,
        )
        .unwrap();
        drop(conn);

        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = SkillArgs {
            action: SkillAction::List(ListArgs {
                namespace: Some("testns".to_string()),
                filter: None,
                json: true,
            }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 0);
        drop(out);
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.contains("cli-list"));
    }

    #[test]
    fn cli_skill_get_smoke() {
        let (_dir, db_path) = fresh_db();
        let conn = db::open(&db_path).unwrap();
        let reg = crate::mcp::handle_skill_register(
            &conn,
            &json!({"inline_skill": minimal_skill_md("cli-get")}),
            None,
        )
        .unwrap();
        let id = reg["id"].as_str().unwrap().to_string();
        drop(conn);

        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = SkillArgs {
            action: SkillAction::Get(GetArgs {
                id: id.clone(),
                json: true,
            }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 0);
        drop(out);
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.contains(&id));
        assert!(text.contains("cli-get"));
    }

    #[test]
    fn cli_skill_export_smoke() {
        let (dir, db_path) = fresh_db();
        let conn = db::open(&db_path).unwrap();
        let reg = crate::mcp::handle_skill_register(
            &conn,
            &json!({"inline_skill": minimal_skill_md("cli-export")}),
            None,
        )
        .unwrap();
        let id = reg["id"].as_str().unwrap().to_string();
        drop(conn);

        let target = dir.path().join("export-out");

        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = SkillArgs {
            action: SkillAction::Export(ExportArgs {
                id: id.clone(),
                output: target.clone(),
                json: true,
            }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 0);
        // SKILL.md must exist in the output folder.
        assert!(target.join("SKILL.md").exists());
    }

    #[test]
    fn cli_skill_get_missing_id_exits_nonzero() {
        let (_dir, db_path) = fresh_db();
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = SkillArgs {
            action: SkillAction::Get(GetArgs {
                id: "no-such-skill".to_string(),
                json: true,
            }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 2);
        drop(out);
        let err = String::from_utf8(stderr).unwrap();
        assert!(err.contains("skill not found"));
    }

    #[test]
    fn cli_skill_compose_smoke() {
        let (_dir, db_path) = fresh_db();
        let conn = db::open(&db_path).unwrap();
        let reg = crate::mcp::handle_skill_register(
            &conn,
            &json!({"inline_skill": minimal_skill_md("cli-compose")}),
            None,
        )
        .unwrap();
        let id = reg["id"].as_str().unwrap().to_string();
        drop(conn);

        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = SkillArgs {
            action: SkillAction::Compose(ComposeArgs {
                id: id.clone(),
                budget_tokens: Some(1000),
                json: true,
            }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 0);
        drop(out);
        let text = String::from_utf8(stdout).unwrap();
        // A skill with no `composes_with_reflections` declaration still
        // returns body and an empty reflections list — pin that.
        assert!(text.contains(&id) || text.contains("\"body\""));
    }

    // ------------------------------------------------------------------
    // Coverage-uplift block (2026-05-19): exercise the non-JSON (human-
    // render) paths for every skill verb so the `if args.json { ... }
    // else { ... writeln!(...) }` else-arms are not dead from a test-
    // coverage standpoint.
    // ------------------------------------------------------------------

    #[test]
    fn cli_skill_register_human_render_emits_summary_line() {
        let (_dir, db_path) = fresh_db();
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = SkillArgs {
            action: SkillAction::Register(RegisterArgs {
                manifest: None,
                inline: Some(minimal_skill_md("cli-register-human")),
                json: false,
            }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 0);
        drop(out);
        let text = String::from_utf8(stdout).unwrap();
        // The human-render path emits "registered skill {ns}/{name} ..."
        assert!(
            text.starts_with("registered skill "),
            "expected human-render summary line, got: {text}"
        );
        assert!(text.contains("cli-register-human"));
        assert!(text.contains("digest="));
        assert!(text.contains("signed="));
    }

    #[test]
    fn cli_skill_list_human_render_emits_table() {
        let (_dir, db_path) = fresh_db();
        let conn = db::open(&db_path).unwrap();
        let _ = crate::mcp::handle_skill_register(
            &conn,
            &json!({"inline_skill": minimal_skill_md("cli-list-human")}),
            None,
        )
        .unwrap();
        drop(conn);

        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = SkillArgs {
            action: SkillAction::List(ListArgs {
                namespace: Some("testns".to_string()),
                filter: Some("cli-list-human".to_string()),
                json: false,
            }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 0);
        drop(out);
        let text = String::from_utf8(stdout).unwrap();
        // Human render prints "{N} skills" + per-skill indented lines.
        assert!(
            text.contains(" skills"),
            "expected count header, got: {text}"
        );
        assert!(text.contains("cli-list-human"));
    }

    #[test]
    fn cli_skill_get_human_render_emits_markdown_header_and_body() {
        let (_dir, db_path) = fresh_db();
        let conn = db::open(&db_path).unwrap();
        let reg = crate::mcp::handle_skill_register(
            &conn,
            &json!({"inline_skill": minimal_skill_md("cli-get-human")}),
            None,
        )
        .unwrap();
        let id = reg["id"].as_str().unwrap().to_string();
        drop(conn);

        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = SkillArgs {
            action: SkillAction::Get(GetArgs { id, json: false }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 0);
        drop(out);
        let text = String::from_utf8(stdout).unwrap();
        // Human render prints "# {ns}/{name}\n\n{body}"
        assert!(text.starts_with("# testns/cli-get-human"));
        assert!(text.contains("Body."));
    }

    #[test]
    fn cli_skill_export_human_render_emits_path_line() {
        let (dir, db_path) = fresh_db();
        let conn = db::open(&db_path).unwrap();
        let reg = crate::mcp::handle_skill_register(
            &conn,
            &json!({"inline_skill": minimal_skill_md("cli-export-human")}),
            None,
        )
        .unwrap();
        let id = reg["id"].as_str().unwrap().to_string();
        drop(conn);

        let target = dir.path().join("export-human-out");
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = SkillArgs {
            action: SkillAction::Export(ExportArgs {
                id: id.clone(),
                output: target.clone(),
                json: false,
            }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 0);
        assert!(target.join("SKILL.md").exists());
        drop(out);
        let text = String::from_utf8(stdout).unwrap();
        // Human render prints "exported skill {id} → {folder}"
        assert!(text.starts_with("exported skill "));
        assert!(text.contains(&id));
    }

    #[test]
    fn cli_skill_register_handler_error_writes_to_stderr_and_returns_2() {
        let (_dir, db_path) = fresh_db();
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        // Pass neither --manifest nor --inline — substrate returns the
        // "either/or" error string. Exits 2 with stderr text.
        let args = SkillArgs {
            action: SkillAction::Register(RegisterArgs {
                manifest: None,
                inline: None,
                json: true,
            }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 2);
        drop(out);
        let err = String::from_utf8(stderr).unwrap();
        assert!(
            err.starts_with("ai-memory skill register:"),
            "expected stderr prefix, got: {err}"
        );
    }

    #[test]
    fn cli_skill_resource_returns_2_on_missing_skill() {
        let (_dir, db_path) = fresh_db();
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = SkillArgs {
            action: SkillAction::Resource(ResourceArgs {
                id: "no-such-skill-id".to_string(),
                path: "doesnt-matter.txt".to_string(),
                json: false,
            }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 2);
        drop(out);
        let err = String::from_utf8(stderr).unwrap();
        assert!(err.starts_with("ai-memory skill resource:"));
    }

    #[test]
    fn cli_skill_promote_returns_2_on_missing_reflection() {
        let (_dir, db_path) = fresh_db();
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = SkillArgs {
            action: SkillAction::Promote(PromoteArgs {
                id: "no-such-reflection".to_string(),
                name: "demo-skill".to_string(),
                description: "Promoted from missing reflection.".to_string(),
                parameters_schema: None,
                json: true,
            }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 2);
        drop(out);
        let err = String::from_utf8(stderr).unwrap();
        assert!(err.starts_with("ai-memory skill promote:"));
    }

    #[test]
    fn cli_skill_compose_returns_2_on_missing_skill() {
        let (_dir, db_path) = fresh_db();
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = SkillArgs {
            action: SkillAction::Compose(ComposeArgs {
                id: "no-such-skill".to_string(),
                budget_tokens: None,
                json: true,
            }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 2);
        drop(out);
        let err = String::from_utf8(stderr).unwrap();
        assert!(err.starts_with("ai-memory skill compose:"));
    }

    #[test]
    fn cli_skill_register_manifest_file_path_normalised_to_parent_dir() {
        // The run_register branch at lines 260-269: if --manifest points
        // to a FILE (not a directory), the parent dir is handed to the
        // substrate. Build a real folder with SKILL.md and pass the file
        // path to exercise the is_file() → parent-dir branch.
        let (dir, db_path) = fresh_db();
        let folder = dir.path().join("skill-folder");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("SKILL.md"),
            minimal_skill_md("cli-manifest-file"),
        )
        .unwrap();

        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = SkillArgs {
            action: SkillAction::Register(RegisterArgs {
                manifest: Some(folder.join("SKILL.md")),
                inline: None,
                json: true,
            }),
        };
        let code = run(&db_path, &args, None, &mut out).unwrap();
        assert_eq!(code, 0);
        drop(out);
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.contains("cli-manifest-file"));
    }
}
