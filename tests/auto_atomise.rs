// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 WT-1-D — auto-atomisation `pre_store` hook acceptance suite.
//!
//! Cargo autodiscovers `tests/auto_atomise.rs` as a single test binary;
//! the `mod` declaration pulls in the per-aspect acceptance tests
//! under `tests/auto_atomise/`.

#[path = "auto_atomise/core.rs"]
mod core;
