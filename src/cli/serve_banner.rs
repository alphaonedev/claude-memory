// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Round-2 F8 + F12 — `ai-memory serve` startup banner.
//!
//! The banner is the operator-facing summary of the daemon's posture
//! at boot: which permissions mode is live, whether the migration
//! warning fired, and whether an Ed25519 signing keypair was
//! auto-generated. The text is composed by pure functions in this
//! module so the daemon's `serve()` body in `daemon_runtime.rs` can
//! call them, and the unit-test suite can assert on the rendered
//! lines without spinning up the full HTTP server.
//!
//! ## Public surface
//!
//! - [`compose_banner`] — render the banner lines for a given
//!   [`BannerInputs`] tuple. Stable across versions; new lines are
//!   appended, never reordered.
//! - [`BannerInputs`] / [`BannerLine`] — the input + output shapes.
//!
//! Mechanical wiring of [`compose_banner`] into the daemon's
//! `tracing::info!` stream is left to the integrator — see
//! `daemon_runtime::serve` for the call-site.

use crate::config::PermissionsMode;
use crate::permissions::{resolve_v07_default_mode, startup_banner_line};

/// Inputs to the banner composer. All fields are derived from
/// `AppConfig` and the runtime keypair-bootstrap result; the composer
/// itself is pure and side-effect free.
#[derive(Debug, Clone)]
pub struct BannerInputs {
    /// `Some(mode)` when the operator explicitly set
    /// `[permissions].mode` in `config.toml`. `None` when the field is
    /// absent (the F8 migration warning fires in this case).
    pub configured_permissions_mode: Option<PermissionsMode>,
    /// `Some(path)` when the F12 keypair-autogen path created a fresh
    /// keypair this boot. `None` when one already existed or the
    /// auto-gen was disabled by `[identity].disabled = true`.
    pub auto_generated_keypair_path: Option<String>,
    /// `true` when the operator has set `[identity].disabled = true`
    /// in config — the daemon emits a single line acknowledging the
    /// opt-out so an unsigned-link deployment is intentional, not
    /// silent.
    pub identity_disabled: bool,
}

/// One rendered banner line, tagged by severity. The daemon maps
/// `Info` → `tracing::info!` and `Warn` → `tracing::warn!` so the
/// migration notice surfaces in operator dashboards as a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BannerLine {
    /// `tracing::info!`-level line.
    Info(String),
    /// `tracing::warn!`-level line. F8 migration warning + F12
    /// "consider backing up" both ride this lane.
    Warn(String),
}

impl BannerLine {
    /// Body of the line, regardless of severity. Useful in tests.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            BannerLine::Info(s) | BannerLine::Warn(s) => s,
        }
    }

    /// `true` if the line is `Warn`.
    #[must_use]
    pub fn is_warn(&self) -> bool {
        matches!(self, BannerLine::Warn(_))
    }
}

/// Compose the v0.7.0 startup banner.
///
/// Always emits at least the `permissions: <mode>` line (F8 banner
/// requirement). Conditionally appends the migration warning, the
/// auto-gen-keypair line, and the identity-disabled acknowledgement.
///
/// The composer never panics and never performs I/O — the daemon's
/// `serve()` body is responsible for routing each [`BannerLine`] to
/// `tracing` (or to a captured buffer in tests).
#[must_use]
pub fn compose_banner(inputs: &BannerInputs) -> Vec<BannerLine> {
    let mut out: Vec<BannerLine> = Vec::new();

    // F8 — resolve the effective mode and the migration warning (if
    // any) using the canonical helper in `permissions.rs`.
    let (mode, migration_warning) = resolve_v07_default_mode(inputs.configured_permissions_mode);
    out.push(BannerLine::Info(startup_banner_line(mode)));
    if let Some(w) = migration_warning {
        out.push(BannerLine::Warn(w));
    }

    // F12 — surface keypair-autogen result. Only one of the two
    // branches fires (auto-gen vs. disabled); when neither fires the
    // pre-existing keypair was re-used and we stay silent so the
    // banner doesn't grow on every boot.
    if let Some(path) = &inputs.auto_generated_keypair_path {
        out.push(BannerLine::Warn(format!(
            "auto-generated identity keypair at {path} — consider backing up"
        )));
    } else if inputs.identity_disabled {
        out.push(BannerLine::Info(
            "identity: disabled in config — link signing skipped".to_string(),
        ));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_unconfigured_mode_emits_enforce_and_warning() {
        let lines = compose_banner(&BannerInputs {
            configured_permissions_mode: None,
            auto_generated_keypair_path: None,
            identity_disabled: false,
        });
        // First line must be the permissions banner at info level.
        assert_eq!(lines[0], BannerLine::Info("permissions: enforce".into()));
        // Second line must be the migration warning.
        assert!(lines[1].is_warn(), "expected warn line, got {:?}", lines[1]);
        assert!(
            lines[1]
                .message()
                .contains("v0.7.0 default changed to enforce")
        );
        // No keypair / disabled lines when neither input is set.
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn banner_configured_advisory_skips_migration_warning() {
        let lines = compose_banner(&BannerInputs {
            configured_permissions_mode: Some(PermissionsMode::Advisory),
            auto_generated_keypair_path: None,
            identity_disabled: false,
        });
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], BannerLine::Info("permissions: advisory".into()));
    }

    #[test]
    fn banner_configured_enforce_skips_migration_warning() {
        let lines = compose_banner(&BannerInputs {
            configured_permissions_mode: Some(PermissionsMode::Enforce),
            auto_generated_keypair_path: None,
            identity_disabled: false,
        });
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], BannerLine::Info("permissions: enforce".into()));
    }

    #[test]
    fn banner_includes_auto_gen_keypair_line() {
        let lines = compose_banner(&BannerInputs {
            configured_permissions_mode: Some(PermissionsMode::Enforce),
            auto_generated_keypair_path: Some("/tmp/k.priv".into()),
            identity_disabled: false,
        });
        // permissions: enforce + the keypair warning.
        assert_eq!(lines.len(), 2);
        assert!(lines[1].is_warn());
        let msg = lines[1].message();
        assert!(
            msg.contains("auto-generated identity keypair at /tmp/k.priv"),
            "got: {msg}"
        );
        assert!(msg.contains("consider backing up"));
    }

    #[test]
    fn banner_identity_disabled_emits_info_line_when_no_autogen() {
        let lines = compose_banner(&BannerInputs {
            configured_permissions_mode: Some(PermissionsMode::Enforce),
            auto_generated_keypair_path: None,
            identity_disabled: true,
        });
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[1],
            BannerLine::Info("identity: disabled in config — link signing skipped".to_string())
        );
    }

    #[test]
    fn banner_identity_disabled_yields_to_autogen_line() {
        // If both flags are somehow set (operator disabled identity but
        // the bootstrap path produced a keypair anyway — defensive)
        // the auto-gen line wins because it's the load-bearing event.
        let lines = compose_banner(&BannerInputs {
            configured_permissions_mode: Some(PermissionsMode::Enforce),
            auto_generated_keypair_path: Some("/tmp/k.priv".into()),
            identity_disabled: true,
        });
        assert_eq!(lines.len(), 2);
        assert!(
            lines[1]
                .message()
                .contains("auto-generated identity keypair")
        );
    }
}
