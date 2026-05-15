// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Form 5 — freshness-decay updater.
//!
//! The [`decayed`] function returns an exponentially decayed copy of
//! the input confidence value. Recall paths can call it on touch to
//! soft-floor stale rows; the actual write back into
//! `memories.confidence` happens only when
//! `AI_MEMORY_CONFIDENCE_DECAY=1` or the namespace policy carries
//! `confidence_decay_half_life_days`.
//!
//! Audit-honest contract: this module is pure. The caller owns the
//! substrate touch (UPDATE the row, stamp `confidence_decayed_at`,
//! flip `confidence_source` to [`crate::models::ConfidenceSource::Decayed`]).

/// Environment-variable opt-in for the recall-time decay updater.
pub const ENV_DECAY: &str = "AI_MEMORY_CONFIDENCE_DECAY";

/// Returns true when [`ENV_DECAY`] is set to `"1"`.
#[must_use]
pub fn decay_enabled() -> bool {
    std::env::var(ENV_DECAY).is_ok_and(|v| v == "1")
}

/// Compute a decayed confidence value.
///
/// `base` is the stored value; `age_days` is the time elapsed since
/// the value was last written (typically `now - max(created_at,
/// confidence_decayed_at)`); `half_life_days` is the namespace-policy
/// override or [`crate::confidence::DEFAULT_HALF_LIFE_DAYS`].
///
/// Formula: `base * 2^(-age / half_life)`, i.e.
/// `base * exp(-age * ln(2) / half_life)`. Honours the standard
/// half-life convention: at `age = half_life`, the value collapses to
/// `0.5 * base`. Clamped to `[0.0, 1.0]`. Negative `age_days` is
/// treated as `0.0` (no future-dated decay). `half_life_days <= 0`
/// is treated as `f64::EPSILON` so the divisor never goes to zero
/// (the value collapses to 0 in that case, which is the documented
/// degenerate-input contract).
#[must_use]
pub fn decayed(base: f64, age_days: f64, half_life_days: f64) -> f64 {
    let age = age_days.max(0.0);
    let half_life = half_life_days.max(f64::EPSILON);
    let factor = (-age * std::f64::consts::LN_2 / half_life).exp();
    (base * factor).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_age_returns_base() {
        assert!((decayed(0.9, 0.0, 30.0) - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn half_life_halves_value() {
        // base=1.0, age=half_life ⇒ ~0.5
        let v = decayed(1.0, 30.0, 30.0);
        assert!((v - 0.5).abs() < 1e-6, "expected ~0.5 got {v}");
    }

    #[test]
    fn very_old_collapses_toward_zero() {
        let v = decayed(0.9, 365.0, 30.0);
        assert!(v < 0.05, "expected near-zero got {v}");
    }

    #[test]
    fn negative_age_treated_as_zero() {
        assert!((decayed(0.7, -5.0, 30.0) - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn zero_half_life_collapses_to_zero() {
        // Degenerate input: half_life=0 ⇒ value collapses to 0
        // immediately (no future-dated decay; this is the contract).
        let v = decayed(0.9, 1.0, 0.0);
        assert!(v < 1e-6, "expected ~0 got {v}");
    }

    #[test]
    fn output_clamped_to_unit_interval() {
        // base > 1.0 is a bug elsewhere but the function must not
        // propagate it.
        let v = decayed(2.0, 0.0, 30.0);
        assert!((v - 1.0).abs() < f64::EPSILON);
        let v = decayed(-0.5, 0.0, 30.0);
        assert!((v - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn monotonic_in_age() {
        let a = decayed(1.0, 0.0, 30.0);
        let b = decayed(1.0, 10.0, 30.0);
        let c = decayed(1.0, 30.0, 30.0);
        assert!(a > b && b > c, "should decay monotonically: {a} {b} {c}");
    }

    #[test]
    fn decay_env_gating_default_off() {
        unsafe { std::env::remove_var(ENV_DECAY) };
        assert!(!decay_enabled());
        unsafe { std::env::set_var(ENV_DECAY, "1") };
        assert!(decay_enabled());
        unsafe { std::env::remove_var(ENV_DECAY) };
    }
}
