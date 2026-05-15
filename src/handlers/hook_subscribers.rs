// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::db;
use crate::models::{Memory, Tier};
use crate::validate;

#[cfg(feature = "sal")]
use super::StorageBackend;
#[cfg(feature = "sal")]
use super::store_err_to_response;
use super::{AppState, Db};
use super::{fanout_or_503, list_namespaces, resolve_caller_agent_id};

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
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

#[derive(Deserialize)]
pub struct InboxQuery {
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub unread_only: Option<bool>,
    #[serde(default)]
    pub limit: Option<u64>,
}

pub async fn get_inbox(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<InboxQuery>,
) -> impl IntoResponse {
    let owner = match resolve_caller_agent_id(None, &headers, q.agent_id.as_deref()) {
        Ok(id) => id,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    };

    // v0.7.0 Wave-3 Continuation 4 (Bucket B / S32+S58) — postgres
    // inbox now reads from the `_inbox/<owner>` namespace via the SAL
    // `list` projection, matching what `notify` (Phase 16) already
    // writes. The handler walks the namespace and projects each row
    // into the inbox-message wire shape. Subscriptions still ride the
    // legacy sqlite `subscriptions` table; the inbox itself does not
    // need that surface — `notify` lands the message directly under
    // `_inbox/<target>` and the inbox is a straight namespace read.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ns = format!("_inbox/{owner}");
        let ctx = crate::store::CallerContext::for_agent(&owner);
        let cap = q
            .limit
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(100)
            .clamp(1, 1000);
        let filter = crate::store::Filter {
            namespace: Some(ns),
            limit: cap,
            ..Default::default()
        };
        return match app.store.list(&ctx, &filter).await {
            Ok(rows) => {
                let messages: Vec<serde_json::Value> = rows
                    .into_iter()
                    .filter(|m| {
                        // Honour `unread_only` when set: any row whose
                        // metadata explicitly carries `read=true` is
                        // filtered out. The default state (no key) is
                        // treated as unread, mirroring the SQLite
                        // contract.
                        if q.unread_only.unwrap_or(false) {
                            m.metadata.get("read").and_then(serde_json::Value::as_bool)
                                != Some(true)
                        } else {
                            true
                        }
                    })
                    .map(|m| {
                        json!({
                            "id": m.id,
                            "title": m.title,
                            "payload": m.content,
                            "content": m.content,
                            "priority": m.priority,
                            "tier": m.tier.as_str(),
                            "namespace": m.namespace,
                            "metadata": m.metadata,
                            "created_at": m.created_at,
                            "updated_at": m.updated_at,
                            "agent_id": m.metadata
                                .get("agent_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or(""),
                            "from_agent_id": m.metadata
                                .get("from_agent_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or(""),
                            "target_agent_id": m.metadata
                                .get("target_agent_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or(""),
                        })
                    })
                    .collect();
                let unread_count = messages
                    .iter()
                    .filter(|m| {
                        m.get("metadata")
                            .and_then(|v| v.get("read"))
                            .and_then(serde_json::Value::as_bool)
                            != Some(true)
                    })
                    .count();
                (
                    StatusCode::OK,
                    Json(json!({
                        "agent_id": owner,
                        "messages": messages,
                        "unread_count": unread_count,
                        "storage_backend": "postgres",
                    })),
                )
                    .into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let mut params = json!({"agent_id": owner});
    if let Some(u) = q.unread_only {
        params["unread_only"] = json!(u);
    }
    if let Some(l) = q.limit {
        params["limit"] = json!(l);
    }
    let lock = app.db.lock().await;
    // Pass the resolved owner as `mcp_client` too so `handle_inbox`'s
    // identity-resolution fallback lands on the same id whichever branch
    // it consults (it prefers `params["agent_id"]` when present).
    let result = crate::mcp::handle_inbox(&lock.0, &params, None);
    drop(lock);
    match result {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
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
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let caller = match resolve_caller_agent_id(None, &headers, q.agent_id.as_deref()) {
            Ok(id) => id,
            Err(e) => {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
            }
        };
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

    // Prefer explicit id. If absent, dispatch by (agent_id, namespace) for
    // S33 — find the first matching row from list() and delete it.
    if let Some(id) = q.id.clone() {
        let mut params = json!({"id": id});
        // Keep the key name stable across both handlers' interior shapes.
        let _ = params.as_object_mut();
        let lock = app.db.lock().await;
        let result = crate::mcp::handle_unsubscribe(&lock.0, &params);
        drop(lock);
        return match result {
            Ok(v) => (StatusCode::OK, Json(v)).into_response(),
            Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
        };
    }

    let caller = match resolve_caller_agent_id(None, &headers, q.agent_id.as_deref()) {
        Ok(id) => id,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    };
    let Some(ns) = q.namespace else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "id or (agent_id, namespace) required"})),
        )
            .into_response();
    };

    let lock = app.db.lock().await;
    let subs = crate::subscriptions::list(&lock.0).unwrap_or_default();
    let target = subs.into_iter().find(|s| {
        s.namespace_filter.as_deref() == Some(ns.as_str())
            && (s.agent_filter.as_deref() == Some(caller.as_str())
                || s.created_by.as_deref() == Some(caller.as_str()))
    });
    let outcome = match target {
        Some(s) => crate::subscriptions::delete(&lock.0, &s.id).map(|r| (s.id, r)),
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
    Query(q): Query<ListSubscriptionsQuery>,
) -> impl IntoResponse {
    // v0.7.0 Wave-3 Continuation 4 (Bucket B / S33) — postgres-backed
    // daemons read subscriptions back from the `_subscriptions/
    // <agent_id>` namespace via the SAL `list` projection. The
    // dispatch loop itself is still sqlite-bound; the wire envelope
    // here lets the cert oracle observe that the subscription
    // round-trips through the persistent store.
    #[cfg(feature = "sal-postgres")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent(q.agent_id.as_deref().unwrap_or("daemon"));
        // When `agent_id` is supplied, scope to `_subscriptions/<aid>`;
        // otherwise scan every `_subscriptions/...` namespace via
        // `taxonomy_namespaces` + per-namespace listing.
        let namespaces: Vec<String> = if let Some(aid) = q.agent_id.as_deref() {
            vec![format!("_subscriptions/{aid}")]
        } else {
            match crate::store::postgres::taxonomy_namespaces_via_store(
                &app.store,
                Some("_subscriptions"),
            )
            .await
            {
                Ok(pairs) => pairs.into_iter().map(|(ns, _)| ns).collect(),
                Err(e) => return store_err_to_response(e),
            }
        };
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
    let subs = match crate::subscriptions::list(&lock.0) {
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
    // Filter by agent_id when the caller passed one (S33's per-agent view).
    let filtered: Vec<_> = match q.agent_id.as_deref() {
        Some(aid) => subs
            .into_iter()
            .filter(|s| {
                s.agent_filter.as_deref() == Some(aid) || s.created_by.as_deref() == Some(aid)
            })
            .collect(),
        None => subs,
    };
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

// --- /api/v1/namespaces/{ns}/standard (POST / GET / DELETE) ----------------
//    +/api/v1/namespaces (POST with body.namespace, GET/DELETE with ?namespace=)
//
// S34/S35 drive the standard via the bare `/api/v1/namespaces` surface; the
// `/namespaces/{ns}/standard` path is kept for API-shape parity with the MCP
// tool namespace. Both share a single underlying implementation.

#[derive(Deserialize)]
pub struct NamespaceStandardBody {
    /// The memory id representing the standard.
    #[serde(default)]
    pub id: Option<String>,
    /// Optional parent namespace for chain lookups.
    #[serde(default)]
    pub parent: Option<String>,
    /// Optional governance policy to merge into the standard's metadata.
    #[serde(default)]
    pub governance: Option<serde_json::Value>,
    /// Accepted for the path-less `/namespaces` form — ignored when the
    /// namespace is supplied via a URL segment.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Some scenarios nest the payload under `standard` (S34 does so).
    #[serde(default)]
    pub standard: Option<Box<NamespaceStandardBody>>,
}

fn flatten_standard_body(body: NamespaceStandardBody) -> NamespaceStandardBody {
    // When the caller nests fields under `standard: { … }` (S34 shape), pull
    // the inner payload up to the top level so the single code path below
    // can read it uniformly.
    if let Some(inner) = body.standard {
        let mut merged = *inner;
        if merged.namespace.is_none() {
            merged.namespace = body.namespace;
        }
        if merged.id.is_none() {
            merged.id = body.id;
        }
        if merged.parent.is_none() {
            merged.parent = body.parent;
        }
        if merged.governance.is_none() {
            merged.governance = body.governance;
        }
        merged
    } else {
        body
    }
}

fn namespace_standard_params(ns: &str, body: &NamespaceStandardBody) -> serde_json::Value {
    let mut params = json!({"namespace": ns});
    if let Some(ref id) = body.id {
        params["id"] = json!(id);
    }
    if let Some(ref p) = body.parent {
        params["parent"] = json!(p);
    }
    if let Some(ref g) = body.governance {
        params["governance"] = g.clone();
    }
    params
}

/// v0.7.0 G-PHASE-E-2 (#707) — merge an incoming governance JSON blob
/// onto an existing one, key-by-key. Mirrors the helper in
/// `mcp::tools::namespace`. Incoming keys override existing ones; keys
/// present only on the existing blob (e.g. an operator-set
/// `require_approval_above_depth`) survive untouched.
///
/// Only consumed on the SAL/postgres branch at line ~1064; gate the
/// definition to match so default-features builds don't emit a
/// dead-code warning.
#[cfg(feature = "sal")]
fn merge_governance_fields_http(
    existing: Option<&serde_json::Value>,
    incoming: &serde_json::Value,
) -> serde_json::Value {
    let mut merged = serde_json::Map::new();
    if let Some(existing_obj) = existing.and_then(serde_json::Value::as_object) {
        for (k, v) in existing_obj {
            merged.insert(k.clone(), v.clone());
        }
    }
    if let Some(incoming_obj) = incoming.as_object() {
        for (k, v) in incoming_obj {
            merged.insert(k.clone(), v.clone());
        }
    } else {
        return incoming.clone();
    }
    serde_json::Value::Object(merged)
}

async fn set_namespace_standard_inner(
    app: &AppState,
    ns: &str,
    body: NamespaceStandardBody,
) -> axum::response::Response {
    let body = flatten_standard_body(body);

    // v0.7.0 Wave-3 Continuation 2 (Phase 11) — postgres-backed
    // namespace standard write path. The trait method handles the
    // structural namespace_meta upsert; governance metadata that the
    // sqlite path layers into the standard memory's metadata is
    // captured by storing the policy in the placeholder memory's
    // metadata.governance JSONB field via the trait's standard
    // store path.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("ai:http");
        // Resolve standard_id: caller-supplied or auto-seed a placeholder.
        let standard_id = if let Some(id) = body.id.clone() {
            id
        } else {
            // Try to find an existing placeholder via list().
            let filter = crate::store::Filter {
                namespace: Some(ns.to_string()),
                limit: 50,
                ..Default::default()
            };
            let existing = match app.store.list(&ctx, &filter).await {
                Ok(rows) => rows
                    .into_iter()
                    .find(|m| m.tags.iter().any(|t| t == "_namespace_standard"))
                    .map(|m| m.id),
                Err(_) => None,
            };
            if let Some(id) = existing {
                id
            } else {
                let now = Utc::now().to_rfc3339();
                let mut metadata = serde_json::json!({"agent_id": "system"});
                if let Some(g) = body.governance.clone()
                    && let Some(obj) = metadata.as_object_mut()
                {
                    obj.insert("governance".to_string(), g);
                }
                let placeholder = Memory {
                    id: Uuid::new_v4().to_string(),
                    tier: Tier::Long,
                    namespace: ns.to_string(),
                    title: format!("_standard:{ns}"),
                    content: format!("namespace standard for {ns}"),
                    tags: vec!["_namespace_standard".to_string()],
                    priority: 5,
                    confidence: 1.0,
                    source: "api".into(),
                    access_count: 0,
                    created_at: now.clone(),
                    updated_at: now,
                    last_accessed_at: None,
                    expires_at: None,
                    metadata,
                    reflection_depth: 0,
                    memory_kind: crate::models::MemoryKind::Observation,
                };
                match app.store.store(&ctx, &placeholder).await {
                    Ok(id) => id,
                    Err(e) => return store_err_to_response(e),
                }
            }
        };

        // v0.7.0 Wave-3 Continuation 5 (Bucket C / S35+S53+S60+S80) —
        // when the caller supplied a `governance` policy AND a pre-
        // existing standard_id, merge the policy into the standard
        // memory's `metadata.governance` so `resolve_governance_policy`
        // (which reads exactly this field via `from_metadata`) finds
        // the policy on the next write. Without this merge step the
        // postgres adapter's chain walk lands on a memory whose
        // metadata has no `governance` key, returns `None`, and the
        // intruder's write is allowed through.
        if let Some(g) = body.governance.clone() {
            // Load the standard memory FIRST so we can merge the
            // incoming `g` onto the existing `metadata.governance`
            // blob — this preserves extra fields like
            // `require_approval_above_depth` that live outside the
            // typed `GovernancePolicy` struct (v0.7.0 G-PHASE-E-2,
            // #707). Mirrors the SQLite handler's merge in
            // `mcp::tools::namespace::handle_namespace_set_standard`.
            let standard_mem = match app.store.get(&ctx, &standard_id).await {
                Ok(m) => m,
                Err(e) => return store_err_to_response(e),
            };
            let merged = merge_governance_fields_http(standard_mem.metadata.get("governance"), &g);
            // Validate the merged blob's typed shape. Deserialising
            // drops unknown fields but the typed sub-set must still
            // parse + pass policy validation. Mirrors the SQLite path
            // at `mcp::tools::namespace`.
            let policy: crate::models::GovernancePolicy =
                match serde_json::from_value(merged.clone()) {
                    Ok(p) => p,
                    Err(e) => {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": format!("invalid governance: {e}")})),
                        )
                            .into_response();
                    }
                };
            if let Err(e) = validate::validate_governance_policy(&policy) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid governance: {e}")})),
                )
                    .into_response();
            }
            let mut metadata = if standard_mem.metadata.is_object() {
                standard_mem.metadata.clone()
            } else {
                json!({})
            };
            if let Some(obj) = metadata.as_object_mut() {
                obj.insert("governance".to_string(), merged);
            }
            let patch = crate::store::UpdatePatch {
                metadata: Some(metadata),
                ..Default::default()
            };
            if let Err(e) = app.store.update(&ctx, &standard_id, patch).await {
                return store_err_to_response(e);
            }
        }
        return match app
            .store
            .set_namespace_standard(&ctx, ns, &standard_id, body.parent.as_deref())
            .await
        {
            Ok(()) => (
                StatusCode::CREATED,
                Json(json!({
                    "namespace": ns,
                    "standard_id": standard_id,
                    "parent": body.parent,
                    "storage_backend": "postgres",
                })),
            )
                .into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    // Auto-seed a placeholder standard memory when the caller didn't supply
    // an `id`. S34's body is `{governance: …}` with no id — we create a
    // minimal standard memory so the governance policy has a home.
    let lock = app.db.lock().await;
    let resolved_id = if let Some(id) = body.id.clone() {
        id
    } else {
        // Look for an existing placeholder first to keep repeat calls
        // idempotent; otherwise insert a new row.
        let existing = db::list(
            &lock.0,
            Some(ns),
            None,
            1,
            0,
            None,
            None,
            None,
            Some("_namespace_standard"),
            None,
        )
        .ok()
        .and_then(|v| v.into_iter().next());
        if let Some(m) = existing {
            m.id
        } else {
            let now = Utc::now().to_rfc3339();
            let placeholder = Memory {
                id: Uuid::new_v4().to_string(),
                tier: Tier::Long,
                namespace: ns.to_string(),
                title: format!("_standard:{ns}"),
                content: format!("namespace standard for {ns}"),
                tags: vec!["_namespace_standard".to_string()],
                priority: 5,
                confidence: 1.0,
                source: "api".into(),
                access_count: 0,
                created_at: now.clone(),
                updated_at: now,
                last_accessed_at: None,
                expires_at: None,
                metadata: serde_json::json!({"agent_id": "system"}),
                reflection_depth: 0,
                memory_kind: crate::models::MemoryKind::Observation,
            };
            match db::insert(&lock.0, &placeholder) {
                Ok(id) => id,
                Err(e) => {
                    tracing::error!("namespace_standard: placeholder insert failed: {e}");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "internal server error"})),
                    )
                        .into_response();
                }
            }
        }
    };
    let mut effective = body;
    effective.id = Some(resolved_id.clone());
    let params = namespace_standard_params(ns, &effective);
    let result = crate::mcp::handle_namespace_set_standard(&lock.0, &params);
    // Capture the standard memory so we can fan it out to peers — cluster
    // visibility of governance rules matters for S34/S35.
    let standard_mem = db::get(&lock.0, &resolved_id).ok().flatten();
    // v0.6.2 (S35): also capture the freshly-written namespace_meta row
    // so peers learn the explicit (namespace, standard_id, parent) tuple.
    // Without this, peers auto-detect a parent via `-` prefix which may
    // disagree with what the originator set.
    let meta_entry = db::get_namespace_meta_entry(&lock.0, ns).ok().flatten();
    drop(lock);

    match result {
        Ok(v) => {
            if let Some(ref mem) = standard_mem
                && let Some(resp) = fanout_or_503(app, mem).await
            {
                return resp;
            }
            if let (Some(entry), Some(fed)) = (meta_entry.as_ref(), app.federation.as_ref()) {
                match crate::federation::broadcast_namespace_meta_quorum(fed, entry).await {
                    Ok(tracker) => {
                        if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("Retry-After", "2")],
                                Json(serde_json::to_value(&payload).unwrap_or_default()),
                            )
                                .into_response();
                        }
                    }
                    Err(err) => {
                        let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("Retry-After", "2")],
                            Json(serde_json::to_value(&payload).unwrap_or_default()),
                        )
                            .into_response();
                    }
                }
            }
            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

pub async fn set_namespace_standard(
    State(app): State<AppState>,
    Path(ns): Path<String>,
    Json(body): Json<NamespaceStandardBody>,
) -> impl IntoResponse {
    set_namespace_standard_inner(&app, &ns, body).await
}

#[derive(Deserialize)]
pub struct NamespaceStandardQuery {
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub inherit: Option<bool>,
}

pub async fn get_namespace_standard(
    State(state): State<Db>,
    Path(ns): Path<String>,
    Query(q): Query<NamespaceStandardQuery>,
) -> impl IntoResponse {
    let mut params = json!({"namespace": ns});
    if let Some(inh) = q.inherit {
        params["inherit"] = json!(inh);
    }
    let lock = state.lock().await;
    let result = crate::mcp::handle_namespace_get_standard(&lock.0, &params);
    drop(lock);
    match result {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

pub async fn clear_namespace_standard(
    State(app): State<AppState>,
    Path(ns): Path<String>,
) -> impl IntoResponse {
    clear_namespace_standard_inner(&app, &ns).await
}

// Query-string forms for the S34/S35 `/api/v1/namespaces?namespace=…` shape.
pub async fn set_namespace_standard_qs(
    State(app): State<AppState>,
    Json(body): Json<NamespaceStandardBody>,
) -> impl IntoResponse {
    let Some(ns) = body
        .namespace
        .clone()
        .or_else(|| body.standard.as_ref().and_then(|s| s.namespace.clone()))
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "namespace is required"})),
        )
            .into_response();
    };
    set_namespace_standard_inner(&app, &ns, body).await
}

pub async fn get_namespace_standard_qs(
    State(app): State<AppState>,
    Query(q): Query<NamespaceStandardQuery>,
) -> impl IntoResponse {
    // If no namespace is supplied this shares a route with the existing
    // `list_namespaces` GET; the router chains the two so a plain
    // `GET /api/v1/namespaces` still returns the list.
    let Some(ns) = q.namespace.clone() else {
        return list_namespaces(State(app)).await.into_response();
    };

    // v0.7.0 Wave-3 Continuation 5 (Bucket C / S35) — postgres-backed
    // daemons resolve the namespace standard via the SAL trait. When
    // `inherit=true` we walk the parent chain (already cached in
    // `namespace_meta.parent_namespace`) leaf→root to find the nearest
    // ancestor that has a standard memory. Without inherit we look up
    // the exact namespace.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("ai:http");
        let inherit = q.inherit.unwrap_or(false);
        // Build chain leaf → root (most-specific first) by trimming
        // `/segment` until empty. The chain matches the SQLite
        // semantics in `db::resolve_namespace_standard` for the
        // simple namespace-hierarchy case.
        let mut chain: Vec<String> = vec![ns.clone()];
        if inherit {
            let mut cur = ns.clone();
            while let Some(pos) = cur.rfind('/') {
                cur.truncate(pos);
                if cur.is_empty() {
                    break;
                }
                chain.push(cur.clone());
            }
        }

        if inherit {
            // S35 contract — return the FULL chain of standards from
            // leaf → root so the caller sees both child and parent
            // rules layered into one view. Mirrors the sqlite
            // `handle_namespace_get_standard` inherit branch which
            // returns `chain` + `standards` arrays.
            let mut standards: Vec<serde_json::Value> = Vec::new();
            for candidate in &chain {
                if let Ok(Some((standard_id, parent))) =
                    app.store.get_namespace_standard(&ctx, candidate).await
                {
                    // Pull the standard memory body so the caller can
                    // see governance + content layered through.
                    let mem_doc = match app.store.get(&ctx, &standard_id).await {
                        Ok(m) => json!({
                            "namespace": candidate,
                            "standard_id": standard_id,
                            "id": standard_id,
                            "title": m.title,
                            "content": m.content,
                            "priority": m.priority,
                            "parent_namespace": parent,
                            "governance": m.metadata.get("governance").cloned()
                                .unwrap_or(serde_json::Value::Null),
                        }),
                        Err(_) => json!({
                            "namespace": candidate,
                            "standard_id": standard_id,
                            "id": standard_id,
                            "parent_namespace": parent,
                        }),
                    };
                    standards.push(mem_doc);
                }
            }
            // Pick the closest (leaf-most) entry as the resolved
            // standard for the response root level so existing
            // single-standard consumers still see the expected
            // `standard_id`.
            let closest = standards.first().cloned().unwrap_or(json!({}));
            return (
                StatusCode::OK,
                Json(json!({
                    "namespace": ns,
                    "chain": chain,
                    "standards": standards,
                    "resolved_namespace": closest.get("namespace").cloned()
                        .unwrap_or(serde_json::Value::Null),
                    "standard_id": closest.get("standard_id").cloned()
                        .unwrap_or(serde_json::Value::Null),
                    "id": closest.get("id").cloned()
                        .unwrap_or(serde_json::Value::Null),
                    "parent_namespace": closest.get("parent_namespace").cloned()
                        .unwrap_or(serde_json::Value::Null),
                    "storage_backend": "postgres",
                })),
            )
                .into_response();
        }
        // Non-inherit form — single exact-match lookup.
        match app.store.get_namespace_standard(&ctx, &ns).await {
            Ok(Some((standard_id, parent))) => {
                return (
                    StatusCode::OK,
                    Json(json!({
                        "namespace": ns,
                        "resolved_namespace": ns,
                        "standard_id": standard_id,
                        "id": standard_id,
                        "parent_namespace": parent,
                        "storage_backend": "postgres",
                    })),
                )
                    .into_response();
            }
            Ok(None) => {}
            Err(e) => return store_err_to_response(e),
        }
        return (
            StatusCode::OK,
            Json(json!({
                "namespace": ns,
                "standard_id": serde_json::Value::Null,
                "id": serde_json::Value::Null,
                "parent_namespace": serde_json::Value::Null,
                "storage_backend": "postgres",
            })),
        )
            .into_response();
    }

    let mut params = json!({"namespace": ns});
    if let Some(inh) = q.inherit {
        params["inherit"] = json!(inh);
    }
    let lock = app.db.lock().await;
    let result = crate::mcp::handle_namespace_get_standard(&lock.0, &params);
    drop(lock);
    match result {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

pub async fn clear_namespace_standard_qs(
    State(app): State<AppState>,
    Query(q): Query<NamespaceStandardQuery>,
) -> impl IntoResponse {
    let Some(ns) = q.namespace else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "namespace is required"})),
        )
            .into_response();
    };
    clear_namespace_standard_inner(&app, &ns).await
}

/// v0.6.2 (S35 follow-up): shared implementation for path and query-string
/// clear handlers. Runs the local clear then, on success, fans the cleared
/// namespace out to peers via `broadcast_namespace_meta_clear_quorum`.
/// Returns 503 `quorum_not_met` when federation is configured and the quorum
/// contract fails — matching the pattern established by
/// `set_namespace_standard_inner`.
async fn clear_namespace_standard_inner(app: &AppState, ns: &str) -> axum::response::Response {
    // v0.7.0 Wave-3 Continuation 2 (Phase 11) — postgres-backed clear.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("ai:http");
        return match app.store.clear_namespace_standard(&ctx, ns).await {
            Ok(true) => (
                StatusCode::OK,
                Json(json!({
                    "cleared": true,
                    "namespace": ns,
                    "storage_backend": "postgres",
                })),
            )
                .into_response(),
            Ok(false) => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "no namespace_meta row matched"})),
            )
                .into_response(),
            Err(e) => store_err_to_response(e),
        };
    }
    let params = json!({"namespace": ns});
    let lock = app.db.lock().await;
    let result = crate::mcp::handle_namespace_clear_standard(&lock.0, &params);
    drop(lock);
    match result {
        Ok(v) => {
            if let Some(fed) = app.federation.as_ref() {
                let namespaces = vec![ns.to_string()];
                match crate::federation::broadcast_namespace_meta_clear_quorum(fed, &namespaces)
                    .await
                {
                    Ok(tracker) => {
                        if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("Retry-After", "2")],
                                Json(serde_json::to_value(&payload).unwrap_or_default()),
                            )
                                .into_response();
                        }
                    }
                    Err(err) => {
                        let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("Retry-After", "2")],
                            Json(serde_json::to_value(&payload).unwrap_or_default()),
                        )
                            .into_response();
                    }
                }
            }
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

// --- /api/v1/session/start (POST) ------------------------------------------

#[derive(Deserialize)]
pub struct SessionStartBody {
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub limit: Option<u64>,
    #[serde(default)]
    pub agent_id: Option<String>,
}

pub async fn session_start(
    State(state): State<Db>,
    headers: HeaderMap,
    Json(body): Json<SessionStartBody>,
) -> impl IntoResponse {
    // agent_id is optional for session_start; but if supplied it must validate.
    if let Some(ref id) = body.agent_id
        && let Err(e) = validate::validate_agent_id(id)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid agent_id: {e}")})),
        )
            .into_response();
    }
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let _ = header_agent_id; // identity currently informational for session_start
    let mut params = json!({});
    if let Some(ref n) = body.namespace {
        params["namespace"] = json!(n);
    }
    if let Some(l) = body.limit {
        params["limit"] = json!(l);
    }
    let lock = state.lock().await;
    let result = crate::mcp::handle_session_start(&lock.0, &params, None);
    drop(lock);
    match result {
        Ok(mut v) => {
            // Stamp a stable session id so callers (S36) can correlate
            // subsequent writes. We don't persist sessions today; the id is
            // advisory and round-tripped via metadata by the caller.
            if let Some(obj) = v.as_object_mut() {
                obj.entry("session_id")
                    .or_insert_with(|| json!(Uuid::new_v4().to_string()));
                if let Some(ref a) = body.agent_id {
                    obj.insert("agent_id".into(), json!(a));
                }
            }
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}
