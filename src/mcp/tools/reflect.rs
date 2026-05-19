// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_reflect` handler.

use crate::db;
use crate::embeddings::Embed;
use crate::hnsw::VectorIndex;
use crate::models::{GovernedAction, Tier};
use serde_json::{Value, json};
use std::path::Path;

/// v0.7.0 recursive-learning Task 4/8 (issue #655) — handler for the
/// `memory_reflect` MCP tool.
///
/// Wraps [`db::reflect`] (the atomic substrate primitive) with MCP-shape
/// arg parsing, agent_id resolution, embedding generation (best effort),
/// and the post-write subscription dispatch. Returns the JSON envelope
/// `{id, reflection_depth, reflects_on, namespace}` documented in the
/// tool's input schema.
///
/// Errors are returned as plain strings (MCP convention). Substrate
/// errors are matched in arm-priority order so Task 5/8 can plug in the
/// `signed_events` audit emission against the `DepthExceeded` variant
/// without touching the happy-path code.

pub(super) fn handle_reflect(
    conn: &rusqlite::Connection,
    db_path: &Path,
    params: &Value,
    embedder: Option<&dyn Embed>,
    vector_index: Option<&VectorIndex>,
    mcp_client: Option<&str>,
    // Issue #815 — when `Some`, every `reflects_on` edge written by
    // this reflect call is signed with this keypair. When `None`
    // (operator hasn't generated a daemon keypair, or the caller is
    // a test harness without one) the edges land unsigned, matching
    // the pre-#815 behaviour. Same signature shape as `handle_link`
    // and `handle_persona_generate` use for the H2 link-signing
    // surface so the dispatcher in `mcp::mod` can pass through the
    // shared `active_keypair` argument verbatim.
    active_keypair: Option<&crate::identity::keypair::AgentKeypair>,
) -> Result<Value, String> {
    // ─── Argument parsing ───────────────────────────────────────────
    let source_ids_arr = params["source_ids"]
        .as_array()
        .ok_or("source_ids is required (array of memory IDs)")?;
    if source_ids_arr.is_empty() {
        return Err("source_ids cannot be empty".to_string());
    }
    let mut source_ids: Vec<String> = Vec::with_capacity(source_ids_arr.len());
    for (i, v) in source_ids_arr.iter().enumerate() {
        match v.as_str() {
            Some(s) => source_ids.push(s.to_string()),
            None => return Err(format!("source_ids[{i}] must be a string")),
        }
    }
    let title = params["title"]
        .as_str()
        .ok_or("title is required")?
        .to_string();
    let content = params["content"]
        .as_str()
        .ok_or("content is required")?
        .to_string();
    let tier_str = params["tier"].as_str().unwrap_or("mid");
    let tier = Tier::from_str(tier_str).ok_or(format!("invalid tier: {tier_str}"))?;
    let namespace = params["namespace"].as_str().map(str::to_string);
    let priority = i32::try_from(params["priority"].as_i64().unwrap_or(5)).unwrap_or(5);
    let confidence = params["confidence"].as_f64().unwrap_or(1.0);
    let tags: Vec<String> = params["tags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let metadata = if params["metadata"].is_object() {
        params["metadata"].clone()
    } else {
        serde_json::json!({})
    };

    // NHI: resolve agent_id via the same precedence chain memory_store
    // uses, so the reflection memory's `metadata.agent_id` is consistent
    // with regular stores.
    let explicit_agent_id = params["agent_id"]
        .as_str()
        .or_else(|| metadata.get("agent_id").and_then(serde_json::Value::as_str));
    let agent_id = crate::identity::resolve_agent_id(explicit_agent_id, mcp_client)
        .map_err(|e| e.to_string())?;

    let input = db::ReflectInput {
        source_ids,
        title: title.clone(),
        content: content.clone(),
        namespace,
        tier,
        tags,
        priority,
        confidence,
        source: "claude".to_string(),
        agent_id,
        metadata,
    };

    // ─── L1-8: require_approval_above_depth gate ────────────────────
    // Evaluated BEFORE the substrate write so we can intercept deep
    // reflections and queue a pending_actions row without writing a
    // partial reflection.  The gate fires only when the resolved
    // namespace chain carries a non-None `require_approval_above_depth`
    // threshold AND the proposed depth exceeds it.
    //
    // Implementation note: computing `new_depth` here mirrors step 3 of
    // `db::reflect_with_hooks` — we load the source memories to find the
    // max existing depth, add 1, then compare against the threshold.
    // This is intentionally a thin MCP-layer pre-check; the substrate
    // still enforces `max_reflection_depth` independently on the write
    // path, so the two gates compose: approval-above-depth fires first,
    // the substrate depth-cap fires second on the actual write.
    {
        let target_namespace = input.namespace.clone().or_else(|| {
            // Mirror the substrate default: first source's namespace.
            input
                .source_ids
                .first()
                .and_then(|id| db::get(conn, id).ok().flatten())
                .map(|m| m.namespace)
        });

        if let Some(ref ns) = target_namespace {
            // L1-8: read the approval threshold directly from the
            // namespace's governance metadata blob — avoids adding a
            // new field to the GovernancePolicy struct (which would
            // require updating every GovernancePolicy { … } literal).
            if let Some(threshold) = db::resolve_require_approval_above_depth(conn, ns) {
                // Compute proposed depth: max(source depths) + 1.
                let max_src_depth = input
                    .source_ids
                    .iter()
                    .filter_map(|id| db::get(conn, id).ok().flatten())
                    .map(|m| m.reflection_depth)
                    .max()
                    .unwrap_or(0);
                #[allow(clippy::cast_sign_loss)]
                let new_depth_u32: u32 = max_src_depth.max(0).saturating_add(1) as u32;

                if new_depth_u32 > threshold {
                    // Serialise enough of the input to reconstruct the
                    // call when the approver resolves the pending row.
                    let payload = json!({
                        "source_ids": input.source_ids,
                        "title": input.title,
                        "content": input.content,
                        "namespace": ns,
                        "tier": input.tier.as_str(),
                        "tags": input.tags,
                        "priority": input.priority,
                        "confidence": input.confidence,
                        "agent_id": input.agent_id,
                        "proposed_depth": new_depth_u32,
                    });
                    let pending_id = db::queue_pending_action(
                        conn,
                        GovernedAction::Reflect,
                        ns,
                        None,
                        &input.agent_id,
                        &payload,
                    )
                    .map_err(|e| e.to_string())?;
                    crate::subscriptions::dispatch_approval_requested(conn, &pending_id, db_path);
                    return Ok(json!({
                        "status": "pending",
                        "pending_id": pending_id,
                        "reason": "governance requires approval for reflections above depth threshold",
                        "action": "reflect",
                        "namespace": ns,
                        "proposed_depth": new_depth_u32,
                        "require_approval_above_depth": threshold,
                    }));
                }
            }
        }
    }

    // ─── Substrate write ────────────────────────────────────────────
    // Error mapping is deliberate: `DepthExceeded` is left as a distinct
    // string shape so Task 5/8 can match on the prefix when wiring the
    // `signed_events` audit emission (and so the HTTP layer can map it
    // back to the typed `MemoryError::ReflectionDepthExceeded` variant).
    //
    // v0.7.0 QW-1 — when the resolved namespace policy opts into
    // `auto_export_reflections_to_filesystem`, install the
    // `post_reflect` hook that deferred-spawns the markdown disk
    // write. The hook is a Box<dyn Fn> spawning std::thread::spawn,
    // so the response path stays as fast as the unhooked write.
    let hooks = {
        let target_ns = input.namespace.clone().or_else(|| {
            input
                .source_ids
                .first()
                .and_then(|id| db::get(conn, id).ok().flatten())
                .map(|m| m.namespace)
        });
        let auto_export = target_ns
            .as_deref()
            .and_then(|ns| db::resolve_governance_policy(conn, ns))
            .map(|p| p.effective_auto_export_reflections_to_filesystem())
            .unwrap_or(false);
        let mut h = if auto_export {
            crate::hooks::post_reflect::build_post_reflect_hook(
                db_path.to_path_buf(),
                crate::hooks::post_reflect::AutoExportConfig::default_for_home(),
            )
        } else {
            db::ReflectHooks::empty()
        };
        // Issue #815 — `build_post_reflect_hook` leaves `active_keypair`
        // None because signing is the handler's concern, not the
        // auto-export hook's. Plug the dispatcher-supplied keypair in
        // so the `create_link_signed` call inside
        // `storage::reflect_with_hooks` reaches the signed path.
        h.active_keypair = active_keypair;
        h
    };
    let outcome = match db::reflect_with_hooks(conn, &input, &hooks) {
        Ok(o) => o,
        Err(db::ReflectError::Validation(m)) => return Err(m),
        Err(db::ReflectError::SourceNotFound(id)) => {
            return Err(format!("source memory not found: {id}"));
        }
        Err(db::ReflectError::DepthExceeded {
            attempted,
            cap,
            namespace,
        }) => {
            // Stable error string shape — Task 5/8 will key its audit
            // emission off this refusal. Keep the structured triple
            // visible (attempted=N, cap=M, namespace='...') so the
            // log analyser doesn't need a regex.
            return Err(format!(
                "REFLECTION_DEPTH_EXCEEDED: reflection depth {attempted} would exceed \
                 namespace max_reflection_depth {cap} (namespace='{namespace}')"
            ));
        }
        Err(db::ReflectError::HookVeto { reason, code }) => {
            // v0.7.0 Task 6/8 — a pre_reflect hook callback returned
            // Deny, vetoing the reflection. The MCP handler today
            // does NOT register any in-substrate hooks (the MCP-side
            // hook chain wiring is G7+'s problem), so this arm is
            // currently unreachable on the MCP path. We surface a
            // stable error-string shape anyway so a future MCP-side
            // hook wire-in lands without churning this arm.
            return Err(format!("REFLECTION_HOOK_VETO (code={code}): {reason}"));
        }
        Err(db::ReflectError::Database(m)) => return Err(m),
    };

    // ─── Best-effort post-write side effects ────────────────────────
    // Generate + persist an embedding for the new reflection memory so
    // semantic recall can find it. Failure is logged, not fatal — the
    // memory is already committed.
    if let Some(emb) = embedder {
        let text = format!("{title} {content}");
        match emb.embed(&text) {
            Ok(embedding) => {
                if let Err(e) = db::set_embedding(conn, &outcome.id, &embedding) {
                    tracing::warn!(
                        "failed to store embedding for reflection {}: {}",
                        &outcome.id,
                        e
                    );
                }
                if let Some(idx) = vector_index {
                    idx.insert(outcome.id.clone(), embedding);
                }
            }
            Err(e) => {
                tracing::warn!(
                    "failed to generate embedding for reflection {}: {}",
                    &outcome.id,
                    e
                );
            }
        }
    }

    // Fire the standard `memory_store` webhook event so downstream
    // subscribers see the new memory the same way they would a direct
    // store. Task 6/8 will layer `pre_reflect` / `post_reflect` hook
    // events on top of this baseline.
    crate::subscriptions::dispatch_event(
        conn,
        "memory_store",
        &outcome.id,
        &outcome.namespace,
        Some(&input.agent_id),
        db_path,
    );

    Ok(json!({
        "id": outcome.id,
        "reflection_depth": outcome.reflection_depth,
        "reflects_on": outcome.reflects_on,
        "namespace": outcome.namespace,
    }))
}

#[cfg(test)]
mod tests {
    //! Coverage C-2 — focused tests for `handle_reflect`.
    //!
    //! Areas covered:
    //! - argument parsing edge cases (empty array, non-string entry)
    //! - error mapping: SourceNotFound, Validation, DepthExceeded
    //! - happy path with mock embedder (post-write embedding store)
    //! - happy path without embedder (no-op for embedding side effect)

    use super::*;
    use crate::embeddings::test_support::MockEmbedder;
    use crate::models::{Memory, MemoryKind};
    use crate::storage as db;
    use serde_json::json;

    fn fresh_db() -> (rusqlite::Connection, tempfile::NamedTempFile) {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let conn = db::open(tmp.path()).expect("db::open");
        (conn, tmp)
    }

    fn seed_observation(conn: &rusqlite::Connection, ns: &str, title: &str) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: format!("body for {title}"),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({"agent_id": "ai:test"}),
            reflection_depth: 0,
            memory_kind: MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
            confidence_source: crate::models::ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
            version: 1,
        };
        db::insert(conn, &mem).expect("insert")
    }

    // Validation: source_ids missing.
    #[test]
    fn missing_source_ids_errors() {
        let (conn, tmp) = fresh_db();
        let err = handle_reflect(
            &conn,
            tmp.path(),
            &json!({"title": "t", "content": "c"}),
            None,
            None,
            None,
            None, // active_keypair — #815 regression coverage uses the dedicated mcp/mod.rs test
        )
        .unwrap_err();
        assert!(err.contains("source_ids"), "got: {err}");
    }

    // Validation: empty source_ids.
    #[test]
    fn empty_source_ids_errors() {
        let (conn, tmp) = fresh_db();
        let err = handle_reflect(
            &conn,
            tmp.path(),
            &json!({"source_ids": [], "title": "t", "content": "c"}),
            None,
            None,
            None,
            None, // active_keypair — #815 regression coverage uses the dedicated mcp/mod.rs test
        )
        .unwrap_err();
        assert!(err.contains("empty"), "got: {err}");
    }

    // Validation: non-string source_id entry.
    #[test]
    fn non_string_source_id_errors() {
        let (conn, tmp) = fresh_db();
        let err = handle_reflect(
            &conn,
            tmp.path(),
            &json!({"source_ids": ["ok", 42], "title": "t", "content": "c"}),
            None,
            None,
            None,
            None, // active_keypair — #815 regression coverage uses the dedicated mcp/mod.rs test
        )
        .unwrap_err();
        assert!(err.contains("must be a string"), "got: {err}");
    }

    // Validation: missing title.
    #[test]
    fn missing_title_errors() {
        let (conn, tmp) = fresh_db();
        let err = handle_reflect(
            &conn,
            tmp.path(),
            &json!({"source_ids": ["x"], "content": "c"}),
            None,
            None,
            None,
            None, // active_keypair — #815 regression coverage uses the dedicated mcp/mod.rs test
        )
        .unwrap_err();
        assert!(err.contains("title"), "got: {err}");
    }

    // Validation: missing content.
    #[test]
    fn missing_content_errors() {
        let (conn, tmp) = fresh_db();
        let err = handle_reflect(
            &conn,
            tmp.path(),
            &json!({"source_ids": ["x"], "title": "t"}),
            None,
            None,
            None,
            None, // active_keypair — #815 regression coverage uses the dedicated mcp/mod.rs test
        )
        .unwrap_err();
        assert!(err.contains("content"), "got: {err}");
    }

    // Validation: invalid tier.
    #[test]
    fn invalid_tier_errors() {
        let (conn, tmp) = fresh_db();
        let err = handle_reflect(
            &conn,
            tmp.path(),
            &json!({"source_ids": ["x"], "title": "t", "content": "c", "tier": "bogus"}),
            None,
            None,
            None,
            None, // active_keypair — #815 regression coverage uses the dedicated mcp/mod.rs test
        )
        .unwrap_err();
        assert!(err.contains("invalid tier"), "got: {err}");
    }

    // SourceNotFound: source id not in DB.
    #[test]
    fn source_not_found_errors() {
        let (conn, tmp) = fresh_db();
        let err = handle_reflect(
            &conn,
            tmp.path(),
            &json!({
                "source_ids": ["11111111-2222-3333-4444-555555555555"],
                "title": "t",
                "content": "c",
            }),
            None,
            None,
            None,
            None, // active_keypair — #815 regression coverage uses the dedicated mcp/mod.rs test
        )
        .unwrap_err();
        assert!(err.contains("source memory not found"), "got: {err}");
    }

    // Happy path without embedder — substrate write succeeds.
    #[test]
    fn happy_path_without_embedder() {
        let (conn, tmp) = fresh_db();
        let src = seed_observation(&conn, "rfl-ns", "obs");
        let resp = handle_reflect(
            &conn,
            tmp.path(),
            &json!({
                "source_ids": [src],
                "title": "reflection",
                "content": "I see the observation",
            }),
            None,
            None,
            None,
            None, // active_keypair — #815 regression coverage uses the dedicated mcp/mod.rs test
        )
        .expect("ok");
        assert!(resp["id"].is_string());
        assert_eq!(resp["reflection_depth"].as_i64(), Some(1));
        assert_eq!(resp["namespace"].as_str(), Some("rfl-ns"));
    }

    // Happy path with embedder — embedding stored on the reflection memory.
    #[test]
    fn happy_path_with_embedder_stores_embedding() {
        let (conn, tmp) = fresh_db();
        let src = seed_observation(&conn, "rfl-emb", "obs");
        let emb = MockEmbedder::new_local().unwrap();
        let resp = handle_reflect(
            &conn,
            tmp.path(),
            &json!({
                "source_ids": [src],
                "title": "t",
                "content": "c",
            }),
            Some(&emb),
            None,
            None,
            None, // active_keypair — #815 regression coverage uses the dedicated mcp/mod.rs test
        )
        .expect("ok");
        let new_id = resp["id"].as_str().unwrap();
        // Embedding column populated on the new reflection.
        let has_emb: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE id = ?1 AND embedding IS NOT NULL",
                rusqlite::params![new_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert_eq!(has_emb, 1, "embedding must be set");
    }

    // Approval-gate path: governance threshold triggers the K10 pending queue.
    #[test]
    fn approval_gate_above_depth_queues_pending() {
        let (conn, tmp) = fresh_db();
        let src = seed_observation(&conn, "rfl-gate", "obs");
        // Seed a namespace_meta row + standard memory with governance
        // setting `max_reflection_depth: 5` (compiled default) and
        // `require_approval_above_depth: 0` so a depth=1 reflection
        // immediately falls above the threshold.
        let std_mem_id = seed_observation(&conn, "rfl-gate", "std");
        // Manually patch metadata.governance with require_approval_above_depth=0
        let gov_metadata = json!({
            "governance": {
                "write": "any",
                "require_approval_above_depth": 0,
            },
        });
        conn.execute(
            "UPDATE memories SET metadata = json(?1) WHERE id = ?2",
            rusqlite::params![gov_metadata.to_string(), &std_mem_id],
        )
        .unwrap();
        db::set_namespace_standard(&conn, "rfl-gate", &std_mem_id, None).unwrap();
        let resp = handle_reflect(
            &conn,
            tmp.path(),
            &json!({
                "source_ids": [src],
                "title": "t",
                "content": "c",
                "namespace": "rfl-gate",
            }),
            None,
            None,
            None,
            None, // active_keypair — #815 regression coverage uses the dedicated mcp/mod.rs test
        )
        .expect("ok");
        // Approval gate fires before substrate write.
        assert_eq!(resp["status"].as_str(), Some("pending"));
        assert!(resp["pending_id"].is_string());
    }
}
