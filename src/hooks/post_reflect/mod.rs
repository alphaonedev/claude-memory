// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-1 — `post_reflect` substrate-side hook submodules.
//!
//! Currently houses the file-backed reflection-chain export hook
//! (`auto_export`). Future post_reflect plugins land alongside it.

pub mod auto_export;
// v0.7.0 QW-2 — auto-persona-regeneration cadence hook. Fires on
// reflection writes when the namespace policy
// `auto_persona_trigger_every_n_memories` is set.
pub mod auto_persona;

pub use auto_export::{AutoExportConfig, build_post_reflect_hook};
pub use auto_persona::{AutoPersonaConfig, run_auto_persona};
