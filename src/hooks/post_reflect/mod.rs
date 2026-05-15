// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-1 — `post_reflect` substrate-side hook submodules.
//!
//! Currently houses the file-backed reflection-chain export hook
//! (`auto_export`). Future post_reflect plugins land alongside it.

pub mod auto_export;

pub use auto_export::{AutoExportConfig, build_post_reflect_hook};
