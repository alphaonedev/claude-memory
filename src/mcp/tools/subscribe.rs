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
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    // Cross-tenant authorization (#870, security-high, 2026-05-18):
    // scope the DELETE to the caller's resolved agent_id. Without this
    // any tenant could enumerate ids (via lucky guess or by exfiltrating
    // another tenant's list output) and remove the other tenant's
    // webhook fleet. The resolution chain matches `handle_subscribe`.
    let caller = crate::identity::resolve_agent_id(None, mcp_client).map_err(|e| e.to_string())?;
    let removed =
        crate::subscriptions::delete(conn, id, Some(&caller)).map_err(|e| e.to_string())?;
    Ok(json!({"id": id, "removed": removed}))
}

pub(super) fn handle_list_subscriptions(
    conn: &rusqlite::Connection,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    // Cross-tenant authorization (#872, security-high, 2026-05-18):
    // only return subscriptions owned by the caller. Pre-fix this
    // returned every tenant's rows.
    let caller = crate::identity::resolve_agent_id(None, mcp_client).map_err(|e| e.to_string())?;
    let subs = crate::subscriptions::list(conn, Some(&caller)).map_err(|e| e.to_string())?;
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

#[cfg(test)]
mod tests {
    //! Coverage C-2 — focused tests for `handle_subscribe`,
    //! `handle_unsubscribe`, `handle_list_subscriptions`, and
    //! `handle_subscription_replay`.

    use super::*;
    use crate::storage as db;
    use serde_json::json;

    fn fresh_conn() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn register_agent(conn: &rusqlite::Connection) -> String {
        // Resolve the agent_id the handler will pick (None override, None mcp_client)
        // so `subscribe`'s registry check finds the row.
        let agent_id = crate::identity::resolve_agent_id(None, None).unwrap();
        db::register_agent(conn, &agent_id, "test", &[]).expect("register");
        agent_id
    }

    // R3-S1.HMAC: no per-subscription secret AND no server-wide secret → refusal.
    #[test]
    fn no_secret_refuses_unsigned() {
        // Belt-and-braces: ensure no global secret is set.
        crate::config::set_active_hooks_hmac_secret(None);
        let conn = fresh_conn();
        let _ = register_agent(&conn);
        let err = handle_subscribe(
            &conn,
            &json!({"url": "https://example.com/hook", "events": "*"}),
            None,
        )
        .unwrap_err();
        assert!(err.contains("HMAC secret required"), "got: {err}");
    }

    // Per-subscription secret allowed → registration proceeds.
    #[test]
    fn per_subscription_secret_accepted() {
        crate::config::set_active_hooks_hmac_secret(None);
        let conn = fresh_conn();
        let _ = register_agent(&conn);
        let resp = handle_subscribe(
            &conn,
            &json!({
                "url": "https://example.com/hook",
                "events": "memory_store",
                "secret": "shared-secret-hex",
            }),
            None,
        )
        .expect("ok");
        assert!(resp["id"].is_string());
        assert_eq!(resp["url"].as_str(), Some("https://example.com/hook"));
        assert_eq!(resp["events"].as_str(), Some("memory_store"));
    }

    // event_types array — structured per-event-type opt-in echoed in response.
    #[test]
    fn event_types_array_propagated() {
        crate::config::set_active_hooks_hmac_secret(None);
        let conn = fresh_conn();
        let _ = register_agent(&conn);
        let resp = handle_subscribe(
            &conn,
            &json!({
                "url": "https://example.com/hook",
                "secret": "shared-secret-hex",
                "event_types": ["memory_store", "memory_link_created"],
            }),
            None,
        )
        .expect("ok");
        let arr = resp["event_types"].as_array().expect("array");
        assert_eq!(arr.len(), 2);
    }

    // Missing url → typed error.
    #[test]
    fn missing_url_errors() {
        crate::config::set_active_hooks_hmac_secret(None);
        let conn = fresh_conn();
        let _ = register_agent(&conn);
        let err = handle_subscribe(&conn, &json!({"secret": "s"}), None).unwrap_err();
        assert!(err.contains("url"), "got: {err}");
    }

    // Unregistered agent refused.
    #[test]
    fn unregistered_agent_refused() {
        crate::config::set_active_hooks_hmac_secret(None);
        let conn = fresh_conn();
        // NB: did not call register_agent
        let err = handle_subscribe(
            &conn,
            &json!({"url": "https://example.com/hook", "secret": "s"}),
            None,
        )
        .unwrap_err();
        assert!(err.contains("not registered"), "got: {err}");
    }

    // Invalid URL rejected by validate_url.
    #[test]
    fn invalid_url_rejected() {
        crate::config::set_active_hooks_hmac_secret(None);
        let conn = fresh_conn();
        let _ = register_agent(&conn);
        let err =
            handle_subscribe(&conn, &json!({"url": "not-a-url", "secret": "s"}), None).unwrap_err();
        assert!(!err.is_empty());
    }

    // handle_unsubscribe — unknown id returns removed: false (no error).
    #[test]
    fn unsubscribe_unknown_id_returns_false() {
        let conn = fresh_conn();
        let resp = handle_unsubscribe(
            &conn,
            &json!({"id": "00000000-0000-0000-0000-000000000000"}),
            None,
        )
        .expect("ok");
        assert_eq!(resp["removed"], false);
    }

    // handle_unsubscribe — missing id errors.
    #[test]
    fn unsubscribe_missing_id_errors() {
        let conn = fresh_conn();
        let err = handle_unsubscribe(&conn, &json!({}), None).unwrap_err();
        assert!(err.contains("id"), "got: {err}");
    }

    // handle_list_subscriptions — empty DB returns count=0.
    #[test]
    fn list_subscriptions_empty() {
        let conn = fresh_conn();
        let resp = handle_list_subscriptions(&conn, None).expect("ok");
        assert_eq!(resp["count"].as_u64(), Some(0));
    }

    // handle_subscription_replay — missing fields error.
    #[test]
    fn subscription_replay_missing_id_errors() {
        let conn = fresh_conn();
        let err = handle_subscription_replay(&conn, &json!({"since": "2026-01-01T00:00:00Z"}))
            .unwrap_err();
        assert!(err.contains("subscription_id"), "got: {err}");
    }

    #[test]
    fn subscription_replay_missing_since_errors() {
        let conn = fresh_conn();
        let err =
            handle_subscription_replay(&conn, &json!({"subscription_id": "sub-1"})).unwrap_err();
        assert!(err.contains("since"), "got: {err}");
    }
}
