// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L2-3 (issue #668) — integration-test entry point for the
//! reflection invalidation propagation walker.
//!
//! Cargo autodiscovers `tests/notification.rs` as a single test
//! binary; the `mod invalidation_test` declaration below pulls in
//! the acceptance tests from `tests/notification/invalidation_test.rs`.

#[path = "notification/invalidation_test.rs"]
mod invalidation_test;
