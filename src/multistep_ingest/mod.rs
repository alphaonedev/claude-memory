// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Form 3 — multi-step ingest orchestrator (issue #756).
//!
//! The Batman 6-form audit found Form 3 (deterministic-where-possible +
//! LLM-where-necessary multi-step pipelines with prompt-cache reuse and
//! explicit-trust deterministic helpers) ABSENT. This module is the
//! substrate-level closeout.
//!
//! # Batman exemplars
//!
//! - **Understand-Anything two-phase**: phase one runs a deterministic
//!   helper script (Jaccard overlap / FTS classifier); phase two calls
//!   the LLM with an explicit instruction to TRUST the helper output and
//!   NOT re-run discovery. This module's [`pipeline::two_phase_default`]
//!   reproduces that shape.
//! - **OpenKB four-step**: stages are load_context → classify → enrich →
//!   emit, all sharing a SYSTEM PROMPT prefix so the prompt-cache key
//!   stays stable across stages within a single run. This module's
//!   [`pipeline::four_step_default`] mirrors that contract.
//!
//! # Subsystem layout
//!
//! - [`mod@pipeline`] — `Pipeline`, `Stage`, `HelperKind` types + the two
//!   default pipelines (two-phase + four-step).
//! - [`mod@cache`] — prompt-cache key derivation and the shared-prefix
//!   builder that makes LLM stages within a run cache-compatible.
//! - [`mod@helpers`] — deterministic helper implementations (Jaccard
//!   overlap, cosine pre-filter, FTS classifier).
//! - [`mod@executor`] — the orchestrator: runs helpers first (parallel
//!   where independent), threads outputs into LLM stages through
//!   explicit-trust slots, returns a stage-by-stage trace.
//!
//! # Audit-honest stubs
//!
//! In this initial closeout the LLM client wrapper is the project's
//! existing `OllamaClient`; the executor accepts an arbitrary
//! `LlmDispatch` trait object so tests can wire a deterministic mock
//! (see [`executor::MockLlmDispatch`]). The production binding to
//! `OllamaClient::generate` lives in [`executor::OllamaDispatch`].
//! Cosine pre-filter and FTS classifier helpers operate on the
//! in-memory `MemoryHandle` envelope shipped from the caller, not the
//! full storage layer — Form 3 is a code-only subsystem; schema is
//! untouched per the issue's hard constraint.

pub mod cache;
pub mod executor;
pub mod helpers;
pub mod pipeline;

pub use cache::{CacheKey, PromptCacheTelemetry};
pub use executor::{
    ExecutionTrace, ExecutorError, IngestExecutor, LlmDispatch, MockLlmDispatch, StageOutcome,
};
pub use helpers::{HelperContext, HelperKind, HelperOutput, HelperParams, MemoryHandle};
pub use pipeline::{
    HelperOutputRef, Pipeline, PipelineVariant, Stage, four_step_default, two_phase_default,
};
