// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Integration-test entry-point for `tests/cli/*` submodules.
//!
//! Cargo autodiscovers `tests/cli.rs` as one test binary; the
//! `#[path]` includes below mount per-feature submodules under it the
//! same way `tests/forensic.rs` mounts `tests/forensic/bundle_test.rs`.

#![allow(clippy::doc_markdown)]

// v0.7.0 QW-1 — file-backed reflection chain export tests.
#[path = "cli/export_reflections.rs"]
mod export_reflections;

// v0.7.0 WT-1-F — `ai-memory atomise` CLI tests.
#[path = "cli/atomise.rs"]
mod atomise;
