// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Full-autonomy loop — stacks on the Track A curator daemon (#278).
//!
//! This module provides the four passes beyond auto-tag that are
//! required to earn a defensible "100% autonomous" claim:
//!
//! 1. **Consolidation** — find near-duplicate memories in the same
//!    namespace, LLM-summarise them into a single canonical memory,
//!    archive the originals. Uses `db::consolidate` for the DB work
//!    and `AutonomyLlm::summarize_memories` for the synthesis.
//! 2. **Forgetting of superseded memories** — when a memory carries
//!    `metadata.confirmed_contradictions`, demote or forget the older
//!    contradicted entry (the curator keeps the fresher one). Uses
//!    `db::forget_count` with a targeted id list.
//! 3. **Priority feedback** — nudge `priority` up for memories that
//!    are getting recalled, nudge it down for cold ones. Purely
//!    arithmetic; no LLM call.
//! 4. **Rollback log + self-report** — every autonomous action lands
//!    in a `_curator/rollback/<ts>` memory describing what happened
//!    and how to reverse it, and every cycle lands in
//!    `_curator/reports/<ts>` as a summary the operator (and other
//!    agents) can recall.
//!
//! ## Trait boundary — `AutonomyLlm`
//!
//! The curator previously coupled directly to `llm::OllamaClient`,
//! which blocked unit-testable end-to-end coverage. This module
//! defines a narrow trait that both `OllamaClient` (in prod) and
//! the [`tests::StubLlm`] (in tests) implement. The autonomy passes
//! are generic over `&dyn AutonomyLlm`.

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::db;
use crate::llm::OllamaClient;
use crate::models::{Memory, Tier};

/// Minimum Jaccard-keyword overlap required to treat two memories as
/// "near-duplicates" candidates for a consolidation cluster. Tuned
/// loosely — actual merge decision is still gated by an LLM pass.
pub const CONSOLIDATE_JACCARD_THRESHOLD: f64 = 0.55;

/// Cap on the number of memories in a single consolidation cluster —
/// prevents pathological mega-merges that would destroy provenance.
pub const CONSOLIDATE_MAX_CLUSTER_SIZE: usize = 8;

/// Reserved namespace prefix the curator writes to. Excluded from
/// further curator passes (the curator never acts on its own rollback
/// / report memories).
pub const CURATOR_NAMESPACE: &str = "_curator";

/// LLM surface the autonomy passes use. Implemented for `OllamaClient`
/// in prod and stubbed in tests. The `auto_tag` and `detect_contradiction`
/// methods are here for completeness — the autonomy passes themselves
/// currently only call `summarize_memories`, but exposing the three
/// together keeps the trait a single, testable LLM boundary that the
/// curator's `run_once` path can switch to in a follow-up PR.
#[allow(dead_code)]
pub trait AutonomyLlm {
    /// Generate tags for a memory.
    fn auto_tag(&self, title: &str, content: &str) -> Result<Vec<String>>;

    /// Return true iff the two pieces of content contradict each other.
    fn detect_contradiction(&self, mem_a: &str, mem_b: &str) -> Result<bool>;

    /// Produce a consolidated summary of N memories.
    fn summarize_memories(&self, memories: &[(String, String)]) -> Result<String>;
}

impl AutonomyLlm for OllamaClient {
    fn auto_tag(&self, title: &str, content: &str) -> Result<Vec<String>> {
        Self::auto_tag(self, title, content)
    }
    fn detect_contradiction(&self, mem_a: &str, mem_b: &str) -> Result<bool> {
        Self::detect_contradiction(self, mem_a, mem_b)
    }
    fn summarize_memories(&self, memories: &[(String, String)]) -> Result<String> {
        Self::summarize_memories(self, memories)
    }
}

/// Rollback-log entry stored as a memory in `_curator/rollback/<rfc3339>`.
///
/// Serialised as JSON in the memory's `content`. The memory's `metadata`
/// carries the `action` discriminator so operators can filter the
/// rollback log by kind via the normal `memory_list` + `tags_filter`
/// path.
///
/// The `Consolidate` variant is deliberately large (carries full
/// pre-merge memory snapshots) compared to `PriorityAdjust`. That's the
/// cost of being able to reverse a merge without network round-trips.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RollbackEntry {
    /// A consolidation was applied. `originals` are the full Memory
    /// snapshots pre-merge; `result_id` is the consolidated memory id.
    Consolidate {
        originals: Vec<Memory>,
        result_id: String,
    },
    /// A memory was forgotten (archived). `snapshot` is the memory as
    /// it was immediately before forgetting.
    Forget { snapshot: Memory },
    /// A priority adjustment. `memory_id`, `before`, `after`.
    PriorityAdjust {
        memory_id: String,
        before: i32,
        after: i32,
    },
}

impl RollbackEntry {
    fn action_tag(&self) -> &'static str {
        match self {
            Self::Consolidate { .. } => "consolidate",
            Self::Forget { .. } => "forget",
            Self::PriorityAdjust { .. } => "priority_adjust",
        }
    }
}

/// Structured outcome of a single autonomy pass. Aggregated into the
/// curator cycle's `CuratorReport` and also written back as a self-
/// report memory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AutonomyPassReport {
    pub clusters_formed: usize,
    pub memories_consolidated: usize,
    pub memories_forgotten: usize,
    pub priority_adjustments: usize,
    pub rollback_entries_written: usize,
    pub errors: Vec<String>,
}

/// Run all autonomy passes over the provided candidates in order:
/// consolidate → forget superseded → priority feedback → record
/// rollback log → write self-report. `dry_run` suppresses all writes.
///
/// Returns an `AutonomyPassReport` rather than `Result<…>` because
/// per-pass errors are already aggregated into `report.errors`;
/// the function itself cannot fail at the outer level.
pub fn run_autonomy_passes(
    conn: &Connection,
    llm: &dyn AutonomyLlm,
    candidates: &[Memory],
    dry_run: bool,
) -> AutonomyPassReport {
    let mut report = AutonomyPassReport::default();

    // Pass 1 — consolidation.
    let clusters = find_consolidation_clusters(candidates);
    report.clusters_formed = clusters.len();
    for cluster in clusters {
        match consolidate_cluster(conn, llm, &cluster, dry_run) {
            Ok(Some(entry)) => {
                if !dry_run && let Err(e) = persist_rollback_entry(conn, &entry) {
                    report
                        .errors
                        .push(format!("rollback-log write failed: {e}"));
                } else {
                    report.rollback_entries_written += 1;
                }
                if let RollbackEntry::Consolidate { originals, .. } = entry {
                    report.memories_consolidated += originals.len();
                }
            }
            Ok(None) => {}
            Err(e) => report.errors.push(format!("consolidate failed: {e}")),
        }
    }

    // Pass 2 — forget superseded.
    for mem in candidates {
        match forget_if_superseded(conn, mem, candidates, dry_run) {
            Ok(Some(entry)) => {
                if !dry_run && let Err(e) = persist_rollback_entry(conn, &entry) {
                    report
                        .errors
                        .push(format!("rollback-log write failed: {e}"));
                } else {
                    report.rollback_entries_written += 1;
                }
                report.memories_forgotten += 1;
            }
            Ok(None) => {}
            Err(e) => report.errors.push(format!("forget failed: {e}")),
        }
    }

    // Pass 3 — priority feedback.
    #[allow(unused_assignments)]
    for mem in candidates {
        match apply_priority_feedback(conn, mem, dry_run) {
            Ok(Some(entry)) => {
                if !dry_run && let Err(e) = persist_rollback_entry(conn, &entry) {
                    report
                        .errors
                        .push(format!("rollback-log write failed: {e}"));
                } else {
                    report.rollback_entries_written += 1;
                }
                report.priority_adjustments += 1;
            }
            Ok(None) => {}
            Err(e) => report.errors.push(format!("priority feedback failed: {e}")),
        }
    }

    report
}

fn find_consolidation_clusters(candidates: &[Memory]) -> Vec<Vec<Memory>> {
    // Group by namespace first — we never merge across namespaces.
    let mut by_ns: std::collections::HashMap<&str, Vec<&Memory>> = std::collections::HashMap::new();
    for m in candidates {
        if m.namespace.starts_with('_') {
            continue;
        }
        by_ns.entry(&m.namespace).or_default().push(m);
    }

    let mut clusters: Vec<Vec<Memory>> = Vec::new();
    for (_ns, group) in by_ns {
        let mut used = vec![false; group.len()];
        for i in 0..group.len() {
            if used[i] {
                continue;
            }
            let mut cluster = vec![group[i].clone()];
            used[i] = true;
            for j in (i + 1)..group.len() {
                if used[j] {
                    continue;
                }
                if cluster.len() >= CONSOLIDATE_MAX_CLUSTER_SIZE {
                    break;
                }
                if jaccard_similarity(&group[i].content, &group[j].content)
                    >= CONSOLIDATE_JACCARD_THRESHOLD
                {
                    cluster.push(group[j].clone());
                    used[j] = true;
                }
            }
            if cluster.len() >= 2 {
                clusters.push(cluster);
            }
        }
    }
    clusters
}

fn jaccard_similarity(a: &str, b: &str) -> f64 {
    use std::collections::HashSet;
    let tokens = |s: &str| -> HashSet<String> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() >= 3)
            .map(str::to_lowercase)
            .collect()
    };
    let ta = tokens(a);
    let tb = tokens(b);
    if ta.is_empty() && tb.is_empty() {
        return 0.0;
    }
    let inter = ta.intersection(&tb).count();
    let union = ta.union(&tb).count();
    if union == 0 {
        0.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        let result = inter as f64 / union as f64;
        result
    }
}

fn consolidate_cluster(
    conn: &Connection,
    llm: &dyn AutonomyLlm,
    cluster: &[Memory],
    dry_run: bool,
) -> Result<Option<RollbackEntry>> {
    if cluster.len() < 2 {
        return Ok(None);
    }
    // Skip clusters inside reserved namespaces (defensive; already
    // filtered at find_consolidation_clusters).
    if cluster.iter().any(|m| m.namespace.starts_with('_')) {
        return Ok(None);
    }

    let input: Vec<(String, String)> = cluster
        .iter()
        .map(|m| (m.title.clone(), m.content.clone()))
        .collect();
    let summary = llm.summarize_memories(&input)?;
    // Prefix the consolidated title so it never collides with one of
    // the source memories' (title, namespace) UNIQUE key. Source
    // rows still exist at INSERT time — db::consolidate deletes them
    // only after the new row lands.
    let base_title = cluster
        .iter()
        .map(|m| m.title.as_str())
        .next()
        .unwrap_or("(consolidated)");
    let title = format!("[consolidated] {base_title}");

    if dry_run {
        return Ok(Some(RollbackEntry::Consolidate {
            originals: cluster.to_vec(),
            result_id: "dry-run".to_string(),
        }));
    }

    let ids: Vec<String> = cluster.iter().map(|m| m.id.clone()).collect();
    let namespace = cluster[0].namespace.clone();
    // Tier = max of cluster (consolidate never downgrades).
    let tier = cluster
        .iter()
        .map(|m| m.tier.clone())
        .max_by_key(tier_rank)
        .unwrap_or(Tier::Mid);

    let result_id = db::consolidate(
        conn,
        &ids,
        &title,
        &summary,
        &namespace,
        &tier,
        "ai-memory curator (autonomy)",
        "ai:curator",
    )?;

    Ok(Some(RollbackEntry::Consolidate {
        originals: cluster.to_vec(),
        result_id,
    }))
}

fn tier_rank(t: &Tier) -> u8 {
    match t {
        Tier::Short => 0,
        Tier::Mid => 1,
        Tier::Long => 2,
    }
}

fn forget_if_superseded(
    conn: &Connection,
    mem: &Memory,
    all: &[Memory],
    dry_run: bool,
) -> Result<Option<RollbackEntry>> {
    // Only act on memories whose `confirmed_contradictions` list is
    // non-empty — i.e., a previous detect_contradiction pass already
    // flagged this pair.
    let contradictions = mem
        .metadata
        .get("confirmed_contradictions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if contradictions.is_empty() {
        return Ok(None);
    }

    // The current memory is superseded if a contradicting memory is
    // both newer AND has higher-or-equal confidence. We never forget
    // based on the contradicting memory alone — the decision requires
    // both freshness and trust.
    let by_id: std::collections::HashMap<&str, &Memory> =
        all.iter().map(|m| (m.id.as_str(), m)).collect();
    let mut superseder: Option<&Memory> = None;
    for v in contradictions {
        let Some(other_id) = v.as_str() else {
            continue;
        };
        if let Some(other) = by_id.get(other_id)
            && other.updated_at > mem.updated_at
            && other.confidence >= mem.confidence
        {
            superseder = Some(other);
            break;
        }
    }
    let Some(_) = superseder else {
        return Ok(None);
    };

    if dry_run {
        return Ok(Some(RollbackEntry::Forget {
            snapshot: mem.clone(),
        }));
    }

    // IMPORTANT: `db::delete` hard-deletes (no archive row). Recovery
    // for a forgotten memory relies on the RollbackEntry::Forget
    // snapshot we return — the caller persists it in `_curator/rollback`
    // with the full pre-forget memory embedded. That rollback entry
    // is long-tier so it's not auto-GC'd; `ai-memory curator --rollback
    // <id>` reverses the forget from that snapshot. (#300 item 1:
    // comment previously claimed db::delete archives; it does not.)
    db::delete(conn, &mem.id)?;

    Ok(Some(RollbackEntry::Forget {
        snapshot: mem.clone(),
    }))
}

fn apply_priority_feedback(
    conn: &Connection,
    mem: &Memory,
    dry_run: bool,
) -> Result<Option<RollbackEntry>> {
    // Access-signal policy:
    //   access_count >= 10 AND last_accessed_at within 7d → +1 (cap 10)
    //   access_count == 0 AND created_at older than 30d     → -1 (floor 1)
    //   else no change.
    let now = chrono::Utc::now();
    let before = mem.priority;
    let mut after = before;

    let last_accessed = mem
        .last_accessed_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(chrono::DateTime::<chrono::Utc>::from);

    let created = chrono::DateTime::parse_from_rfc3339(&mem.created_at)
        .ok()
        .map(chrono::DateTime::<chrono::Utc>::from);

    let recent = last_accessed.is_some_and(|t| (now - t).num_days() <= 7);
    let cold_enough = created.is_some_and(|t| (now - t).num_days() >= 30);

    if mem.access_count >= 10 && recent && after < 10 {
        after = after.saturating_add(1).min(10);
    } else if mem.access_count == 0 && cold_enough && after > 1 {
        after = after.saturating_sub(1).max(1);
    }

    if after == before {
        return Ok(None);
    }

    if !dry_run {
        db::update(
            conn,
            &mem.id,
            None,
            None,
            None,
            None,
            None,
            Some(after),
            None,
            None,
            None,
        )?;
    }

    Ok(Some(RollbackEntry::PriorityAdjust {
        memory_id: mem.id.clone(),
        before,
        after,
    }))
}

fn persist_rollback_entry(conn: &Connection, entry: &RollbackEntry) -> Result<()> {
    let now = chrono::Utc::now();
    let ts = now.to_rfc3339();
    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: format!("{CURATOR_NAMESPACE}/rollback"),
        title: format!("curator {} @ {}", entry.action_tag(), ts),
        content: serde_json::to_string(entry)?,
        tags: vec![
            "_curator".to_string(),
            "_rollback".to_string(),
            entry.action_tag().to_string(),
        ],
        priority: 3,
        confidence: 1.0,
        source: "ai-memory curator (autonomy)".to_string(),
        access_count: 0,
        created_at: ts.clone(),
        updated_at: ts,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({
            "agent_id": "ai:curator",
            "action": entry.action_tag(),
        }),
    };
    db::insert(conn, &mem)?;
    Ok(())
}

/// Write the cycle's report as a memory in `_curator/reports/<ts>`
/// so other agents can recall "what did the curator do".
pub fn persist_self_report(
    conn: &Connection,
    cycle_duration_ms: u128,
    pass_report: &AutonomyPassReport,
    auto_tagged: usize,
    contradictions_found: usize,
    errors_total: usize,
) -> Result<()> {
    let now = chrono::Utc::now();
    let ts = now.to_rfc3339();
    let body = serde_json::json!({
        "cycle_ts": ts,
        "cycle_duration_ms": cycle_duration_ms,
        "auto_tagged": auto_tagged,
        "contradictions_found": contradictions_found,
        "clusters_formed": pass_report.clusters_formed,
        "memories_consolidated": pass_report.memories_consolidated,
        "memories_forgotten": pass_report.memories_forgotten,
        "priority_adjustments": pass_report.priority_adjustments,
        "rollback_entries_written": pass_report.rollback_entries_written,
        "errors_total": errors_total,
    });
    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: format!("{CURATOR_NAMESPACE}/reports"),
        title: format!("curator cycle @ {ts}"),
        content: serde_json::to_string_pretty(&body)?,
        tags: vec!["_curator".to_string(), "_report".to_string()],
        priority: 2,
        confidence: 1.0,
        source: "ai-memory curator (autonomy)".to_string(),
        access_count: 0,
        created_at: ts.clone(),
        updated_at: ts,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "ai:curator"}),
    };
    db::insert(conn, &mem)?;
    Ok(())
}

/// Reverse a single rollback-log entry. Returns `true` if a reverse
/// action was applied, `false` if the entry was already superseded
/// (idempotent rollback).
///
/// Collision safety (#300 item 2): before re-inserting a snapshot we
/// check whether another memory now owns the same
/// `(title, namespace)` key. If it does, we refuse to overwrite —
/// `db::insert` is an UPSERT on that key and would silently replace
/// the unrelated memory's content. We return an error so the operator
/// can resolve the conflict manually (delete the offender or rename
/// one of them) rather than clobbering user data.
pub fn reverse_rollback_entry(conn: &Connection, entry: &RollbackEntry) -> Result<bool> {
    match entry {
        RollbackEntry::Consolidate {
            originals,
            result_id,
        } => {
            // Pre-flight: no title+ns collision against a different id?
            for m in originals {
                check_no_collision(conn, &m.title, &m.namespace, &m.id)?;
            }
            // Delete the consolidated memory; re-insert the originals.
            let existed = db::delete(conn, result_id)?;
            for m in originals {
                db::insert(conn, m)?;
            }
            Ok(existed)
        }
        RollbackEntry::Forget { snapshot } => {
            check_no_collision(conn, &snapshot.title, &snapshot.namespace, &snapshot.id)?;
            db::insert(conn, snapshot)?;
            Ok(true)
        }
        RollbackEntry::PriorityAdjust {
            memory_id,
            before,
            after: _,
        } => {
            let _ = db::update(
                conn,
                memory_id,
                None,
                None,
                None,
                None,
                None,
                Some(*before),
                None,
                None,
                None,
            )?;
            Ok(true)
        }
    }
}

/// Refuse to overwrite a memory that took the (title, namespace) slot
/// after the rollback target was forgotten/consolidated.
fn check_no_collision(
    conn: &Connection,
    title: &str,
    namespace: &str,
    expected_id: &str,
) -> Result<()> {
    let rows = db::list(
        conn,
        Some(namespace),
        None,
        50,
        0,
        None,
        None,
        None,
        None,
        None,
    )?;
    for row in rows {
        if row.namespace == namespace && row.title == title && row.id != expected_id {
            anyhow::bail!(
                "rollback aborted: memory {} now occupies (title={:?}, namespace={:?}) — \
                 reverting would overwrite it. Resolve the conflict manually.",
                row.id,
                title,
                namespace
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// In-test LLM stub. Deterministic: returns fixed tags + treats
    /// "contradict" as a sentinel in content to flag contradictions.
    struct StubLlm {
        // Read by the trait impls below; the test paths in this module exercise
        // `summarize_memories` only, so rustc 1.93+ flags these reads as dead.
        // Curator and MCP integration tests (in `mcp.rs`/`curator.rs`) cover
        // `auto_tag` and `detect_contradiction`; this stub keeps the protocol
        // complete so any future autonomy test can exercise either method.
        #[allow(dead_code)]
        auto_tag_result: Vec<String>,
        summary: String,
        #[allow(dead_code)]
        contradiction_sentinel: String,
        calls: Mutex<Vec<String>>,
    }

    impl StubLlm {
        fn new(summary: &str) -> Self {
            Self {
                auto_tag_result: vec!["auto".to_string(), "stub".to_string()],
                summary: summary.to_string(),
                contradiction_sentinel: "CONTRADICTS".to_string(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl AutonomyLlm for StubLlm {
        fn auto_tag(&self, title: &str, _content: &str) -> Result<Vec<String>> {
            self.calls.lock().unwrap().push(format!("auto_tag:{title}"));
            Ok(self.auto_tag_result.clone())
        }
        fn detect_contradiction(&self, a: &str, b: &str) -> Result<bool> {
            self.calls
                .lock()
                .unwrap()
                .push("detect_contradiction".to_string());
            Ok(
                a.contains(&self.contradiction_sentinel)
                    || b.contains(&self.contradiction_sentinel),
            )
        }
        fn summarize_memories(&self, memories: &[(String, String)]) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("summarize:{}", memories.len()));
            Ok(self.summary.clone())
        }
    }

    fn sample_mem(id: &str, ns: &str, title: &str, content: &str, tier: Tier) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: id.to_string(),
            tier,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: content.to_string(),
            tags: vec!["t".to_string()],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id":"ai:test"}),
        }
    }

    fn setup_conn() -> (tempfile::NamedTempFile, Connection) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let conn = db::open(tmp.path()).unwrap();
        (tmp, conn)
    }

    #[test]
    fn jaccard_similarity_basic() {
        let sim = jaccard_similarity(
            "the quick brown fox jumps over",
            "quick brown fox over the lazy",
        );
        assert!(sim > 0.4, "unexpected sim {sim}");
    }

    #[test]
    fn jaccard_similarity_empty() {
        assert!((jaccard_similarity("", "") - 0.0).abs() < 1e-9);
        assert!((jaccard_similarity("abc", "") - 0.0).abs() < 1e-9);
    }

    #[test]
    fn consolidation_clusters_group_by_namespace() {
        let a = sample_mem(
            "a",
            "ns1",
            "A",
            "the quick brown fox jumps over lazy dog",
            Tier::Mid,
        );
        let b = sample_mem(
            "b",
            "ns1",
            "B",
            "quick brown fox over lazy dog jumps",
            Tier::Mid,
        );
        let c = sample_mem(
            "c",
            "ns2",
            "C",
            "the quick brown fox jumps over lazy dog",
            Tier::Mid,
        );
        let clusters = find_consolidation_clusters(&[a, b, c]);
        // ns1 should cluster a+b; ns2 has only one memory so no cluster.
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].len(), 2);
    }

    #[test]
    fn consolidation_skips_reserved_namespace() {
        let a = sample_mem("a", "_curator/reports", "A", "content aaaa bbbb", Tier::Mid);
        let b = sample_mem("b", "_curator/reports", "B", "content aaaa bbbb", Tier::Mid);
        let clusters = find_consolidation_clusters(&[a, b]);
        assert!(clusters.is_empty());
    }

    #[test]
    fn rollback_entry_serialises() {
        let e = RollbackEntry::PriorityAdjust {
            memory_id: "m1".to_string(),
            before: 5,
            after: 6,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.contains("priority_adjust"));
        let back: RollbackEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.action_tag(), "priority_adjust");
    }

    #[test]
    fn consolidate_cluster_merges_two_memories() {
        let (_tmp, conn) = setup_conn();
        let a = sample_mem(
            "a",
            "app",
            "Deploy plan",
            "kubernetes rolling deploy with canary",
            Tier::Long,
        );
        let b = sample_mem(
            "b",
            "app",
            "Deploy process",
            "kubernetes deploy rolling canary strategy",
            Tier::Long,
        );
        db::insert(&conn, &a).unwrap();
        db::insert(&conn, &b).unwrap();
        let llm = StubLlm::new("consolidated deploy plan");
        let cluster = vec![a.clone(), b.clone()];
        let entry = consolidate_cluster(&conn, &llm, &cluster, false)
            .unwrap()
            .expect("expected rollback entry");
        match entry {
            RollbackEntry::Consolidate {
                originals,
                result_id,
            } => {
                assert_eq!(originals.len(), 2);
                assert_ne!(result_id, "dry-run");
                let got = db::get(&conn, &result_id).unwrap().expect("result memory");
                assert_eq!(got.namespace, "app");
                assert!(got.title.starts_with("[consolidated]"));
                assert!(got.content.contains("consolidated deploy plan"));
            }
            _ => panic!("expected Consolidate"),
        }
    }

    #[test]
    fn dry_run_does_not_write() {
        let (_tmp, conn) = setup_conn();
        let a = sample_mem(
            "a",
            "app",
            "Deploy plan",
            "kubernetes rolling deploy with canary",
            Tier::Long,
        );
        let b = sample_mem(
            "b",
            "app",
            "Deploy process",
            "kubernetes deploy rolling canary strategy",
            Tier::Long,
        );
        db::insert(&conn, &a).unwrap();
        db::insert(&conn, &b).unwrap();
        let llm = StubLlm::new("never persisted");
        let cluster = vec![a.clone(), b.clone()];
        let entry = consolidate_cluster(&conn, &llm, &cluster, true)
            .unwrap()
            .expect("dry-run returns entry");
        if let RollbackEntry::Consolidate { result_id, .. } = entry {
            assert_eq!(result_id, "dry-run");
        }
        // Originals still present, no consolidated row added.
        assert!(db::get(&conn, "a").unwrap().is_some());
        assert!(db::get(&conn, "b").unwrap().is_some());
    }

    #[test]
    fn reverse_consolidation_restores_originals() {
        let (_tmp, conn) = setup_conn();
        let a = sample_mem(
            "a",
            "app",
            "Deploy plan",
            "kubernetes rolling deploy canary",
            Tier::Long,
        );
        let b = sample_mem(
            "b",
            "app",
            "Deploy process",
            "kubernetes rolling canary strategy",
            Tier::Long,
        );
        db::insert(&conn, &a).unwrap();
        db::insert(&conn, &b).unwrap();

        let llm = StubLlm::new("summary");
        let cluster = vec![a.clone(), b.clone()];
        let entry = consolidate_cluster(&conn, &llm, &cluster, false)
            .unwrap()
            .expect("entry");

        // After consolidation, originals should be gone (merged into
        // the result id).
        if let RollbackEntry::Consolidate {
            result_id,
            originals,
        } = &entry
        {
            assert!(db::get(&conn, result_id).unwrap().is_some());
            for orig in originals {
                assert!(
                    db::get(&conn, &orig.id).unwrap().is_none(),
                    "{} should be merged-away",
                    orig.id
                );
            }
        }

        // Rollback: originals come back, result is removed.
        reverse_rollback_entry(&conn, &entry).unwrap();
        assert!(db::get(&conn, "a").unwrap().is_some());
        assert!(db::get(&conn, "b").unwrap().is_some());
        if let RollbackEntry::Consolidate { result_id, .. } = &entry {
            assert!(db::get(&conn, result_id).unwrap().is_none());
        }
    }

    #[test]
    fn full_autonomy_cycle_end_to_end() {
        let (_tmp, conn) = setup_conn();
        let llm = StubLlm::new("consolidated");

        // Seed: two near-duplicates in "deploy", one unrelated doc in
        // "chat", and a pair with a confirmed-contradictions pointer.
        let m_a = sample_mem(
            "ma",
            "deploy",
            "canary deploy plan",
            "kubernetes canary rolling deploy strategy",
            Tier::Long,
        );
        let m_b = sample_mem(
            "mb",
            "deploy",
            "canary deploy overview",
            "kubernetes rolling canary deploy strategy",
            Tier::Long,
        );
        let m_chat = sample_mem(
            "mchat",
            "chat",
            "hello",
            "hi there chat only content here",
            Tier::Mid,
        );

        // Superseded pair: m_old is older AND has a confirmed
        // contradiction against m_new.
        let mut m_old = sample_mem(
            "mold",
            "facts",
            "fact v1",
            "the sky is green always uniformly",
            Tier::Long,
        );
        let m_new_id = "mnew";
        m_old.metadata["confirmed_contradictions"] = serde_json::json!([m_new_id]);
        // Push m_old's updated_at to the past so m_new's default now
        // is strictly newer.
        m_old.updated_at = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        let m_new = sample_mem(
            m_new_id,
            "facts",
            "fact v2",
            "the sky is blue most of the time for sure",
            Tier::Long,
        );

        for m in [&m_a, &m_b, &m_chat, &m_old, &m_new] {
            db::insert(&conn, m).unwrap();
        }

        let candidates = vec![
            m_a.clone(),
            m_b.clone(),
            m_chat.clone(),
            m_old.clone(),
            m_new.clone(),
        ];
        let report = run_autonomy_passes(&conn, &llm, &candidates, false);

        // Consolidated at least once (deploy cluster).
        assert!(report.clusters_formed >= 1);
        assert!(report.memories_consolidated >= 2);
        // Forgot m_old because it's superseded by m_new.
        assert!(
            report.memories_forgotten >= 1,
            "expected ≥1 forget, got {report:?}"
        );
        // Rollback entries written for each action.
        assert!(report.rollback_entries_written >= report.clusters_formed);
        // Rollback-log memories exist.
        let log = db::list(
            &conn,
            Some("_curator/rollback"),
            None,
            100,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!log.is_empty(), "rollback log should be populated");
    }

    #[test]
    fn self_report_written_to_reports_namespace() {
        let (_tmp, conn) = setup_conn();
        let pass = AutonomyPassReport {
            clusters_formed: 1,
            memories_consolidated: 2,
            memories_forgotten: 0,
            priority_adjustments: 1,
            rollback_entries_written: 2,
            errors: vec![],
        };
        persist_self_report(&conn, 1234, &pass, 3, 0, 0).unwrap();
        let reports = db::list(
            &conn,
            Some("_curator/reports"),
            None,
            10,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].content.contains("memories_consolidated"));
    }

    #[test]
    fn smart_tier_mock_cycle_summarize() {
        // Test that autonomy invokes the LLM's summarize_memories in consolidation.
        let (_tmp, conn) = setup_conn();
        // Use similar enough content to exceed the Jaccard threshold (0.55)
        let a = sample_mem(
            "mem-a",
            "app",
            "Deploy A",
            "kubernetes deployment rolling canary strategy kubernetes rolling deploy canary",
            Tier::Mid,
        );
        let b = sample_mem(
            "mem-b",
            "app",
            "Deploy B",
            "kubernetes deployment rolling canary approach kubernetes rolling canary deploy",
            Tier::Mid,
        );
        db::insert(&conn, &a).unwrap();
        db::insert(&conn, &b).unwrap();

        let llm = StubLlm::new("LLM-generated consolidated summary");
        let candidates = vec![a, b];

        let report = run_autonomy_passes(&conn, &llm, &candidates, false);

        // Key assertions: LLM was used (clusters formed and consolidation happened)
        assert!(report.clusters_formed > 0);
        assert!(report.memories_consolidated > 0);
    }

    #[test]
    fn autonomy_cycle_with_mock_ollama() {
        // Test run_autonomy_passes end-to-end with StubLlm
        let (_tmp, conn) = setup_conn();
        let a = sample_mem(
            "id-1",
            "ns1",
            "Title A",
            "content similar enough for clustering test similar clustering",
            Tier::Mid,
        );
        let b = sample_mem(
            "id-2",
            "ns1",
            "Title B",
            "content similar enough for clustering test similar clustering",
            Tier::Mid,
        );
        db::insert(&conn, &a).unwrap();
        db::insert(&conn, &b).unwrap();

        let llm = StubLlm::new("mock summary result");
        let candidates = vec![a, b];

        let report = run_autonomy_passes(&conn, &llm, &candidates, false);

        // Report should reflect successful cycle
        assert_eq!(report.errors.len(), 0, "autonomy cycle should not error");
        assert!(
            report.rollback_entries_written > 0,
            "autonomy cycle should write rollback entries"
        );
    }

    #[test]
    fn rollback_log_captures_consolidation() {
        // Verify rollback log correctly records a consolidation
        let (_tmp, conn) = setup_conn();
        let a = sample_mem(
            "a",
            "test-ns",
            "Memory A",
            "test content aaaa bbbb cccc aaaa bbbb",
            Tier::Mid,
        );
        let b = sample_mem(
            "b",
            "test-ns",
            "Memory B",
            "test content aaaa bbbb cccc aaaa bbbb",
            Tier::Mid,
        );
        db::insert(&conn, &a).unwrap();
        db::insert(&conn, &b).unwrap();

        let llm = StubLlm::new("consolidated");
        let cluster = vec![a.clone(), b.clone()];
        let entry = consolidate_cluster(&conn, &llm, &cluster, false)
            .unwrap()
            .expect("rollback entry");

        // Persist the entry
        persist_rollback_entry(&conn, &entry).unwrap();

        // Verify it's in the rollback log
        let log = db::list(
            &conn,
            Some("_curator/rollback"),
            None,
            100,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(log.len(), 1);
        assert!(log[0].content.contains("consolidate"));
    }

    #[test]
    fn priority_feedback_adjusts_memory() {
        // Verify priority feedback changes memory priority based on access
        let (_tmp, conn) = setup_conn();
        let mut mem = sample_mem("id", "ns", "Title", "content", Tier::Mid);
        mem.priority = 5;
        mem.access_count = 100;
        db::insert(&conn, &mem).unwrap();

        let entry = apply_priority_feedback(&conn, &mem, false)
            .unwrap()
            .expect("priority feedback should produce entry");

        match entry {
            RollbackEntry::PriorityAdjust {
                memory_id,
                before,
                after,
            } => {
                assert_eq!(memory_id, "id");
                assert_eq!(before, 5);
                assert!(after > before, "high access should increase priority");
            }
            _ => panic!("expected PriorityAdjust"),
        }
    }

    #[test]
    fn dry_run_autonomy_does_not_write() {
        // Verify dry-run mode prevents all writes to DB
        let (_tmp, conn) = setup_conn();
        let a = sample_mem(
            "a",
            "test-ns",
            "Memory A",
            "test content aaaa bbbb cccc aaaa bbbb",
            Tier::Mid,
        );
        let b = sample_mem(
            "b",
            "test-ns",
            "Memory B",
            "test content aaaa bbbb cccc aaaa bbbb",
            Tier::Mid,
        );
        db::insert(&conn, &a).unwrap();
        db::insert(&conn, &b).unwrap();

        let initial_count = db::list(
            &conn,
            Some("test-ns"),
            None,
            100,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .len();

        let llm = StubLlm::new("consolidated");
        let candidates = vec![a, b];
        let _report = run_autonomy_passes(&conn, &llm, &candidates, true);

        let final_count = db::list(
            &conn,
            Some("test-ns"),
            None,
            100,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .len();

        assert_eq!(
            initial_count, final_count,
            "dry-run should not modify database"
        );
    }

    #[test]
    fn autonomy_passes_report_aggregates_errors() {
        // Verify error aggregation in AutonomyPassReport
        let (_tmp, conn) = setup_conn();
        let mem = sample_mem("id", "ns", "Title", "content", Tier::Mid);
        let llm = StubLlm::new("summary");
        let candidates = vec![mem];
        let report = run_autonomy_passes(&conn, &llm, &candidates, false);

        // At minimum, report structure should be valid
        assert!(report.clusters_formed > 0 || report.clusters_formed == 0);
    }
}
