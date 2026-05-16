// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Form 5 — auto-confidence + shadow-mode + calibration tooling
//! (issue #758).
//!
//! The Batman 6-form audit (PR #753, `docs/internal/batman-framework-audit.md`)
//! found Form 5 PARTIAL: the `memories.confidence` REAL column had
//! existed since schema v2 and recall ranking consumed it
//! (`+ confidence * 2.0` in the FTS5 score expression at
//! `src/storage/mod.rs`), but the surrounding pipeline was missing the
//! four substrate-honesty surfaces a "first-class confidence" claim
//! requires:
//!
//!   * **Automatic assignment.** Every caller value was taken at face;
//!     no source-age decay, atom-derivation bump, or prior-corroboration
//!     boost ever rewrote it.
//!   * **Shadow-mode telemetry.** No mechanism existed to compare a
//!     caller-provided value against a derived one on a live workload.
//!   * **Calibration.** No per-namespace / per-source-role baseline was
//!     ever computed from observed samples.
//!   * **Freshness decay.** An old fact at confidence=0.9 ranked
//!     identically to a fresh fact at the same value, despite human
//!     memory and downstream LLM reasoning both treating recency as a
//!     trust signal.
//!
//! This module is the Rust-side closeout. The schema half lives in
//! `migrations/sqlite/0033_v07_form5_confidence_calibration.sql` and
//! `migrations/postgres/0020_v07_form5_confidence_calibration.sql`.
//!
//! # Surface
//!
//! * [`derive`] — deterministic auto-derivation from row signals. Opt-in
//!   via `AI_MEMORY_AUTO_CONFIDENCE=1`.
//! * [`shadow::observe`] — writes per-recall samples to
//!   `confidence_shadow_observations` when
//!   `AI_MEMORY_CONFIDENCE_SHADOW=1`. Audit-honest: the caller value is
//!   still the one used downstream; shadow never silently overrides.
//! * [`decay::decayed`] — exponential freshness decay
//!   (`exp(-age / half_life)`); operator opts in with
//!   `AI_MEMORY_CONFIDENCE_DECAY=1` or per-namespace
//!   `confidence_decay_half_life_days` policy.
//! * [`calibrate::calibrate_from_shadow`] — computes per-source
//!   baselines from the shadow-observations table. Driven by the
//!   `ai-memory calibrate confidence` CLI and the
//!   `memory_calibrate_confidence` MCP tool.

use crate::models::{ConfidenceSignals, ConfidenceSource, Memory};

pub mod calibrate;
pub mod decay;
pub mod shadow;

/// Environment-variable opt-in for the auto-derive engine. When unset
/// or any value other than `"1"`, [`derive`] returns the caller's
/// confidence verbatim — preserving the v0.6.x contract.
pub const ENV_AUTO_CONFIDENCE: &str = "AI_MEMORY_AUTO_CONFIDENCE";

/// Returns true when [`ENV_AUTO_CONFIDENCE`] is set to `"1"`. Centralised
/// so the recall path, store path, and tests all read the same flag.
#[must_use]
pub fn auto_confidence_enabled() -> bool {
    std::env::var(ENV_AUTO_CONFIDENCE).is_ok_and(|v| v == "1")
}

/// Context the [`derive`] engine consults at the moment it computes a
/// fresh confidence value.
///
/// Pulled out of the [`Memory`] payload because three of the five signals
/// require substrate-side queries (`prior_corroboration_count` is a
/// `COUNT(*)` over `memory_links`, `baseline_per_source` is a lookup in
/// the calibration table, `half_life_days` honours the per-namespace
/// policy override) and the [`derive`] surface keeps the caller in
/// charge of those substrate touches so this module stays pure.
#[derive(Debug, Clone, Copy)]
pub struct DeriveContext {
    /// How long ago (in days) the cited source body was first observed.
    /// Drives the `freshness_factor` exponent. The caller computes this
    /// from `metadata.observed_at` (Form 4) or the row's `created_at`
    /// as a fallback. Negative values are clamped to `0.0`.
    pub source_age_days: f64,
    /// Whether the row is an atom of an existing memory
    /// (`atom_of IS NOT NULL`). Atom rows inherit a +0.1 confidence
    /// bump because their provenance is anchored to a curator-validated
    /// parent.
    pub atom_derivation: bool,
    /// Count of `memory_links` with this row as `source_id`. More
    /// corroboration → higher confidence; the formula uses
    /// `log10(1 + count)` to keep the bump sub-linear.
    pub prior_corroboration_count: i64,
    /// Per-(namespace, source) baseline from the calibration table.
    /// Pass `0.5` when no calibrated baseline exists yet.
    pub baseline_per_source: f64,
    /// Half-life (in days) used in the freshness decay computation.
    /// Defaults to 30 when the operator hasn't overridden the value
    /// via namespace policy. Capped at `f64::EPSILON` from below so
    /// the divisor in [`decay::decayed`] never goes to zero.
    pub half_life_days: f64,
}

impl Default for DeriveContext {
    fn default() -> Self {
        Self {
            source_age_days: 0.0,
            atom_derivation: false,
            prior_corroboration_count: 0,
            baseline_per_source: 0.5,
            half_life_days: 30.0,
        }
    }
}

/// Default half-life (in days) for the freshness-decay envelope.
/// 30 days mirrors a working agent's "this month vs. last month"
/// salience window; long-tier rows that survive a month already
/// have meaningful corroboration through the `access_count`
/// promotion loop, so the half-life acts as a soft-floor rather
/// than a hard expiry.
pub const DEFAULT_HALF_LIFE_DAYS: f64 = 30.0;

/// Deterministically derive a confidence value from row signals.
///
/// Returns `(confidence, signals, source_marker)`:
///
/// * `confidence` — value in `[0.0, 1.0]`. The formula is:
///
///   ```text
///   base = 0.5
///        + 0.1 * is_atom
///        + 0.05 * log10(1 + corroboration)
///        - 0.02 * source_age_days * decay_rate
///   value = clamp(base, 0.0, 1.0) * freshness_factor
///         + (1 - freshness_factor) * baseline_per_source
///   ```
///
///   where `decay_rate = ln(2) / half_life_days` and
///   `freshness_factor = exp(-age / half_life)`. The blend with the
///   per-source baseline lets a well-calibrated source survive aging
///   without collapsing to the freshness floor.
///
/// * `signals` — the [`ConfidenceSignals`] envelope that produced the
///   value. Stored alongside the row on `memories.confidence_signals`
///   so the derivation is reproducible.
///
/// * `source_marker` — typed discriminator for the
///   `memories.confidence_source` column. Always [`ConfidenceSource::AutoDerived`]
///   here; the [`shadow`] and [`calibrate`] paths use the other
///   variants.
///
/// # Audit honesty
///
/// This function is pure — it does **not** touch the substrate, fire a
/// hook, or read environment variables. The caller is responsible for
/// gating on [`auto_confidence_enabled`] and only persisting the
/// returned value when the opt-in is active. Tests can call it directly
/// with handcrafted [`DeriveContext`] values and get bit-identical
/// outputs across runs.
#[must_use]
pub fn derive(_memory: &Memory, ctx: &DeriveContext) -> (f64, ConfidenceSignals, ConfidenceSource) {
    let age = ctx.source_age_days.max(0.0);
    let half_life = ctx.half_life_days.max(f64::EPSILON);
    let decay_rate = std::f64::consts::LN_2 / half_life;
    // freshness_factor follows the standard half-life convention:
    // value halves every `half_life_days`. Matches `decay::decayed`.
    let freshness_factor = (-age * std::f64::consts::LN_2 / half_life)
        .exp()
        .clamp(0.0, 1.0);

    let atom_bump = if ctx.atom_derivation { 0.1 } else { 0.0 };
    let corroboration_bump = 0.05
        * (1.0_f64 + ctx.prior_corroboration_count.max(0) as f64)
            .log10()
            .max(0.0);
    let age_penalty = 0.02 * age * decay_rate;

    let raw_base = 0.5 + atom_bump + corroboration_bump - age_penalty;
    let clamped_base = raw_base.clamp(0.0, 1.0);
    let baseline = ctx.baseline_per_source.clamp(0.0, 1.0);

    let blended = clamped_base.mul_add(freshness_factor, baseline * (1.0 - freshness_factor));
    let value = blended.clamp(0.0, 1.0);

    let signals = ConfidenceSignals {
        source_age_days: age,
        atom_derivation: ctx.atom_derivation,
        prior_corroboration_count: ctx.prior_corroboration_count,
        freshness_factor,
        baseline_per_source: baseline,
    };

    (value, signals, ConfidenceSource::AutoDerived)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Memory {
        Memory {
            id: "m1".into(),
            ..Memory::default()
        }
    }

    #[test]
    fn derive_atom_bump_lifts_score() {
        let ctx_no_atom = DeriveContext {
            atom_derivation: false,
            ..Default::default()
        };
        let ctx_atom = DeriveContext {
            atom_derivation: true,
            ..Default::default()
        };
        let (no_atom, _, _) = derive(&mem(), &ctx_no_atom);
        let (atom, _, _) = derive(&mem(), &ctx_atom);
        assert!(
            atom > no_atom,
            "atom-derivation should raise confidence: {atom} vs {no_atom}"
        );
    }

    #[test]
    fn derive_corroboration_lifts_score_sublinearly() {
        let (low, _, _) = derive(
            &mem(),
            &DeriveContext {
                prior_corroboration_count: 1,
                ..Default::default()
            },
        );
        let (high, _, _) = derive(
            &mem(),
            &DeriveContext {
                prior_corroboration_count: 100,
                ..Default::default()
            },
        );
        assert!(high > low, "corroboration should monotonically raise score");
    }

    #[test]
    fn derive_age_reduces_score() {
        let (fresh, _, _) = derive(
            &mem(),
            &DeriveContext {
                source_age_days: 0.0,
                ..Default::default()
            },
        );
        let (old, _, _) = derive(
            &mem(),
            &DeriveContext {
                source_age_days: 365.0,
                ..Default::default()
            },
        );
        assert!(
            fresh > old,
            "older sources should have lower confidence: {fresh} vs {old}"
        );
    }

    #[test]
    fn derive_clamps_to_unit_interval() {
        let ctx = DeriveContext {
            source_age_days: 10_000.0,
            atom_derivation: false,
            prior_corroboration_count: 0,
            baseline_per_source: 0.0,
            half_life_days: 30.0,
        };
        let (value, _, _) = derive(&mem(), &ctx);
        assert!((0.0..=1.0).contains(&value), "value out of range: {value}");
    }

    #[test]
    fn derive_returns_signals_envelope_matching_inputs() {
        let ctx = DeriveContext {
            source_age_days: 15.0,
            atom_derivation: true,
            prior_corroboration_count: 5,
            baseline_per_source: 0.6,
            half_life_days: 30.0,
        };
        let (_value, signals, source) = derive(&mem(), &ctx);
        assert_eq!(source, ConfidenceSource::AutoDerived);
        assert!((signals.source_age_days - 15.0).abs() < f64::EPSILON);
        assert!(signals.atom_derivation);
        assert_eq!(signals.prior_corroboration_count, 5);
        assert!((signals.baseline_per_source - 0.6).abs() < f64::EPSILON);
        // freshness at age=15, half_life=30 → 2^-0.5 ≈ 0.707
        assert!((signals.freshness_factor - 0.7071).abs() < 0.01);
    }

    #[test]
    fn derive_is_deterministic() {
        let ctx = DeriveContext {
            source_age_days: 7.5,
            atom_derivation: false,
            prior_corroboration_count: 3,
            baseline_per_source: 0.55,
            half_life_days: 30.0,
        };
        let (a, _, _) = derive(&mem(), &ctx);
        let (b, _, _) = derive(&mem(), &ctx);
        assert!(
            (a - b).abs() < f64::EPSILON,
            "derive must be deterministic for fixed inputs: {a} vs {b}"
        );
    }

    #[test]
    fn derive_never_returns_one_for_default_context() {
        // The default context yields a non-1.0 score (the legacy contract
        // was "caller value taken at face = 1.0"; the auto-derive engine
        // is designed to produce honest values away from the saturation
        // points). Pinning at 0.5 (the baseline) for the default context.
        let (value, _, _) = derive(&mem(), &DeriveContext::default());
        assert!((value - 0.5).abs() < 0.05);
    }

    #[test]
    fn auto_confidence_env_gating_default_off() {
        // Per the audit-honest contract: opt-in only. With no env var
        // set, the helper returns false and callers preserve the
        // caller-provided value.
        // We don't call std::env::set_var here (tests share process
        // env); we just confirm the helper's predicate for "1".
        unsafe { std::env::remove_var(ENV_AUTO_CONFIDENCE) };
        assert!(!auto_confidence_enabled());
        unsafe { std::env::set_var(ENV_AUTO_CONFIDENCE, "0") };
        assert!(!auto_confidence_enabled());
        unsafe { std::env::set_var(ENV_AUTO_CONFIDENCE, "1") };
        assert!(auto_confidence_enabled());
        unsafe { std::env::remove_var(ENV_AUTO_CONFIDENCE) };
    }
}
