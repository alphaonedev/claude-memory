// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Round-2 F8 — `permissions.mode` defaults to `enforce` in v0.7.0.
//!
//! These tests pin the F8 contract: a fresh deployment with no
//! `[permissions]` block in `config.toml` must boot with `enforce`,
//! NOT `advisory`. The migration warning surfaces once at boot so an
//! upgrader who depended on advisory's non-blocking posture sees the
//! change in their daemon logs.
//!
//! Wiring of the helper into the real `serve()` startup path lives in
//! `daemon_runtime.rs`; this file targets the pure functions in
//! `crate::permissions` + `crate::cli::serve_banner` so the assertion
//! does not require booting an HTTP server.

use ai_memory::cli::serve_banner::{BannerInputs, BannerLine, compose_banner};
use ai_memory::config::PermissionsMode;
use ai_memory::permissions::{
    default_v07_secure_mode, resolve_v07_default_mode, startup_banner_line,
};

#[test]
fn fresh_config_with_no_explicit_mode_defaults_to_enforce() {
    // The "fresh permissions config with no explicit mode" case: the
    // operator's `config.toml` did not contain a `[permissions]`
    // block at all (or contained one without `mode = `). The F8
    // resolver must return Enforce.
    let (mode, _warning) = resolve_v07_default_mode(None);
    assert_eq!(mode, PermissionsMode::Enforce);
    assert_eq!(default_v07_secure_mode(), PermissionsMode::Enforce);
}

#[test]
fn unconfigured_mode_emits_migration_warning_text() {
    let (_mode, warning) = resolve_v07_default_mode(None);
    let w = warning.expect("migration warning must fire when mode is unset");
    // Exact phrasing from the F8 spec.
    assert!(w.contains("v0.7.0 default changed to enforce"), "got: {w}");
    assert!(w.contains("permissions.mode=advisory"), "got: {w}");
    assert!(w.contains("release notes"), "got: {w}");
}

#[test]
fn explicit_advisory_disables_migration_warning_and_is_respected() {
    // Operator opts into advisory: no warning fires, the mode is
    // honored verbatim.
    let (mode, warning) = resolve_v07_default_mode(Some(PermissionsMode::Advisory));
    assert_eq!(mode, PermissionsMode::Advisory);
    assert!(warning.is_none(), "no warning when operator opted in");
}

#[test]
fn explicit_off_is_respected() {
    let (mode, warning) = resolve_v07_default_mode(Some(PermissionsMode::Off));
    assert_eq!(mode, PermissionsMode::Off);
    assert!(warning.is_none());
}

#[test]
fn startup_banner_states_active_mode() {
    assert_eq!(
        startup_banner_line(PermissionsMode::Enforce),
        "permissions: enforce"
    );
    assert_eq!(
        startup_banner_line(PermissionsMode::Advisory),
        "permissions: advisory"
    );
    assert_eq!(
        startup_banner_line(PermissionsMode::Off),
        "permissions: off"
    );
}

#[test]
fn compose_banner_unconfigured_emits_enforce_banner_plus_warning() {
    let lines = compose_banner(&BannerInputs {
        configured_permissions_mode: None,
        auto_generated_keypair_path: None,
        identity_disabled: false,
    });
    // Order is stable: banner first, warning second.
    assert!(lines.len() >= 2, "expected >=2 lines, got {lines:?}");
    assert_eq!(lines[0], BannerLine::Info("permissions: enforce".into()));
    assert!(
        lines[1].is_warn(),
        "second line must be the warn-level migration notice"
    );
    assert!(
        lines[1]
            .message()
            .contains("v0.7.0 default changed to enforce")
    );
}

#[test]
fn compose_banner_explicit_advisory_omits_migration_warning() {
    let lines = compose_banner(&BannerInputs {
        configured_permissions_mode: Some(PermissionsMode::Advisory),
        auto_generated_keypair_path: None,
        identity_disabled: false,
    });
    // Just the permissions banner — no warning, no keypair noise.
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0], BannerLine::Info("permissions: advisory".into()));
}
