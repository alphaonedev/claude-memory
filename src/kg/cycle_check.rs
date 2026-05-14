// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Anti-self-reflection cycle detection for `reflects_on` edges.
//!
//! The `reflects_on` relation is directional: `A reflects_on B` means "A was
//! derived from B". A cycle in this relation — e.g. `A → B → A` — is a
//! logical contradiction (A derived from B which was derived from A) and must
//! be refused before the edge is persisted.
//!
//! [`would_create_reflection_cycle`] performs a bounded backward walk from
//! `target_id` following `reflects_on` edges, returning `true` when `source_id`
//! is reachable (cycle detected) or `false` otherwise. The walk is bounded by
//! `max_depth` to prevent runaway traversal in deep reflection graphs;
//! [`cycle_path`] carries the walk log for the refusal audit row.

use rusqlite::{Connection, params};

/// Maximum number of hops the cycle-check walk follows before giving up.
/// Mirrors `GovernancePolicy::effective_max_reflection_depth()` compiled-in
/// default (3) as an upper bound; the caller passes the resolved cap so both
/// are always consistent.
const DEFAULT_MAX_DEPTH: u32 = 16;

/// Result of a cycle-check walk: whether a cycle would be created, and if so,
/// the full path from `source_id` back to `source_id` via `target_id`.
///
/// `cycle_path` is ordered `source_id → target_id → … → source_id`.  When
/// `would_cycle` is `false`, `cycle_path` is empty.
pub struct CycleCheckResult {
    pub would_cycle: bool,
    pub cycle_path: Vec<String>,
}

/// Walk `reflects_on` edges **forward** from `target_id`, bounded by
/// `max_depth` hops.  Returns `true` when `source_id` is reachable from
/// `target_id` by following existing edges (i.e. adding edge
/// `source_id → target_id` would close a cycle).
///
/// The forward walk direction: a `reflects_on` edge `(source=A, target=B)`
/// means "A reflects on B".  In graph terms the directed arc goes A → B.
/// To detect if adding `source → target` creates a cycle we walk forward from
/// `target` via existing edges and check whether we can reach `source`.  If
/// yes, the proposed edge would close the loop.
///
/// Example: existing edges A→B and B→C.  Proposed: C→A.  Walk forward from A:
///   hop 1: {B}  hop 2: {C}  — found A is not in the visited set.  But wait,
///   we walk from `target` (A in the proposed C→A), forward, and check if
///   we find `source` (C).  Hop 1 from A: B.  Hop 2 from B: C.  C == source!
///   Cycle detected.
///
/// Returns a [`CycleCheckResult`] with `would_cycle = true` and the full path
/// when a cycle is found, or `would_cycle = false` with an empty path
/// otherwise.
///
/// # Errors (non-panicking)
///
/// SQL failures during the walk are treated as "no cycle" (fail-open) with a
/// `tracing::warn!` so a transient DB error never silently blocks a valid link
/// write.  The caller is responsible for surfacing the refusal on `true`.
pub fn would_create_reflection_cycle(
    conn: &Connection,
    source_id: &str,
    target_id: &str,
    max_depth: u32,
) -> CycleCheckResult {
    // Direct self-link is already blocked by `validate_link`; handle it
    // defensively here too so the audit path is always consistent.
    if source_id == target_id {
        return CycleCheckResult {
            would_cycle: true,
            cycle_path: vec![source_id.to_string(), target_id.to_string()],
        };
    }

    let bound = if max_depth == 0 {
        DEFAULT_MAX_DEPTH
    } else {
        max_depth
    };

    // BFS / iterative DFS over the backward reflects_on graph.
    // `visited` prevents revisiting nodes in diamond-shaped subgraphs.
    // `path_map` tracks the predecessor for each visited node so we can
    // reconstruct the cycle path if `source_id` is found.
    let mut frontier: Vec<String> = vec![target_id.to_string()];
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    // predecessor[node] = the node from which we first reached `node`
    let mut predecessor: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    visited.insert(target_id.to_string());

    let mut depth = 0u32;

    while !frontier.is_empty() && depth < bound {
        depth += 1;
        let mut next_frontier: Vec<String> = Vec::new();

        for current in &frontier {
            // Walk forward: find all nodes reachable from `current` via
            // `reflects_on` edges where `current` is the source.
            let neighbors = match forward_neighbors(conn, current) {
                Ok(ns) => ns,
                Err(e) => {
                    tracing::warn!(
                        target: "kg::cycle_check",
                        node = %current, error = %e,
                        "SQL error during reflects_on forward walk; treating as no cycle"
                    );
                    continue;
                }
            };

            for neighbor in neighbors {
                if neighbor == source_id {
                    // Cycle found: reconstruct path.
                    let path = reconstruct_path(source_id, target_id, current, &predecessor);
                    return CycleCheckResult {
                        would_cycle: true,
                        cycle_path: path,
                    };
                }
                if visited.insert(neighbor.clone()) {
                    predecessor.insert(neighbor.clone(), current.clone());
                    next_frontier.push(neighbor);
                }
            }
        }

        frontier = next_frontier;
    }

    CycleCheckResult {
        would_cycle: false,
        cycle_path: vec![],
    }
}

/// Return the set of nodes reachable from `node` via `reflects_on` edges
/// (i.e. the "targets" in rows where `source_id = node` and
/// `relation = 'reflects_on'`).
fn forward_neighbors(conn: &Connection, node: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT target_id FROM memory_links \
         WHERE source_id = ?1 AND relation = 'reflects_on'",
    )?;
    let rows = stmt.query_map(params![node], |row| row.get(0))?;
    rows.collect()
}

/// Reconstruct the cycle path given the predecessor map.
///
/// The cycle is: `source_id → target_id → … → found_at → source_id`.
/// We build the segment `target_id → … → found_at` by walking predecessors
/// backward from `found_at` to `target_id`, then prepend `source_id` and
/// append `source_id` again to close the loop.
fn reconstruct_path(
    source_id: &str,
    target_id: &str,
    found_at: &str,
    predecessor: &std::collections::HashMap<String, String>,
) -> Vec<String> {
    // Walk from `found_at` back to `target_id` using predecessor pointers.
    let mut segment: Vec<String> = vec![found_at.to_string()];
    let mut cur = found_at;
    // Predecessor of `target_id` would be absent (it's the root of the
    // backward walk), so this loop terminates.
    while let Some(pred) = predecessor.get(cur) {
        segment.push(pred.clone());
        cur = pred;
        if cur == target_id {
            break;
        }
    }
    segment.reverse();

    // Full cycle: source → target → [middle] → found_at → source
    let mut path = Vec::with_capacity(segment.len() + 2);
    path.push(source_id.to_string());
    path.extend(segment);
    path.push(source_id.to_string());
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn open_db() -> Connection {
        crate::db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn insert_memory(conn: &Connection, id: &str) {
        use crate::models::{Memory, Tier};
        use chrono::Utc;
        let now = Utc::now().to_rfc3339();
        let mem = Memory {
            id: id.to_string(),
            tier: Tier::Mid,
            namespace: "test".to_string(),
            title: format!("memory-{id}"),
            content: "content".to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id": "test-agent"}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
        };
        crate::db::insert(conn, &mem).expect("insert memory");
    }

    fn add_reflects_on(conn: &Connection, source_id: &str, target_id: &str) {
        crate::db::create_link(conn, source_id, target_id, "reflects_on")
            .expect("create reflects_on link");
    }

    // ── Unit tests for the internal cycle-check machinery ─────────────

    #[test]
    fn no_edges_is_no_cycle() {
        let conn = open_db();
        insert_memory(&conn, "a");
        insert_memory(&conn, "b");
        // No links yet — adding A→B is safe.
        let result = would_create_reflection_cycle(&conn, "a", "b", 8);
        assert!(!result.would_cycle);
        assert!(result.cycle_path.is_empty());
    }

    #[test]
    fn direct_cycle_detected() {
        // Existing: B→A. Proposed: A→B. Would close A→B→A.
        let conn = open_db();
        insert_memory(&conn, "a");
        insert_memory(&conn, "b");
        add_reflects_on(&conn, "b", "a"); // B reflects_on A

        let result = would_create_reflection_cycle(&conn, "a", "b", 8);
        assert!(
            result.would_cycle,
            "direct cycle A→B with B→A must be detected"
        );
        assert!(!result.cycle_path.is_empty());
        // Path must start and end with source_id ("a")
        assert_eq!(result.cycle_path.first().map(String::as_str), Some("a"));
        assert_eq!(result.cycle_path.last().map(String::as_str), Some("a"));
    }

    #[test]
    fn indirect_cycle_detected() {
        // Existing: A→B, B→C. Proposed: C→A. Would close C→A→B→C.
        let conn = open_db();
        insert_memory(&conn, "a");
        insert_memory(&conn, "b");
        insert_memory(&conn, "c");
        add_reflects_on(&conn, "a", "b"); // A reflects_on B
        add_reflects_on(&conn, "b", "c"); // B reflects_on C

        // Proposed: C reflects_on A
        let result = would_create_reflection_cycle(&conn, "c", "a", 8);
        assert!(
            result.would_cycle,
            "indirect cycle C→A with A→B→C must be detected"
        );
        assert!(!result.cycle_path.is_empty());
        assert_eq!(result.cycle_path.first().map(String::as_str), Some("c"));
        assert_eq!(result.cycle_path.last().map(String::as_str), Some("c"));
    }

    #[test]
    fn non_cycle_succeeds() {
        // Existing: A→B. Proposed: C→B. C is unrelated to A — no cycle.
        let conn = open_db();
        insert_memory(&conn, "a");
        insert_memory(&conn, "b");
        insert_memory(&conn, "c");
        add_reflects_on(&conn, "a", "b"); // A reflects_on B (existing)

        // Adding C→B: walk backward from B finds A, not C. Safe.
        let result = would_create_reflection_cycle(&conn, "c", "b", 8);
        assert!(
            !result.would_cycle,
            "C→B with only A→B existing is not a cycle"
        );
        assert!(result.cycle_path.is_empty());
    }

    #[test]
    fn depth_bound_respected() {
        // Chain: E→D→C→B→A. Proposed: A→E creates a long cycle.
        // With depth=2 the walk only reaches C (2 hops from E), so A is not
        // found and the function returns false (bounded walk, fail-open).
        let conn = open_db();
        for id in ["a", "b", "c", "d", "e"] {
            insert_memory(&conn, id);
        }
        add_reflects_on(&conn, "e", "d");
        add_reflects_on(&conn, "d", "c");
        add_reflects_on(&conn, "c", "b");
        add_reflects_on(&conn, "b", "a");

        // With bound=2: walk from E backward visits D (hop 1) and C (hop 2).
        // B and A are beyond the bound; A is not found → returns false.
        let bounded = would_create_reflection_cycle(&conn, "a", "e", 2);
        assert!(
            !bounded.would_cycle,
            "bounded walk (depth=2) must not reach A"
        );

        // With bound=5: full chain is reachable → cycle found.
        let unbounded = would_create_reflection_cycle(&conn, "a", "e", 5);
        assert!(
            unbounded.would_cycle,
            "walk with depth=5 must detect the cycle"
        );
    }

    // ---- C-5 (#699): close remaining gaps in cycle_check.rs.
    // Targets: lines 70-73 (direct self-link defensive branch), line 77
    // (`max_depth == 0` fallback to DEFAULT_MAX_DEPTH). ----

    #[test]
    fn direct_self_link_returns_cycle_with_two_node_path() {
        // Lines 70-73: when source_id == target_id, the function bails
        // immediately with would_cycle = true and a two-node path
        // `[source, target]`. This is defensive coverage; the validator
        // also blocks self-links upstream.
        let conn = open_db();
        insert_memory(&conn, "self");

        let result = would_create_reflection_cycle(&conn, "self", "self", 8);
        assert!(
            result.would_cycle,
            "direct self-link must be flagged as a cycle"
        );
        assert_eq!(
            result.cycle_path,
            vec!["self".to_string(), "self".to_string()]
        );
    }

    #[test]
    fn max_depth_zero_falls_back_to_default_bound() {
        // Line 77: `max_depth == 0` triggers the `DEFAULT_MAX_DEPTH`
        // fallback. We assert the function still detects a real cycle
        // when the caller passes the sentinel `0` (i.e. "use default").
        let conn = open_db();
        insert_memory(&conn, "a");
        insert_memory(&conn, "b");
        add_reflects_on(&conn, "b", "a"); // B reflects_on A

        // Pass 0 to invoke the fallback branch.
        let result = would_create_reflection_cycle(&conn, "a", "b", 0);
        assert!(
            result.would_cycle,
            "max_depth=0 should fall back to DEFAULT_MAX_DEPTH and still detect the cycle"
        );
        assert!(!result.cycle_path.is_empty());
    }
}
