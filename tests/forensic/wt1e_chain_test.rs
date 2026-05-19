// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::missing_panics_doc
)]

//! v0.7.0 WT-1-E — forensic-bundle atomisation-chain acceptance.
//!
//! Three acceptance tests pinned to the WT-1-E brief:
//!
//! 5. Bundle contains source + atoms + DerivesFrom edges +
//!    atomisation_complete signed_event when the chain is included.
//! 6. `--include-atomisation-chain=false` skips the chain (atoms
//!    only — no source row, no derives_from edges, no
//!    atomisation_complete event).
//! 7. Round-trip: build → read_ustar → re-import → atoms still link
//!    back to sources via DerivesFrom (chain reconstructible from
//!    the bundle alone).

use ai_memory::models::ConfidenceSource;
use std::sync::{Mutex, OnceLock};

use ai_memory::atomisation::curator::{Atom, Curator, CuratorError};
use ai_memory::atomisation::{Atomiser, AtomiserConfig};
use ai_memory::config::FeatureTier;
use ai_memory::db;
use ai_memory::forensic::bundle::{self, ExportForensicBundleArgs, read_ustar};
use ai_memory::models::{Memory, MemoryKind, Tier};
use ai_memory::storage;

use rusqlite::Connection;
use serde_json::json;
use tempfile::{NamedTempFile, TempDir};

// ---------------------------------------------------------------------------
// Shared scaffolding (mirrors tests/atomisation/core.rs).
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

fn test_serial() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

fn ensure_hook_installed() {
    let _ = storage::GOVERNANCE_PRE_WRITE.set(Box::new(|_: &Memory| Ok(())));
}

fn make_atoms(texts: &[&str]) -> Vec<Atom> {
    texts
        .iter()
        .map(|s| Atom {
            text: (*s).to_string(),
        })
        .collect()
}

fn fresh_db_in(dir: &std::path::Path) -> (std::path::PathBuf, Connection) {
    let p = dir.join("ai-memory.db");
    let conn = db::open(&p).expect("db::open");
    (p, conn)
}

fn fresh_db_tempfile() -> (NamedTempFile, Connection) {
    let tmp = NamedTempFile::new().expect("tempfile");
    let conn = db::open(tmp.path()).expect("db::open");
    (tmp, conn)
}

fn seed_long_source(conn: &Connection, ns: &str, keyword: &str) -> String {
    let now = chrono::Utc::now().to_rfc3339();
    let body = (0..20)
        .map(|i| {
            format!(
                "Paragraph {i}: detailed information about {keyword} systems and \
                 their operational characteristics. The {keyword} subsystem coordinates \
                 multiple components and reports health to upstream observers. \
                 Failures cascade only when the supervisor is unreachable."
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: ns.to_string(),
        title: format!("{keyword}-source-{}", uuid::Uuid::new_v4().simple()),
        content: body,
        tags: vec![keyword.to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({"agent_id": "wt1e-fb-agent"}),
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
    db::insert(conn, &mem).expect("seed")
}

fn atomise(conn: &Connection, source_id: &str, atom_texts: &[&str]) -> Vec<String> {
    let curator = Box::new(MockCurator::new(vec![Ok(make_atoms(atom_texts))]));
    let atomiser = Atomiser::new(curator, None, AtomiserConfig::default(), FeatureTier::Smart);
    let outcome = atomiser
        .atomise_sync(conn, source_id, 200, false, "wt1e-fb-agent")
        .expect("atomise");
    outcome.atom_ids
}

// ---------------------------------------------------------------------------
// Test 5 — forensic export includes the atomisation chain.
// ---------------------------------------------------------------------------

#[test]
fn test_forensic_export_includes_atomisation_chain() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_hook_installed();

    let tmp = TempDir::new().unwrap();
    let (db_path, conn) = fresh_db_in(tmp.path());
    let source_id = seed_long_source(&conn, "fb/wt1e/c5", "kafka");
    let atom_ids = atomise(
        &conn,
        &source_id,
        &[
            "Kafka brokers store partitioned commit logs on disk.",
            "Kafka consumer groups coordinate offsets via the coordinator.",
            "Kafka replication factor controls broker-failure tolerance.",
            "Kafka transactions enable exactly-once message delivery.",
            "Kafka tiered storage offloads cold segments to object stores.",
        ],
    );
    assert!(!atom_ids.is_empty(), "atomisation produced atoms");

    // Build the bundle in-memory so we can inspect it deterministically.
    let args = ExportForensicBundleArgs {
        memory_id: source_id.clone(),
        include_reflections: false,
        include_transcripts: false,
        include_atomisation_chain: true,
        output: None,
    };
    let files =
        bundle::build_files(&conn, &args, Some("2026-05-15T00:00:00Z")).expect("build_files");
    drop(conn);
    let _ = db_path; // keep path alive for tmp dir lifetime

    // (a) Source memory envelope present with atomisation block.
    let src_key = format!("memories/{source_id}.json");
    let src_bytes = files.get(&src_key).expect("source memory bundled");
    let src_v: serde_json::Value = serde_json::from_slice(src_bytes).expect("parse src");
    assert!(
        src_v["atomisation"].is_object(),
        "archived source must carry atomisation enrichment"
    );
    assert_eq!(
        src_v["atomisation"]["atomised_into"].as_i64(),
        Some(i64::try_from(atom_ids.len()).unwrap()),
    );
    assert!(
        src_v["atomisation"]["archived_at"].is_string(),
        "atomisation.archived_at must be present"
    );
    let bundled_atom_ids: Vec<String> = src_v["atomisation"]["atom_ids"]
        .as_array()
        .expect("atom_ids array")
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    assert_eq!(bundled_atom_ids.len(), atom_ids.len());
    for aid in &atom_ids {
        assert!(bundled_atom_ids.contains(aid), "atom {aid} listed");
    }

    // (b) Each atom envelope present and carries atom_of pointer.
    for aid in &atom_ids {
        let key = format!("memories/{aid}.json");
        let bytes = files
            .get(&key)
            .unwrap_or_else(|| panic!("atom {aid} must be bundled"));
        let v: serde_json::Value = serde_json::from_slice(bytes).expect("parse atom");
        assert_eq!(
            v["atomisation"]["atom_of"].as_str(),
            Some(source_id.as_str()),
            "atom envelope must point back via atom_of"
        );
    }

    // (c) Every atom → source `derives_from` edge present.
    let mut edge_count = 0;
    for aid in &atom_ids {
        let edge_key = format!("edges/{aid}__derives_from__{source_id}.json");
        assert!(
            files.contains_key(&edge_key),
            "derives_from edge for atom {aid} → {source_id} must be bundled (key {edge_key})"
        );
        edge_count += 1;
    }
    assert_eq!(edge_count, atom_ids.len());

    // (d) Exactly one atomisation_complete signed_event bundled.
    let mut atomisation_complete_seen = 0;
    let mut link_created_seen = 0;
    for (path, body) in &files {
        if !path.starts_with("signed_events/") {
            continue;
        }
        let v: serde_json::Value = serde_json::from_slice(body).expect("parse event");
        match v["event_type"].as_str() {
            Some("atomisation_complete") => atomisation_complete_seen += 1,
            Some("memory_link.created") => link_created_seen += 1,
            _ => {}
        }
    }
    assert_eq!(
        atomisation_complete_seen, 1,
        "exactly one atomisation_complete event must be in the bundle"
    );
    assert!(
        link_created_seen >= atom_ids.len(),
        "expected at least N memory_link.created events for N atoms; got {link_created_seen}"
    );
}

// ---------------------------------------------------------------------------
// Test 6 — --include-atomisation-chain=false skips the chain.
// ---------------------------------------------------------------------------

#[test]
fn test_forensic_export_chain_disable_flag() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_hook_installed();

    let tmp = TempDir::new().unwrap();
    let (_db_path, conn) = fresh_db_in(tmp.path());
    let source_id = seed_long_source(&conn, "fb/wt1e/c6", "rabbitmq");
    let atom_ids = atomise(
        &conn,
        &source_id,
        &[
            "RabbitMQ uses AMQP 0-9-1 as its primary wire protocol.",
            "RabbitMQ exchanges route messages by binding key matchers.",
            "RabbitMQ mirrored queues provide high availability across nodes.",
            "RabbitMQ shovel and federation move messages between brokers.",
            "RabbitMQ delayed message exchange plugin queues future deliveries.",
        ],
    );

    // Build the bundle for an ATOM with the chain disabled. Expected
    // behaviour: only the atom envelope is emitted; the source row,
    // sibling atoms, derives_from edges, and atomisation_complete
    // event are NOT bundled.
    let atom_id = atom_ids[0].clone();
    let args = ExportForensicBundleArgs {
        memory_id: atom_id.clone(),
        include_reflections: false,
        include_transcripts: false,
        include_atomisation_chain: false,
        output: None,
    };
    let files =
        bundle::build_files(&conn, &args, Some("2026-05-15T00:00:00Z")).expect("build_files");

    // (a) Atom is present.
    assert!(files.contains_key(&format!("memories/{atom_id}.json")));

    // (b) Source is NOT present.
    assert!(
        !files.contains_key(&format!("memories/{source_id}.json")),
        "source must be skipped when chain disabled"
    );

    // (c) Other atoms NOT present (chain skipped).
    for sibling in atom_ids.iter().skip(1) {
        assert!(
            !files.contains_key(&format!("memories/{sibling}.json")),
            "sibling atom {sibling} must be skipped when chain disabled"
        );
    }

    // (d) No derives_from edges.
    for path in files.keys() {
        assert!(
            !path.contains("__derives_from__"),
            "derives_from edge present unexpectedly: {path}"
        );
    }

    // (e) No atomisation_complete event bundled (matched by event_type).
    for (path, body) in &files {
        if !path.starts_with("signed_events/") {
            continue;
        }
        let v: serde_json::Value = serde_json::from_slice(body).expect("parse event");
        assert_ne!(
            v["event_type"].as_str(),
            Some("atomisation_complete"),
            "atomisation_complete event present when chain disabled: {path}"
        );
    }

    // (f) Atom envelope does NOT carry the atomisation enrichment
    // block (the per-envelope enrichment is gated on the same flag).
    let atom_bytes = files.get(&format!("memories/{atom_id}.json")).unwrap();
    let atom_v: serde_json::Value = serde_json::from_slice(atom_bytes).unwrap();
    assert!(
        atom_v["atomisation"].is_null() || atom_v.get("atomisation").is_none(),
        "atomisation block must be skipped when chain disabled"
    );
}

// ---------------------------------------------------------------------------
// Test 7 — round-trip: export → fresh DB import → chain reconstructable.
// ---------------------------------------------------------------------------

#[test]
fn test_forensic_export_can_reconstruct_chain() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ensure_hook_installed();

    let tmp = TempDir::new().unwrap();
    let bundle_path = tmp.path().join("bundle.tar");

    // 1) Build the source DB and atomise.
    let source_id;
    let atom_ids;
    {
        let (db_path, conn) = fresh_db_in(tmp.path());
        source_id = seed_long_source(&conn, "fb/wt1e/c7", "consul");
        atom_ids = atomise(
            &conn,
            &source_id,
            &[
                "Consul gossip mesh propagates membership across LAN segments.",
                "Consul KV stores configuration with last-write-wins semantics.",
                "Consul service catalogue answers DNS and HTTP queries.",
                "Consul ACL tokens scope access to namespaces and policies.",
                "Consul connect issues short-lived TLS certificates per service.",
            ],
        );

        // Export to disk so the round-trip walks the tar parser too.
        let args = ExportForensicBundleArgs {
            memory_id: source_id.clone(),
            include_reflections: false,
            include_transcripts: false,
            include_atomisation_chain: true,
            output: Some(bundle_path.clone()),
        };
        bundle::build(&conn, &args, &bundle_path, Some("2026-05-15T00:00:00Z"))
            .expect("build bundle");
        drop(conn);
        let _ = db_path;
    }

    // 2) Re-read the bundle.
    let bytes = std::fs::read(&bundle_path).expect("read bundle");
    let files = read_ustar(&bytes).expect("parse ustar");

    // 3) Reconstruct the chain in a fresh DB. Each memory envelope
    //    becomes a `memories` row; each edge becomes a
    //    `memory_links` row. The fresh DB has no prior knowledge —
    //    everything the auditor needs lives inside the bundle.
    //    `agent_id_idx` and `scope_idx` are GENERATED columns
    //    (v33 migration), so the INSERT omits them. The
    //    atomisation chain columns (`atomised_into`, `atom_of`)
    //    are real and need to be restored verbatim.
    //
    //    Order matters: insert non-atom rows (atom_of IS NULL)
    //    first, then atoms — `memories.atom_of` is a FK back to
    //    `memories(id)` so atoms can only land after their parent
    //    exists.
    let (_fresh_tmp, fresh_conn) = fresh_db_tempfile();
    let mut memory_envs: Vec<serde_json::Value> = files
        .iter()
        .filter(|(p, _)| {
            p.starts_with("memories/")
                && std::path::Path::new(p)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        })
        .map(|(_, b)| serde_json::from_slice::<serde_json::Value>(b).expect("parse"))
        .collect();
    // Parents (no atom_of) first; atoms second.
    memory_envs.sort_by_key(|v| i32::from(v["atomisation"]["atom_of"].is_string()));
    for v in &memory_envs {
        let id = v["id"].as_str().unwrap().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        fresh_conn
            .execute(
                "INSERT INTO memories (id, tier, namespace, title, content, tags, priority, \
                                      confidence, source, access_count, created_at, updated_at, \
                                      last_accessed_at, expires_at, metadata, reflection_depth, \
                                      embedding, memory_kind, \
                                      atomised_into, atom_of) \
                 VALUES (?1, ?2, ?3, ?4, ?5, '[]', 5, 1.0, 'restored', 0, ?6, ?7, NULL, NULL, \
                         ?8, 0, NULL, 'observation', ?9, ?10)",
                rusqlite::params![
                    id,
                    v["tier"].as_str().unwrap_or("mid"),
                    v["namespace"].as_str().unwrap_or("fb/wt1e/c7"),
                    v["title"].as_str().unwrap_or("restored"),
                    v["content"].as_str().unwrap_or(""),
                    now,
                    now,
                    v["metadata"].to_string(),
                    v["atomisation"]["atomised_into"].as_i64(),
                    v["atomisation"]["atom_of"].as_str(),
                ],
            )
            .expect("insert restored memory");
    }
    for (path, body) in &files {
        if !path.starts_with("edges/")
            || !std::path::Path::new(path)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            continue;
        }
        let v: serde_json::Value = serde_json::from_slice(body).expect("parse edge env");
        fresh_conn
            .execute(
                "INSERT OR IGNORE INTO memory_links \
                 (source_id, target_id, relation, created_at, attest_level) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    v["source_id"].as_str().unwrap(),
                    v["target_id"].as_str().unwrap(),
                    v["relation"].as_str().unwrap(),
                    v["created_at"].as_str().unwrap(),
                    v["attest_level"].as_str().unwrap_or("unsigned"),
                ],
            )
            .expect("insert restored edge");
    }

    // 4) Assertions: every atom has a derives_from edge back to the
    //    source in the fresh DB.
    for aid in &atom_ids {
        let row: i64 = fresh_conn
            .query_row(
                "SELECT COUNT(*) FROM memory_links \
                 WHERE source_id = ?1 AND target_id = ?2 AND relation = 'derives_from'",
                rusqlite::params![aid, source_id],
                |r| r.get(0),
            )
            .expect("query");
        assert_eq!(
            row, 1,
            "round-trip: atom {aid} must have a derives_from edge to source {source_id}"
        );
    }

    // The source memory's atomised_into and atom_of columns
    // round-trip too.
    let (atomised_into, atom_of): (Option<i64>, Option<String>) = fresh_conn
        .query_row(
            "SELECT atomised_into, atom_of FROM memories WHERE id = ?1",
            rusqlite::params![source_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("query source");
    assert_eq!(atomised_into, Some(i64::try_from(atom_ids.len()).unwrap()));
    assert!(atom_of.is_none(), "source row is not itself an atom");

    // And each atom's atom_of points back at the source.
    for aid in &atom_ids {
        let parent: Option<String> = fresh_conn
            .query_row(
                "SELECT atom_of FROM memories WHERE id = ?1",
                rusqlite::params![aid],
                |r| r.get(0),
            )
            .expect("query atom");
        assert_eq!(parent.as_deref(), Some(source_id.as_str()));
    }
}
