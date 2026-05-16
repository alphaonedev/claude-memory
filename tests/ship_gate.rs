// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L3-3 — Integration-test entry point for the grand-slam
//! ship-gate scenarios (issue #676).
//!
//! Cargo autodiscovers `tests/ship_gate.rs` as a single test binary;
//! the `mod` declarations below pull in the per-pillar acceptance
//! tests. Mirrors the same layout as `tests/curator.rs` / `tests/forensic.rs`
//! / `tests/transcripts.rs` so the on-disk shape stays consistent.
//!
//! * `grand_slam_recursive_learning` — Layer 1 + Layer 2 recursive-
//!   learning spine: curator reflection-pass e2e (L2-1), substrate
//!   cycle refusal (L1-2), federation depth replication + cross-peer
//!   refusal (L2-2), reflect-approval API flow (L1-8), migration
//!   round-trip + `memory_kind` backfill (L1-1), single-process
//!   chaos (no duplicate edges).
//!
//! * `grand_slam_skills` — Agent Skills pillar: spec-validation per
//!   agentskills.io §3.1 (L1-5), register → export → re-register
//!   identical-digest round-trip (L1-5 keystone + L2-6),
//!   reflection-as-skill promote folder layout (L2-6), federation
//!   replication with attestation, version-chain idempotency.
//!
//! * `grand_slam_composition` — cross-pillar composition: forensic
//!   bundle build + verify round-trip + tamper detection (L2-5),
//!   substrate-rule R001-R004 enforcement when operator-signed +
//!   enabled (L1-6 A–D), rule federation (L1-6 E), full v33 schema
//!   ladder + closed-taxonomy CHECK constraint, reflection-skill
//!   composition metadata round-trip (L2-7), audit-row emission
//!   completeness.

#[path = "ship_gate/grand_slam_recursive_learning.rs"]
mod grand_slam_recursive_learning;

#[path = "ship_gate/grand_slam_skills.rs"]
mod grand_slam_skills;

#[path = "ship_gate/grand_slam_composition.rs"]
mod grand_slam_composition;
