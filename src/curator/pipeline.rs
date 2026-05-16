// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `CompactionPass` trait — the generic pipeline interface that every
//! compaction strategy (consolidation, reflection, forget-superseded, …)
//! must implement.
//!
//! ## Design contract
//!
//! A `CompactionPass` encapsulates one full lifecycle of compaction:
//!
//! 1. **`cluster`** — partition an input slice of memories into groups
//!    that are candidates for compaction.  Groups with < 2 members are
//!    ignored by callers.
//! 2. **`eligible`** — secondary gate: given an already-formed cluster,
//!    decide whether this pass should actually act on it (e.g. minimum
//!    cluster size, namespace allow-lists, dry-run).
//! 3. **`summarize`** — produce the single consolidated [`Memory`] that
//!    replaces the cluster.  Must NOT write to the database.
//! 4. **`persist`** — atomically write the summary and record the source
//!    ids in the rollback log.
//! 5. **`verify`** — check that the persisted summary is readable and
//!    internally consistent.  A failure here does NOT yet trigger rollback
//!    (rollback is v0.8.0 Pillar 2.5 scope — see issue #664).
//!
//! ## Visibility contract (R7)
//!
//! The trait is `pub(crate)`.  Every helper exposed from this module is
//! at most `pub(super)`.  No new bare `pub` items.
//!
//! ## L2-1 hook
//!
//! `ReflectionPass` (Task L2-1) will `impl CompactionPass` against this
//! trait.  The trait is intentionally small so the reflection engine can
//! plug in with zero changes to the pipeline runner.

use anyhow::Result;

use crate::models::Memory;

/// Type alias used throughout the compaction pipeline.  A memory's
/// stable identifier is its `id` field — a UUID string.
// L1-7: type alias is defined here; external call-sites land in L2-1.
#[allow(dead_code)]
pub(crate) type MemoryId = String;

// ---------------------------------------------------------------------------
// CompactionPass trait
// ---------------------------------------------------------------------------

/// A single, self-contained compaction strategy.
///
/// Implementors live in `src/curator/compaction.rs`
/// (`ConsolidationPass`) and future `src/curator/reflection_pass.rs`
/// (`ReflectionPass`, Task L2-1).  The pipeline runner in
/// `src/curator/compaction.rs` drives the six-step lifecycle.
// L1-7: trait is defined here; the generic runner and L2-1 impl ship next.
#[allow(dead_code)]
pub(crate) trait CompactionPass {
    /// Human-readable name used in log messages and rollback entries.
    fn name(&self) -> &str;

    /// Partition `memories` into groups of candidates.  Groups with fewer
    /// than 2 members are skipped by the pipeline runner.  The partition
    /// strategy is pass-specific (Jaccard keyword overlap, cosine
    /// similarity, recall co-occurrence, …).
    fn cluster(&self, memories: &[Memory]) -> Vec<Vec<MemoryId>>;

    /// Secondary eligibility gate.  Called after `cluster` with a
    /// fully-resolved cluster (all members already fetched from the DB).
    /// Returns `true` iff the pass should act on this cluster now.
    fn eligible(&self, cluster: &[Memory]) -> bool;

    /// Produce the consolidated [`Memory`] from `cluster`.  Must NOT
    /// touch the database — side-effect-free except for LLM calls.
    ///
    /// # Errors
    ///
    /// Returns an error if the LLM call fails or the cluster is
    /// degenerate (empty, single-member, mismatched namespaces).
    fn summarize(&self, cluster: &[Memory]) -> Result<Memory>;

    /// Atomically persist `summary` and record `sources` in the rollback
    /// log.  Called only when `eligible` returned `true` and `summarize`
    /// succeeded.
    ///
    /// # Errors
    ///
    /// Returns an error if the DB write fails.
    fn persist(&self, summary: &Memory, sources: &[MemoryId]) -> Result<()>;

    /// Verify that the persisted summary identified by `summary_id` is
    /// readable and internally consistent.
    ///
    /// A failure here is logged but does NOT yet trigger rollback — that
    /// is deferred to v0.8.0 full Pillar 2.5 scope (issue #664).
    ///
    /// # Errors
    ///
    /// Returns an error if the DB read fails or the summary row is
    /// corrupt / missing.
    fn verify(&self, summary_id: MemoryId) -> Result<()>;
}
