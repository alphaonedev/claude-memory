// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Notify / subscribe / unsubscribe / list_subscriptions HTTP handlers.
//!
//! Extracted from [`super::hook_subscribers`] under issue #650 (handler
//! cap ≤1200 LOC). Handler bodies are unchanged; only the module surface
//! moved. Wire compatibility preserved via `pub use subscriptions::*` in
//! [`super`].

#![allow(clippy::too_many_lines)]

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::json;

use crate::db;
#[cfg(feature = "sal")]
use crate::models::Tier;

use super::AppState;
#[cfg(feature = "sal")]
use super::StorageBackend;
#[cfg(feature = "sal")]
use super::store_err_to_response;
use super::{fanout_or_503, resolve_caller_agent_id};

// --- /api/v1/notify (POST) + /api/v1/inbox (GET) ---------------------------

#[derive(Deserialize)]
pub struct NotifyBody {
    pub target_agent_id: String,
    pub title: String,
    /// Accept either `payload` (MCP tool name) or `content` (S32 scenario).
    #[serde(default)]
    pub payload: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub tier: Option<String>,
    /// Optional explicit sender id — falls back to `X-Agent-Id` header.
    #[serde(default)]
    pub agent_id: Option<String>,
}

pub async fn notify(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<NotifyBody>,
) -> impl IntoResponse {
    let Some(payload) = body.payload.or(body.content) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "payload or content is required"})),
        )
            .into_response();
    };
    let sender = match resolve_caller_agent_id(body.agent_id.as_deref(), &headers, None) {
        Ok(id) => id,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    };

    // v0.7.0 fold-A2A1.1 (#700, F-A2A1.1) — postgres-backed daemons
    // route through the SAL `notify` trait method AND fan the resulting
    // inbox memory out to peers via the same quorum-write contract the
    // sqlite branch already uses below. Federation fanout is now backend-
    // blind: `broadcast_store_quorum` takes a `Memory` + `FederationConfig`
    // and HTTP-POSTs to each peer's `sync_push` regardless of where the
    // local row was persisted. Cross-namespace subscription dispatch
    // is achieved by writing the subscription memory itself through the
    // shared store (see `subscribe` below) so subscribers on every peer
    // see the same `_subscriptions/<aid>` namespace.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let priority_i32 = body.priority.and_then(|p| i32::try_from(p).ok());
        let resolved_tier = match body.tier.as_deref() {
            Some("short") => Some(Tier::Short),
            Some("mid") => Some(Tier::Mid),
            Some("long") => Some(Tier::Long),
            _ => None,
        };
        let ctx = crate::store::CallerContext::for_agent(&sender);
        let new_id = match app
            .store
            .notify(
                &ctx,
                &body.target_agent_id,
                &body.title,
                &payload,
                priority_i32,
                resolved_tier.as_ref(),
            )
            .await
        {
            Ok(id) => id,
            Err(e) => return store_err_to_response(e),
        };
        // Re-fetch the just-written inbox memory so we can hand the full
        // wire-shape (id + metadata + namespace + ts) to the peers via
        // `broadcast_store_quorum`. The trait `notify()` returns only
        // the id; the row materialised on disk is what peers need to
        // mirror so the recipient's `GET /inbox` against any cluster
        // member returns the same row.
        let fanout_mem = match app.store.get(&ctx, &new_id).await {
            Ok(m) => Some(m),
            Err(e) => {
                tracing::warn!(
                    "postgres notify: refetch for fanout failed for {new_id}: {e:?} \
                     (local commit landed; sync-daemon will catch peers up)"
                );
                None
            }
        };
        if let Some(mem) = fanout_mem.as_ref()
            && let Some(resp) = fanout_or_503(&app, mem).await
        {
            return resp;
        }
        return (
            StatusCode::CREATED,
            Json(json!({
                "id": new_id,
                "target_agent_id": body.target_agent_id,
                "namespace": format!("_inbox/{}", body.target_agent_id),
                "storage_backend": "postgres",
            })),
        )
            .into_response();
    }

    let mut params = json!({
        "target_agent_id": body.target_agent_id,
        "title": body.title,
        "payload": payload,
    });
    if let Some(p) = body.priority {
        params["priority"] = json!(p);
    }
    if let Some(t) = body.tier {
        params["tier"] = json!(t);
    }

    let lock = app.db.lock().await;
    let resolved_ttl = lock.2.clone();
    // Route via the MCP handler so the wire contract stays single-sourced.
    // `mcp_client = Some(&sender)` makes `resolve_agent_id(None, _)` return
    // the caller-resolved HTTP id — same effective provenance.
    let mcp_client = sender.clone();
    let result = crate::mcp::handle_notify(&lock.0, &params, &resolved_ttl, Some(&mcp_client));

    // v0.6.2 (S32): capture the just-inserted notify row and fan it out to
    // peers. Without this, alice's notify on node-1 lands in bob's inbox on
    // node-1 only — when bob polls `/api/v1/inbox` against node-2 he sees
    // nothing. The HTTP wrapper bypassed the `create_memory` fanout path
    // that every other `db::insert` write uses, so we wire it here with the
    // same posture as `fanout_or_503`: on quorum miss return 503; on a
    // network error, swallow (local commit landed, sync-daemon catches up).
    let fanout_mem = match &result {
        Ok(v) => v
            .get("id")
            .and_then(|x| x.as_str())
            .and_then(|id| db::get(&lock.0, id).ok().flatten()),
        Err(_) => None,
    };
    drop(lock);

    match result {
        Ok(v) => {
            if let Some(mem) = fanout_mem
                && let Some(resp) = fanout_or_503(&app, &mem).await
            {
                return resp;
            }
            (StatusCode::CREATED, Json(v)).into_response()
        }
        // Issue #851: `mcp::handle_notify` returns Result<_, String> where
        // the inner string can include raw rusqlite text from
        // db::insert(...).map_err(|e| e.to_string()). Sanitize via the
        // standard bad_request_opaque helper.
        Err(e) => super::bad_request_opaque("notify handler error", &e),
    }
}
// --- /api/v1/subscriptions (POST / DELETE / GET) ---------------------------
//
// Two shapes are supported. The webhook shape from the MCP tool
// (`{url, events, secret, namespace_filter, agent_filter}`) is the primary
// contract. Scenario S33 uses a lighter shape (`{agent_id, namespace}`) to
// express "subscribe this agent to a namespace". We accept both: when a
// namespace is supplied without a URL we synthesize an internal loopback URL
// (`http://localhost/_ns/<agent_id>/<namespace>`) that passes SSRF validation
// and sets `agent_filter`/`namespace_filter` accordingly. This lets S33 round-
// trip without needing a separate subscriptions table.

#[derive(Deserialize)]
pub struct SubscribeBody {
    /// Webhook URL — required for the MCP contract, optional for the S33
    /// namespace-subscription shape.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub events: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub namespace_filter: Option<String>,
    #[serde(default)]
    pub agent_filter: Option<String>,
    /// S33 shape: caller-supplied namespace to track.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Optional explicit subscriber id.
    #[serde(default)]
    pub agent_id: Option<String>,
}

pub async fn subscribe(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SubscribeBody>,
) -> impl IntoResponse {
    let caller = match resolve_caller_agent_id(body.agent_id.as_deref(), &headers, None) {
        Ok(id) => id,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    };

    // R3-S1.HMAC (v0.7.0 fix campaign 2026-05-13): refuse to register a
    // subscription when neither a per-subscription `secret` nor a
    // server-wide `[hooks.subscription] hmac_secret` is configured.
    // Previously the dispatch loop silently delivered unsigned bodies
    // when no key was available (subscriptions.rs:600-606), which
    // overstates the "HMAC non-optional" guarantee documented for
    // Bucket-3 receivers. This is a deliberate behaviour break:
    // operators upgrading from <=v0.6 must either supply a per-sub
    // secret or configure the process-wide override before
    // subscribing.
    if body.secret.as_deref().is_none_or(str::is_empty)
        && crate::config::active_hooks_hmac_secret().is_none()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "HMAC secret required: configure per-subscription `hmac_secret` or server-wide `[security] hmac_secret`",
                "hint": "Pass `secret: <value>` in the subscribe request body, OR set [hooks.subscription] hmac_secret in the daemon config. \
                        Unsigned subscription dispatch was disabled in v0.7.0 (fix campaign R3-S1.HMAC, 2026-05-13)."
            })),
        )
            .into_response();
    }

    // Rewrite S33's `{agent_id, namespace}` body into the webhook shape.
    let mut url_was_synthesized = false;
    // Suppress dead-code lint when sal feature is off (the variable is
    // only consulted inside the postgres-dispatch branch below).
    let _ = &url_was_synthesized;
    let (url, namespace_filter, agent_filter) = if let Some(u) = body.url {
        (u, body.namespace_filter, body.agent_filter)
    } else {
        let Some(ns) = body.namespace.clone() else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "url or namespace is required"})),
            )
                .into_response();
        };
        // Synthetic loopback URL — never dispatched (the postgres
        // persistence path doesn't run the webhook loop), serves only
        // to round-trip the (agent_id, namespace) pair through the
        // wire shape. We mark it so the SSRF guard can skip the
        // loopback rejection — H11's allow_loopback_webhooks knob
        // gates real callers, not internally-synthesized stubs.
        // The assignment is unused under default features (the reader
        // is `#[cfg(feature = "sal")]`-gated); allow the unused-assignment
        // warning specifically.
        #[allow(unused_assignments)]
        {
            url_was_synthesized = true;
        }
        let synthetic = format!("http://localhost/_ns/{caller}/{ns}");
        (
            synthetic,
            Some(ns),
            body.agent_filter.or_else(|| Some(caller.clone())),
        )
    };

    let events = body.events.unwrap_or_else(|| "*".to_string());

    // v0.7.0 fold-A2A1.1 (#700, F-A2A1.1) — postgres-backed daemons
    // persist subscriptions as memories under `_subscriptions/<agent_id>`
    // AND fan the subscription memory out to peers via the same quorum
    // contract the sqlite branch uses for `_agents` rows. This is what
    // makes K7-style cross-namespace event-type registration work on
    // postgres: a subscriber attached on peer-A becomes immediately
    // visible on peer-B's `_subscriptions/<aid>` namespace via the
    // sync_push receiver, so an event dispatched on peer-B matches the
    // subscription registered on peer-A. Historical replay via
    // `memory_subscription_replay` then operates on the unified store
    // — the dispatcher reads the same memory row regardless of which
    // peer originated the subscription.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        // Skip SSRF validation for synthetic loopback stubs — they are
        // never dispatched on the postgres path. Real caller-supplied
        // URLs still go through the H11 SSRF guard.
        if !url_was_synthesized && let Err(e) = crate::subscriptions::validate_url(&url) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
        let sub_id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let ns = format!("_subscriptions/{caller}");
        let metadata = json!({
            "kind": "subscription",
            "agent_id": caller,
            "subscription_id": sub_id,
            "url": url,
            "events": events,
            "namespace_filter": namespace_filter,
            "agent_filter": agent_filter,
            "created_by": caller,
            "created_at": now,
        });
        let mem = Memory {
            id: sub_id.clone(),
            tier: Tier::Long,
            namespace: ns,
            title: format!("subscription:{sub_id}"),
            content: format!(
                "subscription for {caller} -> {} (events={events})",
                namespace_filter.as_deref().unwrap_or("*")
            ),
            tags: vec!["subscription".to_string()],
            priority: 5,
            confidence: 1.0,
            source: "subscribe".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata,
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
        };
        let ctx = crate::store::CallerContext::for_agent(&caller);
        let stored_id = match app.store.store(&ctx, &mem).await {
            Ok(id) => id,
            Err(e) => return store_err_to_response(e),
        };
        // Fan the freshly-persisted subscription memory out to peers
        // using the same quorum-write contract as `_agents` /
        // `_inbox` rows. On quorum miss return 503; on a network
        // error, swallow (local commit landed). Mirrors the sqlite
        // branch's `fanout_or_503` call below.
        if let Some(resp) = fanout_or_503(&app, &mem).await {
            return resp;
        }
        return (
            StatusCode::CREATED,
            Json(json!({
                "id": stored_id,
                "url": url,
                "events": events,
                "namespace": namespace_filter,
                "namespace_filter": namespace_filter,
                "agent_filter": agent_filter,
                "agent_id": caller,
                "created_by": caller,
                "storage_backend": "postgres",
            })),
        )
            .into_response();
    }

    // Ensure the caller is a registered agent (the MCP tool enforces this).
    // Auto-register for the S33 shape so scenario callers don't have to
    // pre-call /agents themselves — same auto-create pattern used elsewhere
    // for the HTTP surface.
    let lock = app.db.lock().await;
    let already = db::list_agents(&lock.0)
        .ok()
        .is_some_and(|a| a.iter().any(|x| x.agent_id == caller));
    if !already {
        let _ = db::register_agent(&lock.0, &caller, "ai:generic", &[]);
    }
    // Inline subscribe path — we cannot delegate to `mcp::handle_subscribe`
    // here because that helper re-resolves the caller via
    // `resolve_agent_id(None, Some(mcp_client))`, which synthesizes a
    // `ai:<client>@<host>:pid-N` id rather than using the HTTP-resolved
    // `caller` verbatim. An HTTP caller registered under "ai:bob" must be
    // able to subscribe as "ai:bob", not as "ai:ai:bob@host:pid-N".
    let sub_result: Result<serde_json::Value, String> = (|| {
        crate::subscriptions::validate_url(&url).map_err(|e| e.to_string())?;
        let id = crate::subscriptions::insert(
            &lock.0,
            &crate::subscriptions::NewSubscription {
                url: &url,
                events: &events,
                secret: body.secret.as_deref(),
                namespace_filter: namespace_filter.as_deref(),
                agent_filter: agent_filter.as_deref(),
                created_by: Some(&caller),
                event_types: None,
            },
        )
        .map_err(|e| e.to_string())?;
        Ok(json!({
            "id": id,
            "url": url,
            "events": events,
            "namespace_filter": namespace_filter,
            "agent_filter": agent_filter,
            "created_by": caller,
        }))
    })();
    // Federate the `_agents` write we may have just done so registration is
    // cluster-wide. (Best-effort — subscriptions themselves live in a
    // separate table that does not ride `sync_push` today.)
    let registered_mem = if already {
        None
    } else {
        db::list(
            &lock.0,
            Some("_agents"),
            None,
            1000,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .ok()
        .and_then(|rows| {
            rows.into_iter()
                .find(|m| m.title == format!("agent:{caller}"))
        })
    };
    drop(lock);

    if let Some(ref mem) = registered_mem
        && let Some(resp) = fanout_or_503(&app, mem).await
    {
        return resp;
    }

    match sub_result {
        Ok(mut v) => {
            // Echo the caller's view of the subscription so S33 can find
            // {namespace, agent_id} keys in the response without relying on
            // the synthetic URL.
            if let Some(obj) = v.as_object_mut() {
                if let Some(ref ns) = namespace_filter {
                    obj.insert("namespace".into(), json!(ns));
                }
                obj.insert("agent_id".into(), json!(caller));
            }
            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
pub struct UnsubscribeQuery {
    #[serde(default)]
    pub id: Option<String>,
    /// S33 shape: (`agent_id`, namespace) lookup.
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub namespace: Option<String>,
}

pub async fn unsubscribe(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<UnsubscribeQuery>,
) -> impl IntoResponse {
    // v0.7.0 Wave-3 Continuation 5 (Bucket B / S33) — postgres-backed
    // daemons resolve subscriptions through the SAL `_subscriptions/
    // <agent_id>` namespace mirror that `subscribe` / `list_subscriptions`
    // write into. Both lookup-by-id and lookup-by-(agent_id, namespace)
    // resolve through the same memory-row index. Without this branch
    // the handler reaches into the scratch sqlite db which contains no
    // subscription rows on a postgres-backed daemon.
    //
    // #874 (security-medium, 2026-05-18) — DO NOT pass `q.agent_id` to
    // `resolve_caller_agent_id` as a trusted-input source. The query
    // parameter is caller-supplied and bypassable; authentication must
    // come from the request header (X-Agent-Id) only. The query
    // `agent_id` then degrades to a filter that must match the
    // authenticated caller (mismatch = 403).
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let caller = match resolve_caller_agent_id(None, &headers, None) {
            Ok(id) => id,
            Err(e) => {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
            }
        };
        if let Some(claimed) = q.agent_id.as_deref()
            && claimed != caller
        {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "agent_id query parameter does not match authenticated caller"})),
            )
                .into_response();
        }
        let ctx = crate::store::CallerContext::for_agent(&caller);

        // Lookup the subscription memory-id via the persistent index.
        let target_id: Option<String> = if let Some(id) = q.id.clone() {
            Some(id)
        } else {
            let Some(ns) = q.namespace.clone() else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "id or (agent_id, namespace) required"})),
                )
                    .into_response();
            };
            let sub_ns = format!("_subscriptions/{caller}");
            let filter = crate::store::Filter {
                namespace: Some(sub_ns),
                limit: 1000,
                ..Default::default()
            };
            match app.store.list(&ctx, &filter).await {
                Ok(rows) => rows
                    .into_iter()
                    .find(|m| {
                        m.metadata.get("namespace_filter").and_then(|v| v.as_str())
                            == Some(ns.as_str())
                    })
                    .map(|m| {
                        m.metadata
                            .get("subscription_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                            .unwrap_or(m.id)
                    }),
                Err(e) => return store_err_to_response(e),
            }
        };
        return match target_id {
            Some(id) => match app.store.delete(&ctx, &id).await {
                Ok(()) => (
                    StatusCode::OK,
                    Json(json!({"id": id, "removed": true, "storage_backend": "postgres"})),
                )
                    .into_response(),
                Err(crate::store::StoreError::NotFound { .. }) => (
                    StatusCode::OK,
                    Json(json!({"id": id, "removed": false, "storage_backend": "postgres"})),
                )
                    .into_response(),
                Err(e) => store_err_to_response(e),
            },
            None => (
                StatusCode::OK,
                Json(json!({
                    "id": "",
                    "removed": false,
                    "storage_backend": "postgres",
                })),
            )
                .into_response(),
        };
    }

    // #870 / #874 (security-high/medium, 2026-05-18) — authenticate
    // the caller via header (or body) BEFORE touching the table; never
    // trust `q.agent_id` as identity. Then scope every DELETE to the
    // resolved caller so tenant A cannot remove tenant B's hooks.
    let caller = match resolve_caller_agent_id(None, &headers, None) {
        Ok(id) => id,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    };
    if let Some(claimed) = q.agent_id.as_deref()
        && claimed != caller
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "agent_id query parameter does not match authenticated caller"})),
        )
            .into_response();
    }

    // Prefer explicit id. If absent, dispatch by (agent_id, namespace) for
    // S33 — find the first matching row from list() (already owner-scoped)
    // and delete it.
    if let Some(id) = q.id.clone() {
        let lock = app.db.lock().await;
        let outcome = crate::subscriptions::delete(&lock.0, &id, Some(&caller));
        drop(lock);
        return match outcome {
            Ok(removed) => {
                (StatusCode::OK, Json(json!({"id": id, "removed": removed}))).into_response()
            }
            Err(e) => {
                tracing::error!("unsubscribe: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal server error"})),
                )
                    .into_response()
            }
        };
    }

    let Some(ns) = q.namespace else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "id or (agent_id, namespace) required"})),
        )
            .into_response();
    };

    let lock = app.db.lock().await;
    // Owner-scoped list — the find() below is now redundant on the
    // authorization side but still narrows by namespace_filter.
    //
    // #869 audit (Category B — safe default): a db substrate failure
    // on the list query collapses to an empty `Vec`, so the
    // subsequent `target` lookup is `None` and the handler returns
    // 404 instead of leaking the substrate error — same posture the
    // sanitised 4xx path uses elsewhere in this module.
    let subs = crate::subscriptions::list(&lock.0, Some(&caller)).unwrap_or_default();
    let target = subs
        .into_iter()
        .find(|s| s.namespace_filter.as_deref() == Some(ns.as_str()));
    let outcome = match target {
        Some(s) => crate::subscriptions::delete(&lock.0, &s.id, Some(&caller)).map(|r| (s.id, r)),
        None => Ok((String::new(), false)),
    };
    drop(lock);
    match outcome {
        Ok((id, removed)) => {
            (StatusCode::OK, Json(json!({"id": id, "removed": removed}))).into_response()
        }
        Err(e) => {
            tracing::error!("unsubscribe: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct ListSubscriptionsQuery {
    #[serde(default)]
    pub agent_id: Option<String>,
}

pub async fn list_subscriptions(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ListSubscriptionsQuery>,
) -> impl IntoResponse {
    // #872 / #874 (security-high/medium, 2026-05-18) — authenticate
    // the caller via X-Agent-Id header (NOT the `?agent_id=` query
    // string, which is trivially spoofable and was the bypass surface
    // in #874). The query parameter is degraded to a refinement that
    // must match the authenticated caller, else 403.
    let caller = match resolve_caller_agent_id(None, &headers, None) {
        Ok(id) => id,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    };
    if let Some(claimed) = q.agent_id.as_deref()
        && claimed != caller
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "agent_id query parameter does not match authenticated caller"})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 Continuation 4 (Bucket B / S33) — postgres-backed
    // daemons read subscriptions back from the `_subscriptions/
    // <agent_id>` namespace via the SAL `list` projection. The
    // dispatch loop itself is still sqlite-bound; the wire envelope
    // here lets the cert oracle observe that the subscription
    // round-trips through the persistent store.
    //
    // #872 — always scope to the authenticated caller's namespace; the
    // pre-fix code walked every namespace under `_subscriptions/` when
    // no `agent_id` query param was supplied, leaking every tenant's
    // hooks.
    #[cfg(feature = "sal-postgres")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent(&caller);
        let namespaces: Vec<String> = vec![format!("_subscriptions/{caller}")];
        let mut rows: Vec<serde_json::Value> = Vec::new();
        for ns in namespaces {
            let filter = crate::store::Filter {
                namespace: Some(ns),
                limit: 1000,
                ..Default::default()
            };
            match app.store.list(&ctx, &filter).await {
                Ok(memories) => {
                    for m in memories {
                        let meta = m.metadata;
                        if meta.get("kind").and_then(|v| v.as_str()) != Some("subscription") {
                            continue;
                        }
                        let sub_id = meta
                            .get("subscription_id")
                            .cloned()
                            .unwrap_or_else(|| serde_json::Value::String(m.id.clone()));
                        rows.push(json!({
                            "id": sub_id,
                            "url": meta.get("url").cloned().unwrap_or(serde_json::Value::Null),
                            "events": meta.get("events").cloned().unwrap_or(serde_json::Value::Null),
                            "namespace": meta.get("namespace_filter").cloned().unwrap_or(serde_json::Value::Null),
                            "namespace_filter": meta.get("namespace_filter").cloned().unwrap_or(serde_json::Value::Null),
                            "agent_filter": meta.get("agent_filter").cloned().unwrap_or(serde_json::Value::Null),
                            "agent_id": meta.get("agent_id").cloned().unwrap_or(serde_json::Value::Null),
                            "created_by": meta.get("created_by").cloned().unwrap_or(serde_json::Value::Null),
                            "created_at": meta.get("created_at").cloned().unwrap_or(serde_json::Value::Null),
                            "dispatch_count": 0,
                            "failure_count": 0,
                        }));
                    }
                }
                Err(e) => return store_err_to_response(e),
            }
        }
        let count = rows.len();
        return (
            StatusCode::OK,
            Json(json!({
                "count": count,
                "subscriptions": rows,
                "storage_backend": "postgres",
            })),
        )
            .into_response();
    }
    let state = app.db.clone();
    let lock = state.lock().await;
    // #872 — DB-side ownership scope: only the caller's rows.
    let subs = match crate::subscriptions::list(&lock.0, Some(&caller)) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("list_subscriptions: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };
    drop(lock);
    let filtered = subs;
    // Expose the subscribed namespace as a top-level field per row so S33 can
    // read `namespace` directly without probing `namespace_filter`.
    let rows: Vec<serde_json::Value> = filtered
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "url": s.url,
                "events": s.events,
                "namespace": s.namespace_filter,
                "namespace_filter": s.namespace_filter,
                "agent_filter": s.agent_filter,
                "agent_id": s.agent_filter.clone().or(s.created_by.clone()),
                "created_by": s.created_by,
                "created_at": s.created_at,
                "dispatch_count": s.dispatch_count,
                "failure_count": s.failure_count,
            })
        })
        .collect();
    let count = rows.len();
    (
        StatusCode::OK,
        Json(json!({"count": count, "subscriptions": rows})),
    )
        .into_response()
}
