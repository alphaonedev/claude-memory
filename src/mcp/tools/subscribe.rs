// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP subscription management handlers.

use serde_json::{Value, json};
pub(super) fn handle_subscribe(
    conn: &rusqlite::Connection,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let url = params["url"].as_str().ok_or("url is required")?;
    let events = params["events"].as_str().unwrap_or("*");
    let secret = params["secret"].as_str();
    let namespace_filter = params["namespace_filter"].as_str();
    let agent_filter = params["agent_filter"].as_str();
    let created_by =
        crate::identity::resolve_agent_id(None, mcp_client).map_err(|e| e.to_string())?;

    // R3-S1.HMAC (v0.7.0 fix campaign 2026-05-13): refuse subscription
    // registration when neither a per-subscription `secret` nor a
    // server-wide `[hooks.subscription] hmac_secret` is configured.
    // Mirrors the HTTP subscribe handler — see
    // `crate::handlers::subscribe` for the rationale.
    if secret.is_none_or(str::is_empty) && crate::config::active_hooks_hmac_secret().is_none() {
        return Err(
            "HMAC secret required: configure per-subscription `hmac_secret` or \
             server-wide `[security] hmac_secret`. Pass `secret: <value>` in the \
             tool call, OR set [hooks.subscription] hmac_secret in the daemon \
             config. Unsigned subscription dispatch was disabled in v0.7.0 \
             (fix campaign R3-S1.HMAC, 2026-05-13)."
                .to_string(),
        );
    }

    // P5 (G9): optional structured per-event-type opt-in. Callers pass
    // `event_types: ["memory_store", "memory_link_created"]` to scope a
    // subscription to a narrow event subset. When omitted, the legacy
    // `events` (comma-separated / `*`) field governs — preserves
    // backward compatibility for pre-P5 subscribers.
    let event_types: Option<Vec<String>> = params["event_types"].as_array().map(|arr| {
        arr.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    });

    // Require the caller to be a registered agent (#301 item 4).
    // MCP stdio is single-tenant per process, but the same tool set is
    // exposed on the HTTP daemon where a caller might not be attested.
    // Registration in `_agents` is cheap (single memory_agent_register
    // call) and provides an audit trail; refusing unregistered
    // subscribers closes the "any MCP client owns the webhook fleet"
    // hole flagged by the v0.6.0 security review.
    let registered = crate::db::list_agents(conn)
        .map_err(|e| e.to_string())?
        .into_iter()
        .any(|a| a.agent_id == created_by);
    if !registered {
        return Err(format!(
            "agent {created_by:?} is not registered; call memory_agent_register before memory_subscribe"
        ));
    }

    crate::subscriptions::validate_url(url).map_err(|e| e.to_string())?;

    let id = crate::subscriptions::insert(
        conn,
        &crate::subscriptions::NewSubscription {
            url,
            events,
            secret,
            namespace_filter,
            agent_filter,
            created_by: Some(&created_by),
            event_types: event_types.as_deref(),
        },
    )
    .map_err(|e| e.to_string())?;

    let mut response = json!({
        "id": id,
        "url": url,
        "events": events,
        "namespace_filter": namespace_filter,
        "agent_filter": agent_filter,
        "created_by": created_by,
    });
    if let Some(et) = &event_types {
        response["event_types"] = json!(et);
    }
    Ok(response)
}

pub(crate) fn handle_unsubscribe(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    let removed = crate::subscriptions::delete(conn, id).map_err(|e| e.to_string())?;
    Ok(json!({"id": id, "removed": removed}))
}

pub(super) fn handle_list_subscriptions(conn: &rusqlite::Connection) -> Result<Value, String> {
    let subs = crate::subscriptions::list(conn).map_err(|e| e.to_string())?;
    Ok(json!({"count": subs.len(), "subscriptions": subs}))
}

/// v0.7 K7 — MCP handler for `memory_subscription_replay`. Thin
/// wrapper around [`crate::subscriptions::memory_subscription_replay`]
/// that exposes the operator/governance reliability tool over the
/// MCP wire. Family: `Power` (operator-scoped, not data-plane).
pub(super) fn handle_subscription_replay(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let subscription_id = params["subscription_id"]
        .as_str()
        .ok_or("subscription_id is required")?;
    let since = params["since"]
        .as_str()
        .ok_or("since is required (RFC3339)")?;
    crate::subscriptions::memory_subscription_replay(conn, subscription_id, since)
        .map_err(|e| e.to_string())
}
