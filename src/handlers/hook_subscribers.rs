// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use crate::models::ConfidenceSource;
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
    // #901 (security-high, 2026-05-19) — sibling of #874. The pre-#901
    // path TRUSTED `?agent_id=` query as identity, allowing any caller
    // to read any agent's inbox by passing `?agent_id=victim`. Header
    // is now the only trusted source; the query value (if present)
    // must match the authenticated caller, else 403.
    let owner = match resolve_caller_agent_id(None, &headers, None) {
        Ok(id) => id,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    };
    if let Some(claimed) = q.agent_id.as_deref()
        && claimed != owner
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "agent_id query parameter does not match authenticated caller"})),
        )
            .into_response();
    }

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
    headers: Option<&HeaderMap>,
) -> axum::response::Response {
    // #913 (security-medium / SOC2, 2026-05-19) — admin governance audit.
    // `set_namespace_standard` mutates the governance policy that gates
    // EVERY downstream write into the namespace; the chain entry must be
    // emitted BEFORE the storage write so the audit trail survives a
    // failed downstream write. Mirrors the #911 pattern in
    // `register_agent` / `archive_purge`.
    let header_agent_id = headers.and_then(|h| h.get("x-agent-id").and_then(|v| v.to_str().ok()));
    let caller = crate::identity::resolve_http_agent_id(None, header_agent_id)
        .unwrap_or_else(|_| "anonymous:invalid".to_string());
    crate::governance::audit::record_decision(
        &caller,
        "allow",
        "namespace_set_standard",
        "",
        json!({
            "namespace": ns,
            "standard_id": body.id.clone(),
            "parent": body.parent.clone(),
            "has_governance": body.governance.is_some(),
        }),
    );

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
                            // #869 — typed 503 envelope via the shared helper.
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return super::quorum_not_met_response(&payload);
                        }
                    }
                    Err(err) => {
                        let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                        return super::quorum_not_met_response(&payload);
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
    headers: HeaderMap,
    Path(ns): Path<String>,
    Json(body): Json<NamespaceStandardBody>,
) -> impl IntoResponse {
    set_namespace_standard_inner(&app, &ns, body, Some(&headers)).await
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
    headers: HeaderMap,
    Path(ns): Path<String>,
) -> impl IntoResponse {
    clear_namespace_standard_inner(&app, &ns, Some(&headers)).await
}

// Query-string forms for the S34/S35 `/api/v1/namespaces?namespace=…` shape.
pub async fn set_namespace_standard_qs(
    State(app): State<AppState>,
    headers: HeaderMap,
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
    set_namespace_standard_inner(&app, &ns, body, Some(&headers)).await
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
    headers: HeaderMap,
    Query(q): Query<NamespaceStandardQuery>,
) -> impl IntoResponse {
    let Some(ns) = q.namespace else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "namespace is required"})),
        )
            .into_response();
    };
    clear_namespace_standard_inner(&app, &ns, Some(&headers)).await
}

/// v0.6.2 (S35 follow-up): shared implementation for path and query-string
/// clear handlers. Runs the local clear then, on success, fans the cleared
/// namespace out to peers via `broadcast_namespace_meta_clear_quorum`.
/// Returns 503 `quorum_not_met` when federation is configured and the quorum
/// contract fails — matching the pattern established by
/// `set_namespace_standard_inner`.
async fn clear_namespace_standard_inner(
    app: &AppState,
    ns: &str,
    headers: Option<&HeaderMap>,
) -> axum::response::Response {
    // #913 (security-medium / SOC2, 2026-05-19) — admin governance audit.
    // Clearing a namespace standard removes the governance policy that
    // gates downstream writes; the chain entry MUST land before the
    // storage write so the audit trail captures intent.
    let header_agent_id = headers.and_then(|h| h.get("x-agent-id").and_then(|v| v.to_str().ok()));
    let caller = crate::identity::resolve_http_agent_id(None, header_agent_id)
        .unwrap_or_else(|_| "anonymous:invalid".to_string());
    crate::governance::audit::record_decision(
        &caller,
        "allow",
        "namespace_clear_standard",
        "",
        json!({
            "namespace": ns,
        }),
    );

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
                            // #869 — typed 503 envelope via the shared helper.
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return super::quorum_not_met_response(&payload);
                        }
                    }
                    Err(err) => {
                        let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                        return super::quorum_not_met_response(&payload);
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
