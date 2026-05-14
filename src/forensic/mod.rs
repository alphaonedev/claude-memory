// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L2-5 — forensic evidence bundle assembly + verification.
//!
//! This module is the OSS surface for the `AgenticMem Attest` tier:
//! procurement-grade evidence packets that travel as a single signed
//! tarball. The CLI verbs at [`crate::cli::export`] are thin wrappers
//! around [`bundle::build`] / [`bundle::verify`]; the heavy lifting
//! lives here so the substrate can be exercised from unit tests
//! without spawning a subprocess.
//!
//! See [`bundle`] for the bundle layout, the manifest schema, the
//! deterministic-tar invariants, and the per-file SHA-256 manifest.

pub mod bundle;
