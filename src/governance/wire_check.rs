// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Substrate-level agent-action wire-point helper (issue #691 fold-1).
//!
//! L1-6 Deliverable E wired the `Custom("memory_write")` action into
//! `storage::insert*` via the [`crate::storage::GOVERNANCE_PRE_WRITE`]
//! `OnceLock`. The other four agent-external action variants
//! ([`AgentAction::Bash`], [`AgentAction::FilesystemWrite`],
//! [`AgentAction::NetworkRequest`], [`AgentAction::ProcessSpawn`])
//! ship with rule-engine support in
//! [`crate::governance::agent_action::check_agent_action`] but no
//! production wire-points consult that engine outside the storage
//! write path. This module closes the gap.
//!
//! # Wire shape
//!
//! Every daemon-side wire-point — the skill exporter's filesystem
//! writes, the federation client's outbound HTTPS POST, the hooks
//! executor's child-process spawn, and the LLM client's Ollama HTTP
//! — calls a single uniform helper:
//!
//! ```ignore
//! use crate::governance::wire_check;
//! wire_check::check(&action)?;
//! ```
//!
//! The helper consults the process-wide [`GOVERNANCE_PRE_ACTION`]
//! `OnceLock`. When unset (CLI one-shot mode, pre-hook-install daemon
//! path), the call is a zero-cost no-op `Ok(())`. When set, the
//! closure runs and an `Err(reason)` wraps into a
//! [`crate::storage::GovernanceRefusal`] propagated up the `anyhow`
//! chain — the same typed error the storage hook produces, so the
//! existing `MemoryError::from(anyhow::Error)` impl in `errors.rs`
//! handles the 403 / `GOVERNANCE_REFUSED` mapping uniformly.
//!
//! # Layering rationale (mirrors `storage::GOVERNANCE_PRE_WRITE`)
//!
//! 1. **Operator standing directive**: "rules and standards can NEVER
//!    be bypassed by AI/AI Agents — 100% of the time". A `OnceLock`
//!    enforces installation-is-one-shot at the type level — no reset,
//!    no override, no test-only escape hatch reachable from production
//!    code.
//! 2. **Hot path**: hook closure is read on every external action; an
//!    `RwLock` would add contention. `OnceLock::get()` is lock-free.
//! 3. **CLI exemption preserved**: CLI one-shot binaries
//!    (`ai-memory store …`, `ai-memory mine …`, …) MUST NOT install
//!    the hook — the operator's direct ops stay unimpeded. `OnceLock`
//!    defaults to empty, so the CLI path is the no-op default; only
//!    the daemon's `serve` boot reaches the `.set` callsite.
//! 4. **Modular**: every wire-point becomes one line. Adding a new
//!    wire-point (`AgentAction::Bash` for a future shell harness)
//!    needs zero changes here — the helper already dispatches by
//!    `kind()`.

use crate::governance::agent_action::AgentAction;
use crate::storage::GovernanceRefusal;

/// The wire-point hook signature. Returns `Ok(())` on Allow (the
/// action proceeds); `Err(reason)` on Refuse (the wire-point caller
/// surfaces `GovernanceRefusal { reason }` and aborts the action).
///
/// `Warn` and `Log` rule severities map to `Ok(())` — the hook does
/// not block, the audit chain (if installed) captures the warning.
pub type WireCheckHook = Box<dyn Fn(&AgentAction) -> std::result::Result<(), String> + Send + Sync>;

/// Process-wide agent-action wire-point hook. When `Some`, every
/// non-storage agent-external action consults the closure BEFORE the
/// action proceeds; an `Err(reason)` short-circuits the call site
/// with a [`GovernanceRefusal`].
///
/// Installation is one-shot (`OnceLock::set`); the daemon `serve`
/// bootstrap is the only caller in production. CLI one-shot binaries
/// MUST leave this empty.
///
/// See module-level comment for the full layering rationale.
pub static GOVERNANCE_PRE_ACTION: std::sync::OnceLock<WireCheckHook> = std::sync::OnceLock::new();

/// Consult the [`GOVERNANCE_PRE_ACTION`] hook for `action`. When the
/// hook is unset (CLI mode or pre-hook-install daemon path), this is
/// a zero-cost no-op `Ok(())`. When set, the closure runs and an
/// `Err(reason)` wraps into a [`GovernanceRefusal`] propagated up the
/// `anyhow` chain.
///
/// The function is hot-path; avoid heap allocation on the Allow leg.
///
/// # Errors
///
/// Returns [`GovernanceRefusal`] when the installed hook refuses
/// `action`. The `reason` field carries the operator-authored
/// explanation from the matched rule.
#[inline]
pub fn check(action: &AgentAction) -> std::result::Result<(), GovernanceRefusal> {
    if let Some(hook) = GOVERNANCE_PRE_ACTION.get() {
        if let Err(reason) = hook(action) {
            return Err(GovernanceRefusal { reason });
        }
    }
    Ok(())
}

/// Anyhow-chained variant of [`check`] for call sites whose error type
/// is already `anyhow::Error`. Promotes a [`GovernanceRefusal`] into
/// an `anyhow::Error` so the upstream `MemoryError::from(anyhow::Error)`
/// impl in `errors.rs` can downcast and surface 403 / `GOVERNANCE_REFUSED`.
///
/// # Errors
///
/// Returns the same refusal as [`check`], boxed into `anyhow::Error`.
#[inline]
pub fn check_anyhow(action: &AgentAction) -> anyhow::Result<()> {
    if let Err(refusal) = check(action) {
        return Err(anyhow::Error::new(refusal));
    }
    Ok(())
}

/// Test-only helper: install a custom closure into the
/// [`GOVERNANCE_PRE_ACTION`] hook. Returns `Err(())` if the OnceLock
/// is already populated (production must never call this).
///
/// Hidden behind `#[doc(hidden)]` and `#[cfg(any(test, feature = "...
/// test-helpers"))]` to keep production binaries from reaching this
/// surface accidentally. Tests in `tests/governance_wire_points.rs`
/// install a fresh process via `std::process::Command` or rely on the
/// OnceLock's first-write-wins semantics (one test owns the install
/// for the cargo test process; siblings re-use the same hook).
#[doc(hidden)]
#[cfg(test)]
pub fn install_for_test(hook: WireCheckHook) -> std::result::Result<(), ()> {
    GOVERNANCE_PRE_ACTION.set(hook).map_err(|_| ())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The `OnceLock` is process-wide so we can only install one hook
    /// across the whole test binary. To exercise both Allow and Refuse
    /// shapes deterministically the hook routes by the action's
    /// `cwd` / `path` / `host` / `binary` field — a sentinel value
    /// returns `Err`, every other value returns `Ok`. This keeps the
    /// unit tests inside one cargo test invocation without leaking
    /// state across runs.
    fn install_routing_hook() {
        let _ = install_for_test(Box::new(|action: &AgentAction| match action {
            AgentAction::Bash { command, .. } if command.contains("__refuse__") => {
                Err("bash sentinel".to_string())
            }
            AgentAction::FilesystemWrite { path, .. }
                if path.to_string_lossy().contains("__refuse__") =>
            {
                Err("fs sentinel".to_string())
            }
            AgentAction::NetworkRequest { host, .. } if host.contains("__refuse__") => {
                Err("net sentinel".to_string())
            }
            AgentAction::ProcessSpawn { binary, .. } if binary.contains("__refuse__") => {
                Err("spawn sentinel".to_string())
            }
            AgentAction::Custom { custom_kind, .. } if custom_kind.contains("__refuse__") => {
                Err("custom sentinel".to_string())
            }
            _ => Ok(()),
        }));
    }

    #[test]
    fn check_no_hook_installed_is_allow() {
        // If GOVERNANCE_PRE_ACTION is unset (which it is at process
        // start), every check returns Ok. We can't directly test that
        // here because earlier-running tests in the same binary may
        // have installed the routing hook; we instead verify Allow on
        // a non-sentinel value.
        install_routing_hook();
        let action = AgentAction::Bash {
            command: "ls".into(),
            cwd: None,
        };
        assert!(check(&action).is_ok());
    }

    #[test]
    fn check_bash_refuse_path() {
        install_routing_hook();
        let action = AgentAction::Bash {
            command: "echo __refuse__".into(),
            cwd: None,
        };
        let err = check(&action).expect_err("expected refuse");
        assert_eq!(err.reason, "bash sentinel");
        assert!(format!("{err}").contains("governance-refused"));
    }

    #[test]
    fn check_filesystem_write_refuse_path() {
        install_routing_hook();
        let action = AgentAction::FilesystemWrite {
            path: PathBuf::from("/scratch/__refuse__.txt"),
            byte_estimate: None,
        };
        let err = check(&action).expect_err("expected refuse");
        assert_eq!(err.reason, "fs sentinel");
    }

    #[test]
    fn check_network_request_refuse_path() {
        install_routing_hook();
        let action = AgentAction::NetworkRequest {
            host: "__refuse__.example.com".into(),
            scheme: "https".into(),
        };
        let err = check(&action).expect_err("expected refuse");
        assert_eq!(err.reason, "net sentinel");
    }

    #[test]
    fn check_process_spawn_refuse_path() {
        install_routing_hook();
        let action = AgentAction::ProcessSpawn {
            binary: "__refuse__".into(),
            args: vec!["build".into()],
        };
        let err = check(&action).expect_err("expected refuse");
        assert_eq!(err.reason, "spawn sentinel");
    }

    #[test]
    fn check_anyhow_propagates_refusal() {
        install_routing_hook();
        let action = AgentAction::FilesystemWrite {
            path: PathBuf::from("/__refuse__"),
            byte_estimate: None,
        };
        let e = check_anyhow(&action).expect_err("expected refuse");
        let refusal = e
            .downcast_ref::<GovernanceRefusal>()
            .expect("downcast to GovernanceRefusal");
        assert_eq!(refusal.reason, "fs sentinel");
    }

    #[test]
    fn check_allow_path_non_sentinel() {
        install_routing_hook();
        let actions = [
            AgentAction::Bash {
                command: "true".into(),
                cwd: None,
            },
            AgentAction::FilesystemWrite {
                path: PathBuf::from("/Users/x/safe.txt"),
                byte_estimate: Some(0),
            },
            AgentAction::NetworkRequest {
                host: "good.example.com".into(),
                scheme: "https".into(),
            },
            AgentAction::ProcessSpawn {
                binary: "cargo".into(),
                args: vec![],
            },
            AgentAction::Custom {
                custom_kind: "memory_write".into(),
                payload: serde_json::json!({}),
            },
        ];
        for a in &actions {
            assert!(check(a).is_ok(), "expected allow for {:?}", a.kind());
            assert!(check_anyhow(a).is_ok());
        }
    }
}
