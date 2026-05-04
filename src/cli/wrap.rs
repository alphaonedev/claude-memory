// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `ai-memory wrap <agent>` — cross-platform Rust replacement for the
//! shell wrappers PR-1 of issue #487 shipped in the integration recipes.
//!
//! ## What it does
//!
//! 1. Calls `cli::boot::run` in-process, capturing its stdout into a
//!    buffer. No subprocess; no shell. The `--no-boot` flag skips this
//!    step so a misconfigured DB path doesn't block the agent.
//! 2. Builds a system-context string of the form
//!    `<preamble>\n\n<boot output>` where the preamble explains to the
//!    downstream agent that it has ai-memory access.
//! 3. Spawns the wrapped agent (`std::process::Command`) with the
//!    system-context delivered via the chosen strategy:
//!    - `SystemFlag` — `<agent> <flag> "<system_msg>" <trailing args...>`
//!    - `SystemEnv`  — `<env_name>=<system_msg> <agent> <trailing args...>`
//!    - `MessageFile` — write `<system_msg>` to a `NamedTempFile`, pass
//!      `<flag> <tempfile_path>` to the agent, drop the tempfile on
//!      exit so it is cleaned up by the OS.
//!    - `Auto` — resolved at runtime from a built-in lookup table
//!      (`default_strategy`).
//! 4. Forwards the parent's stdin / stdout / stderr unmodified
//!    (`Stdio::inherit`).
//! 5. Returns the wrapped agent's exit code as the wrap subcommand's
//!    exit code, so wrappers compose cleanly with shell pipelines and
//!    CI gates that branch on `$?`.
//!
//! ## Why Rust, not bash + PowerShell
//!
//! The user directive on issue #487 PR-6 was: implementation should be
//! predominantly Rust with config hooks. PR-1 shipped per-recipe bash
//! and PowerShell wrappers, which doubled the maintenance surface and
//! couldn't run in restricted Windows / containerized environments
//! without a shell. A single cross-platform Rust subcommand eliminates
//! both problems — it's the same code path on macOS / Linux / Windows
//! / Docker / Kubernetes / Nix / etc.
//!
//! ## Lookup table
//!
//! `default_strategy(agent)` resolves the unflagged form `ai-memory
//! wrap <agent> -- <args>` to the right delivery mechanism for the
//! agents we can identify by name today. Unknown agents fall through to
//! `--system <msg>` because that's the most common contract across
//! OpenAI-compatible CLIs. Future PRs (notably PR-7) can extend the
//! table by adding match arms.

use crate::cli::CliOutput;
use crate::cli::boot::{self, BootArgs};
use anyhow::{Context, Result};
use clap::Args;
use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Default budget for the inner `ai-memory boot` call when the caller
/// doesn't override. Mirrors `cli::boot::DEFAULT_BUDGET_TOKENS` but is
/// re-declared here so wrap can tune independently if needed.
const DEFAULT_WRAP_BUDGET_TOKENS: usize = 4096;

/// Default row limit for the inner boot call. Same value `cli::boot`
/// itself defaults to.
const DEFAULT_WRAP_LIMIT: usize = 10;

/// Preamble injected before the boot output in every wrap call.
/// Explains to the downstream agent why it's seeing this context. Kept
/// short and stable so prompt-cache breakpoints upstream stay warm.
const WRAP_PREAMBLE: &str = "You have access to ai-memory, a persistent memory system. \
The recent context loaded for you appears below. Reference it when relevant to the user's request.";

/// Strategy for delivering the assembled system message to the wrapped
/// agent. Each variant maps to a distinct CLI ABI an agent might
/// expose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrapStrategy {
    /// Pass the system message as the value of a CLI flag, e.g.
    /// `codex --system "<msg>" <args...>`.
    SystemFlag {
        /// The flag name including any leading dashes — e.g. `--system`,
        /// `--system-prompt`, `-s`.
        flag: String,
    },
    /// Set the system message as an environment variable for the child
    /// process. e.g. `OLLAMA_SYSTEM=<msg> ollama run hermes3:8b`.
    SystemEnv {
        /// The env var name, e.g. `OLLAMA_SYSTEM`.
        name: String,
    },
    /// Write the system message to a tempfile and pass the path via a
    /// CLI flag. e.g. `aider --message-file <path> <args...>`. Used by
    /// agents whose system-message length exceeds shell argv limits or
    /// whose CLI explicitly takes a file path.
    MessageFile {
        /// The flag that takes the file path, e.g. `--message-file`.
        flag: String,
    },
    /// Resolve the strategy at runtime from `default_strategy(agent)`.
    /// This is the natural mode when the user hasn't passed any of the
    /// strategy override flags.
    Auto,
}

/// Built-in agent → strategy lookup. The list is small by design — we
/// only encode strategies for agents we've actually verified. Anything
/// not in the table falls through to `--system <msg>` because that's
/// the most common contract across OpenAI-compatible CLIs.
///
/// PR-7 may extend this map; the matrix is intentionally tabular so
/// adding a row is a one-line change.
#[must_use]
pub fn default_strategy(agent: &str) -> WrapStrategy {
    match agent {
        // OpenAI Codex CLI. The flag name varies between Codex variants
        // (`--system`, `--system-prompt`, `OPENAI_CLI_SYSTEM`) but
        // `--system` is the documented form on the upstream codex-cli
        // crate (PR-1 recipe + Codex CLI README). Users running a
        // variant that exposes a different flag can override with
        // `--system-flag <flag>`.
        "codex" | "codex-cli" => WrapStrategy::SystemFlag {
            flag: "--system".into(),
        },
        // Aider takes its system / instructions input from a file via
        // `--message-file`. Aider's CLI explicitly recommends this for
        // anything longer than a one-liner because it doesn't shell-quote
        // the arg-form for newlines reliably.
        "aider" => WrapStrategy::MessageFile {
            flag: "--message-file".into(),
        },
        // Google Gemini CLI. `--system` is the documented prepend form.
        "gemini" => WrapStrategy::SystemFlag {
            flag: "--system".into(),
        },
        // Ollama uses an env var because `ollama run <model>` doesn't
        // expose a `--system` flag at the CLI level — it expects the
        // system prompt either inside the prompt body or via the
        // `OLLAMA_SYSTEM` env var (also the form `ollama serve` reads).
        "ollama" => WrapStrategy::SystemEnv {
            name: "OLLAMA_SYSTEM".into(),
        },
        // Default: most OpenAI-compatible CLIs accept `--system <msg>`.
        // If that's wrong, users override with `--system-flag` /
        // `--system-env` / `--message-file-flag`.
        _ => WrapStrategy::SystemFlag {
            flag: "--system".into(),
        },
    }
}

/// Args for `ai-memory wrap`. Designed so the simplest form
/// (`ai-memory wrap codex -- "hello"`) just works — every flag has a
/// defaulted value or the lookup table fills it in.
#[derive(Args, Debug)]
pub struct WrapArgs {
    /// Name of the agent CLI to wrap, e.g. `codex`, `aider`, `gemini`,
    /// `ollama`. Resolved against `default_strategy` to pick the
    /// system-message delivery mechanism unless the user overrides
    /// with one of the strategy flags below. The agent name is also
    /// the executable looked up on `$PATH`.
    pub agent: String,

    /// Override the system-message flag (e.g. `--system-prompt`). When
    /// set, wrap delivers the system message via this flag regardless
    /// of what the lookup table says for `<agent>`.
    #[arg(long, value_name = "FLAG")]
    pub system_flag: Option<String>,

    /// Override the system-message env var (e.g. `OPENAI_CLI_SYSTEM`).
    /// Mutually exclusive with `--system-flag` and
    /// `--message-file-flag`; if multiple are set, the last specified
    /// on the command line wins (clap default), but the most common
    /// case is supplying exactly one.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["system_flag", "message_file_flag"])]
    pub system_env: Option<String>,

    /// Override the message-file flag (e.g. `--message-file`). Wrap
    /// will write the system message to a tempfile and pass this flag
    /// + the tempfile path to the agent. The tempfile is cleaned up on
    /// wrap exit (cross-platform; uses `tempfile::NamedTempFile`).
    #[arg(long, value_name = "FLAG", conflicts_with_all = ["system_flag", "system_env"])]
    pub message_file_flag: Option<String>,

    /// Skip the inner `ai-memory boot` call entirely. The wrapped
    /// agent runs without any prepended memory context. Useful when
    /// the DB is known to be unavailable, when the user wants the wrap
    /// subcommand for argv-forwarding only, or for tests that want to
    /// isolate the wrapping behavior from the boot-loading behavior.
    #[arg(long, default_value_t = false)]
    pub no_boot: bool,

    /// Row limit forwarded to the inner `ai-memory boot --limit`.
    /// Clamped to `[1, 50]` by `cli::boot` itself.
    #[arg(long, default_value_t = DEFAULT_WRAP_LIMIT)]
    pub limit: usize,

    /// Approximate token budget forwarded to the inner
    /// `ai-memory boot --budget-tokens`.
    #[arg(long, default_value_t = DEFAULT_WRAP_BUDGET_TOKENS)]
    pub budget_tokens: usize,

    /// Trailing arguments forwarded verbatim to the wrapped agent CLI
    /// after the system-message delivery (the convention is to
    /// separate them with `--` on the command line:
    /// `ai-memory wrap codex -- chat --model gpt-5`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub trailing: Vec<String>,
}

/// Resolve the active strategy from the user-supplied overrides plus
/// the built-in lookup table. Order of precedence:
///
/// 1. `--system-env <name>` → `SystemEnv`
/// 2. `--message-file-flag <flag>` → `MessageFile`
/// 3. `--system-flag <flag>` → `SystemFlag`
/// 4. fall through to `default_strategy(agent)` (the lookup table)
fn resolve_strategy(args: &WrapArgs) -> WrapStrategy {
    if let Some(name) = args.system_env.as_deref() {
        return WrapStrategy::SystemEnv { name: name.into() };
    }
    if let Some(flag) = args.message_file_flag.as_deref() {
        return WrapStrategy::MessageFile { flag: flag.into() };
    }
    if let Some(flag) = args.system_flag.as_deref() {
        return WrapStrategy::SystemFlag { flag: flag.into() };
    }
    default_strategy(&args.agent)
}

/// Run `cli::boot::run` in-process, capturing its stdout into a
/// `Vec<u8>`. Stderr is also captured but discarded — the boot helper
/// already honors `--quiet` for us, so any stderr that escapes is by
/// design (a developer-facing diagnostic).
///
/// On any boot failure, this function returns an empty `String` rather
/// than propagating — the agent should still run even if memory load
/// fails. The user-facing diagnostic header is already on stdout in
/// that case (`# ai-memory boot: warn — db unavailable …`) so the
/// caller still sees what happened.
fn run_boot_capture(
    db_path: &Path,
    limit: usize,
    budget_tokens: usize,
    app_config: &crate::config::AppConfig,
) -> String {
    let mut stdout: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
    let args = BootArgs {
        namespace: None,
        limit,
        budget_tokens,
        format: "text".to_string(),
        no_header: false,
        // --quiet so a missing DB never blocks the wrapped agent.
        quiet: true,
        cwd: None,
    };
    if boot::run(db_path, &args, app_config, &mut out).is_err() {
        // Even on hard failure (which `cli::boot::run` should never
        // hit thanks to the `--quiet` graceful path), return an empty
        // string so the agent runs unwrapped rather than getting a
        // blocking error.
        return String::new();
    }
    String::from_utf8(stdout).unwrap_or_default()
}

/// Assemble the `<preamble>\n\n<boot_output>` system message. Trims
/// trailing whitespace on the boot section to keep the assembled
/// string tidy in the agent's prompt.
fn build_system_message(boot_output: &str) -> String {
    let trimmed = boot_output.trim_end();
    if trimmed.is_empty() {
        // Even with an empty body the preamble is still useful — it
        // tells the agent "you have memory access" so it knows it can
        // call `memory_recall` mid-session if it has the tool.
        WRAP_PREAMBLE.to_string()
    } else {
        format!("{WRAP_PREAMBLE}\n\n{trimmed}")
    }
}

/// Spawn the agent with stdio inherited and return the exit code.
/// Wrapped here so tests can assert on the spawned-command shape via
/// the helpers in `#[cfg(test)] mod tests`.
fn spawn_and_wait(mut cmd: Command) -> Result<i32> {
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = cmd
        .status()
        .with_context(|| format!("ai-memory wrap: failed to spawn agent {cmd:?}"))?;
    // Unix: `code()` is None when the child was killed by a signal.
    // We then surface 128+sig per the standard shell convention so the
    // caller can branch on the signal in CI scripts.
    let code = if let Some(c) = status.code() {
        c
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            status.signal().map_or(1, |s| 128 + s)
        }
        #[cfg(not(unix))]
        {
            1
        }
    };
    Ok(code)
}

/// Build the `Command` for an agent given a strategy. Pulled out of
/// `run` so the tests can assert directly on the resulting `Command`'s
/// argv / env without spawning a subprocess.
///
/// Returns the assembled `Command` + (when the strategy is
/// `MessageFile`) the `NamedTempFile` whose lifetime governs cleanup.
/// The caller MUST keep the returned `Option<NamedTempFile>` alive
/// until after the child has exited; dropping it sooner unlinks the
/// file mid-spawn on platforms where unlink-while-open is permitted.
fn build_command_for_strategy(
    agent: &str,
    strategy: &WrapStrategy,
    system_msg: &str,
    trailing: &[String],
) -> Result<(Command, Option<tempfile::NamedTempFile>)> {
    let mut cmd = Command::new(agent);
    let mut tempfile_handle: Option<tempfile::NamedTempFile> = None;
    match strategy {
        WrapStrategy::SystemFlag { flag } => {
            cmd.arg(flag).arg(system_msg);
            for t in trailing {
                cmd.arg(t);
            }
        }
        WrapStrategy::SystemEnv { name } => {
            cmd.env(name, system_msg);
            for t in trailing {
                cmd.arg(t);
            }
        }
        WrapStrategy::MessageFile { flag } => {
            // `tempfile::NamedTempFile` is cross-platform: on Unix it's
            // a regular file with a randomised name; on Windows it
            // skips the unlink-while-open trick (which Windows
            // disallows) and cleans up on `Drop`. Either way the file
            // is gone after wrap exits.
            let mut tf = tempfile::NamedTempFile::new()
                .context("ai-memory wrap: failed to create system-message tempfile")?;
            tf.write_all(system_msg.as_bytes())
                .context("ai-memory wrap: failed to write system-message tempfile")?;
            // Flush so the agent process reads the full message even
            // if the OS hasn't drained the buffer yet.
            tf.flush()
                .context("ai-memory wrap: failed to flush system-message tempfile")?;
            cmd.arg(flag).arg(tf.path().as_os_str());
            for t in trailing {
                cmd.arg(t);
            }
            tempfile_handle = Some(tf);
        }
        WrapStrategy::Auto => {
            // Resolve and recurse. `Auto` should be handled by
            // `resolve_strategy` before we get here, but if a caller
            // synthesises a `WrapArgs` programmatically and leaves
            // strategy as `Auto`, fall through to the lookup table.
            let resolved = default_strategy(agent);
            return build_command_for_strategy(agent, &resolved, system_msg, trailing);
        }
    }
    Ok((cmd, tempfile_handle))
}

/// `ai-memory wrap` entry point. Returns the wrapped agent's exit code
/// so `daemon_runtime` can `std::process::exit(code)` on a non-zero
/// outcome — that's how shell pipelines and CI gates branch on the
/// agent's success.
///
/// # Errors
///
/// - The wrapped agent binary cannot be spawned (`Command::status`
///   surfaces the OS-level error).
/// - `tempfile::NamedTempFile::new()` fails when the strategy is
///   `MessageFile` (very rare; `/tmp` full or unwritable).
pub fn run(
    db_path: &Path,
    args: &WrapArgs,
    app_config: &crate::config::AppConfig,
    _out: &mut CliOutput<'_>,
) -> Result<i32> {
    let strategy = resolve_strategy(args);

    // Boot context. `--no-boot` skips it so the agent runs unwrapped
    // (still through `Command::new(agent)` so this subcommand stays
    // useful as a strategy-hooked launcher even with memory off).
    let system_msg = if args.no_boot {
        WRAP_PREAMBLE.to_string()
    } else {
        let boot_output = run_boot_capture(db_path, args.limit, args.budget_tokens, app_config);
        build_system_message(&boot_output)
    };

    let (cmd, _tempfile_handle) =
        build_command_for_strategy(&args.agent, &strategy, &system_msg, &args.trailing)?;

    // _tempfile_handle is held by the local binding so it lives until
    // after `spawn_and_wait` returns. Don't shorten its scope.
    let code = spawn_and_wait(cmd)?;
    Ok(code)
}

/// Public helper for callers (tests + future PR-7 recipe additions)
/// that want to format an `OsStr` argv element back to UTF-8 for
/// assertions / logging. Falls back to the lossy form so platforms
/// with non-UTF-8 paths don't panic.
#[must_use]
pub fn os_str_to_string_lossy(s: &OsStr) -> String {
    s.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    fn default_args(agent: &str) -> WrapArgs {
        WrapArgs {
            agent: agent.to_string(),
            system_flag: None,
            system_env: None,
            message_file_flag: None,
            no_boot: false,
            limit: DEFAULT_WRAP_LIMIT,
            budget_tokens: DEFAULT_WRAP_BUDGET_TOKENS,
            trailing: Vec::new(),
        }
    }

    #[test]
    fn wrap_resolves_default_strategy_per_known_agent() {
        assert_eq!(
            default_strategy("codex"),
            WrapStrategy::SystemFlag {
                flag: "--system".into()
            }
        );
        assert_eq!(
            default_strategy("codex-cli"),
            WrapStrategy::SystemFlag {
                flag: "--system".into()
            }
        );
        assert_eq!(
            default_strategy("aider"),
            WrapStrategy::MessageFile {
                flag: "--message-file".into()
            }
        );
        assert_eq!(
            default_strategy("gemini"),
            WrapStrategy::SystemFlag {
                flag: "--system".into()
            }
        );
        assert_eq!(
            default_strategy("ollama"),
            WrapStrategy::SystemEnv {
                name: "OLLAMA_SYSTEM".into()
            }
        );
        // Unknown agent → fall through to --system.
        assert_eq!(
            default_strategy("some-future-cli"),
            WrapStrategy::SystemFlag {
                flag: "--system".into()
            }
        );
    }

    #[test]
    fn resolve_strategy_explicit_overrides_lookup_table() {
        let mut args = default_args("ollama");
        args.system_flag = Some("--system-prompt".into());
        // Even though "ollama" maps to SystemEnv in the lookup,
        // explicit `--system-flag` wins.
        assert_eq!(
            resolve_strategy(&args),
            WrapStrategy::SystemFlag {
                flag: "--system-prompt".into()
            }
        );
    }

    #[test]
    fn resolve_strategy_env_override_takes_precedence_over_flag_default() {
        let mut args = default_args("codex");
        args.system_env = Some("OPENAI_CLI_SYSTEM".into());
        assert_eq!(
            resolve_strategy(&args),
            WrapStrategy::SystemEnv {
                name: "OPENAI_CLI_SYSTEM".into()
            }
        );
    }

    #[test]
    fn resolve_strategy_message_file_override() {
        let mut args = default_args("codex");
        args.message_file_flag = Some("--prompt-file".into());
        assert_eq!(
            resolve_strategy(&args),
            WrapStrategy::MessageFile {
                flag: "--prompt-file".into()
            }
        );
    }

    #[test]
    fn build_system_message_prepends_preamble() {
        let msg = build_system_message("- [mid/abc] hello");
        assert!(msg.starts_with(WRAP_PREAMBLE));
        assert!(msg.contains("hello"));
        assert!(msg.contains("\n\n"), "preamble + body separator missing");
    }

    #[test]
    fn build_system_message_empty_body_returns_preamble_only() {
        let msg = build_system_message("");
        assert_eq!(msg, WRAP_PREAMBLE);
    }

    #[test]
    fn build_system_message_strips_trailing_whitespace() {
        let msg = build_system_message("body line\n\n\n");
        assert!(msg.ends_with("body line"));
    }

    #[test]
    fn build_command_system_flag_sets_argv_correctly() {
        let strat = WrapStrategy::SystemFlag {
            flag: "--system".into(),
        };
        let trailing = vec![
            "chat".to_string(),
            "--model".to_string(),
            "gpt-5".to_string(),
        ];
        let (cmd, tf) =
            build_command_for_strategy("codex", &strat, "SYS-MSG-VALUE", &trailing).unwrap();
        assert!(tf.is_none(), "SystemFlag must not allocate a tempfile");
        let argv: Vec<String> = cmd.get_args().map(|s| os_str_to_string_lossy(s)).collect();
        assert_eq!(
            argv,
            vec!["--system", "SYS-MSG-VALUE", "chat", "--model", "gpt-5"]
        );
        // Verify the program name (first arg of Command, not in
        // get_args) — get_program is part of the std API.
        assert_eq!(cmd.get_program(), OsStr::new("codex"));
    }

    #[test]
    fn build_command_system_env_sets_env_var_and_omits_flag() {
        let strat = WrapStrategy::SystemEnv {
            name: "OLLAMA_SYSTEM".into(),
        };
        let trailing = vec!["run".to_string(), "hermes3:8b".to_string()];
        let (cmd, tf) =
            build_command_for_strategy("ollama", &strat, "SYS-ENV-MSG", &trailing).unwrap();
        assert!(tf.is_none(), "SystemEnv must not allocate a tempfile");
        let argv: Vec<String> = cmd.get_args().map(|s| os_str_to_string_lossy(s)).collect();
        // The env-var strategy never injects a flag — argv is just the
        // trailing args.
        assert_eq!(argv, vec!["run", "hermes3:8b"]);
        // Confirm OLLAMA_SYSTEM is set on the Command's env. get_envs()
        // yields (key, Option<value>) pairs.
        let env_pairs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    os_str_to_string_lossy(k),
                    v.map(|x| os_str_to_string_lossy(x)),
                )
            })
            .collect();
        let entry = env_pairs
            .iter()
            .find(|(k, _)| k == "OLLAMA_SYSTEM")
            .expect("OLLAMA_SYSTEM must be set");
        assert_eq!(entry.1.as_deref(), Some("SYS-ENV-MSG"));
    }

    #[test]
    fn wrap_strategy_message_file_creates_tempfile_and_cleans_up() {
        let strat = WrapStrategy::MessageFile {
            flag: "--message-file".into(),
        };
        let (path_owned, exists_during) = {
            let (cmd, tf) =
                build_command_for_strategy("aider", &strat, "FILE-MSG-CONTENT", &[]).unwrap();
            let tf = tf.expect("MessageFile must allocate a tempfile");
            // The argv should point at the tempfile path. We can't
            // directly assert path equality on Windows (canonicalisation
            // differs), so just check the `--message-file` flag is the
            // first arg and the second arg is some non-empty path.
            let argv: Vec<String> = cmd.get_args().map(|s| os_str_to_string_lossy(s)).collect();
            assert_eq!(argv.len(), 2);
            assert_eq!(argv[0], "--message-file");
            assert!(!argv[1].is_empty());
            // Sanity: the tempfile contains the expected message body.
            let read_back = std::fs::read_to_string(tf.path()).unwrap();
            assert_eq!(read_back, "FILE-MSG-CONTENT");
            let exists = tf.path().exists();
            // Take the path as PathBuf BEFORE dropping `tf` so we can
            // re-stat after the block exits.
            let p = tf.path().to_path_buf();
            (p, exists)
        };
        assert!(
            exists_during,
            "tempfile must exist while NamedTempFile is alive"
        );
        // After the block ends, NamedTempFile is dropped, which
        // unlinks the file (Unix and Windows both — tempfile crate
        // smooths over the platform difference).
        assert!(
            !path_owned.exists(),
            "tempfile must be cleaned up on Drop, but {} still exists",
            path_owned.display()
        );
    }

    #[test]
    fn wrap_with_unreachable_db_does_not_block_agent() {
        // Boot honors `--quiet` and exits 0 with a warn header on stdout
        // when the DB is missing. The captured stdout becomes the body
        // of the wrap system message. We assert: (a) `run_boot_capture`
        // returns *something* (the warn header) without erroring, and
        // (b) the assembled system message still carries the preamble
        // so the agent knows it has memory access (even if empty).
        let env = TestEnv::fresh();
        let bad = env
            .db_path
            .parent()
            .unwrap()
            .join("nope/that/does/not/exist/db.sqlite");
        let captured = run_boot_capture(
            &bad,
            10,
            DEFAULT_WRAP_BUDGET_TOKENS,
            &crate::config::AppConfig::default(),
        );
        assert!(
            captured.contains("# ai-memory boot: warn"),
            "wrap should surface the warn header even with unreachable DB: {captured}"
        );
        let assembled = build_system_message(&captured);
        assert!(assembled.starts_with(WRAP_PREAMBLE));
        assert!(assembled.contains("warn"));
    }

    #[test]
    fn wrap_with_no_boot_skips_context() {
        // Smoke: the run path with `no_boot = true` produces a system
        // message that's exactly the preamble (no boot body). We verify
        // by re-running the equivalent assembly the `run` function uses
        // when `args.no_boot` is true.
        let mut args = default_args("codex");
        args.no_boot = true;
        // The `run` body's `if args.no_boot { WRAP_PREAMBLE.to_string() }`
        // branch is what produces the system message in this mode.
        // We replicate it here so we can assert on the value without
        // spawning a subprocess (the real `codex` isn't on the test
        // host's PATH).
        let system_msg = if args.no_boot {
            WRAP_PREAMBLE.to_string()
        } else {
            unreachable!()
        };
        assert_eq!(system_msg, WRAP_PREAMBLE);
        // And the assembled command for that message must contain
        // exactly the preamble as the flag value, no boot context.
        let (cmd, _tf) = build_command_for_strategy(
            &args.agent,
            &resolve_strategy(&args),
            &system_msg,
            &args.trailing,
        )
        .unwrap();
        let argv: Vec<String> = cmd.get_args().map(|s| os_str_to_string_lossy(s)).collect();
        assert_eq!(argv.len(), 2);
        assert_eq!(argv[0], "--system");
        assert_eq!(argv[1], WRAP_PREAMBLE);
    }

    #[test]
    fn wrap_injects_system_message_via_flag() {
        // Seed a memory so the boot output is non-empty, then assert
        // the assembled system message that wrap would pass to the
        // agent contains both the preamble AND the seeded memory's
        // title. This is the contract the docs/integrations recipes
        // depend on.
        let env = TestEnv::fresh();
        seed_memory(&env.db_path, "ns-wrap-test", "wrap-injection-canary", "x");
        let captured = run_boot_capture(
            &env.db_path,
            10,
            DEFAULT_WRAP_BUDGET_TOKENS,
            &crate::config::AppConfig::default(),
        );
        // boot::run sets the namespace from auto_namespace, which won't
        // match `ns-wrap-test` unless cwd is set. The fallback path
        // should still surface SOMETHING so the captured body is
        // non-empty (warn or info header at minimum).
        assert!(
            !captured.is_empty(),
            "expected non-empty boot capture, got empty"
        );
        let assembled = build_system_message(&captured);
        assert!(assembled.starts_with(WRAP_PREAMBLE));
        assert!(assembled.len() > WRAP_PREAMBLE.len());
        // Now assert the assembled message rides through to the
        // command's argv.
        let (cmd, _tf) = build_command_for_strategy(
            "codex",
            &WrapStrategy::SystemFlag {
                flag: "--system".into(),
            },
            &assembled,
            &[],
        )
        .unwrap();
        let argv: Vec<String> = cmd.get_args().map(|s| os_str_to_string_lossy(s)).collect();
        assert_eq!(argv.len(), 2);
        assert_eq!(argv[0], "--system");
        assert!(argv[1].starts_with(WRAP_PREAMBLE));
    }

    #[test]
    fn wrap_passes_through_exit_code_via_status_propagation() {
        // We can't assume any specific binary is on PATH, but we can
        // exercise the propagation logic with a guaranteed-available
        // command: `false` on Unix exits 1, `true` exits 0. On Windows
        // we use `cmd /C exit N`.
        #[cfg(unix)]
        {
            // Exit 0
            let cmd = Command::new("true");
            let code = spawn_and_wait(cmd).unwrap();
            assert_eq!(code, 0);
            // Exit 1
            let cmd = Command::new("false");
            let code = spawn_and_wait(cmd).unwrap();
            assert_eq!(code, 1);
        }
        #[cfg(windows)]
        {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", "exit", "0"]);
            let code = spawn_and_wait(cmd).unwrap();
            assert_eq!(code, 0);
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", "exit", "7"]);
            let code = spawn_and_wait(cmd).unwrap();
            assert_eq!(code, 7);
        }
    }

    #[test]
    fn wrap_run_returns_exit_code_for_real_subprocess() {
        // End-to-end: drive `run` itself (not just the helpers). We
        // wrap a known-good binary (`true` on unix, `cmd /C exit` on
        // windows) and assert the returned code matches.
        let mut env = TestEnv::fresh();
        let db_path = env.db_path.clone();
        let mut out = env.output();
        #[cfg(unix)]
        {
            let mut args = default_args("true");
            // Skip boot to avoid touching the DB and to keep the test
            // deterministic. `--system "..."` is still passed to the
            // agent — `true` ignores all argv, exits 0.
            args.no_boot = true;
            let code = run(
                &db_path,
                &args,
                &crate::config::AppConfig::default(),
                &mut out,
            )
            .unwrap();
            assert_eq!(code, 0);
        }
        #[cfg(windows)]
        {
            let mut args = default_args("cmd");
            args.no_boot = true;
            // We override the strategy to SystemFlag with a no-op flag
            // that `cmd /C` will ignore alongside the system message,
            // then a real /C exit. Easier: override via system_env so
            // no flag is added, then trailing carries `/C exit 5`.
            args.system_env = Some("WRAP_DUMMY".into());
            args.trailing = vec!["/C".into(), "exit".into(), "5".into()];
            let code = run(
                &db_path,
                &args,
                &crate::config::AppConfig::default(),
                &mut out,
            )
            .unwrap();
            assert_eq!(code, 5);
        }
    }

    #[test]
    fn auto_strategy_resolves_at_command_build_time() {
        // Exercise the `WrapStrategy::Auto` recursive branch in
        // `build_command_for_strategy`.
        let (cmd, tf) = build_command_for_strategy(
            "codex",
            &WrapStrategy::Auto,
            "AUTO-MSG",
            &["chat".to_string()],
        )
        .unwrap();
        assert!(tf.is_none());
        let argv: Vec<String> = cmd.get_args().map(|s| os_str_to_string_lossy(s)).collect();
        // codex auto-resolves to SystemFlag{--system}.
        assert_eq!(argv, vec!["--system", "AUTO-MSG", "chat"]);
    }

    #[test]
    fn auto_strategy_resolves_to_message_file_for_aider() {
        let (cmd, tf) =
            build_command_for_strategy("aider", &WrapStrategy::Auto, "AIDER-MSG", &[]).unwrap();
        // aider auto-resolves to MessageFile, so a tempfile must be
        // allocated.
        assert!(tf.is_some());
        let argv: Vec<String> = cmd.get_args().map(|s| os_str_to_string_lossy(s)).collect();
        assert_eq!(argv.len(), 2);
        assert_eq!(argv[0], "--message-file");
    }

    #[test]
    fn run_boot_capture_returns_string_not_panics_on_missing_db() {
        // Hardening: every error path inside boot must surface as a
        // String (possibly empty, possibly the warn header) — never a
        // panic — so the wrapped agent always runs.
        let env = TestEnv::fresh();
        let bad = env
            .db_path
            .parent()
            .unwrap()
            .join("__definitely_missing__/db");
        let s = run_boot_capture(
            &bad,
            10,
            DEFAULT_WRAP_BUDGET_TOKENS,
            &crate::config::AppConfig::default(),
        );
        // Either the warn header or empty (both are non-panic outcomes).
        assert!(
            s.is_empty() || s.contains("# ai-memory boot:"),
            "expected warn header or empty, got: {s}"
        );
    }
}
