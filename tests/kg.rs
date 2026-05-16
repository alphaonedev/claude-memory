// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Integration-test entry point for knowledge-graph helpers (v0.7.0 L1-2).
//!
//! Cargo autodiscovers `tests/kg.rs` as a single test binary; the
//! `mod cycle_check_test` declaration below pulls in the cycle-check
//! acceptance tests from `tests/kg/cycle_check_test.rs`.

#[path = "kg/cycle_check_test.rs"]
mod cycle_check_test;
