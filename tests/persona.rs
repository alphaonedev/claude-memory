// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Integration-test entry point for v0.7.0 QW-2 Persona-as-artifact.
//!
//! Cargo autodiscovers `tests/persona.rs` as a single test binary;
//! the `mod` declarations below pull in the acceptance tests from
//! `tests/persona/*.rs`. Mirrors the `tests/offload.rs` pattern.

#[path = "persona/acceptance.rs"]
mod acceptance;
