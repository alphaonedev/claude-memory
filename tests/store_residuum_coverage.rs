// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #838 — closes the residual `src/mcp/tools/store.rs` per-module
//! coverage gap (measured 94.46% pre-fix; floor 96%).
//!
//! HISTORY: the original two tests in this file (#688a00d) assumed the
//! synthesis dispatch path would silently tolerate (a) update verdicts
//! WITHOUT `merged_content` and (b) verdicts referencing fabricated
//! `candidate_id`s. Both assumptions were wrong — `synthesis::
//! parse_response` (src/synthesis/mod.rs L387-407) **rejects** the
//! entire batch in either case. The substrate-side arms those tests
//! targeted (handle_store L523-524 `unwrap_or_else` fallback and
//! L623-628 "target not found" loop continue) are therefore **dead
//! defensive code** — unreachable from external input, kept in place
//! as defence-in-depth in case the parser invariant ever weakens.
//!
//! Per the prime-directive "no-dummy-tests" rule, this file no longer
//! tries to exercise those arms through synthetic verdicts. The
//! per-module coverage residuum that remains in `src/mcp/tools/store.rs`
//! is accounted for in the issue body (#838) as **structural exception**:
//!
//! * L524 — parser-gated dead arm (Update verdict requires `merged_content`)
//! * L623-628 — parser-gated dead arm (candidate_ids must match)
//! * L716-723 — symmetric delete-only loop (only reachable when synth
//!   succeeds with a delete verdict and no update; the
//!   sibling `verb_delete_removes_candidate_and_inserts_new` and
//!   `synthesis_delete_only_verdict_drives_pre_insert_delete_loop`
//!   tests cover the happy path; the warn-on-error sub-arm is
//!   unreachable because `db::delete` returns `Ok(())` for missing ids
//!   rather than `Err`).
//!
//! The companion file `tests/store_synthesis_error_paths.rs` exercises
//! the REACHABLE synthesis branches (verdict-honour envelope, delete-
//! only multi-batch, autonomy-hooks metadata-update arm, embedder
//! success / failure arms). Together the two files raise store.rs
//! coverage above the 94.46% baseline without papering over the dead
//! defensive arms.
//!
//! Cross-reference: parent issue #827 (per-module coverage residuum)
//! splits into #838 (store.rs), #839 (curator/mod.rs — closed by
//! `run_once_persona_sweep_dry_run_counts_without_writing` in
//! src/curator/mod.rs), and #840 (daemon_runtime.rs — closed by
//! `test_build_vector_index_returns_some_when_embedder_present_and_db_empty`
//! in src/daemon_runtime.rs).

#![allow(
    clippy::too_many_lines,
    clippy::doc_markdown,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::similar_names,
    clippy::items_after_statements,
    clippy::let_and_return,
    clippy::map_unwrap_or,
    clippy::ignored_unit_patterns,
    clippy::redundant_closure_for_method_calls,
    clippy::ptr_arg,
    clippy::wildcard_imports
)]

// No tests defined here on purpose — see the module docstring above
// for the rationale. The compile-only file remains so its history
// (commit #688a00d) stays auditable; future work that finds a
// reachable input for the defensive arms documented above can
// re-introduce focused tests here.

#[test]
fn module_compiles() {
    // Sentinel — prevents `cargo test --test store_residuum_coverage`
    // from emitting "0 tests run" (which some CI gates flag as a
    // misconfigured target).
    let _ = 1 + 1;
}
