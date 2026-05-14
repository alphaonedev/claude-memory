// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Integration-test entry point for the v0.7.0 transcripts module
//! (L2-4 reflection-union replay, issue #669).
//!
//! Cargo autodiscovers `tests/transcripts.rs` as a single test binary;
//! the `mod replay_test` declaration below pulls in the acceptance
//! tests from `tests/transcripts/replay_test.rs` exactly the same way
//! `tests/curator.rs` mounts `tests/curator/compaction_test.rs`.

#[path = "transcripts/replay_test.rs"]
mod replay_test;
