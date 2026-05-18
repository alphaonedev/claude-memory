// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Cookbook harness for the v0.7.0 WT-1 atomisation primitive.
//!
//! Drives the [`Atomiser`] engine directly (no MCP, no daemon, no
//! Ollama) so the cookbook recipe at
//! `cookbook/atomisation/01-basic-flow.sh` is reproducible in under a
//! minute from a clean checkout on any host. The production hot-path
//! uses [`ai_memory::atomisation::curator::LlmCurator`] backed by
//! Gemma 4 over Ollama; here we inject a deterministic stub curator so
//! the recipe exercises the substrate semantics (`atom_of` FK,
//! `atomised_into` bump, `derives_from` edge, recall-time atom
//! preference, forensic chain envelope) without an LLM dependency.
//!
//! Audit-honesty note: the stub curator is plumbing-only. It returns
//! pre-baked atom strings derived deterministically from the source
//! body so the recipe demonstrates the substrate side faithfully. The
//! production curator's Gemma 4 prompt + `tiktoken-rs` token-budget
//! enforcement + audit-honest STOP discipline are exercised by the
//! `tests/atomisation/curator.rs` acceptance suite and the
//! `--ignored live_gemma_e2b_smoke` integration test.
//!
//! Flags:
//!   `--db <path>`      `SQLite` path; created if missing.
//!   `--report <path>`  JSON report (`atom_count`, `archived_at`,
//!                      recall and forensic outcomes).

use std::path::PathBuf;
use std::sync::Mutex;

use ai_memory::atomisation::curator::{Atom, Curator, CuratorError};
use ai_memory::atomisation::{Atomiser, AtomiserConfig};
use ai_memory::config::FeatureTier;
use ai_memory::forensic::bundle::{ExportForensicBundleArgs, build, build_files};
use ai_memory::models::{ConfidenceSource, Memory, MemoryKind, Tier};
use ai_memory::storage as db;
use anyhow::{Context, Result, anyhow};

/// Deterministic stub curator. Returns the canned atom set verbatim;
/// no LLM round-trip. Matches the surface
/// [`ai_memory::atomisation::curator::Curator`] consumes.
struct StubCurator {
    atoms: Mutex<Vec<Atom>>,
}

impl StubCurator {
    fn new(atom_texts: &[&str]) -> Self {
        Self {
            atoms: Mutex::new(
                atom_texts
                    .iter()
                    .map(|s| Atom {
                        text: (*s).to_string(),
                    })
                    .collect(),
            ),
        }
    }
}

impl Curator for StubCurator {
    fn decompose(
        &self,
        _body: &str,
        _max_atom_tokens: u32,
        _max_retries: u32,
    ) -> Result<Vec<Atom>, CuratorError> {
        let atoms = self.atoms.lock().expect("stub curator mutex");
        Ok(atoms.clone())
    }
}

struct Args {
    db: PathBuf,
    report: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut db = None;
    let mut report = None;
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or_else(|| anyhow!("flag {flag} needs a value"))?;
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(value)),
            "--report" => report = Some(PathBuf::from(value)),
            other => return Err(anyhow!("unknown flag {other}")),
        }
    }
    Ok(Args {
        db: db.ok_or_else(|| anyhow!("--db required"))?,
        report: report.ok_or_else(|| anyhow!("--report required"))?,
    })
}

/// Synthesise a long memory body so the source is well above the
/// 200-token default atom budget. Deterministic — same input every
/// run for reproducibility.
fn synth_long_body() -> String {
    let para = "The substrate-native atomisation primitive decomposes long memories \
        into atomic propositions. Each atom is written as a first-class memory \
        carrying an atom_of back-pointer to the source and a signed derives_from \
        edge. The parent memory is archived (archived_at stamped, atomised_into \
        bumped to the atom count) so recall surfaces the atoms in place of the \
        parent. The forensic bundle exporter walks the atom_of pointers to \
        include the full chain envelope in the audit tarball.";
    (0..8)
        .map(|i| format!("Paragraph {i}: {para}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Seed the long parent memory. Returns `(parent_id, body_len)`.
fn seed_parent(conn: &rusqlite::Connection, namespace: &str) -> Result<(String, usize)> {
    let now = chrono::Utc::now().to_rfc3339();
    let body = synth_long_body();
    let body_len = body.len();
    let parent = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: namespace.to_string(),
        title: "long-parent-source".to_string(),
        content: body,
        tags: vec!["atomisation".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "cookbook".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "ai:cookbook"}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: vec![],
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::default(),
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    };
    let id = db::insert(conn, &parent).context("insert long parent")?;
    Ok((id, body_len))
}

/// Recall outcome — captures atom-preference observation pair.
struct RecallOutcome {
    parent_skipped_default: bool,
    atoms_surface_count: usize,
    parent_visible_with_flag: bool,
}

fn observe_recall(
    conn: &rusqlite::Connection,
    namespace: &str,
    parent_id: &str,
    atom_ids: &[String],
) -> Result<RecallOutcome> {
    // Default recall must skip the archived parent and surface atoms
    // instead. We assert atoms appear, parent does not.
    let (rows, _budget) = db::recall(
        conn,
        "atomisation atoms decomposes",
        Some(namespace),
        20,
        None,
        None,
        None,
        3600,
        86_400,
        None,
        None,
        /* include_archived = */ false,
        /* source_uri_prefix = */ None,
    )
    .context("recall default")?;
    let recall_ids: Vec<&str> = rows.iter().map(|(m, _)| m.id.as_str()).collect();
    let parent_skipped_default = !recall_ids.contains(&parent_id);
    let atoms_surface_count = atom_ids
        .iter()
        .filter(|aid| recall_ids.contains(&aid.as_str()))
        .count();

    // include_archived=true must let the parent back in.
    let (rows_all, _) = db::recall(
        conn,
        "atomisation atoms decomposes",
        Some(namespace),
        20,
        None,
        None,
        None,
        3600,
        86_400,
        None,
        None,
        /* include_archived = */ true,
        /* source_uri_prefix = */ None,
    )
    .context("recall include_archived")?;
    let parent_visible_with_flag = rows_all.iter().any(|(m, _)| m.id == parent_id);
    Ok(RecallOutcome {
        parent_skipped_default,
        atoms_surface_count,
        parent_visible_with_flag,
    })
}

/// Inner shape — pure-bool envelope so [`ForensicOutcome`] itself
/// stays at three bools (max). Each field pins a distinct invariant
/// the cookbook asserts on; collapsing them would muddy the
/// per-invariant exit-code mapping.
struct EnvelopePresence {
    parent: bool,
    atoms: bool,
    signed_events: bool,
}

/// Forensic outcome — captures bundle-shape observations.
struct ForensicOutcome {
    bundle_path: PathBuf,
    files_count: usize,
    envelopes: EnvelopePresence,
    chain_envelope_included: bool,
}

fn observe_forensic(
    conn: &rusqlite::Connection,
    report_path: &std::path::Path,
    parent_id: &str,
    atom_ids: &[String],
) -> Result<ForensicOutcome> {
    let bundle_dir = report_path
        .parent()
        .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf);
    let bundle_path = bundle_dir.join("forensic-bundle-atomisation.tar");
    let bundle_args = ExportForensicBundleArgs {
        memory_id: parent_id.to_string(),
        include_reflections: false,
        include_transcripts: false,
        output: Some(bundle_path.clone()),
        include_atomisation_chain: true,
    };
    build(conn, &bundle_args, &bundle_path, None).context("build forensic bundle (tar)")?;
    // Inspect bundle contents in-memory so the recipe asserts on what
    // lives inside without re-untarring. The map is path-keyed.
    let bundle_files =
        build_files(conn, &bundle_args, None).context("build forensic bundle map")?;
    let bundle_paths: Vec<&str> = bundle_files.keys().map(String::as_str).collect();
    let envelopes = EnvelopePresence {
        parent: bundle_paths
            .iter()
            .any(|p| p.contains(&format!("memories/{parent_id}"))),
        atoms: atom_ids
            .iter()
            .all(|aid| bundle_paths.iter().any(|p| p.contains(aid))),
        signed_events: bundle_paths.iter().any(|p| p.contains("signed_events")),
    };
    let chain_envelope_included = envelopes.parent && envelopes.atoms && envelopes.signed_events;
    Ok(ForensicOutcome {
        bundle_path,
        files_count: bundle_files.len(),
        envelopes,
        chain_envelope_included,
    })
}

fn main() -> Result<()> {
    let args = parse_args()?;

    // ─── 1. open db + seed long memory ─────────────────────────────────
    let conn = db::open(&args.db).context("open db")?;
    let namespace = "cookbook/atomisation";
    let (parent_id, body_len) = seed_parent(&conn, namespace)?;

    // ─── 2. atomise via the substrate engine ───────────────────────────
    let curator = Box::new(StubCurator::new(&[
        "Atomic proposition 1: atomisation decomposes long memories into atoms.",
        "Atomic proposition 2: each atom carries atom_of and derives_from.",
        "Atomic proposition 3: the parent is archived after atomisation.",
        "Atomic proposition 4: recall surfaces atoms in place of archived parents.",
        "Atomic proposition 5: the forensic bundle includes the chain envelope.",
    ]));
    let atomiser = Atomiser::new(curator, None, AtomiserConfig::default(), FeatureTier::Smart);
    let result = atomiser
        .atomise_sync(&conn, &parent_id, 0, false, "ai:cookbook")
        .map_err(|e| anyhow!("atomise: {e}"))?;

    // ─── 3. verify substrate-side invariants ───────────────────────────
    let parent_after: Memory = db::get(&conn, &parent_id)
        .context("re-fetch parent")?
        .ok_or_else(|| anyhow!("parent missing after atomise"))?;
    let atomised_into_parent: i64 = conn
        .query_row(
            "SELECT atomised_into FROM memories WHERE id = ?1",
            rusqlite::params![&parent_id],
            |row| row.get::<_, i64>(0),
        )
        .context("read atomised_into")?;
    let archived_at_set = parent_after
        .metadata
        .get("atomisation_archived_at")
        .is_some();
    let atom_count_via_atom_of: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE atom_of = ?1",
            rusqlite::params![&parent_id],
            |row| row.get::<_, i64>(0),
        )
        .context("count atom_of rows")?;

    // ─── 4. verify recall-time atom preference ─────────────────────────
    let recall = observe_recall(&conn, namespace, &parent_id, &result.atom_ids)?;

    // ─── 5. forensic bundle chain envelope ─────────────────────────────
    let forensic = observe_forensic(&conn, &args.report, &parent_id, &result.atom_ids)?;

    // ─── 6. emit structured JSON report ────────────────────────────────
    let report = serde_json::json!({
        "parent_id": parent_id,
        "parent_body_bytes": body_len,
        "atom_count": result.atom_count,
        "atom_ids": result.atom_ids,
        "atomised_into": atomised_into_parent,
        "archived_at": result.archived_at,
        "archived_at_metadata_set": archived_at_set,
        "atom_of_row_count": atom_count_via_atom_of,
        "recall": {
            "parent_skipped_by_default": recall.parent_skipped_default,
            "atoms_surface_count": recall.atoms_surface_count,
            "parent_visible_with_include_archived": recall.parent_visible_with_flag,
        },
        "forensic": {
            "bundle_path": forensic.bundle_path,
            "files_written_count": forensic.files_count,
            "parent_envelope_present": forensic.envelopes.parent,
            "atom_envelopes_present": forensic.envelopes.atoms,
            "signed_events_present": forensic.envelopes.signed_events,
            "chain_envelope_included": forensic.chain_envelope_included,
        }
    });
    std::fs::write(&args.report, serde_json::to_string_pretty(&report)?).context("write report")?;

    // Acceptance: bail with a distinct exit code per failing invariant.
    if atomised_into_parent <= 0 {
        std::process::exit(2);
    }
    if atom_count_via_atom_of <= 0 {
        std::process::exit(3);
    }
    if !recall.parent_skipped_default {
        std::process::exit(4);
    }
    if recall.atoms_surface_count == 0 {
        std::process::exit(5);
    }
    if !recall.parent_visible_with_flag {
        std::process::exit(6);
    }
    if !forensic.chain_envelope_included {
        std::process::exit(7);
    }
    Ok(())
}
