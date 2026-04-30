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
//!
//! ## Diagnostic manifest (PR-4 of #487)
//!
//! Boot's header is a transparent, multi-field manifest — never a black box.
//! Every field reflects a fact about *this* invocation so agents and humans
//! always know exactly what was loaded and what's configured. Fields:
//!
//! - `version`     — binary version (`CARGO_PKG_VERSION` at compile time)
//! - `db`          — resolved DB path + schema version + total live memories
//! - `tier`        — active feature tier and *configured* (not loaded)
//!                   embedder / reranker / llm models
//! - `latency`     — wall-clock from `run()` entry to header emit
//! - `namespace`   — resolved namespace + how many memories matched

use crate::cli::CliOutput;
use crate::cli::helpers::{auto_namespace, human_age, id_short};
use crate::config::AppConfig;
use crate::{db, models, toon};
use anyhow::Result;
use clap::Args;
use models::Tier;
use std::path::Path;
use std::time::Instant;

/// Default budget — large enough for ~10 toon-compact rows, small enough that
/// a misconfigured hook can't wedge the first turn with megabytes of context.
const DEFAULT_BUDGET_TOKENS: usize = 4096;

/// Approximate tokens-per-character for cl100k_base / English text. Used for
/// the cheap budget clamp. Real tokenization happens elsewhere (recall_hybrid);
/// boot's budget is advisory and only needs to be in the right order of
/// magnitude to bound output cost.
const TOKENS_PER_CHAR: f32 = 0.25;

/// Sentinel used in manifest fields that couldn't be resolved on this
/// invocation — most often because the DB itself is unreachable, so
/// schema/total/etc. simply don't have an answer.
const UNAVAILABLE: &str = "<unavailable>";

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

impl BootStatus {
    fn label(self) -> &'static str {
        match self {
            Self::OkLoaded => "ok",
            Self::InfoFallback | Self::InfoEmpty => "info",
            Self::WarnDbUnavailable => "warn",
        }
    }
}

/// Read the schema version from the DB's `schema_version` table. Returns
/// the sentinel string when the DB is unreachable or the table is empty —
/// the manifest is best-effort, never an error.
fn read_schema_version(conn: &rusqlite::Connection) -> String {
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |r| r.get::<_, i64>(0),
    )
    .map_or_else(|_| UNAVAILABLE.to_string(), |v| format!("v{v}"))
}

/// Cheap COUNT of live (non-expired) memories. Same expiry semantics as
/// the recall pipeline: NULL `expires_at` is permanent, otherwise must be
/// in the future. Errors degrade to the sentinel rather than fail boot.
fn count_live_memories(conn: &rusqlite::Connection) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE expires_at IS NULL OR expires_at > ?1",
        rusqlite::params![now],
        |r| r.get::<_, i64>(0),
    )
    .map_or_else(|_| UNAVAILABLE.to_string(), |v| v.to_string())
}

/// Diagnostic manifest assembled before each header emit. Every field is
/// a string so the same struct can carry both real values and the
/// `<unavailable>` sentinel without branching downstream.
///
/// Field semantics:
/// - `version`         — `env!("CARGO_PKG_VERSION")` at compile time
/// - `db_path`         — resolved path the boot ran against
/// - `schema_version`  — `vN` from the DB's `schema_version` table
/// - `total_memories`  — count of live (non-expired) rows
/// - `tier`            — active feature tier name
/// - `embedder`        — *configured* embedder (boot does NOT load it)
/// - `reranker`        — *configured* cross-encoder (or "none")
/// - `llm`             — *configured* LLM model id (or "none")
/// - `latency_ms`      — wall-clock from `run()` entry to emit
/// - `namespace`       — the namespace the body actually came from
/// - `count`           — number of memories included in the body
struct BootManifest {
    version: String,
    db_path: String,
    schema_version: String,
    total_memories: String,
    tier: String,
    embedder: String,
    reranker: String,
    llm: String,
    latency_ms: u128,
    namespace: String,
    count: usize,
    // The status note, used by JSON `note` and (for InfoEmpty / Warn) the
    // multi-line text/toon header expansion.
    note: String,
    status: BootStatus,
}

impl BootManifest {
    fn build(
        status: BootStatus,
        namespace: &str,
        count: usize,
        db_path: &Path,
        app_config: &AppConfig,
        schema_version: String,
        total_memories: String,
        latency_ms: u128,
    ) -> Self {
        // Resolve the *configured* tier. Boot doesn't materialize the
        // embedder / LLM / reranker handles, so these reflect what would
        // load — not what actually loaded.
        let feature_tier = app_config.effective_tier(None);
        let tier_config = feature_tier.config();
        let embedder = tier_config
            .embedding_model
            .map_or_else(|| "none".to_string(), |m| m.hf_model_id().to_string());
        let llm = tier_config
            .llm_model
            .map_or_else(|| "none".to_string(), |m| m.ollama_model_id().to_string());
        let reranker = if tier_config.cross_encoder {
            "ms-marco-MiniLM-L-6-v2".to_string()
        } else {
            "none".to_string()
        };

        let note = match status {
            BootStatus::OkLoaded => format!(
                "loaded {count} memor{plural} from ns={namespace}",
                plural = if count == 1 { "y" } else { "ies" }
            ),
            BootStatus::InfoFallback => format!(
                "namespace empty; loaded {count} memor{plural} from global Long tier fallback",
                plural = if count == 1 { "y" } else { "ies" }
            ),
            BootStatus::InfoEmpty => format!(
                "namespace '{namespace}' is empty and no global Long-tier fallback found — \
                 nothing to load (this is normal on a fresh install)"
            ),
            BootStatus::WarnDbUnavailable => format!(
                "db unavailable at {} — proceeding without memory context. \
                 Run `ai-memory doctor` to diagnose. \
                 See https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/integrations/README.md",
                db_path.display()
            ),
        };

        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            db_path: db_path.display().to_string(),
            schema_version,
            total_memories,
            tier: feature_tier.as_str().to_string(),
            embedder,
            reranker,
            llm,
            latency_ms,
            namespace: namespace.to_string(),
            count,
            note,
            status,
        }
    }
}

/// `ai-memory boot` entry point.
#[allow(clippy::too_many_lines)]
pub fn run(
    db_path: &Path,
    args: &BootArgs,
    app_config: &AppConfig,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let start = Instant::now();
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
                let manifest = BootManifest::build(
                    BootStatus::WarnDbUnavailable,
                    &namespace,
                    0,
                    db_path,
                    app_config,
                    UNAVAILABLE.to_string(),
                    UNAVAILABLE.to_string(),
                    start.elapsed().as_millis(),
                );
                emit_status_header(out, &manifest, format)?;
            }
            return Ok(());
        }
    };

    // Cheap diagnostic lookups. Both degrade to the sentinel rather than
    // fail the boot — the manifest is best-effort.
    let schema_version = read_schema_version(&conn);
    let total_memories = count_live_memories(&conn);

    let (mems, used_namespace) = fetch_boot_memories(&conn, &namespace, limit)?;
    let mems = clamp_to_budget(mems, args.budget_tokens);
    let fell_back = !mems.is_empty() && used_namespace.is_empty();

    if mems.is_empty() {
        if !args.no_header {
            let manifest = BootManifest::build(
                BootStatus::InfoEmpty,
                &namespace,
                0,
                db_path,
                app_config,
                schema_version,
                total_memories,
                start.elapsed().as_millis(),
            );
            emit_status_header(out, &manifest, format)?;
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
                let manifest = BootManifest::build(
                    status,
                    displayed_ns,
                    mems.len(),
                    db_path,
                    app_config,
                    schema_version,
                    total_memories,
                    start.elapsed().as_millis(),
                );
                emit_json_with_status(out, &manifest, &mems, fell_back)?;
            }
        }
        BootFormat::Text => {
            if !args.no_header {
                let manifest = BootManifest::build(
                    status,
                    displayed_ns,
                    mems.len(),
                    db_path,
                    app_config,
                    schema_version,
                    total_memories,
                    start.elapsed().as_millis(),
                );
                emit_status_header(out, &manifest, format)?;
            }
            emit_text(out, &mems)?;
        }
        BootFormat::Toon => {
            if !args.no_header {
                let manifest = BootManifest::build(
                    status,
                    displayed_ns,
                    mems.len(),
                    db_path,
                    app_config,
                    schema_version,
                    total_memories,
                    start.elapsed().as_millis(),
                );
                emit_status_header(out, &manifest, format)?;
            }
            emit_toon(out, &mems)?;
        }
    }

    Ok(())
}

/// Always-visible diagnostic header. Agents see this in their session log
/// even when the body is empty, so the absence of memory context is a
/// surfaced signal rather than a silent failure.
///
/// **Format (text/toon)** — multi-line manifest, every field labelled:
/// ```text
/// # ai-memory boot: ok
/// #   version:    0.6.3+patch.1
/// #   db:         /home/u/.claude/ai-memory.db (schema=v19, 161 memories)
/// #   tier:       autonomous (embedder=..., reranker=..., llm=...)
/// #   latency:    12ms
/// #   namespace:  ns-x (loaded 3 memories)
/// ```
///
/// **Format (json)** — single JSON object with every manifest field as a
/// top-level key (`version`, `db_path`, `schema_version`, `total_memories`,
/// `tier`, `embedder`, `reranker`, `llm`, `latency_ms`, `namespace`,
/// `count`, `status`, `note`).
fn emit_status_header(
    out: &mut CliOutput<'_>,
    manifest: &BootManifest,
    format: BootFormat,
) -> Result<()> {
    match format {
        BootFormat::Json => {
            writeln!(
                out.stdout,
                "{}",
                serde_json::json!({
                    "status": manifest.status.label(),
                    "version": manifest.version,
                    "db_path": manifest.db_path,
                    "schema_version": manifest.schema_version,
                    "total_memories": manifest.total_memories,
                    "tier": manifest.tier,
                    "embedder": manifest.embedder,
                    "reranker": manifest.reranker,
                    "llm": manifest.llm,
                    "latency_ms": manifest.latency_ms,
                    "namespace": manifest.namespace,
                    "count": manifest.count,
                    "note": manifest.note,
                })
            )?;
        }
        _ => {
            // Multi-line transparent manifest. Each field is on its own
            // line so a grep / log scrape can pick them out individually,
            // and the human reader sees a top-down summary.
            writeln!(out.stdout, "# ai-memory boot: {}", manifest.status.label())?;
            writeln!(out.stdout, "#   version:    {}", manifest.version)?;
            writeln!(
                out.stdout,
                "#   db:         {} (schema={}, {} memories)",
                manifest.db_path, manifest.schema_version, manifest.total_memories
            )?;
            writeln!(
                out.stdout,
                "#   tier:       {} (embedder={}, reranker={}, llm={})",
                manifest.tier, manifest.embedder, manifest.reranker, manifest.llm
            )?;
            writeln!(out.stdout, "#   latency:    {}ms", manifest.latency_ms)?;
            // Namespace line carries the same status-specific note the
            // single-line PR-1 header used to carry — so a reader can see
            // *why* a particular count showed up.
            match manifest.status {
                BootStatus::OkLoaded => {
                    writeln!(
                        out.stdout,
                        "#   namespace:  {} (loaded {} memor{})",
                        manifest.namespace,
                        manifest.count,
                        if manifest.count == 1 { "y" } else { "ies" }
                    )?;
                }
                BootStatus::InfoFallback => {
                    writeln!(
                        out.stdout,
                        "#   namespace:  {} (fallback: loaded {} memor{} from global Long tier)",
                        manifest.namespace,
                        manifest.count,
                        if manifest.count == 1 { "y" } else { "ies" }
                    )?;
                }
                BootStatus::InfoEmpty => {
                    writeln!(
                        out.stdout,
                        "#   namespace:  {} (empty — nothing to load; this is normal on a fresh install)",
                        manifest.namespace
                    )?;
                }
                BootStatus::WarnDbUnavailable => {
                    writeln!(
                        out.stdout,
                        "#   namespace:  {} (db unavailable — see `ai-memory doctor`)",
                        manifest.namespace
                    )?;
                }
            }
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
    manifest: &BootManifest,
    mems: &[models::Memory],
    fell_back: bool,
) -> Result<()> {
    // Same shape as `emit_status_header` JSON path, plus `memories` and
    // `fell_back_to_global`. Agents that ingest JSON get every manifest
    // field as a top-level key so they can reason about the runtime
    // without parsing a free-text header.
    let body = serde_json::json!({
        "status": manifest.status.label(),
        "version": manifest.version,
        "db_path": manifest.db_path,
        "schema_version": manifest.schema_version,
        "total_memories": manifest.total_memories,
        "tier": manifest.tier,
        "embedder": manifest.embedder,
        "reranker": manifest.reranker,
        "llm": manifest.llm,
        "latency_ms": manifest.latency_ms,
        "namespace": manifest.namespace,
        "count": manifest.count,
        "note": manifest.note,
        "fell_back_to_global": fell_back,
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

    fn default_config() -> AppConfig {
        AppConfig::default()
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
        let cfg = default_config();
        let mut args = default_args();
        args.namespace = Some("ns-x".to_string());
        let mut out = env.output();
        run(&db_path, &args, &cfg, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        // Status line + every manifest field appears.
        assert!(
            stdout.contains("# ai-memory boot: ok"),
            "expected ok status header, got: {stdout}"
        );
        assert!(
            stdout.contains("#   version:"),
            "manifest missing version line: {stdout}"
        );
        assert!(
            stdout.contains("#   db:"),
            "manifest missing db line: {stdout}"
        );
        assert!(
            stdout.contains("#   tier:"),
            "manifest missing tier line: {stdout}"
        );
        assert!(
            stdout.contains("#   latency:"),
            "manifest missing latency line: {stdout}"
        );
        assert!(
            stdout.contains("#   namespace:") && stdout.contains("ns-x"),
            "namespace line should contain ns-x: {stdout}"
        );
        assert!(stdout.contains("loaded 2 memories"));
        assert!(stdout.contains("first"));
        assert!(stdout.contains("second"));
        assert!(!stdout.contains("elsewhere"));
    }

    #[test]
    fn boot_header_includes_version() {
        let mut env = TestEnv::fresh();
        seed_memory(&env.db_path, "ns-v", "row", "x");
        let db_path = env.db_path.clone();
        let cfg = default_config();
        let mut args = default_args();
        args.namespace = Some("ns-v".to_string());
        let mut out = env.output();
        run(&db_path, &args, &cfg, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        // CARGO_PKG_VERSION is "0.6.3+patch.1" or similar; assert the
        // crate constant surfaces verbatim, not a hardcoded string.
        let version = env!("CARGO_PKG_VERSION");
        assert!(
            stdout.contains(version),
            "expected version `{version}` in header: {stdout}"
        );
    }

    #[test]
    fn boot_header_includes_db_path() {
        let mut env = TestEnv::fresh();
        seed_memory(&env.db_path, "ns-d", "row", "x");
        let db_path = env.db_path.clone();
        let cfg = default_config();
        let mut args = default_args();
        args.namespace = Some("ns-d".to_string());
        let mut out = env.output();
        run(&db_path, &args, &cfg, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        let db_str = db_path.display().to_string();
        assert!(
            stdout.contains(&db_str),
            "expected db path `{db_str}` in header: {stdout}"
        );
    }

    #[test]
    fn boot_header_includes_schema_version() {
        let mut env = TestEnv::fresh();
        seed_memory(&env.db_path, "ns-s", "row", "x");
        let db_path = env.db_path.clone();
        let cfg = default_config();
        let mut args = default_args();
        args.namespace = Some("ns-s".to_string());
        let mut out = env.output();
        run(&db_path, &args, &cfg, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(
            stdout.contains("schema=v"),
            "expected `schema=vN` in header: {stdout}"
        );
    }

    #[test]
    fn boot_header_includes_latency_ms() {
        let mut env = TestEnv::fresh();
        seed_memory(&env.db_path, "ns-lat", "row", "x");
        let db_path = env.db_path.clone();
        let cfg = default_config();
        let mut args = default_args();
        args.namespace = Some("ns-lat".to_string());
        let mut out = env.output();
        run(&db_path, &args, &cfg, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        // Latency line must exist; the value must end with `ms` and be
        // numeric (we don't pin a value because wall-clock varies).
        let latency_line = stdout
            .lines()
            .find(|l| l.contains("latency:"))
            .expect("latency line must exist in manifest");
        let suffix = latency_line.split("latency:").nth(1).unwrap().trim();
        assert!(
            suffix.ends_with("ms"),
            "latency value should end with `ms`: {suffix}"
        );
        let num_str = suffix.trim_end_matches("ms");
        assert!(
            num_str.parse::<u128>().is_ok(),
            "latency must parse as integer ms: {num_str}"
        );
    }

    #[test]
    fn boot_json_includes_all_manifest_fields() {
        let mut env = TestEnv::fresh();
        seed_memory(&env.db_path, "ns-jm", "row", "x");
        let db_path = env.db_path.clone();
        let cfg = default_config();
        let mut args = default_args();
        args.namespace = Some("ns-jm".to_string());
        args.format = "json".to_string();
        let mut out = env.output();
        run(&db_path, &args, &cfg, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
        // Status fields (PR-1 contract preserved).
        assert_eq!(parsed["status"], "ok");
        assert_eq!(parsed["namespace"], "ns-jm");
        assert_eq!(parsed["count"], 1);
        assert_eq!(parsed["fell_back_to_global"], false);
        assert!(parsed["memories"].is_array());
        // PR-4 manifest fields.
        for key in [
            "version",
            "db_path",
            "schema_version",
            "total_memories",
            "tier",
            "embedder",
            "reranker",
            "llm",
            "latency_ms",
            "note",
        ] {
            assert!(
                parsed.get(key).is_some(),
                "json output missing manifest field `{key}`: {stdout}"
            );
        }
        assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
        assert!(parsed["latency_ms"].is_number());
        assert!(
            parsed["schema_version"]
                .as_str()
                .unwrap_or("")
                .starts_with('v'),
            "schema_version should be `vN` form"
        );
    }

    #[test]
    fn boot_respects_limit() {
        let mut env = TestEnv::fresh();
        for i in 0..5 {
            seed_memory(&env.db_path, "ns-l", &format!("m{i}"), "x");
        }
        let db_path = env.db_path.clone();
        let cfg = default_config();
        let mut args = default_args();
        args.namespace = Some("ns-l".to_string());
        args.limit = 2;
        let mut out = env.output();
        run(&db_path, &args, &cfg, &mut out).unwrap();
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
        let cfg = default_config();
        let mut args = default_args();
        args.namespace = Some("ns-h".to_string());
        args.no_header = true;
        let mut out = env.output();
        run(&db_path, &args, &cfg, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(!stdout.contains("# ai-memory boot"));
        assert!(stdout.contains("row-one"));
    }

    #[test]
    fn boot_json_format_emits_status_and_memories() {
        let mut env = TestEnv::fresh();
        seed_memory(&env.db_path, "ns-j", "row", "x");
        let db_path = env.db_path.clone();
        let cfg = default_config();
        let mut args = default_args();
        args.namespace = Some("ns-j".to_string());
        args.format = "json".to_string();
        let mut out = env.output();
        run(&db_path, &args, &cfg, &mut out).unwrap();
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
        // PR-4: the warn variant still emits the manifest fields, with
        // `<unavailable>` in slots that need a live DB to fill.
        let mut env = TestEnv::fresh();
        let bad_path = env
            .db_path
            .parent()
            .unwrap()
            .join("subdir/that/does/not/exist/db.sqlite");
        let cfg = default_config();
        let mut args = default_args();
        args.quiet = true;
        let mut out = env.output();
        run(&bad_path, &args, &cfg, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        assert!(
            stdout.contains("# ai-memory boot: warn"),
            "warn header should always appear under --quiet: {stdout}"
        );
        assert!(
            stdout.contains("db unavailable"),
            "header should explain the warning cause: {stdout}"
        );
        // What the warn variant CAN surface even without the DB.
        assert!(
            stdout.contains("#   version:"),
            "warn manifest should still carry version: {stdout}"
        );
        assert!(
            stdout.contains(env!("CARGO_PKG_VERSION")),
            "warn manifest version should be CARGO_PKG_VERSION: {stdout}"
        );
        assert!(
            stdout.contains("#   tier:"),
            "warn manifest should still carry tier: {stdout}"
        );
        assert!(
            stdout.contains("#   latency:"),
            "warn manifest should still carry latency: {stdout}"
        );
        // Slots that need a live DB degrade to the sentinel.
        assert!(
            stdout.contains(UNAVAILABLE),
            "warn manifest should mark unreachable fields as <unavailable>: {stdout}"
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
        let cfg = default_config();
        let args = default_args();
        // quiet = false (default) — error goes to stderr too.
        let mut out = env.output();
        run(&bad_path, &args, &cfg, &mut out).unwrap();
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
        let cfg = default_config();
        let mut args = default_args();
        args.quiet = true;
        args.no_header = true;
        let mut out = env.output();
        run(&bad_path, &args, &cfg, &mut out).unwrap();
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
        let cfg = default_config();
        let mut args = default_args();
        args.namespace = Some("nonexistent-ns".to_string());
        let mut out = env.output();
        run(&db_path, &args, &cfg, &mut out).unwrap();
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
        let cfg = default_config();
        let mut args = default_args();
        args.namespace = Some("nothing-here".to_string());
        let mut out = env.output();
        run(&db_path, &args, &cfg, &mut out).unwrap();
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
        let cfg = default_config();
        let mut args = default_args();
        args.namespace = Some("ns-budget".to_string());
        args.limit = 50;
        args.budget_tokens = 100;
        let mut out = env.output();
        run(&db_path, &args, &cfg, &mut out).unwrap();
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
        let cfg = default_config();
        let mut args = default_args();
        args.format = "json".to_string();
        args.quiet = true;
        let mut out = env.output();
        run(&bad_path, &args, &cfg, &mut out).unwrap();
        let stdout = std::str::from_utf8(&env.stdout).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
        assert_eq!(parsed["status"], "warn");
        assert_eq!(parsed["count"], 0);
        assert!(parsed["note"].as_str().unwrap().contains("db unavailable"));
        // PR-4: warn JSON variant carries the manifest with sentinels in
        // slots that need a live DB.
        assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(parsed["schema_version"], UNAVAILABLE);
        assert_eq!(parsed["total_memories"], UNAVAILABLE);
    }
}
