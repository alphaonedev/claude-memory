// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L2-5 (issue #670) — `ai-memory export-forensic-bundle` and
//! `ai-memory verify-forensic-bundle` CLI surface.
//!
//! Thin dispatch wrappers over [`crate::forensic::bundle`]. The heavy
//! lifting (substrate reads, tar assembly, signature creation /
//! verification) lives in the substrate module so it can be exercised
//! from unit tests without spawning a subprocess. This module exists
//! solely to keep the `Command` enum tidy and to make the two verbs
//! discoverable under `src/cli/`.

use std::path::Path;

use anyhow::Result;

use crate::cli::CliOutput;
use crate::forensic::bundle::{
    ExportForensicBundleArgs, VerifyForensicBundleArgs, run_export, run_verify,
};

/// Dispatch `ai-memory export-forensic-bundle`. See
/// [`run_export`](crate::forensic::bundle::run_export).
///
/// # Errors
///
/// Propagates DB / I/O / signing errors from the substrate.
pub fn export(
    db_path: &Path,
    args: &ExportForensicBundleArgs,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    run_export(db_path, args, out)
}

/// Dispatch `ai-memory verify-forensic-bundle`. See
/// [`run_verify`](crate::forensic::bundle::run_verify).
///
/// # Errors
///
/// Propagates I/O / parse errors. Verification failure returns
/// `Ok(non-zero)` rather than an `Err`.
pub fn verify(args: &VerifyForensicBundleArgs, out: &mut CliOutput<'_>) -> Result<i32> {
    run_verify(args, out)
}
