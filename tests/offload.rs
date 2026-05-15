// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Integration-test entry point for the v0.7.0 QW-3 context-offload
//! substrate primitive.
//!
//! Cargo autodiscovers `tests/offload.rs` as a single test binary;
//! the `mod` declarations below pull in the acceptance tests from
//! `tests/offload/*.rs`. Mirrors the `tests/kg.rs` pattern.

#[path = "offload/acceptance.rs"]
mod acceptance;
