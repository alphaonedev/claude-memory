// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 WT-1-B — atomisation engine integration tests.
//!
//! Cargo autodiscovers `tests/atomisation.rs` as a single test binary;
//! the `mod` declarations pull in the per-aspect acceptance tests
//! under `tests/atomisation/`.

#[path = "atomisation/core.rs"]
mod core;
