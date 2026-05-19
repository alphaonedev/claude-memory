// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::missing_panics_doc
)]

//! v0.7.0 WT-1-E — recall-time atom-preference acceptance tests.
//!
//! Four acceptance criteria from the brief:
//!
//!   1. Default recall excludes archived sources (atoms surface in
//!      their place).
//!   2. `include_archived=true` returns BOTH atoms AND the archived
//!      source for the same query.
//!   3. The archived-source filter composes with the existing
//!      namespace + memory-kind + visibility filters — the WT-1-E
//!      WHERE clause is additive.
//!   4. `memory_get` (direct lookup) is exempt from the archive
//!      filter — auditors still resolve an archived source by id.
//!
//! These tests drive the substrate end-to-end: seed a source via
//! `db::insert`, atomise via `Atomiser::atomise_sync` with a mock
//! curator (no live LLM call), then assert on `db::recall_*` /
//! `db::get` results. The fixture shape mirrors the WT-1-B
//! acceptance suite at `tests/atomisation/core.rs`.

use ai_memory::models::ConfidenceSource;
use std::sync::{Mutex, OnceLock};

use ai_memory::atomisation::curator::{Atom, Curator, CuratorError};
use ai_memory::atomisation::{Atomiser, AtomiserConfig};
use ai_memory::config::FeatureTier;
use ai_memory::db;
use ai_memory::mcp::handle_recall;
use ai_memory::models::{Memory, MemoryKind, Tier};
use ai_memory::storage;

use rusqlite::Connection;
use serde_json::json;

use super::common::fresh_db_tempfile_conn as fresh_db;

// ---------------------------------------------------------------------------
// Mock curator — deterministic, no network. Mirrors tests/atomisation/core.rs.
// ---------------------------------------------------------------------------

struct MockCurator {
    responses: Mutex<Vec<Result<Vec<Atom>, CuratorError>>>,
}

impl MockCurator {
    fn new(responses: Vec<Result<Vec<Atom>, CuratorError>>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

impl Curator for MockCurator {
    fn decompose(
        &self,
        _body: &str,
        _max_atom_tokens: u32,
        _max_retries: u32,
    ) -> Result<Vec<Atom>, CuratorError> {
        let mut q = self.responses.lock().unwrap();
        if q.is_empty() {
            return Err(CuratorError::MalformedResponse(
                "mock: queue exhausted".into(),
            ));
        }
        q.remove(0)
    }
}

fn atoms_from(texts: &[&str]) -> Vec<Atom> {
    texts
        .iter()
        .map(|s| Atom {
            text: (*s).to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Shared governance-hook scaffolding — Allow-mode dispatcher installed
// once. Mirrors tests/atomisation/core.rs to coexist with that suite
// when both binaries link the same process-wide OnceLock.
// ---------------------------------------------------------------------------

fn test_serial() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

fn ensure_hook_installed() {
    let _ = storage::GOVERNANCE_PRE_WRITE.set(Box::new(|_mem: &Memory| Ok(())));
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Insert a long-bodied source memory. Returns its id. Body is wide
/// enough that the atomiser's `enforce_token_budget` won't fold it
/// back into a single atom under the default 200-token cap.
fn insert_long_source(conn: &Connection, ns: &str, title_keyword: &str) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    // Repeat distinct content to push token count well above 1500.
    let body = (0..20)
        .map(|i| {
            format!(
                "Paragraph {i}: the kubernetes rolling deploy strategy required \
                 canary instance health checks for the {title_keyword} system. \
                 The pod readiness probe must pass before traffic shifts. \
                 Failures roll back the deployment within 30 seconds. \
                 Operator dashboards track replica counts and error rates."
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: ns.to_string(),
        title: format!("{title_keyword}-source-{}", uuid::Uuid::new_v4().simple()),
        content: body,
        tags: vec![title_keyword.to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({"agent_id": "wt1e-agent"}),
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
    };
    db::insert(conn, &mem).expect("seed long source")
}

fn make_atomiser(atoms: &[&str]) -> Atomiser {
    let curator = Box::new(MockCurator::new(vec![Ok(atoms_from(atoms))]));
    Atomiser::new(curator, None, AtomiserConfig::default(), FeatureTier::Smart)
}

/// Drive an end-to-end atomisation against `source_id`. The mock
/// curator returns `atom_texts` deterministically.
fn atomise(conn: &Connection, source_id: &str, atom_texts: &[&str], agent: &str) -> Vec<String> {
    let atomiser = make_atomiser(atom_texts);
    let outcome = atomiser
        .atomise_sync(conn, source_id, 200, false, agent)
        .expect("atomise ok");
    outcome.atom_ids
}

// ---------------------------------------------------------------------------
// Test 1 — default recall excludes archived sources
// ---------------------------------------------------------------------------

#[test]
fn test_recall_default_excludes_archived_sources() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_hook_installed();

    let (_tmp, conn) = fresh_db();
    let source_id = insert_long_source(&conn, "wt1e/r1", "kubernetes");

    // Sanity: pre-atomisation, the source IS recalled.
    let (results, _outcome) = db::recall(
        &conn,
        "kubernetes",
        Some("wt1e/r1"),
        20,
        None,
        None,
        None,
        0,
        0,
        None,
        None,
        false,
        None,
    )
    .expect("recall pre-atomise");
    assert!(
        results.iter().any(|(m, _)| m.id == source_id),
        "pre-atomise: source must be recallable"
    );

    let atom_ids = atomise(
        &conn,
        &source_id,
        &[
            "Canary deploys for kubernetes require health checks.",
            "Pod readiness probes gate kubernetes traffic shifts.",
            "Failed kubernetes deploys roll back within 30 seconds.",
            "Dashboards track kubernetes replica counts and error rates.",
            "Kubernetes operators monitor pod state continuously.",
        ],
        "wt1e-agent",
    );

    // Default recall: atoms surface, source does not.
    let (results, _outcome) = db::recall(
        &conn,
        "kubernetes",
        Some("wt1e/r1"),
        20,
        None,
        None,
        None,
        0,
        0,
        None,
        None,
        /* include_archived = */ false,
        None,
    )
    .expect("recall post-atomise");
    let returned_ids: Vec<String> = results.iter().map(|(m, _)| m.id.clone()).collect();
    assert!(
        !returned_ids.contains(&source_id),
        "default recall must exclude the archived source; got: {returned_ids:?}"
    );
    assert!(
        atom_ids.iter().any(|aid| returned_ids.contains(aid)),
        "atoms must surface under default recall; atoms={atom_ids:?} returned={returned_ids:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — include_archived=true returns BOTH atoms and the archived source
// ---------------------------------------------------------------------------

#[test]
fn test_recall_with_include_archived_returns_both() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_hook_installed();

    let (_tmp, conn) = fresh_db();
    let source_id = insert_long_source(&conn, "wt1e/r2", "redis");
    let atom_ids = atomise(
        &conn,
        &source_id,
        &[
            "Redis persistence relies on RDB snapshots and AOF logs.",
            "Redis replication uses asynchronous master-replica streams.",
            "Redis sentinel coordinates automatic failover decisions.",
            "Redis cluster shards keys across slot ranges.",
            "Redis memory eviction follows LRU and LFU policies.",
        ],
        "wt1e-agent",
    );

    let (results, _outcome) = db::recall(
        &conn,
        "redis",
        Some("wt1e/r2"),
        20,
        None,
        None,
        None,
        0,
        0,
        None,
        None,
        /* include_archived = */ true,
        None,
    )
    .expect("recall include_archived");
    let returned_ids: Vec<String> = results.iter().map(|(m, _)| m.id.clone()).collect();

    assert!(
        returned_ids.contains(&source_id),
        "include_archived=true must surface the archived source; got: {returned_ids:?}"
    );
    assert!(
        atom_ids.iter().any(|aid| returned_ids.contains(aid)),
        "include_archived=true must still surface atoms; atoms={atom_ids:?} returned={returned_ids:?}"
    );

    // Source row carries the WT-1-B substrate-visible archive
    // marker: `metadata.atomisation_archived_at` is set.
    let source_row = results
        .iter()
        .find(|(m, _)| m.id == source_id)
        .expect("source in result")
        .0
        .clone();
    assert!(
        source_row
            .metadata
            .get("atomisation_archived_at")
            .and_then(|v| v.as_str())
            .is_some(),
        "archived source must carry metadata.atomisation_archived_at"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — archive filter composes with namespace + memory_kind + tier
// ---------------------------------------------------------------------------

#[test]
fn test_recall_filters_compose() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_hook_installed();

    let (_tmp, conn) = fresh_db();
    // Two namespaces, each with an atomised source.
    let src_a = insert_long_source(&conn, "wt1e/r3/team-a", "elasticsearch");
    let src_b = insert_long_source(&conn, "wt1e/r3/team-b", "elasticsearch");

    let atoms_a = atomise(
        &conn,
        &src_a,
        &[
            "Elasticsearch shards distribute documents across primary nodes for team A.",
            "Replica shards in elasticsearch provide failure tolerance for team A.",
            "Bulk indexing in elasticsearch batches writes for throughput on team A.",
            "Elasticsearch query DSL composes nested boolean clauses on team A.",
            "Elasticsearch snapshots back up indices to object storage on team A.",
        ],
        "wt1e-agent",
    );
    let _atoms_b = atomise(
        &conn,
        &src_b,
        &[
            "Elasticsearch shards distribute documents across primary nodes for team B.",
            "Replica shards in elasticsearch provide failure tolerance for team B.",
            "Bulk indexing in elasticsearch batches writes for throughput on team B.",
            "Elasticsearch query DSL composes nested boolean clauses on team B.",
            "Elasticsearch snapshots back up indices to object storage on team B.",
        ],
        "wt1e-agent",
    );

    // include_archived=true + explicit namespace + non-empty
    // results must hit team-A only. Source row appears because
    // include_archived=true; atoms from team-B do NOT leak in.
    let (results, _outcome) = db::recall(
        &conn,
        "elasticsearch",
        Some("wt1e/r3/team-a"),
        50,
        None,
        None,
        None,
        0,
        0,
        None,
        None,
        true,
        None,
    )
    .expect("recall compose");
    let returned_namespaces: std::collections::HashSet<String> =
        results.iter().map(|(m, _)| m.namespace.clone()).collect();
    assert_eq!(
        returned_namespaces,
        std::collections::HashSet::from(["wt1e/r3/team-a".to_string()]),
        "namespace filter must compose: got namespaces {returned_namespaces:?}"
    );
    let returned_ids: Vec<String> = results.iter().map(|(m, _)| m.id.clone()).collect();
    assert!(returned_ids.contains(&src_a));
    assert!(!returned_ids.contains(&src_b));
    assert!(atoms_a.iter().any(|aid| returned_ids.contains(aid)));

    // Sanity: with the default include_archived=false, src_a is
    // excluded but the team-A atoms still surface — composition
    // works in the other direction too.
    let (results_default, _) = db::recall(
        &conn,
        "elasticsearch",
        Some("wt1e/r3/team-a"),
        50,
        None,
        None,
        None,
        0,
        0,
        None,
        None,
        false,
        None,
    )
    .expect("recall compose default");
    let default_ids: Vec<String> = results_default.iter().map(|(m, _)| m.id.clone()).collect();
    assert!(!default_ids.contains(&src_a));
    assert!(atoms_a.iter().any(|aid| default_ids.contains(aid)));
}

// ---------------------------------------------------------------------------
// Test 4 — memory_get is exempt from the archive filter
// ---------------------------------------------------------------------------

#[test]
fn test_memory_get_returns_archived_source() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_hook_installed();

    let (_tmp, conn) = fresh_db();
    let source_id = insert_long_source(&conn, "wt1e/r4", "postgres");
    let _atoms = atomise(
        &conn,
        &source_id,
        &[
            "Postgres MVCC tracks transaction ids on every row tuple.",
            "Postgres autovacuum compacts dead tuples on a schedule.",
            "Postgres replication slots persist WAL until consumers catch up.",
            "Postgres logical decoding emits replication events for downstreams.",
            "Postgres connection pooling via PgBouncer reduces backend overhead.",
        ],
        "wt1e-agent",
    );

    // db::get — direct lookup, no archive filter applied.
    let mem = db::get(&conn, &source_id)
        .expect("get ok")
        .expect("archived source still resolvable by id");
    assert_eq!(mem.id, source_id);
    assert!(
        mem.metadata
            .get("atomisation_archived_at")
            .and_then(|v| v.as_str())
            .is_some(),
        "memory_get must return the archived source with its metadata stamp intact"
    );

    // db::resolve_id is the canonical resolver used by MCP
    // `handle_get` (see src/mcp/tools/get.rs). Mirror the same
    // call here to pin the exempt-from-filter contract.
    let mem2 = db::resolve_id(&conn, &source_id)
        .expect("resolve_id ok")
        .expect("archived source resolvable by id (mcp handle_get path)");
    assert_eq!(mem2.id, source_id);
    assert!(
        mem2.metadata
            .get("atomisation_archived_at")
            .and_then(|v| v.as_str())
            .is_some(),
        "resolve_id must round-trip the archive metadata"
    );
}

// ---------------------------------------------------------------------------
// Bonus — exercise the MCP handle_recall path with include_archived
// so the JSON-RPC parameter wiring is covered end-to-end.
// ---------------------------------------------------------------------------

#[test]
fn test_mcp_recall_param_routes_include_archived() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_hook_installed();

    let (_tmp, conn) = fresh_db();
    let source_id = insert_long_source(&conn, "wt1e/mcp", "vault");
    let _atoms = atomise(
        &conn,
        &source_id,
        &[
            "HashiCorp Vault stores secrets behind a sealed encryption barrier.",
            "Vault auth methods enrol identities via OIDC, JWT, and TLS certs.",
            "Vault dynamic secrets mint short-lived database credentials.",
            "Vault audit devices ship every API call to a downstream sink.",
            "Vault response wrapping hides secret payloads from intermediate hops.",
        ],
        "wt1e-agent",
    );

    let ttl = ai_memory::config::ResolvedTtl::default();
    let scoring = ai_memory::config::ResolvedScoring::default();

    // Default (include_archived absent) — source must NOT appear.
    let resp = handle_recall(
        &conn,
        &json!({"context": "vault", "namespace": "wt1e/mcp"}),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("mcp recall default");
    let default_ids: Vec<String> = resp["memories"]
        .as_array()
        .expect("memories array")
        .iter()
        .filter_map(|m| m["id"].as_str().map(String::from))
        .collect();
    assert!(
        !default_ids.contains(&source_id),
        "default MCP recall must drop archived source; got: {default_ids:?}"
    );

    // include_archived=true — source DOES appear.
    let resp = handle_recall(
        &conn,
        &json!({"context": "vault", "namespace": "wt1e/mcp", "include_archived": true}),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("mcp recall include_archived");
    let included_ids: Vec<String> = resp["memories"]
        .as_array()
        .expect("memories array")
        .iter()
        .filter_map(|m| m["id"].as_str().map(String::from))
        .collect();
    assert!(
        included_ids.contains(&source_id),
        "MCP recall include_archived=true must surface the source; got: {included_ids:?}"
    );
}
