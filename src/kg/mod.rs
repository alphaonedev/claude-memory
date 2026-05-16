// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Knowledge-graph helpers — substrate-level graph utilities that operate
//! directly on the `memory_links` table.
//!
//! All items in this module are internal (`pub(crate)` or narrower);
//! no bare `pub` items are exported here.

// `cycle_check` is exposed `pub` so integration tests in `tests/kg/`
// can call `would_create_reflection_cycle` directly without going through
// the MCP handler (which is `pub(super)`). The internal helpers
// (`CycleCheckResult`, `forward_neighbors`, `reconstruct_path`) remain
// narrow — only the entry-point and result type are widened.
pub mod cycle_check;
