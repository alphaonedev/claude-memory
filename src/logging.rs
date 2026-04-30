// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Operational logging facility (PR-5 of issue #487).
//!
//! Routes the binary's existing `tracing::info!` / `tracing::warn!` /
//! `tracing::error!` call sites through a rotating, on-disk file
//! appender so operators can ingest server logs into Splunk, Datadog,
//! Loki, etc.
//!
//! **Default-OFF.** Without a `[logging]` block in `config.toml` the
//! daemon keeps the legacy `tracing-subscriber::fmt` setup that writes
//! to stderr. Enabling file logging is opt-in:
//!
//! ```toml
//! [logging]
//! enabled = true
//! path = "~/.local/state/ai-memory/logs/"
//! max_size_mb = 100
//! max_files = 30
//! retention_days = 90
//! structured = false
//! level = "info"
//! ```
//!
//! See [`docs/security/audit-trail.md`](../docs/security/audit-trail.md)
//! for the SIEM ingestion guide.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};

use crate::config::LoggingConfig;
use crate::log_paths;

/// Default file prefix written by the rolling appender. Concrete
/// rotated filenames look like `ai-memory.log.2026-04-30`.
const DEFAULT_PREFIX: &str = "ai-memory.log";

/// Initialise the file logging facility. Returns a [`WorkerGuard`] that
/// the caller MUST keep alive for the lifetime of the process — when
/// dropped it flushes the in-memory buffer to disk. Returns `None`
/// when logging is disabled.
///
/// # Errors
/// - The configured log directory cannot be created.
/// - The rolling file appender cannot be constructed.
pub fn init_file_logging(cfg: &LoggingConfig) -> Result<Option<WorkerGuard>> {
    if !cfg.enabled.unwrap_or(false) {
        return Ok(None);
    }
    let dir = resolve_log_dir(cfg);
    log_paths::ensure_dir_secure(&dir)
        .with_context(|| format!("creating log dir {}", dir.display()))?;
    let appender = build_appender(&dir, cfg)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    // Capture the writer in the static slot so the daemon's tracing
    // subscriber can drain it. `try_init` so multiple test runs
    // (each spinning a fresh subscriber) don't poison the global.
    let level = cfg.level.as_deref().unwrap_or("info");
    let filter = tracing_subscriber::EnvFilter::try_new(level).unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::try_new("info").expect("info is a valid filter")
    });
    let structured = cfg.structured.unwrap_or(false);
    let res = if structured {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(writer)
            .json()
            .try_init()
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(writer)
            .try_init()
    };
    if let Err(e) = res {
        tracing::debug!("file logging subscriber already initialised: {e}");
    }
    Ok(Some(guard))
}

/// Resolve the configured log directory honouring the user-mandated
/// precedence ladder: CLI > env (`AI_MEMORY_LOG_DIR`) > `[logging]
/// path` in config > platform default. The `cfg`-only entry point is
/// kept for callers that don't have a CLI override; subcommand wiring
/// uses [`resolve_log_dir_with_override`] directly.
///
/// Falls back to a best-effort default if the security guard rejects
/// the configured path — the `init_file_logging` path will then re-run
/// the strict resolver and surface the error to the operator.
#[must_use]
pub fn resolve_log_dir(cfg: &LoggingConfig) -> PathBuf {
    log_paths::resolve_log_dir(None, cfg.path.as_deref())
        .map(|r| r.path)
        .unwrap_or_else(|_| log_paths::platform_default(log_paths::DirKind::Log).path)
}

/// Strict version: returns the [`log_paths::ResolvedDir`] so callers
/// can surface the resolution layer in error messages, and propagates
/// the world-writable-refusal error.
///
/// # Errors
/// - Resolved path is world-writable.
pub fn resolve_log_dir_with_override(
    cli_override: Option<&Path>,
    cfg: &LoggingConfig,
) -> Result<log_paths::ResolvedDir> {
    log_paths::resolve_log_dir(cli_override, cfg.path.as_deref())
}

/// Build the rolling file appender with the rotation policy from
/// `cfg`. Defaults to daily rotation with `max_files` retained on
/// disk.
pub fn build_appender(dir: &Path, cfg: &LoggingConfig) -> Result<RollingFileAppender> {
    let rotation = rotation_for(cfg);
    let max_files = cfg.max_files.unwrap_or(30);
    let prefix = cfg
        .filename_prefix
        .clone()
        .unwrap_or_else(|| DEFAULT_PREFIX.to_string());

    RollingFileAppender::builder()
        .filename_prefix(prefix)
        .rotation(rotation)
        .max_log_files(max_files)
        .build(dir)
        .with_context(|| format!("building rolling appender at {}", dir.display()))
}

fn rotation_for(cfg: &LoggingConfig) -> Rotation {
    match cfg.rotation.as_deref().unwrap_or("daily") {
        "minutely" => Rotation::MINUTELY,
        "hourly" => Rotation::HOURLY,
        "never" => Rotation::NEVER,
        _ => Rotation::DAILY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_for_default_is_daily() {
        let cfg = LoggingConfig::default();
        // Rotation enum doesn't impl PartialEq, so format-compare.
        let r = rotation_for(&cfg);
        assert!(format!("{r:?}").to_lowercase().contains("daily"));
    }

    #[test]
    fn rotation_for_hourly() {
        let cfg = LoggingConfig {
            rotation: Some("hourly".to_string()),
            ..Default::default()
        };
        let r = rotation_for(&cfg);
        assert!(format!("{r:?}").to_lowercase().contains("hourly"));
    }

    #[test]
    fn resolve_log_dir_default_under_home() {
        let cfg = LoggingConfig::default();
        let p = resolve_log_dir(&cfg);
        // Default contains the well-known suffix even on bare-min
        // home setups.
        assert!(p.to_string_lossy().contains("ai-memory"));
    }

    #[test]
    fn build_appender_creates_file_under_tmp() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = LoggingConfig {
            enabled: Some(true),
            path: Some(tmp.path().to_string_lossy().into_owned()),
            rotation: Some("never".to_string()),
            ..Default::default()
        };
        let _appender = build_appender(tmp.path(), &cfg).unwrap();
        // The appender lazily creates the log file on first write. Just
        // ensure construction succeeded and the dir is writable.
        assert!(tmp.path().is_dir());
    }

    #[test]
    fn init_file_logging_returns_none_when_disabled() {
        let cfg = LoggingConfig {
            enabled: Some(false),
            ..Default::default()
        };
        let guard = init_file_logging(&cfg).unwrap();
        assert!(guard.is_none());
    }
}
