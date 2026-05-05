// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7 Track G — programmable lifecycle hook pipeline.
//
// This module is the substrate for tasks G1-G11 of the
// `attested-cortex` epic. G1 lands the configuration schema
// (`hooks.toml`) and the SIGHUP-driven hot-reload plumbing.
// Subsequent tasks layer on:
//
//   * G2  — full payload structs for the 20 lifecycle events.
//   * G3  — subprocess executor (exec + daemon modes).
//   * G4  — `HookDecision` contract.
//   * G5  — chain ordering with first-deny-wins short-circuit.
//   * G6  — per-event-class deadlines.
//   * G7+ — actual firing at the memory operation points.
//
// G1 deliberately ships *only* the schema + loader + hot-reload
// signal handler. It does not fire hooks, validate command paths
// against a sandbox, or implement the executor. Those land in
// follow-up PRs on this same `feat/v0.7-g-*` track.

pub mod config;
pub mod decision;
pub mod events;
pub mod executor;

// G2 lifted `HookEvent` out of `config.rs` into `events.rs` and
// attached payload structs to every variant. The re-export keeps
// G1's `use crate::hooks::HookEvent` (and the
// `crate::hooks::config::HookEvent` compatibility alias) resolving.
pub use config::{HookConfig, HookMode, HooksConfigError};
pub use events::HookEvent;
// G4 — full HookDecision contract. G3 shipped a local `Allow +
// Deny` prototype inside `executor.rs`; G4 lifts the type into
// `decision.rs` with the four-variant epic spec (Allow / Modify /
// Deny / AskUser) and a strict JSON wire contract. The re-export
// here keeps G3 call sites (`use crate::hooks::HookDecision`,
// `use crate::hooks::executor::HookDecision`) resolving via the
// canonical `crate::hooks::decision::HookDecision` path.
pub use decision::{DecisionParseError, HookDecision, ModifyPayload, is_pre_event};
// G3 — subprocess hook executor. Re-exports keep call sites
// (`use crate::hooks::HookExecutor`) tidy without requiring every
// caller to know the `executor::` submodule path.
pub use executor::{
    DaemonExecutor, ExecExecutor, ExecutorError, ExecutorMetrics, ExecutorRegistry, HookExecutor,
};
