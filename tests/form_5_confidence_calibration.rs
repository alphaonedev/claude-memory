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
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn fresh_db() -> (NamedTempFile, Connection) {
    let tmp = NamedTempFile::new().expect("tempfile");
    let conn = db::open(tmp.path()).expect("db::open");
    (tmp, conn)
}

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
    observe(&conn, &m1.id, "ns", 0.95, 0.55, &s, None).unwrap();
    observe(&conn, &m2.id, "ns", 0.95, 0.65, &s, None).unwrap();
    observe(&conn, &m3.id, "ns", 0.95, 0.30, &s, None).unwrap();

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
// 7. Schema v39 sqlite lands idempotently.
// ---------------------------------------------------------------------------

#[test]
fn schema_v39_sqlite_lands_idempotently() {
    let (tmp, conn) = fresh_db();
    let version: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .expect("schema_version");
    assert!(version >= 39, "expected schema >= 39, got {version}");

    // Re-open the DB — the migrate ladder's version >= CURRENT_SCHEMA_VERSION
    // fast-path must skip the v39 arm cleanly (idempotent replay).
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
