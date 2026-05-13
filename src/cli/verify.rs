// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `ai-memory verify-reflection-chain` — external verifier for
//! reflection chains (procurement-grade audit tool, v0.7.0 L1-3).
//!
//! Walks the `reflects_on` edges backward from the supplied memory to
//! depth 0, verifies each Ed25519 signature (when present) using the
//! `identity::verify` infrastructure, optionally checks `signed_events`
//! creation entries, and emits a structured chain-integrity report.
//!
//! ## Exit codes
//!
//! - `0` — chain fully verified (or no signatures present and
//!   `bounded_status != "exceeded_cap"`).
//! - `1` — at least one edge failed signature verification, or the
//!   chain exceeds its namespace `max_reflection_depth` cap.
//!
//! ## Output formats
//!
//! - `--format text` (default) — human-readable report printed to
//!   stdout.
//! - `--format json` — structured `AgenticMem Attest` tier evidence
//!   packet serialised as JSON.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::cli::CliOutput;
use crate::identity::sign::SignableLink;

// ─────────────────────────────────────────────────────────────────────
// CLI argument struct (consumed by daemon_runtime)
// ─────────────────────────────────────────────────────────────────────

/// Arguments for `ai-memory verify-reflection-chain`.
#[derive(clap::Args, Debug)]
pub struct VerifyChainArgs {
    /// Memory id to start the chain walk from.
    pub memory_id: String,

    /// Output format: `json` or `text`.
    #[arg(long, value_name = "FORMAT", default_value = "text")]
    pub format: String,

    /// Include `signed_events` creation entries in the report.
    #[arg(long)]
    pub include_signed_events: bool,
}

// ─────────────────────────────────────────────────────────────────────
// Report types
// ─────────────────────────────────────────────────────────────────────

/// One `reflects_on` edge in the ancestry tree, with its verification
/// result.
#[derive(Debug, Serialize)]
pub(super) struct EdgeResult {
    pub(super) source_id: String,
    pub(super) target_id: String,
    /// Signature bytes as hex, or `null` when the edge is unsigned.
    pub(super) signature_hex: Option<String>,
    pub(super) attest_level: String,
    pub(super) verified: bool,
    /// Human-readable reason when `verified = false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) failure_reason: Option<String>,
}

/// Per-`signed_events` row summary for a memory in the chain.
#[derive(Debug, Serialize)]
pub(super) struct SignedEventSummary {
    pub(super) memory_id: String,
    pub(super) event_id: String,
    pub(super) event_type: String,
    pub(super) attest_level: String,
    pub(super) timestamp: String,
    pub(super) signature_present: bool,
}

/// Full chain-integrity report — the `AgenticMem Attest` tier evidence
/// packet.
#[derive(Debug, Serialize)]
pub(super) struct ChainReport {
    /// Root memory id supplied on the command line.
    pub(super) root_id: String,
    /// Total number of distinct memories visited (root + ancestors).
    pub(super) n_memories: usize,
    /// Longest path from root to a depth-0 memory.
    pub(super) chain_depth: usize,
    /// Number of `reflects_on` edges that passed Ed25519 verification
    /// (or were unsigned but presence-confirmed).
    pub(super) edges_verified: usize,
    /// Number of edges that failed verification, with reasons.
    pub(super) edges_failed: usize,
    /// Per-edge detail.
    pub(super) edges: Vec<EdgeResult>,
    /// Maximum `reflection_depth` column value per namespace.
    pub(super) max_reflection_depth_per_namespace: HashMap<String, i32>,
    /// `"within_cap"` when no namespace exceeded its governance cap,
    /// `"exceeded_cap"` when at least one did, or
    /// `"no_cap_configured"` when no governance policy rows exist.
    pub(super) bounded_status: String,
    /// Optional `signed_events` entries when `--include-signed-events`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) signed_events: Vec<SignedEventSummary>,
    /// RFC3339 timestamp of report generation.
    pub(super) generated_at: String,
}

// ─────────────────────────────────────────────────────────────────────
// Helpers — package-private (pub(super) keeps the R7 surface clean)
// ─────────────────────────────────────────────────────────────────────

/// Encode bytes as a lowercase hexadecimal string. Used instead of the
/// `hex` crate (which is not a direct dependency) to keep the
/// dependency surface flat per repo convention.
fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Fetch a single memory's (id, namespace, reflection_depth) tuple.
/// Returns `None` when not found.
fn fetch_memory_meta(conn: &Connection, id: &str) -> Result<Option<(String, String, i32)>> {
    let mut stmt =
        conn.prepare("SELECT id, namespace, reflection_depth FROM memories WHERE id = ?1")?;
    let mut rows = stmt.query(params![id])?;
    if let Some(row) = rows.next()? {
        Ok(Some((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i32>(2).unwrap_or(0),
        )))
    } else {
        Ok(None)
    }
}

/// One `reflects_on` edge row as returned by the DB.
struct EdgeRow {
    target_id: String,
    signature: Option<Vec<u8>>,
    observed_by: Option<String>,
    attest_level: Option<String>,
    valid_from: Option<String>,
    valid_until: Option<String>,
}

/// Fetch all `reflects_on` edges whose `source_id = memory_id`,
/// including the temporal-validity columns that are part of the
/// signed bundle (H2 signs `valid_from` + `valid_until` alongside
/// the other link fields — verification must re-derive the same
/// canonical CBOR, so these must round-trip from the DB).
fn fetch_reflects_on_edges(conn: &Connection, source_id: &str) -> Result<Vec<EdgeRow>> {
    let mut stmt = conn.prepare(
        "SELECT target_id, signature, observed_by, attest_level, valid_from, valid_until \
         FROM memory_links \
         WHERE source_id = ?1 AND relation = 'reflects_on'",
    )?;
    let rows = stmt.query_map(params![source_id], |row| {
        Ok(EdgeRow {
            target_id: row.get::<_, String>(0)?,
            signature: row.get::<_, Option<Vec<u8>>>(1)?,
            observed_by: row.get::<_, Option<String>>(2)?,
            attest_level: row.get::<_, Option<String>>(3)?,
            valid_from: row.get::<_, Option<String>>(4)?,
            valid_until: row.get::<_, Option<String>>(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Fetch up to 1000 `signed_events` rows whose `agent_id` matches any
/// of the supplied memory ids (by convention the audit rows for a
/// reflect use the agent_id field as the actor's identifier; the
/// memory_id is embedded in the payload). Best-effort — returns empty
/// on query failure.
fn fetch_signed_events_for(conn: &Connection, ids: &[String]) -> Result<Vec<SignedEventSummary>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // Build positional params manually — rusqlite's `params!` macro
    // cannot splat a runtime-length slice, so we construct the SQL
    // placeholder string ourselves and pass a slice of `&dyn ToSql`.
    let placeholders: String = ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT id, agent_id, event_type, payload_hash, signature, attest_level, timestamp \
         FROM signed_events \
         WHERE agent_id IN ({placeholders}) \
         ORDER BY timestamp ASC, id ASC \
         LIMIT 1000"
    );
    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> =
        ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(SignedEventSummary {
            event_id: row.get::<_, String>(0)?,
            memory_id: row.get::<_, String>(1)?,
            event_type: row.get::<_, String>(2)?,
            // col 3 = payload_hash (unused in summary)
            signature_present: row.get::<_, Option<Vec<u8>>>(4)?.is_some(),
            attest_level: row.get::<_, String>(5)?,
            timestamp: row.get::<_, String>(6)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Look up the governance `max_reflection_depth` for a namespace.
///
/// Delegates to the existing [`crate::db::resolve_governance_policy`]
/// chain-walker which reads `metadata.governance` from namespace
/// standard memories (walking leaf-first up to the global `*`
/// standard). Returns `None` when no policy with a
/// `max_reflection_depth` exists anywhere in the chain.
fn governance_cap_for_namespace(conn: &Connection, namespace: &str) -> Option<u32> {
    crate::db::resolve_governance_policy(conn, namespace).and_then(|p| p.max_reflection_depth)
}

// ─────────────────────────────────────────────────────────────────────
// Core chain-walk + verify logic
// ─────────────────────────────────────────────────────────────────────

/// Walk the `reflects_on` ancestry tree from `root_id`, verify every
/// edge, and return the [`ChainReport`].
pub(super) fn build_chain_report(
    conn: &Connection,
    root_id: &str,
    include_signed_events: bool,
) -> Result<ChainReport> {
    let generated_at = chrono::Utc::now().to_rfc3339();

    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    queue.push_back((root_id.to_string(), 0));

    let mut edges: Vec<EdgeResult> = Vec::new();
    let mut max_depth_per_ns: HashMap<String, i32> = HashMap::new();
    let mut chain_depth: usize = 0;
    let mut all_ids: Vec<String> = Vec::new();
    let mut any_governance_row = false;
    let mut cap_exceeded = false;

    while let Some((current_id, hop)) = queue.pop_front() {
        if visited.contains(&current_id) {
            continue;
        }
        visited.insert(current_id.clone());
        all_ids.push(current_id.clone());

        if hop > chain_depth {
            chain_depth = hop;
        }

        // Fetch memory meta to track namespace depths and check cap.
        if let Some((_id, ns, rd)) = fetch_memory_meta(conn, &current_id)? {
            let entry = max_depth_per_ns.entry(ns.clone()).or_insert(0_i32);
            if rd > *entry {
                *entry = rd;
            }
            if let Some(cap) = governance_cap_for_namespace(conn, &ns) {
                any_governance_row = true;
                #[allow(clippy::cast_sign_loss)]
                if rd > 0 && rd as u32 > cap {
                    cap_exceeded = true;
                }
            }
        }

        // Walk outbound `reflects_on` edges.
        let out_edges = fetch_reflects_on_edges(conn, &current_id)?;
        for row in out_edges {
            let attest_level = row
                .attest_level
                .clone()
                .unwrap_or_else(|| "unsigned".to_string());

            let (verified, failure_reason, signature_hex) = verify_edge(
                &current_id,
                &row.target_id,
                row.signature.as_deref(),
                row.observed_by.as_deref(),
                row.valid_from.as_deref(),
                row.valid_until.as_deref(),
                &attest_level,
            );

            let target_id = row.target_id.clone();
            edges.push(EdgeResult {
                source_id: current_id.clone(),
                target_id: target_id.clone(),
                signature_hex,
                attest_level,
                verified,
                failure_reason,
            });

            if !visited.contains(&target_id) {
                queue.push_back((target_id, hop + 1));
            }
        }
    }

    let edges_failed = edges.iter().filter(|e| !e.verified).count();
    let edges_verified = edges.len() - edges_failed;

    let bounded_status = if cap_exceeded {
        "exceeded_cap"
    } else if any_governance_row {
        "within_cap"
    } else {
        "no_cap_configured"
    }
    .to_string();

    let signed_events = if include_signed_events {
        fetch_signed_events_for(conn, &all_ids).unwrap_or_default()
    } else {
        Vec::new()
    };

    Ok(ChainReport {
        root_id: root_id.to_string(),
        n_memories: visited.len(),
        chain_depth,
        edges_verified,
        edges_failed,
        edges,
        max_reflection_depth_per_namespace: max_depth_per_ns,
        bounded_status,
        signed_events,
        generated_at,
    })
}

/// Attempt to verify a single `reflects_on` edge's Ed25519 signature.
///
/// Returns `(verified, failure_reason, signature_hex)`. An unsigned
/// edge (no signature blob) is always considered "verified" — absence
/// of a signature is not a failure; it means the edge was written
/// before attestation was enabled.
/// Verify a single `reflects_on` edge's Ed25519 signature.
///
/// `valid_from` and `valid_until` must be the raw values stored in
/// `memory_links` — they are part of the signed canonical CBOR bundle
/// (H2 commits to all six `SignableLink` fields at sign time). Passing
/// the wrong values causes the re-derived payload to diverge from what
/// the signer committed to, which makes Ed25519 reject the signature
/// even for an otherwise honest edge.
///
/// Returns `(verified, failure_reason, signature_hex)`.
fn verify_edge(
    source_id: &str,
    target_id: &str,
    sig_blob: Option<&[u8]>,
    observed_by: Option<&str>,
    valid_from: Option<&str>,
    valid_until: Option<&str>,
    attest_level: &str,
) -> (bool, Option<String>, Option<String>) {
    let signature_hex = sig_blob.map(bytes_to_hex);

    // Unsigned edge — presence-confirmed; no signature to verify.
    let Some(sig) = sig_blob else {
        return (true, None, None);
    };

    let Some(agent_id) = observed_by else {
        return (
            false,
            Some(
                "signature present but observed_by is NULL — \
                 cannot resolve public key"
                    .to_string(),
            ),
            signature_hex,
        );
    };

    if agent_id.is_empty() {
        return (
            false,
            Some("observed_by is empty — cannot resolve public key".to_string()),
            signature_hex,
        );
    }

    let pub_key = crate::identity::verify::lookup_peer_public_key(agent_id);
    let Some(pub_key) = pub_key else {
        return (
            false,
            Some(format!(
                "no public key enrolled for '{agent_id}' \
                 (attest_level={attest_level})"
            )),
            signature_hex,
        );
    };

    let link = SignableLink {
        src_id: source_id,
        dst_id: target_id,
        relation: "reflects_on",
        observed_by: Some(agent_id),
        valid_from,
        valid_until,
    };

    match crate::identity::verify::verify(&pub_key, &link, sig) {
        Ok(()) => (true, None, signature_hex),
        Err(e) => (false, Some(e.to_string()), signature_hex),
    }
}

// ─────────────────────────────────────────────────────────────────────
// Text renderer
// ─────────────────────────────────────────────────────────────────────

pub(super) fn render_text(report: &ChainReport, out: &mut CliOutput<'_>) -> Result<()> {
    writeln!(
        out.stdout,
        "verify-reflection-chain: root={} memories={} depth={} edges={} failed={}",
        report.root_id,
        report.n_memories,
        report.chain_depth,
        report.edges.len(),
        report.edges_failed,
    )?;
    writeln!(out.stdout, "bounded_status: {}", report.bounded_status)?;
    writeln!(out.stdout, "generated_at:   {}", report.generated_at)?;

    if !report.max_reflection_depth_per_namespace.is_empty() {
        writeln!(out.stdout, "\nmax_reflection_depth per namespace:")?;
        let mut ns_vec: Vec<_> = report.max_reflection_depth_per_namespace.iter().collect();
        ns_vec.sort_by_key(|(ns, _)| ns.as_str());
        for (ns, depth) in ns_vec {
            writeln!(out.stdout, "  {ns}: {depth}")?;
        }
    }

    if !report.edges.is_empty() {
        writeln!(out.stdout, "\nedges:")?;
        for e in &report.edges {
            let status = if e.verified { "OK" } else { "FAIL" };
            let src_short = &e.source_id[..e.source_id.len().min(8)];
            let tgt_short = &e.target_id[..e.target_id.len().min(8)];
            write!(
                out.stdout,
                "  [{status}] {src_short} -> {tgt_short}  attest={}",
                e.attest_level,
            )?;
            if let Some(ref reason) = e.failure_reason {
                write!(out.stdout, "  reason=\"{reason}\"")?;
            }
            writeln!(out.stdout)?;
        }
    }

    if !report.signed_events.is_empty() {
        writeln!(
            out.stdout,
            "\nsigned_events ({} rows):",
            report.signed_events.len()
        )?;
        for ev in &report.signed_events {
            writeln!(
                out.stdout,
                "  {} | {} | {} | sig={}",
                ev.event_id,
                ev.event_type,
                ev.timestamp,
                if ev.signature_present { "yes" } else { "no" }
            )?;
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// Entry point called by daemon_runtime dispatch
// ─────────────────────────────────────────────────────────────────────

/// Run the `verify-reflection-chain` subcommand against the SQLite DB at
/// `db_path`. Returns an exit code: `0` if the chain is intact,
/// `1` otherwise.
///
/// # Errors
///
/// Propagates I/O or database errors via `anyhow`.
pub fn run(db_path: &Path, args: &VerifyChainArgs, out: &mut CliOutput<'_>) -> Result<i32> {
    let json = args.format.to_ascii_lowercase() == "json";
    let conn = crate::db::open(db_path).context("open db")?;

    let report = build_chain_report(&conn, &args.memory_id, args.include_signed_events)?;

    if json {
        let payload = serde_json::to_string_pretty(&report).context("serialise chain report")?;
        writeln!(out.stdout, "{payload}")?;
    } else {
        render_text(&report, out)?;
    }

    // Exit non-zero when any edge failed or the chain exceeded its cap.
    if report.edges_failed > 0 || report.bounded_status == "exceeded_cap" {
        Ok(1)
    } else {
        Ok(0)
    }
}

// ─────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rusqlite::params;
    use tempfile::TempDir;

    use crate::db;
    use crate::identity::keypair as kp_mod;
    use crate::identity::sign;
    use crate::models::{Memory, Tier};

    fn open_test_db(tmp: &TempDir) -> (rusqlite::Connection, std::path::PathBuf) {
        let db_path = tmp.path().join("ai-memory.db");
        let conn = db::open(&db_path).expect("db::open");
        (conn, db_path)
    }

    fn insert_mem(conn: &rusqlite::Connection, ns: &str, depth: i32) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let mem = Memory {
            id: id.clone(),
            tier: Tier::Mid,
            namespace: ns.to_string(),
            title: format!("t-{depth}"),
            content: format!("c-{depth}"),
            reflection_depth: depth,
            created_at: now.clone(),
            updated_at: now,
            ..Default::default()
        };
        db::insert(conn, &mem).expect("insert");
        id
    }

    fn link_unsigned(conn: &rusqlite::Connection, src: &str, tgt: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO memory_links \
             (source_id, target_id, relation, created_at, attest_level) \
             VALUES (?1, ?2, 'reflects_on', ?3, 'unsigned')",
            params![src, tgt, Utc::now().to_rfc3339()],
        )
        .expect("link_unsigned");
    }

    /// Attach a `max_reflection_depth` governance policy to `ns` by
    /// inserting a namespace standard memory (the same mechanism the
    /// production path uses — see `resolve_governance_policy`).
    fn set_cap(conn: &rusqlite::Connection, ns: &str, cap: u32) {
        use crate::models::default_metadata;
        let now = Utc::now().to_rfc3339();
        let policy = crate::models::GovernancePolicy {
            max_reflection_depth: Some(cap),
            ..crate::models::GovernancePolicy::default()
        };
        let mut metadata = default_metadata();
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("agent_id".into(), serde_json::Value::String("test".into()));
            obj.insert("governance".into(), serde_json::to_value(&policy).unwrap());
        }
        let standard = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: format!("_standards-{ns}"),
            title: format!("standard for {ns}"),
            content: "policy".into(),
            created_at: now.clone(),
            updated_at: now,
            metadata,
            ..Default::default()
        };
        let sid = db::insert(conn, &standard).expect("insert standard");
        db::set_namespace_standard(conn, ns, &sid, None).expect("set_namespace_standard");
    }

    #[test]
    fn single_memory_no_edges_gives_empty_report() {
        let tmp = TempDir::new().unwrap();
        let (conn, _) = open_test_db(&tmp);
        let id = insert_mem(&conn, "ns", 0);

        let report = build_chain_report(&conn, &id, false).expect("report");

        assert_eq!(report.root_id, id);
        assert_eq!(report.n_memories, 1);
        assert_eq!(report.chain_depth, 0);
        assert_eq!(report.edges.len(), 0);
        assert_eq!(report.edges_failed, 0);
        assert_eq!(report.edges_verified, 0);
        assert_eq!(report.bounded_status, "no_cap_configured");
        assert!(report.signed_events.is_empty());
    }

    #[test]
    fn unsigned_chain_depth2_all_verified() {
        let tmp = TempDir::new().unwrap();
        let (conn, _) = open_test_db(&tmp);
        let d0 = insert_mem(&conn, "ns", 0);
        let d1 = insert_mem(&conn, "ns", 1);
        let d2 = insert_mem(&conn, "ns", 2);
        link_unsigned(&conn, &d2, &d1);
        link_unsigned(&conn, &d1, &d0);

        let report = build_chain_report(&conn, &d2, false).expect("report");

        assert_eq!(report.n_memories, 3);
        assert_eq!(report.chain_depth, 2);
        assert_eq!(report.edges_failed, 0);
        // Unsigned edges count as verified.
        assert!(report.edges.iter().all(|e| e.verified));
    }

    #[test]
    fn cap_exceeded_reported_in_bounded_status() {
        let tmp = TempDir::new().unwrap();
        let (conn, _) = open_test_db(&tmp);
        set_cap(&conn, "cap-ns", 0);
        let d0 = insert_mem(&conn, "cap-ns", 0);
        let d1 = insert_mem(&conn, "cap-ns", 1); // depth 1 > cap 0
        link_unsigned(&conn, &d1, &d0);

        let report = build_chain_report(&conn, &d1, false).expect("report");

        assert_eq!(report.bounded_status, "exceeded_cap");
    }

    #[test]
    fn tampered_sig_edge_marked_failed() {
        let tmp = TempDir::new().unwrap();
        let keys_tmp = TempDir::new().unwrap();
        let (conn, _) = open_test_db(&tmp);

        let agent = kp_mod::generate("tester-l13").expect("gen");
        kp_mod::save(&agent, keys_tmp.path()).expect("save");

        let d0 = insert_mem(&conn, "ns", 0);
        let d1 = insert_mem(&conn, "ns", 1);

        let now = Utc::now().to_rfc3339();
        let link = sign::SignableLink {
            src_id: &d1,
            dst_id: &d0,
            relation: "reflects_on",
            observed_by: Some(&agent.agent_id),
            valid_from: Some(&now),
            valid_until: None,
        };
        let mut sig = sign::sign(&agent, &link).expect("sign");
        sig[0] ^= 0x01; // tamper

        conn.execute(
            "INSERT OR IGNORE INTO memory_links \
             (source_id, target_id, relation, created_at, valid_from, \
              signature, observed_by, attest_level) \
             VALUES (?1, ?2, 'reflects_on', ?3, ?3, ?4, ?5, 'self_signed')",
            params![d1, d0, now, sig, agent.agent_id],
        )
        .expect("insert tampered");

        // Point key lookup at the test key dir.
        unsafe {
            std::env::set_var("AI_MEMORY_KEY_DIR", keys_tmp.path());
        }
        let report = build_chain_report(&conn, &d1, false).expect("report");
        unsafe {
            std::env::remove_var("AI_MEMORY_KEY_DIR");
        }

        assert_eq!(report.edges_failed, 1, "tampered edge must count as failed");
        assert!(
            report.edges[0].failure_reason.is_some(),
            "tampered edge must carry a reason"
        );
    }

    #[test]
    fn include_signed_events_flag_returns_vec() {
        let tmp = TempDir::new().unwrap();
        let (conn, _) = open_test_db(&tmp);
        let id = insert_mem(&conn, "se-ns", 0);

        // With flag=false the vec is always empty.
        let r = build_chain_report(&conn, &id, false).expect("report");
        assert!(r.signed_events.is_empty());

        // With flag=true it may still be empty (no events in this DB),
        // but the call must not error.
        let r2 = build_chain_report(&conn, &id, true).expect("report-se");
        let _ = r2.signed_events; // just assert it's accessible
    }

    #[test]
    fn bytes_to_hex_matches_format_pattern() {
        let b = vec![0x00, 0x0f, 0xff, 0xab];
        assert_eq!(bytes_to_hex(&b), "000fffab");
    }
}
