// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 WT-1-D — `pre_store` substrate-side hook submodules.
//!
//! Currently houses the auto-atomisation deferred-enqueue hook
//! (`auto_atomise`). Future pre_store plugins land alongside it.
//!
//! The naming `pre_store` follows the WT-1-D brief — the hook is
//! consulted at the memory_store call site BEFORE the response is
//! returned to the caller. The actual curator pass runs on a
//! detached worker thread AFTER the transaction commits (the
//! 100ms delay pinned in the brief is the post-commit visibility
//! window for the substrate's WAL/checkpoint dance) — the deferred
//! pattern matches the L2-1 reflection-pass curator and the QW-1
//! `post_reflect` auto-export hook.

pub mod auto_atomise;
pub mod auto_classify_kind;

pub use auto_atomise::{
    AUTO_ATOMISE_DISPATCH, AutoAtomisationDispatch, AutoAtomisationOutcome,
    install_auto_atomise_dispatch, maybe_enqueue_auto_atomise, run_synchronous_auto_atomise,
};
pub use auto_classify_kind::{classify_by_regex, maybe_auto_classify};
