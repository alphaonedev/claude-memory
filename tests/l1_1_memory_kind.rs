// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! L1-1 (v0.7.0) — `MemoryKind::Reflection` typed enum integration tests.
//!
//! Pins the four externally-observable contracts added in this PR:
//!
//!  1. The `memory_reflect` substrate (`db::reflect`) stores the resulting
//!     memory with `memory_kind = 'reflection'`.
//!  2. `db::get` round-trips `memory_kind` correctly for both variants.
//!  3. `Capabilities::memory_kinds` field reports both recognised values.
//!  4. Serde round-trip: `Memory` with `memory_kind = Reflection` survives
//!     JSON serialisation → deserialisation with the variant preserved.
//!
//! Unit tests for the SQLite-layer `memories_by_kind` helper and the
//! migration v30 backfill SQL live in `src/storage/mod.rs` (the internal
//! `#[cfg(test)]` module), where they have direct access to the
//! `pub(crate)` function.

#![allow(clippy::doc_markdown)]

use ai_memory::config::{FeatureTier, TierConfig};
use ai_memory::db::{self, ReflectInput};
use ai_memory::models::ConfidenceSource;
use ai_memory::models::{Memory, MemoryKind, Tier};
use chrono::Utc;
use rusqlite::Connection;

// ─────────────────────────────────────────────────────────────────────────────
// Fixtures
// ─────────────────────────────────────────────────────────────────────────────

fn open_db() -> Connection {
    ai_memory::db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

fn make_obs(namespace: &str, title: &str) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("fixture content for {title}"),
        tags: vec!["l1-1-test".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "test-l1-1"}),
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

fn reflect_input(source_ids: Vec<String>, namespace: &str, title: &str) -> ReflectInput {
    ReflectInput {
        source_ids,
        title: title.to_string(),
        content: format!("synthesised reflection: {title}"),
        namespace: Some(namespace.to_string()),
        tier: Tier::Mid,
        tags: vec!["l1-1-reflection".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "claude".to_string(),
        agent_id: "test-l1-1".to_string(),
        metadata: serde_json::json!({}),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1: `memory_reflect` substrate sets `memory_kind = Reflection`
// ─────────────────────────────────────────────────────────────────────────────

/// `db::reflect` must store the resulting memory with
/// `memory_kind = MemoryKind::Reflection`.  This pins the key behavioural
/// change of L1-1: the substrate that creates reflections now types them
/// correctly rather than leaving them as default `Observation`.
#[test]
fn reflect_sets_memory_kind_to_reflection() {
    let conn = open_db();
    let source = make_obs("l1-ns", "source-memory");
    let source_id = db::insert(&conn, &source).expect("insert source");

    let input = reflect_input(vec![source_id], "l1-ns", "test-reflection");
    let outcome = db::reflect(&conn, &input).expect("reflect must succeed");

    let stored = db::get(&conn, &outcome.id)
        .expect("get must succeed")
        .expect("reflection must be stored");

    assert_eq!(
        stored.memory_kind,
        MemoryKind::Reflection,
        "db::reflect must set memory_kind=Reflection; got {:?}",
        stored.memory_kind
    );
}

/// Ordinary `db::insert` with `memory_kind=Observation` leaves the field
/// as Observation (sanity-check for the contrast above).
#[test]
fn insert_observation_preserves_kind() {
    let conn = open_db();
    let mem = make_obs("l1-ns", "plain-observation");
    let id = db::insert(&conn, &mem).expect("insert");
    let got = db::get(&conn, &id).expect("get").expect("must exist");
    assert_eq!(
        got.memory_kind,
        MemoryKind::Observation,
        "plain insert must preserve MemoryKind::Observation"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2: `memory_kind` field round-trips through JSON serialisation
// ─────────────────────────────────────────────────────────────────────────────

/// A `Memory` with `memory_kind = MemoryKind::Reflection` survives a full
/// JSON serialise → deserialise cycle.  Pins the `#[serde(rename_all =
/// "snake_case")]` contract on the enum so federation peers and HTTP
/// clients see `"reflection"` on the wire.
#[test]
fn memory_kind_serde_roundtrip_reflection() {
    let now = Utc::now().to_rfc3339();
    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: "serde-ns".to_string(),
        title: "serde-roundtrip".to_string(),
        content: "content".to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
        reflection_depth: 1,
        memory_kind: MemoryKind::Reflection,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    };

    let json = serde_json::to_string(&mem).expect("serialize");
    // The wire value must be the snake_case string "reflection".
    assert!(
        json.contains(r#""memory_kind":"reflection""#),
        "serialised Memory must use snake_case wire value; json={json}"
    );

    let back: Memory = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(
        back.memory_kind,
        MemoryKind::Reflection,
        "deserialised memory_kind must be Reflection"
    );
}

/// A JSON payload without the `memory_kind` field (pre-L1-1 peer, federation
/// peer that doesn't emit the field) must deserialise to the `Observation`
/// default (via `#[serde(default)]`).
#[test]
fn memory_kind_missing_field_defaults_to_observation() {
    // Minimal JSON that omits memory_kind.
    let json = r#"{
        "id": "abc",
        "tier": "mid",
        "namespace": "ns",
        "title": "old-peer",
        "content": "c",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "import",
        "access_count": 0,
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "metadata": {},
        "reflection_depth": 0
    }"#;

    let mem: Memory = serde_json::from_str(json).expect("deserialize without memory_kind");
    assert_eq!(
        mem.memory_kind,
        MemoryKind::Observation,
        "missing memory_kind field must default to Observation"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3: `Capabilities::memory_kinds` field reports both supported values
// ─────────────────────────────────────────────────────────────────────────────

/// `TierConfig::capabilities()` must include a `memory_kinds` field that
/// lists `["observation", "reflection"]`, irrespective of feature tier.
#[test]
fn capabilities_reports_memory_kinds_field() {
    for tier in &[
        FeatureTier::Keyword,
        FeatureTier::Semantic,
        FeatureTier::Smart,
    ] {
        let config = tier.config();
        let caps = config.capabilities();

        assert!(
            !caps.memory_kinds.is_empty(),
            "memory_kinds must be non-empty on tier {tier:?}"
        );
        assert!(
            caps.memory_kinds.contains(&"observation".to_string()),
            "memory_kinds must include 'observation' on tier {tier:?}"
        );
        assert!(
            caps.memory_kinds.contains(&"reflection".to_string()),
            "memory_kinds must include 'reflection' on tier {tier:?}"
        );
    }
}

/// `Capabilities` serialises `memory_kinds` as a JSON array.
#[test]
fn capabilities_memory_kinds_serialises_to_json_array() {
    let tier_config: TierConfig = FeatureTier::Keyword.config();
    let caps = tier_config.capabilities();
    let json = serde_json::to_value(&caps).expect("serialize capabilities");
    let kinds = json["memory_kinds"]
        .as_array()
        .expect("memory_kinds must be a JSON array");
    assert!(
        kinds.iter().any(|v| v.as_str() == Some("observation")),
        "JSON array must contain 'observation'"
    );
    assert!(
        kinds.iter().any(|v| v.as_str() == Some("reflection")),
        "JSON array must contain 'reflection'"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 4: `MemoryKind` enum API surface
// ─────────────────────────────────────────────────────────────────────────────

/// `MemoryKind::as_str()` and `MemoryKind::from_str()` are inverse functions.
#[test]
fn memory_kind_as_str_and_from_str_are_inverses() {
    assert_eq!(MemoryKind::Observation.as_str(), "observation");
    assert_eq!(MemoryKind::Reflection.as_str(), "reflection");

    assert_eq!(
        MemoryKind::from_str("observation"),
        Some(MemoryKind::Observation)
    );
    assert_eq!(
        MemoryKind::from_str("reflection"),
        Some(MemoryKind::Reflection)
    );
    assert_eq!(MemoryKind::from_str("unknown"), None);
    assert_eq!(MemoryKind::from_str(""), None);
}

/// `MemoryKind::default()` is `Observation`.
#[test]
fn memory_kind_default_is_observation() {
    assert_eq!(MemoryKind::default(), MemoryKind::Observation);
}
