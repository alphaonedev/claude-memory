// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_list` handler.

use crate::models::Tier;
use crate::{db, validate};
use serde_json::{Value, json};
pub(super) fn handle_list(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let namespace = params["namespace"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    // Ultrareview #339: saturate instead of panic (see handle_search).
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(20)).unwrap_or(usize::MAX);
    let agent_id = params["agent_id"].as_str();
    if let Some(aid) = agent_id {
        validate::validate_agent_id(aid).map_err(|e| e.to_string())?;
    }

    let results = db::list(
        conn,
        namespace,
        tier.as_ref(),
        limit.min(200),
        0,
        None,
        None,
        None,
        None,
        agent_id,
    )
    .map_err(|e| e.to_string())?;
    Ok(json!({"memories": results, "count": results.len()}))
}

#[cfg(test)]
mod tests {
    //! L0.7-3 Tier B chunk-A — coverage tests for `handle_list`.
    //!
    //! Six-category template subset relevant to a read-only list:
    //! A. happy path — empty + populated, optional filters
    //! B. validation — bad agent_id, invalid tier (silently ignored), limit overflow
    //! E. idempotency

    use super::*;
    use crate::models::{Memory, Tier as MTier};
    use crate::storage as db;

    fn fresh_conn() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn make_mem(title: &str, ns: &str, tier: MTier, agent: &str) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: format!("content for {title}"),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({"agent_id": agent}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        }
    }

    // A. happy path — empty db
    #[test]
    fn empty_db_returns_empty_list() {
        let conn = fresh_conn();
        let out = handle_list(&conn, &json!({})).expect("ok");
        assert_eq!(out["count"].as_u64(), Some(0));
        assert!(out["memories"].as_array().unwrap().is_empty());
    }

    // A. happy path — populated, default args
    #[test]
    fn returns_all_memories_with_default_limit() {
        let conn = fresh_conn();
        db::insert(&conn, &make_mem("a", "test", MTier::Mid, "ai:a")).expect("ins");
        db::insert(&conn, &make_mem("b", "test", MTier::Mid, "ai:b")).expect("ins");
        let out = handle_list(&conn, &json!({})).expect("ok");
        assert_eq!(out["count"].as_u64(), Some(2));
    }

    // A. happy path — namespace filter
    #[test]
    fn filters_by_namespace() {
        let conn = fresh_conn();
        db::insert(&conn, &make_mem("a", "ns1", MTier::Mid, "ai:a")).expect("ins");
        db::insert(&conn, &make_mem("b", "ns2", MTier::Mid, "ai:b")).expect("ins");
        let out = handle_list(&conn, &json!({"namespace": "ns1"})).expect("ok");
        assert_eq!(out["count"].as_u64(), Some(1));
    }

    // A. tier filter exercises Tier::from_str branch
    #[test]
    fn filters_by_tier() {
        let conn = fresh_conn();
        db::insert(&conn, &make_mem("a", "ns", MTier::Short, "ai:a")).expect("ins");
        db::insert(&conn, &make_mem("b", "ns", MTier::Long, "ai:b")).expect("ins");
        let out = handle_list(&conn, &json!({"tier": "long"})).expect("ok");
        assert_eq!(out["count"].as_u64(), Some(1));
        // invalid tier silently falls through (and_then None) — listed all.
        let out_bad = handle_list(&conn, &json!({"tier": "nonsense"})).expect("ok");
        assert_eq!(out_bad["count"].as_u64(), Some(2));
    }

    // A. agent_id filter (validated path)
    #[test]
    fn filters_by_agent_id() {
        let conn = fresh_conn();
        db::insert(&conn, &make_mem("a", "ns", MTier::Mid, "ai:alice")).expect("ins");
        db::insert(&conn, &make_mem("b", "ns", MTier::Mid, "ai:bob")).expect("ins");
        let out = handle_list(&conn, &json!({"agent_id": "ai:alice"})).expect("ok");
        assert_eq!(out["count"].as_u64(), Some(1));
    }

    // B. validation — bad agent_id format
    #[test]
    fn invalid_agent_id_rejected() {
        let conn = fresh_conn();
        let err = handle_list(&conn, &json!({"agent_id": "has space"})).unwrap_err();
        assert!(!err.is_empty(), "expected validation err, got {err}");
    }

    // limit overflow (saturating u64 → usize::MAX clamps to 200 cap)
    #[test]
    fn limit_overflow_saturates_and_caps() {
        let conn = fresh_conn();
        db::insert(&conn, &make_mem("a", "ns", MTier::Mid, "ai:a")).expect("ins");
        let out = handle_list(&conn, &json!({"limit": u64::MAX})).expect("ok");
        assert_eq!(out["count"].as_u64(), Some(1));
    }

    // E. idempotency
    #[test]
    fn idempotent_listing() {
        let conn = fresh_conn();
        db::insert(&conn, &make_mem("a", "ns", MTier::Mid, "ai:a")).expect("ins");
        let one = handle_list(&conn, &json!({"namespace": "ns"})).expect("ok");
        let two = handle_list(&conn, &json!({"namespace": "ns"})).expect("ok");
        assert_eq!(one["count"], two["count"]);
    }
}
