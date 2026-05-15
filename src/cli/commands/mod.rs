// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-1 — new-format CLI command modules.
//!
//! Modules under this directory follow the `pub fn run(db, args,
//! out) -> Result<i32>` shape that returns an exit code (rather than
//! exiting the process from inside the handler). The convention
//! matches `src/cli/export.rs` (forensic bundle) so the dispatch arm
//! in `daemon_runtime::run` stays a one-liner.

/// v0.7.0 WT-1-F — `ai-memory atomise` CLI subcommand.
pub mod atomise;
pub mod export_reflections;
