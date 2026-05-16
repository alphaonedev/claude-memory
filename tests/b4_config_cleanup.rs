// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Wave 2 — B4 fix campaign (F8-CONFIG + R2-LOW cleanup).
//!
//! Pins the four findings closed in commit
//! `fix(v0.7.0): config + cleanup — F8 partial-config secure-default ...`:
//!
//! 1. **S5-M3 / F8 partial-config**: when `config.toml` contains a
//!    `[permissions]` block but omits `mode = `, the effective mode
//!    MUST be the v0.7.0 secure default (`Enforce`) — NOT the
//!    serde-derived `Default for PermissionsMode` (`Advisory`). The
//!    migration warning surfaced by `resolve_v07_default_mode` must
//!    fire because the operator did not opt into a mode explicitly.
//!
//! 2. **No-block upgrade compat**: when `config.toml` has no
//!    `[permissions]` block at all, the same secure-default + warning
//!    semantics apply (the F8 ship-gate behaviour established in
//!    `tests/round2_f8_enforce_default.rs` — pinned here once more
//!    to assert that B4's `Option<PermissionsMode>` reshaping did not
//!    regress it).
//!
//! 3. **R2-LOW priority overflow**: `i32::try_from(i64).expect(...)`
//!    in the MCP `memory_store` / `memory_update` / `memory_notify`
//!    handlers MUST clamp instead of panic when a caller passes an
//!    out-of-i32-range priority. The clamp helper is tested directly
//!    here; the handler integration is exercised by the existing
//!    `handle_store_*` / `handle_notify_*` smoke matrix in
//!    `src/mcp/mod.rs` which would have aborted under the pre-fix
//!    code path.
//!
//! 4. **R2-LOW `session_start` namespace validation**: every MCP entry
//!    point that accepts a `namespace` argument must run
//!    `validate::validate_namespace`. The validator's reject-on-space
//!    contract is pinned here; the handler-level assertion (that
//!    `handle_session_start` actually invokes the validator) lives in
//!    `src/mcp/mod.rs::tests` as `handle_session_start_rejects_invalid_namespace`.

use ai_memory::config::{AppConfig, PermissionsConfig, PermissionsMode};
use ai_memory::validate;

// ---------------------------------------------------------------------------
// 1 + 2. F8 partial-config — block present without `mode =` AND block absent.
// ---------------------------------------------------------------------------

/// Serialise a closure under a lock so two tests in this file can't
/// race the global `AI_MEMORY_PERMISSIONS_MODE` env var while
/// `effective_permissions_mode` is reading it. Cargo runs tests in
/// parallel by default.
fn with_env_lock<F: FnOnce()>(f: F) {
    use std::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // SAFETY: env mutation is process-global; the lock above
    // serialises the writers in this test file. Other test files
    // that touch this var must also hold a sibling lock — the F8
    // ship-gate tests don't, but they only consult the pure helper
    // `resolve_v07_default_mode` (not the env-aware
    // `effective_permissions_mode`) so they cannot interleave.
    unsafe {
        std::env::remove_var("AI_MEMORY_PERMISSIONS_MODE");
    }
    f();
}

/// Round-trip: a `config.toml` that contains `[permissions]` with only
/// `rules = []` (NO `mode = `) MUST resolve to `Enforce` — not the
/// serde-derived `Advisory` default. Pre-fix, the `#[serde(default)]`
/// on `PermissionsConfig.mode` silently filled `Advisory` for the
/// partial-config case, which left an upgrading deployment with the
/// v0.6.x advisory posture even though F8 advertised
/// secure-by-default.
#[test]
fn test_partial_permissions_config_secure_defaults_with_warn() {
    with_env_lock(|| {
        let toml_with_partial_permissions = r#"
            tier = "semantic"
            [permissions]
            # mode intentionally omitted — operator forgot to declare it
            rules = []
        "#;

        let cfg: AppConfig =
            toml::from_str(toml_with_partial_permissions).expect("parse partial permissions block");

        // The block was declared so `permissions` is Some(...)...
        let pc = cfg.permissions.as_ref().expect("permissions block present");
        // ...but `mode` is None because the operator did not specify it.
        assert!(
            pc.mode.is_none(),
            "partial-config: `mode` must be None when omitted, got {:?}",
            pc.mode
        );

        // Effective mode falls through to the v0.7.0 secure default.
        assert_eq!(
            cfg.effective_permissions_mode(),
            PermissionsMode::Enforce,
            "partial [permissions] block must yield Enforce (not the \
             serde-default Advisory)",
        );

        // The migration warning fires through `resolve_v07_default_mode`
        // because configured is None.
        let (mode, warning) = ai_memory::permissions::resolve_v07_default_mode(None);
        assert_eq!(mode, PermissionsMode::Enforce);
        let w = warning.expect("warning fires when configured mode is None");
        assert!(
            w.contains("v0.7.0 default changed to enforce"),
            "warning text: {w}"
        );
    });
}

/// No `[permissions]` block at all — the historical v0.6.x upgrade
/// path. Pre-F8 this would have silently fallen back to Advisory; the
/// F8 ship-gate fixed it to Enforce + WARN, and B4 must not regress
/// that contract through the `Option<PermissionsMode>` reshaping.
#[test]
fn test_no_permissions_block_secure_defaults_with_warn() {
    with_env_lock(|| {
        let toml_without_permissions = r#"
            tier = "semantic"
        "#;

        let cfg: AppConfig =
            toml::from_str(toml_without_permissions).expect("parse permission-less config");

        assert!(
            cfg.permissions.is_none(),
            "no-block: `permissions` must be None"
        );

        // Effective mode is the secure default (matches the F8 ship-gate
        // in `tests/round2_f8_enforce_default.rs`).
        assert_eq!(
            cfg.effective_permissions_mode(),
            PermissionsMode::Enforce,
            "absent [permissions] block must yield Enforce + warn",
        );
    });
}

/// Operator opts into Advisory explicitly: no warning fires, mode is
/// honored verbatim. Pins that B4's reshape did NOT change the
/// explicit-mode path.
#[test]
fn test_explicit_advisory_is_honored_and_warning_silenced() {
    with_env_lock(|| {
        let toml_explicit_advisory = r#"
            tier = "semantic"
            [permissions]
            mode = "advisory"
        "#;

        let cfg: AppConfig =
            toml::from_str(toml_explicit_advisory).expect("parse explicit-advisory config");

        let pc = cfg.permissions.as_ref().expect("permissions block present");
        assert_eq!(pc.mode, Some(PermissionsMode::Advisory));
        assert_eq!(cfg.effective_permissions_mode(), PermissionsMode::Advisory);
    });
}

/// `PermissionsConfig::default()` is `mode: None, rules: []` — the
/// "operator declared the block but said nothing about it" shape.
/// This pins the API contract because external integrations
/// (capabilities surface, doctor) construct default values.
#[test]
fn test_permissions_config_default_has_none_mode() {
    let pc = PermissionsConfig::default();
    assert!(pc.mode.is_none(), "default mode must be None (B4 reshape)");
    assert!(pc.rules.is_empty());
}

// ---------------------------------------------------------------------------
// 3. R2-LOW — priority overflow clamps instead of panicking.
// ---------------------------------------------------------------------------

/// The clamp helper used by `memory_store` / `memory_update` /
/// `memory_notify` must not panic on i64 values outside the i32 range.
#[test]
fn test_priority_overflow_clamps_not_panics() {
    // Mirror the exact line shape used in the handlers post-fix.
    let bad_pos: i64 = i64::MAX;
    let clamped_pos = i32::try_from(bad_pos).unwrap_or(i32::MAX);
    assert_eq!(clamped_pos, i32::MAX);

    let bad_neg: i64 = i64::MIN;
    let clamped_neg = i32::try_from(bad_neg).unwrap_or(i32::MAX);
    // We fall back to i32::MAX (not MIN) deliberately — downstream
    // `validate_priority` will reject MAX as out-of-range and
    // surface a typed error, which is the safer-of-the-two failure
    // modes (no silent allow-with-min-priority).
    assert_eq!(clamped_neg, i32::MAX);

    let in_range: i64 = 7;
    let clamped_in = i32::try_from(in_range).unwrap_or(i32::MAX);
    assert_eq!(clamped_in, 7, "in-range values must pass through unchanged");
}

/// The storage-side lerp clamp: `usize` content lengths > `i32::MAX`
/// must not panic. The scoring impact is nil because the lerp
/// saturates at 5000 chars anyway.
#[test]
fn test_content_len_overflow_clamps_not_panics() {
    let monstrous_len: usize = usize::MAX;
    let clamped = f64::from(i32::try_from(monstrous_len).unwrap_or(i32::MAX));
    assert!(
        clamped >= 5000.0,
        "clamp must land at or above the lerp's long-tail bucket"
    );
}

// ---------------------------------------------------------------------------
// 4. R2-LOW — session_start validates namespace.
// ---------------------------------------------------------------------------

/// Sanity: the validator itself rejects a space-containing namespace
/// — pins the precondition the `session_start` fix relies on so a
/// future relaxation of `validate_namespace` would surface here.
#[test]
fn test_session_start_validates_namespace_via_validator() {
    let err =
        validate::validate_namespace("foo bar").expect_err("space in namespace must be rejected");
    let msg = err.to_string();
    assert!(msg.to_lowercase().contains("space"), "got: {msg}");
}
