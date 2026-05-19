// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! CLI command modules. Wave 5a (v0.6.3) extracted these out of
//! `main.rs` so each handler can be unit-tested by capturing output
//! into a `Vec<u8>` via `CliOutput` instead of literal `println!`s.
//!
//! ## Public surface
//!
//! - `CliOutput` (re-exported at `cli::CliOutput`): output abstraction.
//! - `helpers::{id_short, auto_namespace, human_age}`: pure helpers.
//! - `store::run`, `update::run`, `io::{export, import, mine}`:
//!   handler entry points called by `main.rs`'s dispatch arm.
//!
//! Each handler takes `&mut CliOutput<'_>` and routes every emit
//! through `writeln!` so tests can assert on captured bytes.

pub mod agents;
pub mod archive;
pub mod audit;
pub mod backup;
pub mod boot;
/// v0.7.0 QW-1 — new-format CLI command modules (return exit codes
/// rather than calling `process::exit`).
pub mod commands;
pub mod consolidate;
pub mod crud;
pub mod curator;
pub mod doctor;
/// v0.7.0 L2-5 (issue #670) — `ai-memory export-forensic-bundle` and
/// `ai-memory verify-forensic-bundle` subcommands.
pub mod export;
pub mod forget;
pub mod gc;
pub mod governance;
/// v0.7.0 issue #863 — `ai-memory governance check-action` subcommand.
/// Shell-side parity for the MCP tool `memory_check_agent_action` so
/// operators can dry-run a substrate rule from a terminal without
/// driving JSON-RPC over stdio.
pub mod governance_check_action;
/// v0.7.0 7th-form (issue #760) — `ai-memory governance install-defaults`
/// subcommand. Bulk-flip seed rules R001-R004 to `enabled = 1` after
/// operator confirmation (interactive prompt; `--yes` overrides).
pub mod governance_install_defaults;
pub mod governance_migrate;
pub mod helpers;
pub mod identity;
pub mod install;
pub mod io;
pub mod io_writer;
pub mod link;
pub mod logs;
/// v0.7.0 (issue #800) — `ai-memory namespace` subcommand. CRUD over
/// the per-namespace standard policy memory pointer. Closes Crack 1
/// from the Batman Mode acceptance review by giving operators a
/// first-class CLI verb instead of forcing them into an MCP-stdio
/// JSON-RPC dance just to bind a `GovernancePolicy` to a namespace.
pub mod namespace;
/// v0.7.0 QW-3 — `ai-memory offload` / `ai-memory deref` subcommands.
/// Substrate-only wrappers over `crate::offload::ContextOffloader`.
pub mod offload;
pub mod promote;
pub mod recall;
/// v0.7.0 (issue #691) — `ai-memory rules` subcommand. CRUD for the
/// substrate-level agent-action rules engine. Mutation verbs (add /
/// enable / disable / remove) require the operator keypair on disk.
pub mod rules;
#[cfg(feature = "sal")]
pub mod schema_init;
pub mod search;
pub mod serve_banner;
pub mod shell;
pub mod store;
pub mod sync;
pub mod update;
pub mod verify;
pub mod verify_signed_events;
pub mod wrap;

#[cfg(test)]
pub mod test_utils;

// Convenience re-export so callers can `use ai_memory::cli::CliOutput`
// without a deeper path.
pub use io_writer::CliOutput;
