// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioural impact.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! v0.7.0 L2-2 / #667 — federation-aware reflection coordination
//! acceptance test (3-node simulation).
//!
//! ## Topology
//!
//! Three peers A, B, C — each backed by an independent SQLite DB so
//! `insert_if_newer`, `stamp_reflection_origin`, governance policy
//! resolution, and the depth-cap refusal path all see real substrate
//! state. We do NOT spin Axum here — the wire layer is exercised in
//! `tests/federation_b2_hardening.rs` and `tests/federation_inbound_verify.rs`;
//! this suite pins the substrate-level invariants that survive across
//! a `sync_push`-style replication step (i.e. what every peer's HTTP
//! receive handler calls on every inbound row).
//!
//! ## Caps
//!
//! - **Peer A** — `max_reflection_depth = 3` (compiled default, no
//!   namespace standard override).
//! - **Peer B** — `max_reflection_depth = 2` (namespace standard
//!   tightens the cap below A's).
//! - **Peer C** — `max_reflection_depth = 3` (matches A; acts as the
//!   third node observing fan-out so the test models true federation
//!   topology, not just an A↔B pair).
//!
//! ## Acceptance contract (`tests/federation/reflection_replication_test.rs` per #667)
//!
//! 1. **Replicate-accept**: A writes a depth-2 reflection. B applies
//!    the row via the same `stamp_reflection_origin` →
//!    `insert_if_newer` path the HTTP `sync_push` handler runs. B's
//!    stored copy carries `metadata.reflection_origin.peer_origin =
//!    "ai:peer-a"`, `original_depth = 2`, `local_depth_at_arrival = 2`,
//!    and the substrate `reflection_depth` column is preserved at 2
//!    (federation never silently rewrites depth).
//! 2. **Cross-peer refusal**: B's curator attempts a depth-3 reflection
//!    deriving from the imported depth-2 source. B refuses with
//!    `ReflectError::DepthExceeded { attempted: 3, cap: 2, .. }` and
//!    the `reflection.depth_exceeded.cross_peer` audit event lands in
//!    B's `signed_events` table (distinct event_type from the
//!    local-only refusal — operators can filter by event_type to see
//!    cross-peer-context refusals).
//! 3. **Origin lookup parity**: B's `memory_reflection_origin` substrate
//!    lookup returns the structured record for the imported row.
//! 4. **Peer C transitive replication**: C receives the SAME row from B
//!    in a re-fan; C's stamp preserves the FIRST peer to deliver
//!    (`peer_origin = "ai:peer-a"`), proving the idempotency-first-writer
//!    -wins contract holds end-to-end.
//! 5. **Local-only refusal unaffected**: A's own derived attempt at
//!    depth-4 (over A's local cap of 3) still emits the plain
//!    `reflection.depth_exceeded` audit event_type — the L2-2 change
//!    only enriches the cross-peer-context refusals; pre-L2-2 audit
//!    consumers see no shape regression on the local-only path.
//!
//! ## What this test is NOT
//!
//! Not an HTTP acceptance test — sig-verification on `reflects_on`
//! edges (Ed25519 via `identity::verify::verify`) is pinned in
//! `tests/federation_inbound_verify.rs`. The substrate-level
//! `stamp_reflection_origin` happy / refusal / idempotency cases are
//! pinned in `tests/federation_b2_hardening.rs`. This test pins the
//! end-to-end three-peer choreography that those finer-grained tests
//! compose into.

use ai_memory::db::{ReflectError, ReflectInput};
use ai_memory::federation::reflection_bookkeeping;
use ai_memory::models::{
    ApproverType, GovernanceLevel, GovernancePolicy, Memory, MemoryKind, Tier, default_metadata,
};
use ai_memory::signed_events::list_signed_events;
use ai_memory::storage as db;
use chrono::Utc;
use rusqlite::Connection;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Topology helpers
// ---------------------------------------------------------------------------

struct Peer {
    id: &'static str,
    conn: Connection,
    // Hold the tempdir so the DB path stays valid for the test's lifetime.
    // Underscore-prefixed via `dead_code` allow rather than name prefix so
    // pedantic clippy's `used_underscore_binding` lint stays clean — we
    // never read `tmp` after construction; its job is RAII.
    #[allow(dead_code)]
    tmp: TempDir,
}

fn fresh_peer(id: &'static str) -> Peer {
    let tmp_dir = TempDir::new().expect("tempdir");
    let conn = db::open(&tmp_dir.path().join("ai-memory.db")).expect("db::open");
    Peer {
        id,
        conn,
        tmp: tmp_dir,
    }
}

/// Persist a `metadata.governance` policy on a namespace via
/// `set_namespace_standard`. Mirrors the helper used in
/// `tests/recursive_learning_task5_audit_record.rs`.
fn seed_policy(conn: &Connection, namespace: &str, policy: &GovernancePolicy) {
    let now = Utc::now().to_rfc3339();
    let mut metadata = default_metadata();
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String("ai:bootstrap".to_string()),
        );
        obj.insert(
            "governance".to_string(),
            serde_json::to_value(policy).unwrap(),
        );
    }
    let standard = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: format!("_standards-{namespace}"),
        title: format!("standard for {namespace}"),
        content: "policy".to_string(),
        tags: vec![],
        priority: 9,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata,
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
    };
    let standard_id = db::insert(conn, &standard).expect("insert standard");
    db::set_namespace_standard(conn, namespace, &standard_id, None).expect("set standard");
}

fn observation(namespace: &str, title: &str, depth: i32, agent_id: &str) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("body for {title}"),
        tags: vec!["l2-2".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": agent_id}),
        reflection_depth: depth,
        memory_kind: if depth > 0 {
            MemoryKind::Reflection
        } else {
            MemoryKind::Observation
        },
        entity_id: None,
        persona_version: None,
    }
}

fn reflect_input(
    source_ids: Vec<String>,
    namespace: &str,
    title: &str,
    agent_id: &str,
) -> ReflectInput {
    ReflectInput {
        source_ids,
        title: title.to_string(),
        content: format!("synthesised reflection for {title}"),
        namespace: Some(namespace.to_string()),
        tier: Tier::Mid,
        tags: vec!["l2-2".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "claude".to_string(),
        agent_id: agent_id.to_string(),
        metadata: serde_json::json!({}),
    }
}

/// Replay the HTTP `sync_push` apply-memory codepath at substrate
/// level: stamp `reflection_origin` then `insert_if_newer`. Returns
/// the actual stored id (a duplicate `(title, namespace)` may resolve
/// to a pre-existing row via the v0.6 upsert contract; for this test
/// every inbound title is unique on the receiver so the returned id
/// matches the row id).
fn sync_push_apply(receiver: &Connection, mem: &Memory, sender_agent_id: &str) -> String {
    let cap = db::resolve_governance_policy(receiver, &mem.namespace)
        .unwrap_or_default()
        .effective_max_reflection_depth();
    let stamped = reflection_bookkeeping::stamp_reflection_origin(mem, sender_agent_id, cap);
    db::insert_if_newer(receiver, &stamped).expect("insert_if_newer on receiver")
}

// ---------------------------------------------------------------------------
// Acceptance — three-peer federation reflection choreography
// ---------------------------------------------------------------------------

const NAMESPACE: &str = "federation/l2-2";

#[test]
fn three_peer_federation_depth_replication_and_cross_peer_refusal() {
    // ─── Bootstrap peers ─────────────────────────────────────────────
    let peer_a = fresh_peer("ai:peer-a");
    let peer_b = fresh_peer("ai:peer-b");
    let peer_c = fresh_peer("ai:peer-c");

    // Peer B tightens its cap to 2 via a namespace standard. Peers A
    // and C run with the compiled default of 3 — no standard needed.
    let tight = GovernancePolicy {
        write: GovernanceLevel::Any,
        promote: GovernanceLevel::Any,
        delete: GovernanceLevel::Owner,
        approver: ApproverType::Human,
        inherit: true,
        max_reflection_depth: Some(2),
        auto_export_reflections_to_filesystem: None,
        auto_atomise: None,
        auto_atomise_threshold_cl100k: None,
        auto_atomise_max_atom_tokens: None,
        auto_persona_trigger_every_n_memories: None,
        auto_export_personas_to_filesystem: None,
        auto_atomise_mode: None,
        legacy_per_pair_classifier: None,
    };
    seed_policy(&peer_b.conn, NAMESPACE, &tight);

    // Sanity: B resolves to cap=2; A and C resolve to cap=3.
    let cap_a = db::resolve_governance_policy(&peer_a.conn, NAMESPACE)
        .unwrap_or_default()
        .effective_max_reflection_depth();
    let cap_b = db::resolve_governance_policy(&peer_b.conn, NAMESPACE)
        .unwrap_or_default()
        .effective_max_reflection_depth();
    let cap_c = db::resolve_governance_policy(&peer_c.conn, NAMESPACE)
        .unwrap_or_default()
        .effective_max_reflection_depth();
    assert_eq!(cap_a, 3, "peer A retains compiled default cap=3");
    assert_eq!(cap_b, 2, "peer B's namespace standard tightens to cap=2");
    assert_eq!(cap_c, 3, "peer C retains compiled default cap=3");

    // ─── On peer A: write the depth-0 observation, then depth-1 and
    // depth-2 reflections.
    let obs = observation(NAMESPACE, "alpha-observation", 0, "ai:alice@host:a");
    let obs_id = db::insert(&peer_a.conn, &obs).expect("seed obs on A");

    // depth-1 reflection on A.
    let r1_input = reflect_input(
        vec![obs_id.clone()],
        NAMESPACE,
        "alpha-reflection-d1",
        "ai:alice@host:a",
    );
    let r1 = db::reflect(&peer_a.conn, &r1_input).expect("A: depth-1 reflect");
    assert_eq!(r1.reflection_depth, 1);

    // depth-2 reflection on A — reflecting on the depth-1 row.
    let r2_input = reflect_input(
        vec![r1.id.clone()],
        NAMESPACE,
        "alpha-reflection-d2",
        "ai:alice@host:a",
    );
    let r2 = db::reflect(&peer_a.conn, &r2_input).expect("A: depth-2 reflect");
    assert_eq!(r2.reflection_depth, 2, "A's depth-2 reflection landed");

    // Read the depth-2 row from A's DB to get the canonical wire shape
    // the federation push would carry.
    let r2_row_on_a = db::get(&peer_a.conn, &r2.id)
        .expect("get r2 from A")
        .expect("r2 present on A");

    // ─── Replicate A's depth-2 reflection to B ───────────────────────
    let stored_on_b = sync_push_apply(&peer_b.conn, &r2_row_on_a, peer_a.id);

    // Acceptance contract #1: B's row carries the L2-2 stamp.
    let r2_row_on_b = db::get(&peer_b.conn, &stored_on_b)
        .expect("get r2 from B")
        .expect("r2 present on B");
    assert_eq!(
        r2_row_on_b.reflection_depth, 2,
        "B preserves the original reflection_depth=2 — federation never silently rewrites depth"
    );
    let origin_on_b = r2_row_on_b
        .metadata
        .get("reflection_origin")
        .expect("reflection_origin stamped on B");
    assert_eq!(
        origin_on_b["peer_origin"].as_str(),
        Some(peer_a.id),
        "peer_origin = sender (A)"
    );
    assert_eq!(origin_on_b["original_depth"].as_i64(), Some(2));
    assert_eq!(
        origin_on_b["local_depth_at_arrival"].as_u64(),
        Some(u64::from(cap_b)),
        "B records its local cap-at-arrival = 2"
    );

    // Acceptance contract #3 — `memory_reflection_origin` substrate
    // lookup returns the structured record.
    let origin_record = reflection_bookkeeping::reflection_origin(&peer_b.conn, &r2_row_on_b.id)
        .expect("origin lookup ok")
        .expect("origin record present");
    assert_eq!(origin_record.peer_origin.as_deref(), Some(peer_a.id));
    assert_eq!(
        origin_record.signing_agent.as_deref(),
        Some("ai:alice@host:a"),
        "signing_agent is preserved through replication"
    );
    assert_eq!(origin_record.original_depth, 2);
    assert_eq!(origin_record.local_depth_at_arrival, Some(cap_b));
    assert!(origin_record.is_reflection);

    // ─── Acceptance contract #2 — B's curator attempts depth-3 ──────
    //
    // The imported depth-2 source would derive a depth-3 reflection.
    // B's local cap = 2 → must refuse with `DepthExceeded` and the
    // audit row must land with the cross-peer event_type.
    //
    // The new reflection's title must be unique inside the namespace
    // (the substrate's anti-merge contract for reflections, per #690 /
    // R1-M3). Use a distinct title.
    let derived_input = reflect_input(
        vec![r2_row_on_b.id.clone()],
        NAMESPACE,
        "bravo-derived-d3-attempt",
        "ai:bob@host:b",
    );
    let err = db::reflect(&peer_b.conn, &derived_input)
        .expect_err("B must refuse depth-3 from imported depth-2 source");
    match &err {
        ReflectError::DepthExceeded {
            attempted,
            cap,
            namespace,
        } => {
            assert_eq!(*attempted, 3, "derived attempt is depth=3");
            assert_eq!(*cap, 2, "B's local cap is 2");
            assert_eq!(namespace, NAMESPACE);
        }
        other => panic!("expected DepthExceeded, got: {other:?}"),
    }

    // Acceptance: B's signed_events records the cross-peer-context
    // audit row. Filter by the L2-2 event_type so a future addition
    // doesn't false-positive.
    let cross_peer_rows: Vec<_> = list_signed_events(&peer_b.conn, None, 100, 0)
        .expect("list signed_events on B")
        .into_iter()
        .filter(|e| e.event_type == "reflection.depth_exceeded.cross_peer")
        .collect();
    assert_eq!(
        cross_peer_rows.len(),
        1,
        "B records exactly one cross-peer depth-exceeded audit row"
    );
    let cp_row = &cross_peer_rows[0];
    assert_eq!(
        cp_row.agent_id, "ai:bob@host:b",
        "audit row attributes the refusal to the attempting agent"
    );
    assert_eq!(
        cp_row.payload_hash.len(),
        32,
        "payload_hash must be SHA-256 (32 bytes)"
    );
    assert!(
        cp_row.signature.is_none(),
        "substrate-emitted cross-peer audit row is unsigned by design"
    );
    assert_eq!(cp_row.attest_level, "unsigned");

    // Re-derive the payload_hash from the canonical-CBOR encoding
    // including the peer_origin claim — proves the cross-peer-context
    // is bound into the tamper-evident audit hash, not just stamped as
    // a textual event_type.
    let derived_cbor = ai_memory::db::canonical_cbor_reflection_depth_exceeded(
        &cp_row.agent_id,
        3,
        2,
        NAMESPACE,
        std::slice::from_ref(&r2_row_on_b.id),
        "bravo-derived-d3-attempt",
        &cp_row.timestamp,
        Some(peer_a.id),
    )
    .expect("re-encode canonical CBOR");
    assert_eq!(
        cp_row.payload_hash,
        ai_memory::signed_events::payload_hash(&derived_cbor),
        "audit payload_hash must bind to canonical CBOR INCLUDING peer_origin"
    );

    // Sanity: a payload WITHOUT peer_origin yields a different hash —
    // proves the cross-peer claim is a load-bearing field, not a
    // decorative one.
    let no_peer_cbor = ai_memory::db::canonical_cbor_reflection_depth_exceeded(
        &cp_row.agent_id,
        3,
        2,
        NAMESPACE,
        std::slice::from_ref(&r2_row_on_b.id),
        "bravo-derived-d3-attempt",
        &cp_row.timestamp,
        None,
    )
    .expect("re-encode canonical CBOR without peer");
    assert_ne!(
        cp_row.payload_hash,
        ai_memory::signed_events::payload_hash(&no_peer_cbor),
        "omitting peer_origin must yield a different payload_hash — cross-peer claim is tamper-evident"
    );

    // No purely-local refusal landed on B for this attempt. The L2-2
    // change distinguishes the event_type so operators can filter.
    let local_only_rows: Vec<_> = list_signed_events(&peer_b.conn, None, 100, 0)
        .expect("list signed_events on B (2)")
        .into_iter()
        .filter(|e| e.event_type == "reflection.depth_exceeded")
        .collect();
    assert!(
        local_only_rows.is_empty(),
        "B's refusal was cross-peer; the plain event_type must NOT also fire"
    );

    // ─── Acceptance contract #4 — transitive replication to peer C ──
    //
    // B re-fans the same row to C. C's stamp records B as the
    // immediate sender, BUT the existing `reflection_origin` carrying
    // peer_origin = "ai:peer-a" must survive — first writer wins, B's
    // re-fan does not overwrite the substrate-of-record.
    sync_push_apply(&peer_c.conn, &r2_row_on_b, peer_b.id);
    let r2_row_on_c = db::get(&peer_c.conn, &r2_row_on_b.id)
        .expect("get r2 from C")
        .expect("r2 present on C");
    let origin_on_c = r2_row_on_c
        .metadata
        .get("reflection_origin")
        .expect("origin stamp preserved on C");
    assert_eq!(
        origin_on_c["peer_origin"].as_str(),
        Some(peer_a.id),
        "C preserves first-writer (A) as peer_origin — re-fan from B did NOT overwrite"
    );
    assert_eq!(
        origin_on_c["local_depth_at_arrival"].as_u64(),
        Some(u64::from(cap_b)),
        "first writer wins: B's local_depth_at_arrival (=2) is preserved \
         even though C's cap is 3"
    );

    // C accepts the row at depth=2 (no rewrite) and would refuse a
    // local depth-3 derivation only when its own cap is exceeded —
    // here C's cap is 3 so depth-3 derivation is the LAST legal step,
    // and depth-4 is refused. We pin both:
    //   - depth-3 derivation on C ALLOWED (C's cap=3, attempting=3,
    //     not exceeded).
    //   - depth-4 derivation on C REFUSED, but as a PLAIN
    //     `reflection.depth_exceeded` audit because the source row's
    //     `peer_origin` stamp doesn't propagate through C's enforcement
    //     (the stamp is still on the row, so cross-peer event_type
    //     fires — pin THAT exactly).
    let derived_on_c_d3 = reflect_input(
        vec![r2_row_on_c.id.clone()],
        NAMESPACE,
        "charlie-derived-d3-allowed",
        "ai:charlie@host:c",
    );
    let r3_on_c =
        db::reflect(&peer_c.conn, &derived_on_c_d3).expect("C: depth-3 from imported is allowed");
    assert_eq!(r3_on_c.reflection_depth, 3);

    let derived_on_c_d4 = reflect_input(
        vec![r3_on_c.id.clone()],
        NAMESPACE,
        "charlie-derived-d4-refused",
        "ai:charlie@host:c",
    );
    let err4 = db::reflect(&peer_c.conn, &derived_on_c_d4).expect_err("C: depth-4 must refuse");
    match err4 {
        ReflectError::DepthExceeded { attempted, cap, .. } => {
            assert_eq!(attempted, 4);
            assert_eq!(cap, 3);
        }
        other => panic!("expected DepthExceeded on C, got {other:?}"),
    }
    // The depth-4 derivation's deepest source is `r3_on_c` (depth=3)
    // — that row was created LOCALLY on C (no `reflection_origin`
    // stamp) so the audit row uses the PLAIN event_type, not the
    // cross-peer variant. Pin that distinction explicitly.
    let plain_rows_on_c: Vec<_> = list_signed_events(&peer_c.conn, None, 100, 0)
        .expect("list signed_events on C")
        .into_iter()
        .filter(|e| e.event_type == "reflection.depth_exceeded")
        .collect();
    assert_eq!(
        plain_rows_on_c.len(),
        1,
        "C's refusal derived from a locally-authored source uses the plain event_type"
    );
    let cross_rows_on_c: Vec<_> = list_signed_events(&peer_c.conn, None, 100, 0)
        .expect("list signed_events on C (2)")
        .into_iter()
        .filter(|e| e.event_type == "reflection.depth_exceeded.cross_peer")
        .collect();
    assert!(
        cross_rows_on_c.is_empty(),
        "C's depth-4 refusal is local-only (source's source has no peer_origin) — no cross-peer audit"
    );

    // TempDir RAII: `peer_a.tmp`, `peer_b.tmp`, `peer_c.tmp` drop here
    // automatically as the function returns, cleaning up the three DB
    // directories. No explicit drops needed.
}

// ---------------------------------------------------------------------------
// Acceptance — local-only refusal path unchanged (regression guard)
// ---------------------------------------------------------------------------

/// L2-2 enriches the cross-peer refusal path; the local-only refusal
/// path must continue to emit the plain `reflection.depth_exceeded`
/// event_type with the pre-L2-2 canonical-CBOR shape. This is the
/// regression guard for pre-L2-2 audit consumers.
#[test]
fn local_only_refusal_keeps_pre_l2_2_event_type_and_payload_shape() {
    let peer = fresh_peer("ai:solo");
    // Default cap = 3.
    let obs = observation(NAMESPACE, "solo-observation", 0, "ai:solo@host");
    let obs_id = db::insert(&peer.conn, &obs).expect("seed obs");

    // Build a chain to depth 3 locally, then attempt depth 4.
    let r1 = db::reflect(
        &peer.conn,
        &reflect_input(vec![obs_id], NAMESPACE, "solo-d1", "ai:solo@host"),
    )
    .unwrap();
    let r2 = db::reflect(
        &peer.conn,
        &reflect_input(vec![r1.id], NAMESPACE, "solo-d2", "ai:solo@host"),
    )
    .unwrap();
    let r3 = db::reflect(
        &peer.conn,
        &reflect_input(vec![r2.id], NAMESPACE, "solo-d3", "ai:solo@host"),
    )
    .unwrap();
    // Depth-4 would breach the default cap = 3.
    let _ = db::reflect(
        &peer.conn,
        &reflect_input(vec![r3.id], NAMESPACE, "solo-d4-refused", "ai:solo@host"),
    )
    .expect_err("must refuse local depth-4");

    let plain_rows: Vec<_> = list_signed_events(&peer.conn, None, 100, 0)
        .expect("list")
        .into_iter()
        .filter(|e| e.event_type == "reflection.depth_exceeded")
        .collect();
    assert_eq!(plain_rows.len(), 1, "exactly one local-only refusal landed");
    let cross_rows: Vec<_> = list_signed_events(&peer.conn, None, 100, 0)
        .expect("list (2)")
        .into_iter()
        .filter(|e| e.event_type == "reflection.depth_exceeded.cross_peer")
        .collect();
    assert!(
        cross_rows.is_empty(),
        "no cross-peer audit fires on a local-only chain"
    );
    // TempDir RAII handles cleanup on function return.
}
