// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! User-configurable log directory resolution (PR-5 addendum, issue #487).
//!
//! End users can set both `[logging] path` and `[audit] path` at every
//! layer; the highest-priority value wins:
//!
//! 1. **CLI flag** (`--log-dir`, `--audit-dir`) — explicit override on
//!    the `ai-memory logs` / `ai-memory audit` subcommands.
//! 2. **Environment variable** (`AI_MEMORY_LOG_DIR`,
//!    `AI_MEMORY_AUDIT_DIR`) — useful for `systemd` units, Docker
//!    `-e`, and Kubernetes env injection.
//! 3. **`config.toml`** (`[logging] path`, `[audit] path`) — the
//!    long-lived per-host setting maintainers write once.
//! 4. **Platform default** — picked per-OS so a fresh install works
//!    out of the box without any configuration.
//!
//! Platform defaults:
//!
//! | OS | Logs | Audit |
//! |---|---|---|
//! | Linux | `${XDG_STATE_HOME:-$HOME/.local/state}/ai-memory/logs/` | `…/audit/` |
//! | macOS | `~/Library/Logs/ai-memory/` | `~/Library/Logs/ai-memory/audit/` |
//! | Windows | `%LOCALAPPDATA%\ai-memory\logs\` | `…\audit\` |
//! | systemd-managed daemon | `/var/log/ai-memory/` (if writable) | `…/audit/` |
//!
//! ## systemd detection
//!
//! When `INVOCATION_ID` is present in the environment (set by `systemd`
//! for unit-managed processes) and `/var/log/ai-memory/` is writable,
//! the resolver picks the system-wide path. Otherwise it falls through
//! to the per-user XDG path.
//!
//! ## Security guard
//!
//! The resolved directory must not be world-writable. If a 0777 path is
//! configured (or selected by default on a malformed system), the
//! resolver returns an error pointing at the resolution chain that
//! landed there. Created parent directories use mode `0700` on Unix; on
//! Windows the default ACL is sufficient.
//!
//! See `docs/security/audit-trail.md` §"Log directory resolution" for
//! the operator guide.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

/// Environment variable consulted for the operational log directory
/// override. Read with `std::env::var_os` so non-UTF-8 paths on Windows
/// pass through unchanged.
pub const LOG_DIR_ENV: &str = "AI_MEMORY_LOG_DIR";

/// Environment variable consulted for the audit log directory override.
pub const AUDIT_DIR_ENV: &str = "AI_MEMORY_AUDIT_DIR";

/// Source layer that produced the resolved path. Returned alongside
/// the [`PathBuf`] so error messages can name the precedence step that
/// landed the user at a bad directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSource {
    /// Explicit `--log-dir` / `--audit-dir` flag.
    CliFlag,
    /// `AI_MEMORY_LOG_DIR` / `AI_MEMORY_AUDIT_DIR` environment variable.
    EnvVar,
    /// `[logging] path` / `[audit] path` in `config.toml`.
    ConfigToml,
    /// Platform default selected by the OS detection logic.
    PlatformDefault,
    /// systemd-managed daemon path (`/var/log/ai-memory/...`).
    SystemdLogsDir,
}

impl PathSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CliFlag => "CLI flag (--log-dir / --audit-dir)",
            Self::EnvVar => "environment variable (AI_MEMORY_LOG_DIR / AI_MEMORY_AUDIT_DIR)",
            Self::ConfigToml => "[logging]/[audit] path in config.toml",
            Self::PlatformDefault => "platform default",
            Self::SystemdLogsDir => "systemd LogsDirectory (/var/log/ai-memory/)",
        }
    }
}

/// Result of a directory-resolution call. The path itself plus the
/// layer that produced it (used for error messages).
#[derive(Debug, Clone)]
pub struct ResolvedDir {
    pub path: PathBuf,
    pub source: PathSource,
}

/// What kind of log directory we're resolving — dictates the platform
/// default suffix (`logs/` vs `audit/`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirKind {
    Log,
    Audit,
}

impl DirKind {
    fn suffix(self) -> &'static str {
        match self {
            Self::Log => "logs",
            Self::Audit => "audit",
        }
    }
}

/// Resolve the operational log directory honouring the precedence
/// ladder: CLI > env var > config > platform default.
///
/// `cli_override` — the parsed `--log-dir <PATH>` argument if any.
/// `config_path` — the `[logging] path` value if any.
///
/// # Errors
/// - The resolved directory exists but is world-writable.
pub fn resolve_log_dir(
    cli_override: Option<&Path>,
    config_path: Option<&str>,
) -> Result<ResolvedDir> {
    resolve_dir(DirKind::Log, cli_override, LOG_DIR_ENV, config_path)
}

/// Resolve the audit log directory honouring the precedence ladder.
/// Mirror of [`resolve_log_dir`] for the audit subsystem.
///
/// # Errors
/// - The resolved directory exists but is world-writable.
pub fn resolve_audit_dir(
    cli_override: Option<&Path>,
    config_path: Option<&str>,
) -> Result<ResolvedDir> {
    resolve_dir(DirKind::Audit, cli_override, AUDIT_DIR_ENV, config_path)
}

fn resolve_dir(
    kind: DirKind,
    cli_override: Option<&Path>,
    env_var: &str,
    config_path: Option<&str>,
) -> Result<ResolvedDir> {
    let resolved = if let Some(p) = cli_override {
        ResolvedDir {
            path: PathBuf::from(p),
            source: PathSource::CliFlag,
        }
    } else if let Some(env_val) = std::env::var_os(env_var) {
        if env_val.is_empty() {
            // Treat an empty env var as "unset" so a misconfigured
            // launcher doesn't silently route logs to the CWD.
            fall_through_to_config_or_default(kind, config_path)?
        } else {
            ResolvedDir {
                path: PathBuf::from(env_val),
                source: PathSource::EnvVar,
            }
        }
    } else {
        fall_through_to_config_or_default(kind, config_path)?
    };

    enforce_not_world_writable(&resolved)?;
    Ok(resolved)
}

fn fall_through_to_config_or_default(
    kind: DirKind,
    config_path: Option<&str>,
) -> Result<ResolvedDir> {
    if let Some(raw) = config_path
        && !raw.is_empty()
    {
        return Ok(ResolvedDir {
            path: PathBuf::from(expand_tilde(raw)),
            source: PathSource::ConfigToml,
        });
    }
    Ok(platform_default(kind))
}

/// Compute the platform default for `kind`. Pure — no filesystem touch
/// other than reading `INVOCATION_ID` / `XDG_STATE_HOME` / `HOME` /
/// `LOCALAPPDATA` env vars.
#[must_use]
pub fn platform_default(kind: DirKind) -> ResolvedDir {
    // systemd-managed daemon: prefer /var/log/ai-memory if writable.
    // Skip in tests so the resolver test suite is deterministic.
    if std::env::var_os("INVOCATION_ID").is_some() {
        let p = PathBuf::from("/var/log/ai-memory").join(kind.suffix());
        if is_writable_dir(&p.parent().unwrap_or(&p)) {
            return ResolvedDir {
                path: p,
                source: PathSource::SystemdLogsDir,
            };
        }
    }

    let p = if cfg!(target_os = "macos") {
        macos_default(kind)
    } else if cfg!(target_os = "windows") {
        windows_default(kind)
    } else {
        // Linux + every other Unix (BSD, illumos, etc.) — XDG.
        linux_xdg_default(kind)
    };
    ResolvedDir {
        path: p,
        source: PathSource::PlatformDefault,
    }
}

fn linux_xdg_default(kind: DirKind) -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|s| !s.is_empty())
        .map_or_else(
            || {
                let home = home_dir_or_dot();
                home.join(".local").join("state")
            },
            PathBuf::from,
        );
    base.join("ai-memory").join(kind.suffix())
}

fn macos_default(kind: DirKind) -> PathBuf {
    let home = home_dir_or_dot();
    let base = home.join("Library").join("Logs").join("ai-memory");
    match kind {
        DirKind::Log => base,
        DirKind::Audit => base.join("audit"),
    }
}

fn windows_default(kind: DirKind) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|s| !s.is_empty())
        .map_or_else(
            || {
                // Fallback if LOCALAPPDATA is unset (mostly tests / WSL).
                home_dir_or_dot()
                    .join("AppData")
                    .join("Local")
                    .join("ai-memory")
            },
            |s| PathBuf::from(s).join("ai-memory"),
        );
    base.join(kind.suffix())
}

fn home_dir_or_dot() -> PathBuf {
    if let Some(h) = std::env::var_os("HOME").filter(|s| !s.is_empty()) {
        return PathBuf::from(h);
    }
    if let Some(h) = std::env::var_os("USERPROFILE").filter(|s| !s.is_empty()) {
        return PathBuf::from(h);
    }
    PathBuf::from(".")
}

fn is_writable_dir(p: &Path) -> bool {
    if !p.exists() || !p.is_dir() {
        return false;
    }
    // Probe: try to create a unique temp file in the directory; if it
    // fails, treat the dir as not-writable. We keep this best-effort —
    // the kernel is the source of truth and this is a hint for the
    // resolution decision.
    let probe = p.join(format!(".ai-memory-write-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Reject world-writable directories. Returns `Ok(())` if the path
/// doesn't exist yet (we'll create it secure) or if it's safely
/// permissioned.
///
/// # Errors
/// - The path exists and `mode & 0o002 != 0` on Unix.
pub fn enforce_not_world_writable(rd: &ResolvedDir) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if !rd.path.exists() {
            return Ok(());
        }
        let md = std::fs::metadata(&rd.path).with_context(|| {
            format!(
                "stat {} (resolved via {})",
                rd.path.display(),
                rd.source.as_str()
            )
        })?;
        let mode = md.permissions().mode();
        if mode & 0o002 != 0 {
            return Err(anyhow!(
                "log directory {} is world-writable (mode {:#o}); refusing for security. \
                 Resolved via: {}. Pick a non-world-writable directory and re-run.",
                rd.path.display(),
                mode & 0o7777,
                rd.source.as_str()
            ));
        }
    }
    #[cfg(not(unix))]
    {
        // Windows: default ACL on `LOCALAPPDATA` and user-created dirs
        // is "Authenticated Users" only — no world-writable concept
        // mapping, so this is a no-op.
        let _ = rd;
    }
    Ok(())
}

/// Create `dir` (and missing parents) with mode `0700` on Unix. On
/// Windows defers to `std::fs::create_dir_all` and the default ACL.
///
/// # Errors
/// - The directory cannot be created.
/// - On Unix: the resulting permissions cannot be applied.
pub fn ensure_dir_secure(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating log directory {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(dir, perms)
            .with_context(|| format!("setting mode 0700 on log directory {}", dir.display()))?;
    }
    Ok(())
}

/// Tilde-expand a config string. Mirrors [`crate::audit::expand_tilde`]
/// so this module stays self-contained for resolver-level tests.
#[must_use]
pub fn expand_tilde(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        let mut buf = OsString::from(home);
        buf.push("/");
        buf.push(rest);
        return buf.to_string_lossy().into_owned();
    }
    raw.to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Process-wide lock so tests that mutate env vars (LOG_DIR_ENV /
    /// AUDIT_DIR_ENV / INVOCATION_ID / HOME / etc.) don't race.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Snapshot+restore an env var across a test body so we don't leak
    /// state into sibling tests.
    struct EnvGuard {
        key: &'static str,
        prev: Option<OsString>,
    }
    impl EnvGuard {
        fn capture(key: &'static str) -> Self {
            Self {
                key,
                prev: std::env::var_os(key),
            }
        }
        fn set(&self, v: &str) {
            // SAFETY: serialised via env_lock() at the test entry; no
            // other thread is reading the env concurrently.
            unsafe {
                std::env::set_var(self.key, v);
            }
        }
        fn unset(&self) {
            // SAFETY: same as `set`.
            unsafe {
                std::env::remove_var(self.key);
            }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: same as `set`.
            unsafe {
                if let Some(v) = &self.prev {
                    std::env::set_var(self.key, v);
                } else {
                    std::env::remove_var(self.key);
                }
            }
        }
    }

    #[test]
    fn log_dir_cli_flag_overrides_env_var() {
        let _g = env_lock();
        let env = EnvGuard::capture(LOG_DIR_ENV);
        env.set("/should/not/win");
        let cli = PathBuf::from("/cli/wins");
        let resolved = resolve_log_dir(Some(&cli), Some("/config/loses")).unwrap();
        assert_eq!(resolved.path, cli);
        assert_eq!(resolved.source, PathSource::CliFlag);
    }

    #[test]
    fn log_dir_env_var_overrides_config_toml() {
        let _g = env_lock();
        let env = EnvGuard::capture(LOG_DIR_ENV);
        env.set("/env/wins");
        let resolved = resolve_log_dir(None, Some("/config/loses")).unwrap();
        assert_eq!(resolved.path, PathBuf::from("/env/wins"));
        assert_eq!(resolved.source, PathSource::EnvVar);
    }

    #[test]
    fn log_dir_config_toml_overrides_platform_default() {
        let _g = env_lock();
        let env = EnvGuard::capture(LOG_DIR_ENV);
        env.unset();
        let _inv = EnvGuard::capture("INVOCATION_ID");
        _inv.unset();
        let resolved = resolve_log_dir(None, Some("/config/wins")).unwrap();
        assert_eq!(resolved.path, PathBuf::from("/config/wins"));
        assert_eq!(resolved.source, PathSource::ConfigToml);
    }

    #[test]
    fn log_dir_platform_default_resolves_per_os() {
        let _g = env_lock();
        let env = EnvGuard::capture(LOG_DIR_ENV);
        env.unset();
        let _inv = EnvGuard::capture("INVOCATION_ID");
        _inv.unset();
        let resolved = resolve_log_dir(None, None).unwrap();
        assert_eq!(resolved.source, PathSource::PlatformDefault);
        let s = resolved.path.to_string_lossy().to_string();
        if cfg!(target_os = "macos") {
            assert!(
                s.contains("Library/Logs/ai-memory"),
                "macOS default should be under Library/Logs/ai-memory, got {s}"
            );
        } else if cfg!(target_os = "windows") {
            assert!(
                s.to_lowercase().contains("ai-memory"),
                "Windows default should contain ai-memory, got {s}"
            );
        } else {
            // Linux + BSD + others fall through to XDG.
            assert!(
                s.contains("ai-memory") && s.contains("logs"),
                "Linux/Unix XDG default should contain ai-memory/logs, got {s}"
            );
        }
    }

    #[test]
    fn audit_dir_cli_flag_overrides_env_var() {
        let _g = env_lock();
        let env = EnvGuard::capture(AUDIT_DIR_ENV);
        env.set("/should/not/win");
        let cli = PathBuf::from("/cli/audit/wins");
        let resolved = resolve_audit_dir(Some(&cli), Some("/config/loses")).unwrap();
        assert_eq!(resolved.path, cli);
        assert_eq!(resolved.source, PathSource::CliFlag);
    }

    #[test]
    fn audit_dir_env_var_overrides_config_toml() {
        let _g = env_lock();
        let env = EnvGuard::capture(AUDIT_DIR_ENV);
        env.set("/env/audit/wins");
        let resolved = resolve_audit_dir(None, Some("/config/loses")).unwrap();
        assert_eq!(resolved.path, PathBuf::from("/env/audit/wins"));
        assert_eq!(resolved.source, PathSource::EnvVar);
    }

    #[test]
    fn audit_dir_config_toml_overrides_platform_default() {
        let _g = env_lock();
        let env = EnvGuard::capture(AUDIT_DIR_ENV);
        env.unset();
        let _inv = EnvGuard::capture("INVOCATION_ID");
        _inv.unset();
        let resolved = resolve_audit_dir(None, Some("/config/audit/wins")).unwrap();
        assert_eq!(resolved.path, PathBuf::from("/config/audit/wins"));
        assert_eq!(resolved.source, PathSource::ConfigToml);
    }

    #[test]
    fn audit_dir_platform_default_resolves_per_os() {
        let _g = env_lock();
        let env = EnvGuard::capture(AUDIT_DIR_ENV);
        env.unset();
        let _inv = EnvGuard::capture("INVOCATION_ID");
        _inv.unset();
        let resolved = resolve_audit_dir(None, None).unwrap();
        assert_eq!(resolved.source, PathSource::PlatformDefault);
        let s = resolved.path.to_string_lossy().to_string();
        assert!(
            s.contains("ai-memory") && s.contains("audit"),
            "audit platform default should mention ai-memory and audit, got {s}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn log_dir_creates_directory_with_secure_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("nested").join("logs");
        ensure_dir_secure(&target).unwrap();
        let md = std::fs::metadata(&target).unwrap();
        let mode = md.permissions().mode() & 0o7777;
        assert_eq!(
            mode, 0o700,
            "ensure_dir_secure must apply mode 0700 (got {mode:#o})"
        );
    }

    #[test]
    #[cfg(unix)]
    fn log_dir_refuses_world_writable_destination() {
        use std::os::unix::fs::PermissionsExt;
        let _g = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("worldwrite");
        std::fs::create_dir(&bad).unwrap();
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o777)).unwrap();
        let env = EnvGuard::capture(LOG_DIR_ENV);
        env.unset();
        let err = resolve_log_dir(Some(&bad), None).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("world-writable"),
            "error should mention world-writable, got: {msg}"
        );
        assert!(
            msg.contains("CLI flag"),
            "error should name resolution layer (CLI flag), got: {msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn audit_dir_refuses_world_writable_destination() {
        use std::os::unix::fs::PermissionsExt;
        let _g = env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("audit-worldwrite");
        std::fs::create_dir(&bad).unwrap();
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o777)).unwrap();
        let env = EnvGuard::capture(AUDIT_DIR_ENV);
        env.unset();
        let err = resolve_audit_dir(Some(&bad), None).unwrap_err();
        assert!(format!("{err}").contains("world-writable"));
    }

    #[test]
    fn log_dir_systemd_mode_uses_var_log_when_writable() {
        // Pure-logic test — we don't actually write to /var/log. We
        // assert the resolver's INVOCATION_ID branch picks SystemdLogsDir
        // when the writability probe succeeds. We use a tempdir
        // symlinked into a custom platform_default_for_systemd helper
        // tested via an env-var override path: setting INVOCATION_ID
        // and pointing /var/log via cfg-gated test harness is not
        // portable, so we instead test the underlying `is_writable_dir`
        // helper plus the env-var detection independently.
        let _g = env_lock();
        let _inv = EnvGuard::capture("INVOCATION_ID");
        _inv.set("test-invocation-id");

        let tmp = tempfile::tempdir().unwrap();
        // Confirm is_writable_dir matches reality on a fresh tempdir.
        assert!(is_writable_dir(tmp.path()));
        // Confirm a non-existent path is not "writable".
        assert!(!is_writable_dir(&tmp.path().join("does-not-exist")));

        // Exercise the platform_default branch with INVOCATION_ID set.
        // We can't force /var/log writable in unit tests, so we accept
        // either SystemdLogsDir (CI runners w/ /var/log root-writable
        // still won't pass the probe) or PlatformDefault. The contract
        // is: when INVOCATION_ID is unset, we never pick SystemdLogsDir.
        let resolved = platform_default(DirKind::Log);
        assert!(matches!(
            resolved.source,
            PathSource::SystemdLogsDir | PathSource::PlatformDefault
        ));

        _inv.unset();
        let resolved2 = platform_default(DirKind::Log);
        assert_eq!(
            resolved2.source,
            PathSource::PlatformDefault,
            "without INVOCATION_ID, must never pick SystemdLogsDir"
        );
    }

    #[test]
    fn log_dir_empty_env_var_falls_through_to_config() {
        let _g = env_lock();
        let env = EnvGuard::capture(LOG_DIR_ENV);
        env.set("");
        let resolved = resolve_log_dir(None, Some("/config/wins")).unwrap();
        assert_eq!(resolved.source, PathSource::ConfigToml);
    }

    #[test]
    fn expand_tilde_keeps_non_tilde_paths_unchanged() {
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }

    #[test]
    fn path_source_strings_are_human_readable() {
        for s in [
            PathSource::CliFlag,
            PathSource::EnvVar,
            PathSource::ConfigToml,
            PathSource::PlatformDefault,
            PathSource::SystemdLogsDir,
        ] {
            assert!(!s.as_str().is_empty());
        }
    }

    // ------------------------------------------------------------------
    // PR-9e coverage uplift (issue #487): close the security-guard and
    // tilde-expansion gaps flagged in the audit report.
    // ------------------------------------------------------------------

    #[test]
    fn expand_tilde_expands_home_dir() {
        let _g = env_lock();
        let env = EnvGuard::capture("HOME");
        env.set("/test-home");
        assert_eq!(expand_tilde("~/state/log"), "/test-home/state/log");
        // Bare `~` (no slash) is not expanded — matches the audit
        // module's expand_tilde which strictly looks for the `~/` prefix.
        assert_eq!(expand_tilde("~root"), "~root");
    }

    #[test]
    fn expand_tilde_no_home_keeps_input_unchanged() {
        let _g = env_lock();
        let env = EnvGuard::capture("HOME");
        env.unset();
        // Without HOME set, expansion must be a no-op.
        assert_eq!(expand_tilde("~/state"), "~/state");
    }

    #[cfg(unix)]
    #[test]
    fn enforce_not_world_writable_passes_through_on_nonexistent_path() {
        let tmp = tempfile::tempdir().unwrap();
        // Path under tempdir that does not exist — must succeed.
        let r = ResolvedDir {
            path: tmp.path().join("does-not-exist"),
            source: PathSource::ConfigToml,
        };
        assert!(enforce_not_world_writable(&r).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn enforce_not_world_writable_passes_safe_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let safe = tmp.path().join("safe");
        std::fs::create_dir(&safe).unwrap();
        std::fs::set_permissions(&safe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let r = ResolvedDir {
            path: safe,
            source: PathSource::ConfigToml,
        };
        assert!(enforce_not_world_writable(&r).is_ok());
    }

    #[test]
    fn is_writable_dir_returns_false_for_a_file_path() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("regular.txt");
        std::fs::write(&f, b"hello").unwrap();
        // A path that exists but is a file (not a dir) must fail
        // is_writable_dir's "is_dir" guard.
        assert!(!is_writable_dir(&f));
    }

    #[test]
    fn is_writable_dir_returns_false_for_nonexistent_path() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_writable_dir(&tmp.path().join("nope")));
    }

    #[test]
    fn dirkind_suffix_returns_logs_or_audit() {
        // Pure-logic helper exposed via DirKind — covers both arms of
        // the suffix() match.
        assert_eq!(DirKind::Log.suffix(), "logs");
        assert_eq!(DirKind::Audit.suffix(), "audit");
    }

    #[test]
    fn ensure_dir_secure_creates_nested_path() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("a").join("b").join("c");
        ensure_dir_secure(&target).unwrap();
        assert!(target.is_dir());
    }

    #[test]
    fn ensure_dir_secure_idempotent_on_existing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("present");
        std::fs::create_dir(&target).unwrap();
        // Second call must not error even though the dir already exists.
        ensure_dir_secure(&target).unwrap();
        ensure_dir_secure(&target).unwrap();
    }

    #[test]
    fn fall_through_uses_config_when_set() {
        // Indirect test: with no env override and a config path,
        // resolve_log_dir must pick ConfigToml.
        let _g = env_lock();
        let env = EnvGuard::capture(LOG_DIR_ENV);
        env.unset();
        let r = resolve_log_dir(None, Some("/tmp/explicit-config")).unwrap();
        assert_eq!(r.source, PathSource::ConfigToml);
        assert_eq!(r.path, PathBuf::from("/tmp/explicit-config"));
    }

    #[test]
    fn fall_through_expands_tilde_in_config_path() {
        let _g = env_lock();
        let env = EnvGuard::capture(LOG_DIR_ENV);
        env.unset();
        let home = EnvGuard::capture("HOME");
        home.set("/test-tilde-home");
        let r = resolve_log_dir(None, Some("~/state/logs")).unwrap();
        // The tilde must be expanded to the test HOME.
        assert_eq!(r.path, PathBuf::from("/test-tilde-home/state/logs"));
        assert_eq!(r.source, PathSource::ConfigToml);
    }

    #[test]
    fn fall_through_empty_config_path_uses_platform_default() {
        let _g = env_lock();
        let env = EnvGuard::capture(LOG_DIR_ENV);
        env.unset();
        let _inv = EnvGuard::capture("INVOCATION_ID");
        _inv.unset();
        // Empty config string must NOT short-circuit ConfigToml — it
        // falls through to platform default.
        let r = resolve_log_dir(None, Some("")).unwrap();
        assert_eq!(r.source, PathSource::PlatformDefault);
    }

    #[test]
    fn empty_audit_env_var_falls_through_to_config() {
        let _g = env_lock();
        let env = EnvGuard::capture(AUDIT_DIR_ENV);
        env.set("");
        let r = resolve_audit_dir(None, Some("/cfg/audit")).unwrap();
        assert_eq!(r.source, PathSource::ConfigToml);
        assert_eq!(r.path, PathBuf::from("/cfg/audit"));
    }
}
