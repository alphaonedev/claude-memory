// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.x Form 6 (issue #759) — MemoryKind Batman-vocabulary
//! integration tests.
//!
//! Pins the seven externally-observable contracts added in this PR:
//!
//!  1. All ten variants of [`MemoryKind`] serialize and deserialize
//!     round-trip on JSON.
//!  2. Backward-compat — a row written before this change (no
//!     `memory_kind` field on the JSON payload) reads as
//!     `Observation`.
//!  3. Recall `kinds=[Concept]` filter returns only Concept-kind
//!     memories.
//!  4. Multi-kind filter `kinds=[Concept, Claim]` returns the union
//!     (OR-of-kinds).
//!  5. The auto-classify regex pass produces plausible kinds on a
//!     golden set of inputs.
//!  6. With `auto_classify` set to `Off`, the substrate keeps the
//!     caller-supplied kind verbatim.
//!  7. Capabilities v3 emits the new `memory_kind_vocab` block with
//!     the full 10-variant vocabulary and the auto-classify mode
//!     enum.

#![allow(clippy::doc_markdown)]

use ai_memory::config::{CapabilityMemoryKindVocab, FeatureTier, TierConfig};
use ai_memory::hooks::pre_store::{classify_by_regex, maybe_auto_classify};
use ai_memory::mcp::{handle_capabilities_with_conn_v3, handle_recall};
use ai_memory::models::{Memory, MemoryKind, MemoryKindAutoClassify, Tier};
use ai_memory::profile::Profile;
use chrono::Utc;
use rusqlite::Connection;
use serde_json::{Value, json};

// ─────────────────────────────────────────────────────────────────────────────
// Fixtures
// ─────────────────────────────────────────────────────────────────────────────

fn open_db() -> Connection {
    ai_memory::db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

fn make_mem(namespace: &str, title: &str, content: &str, kind: MemoryKind) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        tags: vec!["form-6-test".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({"agent_id": "test-form-6"}),
        reflection_depth: 0,
        memory_kind: kind,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1: every variant round-trips through serde
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn all_ten_variants_round_trip_through_serde() {
    let wires = [
        ("observation", MemoryKind::Observation),
        ("reflection", MemoryKind::Reflection),
        ("persona", MemoryKind::Persona),
        ("concept", MemoryKind::Concept),
        ("entity", MemoryKind::Entity),
        ("claim", MemoryKind::Claim),
        ("relation", MemoryKind::Relation),
        ("event", MemoryKind::Event),
        ("conversation", MemoryKind::Conversation),
        ("decision", MemoryKind::Decision),
    ];
    for (wire, variant) in wires {
        assert_eq!(MemoryKind::from_str(wire), Some(variant));
        assert_eq!(variant.as_str(), wire);

        // serde JSON round-trip
        let v = serde_json::to_value(variant).unwrap();
        assert_eq!(v, Value::String(wire.to_string()));
        let back: MemoryKind = serde_json::from_value(v).unwrap();
        assert_eq!(back, variant);
    }
}

#[test]
fn memory_kind_all_returns_full_vocabulary() {
    let all = MemoryKind::all();
    assert_eq!(all.len(), 10, "Form 6 ships 10 variants total");
    // First three are the v0.7.0 lifecycle variants in declaration
    // order — the L1-1 / QW-2 vocabulary that pre-dates Form 6.
    assert_eq!(all[0], MemoryKind::Observation);
    assert_eq!(all[1], MemoryKind::Reflection);
    assert_eq!(all[2], MemoryKind::Persona);
    // Last seven are Form 6 in declaration order.
    assert_eq!(all[3], MemoryKind::Concept);
    assert_eq!(all[9], MemoryKind::Decision);
}

#[test]
fn memory_kind_parse_csv_drops_unknown_and_dedups() {
    let parsed = MemoryKind::parse_csv("concept, claim, unknown, concept,relation").unwrap();
    assert_eq!(parsed.len(), 3);
    assert!(parsed.contains(&MemoryKind::Concept));
    assert!(parsed.contains(&MemoryKind::Claim));
    assert!(parsed.contains(&MemoryKind::Relation));
}

#[test]
fn memory_kind_parse_csv_returns_none_when_empty_or_all_unknown() {
    assert_eq!(MemoryKind::parse_csv(""), None);
    assert_eq!(MemoryKind::parse_csv("   "), None);
    assert_eq!(MemoryKind::parse_csv("not-a-kind,also-bogus"), None);
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2: backward compat — payload without memory_kind reads as Observation
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn pre_form6_payload_without_kind_field_reads_as_observation() {
    let json = json!({
        "id": "old-mem",
        "tier": "mid",
        "namespace": "ns",
        "title": "t",
        "content": "c",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "access_count": 0,
        "created_at": "2024-01-01T00:00:00Z",
        "updated_at": "2024-01-01T00:00:00Z",
        "metadata": {},
        // No memory_kind on the wire — should default.
    });
    let m: Memory = serde_json::from_value(json).expect("deserialize must succeed");
    assert_eq!(m.memory_kind, MemoryKind::Observation);
}

#[test]
fn future_unknown_kind_string_still_parses_via_storage_layer_fallback() {
    // A future variant emitted by a newer client to an older binary
    // would read as `Observation` via the row_to_memory fallback. We
    // exercise the parse path directly here since from_str is the
    // anchor.
    assert_eq!(MemoryKind::from_str("future_variant_v100"), None);
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3 + 4: recall kinds filter — single + multi
// ─────────────────────────────────────────────────────────────────────────────

fn seed_mixed_kinds(conn: &Connection) {
    // Seed memories spanning a few Batman variants under the same
    // namespace so the recall filter has something to discriminate.
    ai_memory::db::insert(
        conn,
        &make_mem(
            "form6-ns",
            "concept-token",
            "ownership is_a Rust borrow-checker rule needle",
            MemoryKind::Concept,
        ),
    )
    .unwrap();
    ai_memory::db::insert(
        conn,
        &make_mem(
            "form6-ns",
            "claim-token",
            "we claim the GC scheduler is starvation-free needle",
            MemoryKind::Claim,
        ),
    )
    .unwrap();
    ai_memory::db::insert(
        conn,
        &make_mem(
            "form6-ns",
            "entity-token",
            "acme corp is a service provider needle",
            MemoryKind::Entity,
        ),
    )
    .unwrap();
    ai_memory::db::insert(
        conn,
        &make_mem(
            "form6-ns",
            "obs-token",
            "an observation about something needle",
            MemoryKind::Observation,
        ),
    )
    .unwrap();
}

#[test]
fn recall_kinds_single_filter_returns_only_matching_kind() {
    let conn = open_db();
    seed_mixed_kinds(&conn);
    let ttl = ai_memory::config::ResolvedTtl::default();
    let scoring = ai_memory::config::ResolvedScoring::default();
    // Sanity: without a filter, all four seeded rows surface.
    let baseline = handle_recall(
        &conn,
        &json!({
            "context": "needle",
            "namespace": "form6-ns",
        }),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("baseline recall must succeed");
    assert!(
        baseline["count"].as_u64().unwrap_or_default() >= 1,
        "baseline (no kind filter) must return rows; got: {baseline}"
    );
    let baseline_kinds: Vec<String> = baseline["memories"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["memory_kind"].as_str().map(str::to_string))
        .collect();
    assert!(
        baseline_kinds.contains(&"concept".to_string()),
        "baseline must include concept row; got kinds: {baseline_kinds:?}"
    );

    let resp = handle_recall(
        &conn,
        &json!({
            "context": "needle",
            "namespace": "form6-ns",
            "kinds": ["concept"],
        }),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall must succeed");
    let memories = resp["memories"].as_array().expect("memories array");
    assert!(
        !memories.is_empty(),
        "should find the concept row; got: {resp}"
    );
    for m in memories {
        assert_eq!(
            m["memory_kind"].as_str(),
            Some("concept"),
            "kinds=[concept] filter must return only concept rows; got: {m}"
        );
    }
}

#[test]
fn recall_kinds_multi_filter_returns_or_of_kinds() {
    let conn = open_db();
    seed_mixed_kinds(&conn);
    let ttl = ai_memory::config::ResolvedTtl::default();
    let scoring = ai_memory::config::ResolvedScoring::default();
    let resp = handle_recall(
        &conn,
        &json!({
            "context": "needle",
            "namespace": "form6-ns",
            "kinds": ["concept", "claim"],
        }),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall must succeed");
    let memories = resp["memories"].as_array().expect("memories array");
    assert!(memories.len() >= 2, "should see concept + claim");
    for m in memories {
        let k = m["memory_kind"].as_str().expect("kind present");
        assert!(
            k == "concept" || k == "claim",
            "multi-kind filter must return only concept or claim; got: {k}"
        );
    }
}

#[test]
fn recall_kinds_csv_string_form_also_filters() {
    let conn = open_db();
    seed_mixed_kinds(&conn);
    let ttl = ai_memory::config::ResolvedTtl::default();
    let scoring = ai_memory::config::ResolvedScoring::default();
    let resp = handle_recall(
        &conn,
        &json!({
            "context": "needle",
            "namespace": "form6-ns",
            "kinds": "concept,claim",
        }),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall must succeed");
    let memories = resp["memories"].as_array().expect("memories array");
    for m in memories {
        let k = m["memory_kind"].as_str().expect("kind present");
        assert!(
            k == "concept" || k == "claim",
            "CSV form must produce same OR-of-kinds; got: {k}"
        );
    }
}

#[test]
fn recall_kinds_all_keyword_is_treated_as_no_filter() {
    let conn = open_db();
    seed_mixed_kinds(&conn);
    let ttl = ai_memory::config::ResolvedTtl::default();
    let scoring = ai_memory::config::ResolvedScoring::default();
    // With kinds:"all" we expect to see at least one row of each kind
    // (subject to relevance of the "needle" FTS query).
    let resp = handle_recall(
        &conn,
        &json!({
            "context": "needle",
            "namespace": "form6-ns",
            "kinds": "all",
        }),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall must succeed");
    let count = resp["count"].as_u64().unwrap_or_default();
    assert!(
        count >= 4,
        "kinds=all should not filter; want all 4 seeded rows, got {count}"
    );
}

#[test]
fn recall_kinds_unknown_values_dropped_silently() {
    let conn = open_db();
    seed_mixed_kinds(&conn);
    let ttl = ai_memory::config::ResolvedTtl::default();
    let scoring = ai_memory::config::ResolvedScoring::default();
    // ["future_variant"] yields an empty parsed set ⇒ treated as "no
    // filter", same as omission. (Documented forward-compat
    // behaviour.) The recall should still return rows.
    let resp = handle_recall(
        &conn,
        &json!({
            "context": "needle",
            "namespace": "form6-ns",
            "kinds": ["future_variant"],
        }),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall must succeed");
    // Treated as "no filter" — returns all seeded rows.
    assert!(resp["count"].as_u64().unwrap_or_default() >= 4);
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 5: auto-classify regex golden set
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn auto_classify_regex_golden_set() {
    let cases: &[(&str, &str, MemoryKind)] = &[
        // Concept
        (
            "ownership",
            "ownership is_a Rust borrow-checker rule",
            MemoryKind::Concept,
        ),
        (
            "typing",
            "typing refers to the static type system",
            MemoryKind::Concept,
        ),
        // Entity
        (
            "acme corp",
            "Acme corp is a service provider in our chain",
            MemoryKind::Entity,
        ),
        // Claim
        (
            "posture",
            "We claim that the GC scheduler is starvation-free",
            MemoryKind::Claim,
        ),
        // Relation
        (
            "subsystem A",
            "A depends on B for token expiry",
            MemoryKind::Relation,
        ),
        // Event
        (
            "deploy",
            "the cutover happened at 14:32 UTC",
            MemoryKind::Event,
        ),
        // Conversation (speaker-tag form)
        (
            "chat",
            "Alice: should we deploy?\nBob: yes",
            MemoryKind::Conversation,
        ),
        // Decision
        (
            "api migration",
            "We decided to deprecate v1 by Q3",
            MemoryKind::Decision,
        ),
    ];
    for (title, content, expected) in cases {
        let v = classify_by_regex(title, content);
        assert_eq!(
            v,
            Some(*expected),
            "title={title:?} content={content:?} expected={expected:?} got={v:?}"
        );
    }
}

#[test]
fn auto_classify_regex_miss_returns_none() {
    assert_eq!(
        classify_by_regex("note", "just a stray thought without taxonomic signal"),
        None
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 6: auto-classify Off mode preserves caller-supplied kind
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn auto_classify_off_preserves_caller_supplied_kind() {
    let mut m = make_mem(
        "ns",
        "A depends on B",
        "would be Relation under RegexOnly",
        MemoryKind::Claim,
    );
    let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::Off));
    assert_eq!(verdict, MemoryKind::Claim);
    assert_eq!(m.memory_kind, MemoryKind::Claim);
}

#[test]
fn auto_classify_off_observation_stays_observation_even_with_signal() {
    // Off mode: substrate stays quiet even when content carries a
    // strong signal that RegexOnly would catch.
    let mut m = make_mem(
        "ns",
        "deploy",
        "the cutover happened at 14:32 UTC",
        MemoryKind::Observation,
    );
    let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::Off));
    assert_eq!(verdict, MemoryKind::Observation);
}

#[test]
fn auto_classify_regex_only_observations_get_reclassified() {
    let mut m = make_mem(
        "ns",
        "deploy",
        "the cutover happened at 14:32 UTC",
        MemoryKind::Observation,
    );
    let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::RegexOnly));
    assert_eq!(verdict, MemoryKind::Event);
    assert_eq!(m.memory_kind, MemoryKind::Event);
}

#[test]
fn auto_classify_regex_only_keeps_caller_supplied_non_default() {
    // Even under RegexOnly, a caller-supplied non-default kind wins.
    let mut m = make_mem(
        "ns",
        "deploy",
        "the cutover happened at 14:32 UTC",
        MemoryKind::Decision,
    );
    let verdict = maybe_auto_classify(&mut m, Some(MemoryKindAutoClassify::RegexOnly));
    assert_eq!(verdict, MemoryKind::Decision);
    assert_eq!(m.memory_kind, MemoryKind::Decision);
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 7: capabilities v3 carries the memory_kind_vocab block
// ─────────────────────────────────────────────────────────────────────────────

fn semantic_tier() -> TierConfig {
    FeatureTier::Semantic.config()
}

#[test]
fn cap_v3_form6_carries_memory_kind_vocab_block() {
    let tier_config = semantic_tier();
    let conn = open_db();
    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::core(),
        None,
        None,
        None,
    )
    .expect("v3 capabilities serialize");

    let vocab = val["memory_kind_vocab"]
        .as_object()
        .expect("memory_kind_vocab block must be present under v3");
    let vocabulary = vocab["vocabulary"].as_array().expect("vocabulary array");
    let names: Vec<&str> = vocabulary.iter().filter_map(Value::as_str).collect();
    // Compile-anchored — the full 10-variant Batman taxonomy.
    assert_eq!(names.len(), 10);
    for required in [
        "observation",
        "reflection",
        "persona",
        "concept",
        "entity",
        "claim",
        "relation",
        "event",
        "conversation",
        "decision",
    ] {
        assert!(
            names.contains(&required),
            "memory_kind_vocab.vocabulary must include {required}; got: {names:?}"
        );
    }
    assert_eq!(vocab["recall_filter"].as_str(), Some("implemented"));
    assert_eq!(vocab["cli_filter"].as_str(), Some("implemented"));
    assert_eq!(vocab["auto_classify"].as_str(), Some("implemented"));
    let modes = vocab["auto_classify_modes"]
        .as_array()
        .expect("modes array");
    let modes: Vec<&str> = modes.iter().filter_map(Value::as_str).collect();
    assert_eq!(modes, vec!["off", "regex_only", "regex_then_llm"]);
}

#[test]
fn cap_v3_form6_capability_struct_current_matches_enum() {
    // The CapabilityMemoryKindVocab::current() vocabulary is built from
    // MemoryKind::all() at compile time. Verify the static enum-derived
    // list matches the snapshot the capability returns.
    let surface = CapabilityMemoryKindVocab::current();
    let enum_names: Vec<String> = MemoryKind::all()
        .iter()
        .map(|k| k.as_str().to_string())
        .collect();
    assert_eq!(
        surface.vocabulary, enum_names,
        "memory_kind_vocab.vocabulary must mirror MemoryKind::all() in declaration order"
    );
}

#[test]
fn cap_v3_form6_legacy_v3_payload_without_memory_kind_vocab_still_parses() {
    // A pre-Form-6 v3 envelope captured in the wild MUST still
    // deserialize — the new `memory_kind_vocab` field carries
    // `#[serde(default = "default_capability_memory_kind_vocab")]`
    // so an older payload round-trips into a struct with the current-
    // implementation snapshot filled in.
    let pre_form6_json = json!({
        "schema_version": "3",
        "summary": "pre-Form-6 summary",
        "to_describe_to_user": "pre-Form-6 describe",
        "tools": [],
        "tier": "semantic",
        "version": "0.7.0",
        "features": {
            "keyword_search": true,
            "semantic_search": true,
            "hybrid_recall": true,
            "query_expansion": false,
            "auto_consolidation": false,
            "auto_tagging": false,
            "contradiction_analysis": false,
            "cross_encoder_reranking": false,
            "memory_reflection": {"planned": false, "version": "v0.7.0", "enabled": true},
            "embedder_loaded": false,
            "recall_mode_active": "disabled",
            "reranker_active": "off",
            "reflection_boost": {"boost": 1.2, "per_depth_increment": 0.05, "max_depth_cap": 3}
        },
        "models": {"embedding": "none", "embedding_dim": 0, "llm": "none", "cross_encoder": "none"},
        "permissions": {"mode": "advisory", "active_rules": 0},
        "hooks": {"registered_count": 0},
        "compaction": {"planned": true, "version": "v0.8+", "enabled": false},
        "approval": {"pending_requests": 0},
        "transcripts": {"planned": true, "version": "v0.7+", "enabled": false},
        "memory_kinds": ["observation", "reflection"]
    });
    let back: ai_memory::config::CapabilitiesV3 = serde_json::from_value(pre_form6_json)
        .expect("pre-Form-6 v3 payload must still parse with default memory_kind_vocab");
    assert_eq!(back.memory_kind_vocab, CapabilityMemoryKindVocab::current());
}
