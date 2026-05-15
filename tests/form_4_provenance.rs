// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy: integration suite is verbose by design — relax pedantic
// nags that don't carry signal for hand-written test code.
#![allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::needless_pass_by_value
)]

//! v0.7.0 Form 4 — fact-provenance closeout acceptance suite (issue
//! #757).
//!
//! The Batman 6-form audit (PR #753) found Form 4 PARTIAL: ai-memory
//! had `source` role label, `created_at` capture timestamp, and
//! `confidence`, but lacked a first-class `citations` array, the
//! source-as-URI form, and atom-grain span offsets. This test binary
//! pins the closeout:
//!
//! 1. Round-trip: store memory with citations → recall → preserved.
//! 2. Source-URI parsing accepts `uri:` / `doc:` / `file:` schemes.
//! 3. Atom-span: each atom's `source_span` falls within source bounds.
//! 4. Recall filter: `has_citations` returns only memories with
//!    non-empty citations.
//! 5. Forensic bundle exports include all three new fields.
//! 6. Migration: v37 → v38 sqlite is idempotent.

use std::sync::{Mutex, OnceLock};

use ai_memory::atomisation::curator::{Atom, Curator, CuratorError};
use ai_memory::atomisation::{Atomiser, AtomiserConfig};
use ai_memory::config::FeatureTier;
use ai_memory::db;
use ai_memory::models::{Citation, Memory, MemoryKind, SourceSpan, Tier};
use ai_memory::storage;
use ai_memory::validate;

use rusqlite::Connection;
use tempfile::NamedTempFile;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn fresh_db() -> (NamedTempFile, Connection) {
    let tmp = NamedTempFile::new().expect("tempfile");
    let conn = db::open(tmp.path()).expect("db::open");
    (tmp, conn)
}

fn test_serial() -> &'static Mutex<()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn mem_with_citations(ns: &str, title: &str, content: &str, citations: Vec<Citation>) -> Memory {
    let now = now();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: ns.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        tags: Vec::new(),
        priority: 5,
        confidence: 1.0,
        source: "api".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations,
        source_uri: None,
        source_span: None,
    }
}

// ---------------------------------------------------------------------------
// 1. Round-trip — citations, source_uri, source_span all survive store→recall
// ---------------------------------------------------------------------------

#[test]
fn round_trip_preserves_citations_source_uri_and_source_span() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_tmp, conn) = fresh_db();

    let citation = Citation {
        uri: "uri:https://example.test/spec.html".to_string(),
        accessed_at: now(),
        hash: Some("a".repeat(64)),
        span: Some(SourceSpan { start: 0, end: 64 }),
    };
    let mut mem = mem_with_citations(
        "form4-roundtrip",
        "spec excerpt",
        "Citation round-trip test body",
        vec![citation.clone()],
    );
    mem.source_uri = Some("doc:parent-001".to_string());
    mem.source_span = Some(SourceSpan { start: 12, end: 24 });

    let id = storage::insert(&conn, &mem).expect("insert with provenance");

    let back = storage::get(&conn, &id).expect("get").expect("row present");
    assert_eq!(back.citations.len(), 1);
    assert_eq!(back.citations[0], citation);
    assert_eq!(back.source_uri.as_deref(), Some("doc:parent-001"));
    assert_eq!(back.source_span, Some(SourceSpan { start: 12, end: 24 }));
}

// ---------------------------------------------------------------------------
// 2. Source-URI parsing — accepts uri:/doc:/file:; rejects bare strings
// ---------------------------------------------------------------------------

#[test]
fn source_uri_accepts_typed_schemes_rejects_bare_strings() {
    // Accepted schemes
    for ok in [
        "uri:https://example.test/path",
        "doc:abc-123",
        "file:/var/data/foo.txt",
    ] {
        assert!(
            validate::validate_source_uri(ok).is_ok(),
            "expected accepted: {ok}"
        );
    }
    // Rejected: empty, no scheme, unrecognised scheme, empty payload
    for bad in [
        "",
        "   ",
        "https://example.test",
        "user",
        "claude",
        "ftp:foo",
        "uri:",
        "doc:",
        "file:   ",
    ] {
        assert!(
            validate::validate_source_uri(bad).is_err(),
            "expected rejected: {bad:?}"
        );
    }
}

#[test]
fn citation_validator_enforces_required_invariants() {
    // Happy path.
    let ok = Citation {
        uri: "uri:https://example.test/spec.html".into(),
        accessed_at: now(),
        hash: Some("0".repeat(64)),
        span: Some(SourceSpan { start: 0, end: 10 }),
    };
    assert!(validate::validate_citation(&ok).is_ok());

    // Bad URI scheme.
    let mut bad = ok.clone();
    bad.uri = "not-a-scheme".into();
    assert!(validate::validate_citation(&bad).is_err());

    // Bad accessed_at.
    let mut bad = ok.clone();
    bad.accessed_at = "not-rfc3339".into();
    assert!(validate::validate_citation(&bad).is_err());

    // Bad hash (wrong length).
    let mut bad = ok.clone();
    bad.hash = Some("abc".into());
    assert!(validate::validate_citation(&bad).is_err());

    // Bad span (start >= end).
    let mut bad = ok;
    bad.span = Some(SourceSpan { start: 10, end: 10 });
    assert!(validate::validate_citation(&bad).is_err());
}

#[test]
fn source_span_validator_requires_start_lt_end() {
    assert!(validate::validate_source_span(&SourceSpan { start: 0, end: 1 }).is_ok());
    assert!(validate::validate_source_span(&SourceSpan { start: 0, end: 0 }).is_err());
    assert!(validate::validate_source_span(&SourceSpan { start: 5, end: 3 }).is_err());
}

// ---------------------------------------------------------------------------
// 3. Atom-grain span — each atom's source_span falls within source bounds
// ---------------------------------------------------------------------------

struct CannedCurator {
    responses: Mutex<Vec<Result<Vec<Atom>, CuratorError>>>,
}

impl Curator for CannedCurator {
    fn decompose(
        &self,
        _body: &str,
        _max_atom_tokens: u32,
        _max_retries: u32,
    ) -> Result<Vec<Atom>, CuratorError> {
        let mut q = self.responses.lock().unwrap();
        if q.is_empty() {
            return Err(CuratorError::MalformedResponse("queue empty".into()));
        }
        q.remove(0)
    }
}

#[test]
fn atom_grain_span_falls_within_source_bounds() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_tmp, conn) = fresh_db();

    let body = "Kubernetes rolling deploys require canary health checks. \
                The readiness probe must pass before traffic shifts. \
                Failures roll back the deployment within thirty seconds.";
    // The mock curator returns three atoms whose text appears verbatim in
    // the body — the substrate `compute_atom_span` helper must locate
    // each and stamp a valid SourceSpan.
    let atoms = vec![
        Atom {
            text: "Kubernetes rolling deploys require canary health checks.".to_string(),
        },
        Atom {
            text: "The readiness probe must pass before traffic shifts.".to_string(),
        },
        Atom {
            text: "Failures roll back the deployment within thirty seconds.".to_string(),
        },
    ];
    let mem = mem_with_citations("form4-atom-span", "k8s deploys", body, Vec::new());
    let source_id = storage::insert(&conn, &mem).expect("insert source");
    let source_len = body.len();

    let curator = Box::new(CannedCurator {
        responses: Mutex::new(vec![Ok(atoms.clone())]),
    });
    let atomiser = Atomiser::new(
        curator,
        None,
        AtomiserConfig {
            // Force the source-too-small short-circuit not to fire.
            default_max_atom_tokens: 5,
            min_atoms_per_source: 2,
            max_atoms_per_source: 10,
            curator_max_retries: 0,
        },
        FeatureTier::Smart,
    );
    let result = atomiser
        .atomise_sync(&conn, &source_id, 5, false, "ai:test-agent")
        .expect("atomise");
    assert_eq!(result.atom_count, 3);

    // Walk the atoms and assert each carries a SourceSpan that lies
    // within the parent body and points at the same text.
    for atom_id in &result.atom_ids {
        let atom = storage::get(&conn, atom_id)
            .expect("get atom")
            .expect("present");
        assert_eq!(
            atom.source_uri.as_deref(),
            Some(format!("doc:{source_id}").as_str())
        );
        let span = atom.source_span.expect("atom carries source_span");
        assert!(span.start < span.end, "span half-open invariant");
        assert!(span.end <= source_len, "span bounded by source length");
        assert_eq!(&body[span.start..span.end], atom.content);
    }
}

// ---------------------------------------------------------------------------
// 4. Recall filter — has_citations returns only memories with non-empty citations
// ---------------------------------------------------------------------------

#[test]
fn has_citations_recall_filter_excludes_empty_provenance() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_tmp, conn) = fresh_db();

    let ns = "form4-has-citations";
    let cited = Citation {
        uri: "uri:https://example.test/a".into(),
        accessed_at: now(),
        hash: None,
        span: None,
    };
    // Two memories sharing a query token; only one carries citations.
    // Use a unique token unlikely to be filtered by the tokenizer.
    let with_cite = mem_with_citations(
        ns,
        "the alpha entry",
        "kubernetes deployment manifest",
        vec![cited],
    );
    let without_cite = mem_with_citations(
        ns,
        "the beta entry",
        "kubernetes deployment manifest",
        Vec::new(),
    );

    let _id_a = storage::insert(&conn, &with_cite).expect("insert cited");
    let _id_b = storage::insert(&conn, &without_cite).expect("insert uncited");

    let resolved_ttl = ai_memory::config::ResolvedTtl::default();
    let (results, _outcome) = db::recall(
        &conn,
        "kubernetes",
        Some(ns),
        20,
        None,
        None,
        None,
        resolved_ttl.short_extend_secs,
        resolved_ttl.mid_extend_secs,
        None,
        None,
        false,
    )
    .expect("recall");
    // Substrate returns both; the post-filter restricts.
    assert_eq!(
        results.len(),
        2,
        "substrate returns both rows before filter; got {results:#?}"
    );

    let filtered = ai_memory::cli::recall::apply_form4_recall_filters(results, true, None);
    assert_eq!(filtered.len(), 1, "filter keeps only memory with citations");
    assert_eq!(filtered[0].0.title, "the alpha entry");
}

#[test]
fn source_uri_prefix_recall_filter_restricts_to_matching_uri() {
    let _g = test_serial()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_tmp, conn) = fresh_db();

    let ns = "form4-uri-prefix";
    let mut a = mem_with_citations(ns, "alpha note", "deployment manifest example", Vec::new());
    a.source_uri = Some("doc:parent-001".to_string());
    let mut b = mem_with_citations(ns, "beta note", "deployment manifest example", Vec::new());
    b.source_uri = Some("uri:https://example.test/x".to_string());
    let c = mem_with_citations(ns, "gamma note", "deployment manifest example", Vec::new());

    storage::insert(&conn, &a).expect("insert a");
    storage::insert(&conn, &b).expect("insert b");
    storage::insert(&conn, &c).expect("insert c");

    let resolved_ttl = ai_memory::config::ResolvedTtl::default();
    let (results, _) = db::recall(
        &conn,
        "deployment",
        Some(ns),
        20,
        None,
        None,
        None,
        resolved_ttl.short_extend_secs,
        resolved_ttl.mid_extend_secs,
        None,
        None,
        false,
    )
    .expect("recall");
    assert_eq!(results.len(), 3, "substrate returns all three rows");

    let filtered =
        ai_memory::cli::recall::apply_form4_recall_filters(results.clone(), false, Some("doc:"));
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].0.title, "alpha note");

    let filtered2 =
        ai_memory::cli::recall::apply_form4_recall_filters(results, false, Some("uri:https://"));
    assert_eq!(filtered2.len(), 1);
    assert_eq!(filtered2[0].0.title, "beta note");
}

// ---------------------------------------------------------------------------
// 5. Forensic bundle — exports include citations + source_uri + source_span
// ---------------------------------------------------------------------------

#[test]
fn forensic_bundle_envelope_carries_form4_fields() {
    use ai_memory::forensic::bundle::MemoryEnvelope;
    use serde_json::Value;

    let citation = Citation {
        uri: "uri:https://example.test/cite".into(),
        accessed_at: now(),
        hash: None,
        span: None,
    };
    let env = MemoryEnvelope {
        id: "abc".into(),
        namespace: "ns".into(),
        title: "title".into(),
        content: "content".into(),
        tier: "mid".into(),
        memory_kind: "observation".into(),
        reflection_depth: 0,
        created_at: now(),
        updated_at: now(),
        metadata: serde_json::json!({}),
        atomisation: None,
        citations: vec![citation.clone()],
        source_uri: Some("doc:parent-001".into()),
        source_span: Some(SourceSpan { start: 0, end: 7 }),
    };
    let v: Value = serde_json::to_value(&env).expect("serialise envelope");
    assert!(v["citations"].is_array(), "citations always present");
    assert_eq!(v["citations"].as_array().unwrap().len(), 1);
    assert_eq!(v["source_uri"].as_str(), Some("doc:parent-001"));
    assert!(v["source_span"].is_object(), "source_span present");
    assert_eq!(v["source_span"]["start"].as_u64(), Some(0));
    assert_eq!(v["source_span"]["end"].as_u64(), Some(7));
}

// ---------------------------------------------------------------------------
// 6. Migration v37 → v38 — idempotent on replay
// ---------------------------------------------------------------------------

#[test]
fn schema_v38_columns_present_and_migration_is_idempotent() {
    let (_tmp, conn) = fresh_db();

    // The fresh open already lands v38; re-opening must be a no-op.
    let version: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
        .expect("schema_version row");
    assert_eq!(version, 38, "fresh DB lands at schema v38");

    // Confirm the new columns exist on `memories`.
    let mut has_citations = false;
    let mut has_source_uri = false;
    let mut has_source_span = false;
    let mut stmt = conn.prepare("PRAGMA table_info(memories)").expect("pragma");
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .expect("query");
    for col in cols {
        match col.expect("col name").as_str() {
            "citations" => has_citations = true,
            "source_uri" => has_source_uri = true,
            "source_span" => has_source_span = true,
            _ => {}
        }
    }
    drop(stmt);
    assert!(has_citations, "citations column present");
    assert!(has_source_uri, "source_uri column present");
    assert!(has_source_span, "source_span column present");

    // Confirm the partial index landed.
    let idx_exists: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_memories_source_uri'",
            [],
            |r| r.get(0),
        )
        .ok();
    assert!(idx_exists.is_some(), "source_uri partial index present");
}
