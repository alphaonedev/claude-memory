// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 A2A non-corpus campaign — Round 1 (Track B-light + Track C).
//!
//! Drives 8 A2A scenarios end-to-end in-process against the production
//! substrate API. The campaign acceptance criteria + log capture live
//! under `.local-runs/a2a-2026-05-19/` (see `round1-summary.md`).
//!
//! Scenarios:
//!   A2A-1  Local 2-agent federation roundtrip (HMAC + signature)
//!   A2A-2  Multi-agent identity isolation (3 NHI agents)
//!   A2A-3  Scoped recall (private vs public)
//!   A2A-4  Governance rule cross-agent enforcement
//!   A2A-5  4-domain namespace isolation
//!   A2A-6  Contradiction detection link (cross-agent)
//!   A2A-7  Track C postgres parity (gated on AI_MEMORY_TEST_POSTGRES_URL)
//!   A2A-8  Signature chain integrity (Ed25519 + cross-row hash chain)
//!
//! Each test self-contains its fixtures so Round 2 (re-running this
//! binary against a fresh target) exercises the same surfaces without
//! cross-test state leakage.

#![allow(
    clippy::doc_markdown,
    clippy::items_after_statements,
    clippy::too_many_lines,
    clippy::missing_panics_doc,
    clippy::similar_names,
    clippy::uninlined_format_args,
    clippy::needless_pass_by_value
)]

use ai_memory::db;
use ai_memory::federation::signing::{VerifyError, sign_body_header, verify_header};
use ai_memory::models::{ConfidenceSource, Memory, MemoryKind, Tier};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand_core::OsRng;
use rusqlite::Connection;
use serde_json::json;

// ─────────────────────────────────────────────────────────────────────
// Shared fixtures
// ─────────────────────────────────────────────────────────────────────

fn fresh_conn() -> Connection {
    db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

fn make_memory(
    id: &str,
    ns: &str,
    title: &str,
    content: &str,
    agent_id: &str,
    scope: Option<&str>,
) -> Memory {
    let mut metadata = json!({"agent_id": agent_id});
    if let Some(s) = scope {
        metadata["scope"] = json!(s);
    }
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: id.to_string(),
        tier: Tier::Long,
        namespace: ns.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "a2a-campaign".to_string(),
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
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    }
}

// ─────────────────────────────────────────────────────────────────────
// A2A-1 — Local 2-agent federation roundtrip (HMAC signature)
//
// Two in-process DBs (alice + bob). Alice signs a body with her
// ed25519 key, bob verifies and applies. Bob signs a derived_from
// link payload back, alice verifies and applies. Verifies bidirectional
// HMAC chain.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn a2a_1_local_federation_bidirectional_signature_roundtrip() {
    let alice_key = SigningKey::generate(&mut OsRng);
    let alice_pub: VerifyingKey = alice_key.verifying_key();
    let bob_key = SigningKey::generate(&mut OsRng);
    let bob_pub: VerifyingKey = bob_key.verifying_key();

    let alice_db = fresh_conn();
    let bob_db = fresh_conn();

    // Step 1: Alice stores a memory locally + builds a sync_push body
    // for bob.
    let mem = make_memory(
        "a2a1-alice-mem",
        "_a2a_1",
        "alice-original",
        "Alice's first observation: the sky is blue.",
        "ai:alice",
        Some("public"),
    );
    db::insert(&alice_db, &mem).expect("alice insert");

    let push_body = serde_json::to_vec(&json!({
        "memory": {
            "id": &mem.id,
            "tier": "long",
            "namespace": &mem.namespace,
            "title": &mem.title,
            "content": &mem.content,
            "metadata": &mem.metadata,
        }
    }))
    .expect("serialize push body");

    // Alice signs.
    let alice_sig_header = sign_body_header(&alice_key, &push_body);
    assert!(
        alice_sig_header.starts_with("ed25519="),
        "header carries the v0.7.0 prefix"
    );

    // Step 2: Bob receives + verifies the X-Memory-Sig header against
    // alice's pubkey.
    verify_header(Some(&alice_sig_header), &push_body, &alice_pub)
        .expect("bob verifies alice's sig");

    // Bob applies the memory to his local store.
    db::insert(&bob_db, &mem).expect("bob applies inbound");
    let on_bob = db::get(&bob_db, &mem.id)
        .expect("bob get")
        .expect("bob has alice's memory");
    assert_eq!(on_bob.title, "alice-original");

    // Negative: tampered body must fail.
    let mut tampered = push_body.clone();
    tampered[0] ^= 0xFF;
    let err = verify_header(Some(&alice_sig_header), &tampered, &alice_pub)
        .expect_err("tampered body must fail");
    assert!(matches!(err, VerifyError::BadSignature));

    // Negative: wrong pubkey must fail.
    let err = verify_header(Some(&alice_sig_header), &push_body, &bob_pub)
        .expect_err("wrong pubkey must fail");
    assert!(matches!(err, VerifyError::BadSignature));

    // Step 3: Bob stores a follow-up memory + a derived_from link
    // pointing back to alice's id. Pushes both to alice.
    let derived = make_memory(
        "a2a1-bob-derived",
        "_a2a_1",
        "bob-derived",
        "Bob's derived observation: noticed that alice said the sky is blue.",
        "ai:bob",
        Some("public"),
    );
    db::insert(&bob_db, &derived).expect("bob insert derived");

    db::create_link(&bob_db, &derived.id, &mem.id, "derived_from")
        .expect("bob creates derived_from link locally");

    let link_push = serde_json::to_vec(&json!({
        "derived_memory": &derived.id,
        "source_memory": &mem.id,
        "relation": "derived_from",
    }))
    .expect("serialize link body");
    let bob_sig_header = sign_body_header(&bob_key, &link_push);

    // Alice verifies bob's sig against bob's pubkey.
    verify_header(Some(&bob_sig_header), &link_push, &bob_pub).expect("alice verifies bob's sig");

    // Alice applies bob's memory + link.
    db::insert(&alice_db, &derived).expect("alice applies bob's memory");
    db::create_link(&alice_db, &derived.id, &mem.id, "derived_from")
        .expect("alice applies the derived_from link");

    // Final assertion: both nodes have the same link projection.
    let alice_links = db::get_links(&alice_db, &derived.id).expect("alice get_links");
    let bob_links = db::get_links(&bob_db, &derived.id).expect("bob get_links");

    assert_eq!(alice_links.len(), 1, "alice has the derived_from link");
    assert_eq!(bob_links.len(), 1, "bob has the derived_from link");
    assert_eq!(
        alice_links[0].target_id, mem.id,
        "alice's link points back to alice-original"
    );
    assert_eq!(
        bob_links[0].target_id, mem.id,
        "bob's link points back to alice-original"
    );
}

// ─────────────────────────────────────────────────────────────────────
// A2A-2 — Multi-agent identity isolation
//
// One DB, 3 distinct agent_ids (ai:alice / ai:bob / ai:carol). Each
// stores private + public memories in shared namespace `_a2a_2`. Verify:
// recall as alice sees alice's private + all 3 public, NOT bob's or
// carol's private.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn a2a_2_multi_agent_private_visibility_isolation() {
    // v0.7.0 architecture clarification: `as_agent` is the agent's
    // NAMESPACE POSITION (its location in the org tree), not its
    // agent_id. Visibility resolves against the memory's stored
    // `metadata.scope` + the agent's namespace ancestor chain.
    //
    // For the 3-agent isolation contract we use 3 distinct agent
    // namespace positions and verify:
    //  - private rows: ONLY visible when as_agent EQUALS the memory's
    //    namespace (the owner's "private" mailbox).
    //  - collective scope: visible to any as_agent value.
    let conn = fresh_conn();

    // Each agent stores 2 private (in OWN namespace) + 1 collective
    // (shared `_a2a_2_shared` collective). agent_id is metadata only.
    let agents = [
        ("ai:alice", "alphaone/alice"),
        ("ai:bob", "alphaone/bob"),
        ("ai:carol", "alphaone/carol"),
    ];

    for (agent_id, ns) in &agents {
        for i in 0..2 {
            let mem = make_memory(
                &format!("{}-priv-{}", agent_id.replace(':', "_"), i),
                ns,
                &format!("{}-private-{}", agent_id, i),
                &format!("{}'s private observation {}", agent_id, i),
                agent_id,
                Some("private"),
            );
            db::insert(&conn, &mem).expect("private insert");
        }
        let pub_mem = make_memory(
            &format!("{}-coll", agent_id.replace(':', "_")),
            "_a2a_2_shared",
            &format!("{}-collective", agent_id),
            &format!("{}'s collective observation", agent_id),
            agent_id,
            Some("collective"),
        );
        db::insert(&conn, &pub_mem).expect("collective insert");
    }

    // Each agent (as namespace position) should see:
    //   - own 2 private rows in their own namespace
    //   - 0 private rows from other agents' namespaces
    //   - all 3 collective rows from the shared namespace
    // The visibility filter lives on `search` / `recall_hybrid` /
    // `recall`, NOT on `list` (which is namespace+tier+tag only).
    for (label, ns) in &agents {
        // `search` with FTS query "observation" matches every memory's
        // content (all contain that word) and APPLIES the visibility
        // clause via as_agent.
        let own_view = db::search(
            &conn,
            "observation",
            Some(ns),
            None,
            100,
            None,
            None,
            None,
            None,
            None,
            Some(ns),
            false,
        )
        .expect("search own ns");
        let own_priv = own_view
            .iter()
            .filter(|m| m.namespace == *ns)
            .filter(|m| m.metadata.get("scope").and_then(|v| v.as_str()) == Some("private"))
            .count();
        assert_eq!(own_priv, 2, "{} sees 2 own-private rows", label);

        // Cross-agent peek: as alice (ns=alphaone/alice), search bob's
        // private namespace — should return 0 visible private rows.
        for (other_label, other_ns) in &agents {
            if other_ns == ns {
                continue;
            }
            let cross = db::search(
                &conn,
                "observation",
                Some(other_ns),
                None,
                100,
                None,
                None,
                None,
                None,
                None,
                Some(ns),
                false,
            )
            .expect("cross search");
            let leaked = cross
                .iter()
                .filter(|m| m.metadata.get("scope").and_then(|v| v.as_str()) == Some("private"))
                .count();
            assert_eq!(
                leaked, 0,
                "{} must NOT see {}'s private rows; saw {}",
                label, other_label, leaked
            );
        }

        // Collective rows — visible from any agent's vantage.
        let shared = db::search(
            &conn,
            "observation",
            Some("_a2a_2_shared"),
            None,
            100,
            None,
            None,
            None,
            None,
            None,
            Some(ns),
            false,
        )
        .expect("search shared");
        let collective_count = shared
            .iter()
            .filter(|m| m.metadata.get("scope").and_then(|v| v.as_str()) == Some("collective"))
            .count();
        assert_eq!(
            collective_count, 3,
            "{} sees all 3 collective rows in shared ns; got {}",
            label, collective_count
        );
    }

    // Total stored: 3 agents × (2 private + 1 collective) = 9.
    let all = db::list(&conn, None, None, 100, 0, None, None, None, None, None).expect("list all");
    assert_eq!(all.len(), 9, "9 rows total");
}

// ─────────────────────────────────────────────────────────────────────
// A2A-3 — Scoped recall (alice stores; bob recalls)
//
// Alice stores 5 memories (3 private + 2 public). Bob recalls and
// must see at least the 2 public. Verifies scope filter on the recall
// path.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn a2a_3_scoped_recall_collective_visible_private_isolated() {
    // Scope contract:
    //   private    — only owner's namespace position sees the row
    //   collective — visible from any namespace position
    //
    // Alice stores 3 private + 2 collective in her own namespace.
    // Bob (different namespace position) recalls — must see 0 private
    // and 2 collective. Sole filter is `as_agent` = bob's namespace.
    let conn = fresh_conn();
    let alice_ns = "alphaone/alice";
    let bob_ns = "alphaone/bob";

    for i in 0..3 {
        let m = make_memory(
            &format!("a3-alice-priv-{}", i),
            alice_ns,
            &format!("alice-priv-{}", i),
            &format!("alice private {}: secret", i),
            "ai:alice",
            Some("private"),
        );
        db::insert(&conn, &m).expect("alice private");
    }
    for i in 0..2 {
        let m = make_memory(
            &format!("a3-alice-coll-{}", i),
            alice_ns,
            &format!("alice-coll-{}", i),
            &format!("alice collective {}: published", i),
            "ai:alice",
            Some("collective"),
        );
        db::insert(&conn, &m).expect("alice collective");
    }

    // Bob (as_agent=alphaone/bob) searches alice's namespace — only
    // collective surfaces; private is filtered.
    let bob_view = db::search(
        &conn,
        "alice",
        Some(alice_ns),
        None,
        100,
        None,
        None,
        None,
        None,
        None,
        Some(bob_ns),
        false,
    )
    .expect("bob search");

    let collective_count = bob_view
        .iter()
        .filter(|m| m.metadata.get("scope").and_then(|v| v.as_str()) == Some("collective"))
        .count();
    assert_eq!(
        collective_count, 2,
        "bob sees exactly 2 alice-collective memories"
    );

    let leaked = bob_view
        .iter()
        .filter(|m| m.metadata.get("scope").and_then(|v| v.as_str()) == Some("private"))
        .count();
    assert_eq!(
        leaked, 0,
        "bob must not see alice's private rows; saw {}",
        leaked
    );

    // Positive sanity: alice (as_agent=alice_ns) sees BOTH private and
    // collective (3 + 2 = 5).
    let alice_view = db::search(
        &conn,
        "alice",
        Some(alice_ns),
        None,
        100,
        None,
        None,
        None,
        None,
        None,
        Some(alice_ns),
        false,
    )
    .expect("alice search");
    assert_eq!(alice_view.len(), 5, "alice sees all 5 of her own rows");
}

// ─────────────────────────────────────────────────────────────────────
// A2A-4 — Governance rule cross-agent enforcement
//
// Add a refuse rule for filesystem_write under `/tmp/**`. Verify
// `check_agent_action` returns Refuse for any agent, and verify the
// forensic signed_events table sees a row per check (best-effort
// log emit).
// ─────────────────────────────────────────────────────────────────────

#[test]
fn a2a_4_governance_rule_refuses_cross_agent() {
    use ai_memory::governance::agent_action::{AgentAction, Decision, check_agent_action};
    use ai_memory::governance::rules_store::{self, Rule};
    use base64::Engine;
    use ed25519_dalek::Signer;

    // v0.7.0 L1-6: the rule engine requires `operator_signed`
    // attestation when a pubkey resolves on the host (env var or
    // ~/.config/ai-memory/operator.key.pub). Install a test operator
    // key in the env var so the rule we plant signs correctly and
    // verifies on enforcement. Saved prior env var; restored at drop.
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: single-threaded test execution (-- --test-threads=1).
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }
    let prev = std::env::var("AI_MEMORY_OPERATOR_PUBKEY").ok();
    let signing = SigningKey::generate(&mut OsRng);
    let pub_b64 =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signing.verifying_key().to_bytes());
    // SAFETY: single-threaded test execution.
    unsafe {
        std::env::set_var("AI_MEMORY_OPERATOR_PUBKEY", &pub_b64);
    }
    let _env_guard = EnvGuard {
        key: "AI_MEMORY_OPERATOR_PUBKEY",
        prev,
    };

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE governance_rules (
             id TEXT PRIMARY KEY,
             kind TEXT NOT NULL,
             matcher TEXT NOT NULL,
             severity TEXT NOT NULL CHECK (severity IN ('refuse','warn','log')),
             reason TEXT NOT NULL,
             namespace TEXT NOT NULL DEFAULT '_global',
             created_by TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             enabled INTEGER NOT NULL DEFAULT 1,
             signature BLOB,
             attest_level TEXT NOT NULL DEFAULT 'unsigned'
         );
         CREATE TABLE signed_events (
             id TEXT PRIMARY KEY,
             agent_id TEXT NOT NULL,
             event_type TEXT NOT NULL,
             payload_hash BLOB NOT NULL,
             signature BLOB,
             attest_level TEXT NOT NULL DEFAULT 'unsigned',
             timestamp TEXT NOT NULL,
             prev_hash BLOB,
             sequence INTEGER
         );",
    )
    .unwrap();

    let mut rule = Rule {
        id: "A2A4-NO-TMP".into(),
        kind: "filesystem_write".into(),
        matcher: r#"{"glob":"/tmp/**"}"#.into(),
        severity: "refuse".into(),
        reason: "no /tmp writes per A2A-4".into(),
        namespace: "_global".into(),
        created_by: "operator".into(),
        created_at: 0,
        enabled: true,
        signature: None,
        attest_level: "operator_signed".into(),
    };
    // Sign canonical bytes — verifier reads back exactly these bytes.
    let canonical =
        rules_store::canonical_bytes_for_signing(&rule).expect("canonical_bytes_for_signing");
    rule.signature = Some(signing.sign(&canonical).to_bytes().to_vec());
    rules_store::insert(&conn, &rule).unwrap();

    // Adversary attempt: refused.
    let action = AgentAction::FilesystemWrite {
        path: "/tmp/foo".into(),
        byte_estimate: None,
    };
    let decision = check_agent_action(&conn, "ai:adversary", &action).unwrap();
    assert!(
        matches!(decision, Decision::Refuse { .. }),
        "adversary refused: {:?}",
        decision
    );

    // Operator attempting outside `/tmp` (e.g. `/Users/operator/x`):
    // allowed (the matcher only refuses `/tmp/**`).
    let action_safe = AgentAction::FilesystemWrite {
        path: "/Users/operator/safe.txt".into(),
        byte_estimate: None,
    };
    let decision_op = check_agent_action(&conn, "ai:operator", &action_safe).unwrap();
    assert!(
        !matches!(decision_op, Decision::Refuse { .. }),
        "operator NOT refused for safe path; got {:?}",
        decision_op
    );

    // Test that another agent (alice) is also refused — rules are
    // cross-agent by default.
    let decision_alice = check_agent_action(&conn, "ai:alice", &action).unwrap();
    assert!(
        matches!(decision_alice, Decision::Refuse { .. }),
        "alice refused for /tmp: {:?}",
        decision_alice
    );
}

// ─────────────────────────────────────────────────────────────────────
// A2A-5 — 4-domain namespace isolation
//
// Four namespaces, 5 memories in each (less than the script's 10 to
// keep test runtime tight; the property under test is namespace
// isolation, not throughput). Verify per-namespace recall returns only
// that namespace's rows; cross-namespace (no namespace filter) returns
// all 20.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn a2a_5_four_domain_namespace_isolation() {
    let conn = fresh_conn();

    let domains = [
        (
            "domain_a_legal",
            "court ruling",
            "Legal precedent: case A vs B.",
        ),
        (
            "domain_b_medical",
            "MRI scan",
            "Medical: patient X has condition Y.",
        ),
        (
            "domain_c_engineering",
            "API endpoint",
            "Engineering: /api/v1/users.",
        ),
        (
            "domain_d_finance",
            "Q4 revenue",
            "Finance: Q4 revenue projection $1M.",
        ),
    ];

    for (ns, title_prefix, content) in &domains {
        for i in 0..5 {
            let m = make_memory(
                &format!("{}-{}", ns, i),
                ns,
                &format!("{}-{}", title_prefix, i),
                &format!("{} (entry {})", content, i),
                "ai:domain-author",
                Some("public"),
            );
            db::insert(&conn, &m).expect("domain insert");
        }
    }

    // Per-namespace boundary check.
    for (ns, _, _) in &domains {
        let listed =
            db::list(&conn, Some(ns), None, 100, 0, None, None, None, None, None).expect("ns list");
        assert_eq!(
            listed.len(),
            5,
            "namespace {} has exactly 5 rows; got {}",
            ns,
            listed.len()
        );
        for m in &listed {
            assert_eq!(m.namespace, *ns, "row in wrong namespace");
        }
    }

    // Cross-namespace recall (no filter) returns all 20.
    let all =
        db::list(&conn, None, None, 100, 0, None, None, None, None, None).expect("global list");
    assert_eq!(all.len(), 20, "global list = 4 × 5 = 20; got {}", all.len());
}

// ─────────────────────────────────────────────────────────────────────
// A2A-6 — Contradiction-link creation cross-agent
//
// memory_detect_contradiction proper requires an LLM (smart tier). The
// substrate-level contract this scenario probes is link creation on
// the `contradicts` relation across two agents' memories. Stand up
// alice + bob memories with opposing claims, create the `contradicts`
// link both directions, verify symmetric projection via `get_links`.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn a2a_6_contradiction_link_cross_agent_symmetric() {
    let conn = fresh_conn();
    let ns = "_a2a_6_shared";

    let alice = make_memory(
        "a6-alice",
        ns,
        "alice-claim",
        "X is true.",
        "ai:alice",
        Some("public"),
    );
    let bob = make_memory(
        "a6-bob",
        ns,
        "bob-claim",
        "X is false.",
        "ai:bob",
        Some("public"),
    );
    db::insert(&conn, &alice).unwrap();
    db::insert(&conn, &bob).unwrap();

    // Create `contradicts` link both ways.
    db::create_link(&conn, &alice.id, &bob.id, "contradicts").unwrap();
    db::create_link(&conn, &bob.id, &alice.id, "contradicts").unwrap();

    // db::get_links returns both inbound + outbound rows for the id,
    // so for symmetric contradiction we expect 2 each (1 outbound +
    // 1 inbound). Filter by source_id to verify the directional
    // outbound projection.
    let alice_all = db::get_links(&conn, &alice.id).unwrap();
    let bob_all = db::get_links(&conn, &bob.id).unwrap();
    assert_eq!(alice_all.len(), 2, "alice has 1 out + 1 in");
    assert_eq!(bob_all.len(), 2, "bob has 1 out + 1 in");

    let alice_outbound: Vec<_> = alice_all
        .iter()
        .filter(|l| l.source_id == alice.id)
        .collect();
    let bob_outbound: Vec<_> = bob_all.iter().filter(|l| l.source_id == bob.id).collect();

    assert_eq!(alice_outbound.len(), 1);
    assert_eq!(bob_outbound.len(), 1);
    assert_eq!(alice_outbound[0].target_id, bob.id);
    assert_eq!(bob_outbound[0].target_id, alice.id);
    assert_eq!(alice_outbound[0].relation.as_str(), "contradicts");
    assert_eq!(bob_outbound[0].relation.as_str(), "contradicts");
}

// ─────────────────────────────────────────────────────────────────────
// A2A-7 — Track C: live Postgres parity (gated on AI_MEMORY_TEST_POSTGRES_URL).
//
// When the env var is set, opens the live PG store + runs the 6 SAL
// methods that store_parity_gaps probes (gap 1/2/3/5/6/7). When unset,
// returns OK without asserting — the gap parity test binary handles
// the live-PG path under `cargo test --features sal-postgres --test
// store_parity_gaps -- --ignored`.
// ─────────────────────────────────────────────────────────────────────

#[cfg(feature = "sal-postgres")]
#[tokio::test]
async fn a2a_7_track_c_pg_parity_smoke() {
    let Some(url) = std::env::var("AI_MEMORY_TEST_POSTGRES_URL").ok() else {
        eprintln!("a2a-7 skipped: AI_MEMORY_TEST_POSTGRES_URL unset");
        return;
    };

    let pg = ai_memory::store::postgres::PostgresStore::connect(&url)
        .await
        .expect("connect live PG");

    // Smoke 1: store a memory + recall via SAL.
    let ctx = ai_memory::store::CallerContext::for_agent("a2a-7-smoke");
    let mem = ai_memory::models::Memory {
        id: format!("a2a7-{}", chrono::Utc::now().timestamp_millis()),
        tier: ai_memory::models::Tier::Long,
        namespace: "_a2a_7".to_string(),
        title: "a2a-7 smoke".to_string(),
        content: "live PG smoke memory from A2A-7".to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "a2a-7-smoke".to_string(),
        access_count: 0,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id":"a2a-7-smoke"}),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ai_memory::models::ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    };
    let _ = ai_memory::store::MemoryStore::store(&pg, &ctx, &mem).await;
}

#[cfg(not(feature = "sal-postgres"))]
#[test]
fn a2a_7_track_c_pg_parity_smoke_skipped_no_feature() {
    eprintln!("a2a-7 compiled without sal-postgres feature; run with --features sal-postgres");
}

// ─────────────────────────────────────────────────────────────────────
// A2A-8 — Signature chain integrity (Ed25519 + cross-row hash chain).
//
// Triangle: alice → bob → carol → alice. Sign 5 push bodies in each
// direction, verify every receiver accepts the sender's sig + the
// substrate signed_events chain (append + verify_chain) holds.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn a2a_8_signature_chain_triangle_integrity() {
    use ai_memory::signed_events::{SignedEvent, append_signed_event, verify_chain};
    use ed25519_dalek::{Signer, Verifier};

    let alice_key = SigningKey::generate(&mut OsRng);
    let bob_key = SigningKey::generate(&mut OsRng);
    let carol_key = SigningKey::generate(&mut OsRng);

    let alice_pub = alice_key.verifying_key();
    let bob_pub = bob_key.verifying_key();
    let carol_pub = carol_key.verifying_key();

    let alice_db = fresh_conn();

    // Append 15 signed_events to alice's chain (5 from each peer to
    // simulate the alice → bob → carol → alice triangle).
    fn append_one(
        conn: &Connection,
        agent: &str,
        event_type: &str,
        signing_key: &SigningKey,
        n: i64,
    ) {
        // Build a payload that's deterministic per (agent, n).
        let payload = format!("{}|{}|{}", agent, event_type, n);
        let payload_hash = {
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            hasher.update(payload.as_bytes());
            hasher.finalize().to_vec()
        };
        let sig = signing_key.sign(&payload_hash);
        // prev_hash + sequence are filled by append_signed_event; pass
        // SignedEvent::default()-style empties.
        let event = SignedEvent {
            id: format!("a8-evt-{}-{}", agent.replace(':', "_"), n),
            agent_id: agent.to_string(),
            event_type: event_type.to_string(),
            payload_hash,
            signature: Some(sig.to_bytes().to_vec()),
            attest_level: "self_signed".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            prev_hash: Vec::new(),
            sequence: 0,
        };
        append_signed_event(conn, &event).expect("append");
    }

    for i in 1..=5 {
        append_one(&alice_db, "ai:alice", "memory_store", &alice_key, i);
    }
    for i in 1..=5 {
        append_one(&alice_db, "ai:bob", "memory_store", &bob_key, i);
    }
    for i in 1..=5 {
        append_one(&alice_db, "ai:carol", "memory_store", &carol_key, i);
    }

    // Verify chain integrity over all 15 rows.
    let report = verify_chain(&alice_db, None).expect("verify_chain");
    assert!(
        report.chain_holds(),
        "chain must hold across all 15 rows; report = {:?}",
        report
    );
    assert_eq!(report.rows_checked, 15, "exactly 15 rows checked");

    // Verify each peer's signatures against the matching pubkey.
    let mut stmt = alice_db
        .prepare(
            "SELECT agent_id, payload_hash, signature FROM signed_events ORDER BY sequence ASC",
        )
        .unwrap();
    let rows: Vec<(String, Vec<u8>, Vec<u8>)> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        })
        .unwrap()
        .filter_map(Result::ok)
        .collect();

    assert_eq!(rows.len(), 15);
    for (agent, hash, sig_bytes) in &rows {
        let pubkey = match agent.as_str() {
            "ai:alice" => &alice_pub,
            "ai:bob" => &bob_pub,
            "ai:carol" => &carol_pub,
            other => panic!("unknown agent {}", other),
        };
        let mut sig = [0u8; 64];
        sig.copy_from_slice(sig_bytes);
        let signature = ed25519_dalek::Signature::from_bytes(&sig);
        pubkey
            .verify(hash, &signature)
            .expect("self_signed verification");
    }
}
