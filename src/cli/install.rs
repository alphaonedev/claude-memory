// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `ai-memory install <agent>` — wire `ai-memory boot` and the
//! `ai-memory-mcp` server into AI agents' config files (issue #487 PR-2/3).
//!
//! Each target writes a precisely-marked **managed block** so re-running
//! `install` is a no-op and `--uninstall` removes the block surgically
//! without disturbing user-added keys.
//!
//! ## Behavior summary
//!
//! - **Default `--dry-run`**: prints the diff (pretty before/after JSON,
//!   plus a unified diff over the JSON serialization) and writes nothing.
//! - **`--apply`**: writes the modified config; backs up the original to
//!   `<config>.bak.<timestamp>` first.
//! - **`--uninstall`**: removes the managed block, leaving any unrelated
//!   keys the user added intact.
//!
//! ## Idempotent marker
//!
//! Every managed block has a sentinel key:
//!
//! ```text
//! "// ai-memory:managed-block:start": "Do not edit. Managed by `ai-memory install`. https://github.com/alphaonedev/ai-memory-mcp/issues/487"
//! ```
//!
//! plus an `// ai-memory:managed-block:end` sibling. When present, the
//! installer recognises the existing block and:
//!
//! - On install: replaces it with the freshly-rendered block (so config
//!   bumps in future ai-memory releases land cleanly).
//! - On uninstall: removes both sentinel keys and any siblings the
//!   installer originally inserted.
//!
//! ## Targets
//!
//! See [`Target`] for the full list. Each target's specifics
//! (config path discovery, JSON shape) live in `apply_target_*` helpers.

use crate::cli::CliOutput;
use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// Sentinel key that marks the start of a managed block. Used by both
/// install (to recognise an existing block) and uninstall (to find the
/// block to remove).
const MARKER_START_KEY: &str = "// ai-memory:managed-block:start";
const MARKER_END_KEY: &str = "// ai-memory:managed-block:end";

/// Marker payload — the human-readable note bundled with the start key.
/// Updating this string is a no-op on installs already in the wild
/// because the recognition predicate keys off `MARKER_START_KEY` only.
const MARKER_PAYLOAD: &str = "Do not edit. Managed by `ai-memory install`. https://github.com/alphaonedev/ai-memory-mcp/issues/487";

/// Sibling-key list each target stamps inside the managed block. We track
/// this list so uninstall removes exactly the keys we wrote and leaves
/// any user-added siblings alone (defence-in-depth against a user
/// editing `// ai-memory:managed-block:end` out of the file).
const MANAGED_KEYS_PROPERTY: &str = "// ai-memory:managed-keys";

/// Args for `ai-memory install`.
#[derive(Args, Debug)]
pub struct InstallArgs {
    /// The agent target to install into.
    #[command(subcommand)]
    pub target: TargetCmd,
}

/// Per-target subcommand. Each variant carries the same shared option
/// set (`--apply`, `--uninstall`, `--config <path>`) — clap-derive
/// renders one subcommand per target so users get tab-completion on
/// the agent name and per-target `--help`.
#[derive(Subcommand, Debug)]
pub enum TargetCmd {
    /// Claude Code SessionStart hook. Writes `~/.claude/settings.json`.
    ClaudeCode(TargetArgs),
    /// OpenClaw MCP servers. Path documented at
    /// <https://docs.openclaw.ai/cli/mcp>; pass `--config <path>` if your
    /// install puts it elsewhere.
    Openclaw(TargetArgs),
    /// Cursor MCP servers. Writes `~/.cursor/mcp.json`.
    Cursor(TargetArgs),
    /// Cline MCP settings. Path varies by Cline version; pass
    /// `--config <path>` to override.
    Cline(TargetArgs),
    /// Continue MCP servers. Writes `~/.continue/config.json`.
    Continue(TargetArgs),
    /// Windsurf (Codeium) MCP servers. Writes
    /// `~/.codeium/windsurf/mcp_config.json`.
    Windsurf(TargetArgs),

    // ---- v0.6.4-010 — cross-harness install profiles ----
    /// Claude Desktop MCP servers (writes the macOS/Windows config;
    /// pass `--config <path>` on Linux). Args include
    /// `--profile core` (the v0.6.4 default).
    ClaudeDesktop(TargetArgs),
    /// OpenAI Codex CLI MCP servers. Pass `--config <path>` since the
    /// canonical Codex config path varies by Codex version. Args
    /// include `--profile core`.
    Codex(TargetArgs),
    /// xAI Grok CLI MCP servers. Pass `--config <path>` since the
    /// Grok CLI config path varies by version. Args include
    /// `--profile core`.
    GrokCli(TargetArgs),
    /// Google Gemini CLI MCP servers. Pass `--config <path>` since the
    /// Gemini CLI config path varies by version. Args include
    /// `--profile core`.
    GeminiCli(TargetArgs),
}

/// Shared per-target args. Constructed identically for every target so
/// the dispatch table can pull them out generically.
#[derive(Args, Debug, Default, Clone)]
pub struct TargetArgs {
    /// Override the default config path (the home-dir resolution).
    /// REQUIRED for tests so they never touch `~/.claude/settings.json`
    /// on the host machine.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Actually write the file. Without `--apply`, the installer
    /// runs in dry-run mode (the default) and prints what would change.
    /// Mutually exclusive with `--dry-run`. Combine with `--uninstall`
    /// to actually remove the managed block.
    #[arg(long, default_value_t = false, conflicts_with = "dry_run")]
    pub apply: bool,

    /// Force dry-run mode. This is the default, so the flag is mostly
    /// useful in scripts that want to make the no-write contract
    /// explicit. Mutually exclusive with `--apply`.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// Remove the managed block instead of installing it. Default mode
    /// is dry-run; pair with `--apply` to actually delete the block.
    #[arg(long, default_value_t = false)]
    pub uninstall: bool,

    /// Override the resolved `ai-memory` binary path written into the
    /// generated config's `command` field. By default the installer
    /// uses the binary's own `current_exe()` if `ai-memory` is not on
    /// `$PATH`, otherwise the bare string `ai-memory`.
    #[arg(long, value_name = "PATH")]
    pub binary: Option<PathBuf>,
}

/// Concrete target enum used internally. `TargetCmd` carries clap
/// metadata; `Target` is a stable tag for the dispatch table and
/// derives `ValueEnum` for completeness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Target {
    ClaudeCode,
    Openclaw,
    Cursor,
    Cline,
    Continue,
    Windsurf,
    // v0.6.4-010 — additional MCP harnesses.
    ClaudeDesktop,
    Codex,
    GrokCli,
    GeminiCli,
}

impl Target {
    /// Display name used in stdout and managed-keys metadata.
    fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Openclaw => "openclaw",
            Self::Cursor => "cursor",
            Self::Cline => "cline",
            Self::Continue => "continue",
            Self::Windsurf => "windsurf",
            Self::ClaudeDesktop => "claude-desktop",
            Self::Codex => "codex",
            Self::GrokCli => "grok-cli",
            Self::GeminiCli => "gemini-cli",
        }
    }
}

impl TargetCmd {
    fn target(&self) -> Target {
        match self {
            Self::ClaudeCode(_) => Target::ClaudeCode,
            Self::Openclaw(_) => Target::Openclaw,
            Self::Cursor(_) => Target::Cursor,
            Self::Cline(_) => Target::Cline,
            Self::Continue(_) => Target::Continue,
            Self::Windsurf(_) => Target::Windsurf,
            Self::ClaudeDesktop(_) => Target::ClaudeDesktop,
            Self::Codex(_) => Target::Codex,
            Self::GrokCli(_) => Target::GrokCli,
            Self::GeminiCli(_) => Target::GeminiCli,
        }
    }

    fn args(&self) -> &TargetArgs {
        match self {
            Self::ClaudeCode(a)
            | Self::Openclaw(a)
            | Self::Cursor(a)
            | Self::Cline(a)
            | Self::Continue(a)
            | Self::Windsurf(a)
            | Self::ClaudeDesktop(a)
            | Self::Codex(a)
            | Self::GrokCli(a)
            | Self::GeminiCli(a) => a,
        }
    }
}

/// `ai-memory install <agent>` entry point.
///
/// # Errors
///
/// Returns an error when the existing config is not valid JSON, when the
/// resolved config path can't be determined (and `--config` was not
/// passed), or when an `--apply` write fails (permission denied,
/// disk full, etc.).
pub fn run(args: &InstallArgs, out: &mut CliOutput<'_>) -> Result<()> {
    let target = args.target.target();
    let t_args = args.target.args();

    let config_path = resolve_config_path(target, t_args)?;
    let binary = resolve_binary(t_args.binary.as_deref());

    // Read existing config (if any) and parse. If absent, treat as `{}`.
    // If present and malformed, error out — never overwrite a malformed
    // config (the user might have made a typo we can help them fix).
    let (before_text, before_value) = read_config_or_empty(&config_path)?;

    // Compute the desired after-state.
    let after_value = if t_args.uninstall {
        remove_managed_block(target, before_value.clone())?
    } else {
        apply_managed_block(target, before_value.clone(), &binary)?
    };

    // Pretty-print both for diff display and for the eventual write.
    let after_text = serde_json::to_string_pretty(&after_value)? + "\n";

    // Round-trip check: re-parse what we serialized so we never write
    // bytes we couldn't read back.
    let _: Value = serde_json::from_str(&after_text)
        .context("internal error: serialised config did not round-trip through JSON parser")?;

    let action_label = if t_args.uninstall {
        "uninstall"
    } else {
        "install"
    };

    if before_text.trim() == after_text.trim() {
        writeln!(
            out.stdout,
            "ai-memory install: {target} {action} is a no-op (managed block already in desired state)",
            target = target.name(),
            action = action_label,
        )?;
        return Ok(());
    }

    if !t_args.apply {
        // Dry-run mode (the default). Emit a unified-style diff so the
        // caller can scrutinise the change before opting in to write.
        writeln!(
            out.stdout,
            "ai-memory install: dry-run for {target} {action} at {path}",
            target = target.name(),
            action = action_label,
            path = config_path.display(),
        )?;
        writeln!(out.stdout, "--- before")?;
        writeln!(out.stdout, "+++ after")?;
        emit_diff(out, &before_text, &after_text)?;
        writeln!(
            out.stdout,
            "ai-memory install: re-run with --apply to write the changes"
        )?;
        return Ok(());
    }

    // Apply mode. Backup first, then write.
    let backup_path = if config_path.exists() {
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string();
        let backup = config_path.with_extension(format!(
            "{ext}bak.{ts}",
            ext = match config_path.extension().and_then(|e| e.to_str()) {
                Some(existing) => format!("{existing}."),
                None => String::new(),
            }
        ));
        std::fs::copy(&config_path, &backup).with_context(|| {
            format!(
                "backing up {} to {}",
                config_path.display(),
                backup.display()
            )
        })?;
        Some(backup)
    } else {
        None
    };

    if let Some(parent) = config_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory {}", parent.display()))?;
    }

    std::fs::write(&config_path, &after_text)
        .with_context(|| format!("writing {}", config_path.display()))?;

    writeln!(
        out.stdout,
        "ai-memory install: {action} applied to {path}",
        action = action_label,
        path = config_path.display(),
    )?;
    if let Some(b) = backup_path {
        writeln!(out.stdout, "ai-memory install: backup at {}", b.display())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Config-path resolution
// ---------------------------------------------------------------------------

fn resolve_config_path(target: Target, args: &TargetArgs) -> Result<PathBuf> {
    if let Some(ref p) = args.config {
        return Ok(p.clone());
    }
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow!("could not resolve home directory; pass --config <path>"))?;
    let p = match target {
        Target::ClaudeCode => home.join(".claude").join("settings.json"),
        Target::Openclaw => {
            // OpenClaw's documented MCP config path is not stable across
            // versions; the canonical location is documented at
            // https://docs.openclaw.ai/cli/mcp. We require --config for
            // OpenClaw to avoid guessing and writing to the wrong file.
            // TODO(#487): once OpenClaw publishes a stable canonical path,
            // wire it in here.
            bail!(
                "openclaw config path is not auto-discovered yet; pass --config <path>. \
                 See https://docs.openclaw.ai/cli/mcp for the canonical location."
            );
        }
        Target::Cursor => home.join(".cursor").join("mcp.json"),
        Target::Cline => {
            // Cline's config path varies by version (mcp_settings.json
            // location moved between releases). Require explicit --config
            // until upstream stabilises.
            // TODO(#487): once Cline pins a canonical path, wire it.
            bail!(
                "cline config path varies by version; pass --config <path> \
                 (typically ~/.cline/mcp_settings.json or under the VS Code \
                 extension data dir)."
            );
        }
        Target::Continue => home.join(".continue").join("config.json"),
        Target::Windsurf => home
            .join(".codeium")
            .join("windsurf")
            .join("mcp_config.json"),
        // v0.6.4-010 — claude-desktop has documented OS-specific paths.
        // Linux is unstable (depends on AppImage / Flatpak distribution),
        // so require --config there.
        Target::ClaudeDesktop => {
            #[cfg(target_os = "macos")]
            {
                home.join("Library")
                    .join("Application Support")
                    .join("Claude")
                    .join("claude_desktop_config.json")
            }
            #[cfg(target_os = "windows")]
            {
                std::env::var_os("APPDATA")
                    .map(|p| {
                        std::path::PathBuf::from(p)
                            .join("Claude")
                            .join("claude_desktop_config.json")
                    })
                    .unwrap_or_else(|| {
                        home.join("AppData")
                            .join("Roaming")
                            .join("Claude")
                            .join("claude_desktop_config.json")
                    })
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            {
                bail!(
                    "claude-desktop config path is OS-specific and not auto-discovered \
                     on Linux; pass --config <path>. Common location: \
                     ~/.config/Claude/claude_desktop_config.json"
                );
            }
        }
        // v0.6.4-010 — codex, grok-cli, gemini-cli configs vary by version.
        // Mirror the openclaw/cline pattern: require --config explicitly
        // to avoid writing to the wrong file.
        Target::Codex => {
            bail!(
                "codex config path varies by version; pass --config <path>. \
                 Common location: ~/.codex/config.json or ~/.config/codex/mcp.json"
            );
        }
        Target::GrokCli => {
            bail!(
                "grok-cli config path varies by version; pass --config <path>. \
                 Common location: ~/.grok/mcp.json"
            );
        }
        Target::GeminiCli => {
            bail!(
                "gemini-cli config path varies by version; pass --config <path>. \
                 Common location: ~/.gemini/mcp.json"
            );
        }
    };
    Ok(p)
}

/// Resolve the `ai-memory` binary path for the generated config's
/// `command` field. If the user passes `--binary`, use that. Otherwise:
///
/// 1. If `ai-memory` is on `$PATH`, use the bare string `ai-memory` so
///    the config stays portable across machines that have it linked to
///    different absolute paths.
/// 2. Otherwise, use the running binary's `current_exe()` so the
///    generated config is at least functional on the host.
fn resolve_binary(override_path: Option<&Path>) -> String {
    if let Some(p) = override_path {
        return p.display().to_string();
    }
    if which_ai_memory().is_some() {
        return "ai-memory".to_string();
    }
    if let Ok(exe) = std::env::current_exe() {
        return exe.display().to_string();
    }
    "ai-memory".to_string()
}

fn which_ai_memory() -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join("ai-memory");
        if candidate.is_file() {
            return Some(candidate);
        }
        let candidate_exe = dir.join("ai-memory.exe");
        if candidate_exe.is_file() {
            return Some(candidate_exe);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Read / parse
// ---------------------------------------------------------------------------

/// Read `path` and parse as JSON. Returns `("", {})` if the file does
/// not exist (a fresh install on a host that's never run the agent).
/// Errors clearly when the file exists but is not valid JSON.
fn read_config_or_empty(path: &Path) -> Result<(String, Value)> {
    if !path.exists() {
        return Ok((String::new(), Value::Object(Map::new())));
    }
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok((text, Value::Object(Map::new())));
    }
    let value: Value = serde_json::from_str(&text).map_err(|e| {
        anyhow!(
            "existing config at {} is not valid JSON ({e}). \
             Refusing to overwrite — fix the file by hand or remove it, \
             then re-run `ai-memory install`.",
            path.display()
        )
    })?;
    Ok((text, value))
}

// ---------------------------------------------------------------------------
// Apply / remove managed block
// ---------------------------------------------------------------------------

/// Insert or replace the managed block for `target` inside `cfg`.
fn apply_managed_block(target: Target, mut cfg: Value, binary: &str) -> Result<Value> {
    let obj = ensure_object(&mut cfg)?;
    match target {
        Target::ClaudeCode => apply_claude_code(obj, binary),
        Target::Openclaw => apply_openclaw(obj, binary),
        Target::Cursor => apply_cursor(obj, binary),
        Target::Cline => apply_cline(obj, binary),
        Target::Continue => apply_continue(obj, binary),
        Target::Windsurf => apply_windsurf(obj, binary),
        // v0.6.4-010 — these four harnesses use the canonical
        // `mcpServers.ai-memory.{command, args, env}` shape.
        Target::ClaudeDesktop | Target::Codex | Target::GrokCli | Target::GeminiCli => {
            apply_mcp_standard(obj, binary);
        }
    }
    Ok(cfg)
}

/// Remove the managed block for `target` from `cfg` (if present).
fn remove_managed_block(target: Target, mut cfg: Value) -> Result<Value> {
    let obj = match cfg.as_object_mut() {
        Some(o) => o,
        None => return Ok(cfg),
    };
    match target {
        Target::ClaudeCode => remove_claude_code(obj),
        Target::Openclaw => remove_openclaw(obj),
        Target::Cursor => remove_cursor(obj),
        Target::Cline => remove_cline(obj),
        Target::Continue => remove_continue(obj),
        Target::Windsurf => remove_windsurf(obj),
        // v0.6.4-010 — shared mcpServers.ai-memory shape (claude-desktop,
        // codex, grok-cli, gemini-cli).
        Target::ClaudeDesktop | Target::Codex | Target::GrokCli | Target::GeminiCli => {
            remove_mcp_standard(obj);
        }
    }
    Ok(cfg)
}

// --- v0.6.4-010 shared MCP-standard writer --------------------------------
//
// claude-desktop, codex, grok-cli, and gemini-cli all consume the
// canonical `mcpServers.<name>.{command,args,env}` shape — the
// MCP-spec-defined server-config form. `apply_mcp_standard` writes the
// ai-memory entry with `--profile core` baked into the args (the v0.6.4
// default). Operators who want the v0.6.3 surface 1:1 can hand-edit the
// args to `["mcp", "--profile", "full"]`; the install dry-run + diff
// makes that change visible before they apply it.

fn apply_mcp_standard(obj: &mut Map<String, Value>, binary: &str) {
    let mcp_servers = obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !mcp_servers.is_object() {
        *mcp_servers = Value::Object(Map::new());
    }
    let mcp_obj = mcp_servers.as_object_mut().expect("just-inserted object");
    mcp_obj.insert(
        "ai-memory".to_string(),
        serde_json::json!({
            MARKER_START_KEY: MARKER_PAYLOAD,
            MANAGED_KEYS_PROPERTY: ["command", "args", "env"],
            "command": binary,
            // v0.6.4-010 — explicitly request the v0.6.4 default surface.
            // The runtime would default to `core` anyway via
            // effective_profile(), but having it written here makes the
            // selection self-documenting in the user's config file.
            "args": ["mcp", "--profile", "core"],
            "env": {},
            MARKER_END_KEY: MARKER_PAYLOAD,
        }),
    );
}

fn remove_mcp_standard(obj: &mut Map<String, Value>) {
    if let Some(mcp_servers) = obj.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
        mcp_servers.remove("ai-memory");
        if mcp_servers.is_empty() {
            obj.remove("mcpServers");
        }
    }
}

fn ensure_object(v: &mut Value) -> Result<&mut Map<String, Value>> {
    if !v.is_object() {
        bail!("existing config root is not a JSON object; refusing to clobber");
    }
    Ok(v.as_object_mut().expect("checked is_object"))
}

// --- Claude Code ----------------------------------------------------------

/// Hook command stored in claude-code's SessionStart entry. Mirrors the
/// recipe documented in `docs/integrations/claude-code.md`.
fn claude_code_hook_command(binary: &str) -> String {
    format!("{binary} boot --quiet --limit 10 --budget-tokens 4096")
}

fn apply_claude_code(obj: &mut Map<String, Value>, binary: &str) {
    // Build the desired SessionStart entry under the marker.
    let cmd = claude_code_hook_command(binary);
    let entry = serde_json::json!({
        MARKER_START_KEY: MARKER_PAYLOAD,
        MANAGED_KEYS_PROPERTY: ["matcher", "hooks"],
        "matcher": "*",
        "hooks": [
            { "type": "command", "command": cmd }
        ],
        MARKER_END_KEY: MARKER_PAYLOAD,
    });

    // Drop into hooks.SessionStart, removing any existing managed entry,
    // then prepend ours.
    let hooks = obj
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    let hooks_obj = hooks.as_object_mut().expect("just-inserted object");
    let session_start = hooks_obj
        .entry("SessionStart".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !session_start.is_array() {
        *session_start = Value::Array(Vec::new());
    }
    let arr = session_start.as_array_mut().expect("just-inserted array");
    arr.retain(|v| !is_managed_value(v));
    arr.insert(0, entry);
}

fn remove_claude_code(obj: &mut Map<String, Value>) {
    if let Some(hooks) = obj.get_mut("hooks").and_then(|h| h.as_object_mut())
        && let Some(arr) = hooks.get_mut("SessionStart").and_then(|s| s.as_array_mut())
    {
        arr.retain(|v| !is_managed_value(v));
        if arr.is_empty() {
            hooks.remove("SessionStart");
        }
    }
    // Don't strip an empty hooks object if the user had one — leave their
    // structure exactly as we found it minus our block.
    if let Some(hooks) = obj.get("hooks").and_then(|h| h.as_object())
        && hooks.is_empty()
    {
        obj.remove("hooks");
    }
}

// --- OpenClaw -------------------------------------------------------------

fn ai_memory_server_value(binary: &str) -> Value {
    serde_json::json!({
        MARKER_START_KEY: MARKER_PAYLOAD,
        MANAGED_KEYS_PROPERTY: ["command", "args"],
        "command": binary,
        "args": ["mcp"],
        MARKER_END_KEY: MARKER_PAYLOAD,
    })
}

fn apply_openclaw(obj: &mut Map<String, Value>, binary: &str) {
    let mcp = obj
        .entry("mcp".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !mcp.is_object() {
        *mcp = Value::Object(Map::new());
    }
    let mcp_obj = mcp.as_object_mut().expect("just-inserted object");
    let servers = mcp_obj
        .entry("servers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !servers.is_object() {
        *servers = Value::Object(Map::new());
    }
    let servers_obj = servers.as_object_mut().expect("just-inserted object");
    servers_obj.insert("ai-memory".to_string(), ai_memory_server_value(binary));
}

fn remove_openclaw(obj: &mut Map<String, Value>) {
    if let Some(mcp) = obj.get_mut("mcp").and_then(|v| v.as_object_mut())
        && let Some(servers) = mcp.get_mut("servers").and_then(|v| v.as_object_mut())
    {
        if let Some(v) = servers.get("ai-memory") {
            if is_managed_value(v) {
                servers.remove("ai-memory");
            }
        }
        if servers.is_empty() {
            mcp.remove("servers");
        }
        if mcp.is_empty() {
            obj.remove("mcp");
        }
    }
}

// --- Cursor ---------------------------------------------------------------

fn apply_cursor(obj: &mut Map<String, Value>, binary: &str) {
    let servers = obj
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !servers.is_object() {
        *servers = Value::Object(Map::new());
    }
    let servers_obj = servers.as_object_mut().expect("just-inserted object");
    servers_obj.insert("ai-memory".to_string(), ai_memory_server_value(binary));
}

fn remove_cursor(obj: &mut Map<String, Value>) {
    if let Some(servers) = obj.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
        if let Some(v) = servers.get("ai-memory") {
            if is_managed_value(v) {
                servers.remove("ai-memory");
            }
        }
        if servers.is_empty() {
            obj.remove("mcpServers");
        }
    }
}

// --- Cline ----------------------------------------------------------------

fn apply_cline(obj: &mut Map<String, Value>, binary: &str) {
    // Cline shape mirrors Cursor (mcpServers).
    apply_cursor(obj, binary);
}

fn remove_cline(obj: &mut Map<String, Value>) {
    remove_cursor(obj);
}

// --- Continue -------------------------------------------------------------

fn apply_continue(obj: &mut Map<String, Value>, binary: &str) {
    // Continue's MCP config lives under experimental.modelContextProtocolServers
    // (an array of transport entries).
    let exp = obj
        .entry("experimental".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !exp.is_object() {
        *exp = Value::Object(Map::new());
    }
    let exp_obj = exp.as_object_mut().expect("just-inserted object");
    let arr = exp_obj
        .entry("modelContextProtocolServers".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !arr.is_array() {
        *arr = Value::Array(Vec::new());
    }
    let arr = arr.as_array_mut().expect("just-inserted array");
    arr.retain(|v| !is_managed_value(v));
    let entry = serde_json::json!({
        MARKER_START_KEY: MARKER_PAYLOAD,
        MANAGED_KEYS_PROPERTY: ["transport"],
        "transport": {
            "type": "stdio",
            "command": binary,
            "args": ["mcp"],
        },
        MARKER_END_KEY: MARKER_PAYLOAD,
    });
    arr.insert(0, entry);
}

fn remove_continue(obj: &mut Map<String, Value>) {
    if let Some(exp) = obj.get_mut("experimental").and_then(|v| v.as_object_mut()) {
        if let Some(arr) = exp
            .get_mut("modelContextProtocolServers")
            .and_then(|v| v.as_array_mut())
        {
            arr.retain(|v| !is_managed_value(v));
            if arr.is_empty() {
                exp.remove("modelContextProtocolServers");
            }
        }
        if exp.is_empty() {
            obj.remove("experimental");
        }
    }
}

// --- Windsurf -------------------------------------------------------------

fn apply_windsurf(obj: &mut Map<String, Value>, binary: &str) {
    apply_cursor(obj, binary);
}

fn remove_windsurf(obj: &mut Map<String, Value>) {
    remove_cursor(obj);
}

// ---------------------------------------------------------------------------
// Marker recognition
// ---------------------------------------------------------------------------

/// Returns true when `v` is a JSON object carrying our managed-block
/// start sentinel. Used to recognise an existing managed block so
/// install can replace it precisely and uninstall can remove it.
fn is_managed_value(v: &Value) -> bool {
    v.as_object()
        .and_then(|o| o.get(MARKER_START_KEY))
        .is_some()
}

// ---------------------------------------------------------------------------
// Diff emission
// ---------------------------------------------------------------------------

/// Write a minimal unified-style diff between `before` and `after` to
/// `out.stdout`. We avoid pulling a real diff crate; the implementation
/// is intentionally simple — line-by-line, no LCS — because the diff is
/// *advisory* (the caller can still inspect after with `--apply`).
fn emit_diff(out: &mut CliOutput<'_>, before: &str, after: &str) -> Result<()> {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let max_len = before_lines.len().max(after_lines.len());
    for i in 0..max_len {
        let b = before_lines.get(i).copied();
        let a = after_lines.get(i).copied();
        match (b, a) {
            (Some(bl), Some(al)) if bl == al => writeln!(out.stdout, " {bl}")?,
            (Some(bl), Some(al)) => {
                writeln!(out.stdout, "-{bl}")?;
                writeln!(out.stdout, "+{al}")?;
            }
            (Some(bl), None) => writeln!(out.stdout, "-{bl}")?,
            (None, Some(al)) => writeln!(out.stdout, "+{al}")?,
            (None, None) => {}
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::TestEnv;
    use std::fs;

    fn args_for(target: Target, config: PathBuf) -> InstallArgs {
        let t = TargetArgs {
            config: Some(config),
            apply: false,
            dry_run: false,
            uninstall: false,
            binary: Some(PathBuf::from("/usr/local/bin/ai-memory")),
        };
        let target_cmd = match target {
            Target::ClaudeCode => TargetCmd::ClaudeCode(t),
            Target::Openclaw => TargetCmd::Openclaw(t),
            Target::Cursor => TargetCmd::Cursor(t),
            Target::Cline => TargetCmd::Cline(t),
            Target::Continue => TargetCmd::Continue(t),
            Target::Windsurf => TargetCmd::Windsurf(t),
            Target::ClaudeDesktop => TargetCmd::ClaudeDesktop(t),
            Target::Codex => TargetCmd::Codex(t),
            Target::GrokCli => TargetCmd::GrokCli(t),
            Target::GeminiCli => TargetCmd::GeminiCli(t),
        };
        InstallArgs { target: target_cmd }
    }

    fn args_for_apply(target: Target, config: PathBuf) -> InstallArgs {
        let mut a = args_for(target, config);
        match &mut a.target {
            TargetCmd::ClaudeCode(t)
            | TargetCmd::Openclaw(t)
            | TargetCmd::Cursor(t)
            | TargetCmd::Cline(t)
            | TargetCmd::Continue(t)
            | TargetCmd::Windsurf(t)
            | TargetCmd::ClaudeDesktop(t)
            | TargetCmd::Codex(t)
            | TargetCmd::GrokCli(t)
            | TargetCmd::GeminiCli(t) => {
                t.apply = true;
            }
        }
        a
    }

    fn args_for_uninstall_apply(target: Target, config: PathBuf) -> InstallArgs {
        let mut a = args_for(target, config);
        match &mut a.target {
            TargetCmd::ClaudeCode(t)
            | TargetCmd::Openclaw(t)
            | TargetCmd::Cursor(t)
            | TargetCmd::Cline(t)
            | TargetCmd::Continue(t)
            | TargetCmd::Windsurf(t)
            | TargetCmd::ClaudeDesktop(t)
            | TargetCmd::Codex(t)
            | TargetCmd::GrokCli(t)
            | TargetCmd::GeminiCli(t) => {
                t.uninstall = true;
                t.apply = true;
            }
        }
        a
    }

    fn config_path(env: &TestEnv, name: &str) -> PathBuf {
        env.db_path.parent().unwrap().join(name)
    }

    fn seed(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    // --------------------------------------------------------------
    // claude-code
    // --------------------------------------------------------------

    #[test]
    fn claude_code_install_dry_run_emits_diff_no_writes() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "settings.json");
        seed(&path, "{\n}\n");
        let mtime_before = fs::metadata(&path).unwrap().modified().unwrap();
        let args = args_for(Target::ClaudeCode, path.clone());
        let mut out = env.output();
        run(&args, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(stdout.contains("dry-run"));
        assert!(stdout.contains("SessionStart"));
        assert!(stdout.contains("ai-memory"));
        assert!(stdout.contains(MARKER_START_KEY));
        let mtime_after = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after, "dry-run must not write");
    }

    #[test]
    fn claude_code_install_apply_writes_marker_block() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "settings.json");
        seed(&path, "{}\n");
        let args = args_for_apply(Target::ClaudeCode, path.clone());
        let mut out = env.output();
        run(&args, &mut out).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains(MARKER_START_KEY));
        assert!(written.contains(MARKER_END_KEY));
        assert!(written.contains("SessionStart"));
        assert!(written.contains("ai-memory"));
        // Must remain valid JSON.
        let _: Value = serde_json::from_str(&written).unwrap();
    }

    #[test]
    fn claude_code_install_apply_preserves_user_keys() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "settings.json");
        seed(
            &path,
            r#"{"theme":"dark","permissions":{"allow":["npm:*"]}}"#,
        );
        let args = args_for_apply(Target::ClaudeCode, path.clone());
        let mut out = env.output();
        run(&args, &mut out).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed["theme"], "dark");
        assert_eq!(parsed["permissions"]["allow"][0], "npm:*");
        assert!(parsed["hooks"]["SessionStart"].is_array());
    }

    #[test]
    fn claude_code_install_apply_is_idempotent() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "settings.json");
        seed(&path, "{}\n");
        let args = args_for_apply(Target::ClaudeCode, path.clone());
        let mut out = env.output();
        run(&args, &mut out).unwrap();
        let after_first = fs::read_to_string(&path).unwrap();
        // Second run should produce a no-op message and no change.
        env.stdout.clear();
        let mut out2 = env.output();
        run(&args, &mut out2).unwrap();
        let after_second = fs::read_to_string(&path).unwrap();
        assert_eq!(after_first, after_second);
        let stdout2 = std::str::from_utf8(&env.stdout).unwrap();
        assert!(
            stdout2.contains("no-op"),
            "second install should be no-op: {stdout2}"
        );
    }

    #[test]
    fn claude_code_uninstall_removes_marker_block_only() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "settings.json");
        let original = "{\n  \"theme\": \"dark\"\n}\n";
        seed(&path, original);
        // Install, then uninstall.
        run(
            &args_for_apply(Target::ClaudeCode, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let after_install = fs::read_to_string(&path).unwrap();
        assert!(after_install.contains(MARKER_START_KEY));
        run(
            &args_for_uninstall_apply(Target::ClaudeCode, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let after_uninstall = fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&after_uninstall).unwrap();
        assert_eq!(parsed["theme"], "dark");
        assert!(
            parsed.get("hooks").is_none(),
            "hooks should be gone after uninstall"
        );
        assert!(!after_uninstall.contains(MARKER_START_KEY));
    }

    #[test]
    fn claude_code_install_refuses_malformed_config() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "settings.json");
        seed(&path, "{not valid json");
        let args = args_for_apply(Target::ClaudeCode, path.clone());
        let mut out = env.output();
        let err = run(&args, &mut out).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("not valid JSON"),
            "error should explain malformed json: {msg}"
        );
        // File must NOT have been overwritten.
        let still = fs::read_to_string(&path).unwrap();
        assert_eq!(still, "{not valid json");
    }

    #[test]
    fn claude_code_install_writes_backup_file() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "settings.json");
        seed(&path, "{}\n");
        let args = args_for_apply(Target::ClaudeCode, path.clone());
        let mut out = env.output();
        run(&args, &mut out).unwrap();
        // Find a sibling whose name starts with `settings.json.bak.`.
        let parent = path.parent().unwrap();
        let backups: Vec<_> = fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("settings.json.bak.")
                    || e.file_name().to_string_lossy().starts_with("settings.bak.")
            })
            .collect();
        assert!(
            !backups.is_empty(),
            "expected a settings.bak.<ts> backup beside the config; saw: {:?}",
            fs::read_dir(parent)
                .unwrap()
                .filter_map(|e| e.ok())
                .map(|e| e.file_name())
                .collect::<Vec<_>>()
        );
    }

    // --------------------------------------------------------------
    // cursor
    // --------------------------------------------------------------

    #[test]
    fn cursor_install_dry_run_emits_diff_no_writes() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "mcp.json");
        seed(&path, "{}\n");
        let mtime_before = fs::metadata(&path).unwrap().modified().unwrap();
        let args = args_for(Target::Cursor, path.clone());
        let mut out = env.output();
        run(&args, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(stdout.contains("dry-run"));
        assert!(stdout.contains("mcpServers"));
        let mtime_after = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after);
    }

    #[test]
    fn cursor_install_apply_writes_marker_block() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "mcp.json");
        seed(&path, "{}\n");
        let args = args_for_apply(Target::Cursor, path.clone());
        run(&args, &mut env.output()).unwrap();
        let written = fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&written).unwrap();
        assert!(parsed["mcpServers"]["ai-memory"][MARKER_START_KEY].is_string());
        assert_eq!(
            parsed["mcpServers"]["ai-memory"]["command"],
            "/usr/local/bin/ai-memory"
        );
    }

    #[test]
    fn cursor_install_apply_preserves_user_keys() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "mcp.json");
        seed(
            &path,
            r#"{"mcpServers":{"my-other":{"command":"x"}},"telemetry":false}"#,
        );
        run(
            &args_for_apply(Target::Cursor, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["telemetry"], false);
        assert_eq!(parsed["mcpServers"]["my-other"]["command"], "x");
        assert!(parsed["mcpServers"]["ai-memory"][MARKER_START_KEY].is_string());
    }

    #[test]
    fn cursor_install_apply_is_idempotent() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "mcp.json");
        seed(&path, "{}\n");
        let args = args_for_apply(Target::Cursor, path.clone());
        run(&args, &mut env.output()).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        run(&args, &mut env.output()).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn cursor_uninstall_removes_marker_block_only() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "mcp.json");
        let original = r#"{"mcpServers":{"my-other":{"command":"x"}}}"#;
        seed(&path, original);
        run(
            &args_for_apply(Target::Cursor, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        run(
            &args_for_uninstall_apply(Target::Cursor, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["mcpServers"]["my-other"]["command"], "x");
        assert!(
            parsed["mcpServers"]
                .as_object()
                .unwrap()
                .get("ai-memory")
                .is_none()
        );
    }

    #[test]
    fn cursor_install_refuses_malformed_config() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "mcp.json");
        seed(&path, "not json");
        let args = args_for_apply(Target::Cursor, path.clone());
        let err = run(&args, &mut env.output()).unwrap_err();
        assert!(format!("{err}").contains("not valid JSON"));
    }

    #[test]
    fn cursor_install_writes_backup_file() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "mcp.json");
        seed(&path, "{}\n");
        run(
            &args_for_apply(Target::Cursor, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parent = path.parent().unwrap();
        let any_backup = fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains("bak."));
        assert!(any_backup);
    }

    // --------------------------------------------------------------
    // openclaw
    // --------------------------------------------------------------

    #[test]
    fn openclaw_install_dry_run_emits_diff_no_writes() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "openclaw.json");
        seed(&path, "{}\n");
        let mtime_before = fs::metadata(&path).unwrap().modified().unwrap();
        run(&args_for(Target::Openclaw, path.clone()), &mut env.output()).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(stdout.contains("dry-run"));
        assert!(stdout.contains("mcp"));
        assert_eq!(
            mtime_before,
            fs::metadata(&path).unwrap().modified().unwrap()
        );
    }

    #[test]
    fn openclaw_install_apply_writes_marker_block() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "openclaw.json");
        seed(&path, "{}\n");
        run(
            &args_for_apply(Target::Openclaw, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed["mcp"]["servers"]["ai-memory"][MARKER_START_KEY].is_string());
    }

    #[test]
    fn openclaw_install_apply_preserves_user_keys() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "openclaw.json");
        seed(
            &path,
            r#"{"mcp":{"servers":{"other":{"command":"y"}}},"editor":"vim"}"#,
        );
        run(
            &args_for_apply(Target::Openclaw, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["editor"], "vim");
        assert_eq!(parsed["mcp"]["servers"]["other"]["command"], "y");
        assert!(parsed["mcp"]["servers"]["ai-memory"][MARKER_START_KEY].is_string());
    }

    #[test]
    fn openclaw_install_apply_is_idempotent() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "openclaw.json");
        seed(&path, "{}\n");
        let args = args_for_apply(Target::Openclaw, path.clone());
        run(&args, &mut env.output()).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        run(&args, &mut env.output()).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn openclaw_uninstall_removes_marker_block_only() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "openclaw.json");
        seed(&path, r#"{"mcp":{"servers":{"other":{"command":"y"}}}}"#);
        run(
            &args_for_apply(Target::Openclaw, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        run(
            &args_for_uninstall_apply(Target::Openclaw, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["mcp"]["servers"]["other"]["command"], "y");
        assert!(
            parsed["mcp"]["servers"]
                .as_object()
                .unwrap()
                .get("ai-memory")
                .is_none()
        );
    }

    #[test]
    fn openclaw_install_refuses_malformed_config() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "openclaw.json");
        seed(&path, "garbage");
        let err = run(
            &args_for_apply(Target::Openclaw, path.clone()),
            &mut env.output(),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("not valid JSON"));
    }

    #[test]
    fn openclaw_install_writes_backup_file() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "openclaw.json");
        seed(&path, "{}\n");
        run(
            &args_for_apply(Target::Openclaw, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parent = path.parent().unwrap();
        assert!(
            fs::read_dir(parent)
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains("bak."))
        );
    }

    // --------------------------------------------------------------
    // cline (shape ≈ cursor)
    // --------------------------------------------------------------

    #[test]
    fn cline_install_dry_run_emits_diff_no_writes() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "cline.json");
        seed(&path, "{}\n");
        let mtime_before = fs::metadata(&path).unwrap().modified().unwrap();
        run(&args_for(Target::Cline, path.clone()), &mut env.output()).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(stdout.contains("dry-run"));
        assert!(stdout.contains("mcpServers"));
        assert_eq!(
            mtime_before,
            fs::metadata(&path).unwrap().modified().unwrap()
        );
    }

    #[test]
    fn cline_install_apply_writes_marker_block() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "cline.json");
        seed(&path, "{}\n");
        run(
            &args_for_apply(Target::Cline, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed["mcpServers"]["ai-memory"][MARKER_START_KEY].is_string());
    }

    #[test]
    fn cline_install_apply_preserves_user_keys() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "cline.json");
        seed(&path, r#"{"mcpServers":{"x":{"command":"q"}},"foo":1}"#);
        run(
            &args_for_apply(Target::Cline, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["foo"], 1);
        assert_eq!(parsed["mcpServers"]["x"]["command"], "q");
    }

    #[test]
    fn cline_install_apply_is_idempotent() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "cline.json");
        seed(&path, "{}\n");
        let args = args_for_apply(Target::Cline, path.clone());
        run(&args, &mut env.output()).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        run(&args, &mut env.output()).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn cline_uninstall_removes_marker_block_only() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "cline.json");
        seed(&path, r#"{"mcpServers":{"x":{"command":"q"}}}"#);
        run(
            &args_for_apply(Target::Cline, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        run(
            &args_for_uninstall_apply(Target::Cline, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["mcpServers"]["x"]["command"], "q");
        assert!(
            parsed["mcpServers"]
                .as_object()
                .unwrap()
                .get("ai-memory")
                .is_none()
        );
    }

    #[test]
    fn cline_install_refuses_malformed_config() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "cline.json");
        seed(&path, "totally not json");
        let err = run(
            &args_for_apply(Target::Cline, path.clone()),
            &mut env.output(),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("not valid JSON"));
    }

    #[test]
    fn cline_install_writes_backup_file() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "cline.json");
        seed(&path, "{}\n");
        run(
            &args_for_apply(Target::Cline, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        assert!(
            fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains("bak."))
        );
    }

    // --------------------------------------------------------------
    // continue
    // --------------------------------------------------------------

    #[test]
    fn continue_install_dry_run_emits_diff_no_writes() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "continue.json");
        seed(&path, "{}\n");
        let mtime_before = fs::metadata(&path).unwrap().modified().unwrap();
        run(&args_for(Target::Continue, path.clone()), &mut env.output()).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(stdout.contains("dry-run"));
        assert!(stdout.contains("modelContextProtocolServers"));
        assert_eq!(
            mtime_before,
            fs::metadata(&path).unwrap().modified().unwrap()
        );
    }

    #[test]
    fn continue_install_apply_writes_marker_block() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "continue.json");
        seed(&path, "{}\n");
        run(
            &args_for_apply(Target::Continue, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let arr = parsed["experimental"]["modelContextProtocolServers"]
            .as_array()
            .unwrap();
        assert!(arr.iter().any(is_managed_value));
    }

    #[test]
    fn continue_install_apply_preserves_user_keys() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "continue.json");
        seed(
            &path,
            r#"{"models":[{"name":"x"}],"experimental":{"foo":true}}"#,
        );
        run(
            &args_for_apply(Target::Continue, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["models"][0]["name"], "x");
        assert_eq!(parsed["experimental"]["foo"], true);
    }

    #[test]
    fn continue_install_apply_is_idempotent() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "continue.json");
        seed(&path, "{}\n");
        let args = args_for_apply(Target::Continue, path.clone());
        run(&args, &mut env.output()).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        run(&args, &mut env.output()).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn continue_uninstall_removes_marker_block_only() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "continue.json");
        seed(&path, r#"{"models":[{"name":"x"}]}"#);
        run(
            &args_for_apply(Target::Continue, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        run(
            &args_for_uninstall_apply(Target::Continue, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["models"][0]["name"], "x");
        assert!(parsed.get("experimental").is_none());
    }

    #[test]
    fn continue_install_refuses_malformed_config() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "continue.json");
        seed(&path, "[1,2,");
        let err = run(
            &args_for_apply(Target::Continue, path.clone()),
            &mut env.output(),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("not valid JSON"));
    }

    #[test]
    fn continue_install_writes_backup_file() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "continue.json");
        seed(&path, "{}\n");
        run(
            &args_for_apply(Target::Continue, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        assert!(
            fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains("bak."))
        );
    }

    // --------------------------------------------------------------
    // windsurf (shape ≈ cursor)
    // --------------------------------------------------------------

    #[test]
    fn windsurf_install_dry_run_emits_diff_no_writes() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "mcp_config.json");
        seed(&path, "{}\n");
        let mtime_before = fs::metadata(&path).unwrap().modified().unwrap();
        run(&args_for(Target::Windsurf, path.clone()), &mut env.output()).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(stdout.contains("dry-run"));
        assert!(stdout.contains("mcpServers"));
        assert_eq!(
            mtime_before,
            fs::metadata(&path).unwrap().modified().unwrap()
        );
    }

    #[test]
    fn windsurf_install_apply_writes_marker_block() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "mcp_config.json");
        seed(&path, "{}\n");
        run(
            &args_for_apply(Target::Windsurf, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(parsed["mcpServers"]["ai-memory"][MARKER_START_KEY].is_string());
    }

    #[test]
    fn windsurf_install_apply_preserves_user_keys() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "mcp_config.json");
        seed(&path, r#"{"mcpServers":{"k":{"command":"l"}},"a":42}"#);
        run(
            &args_for_apply(Target::Windsurf, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["a"], 42);
        assert_eq!(parsed["mcpServers"]["k"]["command"], "l");
    }

    #[test]
    fn windsurf_install_apply_is_idempotent() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "mcp_config.json");
        seed(&path, "{}\n");
        let args = args_for_apply(Target::Windsurf, path.clone());
        run(&args, &mut env.output()).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        run(&args, &mut env.output()).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn windsurf_uninstall_removes_marker_block_only() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "mcp_config.json");
        seed(&path, r#"{"mcpServers":{"k":{"command":"l"}}}"#);
        run(
            &args_for_apply(Target::Windsurf, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        run(
            &args_for_uninstall_apply(Target::Windsurf, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["mcpServers"]["k"]["command"], "l");
        assert!(
            parsed["mcpServers"]
                .as_object()
                .unwrap()
                .get("ai-memory")
                .is_none()
        );
    }

    #[test]
    fn windsurf_install_refuses_malformed_config() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "mcp_config.json");
        seed(&path, "::");
        let err = run(
            &args_for_apply(Target::Windsurf, path.clone()),
            &mut env.output(),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("not valid JSON"));
    }

    #[test]
    fn windsurf_install_writes_backup_file() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "mcp_config.json");
        seed(&path, "{}\n");
        run(
            &args_for_apply(Target::Windsurf, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        assert!(
            fs::read_dir(path.parent().unwrap())
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().contains("bak."))
        );
    }

    // --------------------------------------------------------------
    // generic / cross-cutting
    // --------------------------------------------------------------

    #[test]
    fn install_creates_missing_config_file_under_apply() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "fresh-config.json");
        assert!(!path.exists());
        run(
            &args_for_apply(Target::Cursor, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        assert!(path.exists());
        let _: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    }

    #[test]
    fn install_round_trip_install_then_uninstall_restores_original_for_empty_seed() {
        // For a config that started as `{}\n`, install + uninstall should
        // produce a configuration that re-parses to `{}` (key set is empty).
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "rt.json");
        seed(&path, "{}\n");
        run(
            &args_for_apply(Target::Cursor, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        run(
            &args_for_uninstall_apply(Target::Cursor, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed, serde_json::json!({}));
    }

    #[test]
    fn resolve_binary_uses_override_when_provided() {
        let p = std::path::PathBuf::from("/custom/path/ai-memory");
        let resolved = resolve_binary(Some(&p));
        assert_eq!(resolved, "/custom/path/ai-memory");
    }

    // ---- v0.6.4-010 — per-harness install profiles ----
    //
    // The four MCP-standard harnesses (claude-desktop, codex, grok-cli,
    // gemini-cli) use the same `mcpServers.ai-memory.{command,args,env}`
    // shape. We test all four with a single shared assertion fixture
    // since the writer is shared.

    fn assert_mcp_standard_apply(target: Target, fname: &str) {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, fname);
        seed(&path, "{}\n");
        run(&args_for_apply(target, path.clone()), &mut env.output()).unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        // Standard MCP shape.
        assert!(
            parsed["mcpServers"]["ai-memory"][MARKER_START_KEY].is_string(),
            "{} missing managed-block marker",
            target.name()
        );
        // v0.6.4 default profile baked into args.
        let args = parsed["mcpServers"]["ai-memory"]["args"]
            .as_array()
            .unwrap();
        let strs: Vec<&str> = args.iter().filter_map(Value::as_str).collect();
        assert_eq!(
            strs,
            vec!["mcp", "--profile", "core"],
            "{} should write `mcp --profile core` args",
            target.name()
        );
        let cmd = parsed["mcpServers"]["ai-memory"]["command"]
            .as_str()
            .unwrap();
        assert_eq!(cmd, "/usr/local/bin/ai-memory");
    }

    #[test]
    fn claude_desktop_apply_writes_mcp_standard_with_profile_core() {
        assert_mcp_standard_apply(Target::ClaudeDesktop, "claude_desktop_config.json");
    }

    #[test]
    fn codex_apply_writes_mcp_standard_with_profile_core() {
        assert_mcp_standard_apply(Target::Codex, "codex_config.json");
    }

    #[test]
    fn grok_cli_apply_writes_mcp_standard_with_profile_core() {
        assert_mcp_standard_apply(Target::GrokCli, "grok_mcp.json");
    }

    #[test]
    fn gemini_cli_apply_writes_mcp_standard_with_profile_core() {
        assert_mcp_standard_apply(Target::GeminiCli, "gemini_mcp.json");
    }

    #[test]
    fn mcp_standard_uninstall_round_trip_restores_empty() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "claude_desktop_config.json");
        seed(&path, "{}\n");
        run(
            &args_for_apply(Target::ClaudeDesktop, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        run(
            &args_for_uninstall_apply(Target::ClaudeDesktop, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        // Empty mcpServers should be removed entirely.
        assert!(
            !parsed.as_object().unwrap().contains_key("mcpServers"),
            "uninstall should remove the empty mcpServers wrapper"
        );
    }

    #[test]
    fn mcp_standard_apply_preserves_user_keys() {
        let mut env = TestEnv::fresh();
        let path = config_path(&env, "codex_config.json");
        seed(
            &path,
            r#"{"mcpServers":{"other-mcp":{"command":"x","args":[]}},"unrelated":42}"#,
        );
        run(
            &args_for_apply(Target::Codex, path.clone()),
            &mut env.output(),
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        // Sibling server preserved.
        assert_eq!(parsed["mcpServers"]["other-mcp"]["command"], "x");
        // Sibling top-level key preserved.
        assert_eq!(parsed["unrelated"], 42);
        // ai-memory entry written.
        assert!(parsed["mcpServers"]["ai-memory"].is_object());
    }
}
