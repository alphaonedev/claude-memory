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

pub use config::{HookConfig, HookEvent, HookMode, HooksConfigError};
