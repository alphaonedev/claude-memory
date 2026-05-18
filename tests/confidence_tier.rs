// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Gap 4 (issue #887) — derived `ConfidenceTier` enum
//! regression suite.
//!
//! Acceptance criteria from the playbook:
//!
//! 1. `Memory::confidence_tier()` returns `Confirmed` (>=0.95),
//!    `Likely` (0.7..0.95), `Ambiguous` (<0.7) for three sample
//!    rows at 0.99 / 0.85 / 0.5.
//! 2. `memory_recall(confidence_tier="ambiguous")` returns only the
//!    0.5-confidence row.
//! 3. `memory_capabilities` surfaces the threshold map under the
//!    `confidence_calibration.tier_thresholds` block so callers can
//!    filter without re-deriving the breakpoints.

use ai_memory::config::CapabilityConfidenceCalibration;
use ai_memory::models::{ConfidenceTier, Memory};
use rusqlite::params;

fn fresh_db() -> rusqlite::Connection {
    ai_memory::storage::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

#[test]
fn gap4_confidence_tier_thresholds_pinned() {
    // The substrate-side mapping is load-bearing. Pin all three
    // boundary cases — anything below `LIKELY_MIN` is ambiguous,
    // anything at-or-above `CONFIRMED_MIN` is confirmed, the rest
    // is likely.
    let confirmed = Memory {
        confidence: 0.99,
        ..Memory::default()
    };
    let likely = Memory {
        confidence: 0.85,
        ..Memory::default()
    };
    let ambiguous = Memory {
        confidence: 0.5,
        ..Memory::default()
    };
    assert_eq!(confirmed.confidence_tier(), ConfidenceTier::Confirmed);
    assert_eq!(likely.confidence_tier(), ConfidenceTier::Likely);
    assert_eq!(ambiguous.confidence_tier(), ConfidenceTier::Ambiguous);

    // Boundary cases — exclusive vs inclusive matter.
    assert_eq!(
        Memory {
            confidence: 0.95,
            ..Memory::default()
        }
        .confidence_tier(),
        ConfidenceTier::Confirmed,
        "0.95 is inclusive lower bound of confirmed"
    );
    assert_eq!(
        Memory {
            confidence: 0.7,
            ..Memory::default()
        }
        .confidence_tier(),
        ConfidenceTier::Likely,
        "0.7 is inclusive lower bound of likely"
    );
    // NaN ⇒ Ambiguous (conservative fallback for corrupt input).
    assert_eq!(
        ConfidenceTier::from_confidence(f64::NAN),
        ConfidenceTier::Ambiguous,
    );
}

#[test]
fn gap4_confidence_tier_parse_roundtrips_via_str() {
    for tier in [
        ConfidenceTier::Confirmed,
        ConfidenceTier::Likely,
        ConfidenceTier::Ambiguous,
    ] {
        let s = tier.as_str();
        assert_eq!(ConfidenceTier::parse(s), Some(tier));
    }
    assert_eq!(
        ConfidenceTier::parse("  CONFIRMED  "),
        Some(ConfidenceTier::Confirmed),
        "case-insensitive + whitespace-trimming"
    );
    assert_eq!(ConfidenceTier::parse("bogus"), None);
}

#[test]
fn gap4_capabilities_carry_tier_threshold_block() {
    // The `memory_capabilities` v3 calibration surface MUST carry
    // the threshold map so external callers can filter / score
    // against the substrate's official breakpoints.
    let surface = CapabilityConfidenceCalibration::current();
    let thresholds = &surface.tier_thresholds;
    assert!(
        (thresholds.confirmed - ConfidenceTier::CONFIRMED_MIN).abs() < f64::EPSILON,
        "confirmed threshold matches model constant"
    );
    assert!(
        (thresholds.likely - ConfidenceTier::LIKELY_MIN).abs() < f64::EPSILON,
        "likely threshold matches model constant"
    );
    assert!(
        (thresholds.ambiguous - 0.0).abs() < f64::EPSILON,
        "ambiguous is the implicit floor"
    );
}

#[test]
fn gap4_recall_confidence_tier_filter_returns_only_ambiguous() {
    use ai_memory::config::{ResolvedScoring, ResolvedTtl};

    let conn = fresh_db();
    // Seed three memories at the canonical 0.99 / 0.85 / 0.5 marks.
    let now = chrono::Utc::now().to_rfc3339();
    for (id, conf) in &[("m-conf", 0.99_f64), ("m-likely", 0.85), ("m-amb", 0.5)] {
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, confidence, created_at, updated_at) \
             VALUES (?1, 'long', 'g4', ?2, ?3, ?4, ?5, ?5)",
            params![id, format!("title-{id}"), format!("payload {id}"), conf, now],
        )
        .unwrap();
    }
    // FTS5 sync.
    conn.execute(
        "INSERT INTO memories_fts(rowid, title, content) \
         SELECT rowid, title, content FROM memories WHERE namespace = 'g4'",
        [],
    )
    .ok();

    let ttl = ResolvedTtl::default();
    let scoring = ResolvedScoring::default();

    // Filter by `ambiguous` ⇒ only the 0.5-confidence row should
    // survive.
    let resp = ai_memory::mcp::handle_recall(
        &conn,
        &serde_json::json!({
            "context": "payload",
            "namespace": "g4",
            "confidence_tier": "ambiguous",
        }),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall ok");
    let memories = resp["memories"]
        .as_array()
        .expect("recall response carries memories array");
    assert_eq!(
        memories.len(),
        1,
        "ambiguous filter must return exactly the 0.5 row"
    );
    assert_eq!(memories[0]["id"].as_str(), Some("m-amb"));
    assert_eq!(
        memories[0]["confidence_tier"].as_str(),
        Some("ambiguous"),
        "row carries the derived tier when verbose_provenance=true (default)"
    );

    // Filter by `confirmed` ⇒ only the 0.99 row.
    let resp_conf = ai_memory::mcp::handle_recall(
        &conn,
        &serde_json::json!({
            "context": "payload",
            "namespace": "g4",
            "confidence_tier": "confirmed",
        }),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall ok");
    let conf_memories = resp_conf["memories"].as_array().unwrap();
    assert_eq!(conf_memories.len(), 1);
    assert_eq!(conf_memories[0]["id"].as_str(), Some("m-conf"));
}
