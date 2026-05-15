// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L2-3 (issue #668) — Reflection invalidation propagation
//! (notification, not cascade).
//!
//! ## Wire contract
//!
//! When a `supersedes` edge lands with both endpoints carrying
//! `memory_kind = 'reflection'`, the substrate
//!
//! 1. Walks every memory `Mi` whose row satisfies
//!    `(Mi.id, invalidated_reflection_id, "reflects_on")` in
//!    `memory_links`. That is the set of memories which used the
//!    now-invalidated reflection as a reasoning source.
//! 2. For each `Mi`, writes one **notification memory** into the
//!    namespace `<Mi.namespace>/_invalidations`. The notification's
//!    `metadata` carries the four identifiers a curator/operator
//!    needs to triage: `dependent_id`, `invalidated_id`,
//!    `invalidating_id`, and an RFC3339 `timestamp`.
//! 3. Appends one `reflection.invalidation_notified` row to
//!    `signed_events` per notification so an auditor can replay the
//!    exact set of dependents that were flagged for review.
//!
//! ## Why notification, not cascade
//!
//! Dependents are **not** auto-superseded. A reflection that pointed
//! at the invalidated reflection may still be valid (the new winner
//! could be a narrower restatement, a rephrasing, or a strictly
//! stronger claim that the dependent should adopt unchanged).
//! Auto-cascading the invalidation would
//!
//! * destroy curator/operator judgment on whether the dependent
//!   chain is genuinely affected, and
//! * burn the trust budget the substrate has built: a single bad
//!   supersession would silently nuke an arbitrarily large reflection
//!   sub-graph.
//!
//! The notification memory is the explicit hand-off — operators (via
//! the new `memory_dependents_of_invalidated` MCP tool) or the
//! curator pass surface the flagged set and the human/agent decides
//! per-dependent.
//!
//! ## Idempotency
//!
//! The walker is **not** internally idempotent in v0.7.0 — calling
//! it twice on the same invalidation produces two notification
//! memories per dependent (they upsert on `(title, namespace)` so
//! the row count stays bounded by the namespace+title combinatorics,
//! but each call still attempts the insert). The MCP-side caller in
//! `mcp::tools::link::handle_link` only fires the walker once per
//! successful supersedes write, so duplicates require a deliberate
//! re-invocation. This keeps the helper simple; the v0.8.0 backlog
//! tracks moving idempotency into the walker itself if the
//! cross-peer federation case demands it.

use crate::models::{Memory, MemoryKind, Tier};
use anyhow::Result;
use rusqlite::{Connection, params};
use serde_json::json;

/// One namespaced notification row to be written into
/// `<namespace>/_invalidations`.
///
/// Internal-only struct: callers consume the higher-level entry
/// points (`propagate_reflection_invalidation`,
/// `list_dependents_of_invalidated`). Kept distinct from the wire
/// `Memory` so the walker can stage all rows before any DB write
/// (the write loop short-circuits on the first error so a partial
/// fan-out leaves a deterministic prefix).
#[derive(Debug, Clone)]
struct PendingNotification {
    dependent_id: String,
    dependent_namespace: String,
    invalidated_id: String,
    invalidating_id: String,
    timestamp: String,
}

/// Public entry point for the substrate-side walker.
///
/// Called by `mcp::tools::link::handle_link` exactly once per
/// successful Reflection→Reflection `supersedes` write. The caller
/// has already verified
///
/// * the edge relation is `"supersedes"`,
/// * both `invalidated_id` and `invalidating_id` resolve to
///   memories whose `memory_kind == MemoryKind::Reflection`.
///
/// Returns the list of dependent memory ids that were notified —
/// useful both for the `memory_link` wire response (so the caller
/// can log how many dependents were flagged) and for the test
/// suite's acceptance checks.
///
/// # Errors
///
/// Returns the first SQL error encountered. On error, any
/// notifications already written before the failure are left in
/// place — the substrate prefers eventual consistency to atomic
/// rollback here because (a) each notification is independently
/// useful, and (b) the `signed_events` companion row gives the
/// auditor the exact partial-prefix for forensic replay.
pub fn propagate_reflection_invalidation(
    conn: &Connection,
    invalidated_id: &str,
    invalidating_id: &str,
    signing_agent_id: &str,
) -> Result<Vec<String>> {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let dependents = list_dependents_of_invalidated_internal(conn, invalidated_id)?;
    let mut notified_ids: Vec<String> = Vec::with_capacity(dependents.len());

    for (dependent_id, dependent_namespace) in dependents {
        let pending = PendingNotification {
            dependent_id: dependent_id.clone(),
            dependent_namespace,
            invalidated_id: invalidated_id.to_string(),
            invalidating_id: invalidating_id.to_string(),
            timestamp: timestamp.clone(),
        };
        write_notification(conn, &pending, signing_agent_id)?;
        notified_ids.push(dependent_id);
    }

    Ok(notified_ids)
}

/// List the dependents of an invalidated reflection — every memory
/// whose row writes `(self → invalidated_id, "reflects_on")` into
/// `memory_links`. Returned as `(dependent_id, dependent_namespace)`
/// so the caller can shape the notification's target namespace
/// without a second DB round-trip.
///
/// Public via the parent module so the
/// `memory_dependents_of_invalidated` MCP tool can call it directly
/// without re-running the walker.
///
/// # Errors
///
/// Bubbles up rusqlite errors from the inner JOIN.
pub fn list_dependents_of_invalidated(
    conn: &Connection,
    invalidated_id: &str,
) -> Result<Vec<DependentRecord>> {
    let rows = list_dependents_of_invalidated_internal(conn, invalidated_id)?;
    Ok(rows
        .into_iter()
        .map(|(id, namespace)| DependentRecord { id, namespace })
        .collect())
}

/// Wire shape for the `memory_dependents_of_invalidated` MCP tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependentRecord {
    pub id: String,
    pub namespace: String,
}

/// SQL helper: pull `(dependent_id, dependent_namespace)` for every
/// inbound `reflects_on` edge pointed at `invalidated_id`.
fn list_dependents_of_invalidated_internal(
    conn: &Connection,
    invalidated_id: &str,
) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT m.id, m.namespace
           FROM memory_links l
           JOIN memories m ON m.id = l.source_id
          WHERE l.target_id = ?1 AND l.relation = 'reflects_on'",
    )?;
    let rows = stmt.query_map(params![invalidated_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out: Vec<(String, String)> = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Compose `<namespace>/_invalidations` for the notification target.
///
/// Hierarchical namespaces (e.g. `team/foo`) get the
/// `_invalidations` suffix appended after the deepest segment so the
/// dependent's parent scope still owns the notification.
fn invalidations_namespace_for(parent: &str) -> String {
    format!("{parent}/_invalidations")
}

/// Persist one notification memory + one `signed_events` row.
///
/// The notification memory is `Tier::Mid` (7-day TTL) — long enough
/// for a weekly curator pass to surface it, short enough that an
/// abandoned notification doesn't permanently bloat the namespace.
/// Operators that want the audit trail forever can re-promote a
/// notification to `Long` tier via `memory_promote` after triage.
fn write_notification(
    conn: &Connection,
    pending: &PendingNotification,
    signing_agent_id: &str,
) -> Result<()> {
    let now = pending.timestamp.clone();
    let target_namespace = invalidations_namespace_for(&pending.dependent_namespace);

    // Title carries the dependent + invalidated pair so the
    // namespace upsert is idempotent on the (dependent, invalidated)
    // pair — re-invoking the walker for the same pair doesn't
    // multiply rows.
    let title = format!(
        "invalidation: {} -> {}",
        pending.invalidated_id, pending.dependent_id
    );

    let metadata = json!({
        "agent_id": signing_agent_id,
        "notification_kind": "reflection_invalidation",
        "dependent_id": pending.dependent_id,
        "invalidated_id": pending.invalidated_id,
        "invalidating_id": pending.invalidating_id,
        "timestamp": pending.timestamp,
    });

    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: target_namespace,
        title,
        content: format!(
            "Reflection {invalidated} was superseded by {invalidating}. \
             Memory {dependent} reflects_on the now-invalidated reflection \
             and may need re-evaluation.",
            invalidated = pending.invalidated_id,
            invalidating = pending.invalidating_id,
            dependent = pending.dependent_id,
        ),
        tags: vec!["_invalidation".to_string()],
        priority: 7,
        confidence: 1.0,
        source: "notification".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None, // filled in by storage::insert via tier default
        metadata,
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };

    crate::storage::insert(conn, &mem)?;

    // Audit row: lets a downstream auditor replay every dependent
    // that was flagged for a given invalidation without scanning
    // the namespace.
    let payload_bytes = json!({
        "event": "reflection.invalidation_notified",
        "dependent_id": pending.dependent_id,
        "invalidated_id": pending.invalidated_id,
        "invalidating_id": pending.invalidating_id,
        "timestamp": pending.timestamp,
    })
    .to_string()
    .into_bytes();

    let event = crate::signed_events::SignedEvent {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: signing_agent_id.to_string(),
        event_type: "reflection.invalidation_notified".to_string(),
        payload_hash: crate::signed_events::payload_hash(&payload_bytes),
        signature: None,
        attest_level: "unsigned".to_string(),
        timestamp: pending.timestamp.clone(),
        ..crate::signed_events::SignedEvent::default()
    };
    if let Err(e) = crate::signed_events::append_signed_event(conn, &event) {
        // Best-effort — the notification memory itself is the
        // load-bearing artifact. Log loudly so the operator catches
        // a torn write but don't fail the walker (other dependents
        // still need their notifications).
        tracing::warn!(
            target: "signed_events",
            dependent_id = %pending.dependent_id,
            invalidated_id = %pending.invalidated_id,
            "failed to append reflection.invalidation_notified row: {e}"
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Memory;
    use crate::storage as db;

    fn fresh_conn() -> Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn make_mem(title: &str, namespace: &str, kind: MemoryKind) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: namespace.to_string(),
            title: title.to_string(),
            content: format!("body {title}"),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({"agent_id": "ai:tester"}),
            reflection_depth: if matches!(kind, MemoryKind::Reflection) {
                1
            } else {
                0
            },
            memory_kind: kind,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        }
    }

    #[test]
    fn invalidations_namespace_appends_underscore_segment() {
        assert_eq!(
            invalidations_namespace_for("team/alpha"),
            "team/alpha/_invalidations"
        );
        assert_eq!(
            invalidations_namespace_for("global"),
            "global/_invalidations"
        );
    }

    #[test]
    fn list_dependents_returns_inbound_reflects_on_only() {
        let conn = fresh_conn();
        // R1 (reflection) is the target of two reflects_on edges and one
        // related_to edge. Only the reflects_on rows should surface.
        let r1 = make_mem("R1", "ns-a", MemoryKind::Reflection);
        let m1 = make_mem("M1", "ns-a", MemoryKind::Observation);
        let m2 = make_mem("M2", "ns-b", MemoryKind::Observation);
        let m3 = make_mem("M3", "ns-a", MemoryKind::Observation);
        let r1_id = db::insert(&conn, &r1).expect("insert r1");
        let m1_id = db::insert(&conn, &m1).expect("insert m1");
        let m2_id = db::insert(&conn, &m2).expect("insert m2");
        let m3_id = db::insert(&conn, &m3).expect("insert m3");
        db::create_link(&conn, &m1_id, &r1_id, "reflects_on").expect("link m1");
        db::create_link(&conn, &m2_id, &r1_id, "reflects_on").expect("link m2");
        db::create_link(&conn, &m3_id, &r1_id, "related_to").expect("link m3 (noise)");

        let deps = list_dependents_of_invalidated(&conn, &r1_id).expect("walk");
        let ids: Vec<&str> = deps.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "only reflects_on edges count, got {ids:?}");
        assert!(ids.contains(&m1_id.as_str()));
        assert!(ids.contains(&m2_id.as_str()));
        assert!(!ids.contains(&m3_id.as_str()), "related_to leaked through");
    }

    #[test]
    fn propagate_writes_one_notification_per_dependent() {
        let conn = fresh_conn();
        let r1 = make_mem("R1", "ns-a", MemoryKind::Reflection);
        let r2 = make_mem("R2", "ns-a", MemoryKind::Reflection);
        let m1 = make_mem("M1", "ns-a", MemoryKind::Observation);
        let m2 = make_mem("M2", "ns-b", MemoryKind::Observation);
        let r1_id = db::insert(&conn, &r1).expect("insert r1");
        let r2_id = db::insert(&conn, &r2).expect("insert r2");
        let m1_id = db::insert(&conn, &m1).expect("insert m1");
        let m2_id = db::insert(&conn, &m2).expect("insert m2");
        db::create_link(&conn, &m1_id, &r1_id, "reflects_on").expect("m1→r1");
        db::create_link(&conn, &m2_id, &r1_id, "reflects_on").expect("m2→r1");

        let notified =
            propagate_reflection_invalidation(&conn, &r1_id, &r2_id, "ai:tester").expect("walk");
        assert_eq!(notified.len(), 2);

        // Notification rows landed in the dependent's namespace under
        // /_invalidations.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE namespace = ?1",
                params!["ns-a/_invalidations"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "ns-a got 1 notification (for m1)");

        let count_b: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE namespace = ?1",
                params!["ns-b/_invalidations"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count_b, 1, "ns-b got 1 notification (for m2)");
    }

    #[test]
    fn propagate_records_signed_events_row_per_notification() {
        let conn = fresh_conn();
        let r1 = make_mem("R1", "ns-a", MemoryKind::Reflection);
        let r2 = make_mem("R2", "ns-a", MemoryKind::Reflection);
        let m1 = make_mem("M1", "ns-a", MemoryKind::Observation);
        let r1_id = db::insert(&conn, &r1).expect("insert r1");
        let r2_id = db::insert(&conn, &r2).expect("insert r2");
        let m1_id = db::insert(&conn, &m1).expect("insert m1");
        db::create_link(&conn, &m1_id, &r1_id, "reflects_on").expect("m1→r1");

        let _ =
            propagate_reflection_invalidation(&conn, &r1_id, &r2_id, "ai:tester").expect("walk");

        let cnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM signed_events WHERE event_type = ?1",
                params!["reflection.invalidation_notified"],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert_eq!(cnt, 1, "one signed_events row per notification");
    }

    #[test]
    fn propagate_with_no_dependents_is_a_no_op() {
        let conn = fresh_conn();
        let r1 = make_mem("R1", "ns-a", MemoryKind::Reflection);
        let r2 = make_mem("R2", "ns-a", MemoryKind::Reflection);
        let r1_id = db::insert(&conn, &r1).expect("insert r1");
        let r2_id = db::insert(&conn, &r2).expect("insert r2");
        let notified =
            propagate_reflection_invalidation(&conn, &r1_id, &r2_id, "ai:tester").expect("walk");
        assert!(notified.is_empty());
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE namespace LIKE '%_invalidations'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn metadata_carries_all_four_required_fields() {
        let conn = fresh_conn();
        let r1 = make_mem("R1", "ns-a", MemoryKind::Reflection);
        let r2 = make_mem("R2", "ns-a", MemoryKind::Reflection);
        let m1 = make_mem("M1", "ns-a", MemoryKind::Observation);
        let r1_id = db::insert(&conn, &r1).expect("insert r1");
        let r2_id = db::insert(&conn, &r2).expect("insert r2");
        let m1_id = db::insert(&conn, &m1).expect("insert m1");
        db::create_link(&conn, &m1_id, &r1_id, "reflects_on").expect("m1→r1");

        let _ =
            propagate_reflection_invalidation(&conn, &r1_id, &r2_id, "ai:tester").expect("walk");

        let meta_str: String = conn
            .query_row(
                "SELECT metadata FROM memories WHERE namespace = ?1 LIMIT 1",
                params!["ns-a/_invalidations"],
                |r| r.get(0),
            )
            .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_str).unwrap();
        assert_eq!(meta["dependent_id"].as_str(), Some(m1_id.as_str()));
        assert_eq!(meta["invalidated_id"].as_str(), Some(r1_id.as_str()));
        assert_eq!(meta["invalidating_id"].as_str(), Some(r2_id.as_str()));
        assert!(meta["timestamp"].is_string());
        assert_eq!(
            meta["notification_kind"].as_str(),
            Some("reflection_invalidation")
        );
    }
}
