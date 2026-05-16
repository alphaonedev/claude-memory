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
//! The recall-time substrate touch lives in [`apply_decay_touch`]:
//! when `AI_MEMORY_CONFIDENCE_DECAY=1` and a memory is recalled,
//! `crate::store::sqlite::touch_after_recall` calls this helper to
//! UPDATE the row in place, stamp `confidence_decayed_at`, overwrite
//! `confidence` with the decayed value, and flip `confidence_source`
//! to [`crate::models::ConfidenceSource::Decayed`].
//!
//! Audit-honest contract: this module is pure (the math) plus one
//! tightly-scoped substrate writer ([`apply_decay_touch`]) that lives
//! here — not in `src/storage/mod.rs` — so the Cluster F recall SQL
//! stays untouched.

use rusqlite::{Connection, params};

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

/// Substrate-side decay touch fired from `touch_after_recall` when
/// `AI_MEMORY_CONFIDENCE_DECAY=1`. Reads the row's current
/// `confidence`, `created_at`, and `confidence_decayed_at`, computes
/// the decayed value via [`decayed`] using
/// [`crate::confidence::DEFAULT_HALF_LIFE_DAYS`] (per-namespace
/// half-life override is a future-Cluster knob), and writes back the
/// new value, the `'decayed'` source marker, and a fresh
/// `confidence_decayed_at` timestamp.
///
/// Idempotent — re-running on a row that has already been decayed
/// uses the most recent `confidence_decayed_at` as the age anchor, so
/// repeated touches converge rather than collapsing the value to zero.
/// Returns `Ok(true)` when a row was updated, `Ok(false)` when no row
/// matched the id (silently swallowed by the caller).
///
/// # Errors
///
/// Returns the underlying `rusqlite` error on SQL failure.
pub fn apply_decay_touch(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    use chrono::{DateTime, Utc};
    // Read the row's age anchor + current confidence in one shot. The
    // anchor is `confidence_decayed_at` when present (subsequent
    // decays compute from the last decay timestamp, not creation),
    // falling back to `created_at` for first-touch rows.
    let row: Option<(f64, String, Option<String>)> = conn
        .query_row(
            "SELECT confidence, created_at, confidence_decayed_at
             FROM memories WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    let Some((current_confidence, created_at, decayed_at)) = row else {
        return Ok(false);
    };

    let now = Utc::now();
    let anchor_str = decayed_at.unwrap_or(created_at);
    let anchor = DateTime::parse_from_rfc3339(&anchor_str)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(now);
    let age_days = (now - anchor).num_seconds() as f64 / 86_400.0;
    let new_value = decayed(
        current_confidence,
        age_days,
        crate::confidence::DEFAULT_HALF_LIFE_DAYS,
    );
    let stamp = now.to_rfc3339();
    let n = conn.execute(
        "UPDATE memories
         SET confidence = ?1,
             confidence_source = 'decayed',
             confidence_decayed_at = ?2
         WHERE id = ?3",
        params![new_value, stamp, id],
    )?;
    Ok(n > 0)
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
