// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #910 — SAL-level scope=private visibility filter, the
//! load-bearing enforcement surface beneath every query path.
//!
//! Pre-fix the visibility filter only ran in two handlers
//! (`list_memories`, `kg_query`); a caller authenticated as `bob`
//! could enumerate `alice`'s scope=private rows via any of the
//! OTHER query paths (`recall_hybrid`, `search`, `get`, `find_paths`,
//! `list_memories_updated_since`, `export_memories`, etc.) because
//! they dispatched straight through the SAL trait without a
//! visibility post-filter.
//!
//! Post-fix the canonical [`ai_memory::store::is_visible_to_caller`]
//! predicate runs inside every SAL query method, so every
//! upper-layer caller — HTTP handler, MCP tool, federation
//! receiver, future SDK consumer — inherits the filter without
//! having to remember a per-callsite post-filter.
//!
//! These tests pin the contract by exercising every SAL query
//! method directly against a two-agent scenario:
//!
//! 1. Alice stores a memory with `metadata.scope == "private"` and
//!    `metadata.agent_id == "alice"`.
//! 2. Bob calls each query path through the SAL trait surface.
//! 3. Every query path MUST return zero rows / zero paths /
//!    `NotFound` for Alice's private row.
//! 4. Alice (the owner) MUST see her own row on every path
//!    (owner-exemption sanity).

#![cfg(feature = "sal")]

use std::sync::Arc;

use ai_memory::models::{
    ConfidenceSource, Memory, MemoryKind, MemoryLink, MemoryLinkRelation, Tier,
};
use ai_memory::store::{CallerContext, Filter, MemoryStore, sqlite::SqliteStore};
use serde_json::json;
use tempfile::NamedTempFile;

const NS: &str = "shared-ns-sal-910";

fn make_memory(title: &str, content: &str, owner: &str, scope: &str) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: NS.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        tags: vec!["sal910".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({"agent_id": owner, "scope": scope}),
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

async fn fixture() -> (Arc<dyn MemoryStore>, NamedTempFile, String, String) {
    let f = NamedTempFile::new().expect("tempfile");
    let store: Arc<dyn MemoryStore> =
        Arc::new(SqliteStore::open(f.path()).expect("open SqliteStore"));
    let alice_ctx = CallerContext::for_agent("alice");
    // Alice stores a SCOPE=PRIVATE row + a SCOPE=COLLECTIVE control.
    let private_mem = make_memory(
        "alice-private-sal-910",
        "alice's private body — bob must never see this row",
        "alice",
        "private",
    );
    let collective_mem = make_memory(
        "alice-collective-sal-910",
        "alice's collective body — bob may see this row",
        "alice",
        "collective",
    );
    let private_id = store
        .store(&alice_ctx, &private_mem)
        .await
        .expect("store private");
    let collective_id = store
        .store(&alice_ctx, &collective_mem)
        .await
        .expect("store collective");
    (store, f, private_id, collective_id)
}

// =====================================================================
// 1. `list` — bob sees collective but NOT private; alice sees both.
// =====================================================================

#[tokio::test]
async fn bob_list_excludes_alice_private_910_sal() {
    let (store, _f, _pid, _cid) = fixture().await;
    let bob = CallerContext::for_agent("bob");
    let filter = Filter {
        namespace: Some(NS.to_string()),
        limit: 100,
        ..Filter::default()
    };
    let rows = store.list(&bob, &filter).await.expect("list");
    assert_eq!(
        rows.len(),
        1,
        "#910 SAL: bob must see only alice's collective row (got {})",
        rows.len()
    );
    for r in &rows {
        let scope = r
            .metadata
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("private");
        assert_ne!(
            scope, "private",
            "#910 SAL: bob's list MUST NOT contain a private row"
        );
    }
}

#[tokio::test]
async fn alice_list_includes_own_private_910_sal() {
    let (store, _f, _pid, _cid) = fixture().await;
    let alice = CallerContext::for_agent("alice");
    let filter = Filter {
        namespace: Some(NS.to_string()),
        limit: 100,
        ..Filter::default()
    };
    let rows = store.list(&alice, &filter).await.expect("list");
    assert_eq!(
        rows.len(),
        2,
        "#910 SAL: alice (owner) MUST see her own private + collective rows"
    );
}

// =====================================================================
// 2. `search` — bob does not find alice's private row.
// =====================================================================

#[tokio::test]
async fn bob_search_excludes_alice_private_910_sal() {
    let (store, _f, _pid, _cid) = fixture().await;
    let bob = CallerContext::for_agent("bob");
    let filter = Filter {
        namespace: Some(NS.to_string()),
        limit: 100,
        ..Filter::default()
    };
    let rows = store.search(&bob, "body", &filter).await.expect("search");
    for r in &rows {
        let scope = r
            .metadata
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("private");
        assert_ne!(
            scope, "private",
            "#910 SAL: bob's search MUST NOT surface a private row, got id={}",
            r.id
        );
    }
}

#[tokio::test]
async fn alice_search_includes_own_private_910_sal() {
    let (store, _f, _pid, _cid) = fixture().await;
    let alice = CallerContext::for_agent("alice");
    let filter = Filter {
        namespace: Some(NS.to_string()),
        limit: 100,
        ..Filter::default()
    };
    let rows = store
        .search(&alice, "private", &filter)
        .await
        .expect("search");
    assert!(
        rows.iter().any(|r| r.title == "alice-private-sal-910"),
        "#910 SAL: alice (owner) MUST find her own private row via search"
    );
}

// =====================================================================
// 3. `recall_hybrid` — keyword-only fallback path.
// =====================================================================

#[tokio::test]
async fn bob_recall_excludes_alice_private_910_sal() {
    let (store, _f, _pid, _cid) = fixture().await;
    let bob = CallerContext::for_agent("bob");
    let filter = Filter {
        namespace: Some(NS.to_string()),
        limit: 100,
        ..Filter::default()
    };
    let results = store
        .recall_hybrid(&bob, "body", None, &filter)
        .await
        .expect("recall_hybrid");
    for (r, _score) in &results {
        let scope = r
            .metadata
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("private");
        assert_ne!(
            scope, "private",
            "#910 SAL: bob's recall_hybrid MUST NOT surface a private row, got id={}",
            r.id
        );
    }
}

// =====================================================================
// 4. `get` — direct id fetch returns NotFound for non-owner of private.
// =====================================================================

#[tokio::test]
async fn bob_get_alice_private_returns_notfound_910_sal() {
    let (store, _f, private_id, _cid) = fixture().await;
    let bob = CallerContext::for_agent("bob");
    let err = store
        .get(&bob, &private_id)
        .await
        .expect_err("#910 SAL: bob's get MUST fail-closed");
    assert!(
        matches!(err, ai_memory::store::StoreError::NotFound { .. }),
        "#910 SAL: bob's get on alice's private id MUST surface as NotFound (not leak existence), got {err:?}"
    );
}

#[tokio::test]
async fn bob_get_alice_collective_succeeds_910_sal() {
    let (store, _f, _pid, collective_id) = fixture().await;
    let bob = CallerContext::for_agent("bob");
    let mem = store
        .get(&bob, &collective_id)
        .await
        .expect("#910 SAL precision: collective row MUST be visible cross-tenant");
    assert_eq!(mem.id, collective_id);
}

#[tokio::test]
async fn alice_get_own_private_succeeds_910_sal() {
    let (store, _f, private_id, _cid) = fixture().await;
    let alice = CallerContext::for_agent("alice");
    let mem = store
        .get(&alice, &private_id)
        .await
        .expect("#910 SAL owner exemption: alice MUST see her own private row");
    assert_eq!(mem.id, private_id);
}

// =====================================================================
// 5. `find_paths` — graph traversal drops paths through invisible nodes.
// =====================================================================

#[tokio::test]
async fn bob_find_paths_drops_invisible_nodes_910_sal() {
    let (store, _f, private_id, collective_id) = fixture().await;
    let alice_ctx = CallerContext::for_agent("alice");
    // Wire a 1-hop link so a path exists in the graph.
    let link = MemoryLink {
        source_id: collective_id.clone(),
        target_id: private_id.clone(),
        relation: MemoryLinkRelation::RelatedTo,
        created_at: chrono::Utc::now().to_rfc3339(),
        valid_from: None,
        valid_until: None,
        observed_by: None,
        signature: None,
        attest_level: None,
    };
    store.link(&alice_ctx, &link).await.expect("link");

    let bob = CallerContext::for_agent("bob");
    let paths = store
        .find_paths(&bob, &collective_id, &private_id, Some(3), Some(16))
        .await
        .expect("find_paths");
    assert!(
        paths.is_empty(),
        "#910 SAL: bob's find_paths MUST drop any path that traverses alice's private row, got {paths:?}"
    );

    // Alice (owner) sees the path.
    let paths_alice = store
        .find_paths(&alice_ctx, &collective_id, &private_id, Some(3), Some(16))
        .await
        .expect("find_paths alice");
    assert!(
        !paths_alice.is_empty(),
        "#910 SAL owner exemption: alice MUST see her own path"
    );
}

// =====================================================================
// 6. `as_agent` (Task 1.5) — the impersonation override flows through.
// =====================================================================

#[tokio::test]
async fn as_agent_alice_lets_operator_see_private_910_sal() {
    let (store, _f, private_id, _cid) = fixture().await;
    // Operator opens a `CallerContext` whose authenticated principal
    // is `operator-svc`, but with `as_agent=alice` so the SAL filter
    // runs as alice (the row's owner) — `is_visible_to_caller`
    // returns true.
    let mut ctx = CallerContext::for_agent("operator-svc");
    ctx.as_agent = Some("alice".to_string());
    let mem = store
        .get(&ctx, &private_id)
        .await
        .expect("#910 SAL: as_agent override MUST flow into visibility");
    assert_eq!(mem.id, private_id);
}

// =====================================================================
// 7. CallerContext::effective_principal — surface predictability pin.
// =====================================================================

#[test]
fn effective_principal_prefers_as_agent_over_agent_id() {
    let mut ctx = CallerContext::for_agent("auth-bob");
    assert_eq!(ctx.effective_principal(), "auth-bob");
    ctx.as_agent = Some("act-as-alice".to_string());
    assert_eq!(ctx.effective_principal(), "act-as-alice");
}

// =====================================================================
// 8. is_visible_to_caller — predicate table truth.
// =====================================================================

#[test]
fn is_visible_to_caller_predicate_truth_table() {
    use ai_memory::store::is_visible_to_caller;

    let private_alice = make_memory("t-1", "c", "alice", "private");
    let collective = make_memory("t-2", "c", "alice", "collective");
    let team = make_memory("t-3", "c", "alice", "team");
    let no_scope = Memory {
        metadata: json!({"agent_id": "alice"}),
        ..make_memory("t-4", "c", "alice", "private")
    };

    // private to alice ⇒ visible only to alice
    assert!(is_visible_to_caller(&private_alice, "alice"));
    assert!(!is_visible_to_caller(&private_alice, "bob"));
    // collective ⇒ visible to everyone
    assert!(is_visible_to_caller(&collective, "bob"));
    assert!(is_visible_to_caller(&collective, "alice"));
    // team ⇒ visible to everyone (only `private` is dropped at SAL —
    // namespace-shape scopes are handled by the storage-layer
    // visibility_clause; SAL drops only `private`)
    assert!(is_visible_to_caller(&team, "bob"));
    // missing scope ⇒ treated as private (NHI contract default)
    assert!(is_visible_to_caller(&no_scope, "alice"));
    assert!(!is_visible_to_caller(&no_scope, "bob"));

    // Inbox-row carve-out — notify writes a private memory with
    // target_agent_id; the target must see their own inbox.
    let inbox = Memory {
        metadata: json!({
            "agent_id": "alice",          // sender
            "target_agent_id": "bob",     // inbox owner
        }),
        ..make_memory("t-5", "c", "alice", "private")
    };
    assert!(
        is_visible_to_caller(&inbox, "alice"),
        "sender keeps visibility via owner check"
    );
    assert!(
        is_visible_to_caller(&inbox, "bob"),
        "target sees their own inbox via target_agent_id check"
    );
    assert!(
        !is_visible_to_caller(&inbox, "charlie"),
        "uninvolved party MUST NOT see another agent's inbox"
    );
}
