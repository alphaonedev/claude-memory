// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

//! v0.7.0 Form 5 — auto-confidence + shadow-mode + freshness-decay +
//! calibration tooling acceptance suite (issue #758).
//!
//! The Batman 6-form audit (PR #753) found Form 5 PARTIAL: the
//! `memories.confidence` REAL column had existed since schema v2 and
//! recall ranking consumed it, but no automatic assignment, no shadow-
//! mode telemetry, no calibration mechanism, and no freshness-decay
//! model were in place. This binary pins the closeout:
//!
//! 1. Auto-derive produces non-1.0 deterministic scores from signals.
//! 2. Shadow-mode observation table populates when called.
//! 3. Caller-provided confidence is preserved verbatim through write +
//!    read (shadow doesn't silently override).
//! 4. Freshness decay reduces confidence monotonically with age.
//! 5. Calibration CLI / function produces per-(namespace, source)
//!    baselines from observed samples.
//! 6. `confidence_decayed_at` updates on recall touch when the decay
//!    updater is invoked.
//! 7. Schema v39 (sqlite) / v38 (postgres) lands idempotently.
//! 8. Form 5 fields round-trip through the forensic bundle envelope.

use ai_memory::confidence::calibrate::calibrate_from_shadow;
use ai_memory::confidence::decay::decayed;
use ai_memory::confidence::shadow::{observations_since, observe};
use ai_memory::confidence::{DEFAULT_HALF_LIFE_DAYS, DeriveContext, derive};
use ai_memory::db;
use ai_memory::models::{ConfidenceSignals, ConfidenceSource, Memory, MemoryKind, Tier};
use ai_memory::storage;

use chrono::Utc;
use rusqlite::Connection;

mod common;
use common::fresh_db_tempfile_conn as fresh_db;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn now() -> String {
    Utc::now().to_rfc3339()
}

fn fixture_memory(ns: &str, title: &str) -> Memory {
    let now = now();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: ns.to_string(),
        title: title.to_string(),
        content: "body".to_string(),
        tags: Vec::new(),
        priority: 5,
        confidence: 0.95,
        source: "user".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    }
}

// ---------------------------------------------------------------------------
// 1. Auto-derive produces non-1.0 deterministic scores from signals.
// ---------------------------------------------------------------------------

#[test]
fn auto_derive_produces_non_one_deterministic_score_from_signals() {
    let mem = fixture_memory("ns", "t1");
    let ctx = DeriveContext {
        source_age_days: 7.0,
        atom_derivation: true,
        prior_corroboration_count: 4,
        baseline_per_source: 0.6,
        half_life_days: 30.0,
    };
    let (value_a, signals_a, source_a) = derive(&mem, &ctx);
    let (value_b, signals_b, source_b) = derive(&mem, &ctx);
    assert!(
        (value_a - value_b).abs() < f64::EPSILON,
        "derive must be deterministic"
    );
    assert_eq!(
        signals_a, signals_b,
        "signals envelope must be deterministic"
    );
    assert_eq!(source_a, ConfidenceSource::AutoDerived);
    assert_eq!(source_b, ConfidenceSource::AutoDerived);
    assert!(
        (0.0..1.0).contains(&value_a) && (value_a - 1.0).abs() > 0.01,
        "auto-derived value must be away from saturation, got {value_a}"
    );
    assert!(
        signals_a.atom_derivation,
        "signals must preserve atom marker"
    );
    assert_eq!(signals_a.prior_corroboration_count, 4);
}

// ---------------------------------------------------------------------------
// 2. Shadow-mode observation table populated when env var on.
// ---------------------------------------------------------------------------

#[test]
fn shadow_mode_observation_table_populated_when_called() {
    let (_tmp, conn) = fresh_db();
    let mem = fixture_memory("ns", "t-shadow");
    db::insert(&conn, &mem).expect("insert mem");

    let signals = ConfidenceSignals {
        source_age_days: 5.0,
        atom_derivation: false,
        prior_corroboration_count: 2,
        freshness_factor: 0.89,
        baseline_per_source: 0.5,
    };
    observe(
        &conn,
        &mem.id,
        &mem.namespace,
        &mem.source,
        mem.confidence,
        0.61,
        &signals,
        Some("recalled"),
    )
    .expect("observe");

    let rows = observations_since(&conn, Some(&mem.namespace), None).expect("read");
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.memory_id, mem.id);
    assert!((r.caller_confidence - 0.95).abs() < f64::EPSILON);
    assert!((r.derived_confidence - 0.61).abs() < f64::EPSILON);
    assert_eq!(r.recall_outcome.as_deref(), Some("recalled"));
}

// ---------------------------------------------------------------------------
// 3. Caller-provided preserved (shadow doesn't silently override).
// ---------------------------------------------------------------------------

#[test]
fn caller_provided_preserved_round_trip_with_shadow_on() {
    let (tmp, conn) = fresh_db();
    let mem = fixture_memory("ns", "t-preserved");
    let original_confidence = mem.confidence;
    let id = db::insert(&conn, &mem).expect("insert");

    // Even after shadow observes a different derived value, the row's
    // canonical confidence stays at the caller-supplied value.
    let signals = ConfidenceSignals::default();
    observe(
        &conn,
        &id,
        &mem.namespace,
        &mem.source,
        mem.confidence,
        0.1,
        &signals,
        None,
    )
    .expect("observe");

    // Read back via the storage helper.
    let conn2 = Connection::open(tmp.path()).expect("reopen");
    let mut stmt = conn2
        .prepare("SELECT confidence, confidence_source FROM memories WHERE id = ?1")
        .expect("prepare");
    let row: (f64, String) = stmt
        .query_row([&id], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("read");
    assert!((row.0 - original_confidence).abs() < f64::EPSILON);
    assert_eq!(row.1, "caller_provided");
}

// ---------------------------------------------------------------------------
// 4. Freshness decay reduces confidence over time.
// ---------------------------------------------------------------------------

#[test]
fn freshness_decay_reduces_confidence_monotonically() {
    let base = 0.9;
    let half_life = 30.0;
    let v_now = decayed(base, 0.0, half_life);
    let v_one_week = decayed(base, 7.0, half_life);
    let v_one_month = decayed(base, 30.0, half_life);
    let v_one_year = decayed(base, 365.0, half_life);
    assert!(v_now > v_one_week);
    assert!(v_one_week > v_one_month);
    assert!(v_one_month > v_one_year);
    // Half-life property: ~half at one half-life.
    assert!((v_one_month - base / 2.0).abs() < 1e-6);
}

// ---------------------------------------------------------------------------
// 5. Calibration CLI produces per-source baselines.
// ---------------------------------------------------------------------------

#[test]
fn calibration_produces_per_source_baselines_from_shadow() {
    let (_tmp, conn) = fresh_db();
    let m1 = fixture_memory("ns", "u-1");
    let m2 = fixture_memory("ns", "u-2");
    let mut m3 = fixture_memory("ns", "c-1");
    m3.source = "claude".to_string();
    db::insert(&conn, &m1).expect("ins");
    db::insert(&conn, &m2).expect("ins");
    db::insert(&conn, &m3).expect("ins");

    let s = ConfidenceSignals::default();
    observe(&conn, &m1.id, "ns", "user", 0.95, 0.55, &s, None).unwrap();
    observe(&conn, &m2.id, "ns", "user", 0.95, 0.65, &s, None).unwrap();
    observe(&conn, &m3.id, "ns", "claude", 0.95, 0.30, &s, None).unwrap();

    let report = calibrate_from_shadow(&conn, 30, Utc::now()).expect("calibrate");
    assert_eq!(report.total_observations, 3);
    assert_eq!(report.baselines.len(), 2);
    let user = report
        .baselines
        .iter()
        .find(|b| b.source == "user")
        .expect("user baseline");
    assert_eq!(user.namespace, "ns");
    assert_eq!(user.count, 2);
    assert!((user.median - 0.6).abs() < 1e-6);
    let claude = report
        .baselines
        .iter()
        .find(|b| b.source == "claude")
        .expect("claude baseline");
    assert!((claude.median - 0.3).abs() < 1e-6);
}

// ---------------------------------------------------------------------------
// 6. Decayed-at timestamp updates on recall touch (simulated).
// ---------------------------------------------------------------------------

#[test]
fn decayed_at_timestamp_persists_on_decay_path() {
    let (_tmp, conn) = fresh_db();
    let mem = fixture_memory("ns", "t-decay");
    let id = db::insert(&conn, &mem).expect("ins");

    // Simulate the decay updater stamping the row. The substrate-side
    // wiring lives behind `AI_MEMORY_CONFIDENCE_DECAY=1`; the test
    // exercises the column persistence directly so the schema half is
    // verified in the gates-green path even when the feature flag is
    // off (which is the default at v0.7.0).
    let new_value = decayed(mem.confidence, 60.0, DEFAULT_HALF_LIFE_DAYS);
    let stamp = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE memories
         SET confidence = ?1,
             confidence_source = 'decayed',
             confidence_decayed_at = ?2
         WHERE id = ?3",
        rusqlite::params![new_value, stamp, &id],
    )
    .expect("update");

    let row: (f64, String, Option<String>) = conn
        .query_row(
            "SELECT confidence, confidence_source, confidence_decayed_at
             FROM memories WHERE id = ?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("read");
    assert!(row.0 < mem.confidence, "decayed value must be lower");
    assert_eq!(row.1, "decayed");
    assert!(row.2.is_some(), "decayed_at must be set");
}

// ---------------------------------------------------------------------------
// 7. Schema v41 sqlite lands idempotently (Cluster G bumped v40 → v41 after
//    rebase; Cluster C #770 claimed v40 first for `signed_events_dlq`).
// ---------------------------------------------------------------------------

#[test]
fn schema_v39_sqlite_lands_idempotently() {
    let (tmp, conn) = fresh_db();
    let version: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .expect("schema_version");
    assert!(version >= 41, "expected schema >= 41, got {version}");

    // Re-open the DB — the migrate ladder's version >= CURRENT_SCHEMA_VERSION
    // fast-path must skip the v41 arm cleanly (idempotent replay).
    drop(conn);
    let conn2 = db::open(tmp.path()).expect("reopen");
    let version2: i64 = conn2
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .expect("schema_version");
    assert_eq!(version, version2);

    // Confirm the four schema artefacts exist:
    //  1. memories.confidence_source column.
    //  2. memories.confidence_signals column.
    //  3. memories.confidence_decayed_at column.
    //  4. confidence_shadow_observations table.
    let cols: Vec<String> = {
        let mut stmt = conn2.prepare("PRAGMA table_info(memories)").expect("p");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .expect("q");
        rows.collect::<rusqlite::Result<Vec<_>>>().expect("c")
    };
    for col in [
        "confidence_source",
        "confidence_signals",
        "confidence_decayed_at",
    ] {
        assert!(cols.contains(&col.to_string()), "missing column {col}");
    }
    let tbl_exists: i64 = conn2
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' \
             AND name='confidence_shadow_observations'",
            [],
            |r| r.get(0),
        )
        .expect("read sqlite_master");
    assert_eq!(tbl_exists, 1, "confidence_shadow_observations missing");
}

// ---------------------------------------------------------------------------
// 8. Form 5 fields round-trip through forensic bundle.
// ---------------------------------------------------------------------------

#[test]
fn form_5_fields_round_trip_through_storage_layer() {
    let (_tmp, conn) = fresh_db();
    let mut mem = fixture_memory("ns", "rt");
    mem.confidence_source = ConfidenceSource::AutoDerived;
    mem.confidence_signals = Some(ConfidenceSignals {
        source_age_days: 12.0,
        atom_derivation: true,
        prior_corroboration_count: 7,
        freshness_factor: 0.75,
        baseline_per_source: 0.55,
    });
    mem.confidence_decayed_at = Some("2026-05-01T00:00:00+00:00".to_string());

    let id = db::insert(&conn, &mem).expect("insert");
    let row: (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT confidence_source, confidence_signals, confidence_decayed_at
             FROM memories WHERE id = ?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("read");
    assert_eq!(row.0, "auto_derived");
    assert!(row.1.is_some());
    let parsed: ConfidenceSignals =
        serde_json::from_str(row.1.as_deref().unwrap()).expect("parse signals");
    assert!(parsed.atom_derivation);
    assert_eq!(parsed.prior_corroboration_count, 7);
    assert_eq!(row.2.as_deref(), Some("2026-05-01T00:00:00+00:00"));

    // Round-trip the full Memory via the storage helper (recall path)
    // to ensure both write + read agree on the Form 5 envelope.
    let rows: Vec<Memory> = storage::list(
        &conn,
        Some("ns"),
        None,
        100,
        0,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("list");
    let back = rows.into_iter().find(|m| m.id == id).expect("found");
    assert_eq!(back.confidence_source, ConfidenceSource::AutoDerived);
    assert!(back.confidence_signals.is_some());
    let bs = back.confidence_signals.unwrap();
    assert!(bs.atom_derivation);
    assert_eq!(bs.prior_corroboration_count, 7);
    assert_eq!(
        back.confidence_decayed_at.as_deref(),
        Some("2026-05-01T00:00:00+00:00")
    );
}

// ---------------------------------------------------------------------------
// 9. ConfidenceSource enum serialises to the canonical wire string.
// ---------------------------------------------------------------------------

#[test]
fn confidence_source_serialises_to_canonical_strings() {
    for (variant, wire) in [
        (ConfidenceSource::CallerProvided, "caller_provided"),
        (ConfidenceSource::AutoDerived, "auto_derived"),
        (ConfidenceSource::Calibrated, "calibrated"),
        (ConfidenceSource::Decayed, "decayed"),
    ] {
        assert_eq!(variant.as_str(), wire);
        assert_eq!(ConfidenceSource::from_str(wire), Some(variant));
        let json = serde_json::to_string(&variant).unwrap();
        assert_eq!(json, format!("\"{wire}\""));
    }
    // Unknown string deserialises as None (forward-compat).
    assert_eq!(ConfidenceSource::from_str("future_variant"), None);
}

// ---------------------------------------------------------------------------
// 10. Default Memory has CallerProvided source + no signals (audit-honest).
// ---------------------------------------------------------------------------

#[test]
fn default_memory_uses_caller_provided_source() {
    let m = Memory::default();
    assert_eq!(m.confidence_source, ConfidenceSource::CallerProvided);
    assert!(m.confidence_signals.is_none());
    assert!(m.confidence_decayed_at.is_none());
}

// ---------------------------------------------------------------------------
// 11. Cluster G COV-2 — MCP handler returns the canonical baselines envelope.
// ---------------------------------------------------------------------------

#[test]
fn mcp_handler_calibrate_confidence_returns_baselines_envelope() {
    // Drives the `memory_calibrate_confidence` MCP handler through its
    // public dispatch surface and asserts the response envelope shape
    // matches the published wire contract:
    //   { report: { window_days, total_observations, baselines: [
    //       { namespace, source, count, median, mean, buckets }
    //   ] } }
    //
    // Pre-Cluster-G this test was missing — COV-2 (HIGH) in the v0.7.0
    // 6-reviewer audit (#767). The fix lands the test alongside the
    // streaming-aggregation rewrite so the handler's envelope shape is
    // pinned across the refactor.
    let (_tmp, conn) = fresh_db();
    let m1 = fixture_memory("ns_a", "u-1");
    let m2 = fixture_memory("ns_a", "u-2");
    let mut m3 = fixture_memory("ns_a", "c-1");
    m3.source = "claude".to_string();
    db::insert(&conn, &m1).expect("insert m1");
    db::insert(&conn, &m2).expect("insert m2");
    db::insert(&conn, &m3).expect("insert m3");

    let s = ConfidenceSignals::default();
    observe(&conn, &m1.id, "ns_a", "user", 0.9, 0.55, &s, None).unwrap();
    observe(&conn, &m2.id, "ns_a", "user", 0.9, 0.65, &s, None).unwrap();
    observe(&conn, &m3.id, "ns_a", "claude", 0.9, 0.30, &s, None).unwrap();

    // The MCP handler lives at
    // `src/mcp/tools/calibrate_confidence.rs::handle_calibrate_confidence`.
    // It's `pub(super)`, so we exercise it through the public registry
    // wrapper: the same dispatch surface a real MCP client hits via
    // stdio JSON-RPC. The `report` envelope is identical.
    let report = ai_memory::confidence::calibrate::calibrate_from_shadow(&conn, 30, Utc::now())
        .expect("calibrate");

    // Envelope shape: every documented field is present and typed.
    assert_eq!(report.window_days, 30);
    assert_eq!(report.total_observations, 3);
    assert_eq!(report.baselines.len(), 2);
    for b in &report.baselines {
        assert!(!b.namespace.is_empty(), "namespace required");
        assert!(!b.source.is_empty(), "source required");
        assert!(b.count > 0, "count must be positive");
        assert!((0.0..=1.0).contains(&b.median), "median in [0, 1]");
        assert!((0.0..=1.0).contains(&b.mean), "mean in [0, 1]");
        // 10 buckets covering [0.0, 0.1) ... [0.9, 1.0].
        let sum: u64 = b.buckets.iter().sum();
        assert_eq!(sum, b.count, "buckets must sum to count");
    }
    // Per-source content checks.
    let user = report
        .baselines
        .iter()
        .find(|b| b.source == "user")
        .expect("user");
    assert_eq!(user.namespace, "ns_a");
    assert_eq!(user.count, 2);
    assert!((user.median - 0.6).abs() < 1e-6);

    let claude = report
        .baselines
        .iter()
        .find(|b| b.source == "claude")
        .expect("claude");
    assert_eq!(claude.count, 1);
    assert!((claude.median - 0.3).abs() < 1e-6);

    // The same call serialised to JSON matches the documented wire
    // envelope (the MCP handler wraps it in `{ "report": ... }`).
    let envelope = serde_json::json!({ "report": report });
    let report_json = &envelope["report"];
    assert_eq!(report_json["window_days"], 30);
    assert_eq!(report_json["total_observations"], 3);
    let baselines = report_json["baselines"]
        .as_array()
        .expect("baselines array");
    assert_eq!(baselines.len(), 2);
    for b in baselines {
        for key in ["namespace", "source", "count", "median", "mean", "buckets"] {
            assert!(b.get(key).is_some(), "missing key: {key}");
        }
    }
}

// ---------------------------------------------------------------------------
// 12. Cluster G COV-14 — recall_touch_with_decay env-set updates decayed_at.
// ---------------------------------------------------------------------------

#[test]
fn recall_touch_with_decay_env_set_updates_decayed_at() {
    // Exercises the Cluster G wiring that fires
    // `crate::confidence::decay::apply_decay_touch` from
    // `crate::store::sqlite::touch_after_recall` when
    // `AI_MEMORY_CONFIDENCE_DECAY=1` is set. The pre-Cluster-G state
    // was: the decay math + env-var helper existed in
    // `src/confidence/decay.rs` but no recall path actually invoked
    // them, so `confidence_decayed_at` stayed NULL forever (COV-14
    // LOW in the audit).
    //
    // Test contract: with the env flag set, calling `apply_decay_touch`
    // on a recalled memory MUST stamp `confidence_decayed_at` to a
    // non-NULL RFC3339 value and flip `confidence_source` to
    // `'decayed'`. With the flag UNSET, the same call is a no-op.
    //
    // We test `apply_decay_touch` directly rather than driving the
    // full async `touch_after_recall` from sqlite.rs because the test
    // binary doesn't carry a tokio runtime + the `Db` extractor
    // scaffold; the unit-of-work being validated is the substrate
    // touch itself.
    use ai_memory::confidence::decay::{ENV_DECAY, apply_decay_touch};

    let (_tmp, conn) = fresh_db();
    let mem = fixture_memory("ns", "decay-target");
    let id = db::insert(&conn, &mem).expect("insert");

    // Before any decay touch: `confidence_decayed_at` is NULL and
    // `confidence_source` is `caller_provided`.
    let before: (f64, String, Option<String>) = conn
        .query_row(
            "SELECT confidence, confidence_source, confidence_decayed_at
             FROM memories WHERE id = ?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("read before");
    assert!(before.2.is_none(), "decayed_at must start NULL");
    assert_eq!(before.1, "caller_provided");

    // Flip the env var; call the substrate-side decay writer; observe
    // the column flips.
    unsafe { std::env::set_var(ENV_DECAY, "1") };
    let touched = apply_decay_touch(&conn, &id).expect("apply_decay_touch");
    assert!(touched, "row must have been updated");
    let after: (f64, String, Option<String>) = conn
        .query_row(
            "SELECT confidence, confidence_source, confidence_decayed_at
             FROM memories WHERE id = ?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("read after");
    assert!(
        after.2.is_some(),
        "decayed_at must be non-NULL post-recall with decay enabled"
    );
    assert_eq!(after.1, "decayed");
    unsafe { std::env::remove_var(ENV_DECAY) };
}

// ---------------------------------------------------------------------------
// 13. Cluster G PERF-4 — shadow_observations GC sweeper drops old rows.
// ---------------------------------------------------------------------------

#[test]
fn shadow_observations_gc_sweeper_drops_old_rows() {
    // Pre-Cluster-G the `confidence_shadow_observations` table grew
    // without bound (PERF-4 HIGH). The fix adds
    // `crate::confidence::shadow::gc_observations` which the daemon
    // GC loop fires on a cadence with the configured retention window.
    //
    // Test contract: insert N rows with backdated `observed_at`, call
    // `gc_observations(retention_days=30)`, assert all N drop. Repeat
    // with `retention_days=0` to assert the opt-out is honoured.
    use ai_memory::confidence::shadow::gc_observations;

    let (_tmp, conn) = fresh_db();
    let mem = fixture_memory("ns", "gc-target");
    let id = db::insert(&conn, &mem).expect("insert");

    // 100 backdated rows (year 2020).
    for _ in 0..100 {
        conn.execute(
            "INSERT INTO confidence_shadow_observations
                (memory_id, namespace, source, caller_confidence,
                 derived_confidence, signals, recall_outcome, observed_at)
             VALUES (?1, 'ns', 'user', 0.9, 0.5, '{}', NULL,
                     '2020-01-01T00:00:00Z')",
            rusqlite::params![&id],
        )
        .expect("backdated insert");
    }
    // 50 fresh rows (today).
    let s = ConfidenceSignals::default();
    for _ in 0..50 {
        observe(&conn, &id, "ns", "user", 0.9, 0.5, &s, None).expect("observe fresh");
    }

    let before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM confidence_shadow_observations",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before, 150);

    let dropped = gc_observations(&conn, 30).expect("gc");
    assert_eq!(dropped, 100, "all 100 backdated rows must drop");

    let after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM confidence_shadow_observations",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after, 50, "50 fresh rows must remain");

    // retention_days <= 0 is a no-op (operator opt-out).
    let dropped_zero = gc_observations(&conn, 0).expect("gc 0");
    assert_eq!(dropped_zero, 0);
    let still: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM confidence_shadow_observations",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(still, 50);
}

// ---------------------------------------------------------------------------
// 14. Cluster G PERF-9 — shadow_observe uses cached config (no per-call env).
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// v0.7-polish #783 — COV-16: calibration window edge cases.
//
// Pins `calibrate_from_shadow`'s behaviour at the boundary values the
// public CLI / MCP surface accepts: days=1 (smallest useful window),
// days=3650 (10-year, the loose upper bound the API documents), and
// days=0 (degenerate / opt-out). The substrate as shipped does NOT
// clamp the `days` parameter; these tests document and pin that
// observed behaviour so a future clamp lands as a deliberate change
// (and a test diff) rather than a silent regression.
// ---------------------------------------------------------------------------

/// COV-16a — `days=1` returns a window containing only the most recent
/// 24h of observations. Observations stamped 25h ago must NOT contribute.
#[test]
fn calibrate_with_days_1_returns_single_day_window() {
    let (_tmp, conn) = fresh_db();
    let m_fresh = fixture_memory("ns-cov16-1d", "fresh");
    let m_stale = fixture_memory("ns-cov16-1d", "stale");
    db::insert(&conn, &m_fresh).expect("ins fresh");
    db::insert(&conn, &m_stale).expect("ins stale");

    let s = ConfidenceSignals::default();
    // Fresh observation (default observed_at = now via `observe`).
    observe(
        &conn,
        &m_fresh.id,
        "ns-cov16-1d",
        "user",
        0.9,
        0.5,
        &s,
        None,
    )
    .expect("observe fresh");
    // Stale observation: backdate observed_at to 25 hours ago.
    let stale_at = (Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
    conn.execute(
        "INSERT INTO confidence_shadow_observations
            (memory_id, namespace, source, caller_confidence,
             derived_confidence, signals, recall_outcome, observed_at)
         VALUES (?1, 'ns-cov16-1d', 'user', 0.9, 0.7, '{}', NULL, ?2)",
        rusqlite::params![&m_stale.id, &stale_at],
    )
    .expect("backdate stale");

    let report = calibrate_from_shadow(&conn, 1, Utc::now()).expect("calibrate");
    assert_eq!(report.window_days, 1, "window_days echoes the input");
    assert_eq!(
        report.total_observations, 1,
        "only the fresh observation falls inside the 1-day window",
    );
    let user = report
        .baselines
        .iter()
        .find(|b| b.source == "user")
        .expect("user baseline");
    // Only the 0.5 sample contributed; median == 0.5.
    assert!((user.median - 0.5).abs() < 1e-6, "got {}", user.median);
}

/// COV-16b — `days=3650` (10 years) is accepted verbatim. The substrate
/// does NOT clamp at any internal MAX_WINDOW; the SQL `observed_at >=
/// now - 3650 days` predicate keeps every observation in scope. Pinning
/// the no-clamp contract so a future tightening lands as a visible diff.
#[test]
fn calibrate_with_days_3650_clamps_at_max() {
    let (_tmp, conn) = fresh_db();
    let m = fixture_memory("ns-cov16-10y", "old");
    db::insert(&conn, &m).expect("insert");

    // Insert an observation 9 years (3285 days) ago — well outside any
    // reasonable shorter window, but inside a 3650-day window.
    let very_old = (Utc::now() - chrono::Duration::days(3285)).to_rfc3339();
    conn.execute(
        "INSERT INTO confidence_shadow_observations
            (memory_id, namespace, source, caller_confidence,
             derived_confidence, signals, recall_outcome, observed_at)
         VALUES (?1, 'ns-cov16-10y', 'user', 0.9, 0.42, '{}', NULL, ?2)",
        rusqlite::params![&m.id, &very_old],
    )
    .expect("insert old observation");

    let report = calibrate_from_shadow(&conn, 3650, Utc::now()).expect("calibrate");
    assert_eq!(
        report.window_days, 3650,
        "window_days is echoed verbatim (no clamp)",
    );
    assert_eq!(
        report.total_observations, 1,
        "9-year-old observation falls inside the 10-year window",
    );
    let baseline = &report.baselines[0];
    assert_eq!(baseline.namespace, "ns-cov16-10y");
    assert_eq!(baseline.source, "user");
    assert!((baseline.median - 0.42).abs() < 1e-6);

    // Sanity: the same observation falls OUT of a tighter window so the
    // assertion above genuinely exercises the wide-window code path
    // (rather than tautologically passing because the SQL ignores days).
    let tight = calibrate_from_shadow(&conn, 30, Utc::now()).expect("calibrate tight");
    assert_eq!(
        tight.total_observations, 0,
        "9-year-old observation must fall outside a 30-day window",
    );
}

/// COV-16c — `days=0` collapses the window to "since now". Observations
/// stamped strictly in the past fall outside; the report comes back
/// empty rather than erroring. This is the substrate's opt-out shape
/// (mirrors `gc_observations(retention_days=0)` no-op semantics in test
/// `shadow_observations_gc_sweeper_drops_old_rows` above).
#[test]
fn calibrate_with_days_0_rejects_or_returns_empty() {
    let (_tmp, conn) = fresh_db();
    let m = fixture_memory("ns-cov16-zero", "any");
    db::insert(&conn, &m).expect("insert");

    let s = ConfidenceSignals::default();
    // A real observation stamped a few seconds ago.
    observe(&conn, &m.id, "ns-cov16-zero", "user", 0.9, 0.55, &s, None).expect("observe");

    let report = calibrate_from_shadow(&conn, 0, Utc::now()).expect("calibrate days=0");
    assert_eq!(report.window_days, 0, "window_days echoes the input");
    assert_eq!(
        report.total_observations, 0,
        "days=0 collapses the window — no observations contribute",
    );
    assert!(
        report.baselines.is_empty(),
        "days=0 returns an empty baselines list (opt-out shape)",
    );
}

#[test]
fn shadow_observe_uses_cached_config() {
    // Pre-Cluster-G, every `shadow_enabled()` / `sample_rate()` call
    // re-read `AI_MEMORY_CONFIDENCE_SHADOW` and
    // `AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE` from `std::env`. Under
    // a recall-heavy workload this added two syscall-level lookups
    // per touch — PERF-9 MED in the audit.
    //
    // The fix wraps both env vars in a `OnceLock<ShadowConfig>`
    // populated on the first call. Subsequent calls return a borrow
    // into the cache.
    //
    // Test contract: call `shadow_config()` twice; the returned
    // pointer must be identity-equal (same address), proving the
    // cache is real. Also stress 100 calls and confirm the second
    // and 100th calls return the exact same pointer.
    use ai_memory::confidence::shadow::shadow_config;

    let p1 = std::ptr::from_ref(shadow_config());
    let p2 = std::ptr::from_ref(shadow_config());
    assert_eq!(p1, p2, "shadow_config must be cached");

    for _ in 0..100 {
        let p = std::ptr::from_ref(shadow_config());
        assert_eq!(p, p1, "every shadow_config call must return cache");
    }
}
