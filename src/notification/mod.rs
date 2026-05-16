// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L2-3 (issue #668) — Reflection invalidation propagation.
//!
//! When a Reflection→Reflection `supersedes` edge lands, the substrate
//! walks every memory that `reflects_on` the now-invalidated reflection
//! and writes a notification memory into `<namespace>/_invalidations`.
//! Operators (or the curator) inspect those notifications and decide
//! whether to re-reflect, supersede, or leave the dependent untouched
//! — the propagation is **notification, not cascade**.
//!
//! The split into a free-standing top-level module (rather than
//! folding the helper into `storage::` or `mcp::tools::`) keeps the
//! invalidation walker decoupled from both the storage shape and the
//! MCP wire surface. The walker takes a `&Connection` and a small
//! `InvalidationContext` struct; callers compose it however they like
//! (today: the `memory_link` handler in `mcp::tools::link`, the new
//! `memory_dependents_of_invalidated` MCP tool in `mcp::tools`).

pub mod invalidation;
