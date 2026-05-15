# Confidence Calibration (v0.7.0 Form 5)

This document explains the substrate-side confidence pipeline: how
ai-memory derives, decays, observes, and calibrates the
`memories.confidence` value at v0.7.0 and later.

Before v0.7.0 Form 5 (issue #758) the `confidence` REAL column had
existed since schema v2 and recall ranking consumed it
(`+ confidence * 2.0` in the FTS5 score expression at
`src/storage/mod.rs`). The audit (PR #753) found the surrounding
pipeline incomplete: every caller value was taken at face, there was no
freshness signal, no telemetry capturing whether the caller's value
agreed with what the substrate would have computed, and no calibration
mechanism. Form 5 closes those four gaps; the legacy contract is
preserved unchanged when no opt-in flag is set.

## Model

A memory's confidence is one of four typed provenance buckets, named on
the `memories.confidence_source TEXT NOT NULL DEFAULT 'caller_provided'`
column (schema v39 sqlite / v38 postgres):

* `caller_provided` — the legacy default. Recall ranking and forensic
  bundles trust the caller's value verbatim.
* `auto_derived` — the deterministic engine in
  `src/confidence/mod.rs::derive` computed the value at write time from
  observable row signals (see *Signals* below). Opt-in via
  `AI_MEMORY_AUTO_CONFIDENCE=1`.
* `calibrated` — the operator-driven sweep
  (`ai-memory calibrate confidence --from-shadow` /
  `memory_calibrate_confidence` MCP tool) replaced the live value with
  a per-(namespace, source) baseline computed from shadow-mode samples.
* `decayed` — the freshness-decay updater applied
  `exp(-age * ln(2) / half_life)` on a recall touch. Opt-in via
  `AI_MEMORY_CONFIDENCE_DECAY=1` or per-namespace
  `confidence_decay_half_life_days` policy.

The discriminator lets recall ranking, forensic bundle export, and the
calibration sweep reason about the trust path of a score without
re-running the derivation. The companion column
`memories.confidence_signals TEXT NULL` stores a JSON snapshot of the
signals that produced a derived or calibrated value (NULL for legacy
and caller-provided rows). `memories.confidence_decayed_at TEXT NULL`
records the RFC3339 stamp of the last decay update.

## Signals

`ConfidenceSignals` carries five fields, all preserved alongside the
row so the derivation is reproducible after the fact:

| Field | Source | Effect |
|---|---|---|
| `source_age_days` | `metadata.observed_at` (Form 4) or `created_at` | drives the freshness factor |
| `atom_derivation` | `atom_of IS NOT NULL` (WT-1-A) | +0.1 base bump |
| `prior_corroboration_count` | `COUNT(*)` over outbound `memory_links` | `+0.05 * log10(1 + n)` |
| `freshness_factor` | `2^(-age / half_life)` clamped to `[0,1]` | blends with baseline |
| `baseline_per_source` | calibration table for `(namespace, source)` | drift floor when fresh content is stale |

The deterministic auto-derive formula is:

```text
base = 0.5
     + 0.1 * is_atom
     + 0.05 * log10(1 + corroboration)
     - 0.02 * age * (ln(2) / half_life)
value = clamp(base, 0, 1) * freshness_factor
      + (1 - freshness_factor) * baseline_per_source
```

`derive` is pure and audit-honest — it does **not** touch the substrate,
fire a hook, or read environment variables. Callers gate on
`auto_confidence_enabled()` and persist the returned value only when
the operator has opted in. Tests pass handcrafted `DeriveContext` values
and get bit-identical outputs across runs.

## Shadow-mode usage

Shadow mode captures both the caller-supplied value and the value the
auto-derive engine would have computed, alongside the signal envelope
that produced the derivation. Per-recall rows land in the
`confidence_shadow_observations` table when
`AI_MEMORY_CONFIDENCE_SHADOW=1`, sampled at
`AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE` (0.0..=1.0; default 1.0).

The critical audit-honest property: shadow mode **never silently
overrides** the caller's confidence. The recall ranker still uses the
caller value downstream; the derived value is stored only for later
calibration review. This is the load-bearing property that lets
operators safely turn the engine on in production before any actual
recall-side behaviour changes.

Each observation row carries:

* `memory_id`, `namespace`, `observed_at` (the join key + window)
* `caller_confidence`, `derived_confidence` (the side-by-side comparison)
* `signals` (the JSON envelope that produced `derived_confidence`)
* `recall_outcome` (`recalled` | `skipped` | NULL) so the calibration
  sweep can distinguish helpful vs. discarded candidates

## Decay function

Freshness decay applies the standard half-life model:

```text
decayed(base, age_days, half_life_days)
    = base * 2^(-age / half_life)
    = base * exp(-age * ln(2) / half_life)
```

clamped to `[0.0, 1.0]`. At `age == half_life`, the value collapses to
`0.5 * base`. The default half-life is 30 days
(`DEFAULT_HALF_LIFE_DAYS`); each namespace can override via the
`confidence_decay_half_life_days` policy field.

`decay::decayed` is pure (no I/O). The recall path is the caller — when
`AI_MEMORY_CONFIDENCE_DECAY=1` or the namespace policy is set, recall
computes the decayed value, UPDATEs `memories.confidence`, sets
`confidence_source = 'decayed'`, and stamps `confidence_decayed_at`.

## Calibration workflow

1. Operator turns on shadow mode for a window:
   `AI_MEMORY_CONFIDENCE_SHADOW=1`. Optionally caps the sample rate via
   `AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE=0.1` (10% of recall touches
   record a row).
2. Daemon runs normal traffic. Per-recall samples accumulate.
3. Operator (or the daemon) drains the report:
   `ai-memory calibrate confidence --from-shadow --days 30`. Equivalent
   MCP tool: `memory_calibrate_confidence` (Family::Power).
4. Output is a `CalibrationReport` envelope: `window_days`,
   `total_observations`, and a list of `PerSourceBaseline` rows with
   median, mean, count, and a 10-bucket histogram per
   `(namespace, source)` pair.
5. Operator reviews the report. If the sample is well-distributed and
   the medians look sensible, the baselines become the
   `baseline_per_source` signal `derive` consumes on the next write
   wave. Persistence into a calibration store is operator-driven in a
   follow-up; v0.7.0 ships the observation pipeline and the read-side
   report only. A poorly-sampled window can't silently re-pin a
   namespace's confidence ceiling.

The audit-honest contract: every step is opt-in and reviewable. No
substrate write changes the canonical confidence value until the
operator authorises it.

## See also

* `src/confidence/mod.rs` — `derive`, `DeriveContext`,
  `auto_confidence_enabled`.
* `src/confidence/decay.rs` — `decayed`, `decay_enabled`.
* `src/confidence/shadow.rs` — `observe`, `observations_since`,
  `should_sample`.
* `src/confidence/calibrate.rs` — `calibrate_from_shadow`,
  `CalibrationReport`, `PerSourceBaseline`.
* `migrations/sqlite/0033_v07_form5_confidence_calibration.sql` —
  schema half (mirror at `migrations/postgres/0020_…`).
* `tests/form_5_confidence_calibration.rs` — acceptance suite.
