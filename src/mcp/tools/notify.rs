// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_notify` and `memory_inbox` handlers.

use crate::models::{Memory, Tier};
use crate::{db, validate};
use serde_json::{Value, json};
pub(crate) fn handle_notify(
    conn: &rusqlite::Connection,
    params: &Value,
    resolved_ttl: &crate::config::ResolvedTtl,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let target = params["target_agent_id"]
        .as_str()
        .ok_or("target_agent_id is required")?;
    let title = params["title"].as_str().ok_or("title is required")?;
    let payload = params["payload"].as_str().ok_or("payload is required")?;
    // B4 (R2-LOW) — clamp instead of panic on out-of-range JSON; the
    // `.clamp(1, 10)` below enforces the semantic priority range, but
    // an i64 like `9_999_999_999` would have aborted the stdio MCP
    // server before the clamp ran.
    let priority = i32::try_from(params["priority"].as_i64().unwrap_or(5))
        .unwrap_or(i32::MAX)
        .clamp(1, 10);
    let tier_str = params["tier"].as_str().unwrap_or("mid");
    let tier = Tier::from_str(tier_str).ok_or(format!("invalid tier: {tier_str}"))?;

    validate::validate_agent_id(target).map_err(|e| e.to_string())?;
    validate::validate_title(title).map_err(|e| e.to_string())?;
    validate::validate_content(payload).map_err(|e| e.to_string())?;

    let sender = crate::identity::resolve_agent_id(None, mcp_client).map_err(|e| e.to_string())?;
    let namespace = super::agent::messages_namespace_for(target);

    let now = chrono::Utc::now();
    let expires_at = resolved_ttl
        .ttl_for_tier(&tier)
        .map(|s| (now + chrono::Duration::seconds(s)).to_rfc3339());

    let metadata = json!({
        "agent_id": sender.clone(),
        "recipient_agent_id": target,
        "message_kind": "notify",
    });

    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier,
        namespace: namespace.clone(),
        title: title.to_string(),
        content: payload.to_string(),
        tags: vec!["_message".to_string()],
        priority,
        confidence: 1.0,
        source: "notify".to_string(),
        access_count: 0,
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
        last_accessed_at: None,
        expires_at,
        metadata,
        reflection_depth: 0,
        memory_kind: crate::models::MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    let actual_id = db::insert(conn, &mem).map_err(|e| e.to_string())?;

    Ok(json!({
        "id": actual_id,
        "from": sender,
        "to": target,
        "namespace": namespace,
        "tier": mem.tier,
        "delivered_at": mem.created_at,
    }))
}

pub(crate) fn handle_inbox(
    conn: &rusqlite::Connection,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    // Caller identity is the default inbox owner — agents read their own
    // inbox unless an explicit agent_id is supplied.
    let explicit = params["agent_id"].as_str();
    let owner =
        crate::identity::resolve_agent_id(explicit, mcp_client).map_err(|e| e.to_string())?;
    let unread_only = params["unread_only"].as_bool().unwrap_or(false);
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(50))
        .unwrap_or(usize::MAX)
        .min(500);
    let namespace = super::agent::messages_namespace_for(&owner);
    let items = db::list(
        conn,
        Some(&namespace),
        None,
        limit,
        0,
        None,
        None,
        None,
        None,
        None,
    )
    .map_err(|e| e.to_string())?;
    let filtered: Vec<&Memory> = items
        .iter()
        .filter(|m| !unread_only || m.access_count == 0)
        .collect();
    let messages: Vec<Value> = filtered
        .iter()
        .map(|m| {
            let sender = m
                .metadata
                .get("agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            json!({
                "id": m.id,
                "from": sender,
                "title": m.title,
                "payload": m.content,
                "priority": m.priority,
                "tier": m.tier,
                "created_at": m.created_at,
                "read": m.access_count > 0,
                "access_count": m.access_count,
            })
        })
        .collect();
    Ok(json!({
        "agent_id": owner,
        "namespace": namespace,
        "count": messages.len(),
        "unread_only": unread_only,
        "messages": messages,
    }))
}

// --- v0.6.0.0 webhook subscriptions ---------------------------------------
