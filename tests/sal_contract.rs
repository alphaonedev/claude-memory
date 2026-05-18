// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Storage Abstraction Layer (SAL) **adapter contract tests**.
//!
//! v0.6.3 W11 / S11a. Both `SqliteStore` and `PostgresStore` implement
//! the `MemoryStore` trait — but the per-adapter unit tests in
//! `src/store/{sqlite,postgres}.rs` had drifted (10 tests for
//! `Postgres`, 4 for `SQLite`). One adapter could shift behaviour without
//! the other catching it.
//!
//! This file holds **generic** tests that take `&dyn MemoryStore` and
//! run identically against every backend, exposing divergence the
//! moment one of them moves.
//!
//! ## Gating
//!
//! - The whole file is `#[cfg(feature = "sal")]` — without `sal` there
//!   is no `MemoryStore` trait, so the binary builds with zero tests.
//! - The Postgres mod additionally requires `feature = "sal-postgres"`
//!   AND the `AI_MEMORY_TEST_POSTGRES_URL` env var. When the env var
//!   is unset the postgres tests skip with `eprintln!` rather than
//!   failing — matches the live-PG patterns in
//!   `src/store/postgres.rs::tests`.
//!
//! ## Running
//!
//! ```bash
//! # SQLite contract only (default — no extra deps):
//! cargo test --features sal --test sal_contract -- --test-threads=2
//!
//! # Both backends (requires running Postgres + pgvector):
//! docker compose -f packaging/docker-compose.postgres.yml up -d
//! export AI_MEMORY_TEST_POSTGRES_URL=postgres://ai_memory:ai_memory_test@localhost:5433/ai_memory_test
//! cargo test --features sal-postgres --test sal_contract -- --test-threads=2
//! ```

#![cfg(feature = "sal")]

use ai_memory::models::ConfidenceSource;
use ai_memory::models::{AgentRegistration, Memory, MemoryLink, Tier};
use ai_memory::store::{CallerContext, Capabilities, Filter, MemoryStore, StoreError, UpdatePatch};

mod common;

// ---------------------------------------------------------------------------
// Generic helpers + contract bodies — backend-agnostic.
// ---------------------------------------------------------------------------

/// Build a Memory with stable fields and a random uuid id, so each
/// call produces a unique row even with `SQLite`'s `ON CONFLICT (title,
/// namespace)` upsert semantics.
fn make_memory(namespace: &str, title: &str, content: &str) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        tags: vec!["contract".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "sal-contract".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "ai:contract-test"}),
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
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

fn ctx() -> CallerContext {
    CallerContext::for_agent("ai:contract-test")
}

/// Stable per-test namespace tag — mirrors the Postgres adapter's
/// own `sample_memory` calls that suffix with a uuid so concurrent
/// runs don't collide on the unique (title, namespace) key.
fn unique_namespace(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}

// ---------------------------------------------------------------------------
// 1. insert + get round-trip.
// ---------------------------------------------------------------------------

async fn contract_insert_and_get(store: &dyn MemoryStore) {
    let ns = unique_namespace("c1-insert");
    let mem = make_memory(&ns, "round-trip", "the quick brown fox jumps");
    let returned_id = store.store(&ctx(), &mem).await.expect("store");
    let fetched = store.get(&ctx(), &returned_id).await.expect("get");
    assert_eq!(fetched.title, "round-trip");
    assert_eq!(fetched.namespace, ns);
    assert_eq!(fetched.content, "the quick brown fox jumps");
}

// ---------------------------------------------------------------------------
// 2. list respects `limit`.
//
// Adapted from "pagination" because the `MemoryStore` trait's `list`
// only exposes `limit` — there's no offset on the trait surface
// (SqliteStore::list always passes 0 for offset). The contract is:
// a `Filter { limit: N }` returns at most N results.
// ---------------------------------------------------------------------------

async fn contract_list_respects_limit(store: &dyn MemoryStore) {
    let ns = unique_namespace("c2-limit");
    for i in 0..6_u32 {
        let mem = make_memory(
            &ns,
            &format!("title-{i:02}"),
            &format!("body number {i} for the limit test"),
        );
        store.store(&ctx(), &mem).await.expect("store");
    }
    let f3 = Filter {
        namespace: Some(ns.clone()),
        limit: 3,
        ..Filter::default()
    };
    let listed = store.list(&ctx(), &f3).await.expect("list");
    assert!(
        listed.len() <= 3,
        "limit=3 must cap result count, got {}",
        listed.len()
    );
    let f10 = Filter {
        namespace: Some(ns.clone()),
        limit: 10,
        ..Filter::default()
    };
    let all = store.list(&ctx(), &f10).await.expect("list-all");
    assert_eq!(all.len(), 6, "all 6 inserted rows should be reachable");
}

// ---------------------------------------------------------------------------
// 3. delete by id.
// ---------------------------------------------------------------------------

async fn contract_delete_by_id(store: &dyn MemoryStore) {
    let ns = unique_namespace("c3-delete");
    let mem = make_memory(&ns, "to-delete", "transient row content");
    let id = store.store(&ctx(), &mem).await.expect("store");
    store.delete(&ctx(), &id).await.expect("delete");
    let err = store
        .get(&ctx(), &id)
        .await
        .expect_err("get after delete must error");
    assert!(
        matches!(err, StoreError::NotFound { .. }),
        "expected NotFound, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. update preserves id; content patch reaches storage.
// ---------------------------------------------------------------------------

async fn contract_update_preserves_id(store: &dyn MemoryStore) {
    let ns = unique_namespace("c4-update");
    let mem = make_memory(&ns, "patchable", "initial content body");
    let id = store.store(&ctx(), &mem).await.expect("store");
    let patch = UpdatePatch {
        content: Some("revised content body after patch".to_string()),
        ..UpdatePatch::default()
    };
    store
        .update(&ctx(), &id, patch)
        .await
        .expect("update should succeed");
    let after = store.get(&ctx(), &id).await.expect("get after update");
    assert_eq!(after.id, id, "id must not change across update");
    assert_eq!(after.content, "revised content body after patch");
    assert_eq!(after.title, "patchable", "untouched fields preserved");
}

// ---------------------------------------------------------------------------
// 5. namespace filter — list with namespace returns only matching
// rows (no leakage from sibling namespaces).
//
// The "ancestry" form in the original brief depended on a recursive
// namespace filter that the SAL trait does not expose; the surface
// it does expose is exact-match on `Filter::namespace`. This test
// asserts the parity contract: identical filter ⇒ identical scope on
// both backends.
// ---------------------------------------------------------------------------

async fn contract_namespace_filter_isolates(store: &dyn MemoryStore) {
    let ns_a = unique_namespace("c5-ns-a");
    let ns_b = unique_namespace("c5-ns-b");
    store
        .store(
            &ctx(),
            &make_memory(&ns_a, "in-a-1", "alpha row one body content"),
        )
        .await
        .expect("store a1");
    store
        .store(
            &ctx(),
            &make_memory(&ns_a, "in-a-2", "alpha row two body content"),
        )
        .await
        .expect("store a2");
    store
        .store(
            &ctx(),
            &make_memory(&ns_b, "in-b-1", "beta row one body content"),
        )
        .await
        .expect("store b1");

    let filter_a = Filter {
        namespace: Some(ns_a.clone()),
        limit: 100,
        ..Filter::default()
    };
    let only_a = store.list(&ctx(), &filter_a).await.expect("list");
    assert_eq!(only_a.len(), 2);
    assert!(only_a.iter().all(|m| m.namespace == ns_a));
}

// ---------------------------------------------------------------------------
// 6. capabilities advertise DURABLE + STRONG_CONSISTENCY.
//
// Adapted from "governance decision persists" — `MemoryStore` has no
// `get/set policy` method on the trait; that lives a layer above. The
// closest backend-trait-level invariant is the capabilities bitfield
// the red-team called out as a divergence risk in #302. Both backends
// MUST advertise DURABLE (persists across restart) + STRONG_CONSISTENCY
// (subsequent reads see prior writes). This test enforces that floor.
// ---------------------------------------------------------------------------

async fn contract_capabilities_floor(store: &dyn MemoryStore) {
    let caps = store.capabilities();
    assert!(
        caps.contains(Capabilities::DURABLE),
        "every backend must advertise DURABLE, got {caps:?}"
    );
    assert!(
        caps.contains(Capabilities::STRONG_CONSISTENCY),
        "every backend must advertise STRONG_CONSISTENCY, got {caps:?}"
    );
    assert!(
        caps.contains(Capabilities::FULLTEXT),
        "every backend must advertise FULLTEXT, got {caps:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. verify() returns a report; signature_verified is always false in
// v0.6.x (real signing lands with Task 1.4 — see #302).
//
// Adapted from "sync_state advance" — the trait has no sync-state
// method; verify() is the closest cross-adapter contract surface and
// the one whose `signature_verified` flag was explicitly called out
// as load-bearing for the trust model.
// ---------------------------------------------------------------------------

async fn contract_verify_returns_report(store: &dyn MemoryStore) {
    let ns = unique_namespace("c7-verify");
    let mem = make_memory(&ns, "verifiable", "non-empty body for verify check");
    let id = store.store(&ctx(), &mem).await.expect("store");
    let report = store.verify(&ctx(), &id).await.expect("verify");
    assert_eq!(report.memory_id, id);
    assert!(
        !report.signature_verified,
        "v0.6.x verify() must NOT claim signature_verified — Task 1.4 follow-up"
    );
}

// ---------------------------------------------------------------------------
// 8. get-after-delete returns NotFound (round-trip → erase → vanish).
//
// Adapted from "archive round trip" — there's no archive method on
// the SAL trait surface. The closest cross-adapter contract is the
// hard-delete erasure invariant: once deleted, get must fail with
// NotFound; calling delete a second time must also fail with NotFound
// (no silent success).
// ---------------------------------------------------------------------------

async fn contract_double_delete_is_not_found(store: &dyn MemoryStore) {
    let ns = unique_namespace("c8-doubledel");
    let mem = make_memory(&ns, "twice-removed", "body for the double-delete test");
    let id = store.store(&ctx(), &mem).await.expect("store");
    store.delete(&ctx(), &id).await.expect("first delete");
    let err = store
        .delete(&ctx(), &id)
        .await
        .expect_err("second delete must error");
    assert!(
        matches!(err, StoreError::NotFound { .. }),
        "double-delete must surface NotFound, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 9. FTS search — inserting "hello world" makes "hello" a hit.
// ---------------------------------------------------------------------------

async fn contract_fts_search_finds_inserted(store: &dyn MemoryStore) {
    let ns = unique_namespace("c9-fts");
    let mem = make_memory(
        &ns,
        "greetings",
        "hello world this is the body content for full text search",
    );
    let id = store.store(&ctx(), &mem).await.expect("store");
    let filter = Filter {
        namespace: Some(ns.clone()),
        limit: 10,
        ..Filter::default()
    };
    let hits = store
        .search(&ctx(), "hello", &filter)
        .await
        .expect("search");
    assert!(
        hits.iter().any(|m| m.id == id),
        "FTS for 'hello' should find the inserted row (got {} hits)",
        hits.len()
    );
}

// ---------------------------------------------------------------------------
// 10. concurrent writes — N tokio tasks each insert one row, after
// joining the namespace contains N rows. Surfaces lock-handling
// regressions (e.g. a future SqliteStore that drops the `Mutex`
// without enabling WAL would deadlock or lose writes).
// ---------------------------------------------------------------------------

async fn contract_concurrent_writes_no_data_loss<S>(store: std::sync::Arc<S>)
where
    S: MemoryStore + 'static,
{
    let ns = unique_namespace("c10-concurrent");
    let writers: u32 = 8;
    let mut handles = Vec::with_capacity(writers as usize);
    for i in 0..writers {
        let store = store.clone();
        let ns = ns.clone();
        handles.push(tokio::spawn(async move {
            let mem = make_memory(
                &ns,
                &format!("row-{i:02}"),
                &format!("body for concurrent writer {i}"),
            );
            store.store(&ctx(), &mem).await
        }));
    }
    for h in handles {
        h.await.expect("join").expect("store");
    }
    let listed = store
        .list(
            &ctx(),
            &Filter {
                namespace: Some(ns),
                limit: 100,
                ..Filter::default()
            },
        )
        .await
        .expect("list");
    assert_eq!(
        listed.len(),
        writers as usize,
        "all N concurrent writes must survive (got {} of {})",
        listed.len(),
        writers
    );
}

// ---------------------------------------------------------------------------
// 11. links CRUD round-trip — F6 Gap 2/3 (v0.7.0).
//
// Both adapters MUST: insert a link, surface it via `list_links`, treat
// duplicate inserts as idempotent (no error, no extra row), and accept
// the signed-link path through `link_signed`. Coverage spans the full
// SAL link surface so a future divergence between SqliteStore and
// PostgresStore catches here.
// ---------------------------------------------------------------------------

fn make_link(source_id: &str, target_id: &str, relation: &str) -> MemoryLink {
    MemoryLink {
        source_id: source_id.to_string(),
        target_id: target_id.to_string(),
        // v0.7.0 fix campaign R1-M4 — typed relation closed-set.
        relation: ai_memory::models::MemoryLinkRelation::from_str(relation)
            .expect("test fixture relation must be one of the closed-set variants"),
        created_at: chrono::Utc::now().to_rfc3339(),
        signature: None,
        observed_by: None,
        valid_from: None,
        valid_until: None,
        attest_level: None,
    }
}

async fn contract_links_crud_roundtrip(store: &dyn MemoryStore) {
    let ns = unique_namespace("c11-links");
    // Seed two memories that the link will point at.
    let mem_a = make_memory(&ns, "link-source", "the source of the link");
    let mem_b = make_memory(&ns, "link-target", "the target of the link");
    let id_a = store.store(&ctx(), &mem_a).await.expect("store a");
    let id_b = store.store(&ctx(), &mem_b).await.expect("store b");

    // 1. Insert: unsigned write through the trait's `link()`.
    let link = make_link(&id_a, &id_b, "related_to");
    store.link(&ctx(), &link).await.expect("link write");

    // 2. Surface via `list_links` — namespace filter narrows to source's
    //    namespace.
    let listed = store
        .list_links(Some(&ns))
        .await
        .expect("list_links scoped");
    let matches: Vec<&MemoryLink> = listed
        .iter()
        .filter(|l| {
            l.source_id == id_a
                && l.target_id == id_b
                && l.relation == ai_memory::models::MemoryLinkRelation::RelatedTo
        })
        .collect();
    assert_eq!(matches.len(), 1, "exactly one matching link expected");

    // 3. Duplicate insert is idempotent — no error, no extra row.
    store.link(&ctx(), &link).await.expect("link write 2");
    let listed_again = store
        .list_links(Some(&ns))
        .await
        .expect("list_links scoped 2");
    let matches_again: Vec<&MemoryLink> = listed_again
        .iter()
        .filter(|l| {
            l.source_id == id_a
                && l.target_id == id_b
                && l.relation == ai_memory::models::MemoryLinkRelation::RelatedTo
        })
        .collect();
    assert_eq!(
        matches_again.len(),
        1,
        "duplicate link writes must collapse on the unique key"
    );

    // 4. Different relation between the same pair is a distinct row.
    let link_alt = make_link(&id_a, &id_b, "supersedes");
    store.link(&ctx(), &link_alt).await.expect("link alt write");
    let listed_three = store
        .list_links(Some(&ns))
        .await
        .expect("list_links scoped 3");
    let edge_count = listed_three
        .iter()
        .filter(|l| l.source_id == id_a && l.target_id == id_b)
        .count();
    assert_eq!(
        edge_count, 2,
        "(src, tgt, relation) triple is the unique key — distinct relations live as distinct rows"
    );

    // 5. `list_links(None)` returns at least our two links (may include
    //    rows from concurrent tests using other namespaces — assert
    //    superset rather than equality).
    let unscoped = store.list_links(None).await.expect("list_links unscoped");
    assert!(
        unscoped.len() >= 2,
        "unscoped list_links must include namespace-scoped rows"
    );
}

async fn contract_link_signed_roundtrip(store: &dyn MemoryStore) {
    let ns = unique_namespace("c12-link-signed");
    let mem_a = make_memory(&ns, "signed-source", "signed source body");
    let mem_b = make_memory(&ns, "signed-target", "signed target body");
    let id_a = store.store(&ctx(), &mem_a).await.expect("store a");
    let id_b = store.store(&ctx(), &mem_b).await.expect("store b");

    // Generate a fresh keypair (in-memory only — no disk write).
    let kp = ai_memory::identity::keypair::generate("ai:contract-test").expect("generate keypair");
    assert!(
        kp.can_sign(),
        "freshly generated keypair must hold a private key"
    );

    // Signed write — `attest_level` must be `"self_signed"`.
    let link = make_link(&id_a, &id_b, "related_to");
    let level = store
        .link_signed(&ctx(), &link, Some(&kp))
        .await
        .expect("link_signed");
    assert_eq!(
        level, "self_signed",
        "keypair-bearing link must report self_signed"
    );

    // Read back through `list_links` and verify the signature is
    // 64 bytes (Ed25519) and `observed_by` matches the signer.
    let listed = store
        .list_links(Some(&ns))
        .await
        .expect("list_links signed");
    let row = listed
        .iter()
        .find(|l| l.source_id == id_a && l.target_id == id_b)
        .expect("signed link row");
    assert_eq!(
        row.signature.as_ref().map(Vec::len),
        Some(64),
        "Ed25519 signature is 64 bytes"
    );
    assert_eq!(
        row.observed_by.as_deref(),
        Some("ai:contract-test"),
        "observed_by must echo the signing agent_id"
    );

    // Unsigned write (no keypair) — `attest_level = "unsigned"`,
    // `signature = NULL`, `observed_by = NULL`.
    let mem_c = make_memory(&ns, "unsigned-source", "unsigned source body");
    let id_c = store.store(&ctx(), &mem_c).await.expect("store c");
    let unsigned_link = make_link(&id_c, &id_b, "related_to");
    let level_unsigned = store
        .link_signed(&ctx(), &unsigned_link, None)
        .await
        .expect("link_signed unsigned");
    assert_eq!(
        level_unsigned, "unsigned",
        "no-keypair path must surface unsigned"
    );
    let listed_unsigned = store
        .list_links(Some(&ns))
        .await
        .expect("list_links unsigned");
    let unsigned_row = listed_unsigned
        .iter()
        .find(|l| l.source_id == id_c && l.target_id == id_b)
        .expect("unsigned link row");
    assert!(
        unsigned_row.signature.is_none(),
        "unsigned write must leave signature NULL"
    );
    assert!(
        unsigned_row.observed_by.is_none(),
        "unsigned write must leave observed_by NULL"
    );
}

// ---------------------------------------------------------------------------
// 13. agent registration round-trip — F6 Gap 4 (v0.7.0).
//
// Both adapters MUST persist agent registrations in a re-readable form,
// preserve `registered_at` across re-registration (caller-observable
// provenance), and refresh `last_seen_at` on every re-register call.
// ---------------------------------------------------------------------------

async fn contract_agent_registration_roundtrip(store: &dyn MemoryStore) {
    let agent_id = format!("ai:contract-{}", uuid::Uuid::new_v4());
    let agent = AgentRegistration {
        agent_id: agent_id.clone(),
        agent_type: "test-runner".to_string(),
        capabilities: vec!["read".to_string(), "write".to_string()],
        registered_at: String::new(), // adapter populates
        last_seen_at: String::new(),
    };

    // First registration must succeed.
    store
        .register_agent(&ctx(), &agent)
        .await
        .expect("register first time");

    // Re-registration is idempotent (no error). The persisted record's
    // `registered_at` must NOT regress on re-register; both adapters
    // store the agent as a memory in `_agents`, so we read it back via
    // `list` to assert.
    let f = Filter {
        namespace: Some("_agents".to_string()),
        limit: 100,
        ..Filter::default()
    };
    let all = store.list(&ctx(), &f).await.expect("list _agents");
    let row_first = all
        .iter()
        .find(|m| m.title == format!("agent:{agent_id}"))
        .expect("registered agent row")
        .clone();
    let registered_at_first = row_first
        .metadata
        .get("registered_at")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .expect("registered_at stamp");

    // Sleep just enough to advance the wall clock past the timestamp's
    // resolution before re-registering.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    store
        .register_agent(&ctx(), &agent)
        .await
        .expect("register second time");

    let all_after = store.list(&ctx(), &f).await.expect("list _agents 2");
    let row_second = all_after
        .iter()
        .find(|m| m.title == format!("agent:{agent_id}"))
        .expect("registered agent row 2")
        .clone();
    let registered_at_second = row_second
        .metadata
        .get("registered_at")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .expect("registered_at stamp 2");
    assert_eq!(
        registered_at_first, registered_at_second,
        "registered_at must be preserved across re-registration"
    );

    let last_seen_first = row_first
        .metadata
        .get("last_seen_at")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .expect("last_seen_at stamp");
    let last_seen_second = row_second
        .metadata
        .get("last_seen_at")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .expect("last_seen_at stamp 2");
    assert!(
        last_seen_second >= last_seen_first,
        "last_seen_at must monotonically advance on re-registration \
         (first={last_seen_first}, second={last_seen_second})"
    );
}

// ===========================================================================
// SQLite adapter wrappers — runs every contract above against a fresh
// temp-DB SqliteStore. Matches the per-test-fresh-store pattern from
// the existing `src/store/sqlite.rs::tests` module.
// ===========================================================================

mod sqlite_contract {
    use super::{
        contract_agent_registration_roundtrip, contract_capabilities_floor,
        contract_concurrent_writes_no_data_loss, contract_delete_by_id,
        contract_double_delete_is_not_found, contract_fts_search_finds_inserted,
        contract_insert_and_get, contract_link_signed_roundtrip, contract_links_crud_roundtrip,
        contract_list_respects_limit, contract_namespace_filter_isolates,
        contract_update_preserves_id, contract_verify_returns_report,
    };
    use ai_memory::store::sqlite::SqliteStore;

    /// Each test gets its own `NamedTempFile` to keep the harness clean.
    /// The file is held alongside the store so it lives long enough.
    struct Fixture {
        store: SqliteStore,
        // RAII: tempfile cleans up on drop. We carry it through.
        _tmp: tempfile::NamedTempFile,
    }

    fn fresh_store() -> Fixture {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let store = SqliteStore::open(tmp.path()).expect("open SqliteStore");
        Fixture { store, _tmp: tmp }
    }

    #[tokio::test]
    async fn insert_and_get() {
        let fx = fresh_store();
        contract_insert_and_get(&fx.store).await;
    }

    #[tokio::test]
    async fn list_respects_limit() {
        let fx = fresh_store();
        contract_list_respects_limit(&fx.store).await;
    }

    #[tokio::test]
    async fn delete_by_id() {
        let fx = fresh_store();
        contract_delete_by_id(&fx.store).await;
    }

    #[tokio::test]
    async fn update_preserves_id() {
        let fx = fresh_store();
        contract_update_preserves_id(&fx.store).await;
    }

    #[tokio::test]
    async fn namespace_filter_isolates() {
        let fx = fresh_store();
        contract_namespace_filter_isolates(&fx.store).await;
    }

    #[tokio::test]
    async fn capabilities_floor() {
        let fx = fresh_store();
        contract_capabilities_floor(&fx.store).await;
    }

    #[tokio::test]
    async fn verify_returns_report() {
        let fx = fresh_store();
        contract_verify_returns_report(&fx.store).await;
    }

    #[tokio::test]
    async fn double_delete_is_not_found() {
        let fx = fresh_store();
        contract_double_delete_is_not_found(&fx.store).await;
    }

    #[tokio::test]
    async fn fts_search_finds_inserted() {
        let fx = fresh_store();
        contract_fts_search_finds_inserted(&fx.store).await;
    }

    #[tokio::test]
    async fn concurrent_writes_no_data_loss() {
        // SqliteStore wraps a `tokio::sync::Mutex<rusqlite::Connection>`
        // + WAL journal mode (set in `db::open`), so concurrent calls
        // serialize but don't drop writes — exactly the contract this
        // test asserts.
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let store = std::sync::Arc::new(SqliteStore::open(tmp.path()).expect("open"));
        contract_concurrent_writes_no_data_loss(store).await;
        // Hold tmp until the end of the test so the DB file outlives
        // the spawned tasks.
        drop(tmp);
    }

    #[tokio::test]
    async fn links_crud_roundtrip() {
        let fx = fresh_store();
        contract_links_crud_roundtrip(&fx.store).await;
    }

    #[tokio::test]
    async fn link_signed_roundtrip() {
        let fx = fresh_store();
        contract_link_signed_roundtrip(&fx.store).await;
    }

    #[tokio::test]
    async fn agent_registration_roundtrip() {
        let fx = fresh_store();
        contract_agent_registration_roundtrip(&fx.store).await;
    }
}

// ===========================================================================
// Postgres adapter wrappers — same contracts, behind two gates:
//
// 1. `feature = "sal-postgres"` for the `PostgresStore` type to exist.
// 2. `AI_MEMORY_TEST_POSTGRES_URL` set at test run time, otherwise
//    each test eprintln's "skip" and returns Ok. This matches the
//    pattern used by the live integration tests inside
//    `src/store/postgres.rs::tests`.
//
// We deliberately do NOT add a new `test-postgres` feature — adding a
// feature flag for a runtime env-var skip is friction without payoff,
// and the matching pattern is already established in-tree.
// ===========================================================================

#[cfg(feature = "sal-postgres")]
mod postgres_contract {
    use super::{
        contract_agent_registration_roundtrip, contract_capabilities_floor,
        contract_concurrent_writes_no_data_loss, contract_delete_by_id,
        contract_double_delete_is_not_found, contract_fts_search_finds_inserted,
        contract_insert_and_get, contract_link_signed_roundtrip, contract_links_crud_roundtrip,
        contract_list_respects_limit, contract_namespace_filter_isolates,
        contract_update_preserves_id, contract_verify_returns_report,
    };
    use ai_memory::store::postgres::PostgresStore;

    use crate::common::postgres_url;

    async fn fresh_store() -> Option<PostgresStore> {
        let url = postgres_url()?;
        match PostgresStore::connect(&url).await {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("skip: PostgresStore::connect failed: {e}");
                None
            }
        }
    }

    #[tokio::test]
    async fn insert_and_get() {
        let Some(store) = fresh_store().await else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        contract_insert_and_get(&store).await;
    }

    #[tokio::test]
    async fn list_respects_limit() {
        let Some(store) = fresh_store().await else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        contract_list_respects_limit(&store).await;
    }

    #[tokio::test]
    async fn delete_by_id() {
        let Some(store) = fresh_store().await else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        contract_delete_by_id(&store).await;
    }

    #[tokio::test]
    async fn update_preserves_id() {
        let Some(store) = fresh_store().await else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        contract_update_preserves_id(&store).await;
    }

    #[tokio::test]
    async fn namespace_filter_isolates() {
        let Some(store) = fresh_store().await else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        contract_namespace_filter_isolates(&store).await;
    }

    #[tokio::test]
    async fn capabilities_floor() {
        let Some(store) = fresh_store().await else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        contract_capabilities_floor(&store).await;
    }

    #[tokio::test]
    async fn verify_returns_report() {
        let Some(store) = fresh_store().await else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        contract_verify_returns_report(&store).await;
    }

    #[tokio::test]
    async fn double_delete_is_not_found() {
        let Some(store) = fresh_store().await else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        contract_double_delete_is_not_found(&store).await;
    }

    #[tokio::test]
    async fn fts_search_finds_inserted() {
        let Some(store) = fresh_store().await else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        contract_fts_search_finds_inserted(&store).await;
    }

    #[tokio::test]
    async fn concurrent_writes_no_data_loss() {
        let Some(store) = fresh_store().await else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        let store = std::sync::Arc::new(store);
        contract_concurrent_writes_no_data_loss(store).await;
    }

    #[tokio::test]
    async fn links_crud_roundtrip() {
        let Some(store) = fresh_store().await else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        contract_links_crud_roundtrip(&store).await;
    }

    #[tokio::test]
    async fn link_signed_roundtrip() {
        let Some(store) = fresh_store().await else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        contract_link_signed_roundtrip(&store).await;
    }

    #[tokio::test]
    async fn agent_registration_roundtrip() {
        let Some(store) = fresh_store().await else {
            eprintln!("skip: AI_MEMORY_TEST_POSTGRES_URL not set");
            return;
        };
        contract_agent_registration_roundtrip(&store).await;
    }
}
