// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_reflect` handler.

use crate::db;
use crate::embeddings::Embedder;
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
    embedder: Option<&Embedder>,
    vector_index: Option<&VectorIndex>,
    mcp_client: Option<&str>,
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
    let outcome = match db::reflect(conn, &input) {
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
