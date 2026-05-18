// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 WT-1-E — recall-time atom-preference acceptance suite.
//!
//! Cargo autodiscovers `tests/recall.rs` as a single test binary;
//! the `mod wt1e` declaration below pulls in the acceptance tests
//! from `tests/recall/wt1e.rs`. Mirrors the
//! `tests/forensic.rs` ↔ `tests/forensic/bundle_test.rs` pattern.

#[path = "common/mod.rs"]
mod common;

#[path = "recall/wt1e.rs"]
mod wt1e;
