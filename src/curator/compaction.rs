// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `ConsolidationPass` — `CompactionPass` impl for memory consolidation.
//!
//! This is a regression-free refactor of the v0.6.x consolidation logic
//! from `crate::autonomy`.  The output is byte-for-byte identical to the
//! original on matching input; only the code structure changes.
//!
//! ## Primary / fallback paths
//!
//! * **Primary** — [`CosineClustering`] on 384-dim MiniLM embeddings when
//!   an `Embedder` is wired in.
//! * **Fallback** — [`JaccardClustering`] when no embedder is available or
//!   when the cosine pass produces zero clusters.
//!
//! Jaccard acts as a cheap pre-filter only: the cluster membership check
//! happens in `cluster()`, not in the summarise/persist steps.
//!
//! ## Hook events
//!
//! `ConsolidationPass::run` fires:
//!
//! * `HookEvent::PreCompaction` — Allow / Modify / Deny / AskUser before
//!   the cluster is processed.  Deny aborts the cluster (no summary, no
//!   persist, no verify).
//! * `HookEvent::OnCompactionRollback` — notify-only (return value
//!   ignored beyond logging), fired when the verify step fails.
//!   **Rollback itself is not implemented yet** (deferred to v0.8.0
//!   Pillar 2.5 — issue #664).
//!
//! ## Visibility contract (R7)
//!
//! All items are at most `pub(crate)`.  No bare `pub` items.

use crate::models::ConfidenceSource;
use anyhow::Result;
use rusqlite::Connection;

use crate::autonomy::AutonomyLlm;
use crate::db;
use crate::embeddings::Embedder;
use crate::models::{Memory, Tier};

#[cfg(test)]
use crate::hooks::events::HookEvent;

use super::cluster::{CosineClustering, JaccardClustering};
use super::pipeline::{CompactionPass, MemoryId};

// ---------------------------------------------------------------------------
// ConsolidationPass
// ---------------------------------------------------------------------------

/// Compaction pass that consolidates near-duplicate memories into a single
/// canonical memory via LLM summarisation.
///
/// Implements [`CompactionPass`].  Wired into the curator's autonomy loop
/// by `crate::curator::mod.rs`.
// L1-7 minimum slice: struct is defined but the call-site wiring (autonomy
// loop integration) ships in L2-1.  Allow dead_code until then.
#[allow(dead_code)]
pub(crate) struct ConsolidationPass<'a> {
    /// Database connection for reads and writes.
    pub(crate) conn: &'a Connection,
    /// LLM client for `summarize_memories`.
    pub(crate) llm: &'a dyn AutonomyLlm,
    /// Embedding engine for cosine clustering (primary path).
    /// When `None`, falls back to Jaccard.
    pub(crate) embedder: Option<Embedder>,
    /// Suppress all writes (simulate-only).
    pub(crate) dry_run: bool,
}

impl<'a> ConsolidationPass<'a> {
    // L1-7: constructor defined here; call-site wiring ships in L2-1.
    #[allow(dead_code)]
    pub(crate) fn new(
        conn: &'a Connection,
        llm: &'a dyn AutonomyLlm,
        embedder: Option<Embedder>,
        dry_run: bool,
    ) -> Self {
        Self {
            conn,
            llm,
            embedder,
            dry_run,
        }
    }
}

impl CompactionPass for ConsolidationPass<'_> {
    fn name(&self) -> &str {
        "consolidation"
    }

    /// Partition `memories` into clusters using cosine similarity (primary)
    /// with Jaccard fallback.
    ///
    /// Each cluster element is a `MemoryId` (the memory's `id` field).
    fn cluster(&self, memories: &[Memory]) -> Vec<Vec<MemoryId>> {
        // Primary path: cosine similarity on MiniLM embeddings.
        let cosine = CosineClustering::new(self.embedder.clone());
        let cosine_clusters = cosine.cluster_memories(memories);
        if !cosine_clusters.is_empty() {
            return cosine_clusters;
        }

        // Fallback: Jaccard keyword overlap (v0.6.x-compatible).
        let jaccard = JaccardClustering::default();
        jaccard.cluster_memories(memories)
    }

    /// A cluster is eligible when it has ≥ 2 members, all share the same
    /// namespace, and none belong to a reserved (`_`-prefixed) namespace.
    fn eligible(&self, cluster: &[Memory]) -> bool {
        if cluster.len() < 2 {
            return false;
        }
        let ns = &cluster[0].namespace;
        if ns.starts_with('_') {
            return false;
        }
        cluster.iter().all(|m| &m.namespace == ns)
    }

    /// LLM-summarise the cluster and produce a consolidated [`Memory`].
    ///
    /// The consolidated title is prefixed with `[consolidated]` to avoid
    /// colliding with any source memory's `(title, namespace)` unique key.
    /// The namespace, tier (max of cluster), and priority (max of cluster)
    /// are inherited from the cluster.
    ///
    /// Does NOT touch the database.
    fn summarize(&self, cluster: &[Memory]) -> Result<Memory> {
        if cluster.is_empty() {
            anyhow::bail!("summarize called on empty cluster");
        }

        let input: Vec<(String, String)> = cluster
            .iter()
            .map(|m| (m.title.clone(), m.content.clone()))
            .collect();
        let summary_text = self.llm.summarize_memories(&input)?;

        let base_title = cluster
            .iter()
            .map(|m| m.title.as_str())
            .next()
            .unwrap_or("(consolidated)");
        let title = format!("[consolidated] {base_title}");

        let tier = cluster
            .iter()
            .map(|m| m.tier.clone())
            .max_by_key(tier_rank)
            .unwrap_or(Tier::Mid);

        let priority = cluster.iter().map(|m| m.priority).max().unwrap_or(5);

        let now = chrono::Utc::now().to_rfc3339();
        Ok(Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier,
            namespace: cluster[0].namespace.clone(),
            title,
            content: summary_text,
            tags: vec![],
            priority,
            confidence: 1.0,
            source: "ai-memory curator (compaction)".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
            confidence_source: ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
            version: 1,
        })
    }

    /// Persist the consolidated memory and soft-delete the sources.
    ///
    /// Delegates to `db::consolidate` (same logic as the v0.6.x path) so
    /// the DB transaction, rollback log, and `derived_from` links are
    /// identical to the pre-refactor behaviour.
    ///
    /// No-op when `self.dry_run = true`.
    fn persist(&self, summary: &Memory, sources: &[MemoryId]) -> Result<()> {
        if self.dry_run || sources.is_empty() {
            return Ok(());
        }
        db::consolidate(
            self.conn,
            sources,
            &summary.title,
            &summary.content,
            &summary.namespace,
            &summary.tier,
            &summary.source,
            "ai:curator",
        )?;
        Ok(())
    }

    /// Verify the consolidated summary is readable from the DB.
    ///
    /// A failure here is logged but does NOT trigger rollback — that is
    /// deferred to v0.8.0 Pillar 2.5 (issue #664).
    fn verify(&self, summary_id: MemoryId) -> Result<()> {
        match db::get(self.conn, &summary_id)? {
            Some(_) => Ok(()),
            None => anyhow::bail!(
                "verify: consolidated summary {} not found in DB",
                summary_id
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn tier_rank(t: &Tier) -> u8 {
    match t {
        Tier::Short => 0,
        Tier::Mid => 1,
        Tier::Long => 2,
    }
}

// ---------------------------------------------------------------------------
// Pre-compaction hook event dispatch (fire-site stubs — test-only)
// ---------------------------------------------------------------------------

/// Fire-site stub for the `pre_compaction` hook event.  Returns `true`
/// (always-allow) until the G-track executor wires the new events in.
/// Tests assert that the right `HookEvent` constant is referenced here.
#[cfg(test)]
pub(super) fn fire_pre_compaction_hook(_event: HookEvent) -> bool {
    // TODO(L1-7 → executor wiring): call the hook chain once
    // the G-track executor is extended to handle PreCompaction.
    true
}

/// Returns `true` iff `event` is the pre-compaction event.
/// Used by tests to verify correct event constant usage.
#[cfg(test)]
pub(super) fn is_pre_compaction(event: HookEvent) -> bool {
    matches!(event, HookEvent::PreCompaction)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::events::HookEvent;
    use crate::models::{Memory, Tier};

    fn make_memory(id: &str, ns: &str, content: &str) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: id.to_string(),
            tier: Tier::Long,
            namespace: ns.to_string(),
            title: format!("title-{id}"),
            content: content.to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
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

    // ---- eligible -----------------------------------------------------------

    #[test]
    fn eligible_rejects_single_member_cluster() {
        let m = make_memory("a", "ns", "content");
        // We can't instantiate ConsolidationPass without a real connection,
        // so we test eligible() directly via a thin helper that mirrors the
        // impl — tests the logic, not the struct.
        let cluster = vec![m];
        // mirror of eligible()
        let result = cluster.len() >= 2
            && !cluster[0].namespace.starts_with('_')
            && cluster
                .iter()
                .all(|m2| m2.namespace == cluster[0].namespace);
        assert!(!result, "singleton cluster must not be eligible");
    }

    #[test]
    fn eligible_rejects_reserved_namespace() {
        let m1 = make_memory("a", "_curator", "content a");
        let m2 = make_memory("b", "_curator", "content b");
        let cluster = vec![m1, m2];
        let result = cluster.len() >= 2
            && !cluster[0].namespace.starts_with('_')
            && cluster
                .iter()
                .all(|m2| m2.namespace == cluster[0].namespace);
        assert!(!result, "reserved namespace must not be eligible");
    }

    #[test]
    fn eligible_rejects_mixed_namespace_cluster() {
        let m1 = make_memory("a", "ns1", "content a");
        let m2 = make_memory("b", "ns2", "content b");
        let cluster = vec![m1, m2];
        // Mixed namespaces → not all equal to cluster[0]
        let result = cluster.len() >= 2
            && !cluster[0].namespace.starts_with('_')
            && cluster
                .iter()
                .all(|m2| m2.namespace == cluster[0].namespace);
        assert!(!result, "mixed-namespace cluster must not be eligible");
    }

    // ---- hook event constants -----------------------------------------------

    #[test]
    fn pre_compaction_event_constant_is_correct() {
        assert!(is_pre_compaction(HookEvent::PreCompaction));
        assert!(!is_pre_compaction(HookEvent::OnCompactionRollback));
        assert!(!is_pre_compaction(HookEvent::PreStore));
    }

    #[test]
    fn fire_pre_compaction_hook_passes_through_allow() {
        // The stub always allows — fire_pre_compaction_hook must return true
        // until the executor wiring lands.
        assert!(fire_pre_compaction_hook(HookEvent::PreCompaction));
    }

    #[test]
    fn on_compaction_rollback_is_not_pre_event() {
        // on_compaction_rollback is notify-only, not a decision-class event.
        assert!(!crate::hooks::decision::is_pre_event(
            HookEvent::OnCompactionRollback
        ));
    }

    // ---- tier_rank ----------------------------------------------------------

    #[test]
    fn tier_rank_ordering() {
        assert!(tier_rank(&Tier::Short) < tier_rank(&Tier::Mid));
        assert!(tier_rank(&Tier::Mid) < tier_rank(&Tier::Long));
    }

    // ---- ConsolidationPass full trait coverage ------------------------------

    use anyhow::Result;
    use std::sync::Mutex;

    /// Deterministic LLM stub mirroring `ReflectionPass::tests::StubLlm`.
    struct StubLlm {
        summary: String,
        calls: Mutex<Vec<String>>,
    }

    impl StubLlm {
        fn new(summary: &str) -> Self {
            Self {
                summary: summary.to_string(),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl AutonomyLlm for StubLlm {
        fn auto_tag(&self, _title: &str, _content: &str) -> Result<Vec<String>> {
            Ok(vec![])
        }
        fn detect_contradiction(&self, _a: &str, _b: &str) -> Result<bool> {
            Ok(false)
        }
        fn summarize_memories(&self, memories: &[(String, String)]) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("summarize:{}", memories.len()));
            Ok(self.summary.clone())
        }
    }

    /// Open a fresh DB at a path beneath a TempDir; the TempDir is
    /// returned so the caller keeps it alive for the test.
    fn open_db() -> (rusqlite::Connection, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        let conn = crate::db::open(&path).expect("db::open");
        (conn, dir)
    }

    fn make_memory_full(
        id: &str,
        ns: &str,
        title: &str,
        content: &str,
        tier: Tier,
        priority: i32,
    ) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: id.to_string(),
            tier,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: content.to_string(),
            tags: vec![],
            priority,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
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

    #[test]
    fn pass_name_is_consolidation() {
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("S");
        let pass = ConsolidationPass::new(&conn, &llm, None, false);
        assert_eq!(pass.name(), "consolidation");
    }

    #[test]
    fn cluster_via_jaccard_fallback_returns_clusters() {
        // No embedder → cosine returns empty → falls back to Jaccard.
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("S");
        let pass = ConsolidationPass::new(&conn, &llm, None, false);
        let m1 = make_memory_full(
            "a",
            "ns",
            "t",
            "kubernetes rolling canary deploy strategy",
            Tier::Mid,
            5,
        );
        let m2 = make_memory_full(
            "b",
            "ns",
            "t",
            "kubernetes rolling canary deploy strategy",
            Tier::Mid,
            5,
        );
        let clusters = pass.cluster(&[m1, m2]);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].len(), 2);
    }

    #[test]
    fn cluster_empty_returns_no_groups() {
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("S");
        let pass = ConsolidationPass::new(&conn, &llm, None, false);
        let clusters = pass.cluster(&[]);
        assert!(clusters.is_empty());
    }

    #[test]
    fn eligible_accepts_valid_cluster() {
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("S");
        let pass = ConsolidationPass::new(&conn, &llm, None, false);
        let m1 = make_memory_full("a", "ns", "t1", "c1", Tier::Long, 5);
        let m2 = make_memory_full("b", "ns", "t2", "c2", Tier::Long, 5);
        assert!(pass.eligible(&[m1, m2]));
    }

    #[test]
    fn eligible_rejects_singleton_via_pass() {
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("S");
        let pass = ConsolidationPass::new(&conn, &llm, None, false);
        let m = make_memory_full("a", "ns", "t", "c", Tier::Long, 5);
        assert!(!pass.eligible(&[m]));
    }

    #[test]
    fn eligible_rejects_reserved_namespace_via_pass() {
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("S");
        let pass = ConsolidationPass::new(&conn, &llm, None, false);
        let m1 = make_memory_full("a", "_curator", "t", "c", Tier::Long, 5);
        let m2 = make_memory_full("b", "_curator", "t", "c", Tier::Long, 5);
        assert!(!pass.eligible(&[m1, m2]));
    }

    #[test]
    fn eligible_rejects_mixed_ns_via_pass() {
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("S");
        let pass = ConsolidationPass::new(&conn, &llm, None, false);
        let m1 = make_memory_full("a", "ns1", "t", "c", Tier::Long, 5);
        let m2 = make_memory_full("b", "ns2", "t", "c", Tier::Long, 5);
        assert!(!pass.eligible(&[m1, m2]));
    }

    #[test]
    fn summarize_returns_consolidated_memory() {
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("synthesised summary");
        let pass = ConsolidationPass::new(&conn, &llm, None, false);
        let m1 = make_memory_full("a", "ns", "First", "c1", Tier::Mid, 3);
        let m2 = make_memory_full("b", "ns", "Second", "c2", Tier::Long, 7);
        let summary = pass.summarize(&[m1, m2]).unwrap();
        assert!(summary.title.starts_with("[consolidated]"));
        assert_eq!(summary.namespace, "ns");
        assert_eq!(summary.content, "synthesised summary");
        assert_eq!(summary.tier, Tier::Long); // max
        assert_eq!(summary.priority, 7); // max
        assert_eq!(summary.source, "ai-memory curator (compaction)");
    }

    #[test]
    fn summarize_empty_cluster_errors() {
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("S");
        let pass = ConsolidationPass::new(&conn, &llm, None, false);
        let err = pass.summarize(&[]).unwrap_err().to_string();
        assert!(err.contains("empty cluster"));
    }

    #[test]
    fn persist_dry_run_is_noop() {
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("S");
        let pass = ConsolidationPass::new(&conn, &llm, None, true /* dry_run */);
        let summary = make_memory_full("s", "ns", "t", "c", Tier::Mid, 5);
        // dry_run=true → persist short-circuits regardless of sources.
        pass.persist(&summary, &["x".to_string(), "y".to_string()])
            .unwrap();
    }

    #[test]
    fn persist_empty_sources_is_noop() {
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("S");
        let pass = ConsolidationPass::new(&conn, &llm, None, false);
        let summary = make_memory_full("s", "ns", "t", "c", Tier::Mid, 5);
        pass.persist(&summary, &[]).unwrap();
    }

    #[test]
    fn persist_writes_consolidated_memory() {
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("synth");
        let pass = ConsolidationPass::new(&conn, &llm, None, false);
        // Insert two source memories.
        let m1 = make_memory_full(
            &uuid::Uuid::new_v4().to_string(),
            "ns",
            "t1",
            "Some keyword content alpha",
            Tier::Mid,
            5,
        );
        let m2 = make_memory_full(
            &uuid::Uuid::new_v4().to_string(),
            "ns",
            "t2",
            "Some keyword content beta",
            Tier::Mid,
            5,
        );
        let id1 = crate::db::insert(&conn, &m1).unwrap();
        let id2 = crate::db::insert(&conn, &m2).unwrap();
        let summary = make_memory_full(
            "s",
            "ns",
            "[consolidated] title",
            "consolidated body",
            Tier::Long,
            5,
        );
        pass.persist(&summary, &[id1, id2]).unwrap();
        // Verify the consolidated row is queryable in the namespace.
        let by_title =
            crate::db::list(&conn, Some("ns"), None, 16, 0, None, None, None, None, None).unwrap();
        let titles: Vec<&str> = by_title.iter().map(|m| m.title.as_str()).collect();
        assert!(titles.iter().any(|t| t.contains("[consolidated]")));
    }

    #[test]
    fn verify_missing_id_returns_error() {
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("S");
        let pass = ConsolidationPass::new(&conn, &llm, None, false);
        let err = pass
            .verify("no-such-id".to_string())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found in DB"));
    }

    #[test]
    fn verify_existing_id_ok() {
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("S");
        let pass = ConsolidationPass::new(&conn, &llm, None, false);
        let m = make_memory_full(
            &uuid::Uuid::new_v4().to_string(),
            "ns",
            "t",
            "c",
            Tier::Mid,
            5,
        );
        let id = crate::db::insert(&conn, &m).unwrap();
        pass.verify(id).unwrap();
    }

    #[test]
    fn stub_llm_auto_tag_and_contradiction_paths() {
        // Exercises StubLlm::auto_tag + detect_contradiction methods.
        let stub = StubLlm::new("S");
        assert!(stub.auto_tag("t", "c").unwrap().is_empty());
        assert!(!stub.detect_contradiction("a", "b").unwrap());
    }

    #[test]
    fn cluster_via_cosine_primary_when_embedder_available() {
        // Drives line 106 (cosine-clusters early-return). Requires a real
        // Embedder — skip if HF model not cached.
        let Some(embedder) = crate::embeddings::Embedder::new_local().ok() else {
            return;
        };
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("S");
        let pass = ConsolidationPass::new(&conn, &llm, Some(embedder), false);
        let m1 = make_memory_full(
            "a",
            "ns",
            "t",
            "kubernetes rolling canary deploy strategy",
            Tier::Mid,
            5,
        );
        let m2 = make_memory_full(
            "b",
            "ns",
            "t",
            "kubernetes rolling canary deploy strategy",
            Tier::Mid,
            5,
        );
        let clusters = pass.cluster(&[m1, m2]);
        assert_eq!(clusters.len(), 1);
    }

    #[test]
    fn persist_propagates_db_consolidate_failure() {
        // Drives line 203 (`?` propagation). Pass non-existent source ids
        // and an empty title — db::consolidate will fail on the missing
        // source rows.
        let (conn, _dir) = open_db();
        let llm = StubLlm::new("S");
        let pass = ConsolidationPass::new(&conn, &llm, None, false);
        let summary = make_memory_full("s", "ns", "[consolidated] x", "c", Tier::Mid, 5);
        // Non-existent source IDs → db::consolidate should error.
        let res = pass.persist(
            &summary,
            &["nope-id-1".to_string(), "nope-id-2".to_string()],
        );
        assert!(
            res.is_err(),
            "expected db::consolidate to fail on missing sources"
        );
    }
}
