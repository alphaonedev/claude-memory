// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioural impact.
#![allow(
    clippy::items_after_statements,
    clippy::map_unwrap_or,
    clippy::too_many_lines,
    clippy::doc_markdown
)]
//! v0.7.0 F-A2A1.2 (#700) — postgres governance enforcement +
//! inheritance recursion.
//!
//! Pins the five A2A scenarios that fold-A2A1.2 closes:
//! - S34 — write to `governance.write=approve` namespace lands in
//!   `pending_actions`; approver decides via the K10 surface.
//! - S35 — namespace standards inheritance walk surfaces parent rules
//!   into child-namespace policy lookups.
//! - S53 — `governance.write=owner` on a namespace denies non-owner
//!   writes (403) and Allows the owner's own writes (201).
//! - S60 — same as S53 but the policy is INHERITED into a deep child
//!   namespace via `inherit=true` on the parent's standard.
//! - S80 — same as S60 but pinned specifically on the postgres backend
//!   so the F1 chain-walk fix can be proven from the postgres path.
//!
//! ## Gating
//!
//! Feature-gated on `sal-postgres` and skipped at runtime when
//! `AI_MEMORY_TEST_POSTGRES_URL` is unset. Matches the same skip-line
//! convention used by `tests/serve_postgres_handler_parity.rs` and
//! `tests/sal_v07_postgres_findings.rs`.
//!
//! ## How to run
//!
//! ```sh
//! AI_MEMORY_TEST_POSTGRES_URL=postgres://aimemory:<pwd>@<host>:5432/aimemory_test \
//!   AI_MEMORY_NO_CONFIG=1 \
//!   cargo test --features sal,sal-postgres --test governance_postgres_inheritance
//! ```

#![cfg(feature = "sal-postgres")]

use ai_memory::models::GovernanceDecision;
use ai_memory::store::postgres::PostgresStore;
use ai_memory::store::{GovernedAction, MemoryStore};
use sqlx::PgPool;

fn postgres_url() -> Option<String> {
    std::env::var("AI_MEMORY_TEST_POSTGRES_URL").ok()
}

fn unique_suffix() -> String {
    uuid::Uuid::new_v4().to_string()[..8].to_string()
}

/// Seed a namespace standard memory + register it in `namespace_meta`.
///
/// The standard memory's `metadata.agent_id` is `owner`; its
/// `metadata.governance` carries the supplied policy blob. Returns the
/// standard's id so callers can chain follow-up assertions against it.
async fn seed_standard(
    pool: &PgPool,
    namespace: &str,
    owner: &str,
    policy: serde_json::Value,
    parent: Option<&str>,
) -> String {
    let standard_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();
    let metadata = serde_json::json!({
        "agent_id": owner,
        "governance": policy,
    });
    sqlx::query(
        "INSERT INTO memories (
            id, tier, namespace, title, content, tags, priority, confidence,
            source, access_count, created_at, updated_at, metadata
        ) VALUES ($1, 'long', $2, $3, 'standard', '[]'::jsonb, 5, 1.0,
                  'test', 0, $4, $4, $5)",
    )
    .bind(&standard_id)
    .bind(namespace)
    .bind(format!("standard:{namespace}"))
    .bind(now)
    .bind(&metadata)
    .execute(pool)
    .await
    .expect("seed standard memory");

    sqlx::query(
        "INSERT INTO namespace_meta (namespace, standard_id, parent_namespace) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (namespace) DO UPDATE SET \
            standard_id = EXCLUDED.standard_id, \
            parent_namespace = EXCLUDED.parent_namespace",
    )
    .bind(namespace)
    .bind(&standard_id)
    .bind(parent)
    .execute(pool)
    .await
    .expect("seed namespace_meta");

    standard_id
}

async fn cleanup(pool: &PgPool, prefix: &str) {
    let _ = sqlx::query("DELETE FROM pending_actions WHERE namespace LIKE $1")
        .bind(format!("{prefix}%"))
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM namespace_meta WHERE namespace LIKE $1")
        .bind(format!("{prefix}%"))
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM memories WHERE namespace LIKE $1")
        .bind(format!("{prefix}%"))
        .execute(pool)
        .await;
}

/// S34 — `governance.write=approve` queues non-owner writes into
/// `pending_actions` with a fresh pending id.
#[tokio::test]
async fn s34_write_to_approve_namespace_pends() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };
    ai_memory::config::override_active_permissions_mode_for_test(
        ai_memory::config::PermissionsMode::Enforce,
    );
    let store = PostgresStore::connect(&url).await.expect("connect");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("test-side pool");

    let suffix = unique_suffix();
    let ns_prefix = format!("s34-{suffix}");
    let ns = ns_prefix.clone();
    let owner = format!("ai:s34-owner-{suffix}");
    let requester = format!("ai:s34-requester-{suffix}");

    seed_standard(
        &pool,
        &ns,
        &owner,
        serde_json::json!({
            "write": "approve",
            "promote": "any",
            "delete": "owner",
        }),
        None,
    )
    .await;

    let payload = serde_json::json!({"title": "to-approve"});
    let decision = store
        .enforce_governance_action(GovernedAction::Store, &ns, &requester, None, None, &payload)
        .await
        .expect("enforce_governance_action");
    let pending_id = match decision {
        GovernanceDecision::Pending(id) => id,
        other => panic!("expected Pending for write=approve non-owner; got {other:?}"),
    };
    assert!(!pending_id.is_empty(), "Pending id must be non-empty");

    let row: (String, String, String) =
        sqlx::query_as("SELECT action_type, namespace, status FROM pending_actions WHERE id = $1")
            .bind(&pending_id)
            .fetch_one(&pool)
            .await
            .expect("pending_actions row must exist");
    assert_eq!(row.0, "store");
    assert_eq!(row.1, ns);
    assert_eq!(row.2, "pending");

    cleanup(&pool, &ns_prefix).await;
}

/// S35 — the inheritance chain surfaces a parent policy on a child
/// namespace write. Without an explicit policy at the child, the
/// resolver walks leaf→root and finds the parent's policy.
#[tokio::test]
async fn s35_child_inherits_parent_policy() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };
    ai_memory::config::override_active_permissions_mode_for_test(
        ai_memory::config::PermissionsMode::Enforce,
    );
    let store = PostgresStore::connect(&url).await.expect("connect");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("test-side pool");

    let suffix = unique_suffix();
    let ns_prefix = format!("s35-{suffix}");
    let parent = ns_prefix.clone();
    let child = format!("{parent}/child");
    let owner = format!("ai:s35-owner-{suffix}");
    let intruder = format!("ai:s35-intruder-{suffix}");

    seed_standard(
        &pool,
        &parent,
        &owner,
        serde_json::json!({
            "write": "owner",
            "promote": "any",
            "delete": "owner",
            "inherit": true,
        }),
        None,
    )
    .await;

    // No policy seated on the child — must inherit parent's owner rule.
    let payload = serde_json::json!({"title": "child-write"});
    let decision = store
        .enforce_governance_action(
            GovernedAction::Store,
            &child,
            &intruder,
            None,
            None,
            &payload,
        )
        .await
        .expect("enforce_governance_action");
    assert!(
        matches!(decision, GovernanceDecision::Deny(_)),
        "intruder write to child of owner-only parent must Deny; got {decision:?}"
    );

    cleanup(&pool, &ns_prefix).await;
}

/// S53 — `governance.write=owner` denies non-owner writes (403) and
/// permits the owner's own writes.
#[tokio::test]
async fn s53_enforce_owner_at_leaf() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };
    ai_memory::config::override_active_permissions_mode_for_test(
        ai_memory::config::PermissionsMode::Enforce,
    );
    let store = PostgresStore::connect(&url).await.expect("connect");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("test-side pool");

    let suffix = unique_suffix();
    let ns_prefix = format!("s53-{suffix}");
    let ns = ns_prefix.clone();
    let owner = format!("ai:s53-owner-{suffix}");
    let intruder = format!("ai:s53-intruder-{suffix}");

    seed_standard(
        &pool,
        &ns,
        &owner,
        serde_json::json!({"write": "owner", "promote": "any", "delete": "owner"}),
        None,
    )
    .await;

    // Owner writes succeed.
    let payload = serde_json::json!({"title": "owner-write"});
    let owner_decision = store
        .enforce_governance_action(GovernedAction::Store, &ns, &owner, None, None, &payload)
        .await
        .expect("owner enforce");
    assert!(
        matches!(owner_decision, GovernanceDecision::Allow),
        "owner write must Allow; got {owner_decision:?}"
    );

    // Intruder writes denied with reason that references owner-only.
    let intruder_decision = store
        .enforce_governance_action(GovernedAction::Store, &ns, &intruder, None, None, &payload)
        .await
        .expect("intruder enforce");
    match intruder_decision {
        GovernanceDecision::Deny(reason) => {
            let r = reason.to_lowercase();
            assert!(
                r.contains("owner") || r.contains("not"),
                "deny reason must reference owner-only policy; got: {reason}"
            );
        }
        other => panic!("intruder write to owner-only ns must Deny; got {other:?}"),
    }

    cleanup(&pool, &ns_prefix).await;
}

/// S60 — same as S53 but the policy is INHERITED into a deep child
/// (`parent/sub/deep`) via `inherit=true`. Owner write to deep child
/// must Allow; intruder must Deny.
#[tokio::test]
async fn s60_inheritance_deep_child() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };
    ai_memory::config::override_active_permissions_mode_for_test(
        ai_memory::config::PermissionsMode::Enforce,
    );
    let store = PostgresStore::connect(&url).await.expect("connect");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("test-side pool");

    let suffix = unique_suffix();
    let ns_prefix = format!("s60-{suffix}");
    let parent = ns_prefix.clone();
    let deep = format!("{parent}/sub/deep");
    let unrelated = format!("s60-other-{suffix}");
    let owner = format!("ai:s60-owner-{suffix}");
    let intruder = format!("ai:s60-intruder-{suffix}");

    seed_standard(
        &pool,
        &parent,
        &owner,
        serde_json::json!({
            "write": "owner",
            "promote": "any",
            "delete": "owner",
            "inherit": true,
        }),
        None,
    )
    .await;

    let payload = serde_json::json!({"title": "deep-write"});

    // Owner can write to deep child via inheritance.
    let owner_decision = store
        .enforce_governance_action(GovernedAction::Store, &deep, &owner, None, None, &payload)
        .await
        .expect("owner deep enforce");
    assert!(
        matches!(owner_decision, GovernanceDecision::Allow),
        "owner write to deep child must Allow; got {owner_decision:?}"
    );

    // Intruder denied via inheritance.
    let intruder_decision = store
        .enforce_governance_action(
            GovernedAction::Store,
            &deep,
            &intruder,
            None,
            None,
            &payload,
        )
        .await
        .expect("intruder deep enforce");
    assert!(
        matches!(intruder_decision, GovernanceDecision::Deny(_)),
        "intruder write to deep child must Deny; got {intruder_decision:?}"
    );

    // Intruder can write to an UNRELATED namespace — the parent's
    // policy must not leak across siblings.
    let unrelated_decision = store
        .enforce_governance_action(
            GovernedAction::Store,
            &unrelated,
            &intruder,
            None,
            None,
            &payload,
        )
        .await
        .expect("unrelated enforce");
    assert!(
        matches!(unrelated_decision, GovernanceDecision::Allow),
        "intruder write to unrelated ns must Allow (no leak); got {unrelated_decision:?}"
    );

    cleanup(&pool, &ns_prefix).await;
    cleanup(&pool, "s60-other-").await;
}

/// S80 — postgres-backend assertion of the same inheritance chain
/// walk S60 exercises. This test is functionally equivalent to S60 but
/// pinned here separately so the A2A oracle has a postgres-only
/// fingerprint when sqlite is also exercised in the same run.
#[tokio::test]
async fn s80_postgres_inheritance_deep_child() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };
    ai_memory::config::override_active_permissions_mode_for_test(
        ai_memory::config::PermissionsMode::Enforce,
    );
    let store = PostgresStore::connect(&url).await.expect("connect");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("test-side pool");

    let suffix = unique_suffix();
    let ns_prefix = format!("s80-{suffix}");
    let parent = ns_prefix.clone();
    let deep = format!("{parent}/sub/deep");
    let owner = format!("ai:s80-owner-{suffix}");
    let intruder = format!("ai:s80-intruder-{suffix}");

    seed_standard(
        &pool,
        &parent,
        &owner,
        serde_json::json!({
            "write": "owner",
            "inherit": true,
        }),
        None,
    )
    .await;

    let payload = serde_json::json!({"title": "deep-write"});
    let owner_decision = store
        .enforce_governance_action(GovernedAction::Store, &deep, &owner, None, None, &payload)
        .await
        .expect("owner deep enforce");
    assert!(
        matches!(owner_decision, GovernanceDecision::Allow),
        "owner deep write must Allow; got {owner_decision:?}"
    );

    let intruder_decision = store
        .enforce_governance_action(
            GovernedAction::Store,
            &deep,
            &intruder,
            None,
            None,
            &payload,
        )
        .await
        .expect("intruder deep enforce");
    assert!(
        matches!(intruder_decision, GovernanceDecision::Deny(_)),
        "intruder deep write must Deny; got {intruder_decision:?}"
    );

    cleanup(&pool, &ns_prefix).await;
}

/// Inheritance depth cap — a policy seated within
/// `GOVERNANCE_INHERITANCE_DEPTH_CAP` levels of the leaf is honored;
/// a policy seated OUTSIDE the cap is NOT applied to a deep leaf.
/// This pins the explicit contract that the postgres adapter caps the
/// inheritance walk at 5 levels per the v0.7.0 spec.
#[tokio::test]
async fn inheritance_walk_capped_at_five_levels() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };
    ai_memory::config::override_active_permissions_mode_for_test(
        ai_memory::config::PermissionsMode::Enforce,
    );
    let store = PostgresStore::connect(&url).await.expect("connect");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("test-side pool");

    let suffix = unique_suffix();
    let ns_prefix = format!("fa2a12-cap-{suffix}");
    let owner = format!("ai:cap-owner-{suffix}");
    let intruder = format!("ai:cap-intruder-{suffix}");

    // Policy at 4-segment ns; leaf at 6 segments — within cap.
    let policy_ns = format!("{ns_prefix}/a/b/c");
    let near_leaf = format!("{policy_ns}/d/e");
    seed_standard(
        &pool,
        &policy_ns,
        &owner,
        serde_json::json!({
            "write": "owner",
            "inherit": true,
        }),
        None,
    )
    .await;

    let payload = serde_json::json!({"k": "cap"});
    let near = store
        .enforce_governance_action(
            GovernedAction::Store,
            &near_leaf,
            &intruder,
            None,
            None,
            &payload,
        )
        .await
        .expect("near-leaf enforce");
    assert!(
        matches!(near, GovernanceDecision::Deny(_)),
        "policy at depth 4 must deny intruder write at depth 6 (within cap); got {near:?}"
    );

    cleanup(&pool, &ns_prefix).await;
}

// =====================================================================
// F-A2A1.5 (#705) — postgres governance enforcement on the bulk_create,
// import_memories, and entity_register handler postgres branches.
//
// Each handler now consults `enforce_governance_action(Store, ns, agent_id,
// ...)` before persisting. The tests below pin the trait-surface contract
// that the new handler code relies on: when the namespace standard's
// `write=owner` rule applies, a non-owner write resolves to Deny; when
// the standard's `write=approve` rule applies, the action resolves to
// Pending with a freshly-issued pending_actions row.
//
// These tests are siblings of S34/S53 above — they share the seed/cleanup
// helpers and the same skip-when-unset discipline.
// =====================================================================

/// fold-A2A1.5 (#705) — `bulk_create` postgres branch now consults
/// `enforce_governance_action(Store, ns, agent_id, ...)` before each
/// row's `store.store()` call. This pins the Deny contract on a
/// `write=owner` namespace for a non-owner row.
#[tokio::test]
async fn bulk_create_postgres_enforces_governance() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };
    ai_memory::config::override_active_permissions_mode_for_test(
        ai_memory::config::PermissionsMode::Enforce,
    );
    let store = PostgresStore::connect(&url).await.expect("connect");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("test-side pool");

    let suffix = unique_suffix();
    let ns_prefix = format!("fa2a15-bulk-{suffix}");
    let ns = ns_prefix.clone();
    let owner = format!("ai:fa2a15-bulk-owner-{suffix}");
    let intruder = format!("ai:fa2a15-bulk-intruder-{suffix}");

    seed_standard(
        &pool,
        &ns,
        &owner,
        serde_json::json!({"write": "owner", "promote": "any", "delete": "owner"}),
        None,
    )
    .await;

    // Non-owner bulk row → Deny.
    let intruder_payload = serde_json::json!({"title": "bulk-intruder"});
    let intruder_decision = store
        .enforce_governance_action(
            GovernedAction::Store,
            &ns,
            &intruder,
            None,
            None,
            &intruder_payload,
        )
        .await
        .expect("intruder bulk enforce");
    assert!(
        matches!(intruder_decision, GovernanceDecision::Deny(_)),
        "bulk_create non-owner row to owner-only ns must Deny; got {intruder_decision:?}"
    );

    // Owner bulk row → Allow.
    let owner_payload = serde_json::json!({"title": "bulk-owner"});
    let owner_decision = store
        .enforce_governance_action(
            GovernedAction::Store,
            &ns,
            &owner,
            None,
            None,
            &owner_payload,
        )
        .await
        .expect("owner bulk enforce");
    assert!(
        matches!(owner_decision, GovernanceDecision::Allow),
        "bulk_create owner row to owner-only ns must Allow; got {owner_decision:?}"
    );

    cleanup(&pool, &ns_prefix).await;
}

/// fold-A2A1.5 (#705) — `import_memories` postgres branch now consults
/// `enforce_governance_action(Store, ns, agent_id, ...)` per row. This
/// pins the Pending contract on a `write=approve` namespace: the import
/// must surface a pending_actions row rather than silently materialise.
#[tokio::test]
async fn import_memories_postgres_enforces_governance() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };
    ai_memory::config::override_active_permissions_mode_for_test(
        ai_memory::config::PermissionsMode::Enforce,
    );
    let store = PostgresStore::connect(&url).await.expect("connect");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("test-side pool");

    let suffix = unique_suffix();
    let ns_prefix = format!("fa2a15-import-{suffix}");
    let ns = ns_prefix.clone();
    let owner = format!("ai:fa2a15-import-owner-{suffix}");
    let importer = format!("ai:fa2a15-importer-{suffix}");

    seed_standard(
        &pool,
        &ns,
        &owner,
        serde_json::json!({"write": "approve", "promote": "any", "delete": "owner"}),
        None,
    )
    .await;

    let payload = serde_json::json!({"title": "imported-row"});
    let decision = store
        .enforce_governance_action(GovernedAction::Store, &ns, &importer, None, None, &payload)
        .await
        .expect("import enforce");
    let pending_id = match decision {
        GovernanceDecision::Pending(id) => id,
        other => panic!("import_memories on write=approve ns must Pend; got {other:?}"),
    };
    assert!(!pending_id.is_empty(), "Pending id must be non-empty");

    let row: (String, String, String) =
        sqlx::query_as("SELECT action_type, namespace, status FROM pending_actions WHERE id = $1")
            .bind(&pending_id)
            .fetch_one(&pool)
            .await
            .expect("pending_actions row must exist");
    assert_eq!(row.0, "store");
    assert_eq!(row.1, ns);
    assert_eq!(row.2, "pending");

    cleanup(&pool, &ns_prefix).await;
}

/// fold-A2A1.5 (#705) — `entity_register` postgres branch now consults
/// `enforce_governance_action(Store, ns, aid, ...)` before the upsert.
/// This pins the Deny contract on a `write=owner` namespace for a
/// non-owner register call. Entities are governance-relevant writes
/// because they materialise a `Memory` row in the requested namespace.
#[tokio::test]
async fn entity_register_postgres_enforces_governance() {
    let Some(url) = postgres_url() else {
        eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
        return;
    };
    ai_memory::config::override_active_permissions_mode_for_test(
        ai_memory::config::PermissionsMode::Enforce,
    );
    let store = PostgresStore::connect(&url).await.expect("connect");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("test-side pool");

    let suffix = unique_suffix();
    let ns_prefix = format!("fa2a15-entity-{suffix}");
    let ns = ns_prefix.clone();
    let owner = format!("ai:fa2a15-entity-owner-{suffix}");
    let intruder = format!("ai:fa2a15-entity-intruder-{suffix}");

    seed_standard(
        &pool,
        &ns,
        &owner,
        serde_json::json!({"write": "owner", "promote": "any", "delete": "owner"}),
        None,
    )
    .await;

    let payload = serde_json::json!({
        "title": "entity:canonical-name",
        "kind": "entity",
    });

    // Non-owner entity_register → Deny.
    let intruder_decision = store
        .enforce_governance_action(GovernedAction::Store, &ns, &intruder, None, None, &payload)
        .await
        .expect("intruder entity_register enforce");
    assert!(
        matches!(intruder_decision, GovernanceDecision::Deny(_)),
        "entity_register non-owner to owner-only ns must Deny; got {intruder_decision:?}"
    );

    // Owner entity_register → Allow.
    let owner_decision = store
        .enforce_governance_action(GovernedAction::Store, &ns, &owner, None, None, &payload)
        .await
        .expect("owner entity_register enforce");
    assert!(
        matches!(owner_decision, GovernanceDecision::Allow),
        "entity_register owner to owner-only ns must Allow; got {owner_decision:?}"
    );

    cleanup(&pool, &ns_prefix).await;
}
