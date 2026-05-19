// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Memory CRUD HTTP handlers — `get_memory`, `update_memory`,
//! `delete_memory`, and `promote_memory`.
//!
//! Extracted from [`super::http`] under issue #650 (handler cap ≤1200
//! LOC). Handler bodies are unchanged; only the module surface moved.
//! Wire compatibility preserved via `pub use memories::*` in [`super`].

#![allow(clippy::too_many_lines)]

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde_json::json;

use crate::db;
use crate::models::{Tier, UpdateMemory};
use crate::validate;

use super::AppState;
#[cfg(feature = "sal")]
use super::StorageBackend;
#[cfg(feature = "sal")]
use super::store_err_to_response;

pub async fn get_memory(
    State(app): State<AppState>,
    #[cfg_attr(not(feature = "sal"), allow(unused_variables))] headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 — Postgres-backed daemons dispatch through the
    // SAL trait. The legacy `db::resolve_id` path is SQLite-bound (it
    // walks `memories` + `memory_links` directly through the
    // mutex-guarded rusqlite connection); routing the postgres branch
    // through `app.store` keeps the wire-shape identical while
    // hitting the right backend. SQLite-backed daemons keep the
    // legacy direct-rusqlite path for v0.7.0 binary parity.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        // #910 SAL-level — resolve the caller from `X-Agent-Id` so the
        // SAL `get` filter has a known principal. Header-only auth on
        // this GET surface; anonymous callers get a per-request
        // `anonymous:req-…` id and see only non-private rows. Bound
        // inside the cfg block so default-features builds don't flag
        // it as unused.
        let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
        let caller = crate::identity::resolve_http_agent_id(None, header_agent_id)
            .unwrap_or_else(|_| format!("anonymous:req-{}", uuid::Uuid::new_v4()));
        let ctx = crate::store::CallerContext::for_agent(&caller);
        return match app.store.get(&ctx, &id).await {
            Ok(mem) => {
                // List_links surfaces the full edge set (no namespace
                // filter) so the postgres adapter's `list_links` walks
                // its `memory_links` table and the local-side filter
                // narrows to edges anchored at this memory id.
                let edges = match app.store.list_links(None).await {
                    Ok(rows) => rows
                        .into_iter()
                        .filter(|l| l.source_id == mem.id || l.target_id == mem.id)
                        .collect::<Vec<_>>(),
                    Err(e) => {
                        tracing::warn!(
                            "store.list_links during get_memory failed: {e}; \
                             returning memory with empty links"
                        );
                        Vec::new()
                    }
                };
                Json(json!({"memory": mem, "links": edges})).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::resolve_id(&lock.0, &id) {
        Ok(Some(mem)) => {
            // #869 audit (Category B — safe default): a substrate
            // failure on `get_links` is non-fatal — the memory body
            // itself was retrieved cleanly. Empty `links` array
            // degrades graph navigation rather than failing the GET.
            let links = db::get_links(&lock.0, &mem.id).unwrap_or_default();
            Json(json!({"memory": mem, "links": links})).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response(),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("ambiguous ID prefix") {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
            }
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn update_memory(
    State(app): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<UpdateMemory>,
) -> impl IntoResponse {
    let state = app.db.clone();
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    if let Err(e) = validate::validate_update(&body) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    // v0.7.0 Provenance Gap 1 (#884) — `If-Match: <version>` opt-in
    // optimistic-concurrency gate. When the header is supplied with
    // a parseable integer, the storage::update_with_expected_version
    // path refuses the mutation with a 409 CONFLICT envelope carrying
    // both expected + current versions when the stored row has
    // drifted. When the header is absent or unparseable, the legacy
    // last-write-wins behaviour is preserved.
    let if_match_version: Option<i64> = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            // Allow both bare integers and quoted ETag-style values
            // ("42" or 42).
            let trimmed = s.trim().trim_matches('"');
            trimmed.parse::<i64>().ok()
        });

    // v0.7.0 Wave-3 — Postgres-backed daemons take the SAL trait
    // dispatch path. The trait's `update` accepts an `UpdatePatch`
    // shape; map the `UpdateMemory` body into the trait shape and
    // delegate. The legacy SQLite path below threads federation,
    // embedder regen, audit, and governance hooks; Postgres takes
    // the simpler shape until those layers are also trait-routed.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let patch = crate::store::UpdatePatch {
            title: body.title.clone(),
            content: body.content.clone(),
            tier: body.tier.clone(),
            namespace: body.namespace.clone(),
            tags: body.tags.clone(),
            priority: body.priority,
            confidence: body.confidence,
            metadata: body.metadata.clone(),
            // v0.7.0 Provenance Gap 2 (#906) — thread source_uri patch.
            source_uri: body.source_uri.clone(),
        };
        let ctx = crate::store::CallerContext::for_agent("ai:http");
        return match app.store.update(&ctx, &id, patch).await {
            Ok(()) => {
                // Re-fetch through the trait so the response payload
                // mirrors the legacy SQLite path's "return the updated
                // row" wire shape.
                match app.store.get(&ctx, &id).await {
                    Ok(mem) => Json(json!(mem)).into_response(),
                    Err(_) => Json(json!({"updated": true, "id": id})).into_response(),
                }
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = state.lock().await;
    // Resolve prefix if exact ID not found
    let resolved_id = match db::resolve_id(&lock.0, &id) {
        Ok(Some(mem)) => mem.id,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("ambiguous ID prefix") {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
            }
            tracing::error!("handler error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };
    // Preserve existing agent_id when caller provides new metadata — provenance
    // is immutable after first write (see NHI design in crate::identity).
    let preserved_metadata = body.metadata.as_ref().map(|new_meta| {
        let existing_meta = db::get(&lock.0, &resolved_id).ok().flatten().map_or_else(
            || serde_json::Value::Object(serde_json::Map::new()),
            |m| m.metadata,
        );
        crate::identity::preserve_agent_id(&existing_meta, new_meta)
    });
    match db::update_with_expected_version(
        &lock.0,
        &resolved_id,
        body.title.as_deref(),
        body.content.as_deref(),
        body.tier.as_ref(),
        body.namespace.as_deref(),
        body.tags.as_ref(),
        body.priority,
        body.confidence,
        body.expires_at.as_deref(),
        preserved_metadata.as_ref(),
        body.source_uri.as_deref(),
        if_match_version,
    ) {
        Ok((true, _)) => {
            let mem = db::get(&lock.0, &resolved_id).ok().flatten();
            // Issue #219: regenerate the embedding when the searchable text
            // (title/content) changed. Without this, the semantic index keeps
            // pointing at the old vector and stale semantic recall results
            // linger even after the row is updated.
            let content_changed = body.title.is_some() || body.content.is_some();
            let mut lock_opt = Some(lock);
            if content_changed && let Some(ref m) = mem {
                let text = format!("{} {}", m.title, m.content);
                if let Some(emb) = app.embedder.as_ref().as_ref() {
                    match emb.embed(&text) {
                        Ok(vec) => {
                            if let Some(ref l) = lock_opt
                                && let Err(e) = db::set_embedding(&l.0, &resolved_id, &vec)
                            {
                                tracing::warn!(
                                    "failed to refresh embedding for {resolved_id}: {e}"
                                );
                            }
                            // Drop DB lock before touching vector index.
                            lock_opt.take();
                            let mut idx_lock = app.vector_index.lock().await;
                            if let Some(idx) = idx_lock.as_mut() {
                                idx.remove(&resolved_id);
                                idx.insert(resolved_id.clone(), vec);
                            }
                        }
                        Err(e) => tracing::warn!("embedding regeneration failed: {e}"),
                    }
                }
            }
            // Drop the DB lock before fanning out — peers POST back to
            // our sync_push so we'd deadlock if we held it.
            drop(lock_opt);
            // v0.6.0.1: fan out the mutation to peers so remote readers
            // see the update, not the pre-update row. insert_if_newer on
            // peers sees a newer updated_at and applies.
            if let (Some(fed), Some(m)) = (app.federation.as_ref(), mem.as_ref())
                && let Ok(tracker) = crate::federation::broadcast_store_quorum(fed, m).await
                && let Err(err) = crate::federation::finalise_quorum(&tracker)
            {
                // #869 — typed 503 envelope via the shared helper.
                let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                return super::quorum_not_met_response(&payload);
            }
            Json(json!(mem)).into_response()
        }
        Ok((false, _)) => {
            (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
        }
        Err(e) => {
            // v0.7.0 Provenance Gap 1 (#884) — typed VersionConflict
            // surfaces as 409 with a structured envelope naming both
            // expected + current versions so callers can re-read and
            // retry with the fresh version.
            if let Some(vc) = e.downcast_ref::<crate::storage::VersionConflict>() {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "status": "conflict",
                        "id": vc.id,
                        "expected_version": vc.expected,
                        "current_version": vc.current,
                        "error": e.to_string(),
                    })),
                )
                    .into_response();
            }
            let msg = e.to_string();
            if msg.contains("already exists in namespace") {
                return (StatusCode::CONFLICT, Json(json!({"error": msg}))).into_response();
            }
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

#[allow(clippy::too_many_lines)]
pub async fn delete_memory(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let state = app.db.clone();
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    // #913 (security-medium / SOC2, 2026-05-19) — admin/destructive
    // action audit. Memory delete is the canonical destructive operation;
    // the forensic-chain entry MUST land before the storage write so the
    // audit trail captures intent even when the downstream delete errors.
    // The existing `audit::emit(AuditAction::Delete)` further down writes
    // the SIEM-shaped enterprise audit row AFTER the delete commits;
    // these two channels are intentionally complementary.
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let caller_for_forensic = crate::identity::resolve_http_agent_id(None, header_agent_id)
        .unwrap_or_else(|_| "anonymous:invalid".to_string());
    crate::governance::audit::record_decision(
        &caller_for_forensic,
        "allow",
        "memory_delete",
        "",
        json!({ "id": &id }),
    );

    // v0.7.0 Wave-3 — Postgres-backed daemons dispatch through the
    // SAL trait. The legacy delete path threads governance, audit,
    // and federation fanout through the SQLite mutex; those layers
    // (governance owner-walk, audit chain, quorum broadcast) are
    // SQLite-bound today, so the postgres-eligible delete is the
    // simpler "delete by id" surface the SAL trait already provides.
    // Operators who need the full governance + audit + quorum bundle
    // on Postgres should follow the migration plan in
    // `docs/postgres-age-guide.md`.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        // Resolve the target memory before delete so the audit emit
        // captures namespace + title metadata (Phase 9 — audit emit
        // parity on postgres).
        let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
        let agent_id = crate::identity::resolve_http_agent_id(None, header_agent_id)
            .unwrap_or_else(|_| "ai:http".to_string());
        let ctx = crate::store::CallerContext::for_agent(agent_id.clone());
        let target = app.store.get(&ctx, &id).await.ok();

        // F-A2A1.2 (#700) — governance enforcement on the postgres delete
        // path. Mirrors the sqlite gate at line ~1913 below: a denied
        // delete returns 403; an `Approve`-level policy queues a pending
        // action and returns 202 Accepted. Without this gate the postgres
        // branch silently bypassed the namespace standard's `delete=`
        // rule, allowing any caller to delete a row in a governed
        // namespace. Closes the postgres half of the same surface S34/S60
        // exercise on the write path.
        if let Some(ref mem) = target {
            use crate::models::GovernanceDecision;
            let memory_owner = mem
                .metadata
                .get("agent_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let payload = json!({"id": mem.id, "title": mem.title});
            match app
                .store
                .enforce_governance_action(
                    crate::store::GovernedAction::Delete,
                    &mem.namespace,
                    &agent_id,
                    Some(&mem.id),
                    memory_owner.as_deref(),
                    &payload,
                )
                .await
            {
                Ok(GovernanceDecision::Allow) => {}
                Ok(GovernanceDecision::Deny(reason)) => {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({"error": format!("delete denied by governance: {reason}")})),
                    )
                        .into_response();
                }
                Ok(GovernanceDecision::Pending(pending_id)) => {
                    return (
                        StatusCode::ACCEPTED,
                        Json(json!({
                            "status": "pending",
                            "pending_id": pending_id,
                            "reason": "governance requires approval",
                            "action": "delete",
                            "memory_id": mem.id,
                            "storage_backend": "postgres",
                        })),
                    )
                        .into_response();
                }
                Err(e) => return store_err_to_response(e),
            }
        }

        return match app.store.delete(&ctx, &id).await {
            Ok(()) => {
                if crate::audit::is_enabled() {
                    let (namespace, title, tier) = target
                        .as_ref()
                        .map(|m| {
                            (
                                m.namespace.clone(),
                                Some(m.title.clone()),
                                Some(m.tier.to_string()),
                            )
                        })
                        .unwrap_or_else(|| (String::new(), None, None));
                    crate::audit::emit(crate::audit::EventBuilder::new(
                        crate::audit::AuditAction::Delete,
                        crate::audit::actor(agent_id, "http_header", None),
                        crate::audit::target_memory(id.clone(), namespace, title, tier, None),
                    ));
                }
                (StatusCode::OK, Json(json!({"deleted": true, "id": id}))).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = state.lock().await;
    // Resolve the target memory so governance has owner context.
    let target = match db::resolve_id(&lock.0, &id) {
        Ok(Some(m)) => m,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("ambiguous ID prefix") {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
            }
            tracing::error!("handler error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };

    // Task 1.9: governance enforcement (delete-side).
    {
        use crate::models::{GovernanceDecision, GovernedAction};
        let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
        let agent_id = match crate::identity::resolve_http_agent_id(None, header_agent_id) {
            Ok(a) => a,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid agent_id: {e}")})),
                )
                    .into_response();
            }
        };
        let mem_owner = target
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let payload = json!({"id": target.id, "title": target.title});
        match db::enforce_governance(
            &lock.0,
            GovernedAction::Delete,
            &target.namespace,
            &agent_id,
            Some(&target.id),
            mem_owner.as_deref(),
            &payload,
        ) {
            Ok(GovernanceDecision::Allow) => {}
            Ok(GovernanceDecision::Deny(reason)) => {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": format!("delete denied by governance: {reason}")})),
                )
                    .into_response();
            }
            Ok(GovernanceDecision::Pending(pending_id)) => {
                // v0.6.2 (S34): fan out the new pending delete row so peers
                // see consistent governance queue state.
                let pending_row = db::get_pending_action(&lock.0, &pending_id).ok().flatten();
                // v0.7.0 K4 — surface the new row through the
                // subscription dispatcher (`approval_requested`). See
                // the store-side companion call for rationale.
                crate::subscriptions::dispatch_approval_requested(&lock.0, &pending_id, &lock.1);
                let target_id = target.id.clone();
                drop(lock);
                if let (Some(pa), Some(fed)) = (pending_row.as_ref(), app.federation.as_ref()) {
                    match crate::federation::broadcast_pending_quorum(fed, pa).await {
                        Ok(tracker) => {
                            if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                                // #869 — typed 503 envelope via the shared helper.
                                let payload =
                                    crate::federation::QuorumNotMetPayload::from_err(&err);
                                return super::quorum_not_met_response(&payload);
                            }
                        }
                        Err(err) => {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return super::quorum_not_met_response(&payload);
                        }
                    }
                }
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({
                        "status": "pending",
                        "pending_id": pending_id,
                        "reason": "governance requires approval",
                        "action": "delete",
                        "memory_id": target_id,
                    })),
                )
                    .into_response();
            }
            Err(e) => {
                tracing::error!("governance error: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "governance check failed"})),
                )
                    .into_response();
            }
        }
    }

    let delete_outcome = db::delete(&lock.0, &target.id);
    // v0.6.4-017 — G9 HTTP webhook parity. Fire `memory_delete` after
    // the row is gone (mirrors the MCP pattern at mcp.rs:2227). Snapshot
    // fields come from the pre-delete `target`. Best-effort,
    // fire-and-forget: dispatch does a quick subscriber lookup on the
    // current connection and spawns a thread for the HTTP POST so the
    // response is never blocked. Held inside the lock so the subscriber
    // list query has a connection — release happens after.
    if matches!(delete_outcome, Ok(true)) {
        let details = serde_json::to_value(crate::subscriptions::DeleteEventDetails {
            title: target.title.clone(),
            tier: target.tier.to_string(),
        })
        .ok();
        let owner_aid = target
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        crate::subscriptions::dispatch_event_with_details(
            &lock.0,
            "memory_delete",
            &target.id,
            &target.namespace,
            owner_aid.as_deref(),
            &lock.1,
            details,
        );
    }
    // Drop DB lock before fanning out — peers POST back to our
    // sync_push and we'd deadlock on the shared Mutex if we held it.
    drop(lock);
    match delete_outcome {
        Ok(true) => {
            // PR-5 (issue #487): security audit trail for HTTP delete.
            let owner = target
                .metadata
                .get("agent_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    headers
                        .get("x-agent-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("anonymous")
                        .to_string()
                });
            crate::audit::emit(crate::audit::EventBuilder::new(
                crate::audit::AuditAction::Delete,
                crate::audit::actor(owner, "http_header", None),
                crate::audit::target_memory(
                    target.id.clone(),
                    target.namespace.clone(),
                    Some(target.title.clone()),
                    Some(target.tier.to_string()),
                    None,
                ),
            ));
            // v0.6.0.1: propagate tombstone via sync_push.deletions.
            if let Some(fed) = app.federation.as_ref()
                && let Ok(tracker) =
                    crate::federation::broadcast_delete_quorum(fed, &target.id).await
                && let Err(err) = crate::federation::finalise_quorum(&tracker)
            {
                // #869 — typed 503 envelope via the shared helper.
                let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                return super::quorum_not_met_response(&payload);
            }
            Json(json!({"deleted": true})).into_response()
        }
        _ => (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response(),
    }
}

#[allow(clippy::too_many_lines)]
pub async fn promote_memory(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let state = app.db.clone();
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 Continuation 5 (state-flake / S16+S49) — postgres-
    // backed daemons resolve the memory through the SAL trait so a
    // freshly-stored row promotes correctly across daemon restart.
    // Without this branch the handler reaches into the scratch SQLite
    // db (`:memory:` in test, stale on droplet after disposable DB
    // reset) and returns 404 — the documented Wave 4 R2 flake.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
        let agent_id = match crate::identity::resolve_http_agent_id(None, header_agent_id) {
            Ok(a) => a,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid agent_id: {e}")})),
                )
                    .into_response();
            }
        };
        let ctx = crate::store::CallerContext::for_agent(&agent_id);
        // F-A2A1.4 (#700, S16/S49) — bounded retry on NotFound. A
        // freshly-stored row that travelled through a read replica or
        // is still settling in WAL flush can briefly return
        // NotFound from the SAL `get`. The 22-failure triage (memory
        // 9ffaa55d) classified this as Bucket-A: the row exists, the
        // promote handler just races the visibility window. Retry up
        // to 4 times with bounded backoff (5/10/15/20 ms — 50 ms
        // total) before surfacing 404 — well below the 2 s daemon
        // p99 SLO and dwarfed by typical store-side replication
        // latency. See `get_with_visibility_retry` for the helper.
        let target =
            match super::http::get_with_visibility_retry(app.store.as_ref(), &ctx, &id).await {
                Ok(m) => m,
                Err(crate::store::StoreError::NotFound { .. }) => {
                    return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"})))
                        .into_response();
                }
                Err(e) => return store_err_to_response(e),
            };

        // F-A2A1.2 (#700) — governance enforcement on the postgres promote
        // path. Mirrors the sqlite gate at line ~2169 below: an `owner`
        // policy on the namespace standard denies a non-owner promote
        // (403); an `approve`-level policy queues a pending action (202).
        // The postgres branch previously skipped this gate, letting any
        // caller promote a row to `long` tier regardless of namespace
        // governance.
        {
            use crate::models::GovernanceDecision;
            let memory_owner = target
                .metadata
                .get("agent_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let payload = json!({"id": target.id});
            match app
                .store
                .enforce_governance_action(
                    crate::store::GovernedAction::Promote,
                    &target.namespace,
                    &agent_id,
                    Some(&target.id),
                    memory_owner.as_deref(),
                    &payload,
                )
                .await
            {
                Ok(GovernanceDecision::Allow) => {}
                Ok(GovernanceDecision::Deny(reason)) => {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({"error": format!("promote denied by governance: {reason}")})),
                    )
                        .into_response();
                }
                Ok(GovernanceDecision::Pending(pending_id)) => {
                    return (
                        StatusCode::ACCEPTED,
                        Json(json!({
                            "status": "pending",
                            "pending_id": pending_id,
                            "reason": "governance requires approval",
                            "action": "promote",
                            "memory_id": target.id,
                            "storage_backend": "postgres",
                        })),
                    )
                        .into_response();
                }
                Err(e) => return store_err_to_response(e),
            }
        }

        let patch = crate::store::UpdatePatch {
            tier: Some(Tier::Long),
            ..Default::default()
        };
        return match app.store.update(&ctx, &target.id, patch).await {
            Ok(()) => {
                // F-A2A1.4 (#700, S16/S49) — post-promote federation
                // fanout on the postgres branch. Mirrors the sqlite
                // path at lines ~2406-2417: after a successful local
                // tier-update, re-fetch the row to capture the new
                // tier + cleared expiry and broadcast via
                // `broadcast_store_quorum` so peers' projections of
                // the same memory inherit the tier ladder. Without
                // this, a `notify` recipient on peer-B still sees the
                // row at its pre-promote tier and a recall against
                // `tier=long` on peer-B silently misses it.
                //
                // Failure handling: fanout failures surface as 503
                // with `Retry-After: 2` mirroring sqlite. The local
                // tier update has already committed — per ADR-0001
                // we do NOT roll back the local commit on quorum
                // failure; the sync daemon's eventual-consistency
                // loop catches stragglers.
                if let Some(fed) = app.federation.as_ref() {
                    let promoted_mem = match app.store.get(&ctx, &target.id).await {
                        Ok(m) => Some(m),
                        Err(_) => None,
                    };
                    if let Some(ref m) = promoted_mem {
                        match crate::federation::broadcast_store_quorum(fed, m).await {
                            Ok(tracker) => {
                                if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                                    // #869 — typed 503 envelope via the shared helper.
                                    let payload =
                                        crate::federation::QuorumNotMetPayload::from_err(&err);
                                    return super::quorum_not_met_response(&payload);
                                }
                            }
                            Err(err) => {
                                let payload =
                                    crate::federation::QuorumNotMetPayload::from_err(&err);
                                return super::quorum_not_met_response(&payload);
                            }
                        }
                    }
                }
                Json(json!({
                    "promoted": true,
                    "id": target.id,
                    "tier": "long",
                    "storage_backend": "postgres",
                }))
                .into_response()
            }
            Err(crate::store::StoreError::NotFound { .. }) => {
                (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = state.lock().await;
    // Resolve prefix if exact ID not found — capture full memory for governance.
    let target = match db::resolve_id(&lock.0, &id) {
        Ok(Some(mem)) => mem,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response();
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("ambiguous ID prefix") {
                return (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
            }
            tracing::error!("handler error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };
    // Task 1.9: governance enforcement (promote-side).
    {
        use crate::models::{GovernanceDecision, GovernedAction};
        let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
        let agent_id = match crate::identity::resolve_http_agent_id(None, header_agent_id) {
            Ok(a) => a,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid agent_id: {e}")})),
                )
                    .into_response();
            }
        };
        let mem_owner = target
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let payload = json!({"id": target.id});
        match db::enforce_governance(
            &lock.0,
            GovernedAction::Promote,
            &target.namespace,
            &agent_id,
            Some(&target.id),
            mem_owner.as_deref(),
            &payload,
        ) {
            Ok(GovernanceDecision::Allow) => {}
            Ok(GovernanceDecision::Deny(reason)) => {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": format!("promote denied by governance: {reason}")})),
                )
                    .into_response();
            }
            Ok(GovernanceDecision::Pending(pending_id)) => {
                // v0.6.2 (S34): fan out the new pending promote row too.
                let pending_row = db::get_pending_action(&lock.0, &pending_id).ok().flatten();
                // v0.7.0 K4 — surface the new row through the
                // subscription dispatcher (`approval_requested`). See
                // the store-side companion call for rationale.
                crate::subscriptions::dispatch_approval_requested(&lock.0, &pending_id, &lock.1);
                let target_id = target.id.clone();
                drop(lock);
                if let (Some(pa), Some(fed)) = (pending_row.as_ref(), app.federation.as_ref()) {
                    match crate::federation::broadcast_pending_quorum(fed, pa).await {
                        Ok(tracker) => {
                            if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                                // #869 — typed 503 envelope via the shared helper.
                                let payload =
                                    crate::federation::QuorumNotMetPayload::from_err(&err);
                                return super::quorum_not_met_response(&payload);
                            }
                        }
                        Err(err) => {
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return super::quorum_not_met_response(&payload);
                        }
                    }
                }
                return (
                    StatusCode::ACCEPTED,
                    Json(json!({
                        "status": "pending",
                        "pending_id": pending_id,
                        "reason": "governance requires approval",
                        "action": "promote",
                        "memory_id": target_id,
                    })),
                )
                    .into_response();
            }
            Err(e) => {
                tracing::error!("governance error: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "governance check failed"})),
                )
                    .into_response();
            }
        }
    }

    let resolved_id = target.id.clone();
    match db::update(
        &lock.0,
        &resolved_id,
        None,
        None,
        Some(&Tier::Long),
        None,
        None,
        None,
        None,
        None,
        None,
    ) {
        Ok((true, _)) => {
            if let Err(e) = lock.0.execute(
                "UPDATE memories SET expires_at = NULL WHERE id = ?1",
                rusqlite::params![resolved_id],
            ) {
                tracing::error!("promote clear expiry failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal server error"})),
                )
                    .into_response();
            }
            // v0.6.0.1: fan out the promoted memory so peers pick up the
            // new tier + cleared expiry via insert_if_newer's newer-wins merge.
            let promoted_mem = db::get(&lock.0, &resolved_id).ok().flatten();
            // v0.6.4-017 — G9 HTTP webhook parity. Fire `memory_promote`
            // (tier mode — HTTP only does tier promotion, MCP also does
            // vertical). Mirrors mcp.rs:2369 pattern.
            let owner_aid = target
                .metadata
                .get("agent_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let details = serde_json::to_value(crate::subscriptions::PromoteEventDetails {
                mode: "tier".to_string(),
                tier: Some("long".to_string()),
                to_namespace: None,
                clone_id: None,
            })
            .ok();
            crate::subscriptions::dispatch_event_with_details(
                &lock.0,
                "memory_promote",
                &resolved_id,
                &target.namespace,
                owner_aid.as_deref(),
                &lock.1,
                details,
            );
            drop(lock);
            if let (Some(fed), Some(m)) = (app.federation.as_ref(), promoted_mem.as_ref())
                && let Ok(tracker) = crate::federation::broadcast_store_quorum(fed, m).await
                && let Err(err) = crate::federation::finalise_quorum(&tracker)
            {
                // #869 — typed 503 envelope via the shared helper.
                let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                return super::quorum_not_met_response(&payload);
            }
            Json(json!({"promoted": true, "id": resolved_id, "tier": "long"})).into_response()
        }
        Ok((false, _)) => {
            (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))).into_response()
        }
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}
