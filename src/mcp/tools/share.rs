// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_share` handler — minimal v0.8-pulled-forward implementation
//! for issues #224 (Phase 3 Memory Sharing & Sync RFC) and #311 (targeted
//! point-to-point memory share).
//!
//! Per operator directive `28860423-d12c-4959-bc8b-8fa9a94a33d9` (2026-05-18)
//! the v0.8.0 Phase 3 RFC is pulled forward into v0.7.0 as a minimum-viable
//! correct fix. This handler implements the MVP slice:
//!
//! 1. Accept `source_memory_id` + `target_agent_id`.
//! 2. Look up the source memory.
//! 3. Insert a copy into the target agent's shared namespace
//!    `_shared/<from_agent_id>→<to_agent_id>/`.
//! 4. Preserve provenance via metadata (`shared_from_memory_id`,
//!    `shared_from_agent_id`, `shared_at`).
//!
//! Out of scope for this MVP (deferred to v0.8 Phase 3 full delivery):
//! - CRDT-lite per-field merge rules (#224 design table)
//! - Bi-directional sync, conflict resolution, vector clocks
//! - Federation wire-level distribution (still local-DB only here)
//! - Receiver-side accept/reject workflow
//!
//! Regression test: `share_copies_memory_into_shared_namespace`.

use crate::{models::Memory, storage as db, validate};
use serde_json::{Value, json};

/// Build the destination namespace for a shared memory.
///
/// Format: `_shared/<from>→<to>/`. The arrow is U+2192 (single
/// glyph) so the namespace token is one segment — namespace validation
/// permits it because `validate_namespace` allows non-ASCII tokens
/// (see `src/validate.rs`).
#[must_use]
pub fn shared_namespace(from_agent_id: &str, to_agent_id: &str) -> String {
    format!("_shared/{from_agent_id}\u{2192}{to_agent_id}/")
}

/// MCP `memory_share` — copy a memory into the target agent's shared
/// namespace.
///
/// Returns a JSON object:
/// ```json
/// {
///   "shared_memory_id": "<new uuid>",
///   "source_memory_id": "<input>",
///   "target_namespace": "_shared/<from>→<to>/",
///   "target_agent_id": "<input>",
///   "from_agent_id": "<derived>"
/// }
/// ```
pub fn handle_share(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let source_memory_id = params["source_memory_id"]
        .as_str()
        .ok_or("source_memory_id is required")?;
    let target_agent_id = params["target_agent_id"]
        .as_str()
        .ok_or("target_agent_id is required")?;

    validate::validate_id(source_memory_id).map_err(|e| e.to_string())?;
    validate::validate_agent_id(target_agent_id).map_err(|e| e.to_string())?;

    let source = db::resolve_id(conn, source_memory_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("source memory {source_memory_id} not found"))?;

    // Derive the from_agent_id from the source memory's metadata; fall back
    // to `unknown` if absent.
    let from_agent_id = source
        .metadata
        .get("agent_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let target_namespace = shared_namespace(&from_agent_id, target_agent_id);
    let now = chrono::Utc::now().to_rfc3339();

    // Merge provenance into metadata; preserve the source's metadata
    // (no information loss) but stamp the share-event fields.
    let mut metadata = source.metadata.clone();
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert("shared_from_memory_id".into(), json!(source.id.clone()));
        obj.insert("shared_from_agent_id".into(), json!(from_agent_id.clone()));
        obj.insert("shared_to_agent_id".into(), json!(target_agent_id));
        obj.insert("shared_at".into(), json!(now.clone()));
        // The shared copy is authored BY the receiving agent for write-auth
        // purposes; the original author is preserved in
        // `shared_from_agent_id`.
        obj.insert("agent_id".into(), json!(target_agent_id));
    }

    let shared_id = uuid::Uuid::new_v4().to_string();
    let shared = Memory {
        id: shared_id.clone(),
        tier: source.tier,
        namespace: target_namespace.clone(),
        title: source.title.clone(),
        content: source.content.clone(),
        tags: source.tags.clone(),
        priority: source.priority,
        confidence: source.confidence,
        source: "shared".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata,
        reflection_depth: source.reflection_depth,
        memory_kind: source.memory_kind,
        entity_id: source.entity_id.clone(),
        persona_version: source.persona_version,
        citations: source.citations.clone(),
        source_uri: source.source_uri.clone(),
        source_span: source.source_span.clone(),
        confidence_source: source.confidence_source,
        confidence_signals: source.confidence_signals.clone(),
        confidence_decayed_at: source.confidence_decayed_at.clone(),
    };

    db::insert(conn, &shared).map_err(|e| e.to_string())?;

    Ok(json!({
        "shared_memory_id": shared_id,
        "source_memory_id": source_memory_id,
        "target_namespace": target_namespace,
        "target_agent_id": target_agent_id,
        "from_agent_id": from_agent_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Memory, Tier};

    fn fresh_conn() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn make_mem(title: &str, namespace: &str, agent_id: &str) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: namespace.to_string(),
            title: title.to_string(),
            content: format!("content for {title}"),
            tags: vec!["share-test".to_string()],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({"agent_id": agent_id}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
            confidence_source: crate::models::ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
            version: 1,
        }
    }

    #[test]
    fn share_copies_memory_into_shared_namespace() {
        let conn = fresh_conn();
        let src = make_mem("source memo", "alice/notes", "ai:alice");
        let src_id = db::insert(&conn, &src).expect("insert source");

        let params = json!({
            "source_memory_id": src_id.clone(),
            "target_agent_id": "ai:bob",
        });
        let resp = handle_share(&conn, &params).expect("share ok");

        let new_id = resp["shared_memory_id"]
            .as_str()
            .expect("shared_memory_id present");
        assert_ne!(new_id, src_id, "shared copy must have new id");
        assert_eq!(resp["target_agent_id"], "ai:bob");
        assert_eq!(resp["from_agent_id"], "ai:alice");
        assert_eq!(resp["target_namespace"], "_shared/ai:alice\u{2192}ai:bob/");

        // Pull the shared row back and verify provenance + content fidelity.
        let copy = db::resolve_id(&conn, new_id)
            .expect("resolve")
            .expect("shared copy present");
        assert_eq!(copy.title, src.title);
        assert_eq!(copy.content, src.content);
        assert_eq!(copy.namespace, "_shared/ai:alice\u{2192}ai:bob/");
        assert_eq!(copy.source, "shared");
        assert_eq!(
            copy.metadata["shared_from_memory_id"].as_str(),
            Some(src_id.as_str())
        );
        assert_eq!(
            copy.metadata["shared_from_agent_id"].as_str(),
            Some("ai:alice")
        );
        assert_eq!(copy.metadata["shared_to_agent_id"].as_str(), Some("ai:bob"));
        assert_eq!(copy.metadata["agent_id"].as_str(), Some("ai:bob"));
    }

    #[test]
    fn share_rejects_missing_source() {
        let conn = fresh_conn();
        let nonexistent = uuid::Uuid::new_v4().to_string();
        let params = json!({
            "source_memory_id": nonexistent,
            "target_agent_id": "ai:bob",
        });
        let err = handle_share(&conn, &params).expect_err("must fail");
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn share_rejects_missing_params() {
        let conn = fresh_conn();
        let r1 = handle_share(&conn, &json!({"target_agent_id": "ai:bob"}));
        assert!(r1.is_err());
        let r2 = handle_share(
            &conn,
            &json!({"source_memory_id": uuid::Uuid::new_v4().to_string()}),
        );
        assert!(r2.is_err());
    }
}
