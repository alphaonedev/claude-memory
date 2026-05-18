// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! HTTP handler module index. Per-domain handler code lives in the
//! sibling sub-modules; this file is the public-facing re-export
//! surface plus the inline test scaffolding.
//!
//! Issue #650 history: the original `src/handlers.rs` was an 18 574-line
//! monolith. The first split (commit `7f3f676`) carved off
//! `federation_receive`, `hook_subscribers`, `http`, and `transport`.
//! This module finishes the job by extracting the four remaining
//! production groupings (`errors`, `system`, `parity`, `approvals`)
//! into their own files and relocating the 13 900-line inline test
//! scaffold into [`tests`].
//!
//! Sub-modules:
//!
//! - [`transport`]   — `AppState`, `Db`, auth middleware, shared
//!   constants, low-level helpers (`constant_time_eq`, `MAX_BULK_SIZE`,
//!   `BULK_FANOUT_CONCURRENCY`).
//! - [`http`]        — the bulk of HTTP endpoints (memories CRUD,
//!   recall, governance, federation read paths, archive, skills, KG,
//!   subscriptions, etc.). Further per-domain decomposition is tracked
//!   as a follow-up.
//! - [`federation_receive`] — federation push/since/quorum receive-side
//!   handlers (peer-attested).
//! - [`hook_subscribers`]   — webhook-style external hook subscriber
//!   handlers (HMAC-gated).
//! - [`errors`]      — issue #851 HTTP error-sanitization helpers
//!   (`sanitize_bulk_row_error`, `internal_error_response`,
//!   `bad_request_opaque`).
//! - [`system`]      — `/api/v1/capabilities` and other system-level
//!   read endpoints.
//! - [`parity`]      — cross-cutting HTTP-parity helpers
//!   (`fanout_or_503`, `resolve_caller_agent_id`).
//! - [`approvals`]   — v0.7.0 K10 approval API (`POST
//!   /api/v1/approvals/{pending_id}` + SSE stream).

pub mod approvals;
pub mod archive;
pub mod errors;
pub mod federation_receive;
pub mod hook_subscribers;
pub mod http;
pub mod parity;
pub mod skills;
pub mod system;
pub mod transport;

// Re-export the public-facing handler surface so external callers
// (router wiring in `src/lib.rs`, integration tests) can still
// reference `handlers::<name>` without knowing which sub-module the
// item came from. Wire compatibility is preserved verbatim.
pub use approvals::*;
pub use archive::*;
pub use errors::*;
pub use federation_receive::*;
pub use hook_subscribers::*;
pub use http::*;
pub(crate) use parity::*;
pub use skills::*;
pub use system::*;
pub use transport::*;

// Inline test scaffold (`#[cfg(test)] mod tests`) preserved verbatim
// from the pre-split mod.rs body. Tracked for future per-domain
// decomposition into `tests/handlers_<domain>.rs` integration test
// crates; the move-out is gated on exposing a stable `AppState`
// constructor helper from production code so tests outside the crate
// can build it without re-inventing fixture wiring (see #650 follow-up).
#[cfg(test)]
#[path = "tests.rs"]
mod tests;
