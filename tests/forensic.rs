// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Integration-test entry point for the v0.7.0 L2-5 forensic
//! evidence bundle (issue #670).
//!
//! Cargo autodiscovers `tests/forensic.rs` as a single test binary;
//! the `mod bundle_test` declaration below pulls in the acceptance
//! tests from `tests/forensic/bundle_test.rs` the same way
//! `tests/transcripts.rs` mounts `tests/transcripts/replay_test.rs`.

#[path = "forensic/bundle_test.rs"]
mod bundle_test;

#[path = "forensic/wt1e_chain_test.rs"]
mod wt1e_chain_test;
