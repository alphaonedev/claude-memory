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

pub mod helpers;
pub mod io;
pub mod io_writer;
pub mod store;
pub mod update;

#[cfg(test)]
pub mod test_utils;

// Convenience re-export so callers can `use ai_memory::cli::CliOutput`
// without a deeper path.
pub use io_writer::CliOutput;
