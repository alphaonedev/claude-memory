// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7 Track G — Task G1: hook configuration schema + SIGHUP hot reload.
//
// # Canonical `hooks.toml` schema
//
// ```toml
// [[hook]]
// event = "post_store"
// command = "/usr/local/bin/auto-link-detector"
// priority = 100
// timeout_ms = 5000
// mode = "daemon"
// enabled = true
// namespace = "team/*"
// ```
//
// Multiple `[[hook]]` blocks may target the same event; insertion
// order is preserved so G5's chain-ordering pass can apply
// priority-descending sort deterministically.
//
// # Default config path
//
// `dirs::config_dir().join("ai-memory/hooks.toml")`. On Linux that
// resolves to `~/.config/ai-memory/hooks.toml`; on macOS,
// `~/Library/Application Support/ai-memory/hooks.toml`.
//
// # Hot reload
//
// `spawn_reload_task` listens for `SIGHUP` and atomically swaps
// the config snapshot held behind an `Arc<RwLock<…>>`. In-flight
// hook executions (landing in G3) read the snapshot once at
// dispatch time, so a reload mid-fire never tears.
//
// # Validation rules (G1)
//
// * `priority` — any `i32` (descending sort lives in G5).
// * `timeout_ms` — `u32`, capped at 30_000ms. Larger values are
//   rejected with a named [`HooksConfigError::Validation`].
// * `command` — must be non-empty. Path existence is *not*
//   checked here; the executor (G3) is the right layer for that
//   so a missing binary surfaces as an executor error with full
//   context, not a config-parse error before the daemon boots.
// * `namespace` — non-empty string. A real glob/pattern matcher
//   does not yet exist in this crate; G2/G3 will swap in the
//   real one when it ships. See the TODO below.
// * Parse errors include the failing TOML span (line:col) via
//   `toml::de::Error::span()` when the underlying error carries
//   one.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// HookEvent
// ---------------------------------------------------------------------------
//
// G1 shipped a 20-variant stub of `HookEvent` here so the
// configuration loader had a tag type to deserialize against.
// G2 lifts the canonical definition into `crate::hooks::events`
// and attaches a payload struct to every variant. The re-export
// below preserves `use crate::hooks::config::HookEvent` for any
// caller that landed against the G1 path.

pub use super::events::HookEvent;

// ---------------------------------------------------------------------------
// HookMode
// ---------------------------------------------------------------------------

/// Execution mode for a hook entry.
///
/// * [`HookMode::Exec`] — subprocess per fire; JSON over stdio.
/// * [`HookMode::Daemon`] — long-lived child; JSON-RPC framed.
///
/// G3 implements both. Hot-path events (`post_recall`,
/// `post_search`) default to `daemon` to preserve the v0.6.3
/// 50ms recall budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookMode {
    Exec,
    Daemon,
}

// ---------------------------------------------------------------------------
// FailMode
// ---------------------------------------------------------------------------

/// Hook crash-handling posture, consumed by G5's chain runner.
///
/// * [`FailMode::Open`] — when the executor returns `Err` (spawn
///   failure, decode failure, timeout, daemon unavailable, …) the
///   chain logs a warning and treats the failed fire as `Allow`. This
///   is the v0.7 default because the bias on the request path is
///   "fail open, log loudly" — a buggy hook must not brick recall.
/// * [`FailMode::Closed`] — the chain converts the executor error
///   into `ChainResult::Deny` and short-circuits the chain. Reserved
///   for hooks that gate compliance-critical paths (PII redaction,
///   regulated-tenant access control) where a silent fail-open is
///   worse than a hard refusal.
///
/// The field is optional in `hooks.toml`; missing entries default to
/// [`FailMode::Open`] so G3-era configs keep their behaviour after
/// G5 lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailMode {
    Open,
    Closed,
}

impl Default for FailMode {
    fn default() -> Self {
        FailMode::Open
    }
}

/// Serde default helper — `serde(default)` only reaches for `Default::default`
/// on the *field type*; this named function lets the
/// `#[serde(default = "...")]` form work without a wrapper newtype.
fn default_fail_mode() -> FailMode {
    FailMode::Open
}

// ---------------------------------------------------------------------------
// HookConfig
// ---------------------------------------------------------------------------

/// Maximum allowed `timeout_ms`. A hook taking longer than 30s
/// is almost certainly a bug; the chain-orchestrator (G5/G6)
/// would otherwise stall the memory operation that fired it.
pub const MAX_TIMEOUT_MS: u32 = 30_000;

/// One `[[hook]]` block from `hooks.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookConfig {
    pub event: HookEvent,
    pub command: PathBuf,
    pub priority: i32,
    pub timeout_ms: u32,
    pub mode: HookMode,
    pub enabled: bool,
    pub namespace: String,
    /// G5 — chain crash-handling posture. Defaults to
    /// [`FailMode::Open`] so existing G3-era configs keep firing
    /// fail-open even after G5 wires the chain runner in. Hooks
    /// that gate compliance-critical paths set
    /// `fail_mode = "closed"` to convert executor errors into a
    /// chain-level `Deny`.
    #[serde(default = "default_fail_mode")]
    pub fail_mode: FailMode,
}

/// Top-level TOML shape: `[[hook]]` blocks collect into
/// `hooks: Vec<HookConfig>`.
#[derive(Debug, Deserialize)]
struct HooksFile {
    #[serde(default, rename = "hook")]
    hooks: Vec<HookConfig>,
}

impl HookConfig {
    /// Load and validate the hook config file at `path`.
    ///
    /// Returns the hook entries in their original on-disk order;
    /// G5's chain ordering pass is responsible for the
    /// priority-descending sort.
    pub fn load_from_file(path: &Path) -> Result<Vec<HookConfig>, HooksConfigError> {
        let contents = std::fs::read_to_string(path).map_err(HooksConfigError::Io)?;
        Self::load_from_str(&contents)
    }

    /// Parse + validate from a TOML string. Split out from
    /// [`Self::load_from_file`] so unit tests can exercise the
    /// parser without touching disk.
    pub fn load_from_str(contents: &str) -> Result<Vec<HookConfig>, HooksConfigError> {
        let parsed: HooksFile = toml::from_str(contents).map_err(|e| {
            // toml 0.8's `de::Error::span()` returns a byte range
            // into the input; convert to (line, col) for the
            // operator-facing error message.
            let (line, col) = e
                .span()
                .map(|s| byte_offset_to_line_col(contents, s.start))
                .unwrap_or((0, 0));
            HooksConfigError::Toml {
                line,
                column: col,
                message: e.to_string(),
            }
        })?;

        for (idx, h) in parsed.hooks.iter().enumerate() {
            validate_hook(idx, h)?;
        }

        Ok(parsed.hooks)
    }

    /// `dirs::config_dir().join("ai-memory/hooks.toml")` — the
    /// platform-correct default location.
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("ai-memory/hooks.toml"))
    }
}

fn validate_hook(idx: usize, h: &HookConfig) -> Result<(), HooksConfigError> {
    if h.timeout_ms > MAX_TIMEOUT_MS {
        return Err(HooksConfigError::Validation {
            field: format!("hook[{idx}].timeout_ms"),
            reason: format!("{} exceeds maximum {MAX_TIMEOUT_MS}ms", h.timeout_ms),
        });
    }
    if h.command.as_os_str().is_empty() {
        return Err(HooksConfigError::Validation {
            field: format!("hook[{idx}].command"),
            reason: "must be a non-empty path".into(),
        });
    }
    // TODO(G2/G3): validate namespace against the real
    // pattern matcher once it ships. Today no glob matcher
    // exists in src/ — `db::matches_subtree` is prefix-only
    // and not callable from this layer. For now we accept any
    // non-empty string.
    if h.namespace.trim().is_empty() {
        return Err(HooksConfigError::Validation {
            field: format!("hook[{idx}].namespace"),
            reason: "must be a non-empty pattern (use \"*\" to match all)".into(),
        });
    }
    Ok(())
}

/// Convert a byte offset into a 1-indexed (line, column) pair
/// suitable for human-facing error messages.
fn byte_offset_to_line_col(s: &str, offset: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut col = 1usize;
    for (i, ch) in s.char_indices() {
        if i >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

// ---------------------------------------------------------------------------
// HooksConfigError
// ---------------------------------------------------------------------------

/// Errors surfaced by the hook config loader.
#[derive(Debug)]
pub enum HooksConfigError {
    /// Could not read the config file.
    Io(std::io::Error),
    /// TOML parse failure. `line` / `column` are 1-indexed when
    /// the underlying error carried a span; otherwise both are
    /// `0` to signal "location unknown".
    Toml {
        line: usize,
        column: usize,
        message: String,
    },
    /// Schema-level validation failure (e.g. `timeout_ms` over
    /// the 30s ceiling). `field` names the offending entry using
    /// `hook[<idx>].<field>` so operators can locate it in the
    /// source TOML.
    Validation { field: String, reason: String },
}

impl fmt::Display for HooksConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HooksConfigError::Io(e) => write!(f, "hooks.toml read error: {e}"),
            HooksConfigError::Toml {
                line,
                column,
                message,
            } => {
                if *line == 0 {
                    write!(f, "hooks.toml parse error: {message}")
                } else {
                    write!(
                        f,
                        "hooks.toml parse error at line {line}, column {column}: {message}"
                    )
                }
            }
            HooksConfigError::Validation { field, reason } => {
                write!(f, "hooks.toml validation error in {field}: {reason}")
            }
        }
    }
}

impl std::error::Error for HooksConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            HooksConfigError::Io(e) => Some(e),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Hot reload (SIGHUP)
// ---------------------------------------------------------------------------

/// Shared, hot-swappable snapshot of the loaded hook config. The
/// executor (G3) holds an `Arc<HookConfigSnapshot>` and reads it
/// once per dispatch so an in-flight execution always lands on a
/// consistent view of the config — even if SIGHUP arrives mid-fire.
pub type HookConfigSnapshot = RwLock<Vec<HookConfig>>;

/// Spawn a tokio task that listens for `SIGHUP` and reloads
/// `path` into `snapshot` on every signal.
///
/// G1 ships the signal-handler plumbing; the hooks loaded into
/// the snapshot become live as soon as G3's executor starts
/// reading from it. Until then this is a no-op observable only
/// via a `tracing::info!` log line per reload — exactly what the
/// G1 epic doc calls for ("for now just load + emit a tracing
/// info on reload").
///
/// Returns the [`tokio::task::JoinHandle`] so the daemon main
/// loop can shut the task down on graceful exit.
#[cfg(unix)]
pub fn spawn_reload_task(
    path: PathBuf,
    snapshot: Arc<HookConfigSnapshot>,
) -> tokio::task::JoinHandle<()> {
    use tokio::signal::unix::{SignalKind, signal};

    tokio::spawn(async move {
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "hooks: failed to install SIGHUP handler");
                return;
            }
        };

        while sighup.recv().await.is_some() {
            match HookConfig::load_from_file(&path) {
                Ok(new_cfg) => {
                    let count = new_cfg.len();
                    let mut guard = snapshot.write().await;
                    *guard = new_cfg;
                    tracing::info!(
                        path = %path.display(),
                        hooks = count,
                        "hooks: reloaded config on SIGHUP"
                    );
                }
                Err(e) => {
                    // Reload failure leaves the previous
                    // snapshot in place — operators get a loud
                    // error log but the running daemon keeps
                    // serving with the last-known-good config.
                    tracing::error!(
                        path = %path.display(),
                        error = %e,
                        "hooks: SIGHUP reload failed; keeping previous config"
                    );
                }
            }
        }
    })
}

// On non-unix platforms SIGHUP doesn't exist. The daemon is
// unix-only in practice (the Linux/macOS systemd + launchd
// units are the only supported deployments), so this is a stub
// to keep the windows build green for tooling like `cargo
// check --target x86_64-pc-windows-msvc`.
#[cfg(not(unix))]
pub fn spawn_reload_task(
    _path: PathBuf,
    _snapshot: Arc<HookConfigSnapshot>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tracing::warn!("hooks: SIGHUP reload not supported on this platform");
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const VALID_CANONICAL: &str = r#"
[[hook]]
event = "post_store"
command = "/usr/local/bin/auto-link-detector"
priority = 100
timeout_ms = 5000
mode = "daemon"
enabled = true
namespace = "team/*"
"#;

    #[test]
    fn parses_canonical_example() {
        let hooks = HookConfig::load_from_str(VALID_CANONICAL).expect("parses");
        assert_eq!(hooks.len(), 1);
        let h = &hooks[0];
        assert_eq!(h.event, HookEvent::PostStore);
        assert_eq!(
            h.command,
            PathBuf::from("/usr/local/bin/auto-link-detector")
        );
        assert_eq!(h.priority, 100);
        assert_eq!(h.timeout_ms, 5_000);
        assert_eq!(h.mode, HookMode::Daemon);
        assert!(h.enabled);
        assert_eq!(h.namespace, "team/*");
    }

    #[test]
    fn rejects_timeout_over_cap() {
        let toml_src = r#"
[[hook]]
event = "post_recall"
command = "/bin/true"
priority = 0
timeout_ms = 60000
mode = "exec"
enabled = true
namespace = "*"
"#;
        let err = HookConfig::load_from_str(toml_src).unwrap_err();
        match err {
            HooksConfigError::Validation { field, reason } => {
                assert!(field.ends_with("timeout_ms"), "field was {field}");
                assert!(reason.contains("30000"), "reason was {reason}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn invalid_toml_reports_line_number() {
        // `mode = ` with no value — the parser will fail on the
        // line carrying the broken assignment. We assert the
        // error names a non-zero line so operators can grep for it.
        let toml_src = "\n\n[[hook]]\nevent = \"post_store\"\nmode = \n";
        let err = HookConfig::load_from_str(toml_src).unwrap_err();
        match err {
            HooksConfigError::Toml {
                line, ref message, ..
            } => {
                assert!(line > 0, "expected non-zero line, got {line}");
                let displayed = err.to_string();
                assert!(
                    displayed.contains(&format!("line {line}")),
                    "Display did not surface line: {displayed} (raw msg: {message})"
                );
            }
            other => panic!("expected Toml, got {other:?}"),
        }
    }

    #[test]
    fn multiple_hooks_same_event_preserve_order() {
        let toml_src = r#"
[[hook]]
event = "post_store"
command = "/bin/first"
priority = 10
timeout_ms = 1000
mode = "exec"
enabled = true
namespace = "*"

[[hook]]
event = "post_store"
command = "/bin/second"
priority = 5
timeout_ms = 1000
mode = "exec"
enabled = true
namespace = "*"

[[hook]]
event = "post_store"
command = "/bin/third"
priority = 50
timeout_ms = 1000
mode = "exec"
enabled = true
namespace = "*"
"#;
        let hooks = HookConfig::load_from_str(toml_src).expect("parses");
        assert_eq!(hooks.len(), 3);
        assert_eq!(hooks[0].command, PathBuf::from("/bin/first"));
        assert_eq!(hooks[1].command, PathBuf::from("/bin/second"));
        assert_eq!(hooks[2].command, PathBuf::from("/bin/third"));
        // All three target the same event.
        assert!(hooks.iter().all(|h| h.event == HookEvent::PostStore));
    }

    #[test]
    fn rejects_empty_namespace() {
        let toml_src = r#"
[[hook]]
event = "post_store"
command = "/bin/true"
priority = 0
timeout_ms = 1000
mode = "exec"
enabled = true
namespace = ""
"#;
        let err = HookConfig::load_from_str(toml_src).unwrap_err();
        assert!(matches!(err, HooksConfigError::Validation { .. }));
    }

    #[test]
    fn rejects_empty_command() {
        let toml_src = r#"
[[hook]]
event = "post_store"
command = ""
priority = 0
timeout_ms = 1000
mode = "exec"
enabled = true
namespace = "*"
"#;
        let err = HookConfig::load_from_str(toml_src).unwrap_err();
        match err {
            HooksConfigError::Validation { field, .. } => {
                assert!(field.ends_with("command"), "field was {field}");
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    #[test]
    fn empty_file_yields_zero_hooks() {
        let hooks = HookConfig::load_from_str("").expect("parses");
        assert!(hooks.is_empty());
    }

    #[test]
    fn load_from_file_round_trip() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        tmp.write_all(VALID_CANONICAL.as_bytes()).expect("write");
        let hooks = HookConfig::load_from_file(tmp.path()).expect("loads");
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].event, HookEvent::PostStore);
    }

    /// Hot-reload smoke test: load config A, replace the file
    /// on disk, call `load_from_file` again (the same code path
    /// the SIGHUP task drives), assert the snapshot now reflects
    /// config B.
    ///
    /// We exercise the loader directly rather than spawning the
    /// signal task because portable + deterministic test signal
    /// delivery on macOS+Linux is fiddly enough that the value
    /// add lives in the loader, not in tokio's signal plumbing.
    /// G3 will gain an end-to-end SIGHUP integration test once
    /// the executor is wired in.
    #[tokio::test]
    async fn sighup_reload_swaps_snapshot() {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        tmp.write_all(VALID_CANONICAL.as_bytes()).expect("write A");

        let snapshot: Arc<HookConfigSnapshot> = Arc::new(RwLock::new(
            HookConfig::load_from_file(tmp.path()).expect("load A"),
        ));

        {
            let guard = snapshot.read().await;
            assert_eq!(guard.len(), 1);
            assert_eq!(
                guard[0].command,
                PathBuf::from("/usr/local/bin/auto-link-detector")
            );
        }

        // Replace on-disk content with config B (different
        // command, two entries) — this mirrors what an operator
        // does before sending SIGHUP.
        let config_b = r#"
[[hook]]
event = "pre_store"
command = "/opt/hooks/redact-pii"
priority = 200
timeout_ms = 2500
mode = "exec"
enabled = true
namespace = "*"

[[hook]]
event = "post_recall"
command = "/opt/hooks/expand-context"
priority = 50
timeout_ms = 100
mode = "daemon"
enabled = false
namespace = "team/*"
"#;
        std::fs::write(tmp.path(), config_b).expect("rewrite to B");

        // Drive the same code path the SIGHUP task uses.
        let new_cfg = HookConfig::load_from_file(tmp.path()).expect("load B");
        {
            let mut guard = snapshot.write().await;
            *guard = new_cfg;
        }

        let guard = snapshot.read().await;
        assert_eq!(guard.len(), 2);
        assert_eq!(guard[0].event, HookEvent::PreStore);
        assert_eq!(guard[0].command, PathBuf::from("/opt/hooks/redact-pii"));
        assert_eq!(guard[1].event, HookEvent::PostRecall);
        assert!(!guard[1].enabled);
    }

    #[test]
    fn default_path_is_under_config_dir() {
        // We can't assert the full path on every platform but we
        // can verify it ends with `ai-memory/hooks.toml` when
        // `dirs::config_dir()` resolves at all.
        if let Some(p) = HookConfig::default_path() {
            let s = p.to_string_lossy();
            assert!(
                s.ends_with("ai-memory/hooks.toml") || s.ends_with("ai-memory\\hooks.toml"),
                "unexpected default path: {s}"
            );
        }
    }

    #[test]
    fn hook_event_serde_uses_snake_case() {
        // Sanity-check the rename — config files use
        // `pre_governance_decision` not `PreGovernanceDecision`.
        let json = serde_json::to_string(&HookEvent::PreGovernanceDecision).unwrap();
        assert_eq!(json, "\"pre_governance_decision\"");
        let back: HookEvent = serde_json::from_str("\"on_index_eviction\"").unwrap();
        assert_eq!(back, HookEvent::OnIndexEviction);
    }

    #[test]
    fn hook_mode_serde_uses_snake_case() {
        let exec_json = serde_json::to_string(&HookMode::Exec).unwrap();
        let daemon_json = serde_json::to_string(&HookMode::Daemon).unwrap();
        assert_eq!(exec_json, "\"exec\"");
        assert_eq!(daemon_json, "\"daemon\"");
    }
}
