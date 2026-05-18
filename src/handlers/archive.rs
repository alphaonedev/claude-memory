// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! HTTP handlers for the archive surface (#650 follow-up per-domain
//! split). The five archive endpoints listed below were extracted
//! verbatim from `src/handlers/http.rs` (commit 88d9a96, lines
//! 7196-7558). Wire compatibility is preserved via the
//! `pub use archive::*` re-export from `src/handlers/mod.rs`; the
//! Axum router registrations in `src/lib.rs` are unchanged.
//!
//! Routes wired here:
//!
//! * `GET    /api/v1/archive`              → [`list_archive`]
//! * `POST   /api/v1/archive`              → [`archive_by_ids`]
//! * `DELETE /api/v1/archive`              → [`purge_archive`]
//! * `POST   /api/v1/archive/{id}/restore` → [`restore_archive`]
//! * `GET    /api/v1/archive/stats`        → [`archive_stats`]

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::json;

use crate::db;
use crate::validate;

use super::AppState;
use super::MAX_BULK_SIZE;
#[cfg(feature = "sal")]
use super::StorageBackend;
#[cfg(feature = "sal")]
use super::store_err_to_response;

#[derive(Debug, Deserialize)]
pub struct ArchiveListQuery {
    pub namespace: Option<String>,
    #[serde(default = "default_archive_limit")]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[allow(clippy::unnecessary_wraps)]
fn default_archive_limit() -> Option<usize> {
    Some(50)
}

pub async fn list_archive(
    State(app): State<AppState>,
    Query(q): Query<ArchiveListQuery>,
) -> impl IntoResponse {
    // Ultrareview #350: validate limit range. `usize` already precludes
    // negative values at the serde layer, but `limit=0` silently
    // returned an empty page — indistinguishable from "no results".
    // Require 1..=1000 and reject 0 with a specific error.
    if matches!(q.limit, Some(0)) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "limit must be >= 1"})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 Continuation — postgres-backed daemons project from
    // the `archived_memories` table via the SAL adapter. The trait does
    // not yet expose archive operations, so we dispatch via the typed
    // `PostgresStore::list_archived` helper added under feature
    // `sal-postgres`. Returns the same wire envelope as sqlite.
    #[cfg(feature = "sal-postgres")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let limit = q.limit.unwrap_or(50).clamp(1, 1000);
        let offset = q.offset.unwrap_or(0);
        return match crate::store::postgres::list_archived_via_store(
            &app.store,
            q.namespace.as_deref(),
            limit,
            offset,
        )
        .await
        {
            Ok(items) => Json(json!({"archived": items, "count": items.len()})).into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    let offset = q.offset.unwrap_or(0);
    match db::list_archived(&lock.0, q.namespace.as_deref(), limit, offset) {
        Ok(items) => Json(json!({"archived": items, "count": items.len()})).into_response(),
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

pub async fn restore_archive(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    // v0.7.0 Wave-3 Continuation 3 (Phase 19) — postgres-backed daemons
    // route through the SAL `archive_restore` trait method. Federation
    // fanout for restore stays sqlite-only (the `broadcast_restore_quorum`
    // path uses sqlite-coupled fed-tracker state); postgres-backed
    // operators relying on multi-node consistency should poll peers.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("http");
        return match app.store.archive_restore(&ctx, &id).await {
            Ok(true) => Json(json!({"restored": true, "id": id, "storage_backend": "postgres"}))
                .into_response(),
            Ok(false) => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "not found in archive"})),
            )
                .into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let restored = {
        let lock = app.db.lock().await;
        match db::restore_archived(&lock.0, &id) {
            Ok(v) => v,
            Err(e) => {
                tracing::error!("handler error: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal server error"})),
                )
                    .into_response();
            }
        }
    };
    if !restored {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "not found in archive"})),
        )
            .into_response();
    }

    // v0.6.2 (S29): broadcast the restore to peers so they move the row
    // from `archived_memories` → `memories` in lockstep. Without this, a
    // POST /api/v1/archive/{id}/restore on node-1 leaves node-2..4 with
    // the row still archived, so node-4 never sees M1 re-enter the active
    // set (the testbook-v3 S29 assertion). Same posture as
    // `archive_by_ids`: on a quorum miss we short-circuit with 503 so
    // operators can retry.
    if let Some(fed) = app.federation.as_ref() {
        match crate::federation::broadcast_restore_quorum(fed, &id).await {
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
            Err(e) => {
                // Local commit already landed — sync-daemon catches
                // stragglers. Same posture as `fanout_or_503`.
                tracing::warn!("restore fanout error (local committed): {e:?}");
            }
        }
    }

    Json(json!({"restored": true, "id": id})).into_response()
}

#[derive(Debug, Deserialize)]
pub struct PurgeQuery {
    pub older_than_days: Option<i64>,
}

pub async fn purge_archive(
    State(app): State<AppState>,
    Query(q): Query<PurgeQuery>,
) -> impl IntoResponse {
    // v0.7.0 Wave-3 Continuation 3 (Phase 19) — postgres-backed daemons
    // route through the SAL trait. Wire shape preserved: `{purged}`.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        return match app.store.archive_purge(q.older_than_days).await {
            Ok(n) => Json(json!({"purged": n, "storage_backend": "postgres"})).into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::purge_archive(&lock.0, q.older_than_days) {
        Ok(n) => Json(json!({"purged": n})).into_response(),
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

pub async fn archive_stats(State(app): State<AppState>) -> impl IntoResponse {
    // v0.7.0 Wave-3 Continuation — postgres-backed daemons aggregate
    // counts directly from the `archived_memories` table.
    #[cfg(feature = "sal-postgres")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        return match crate::store::postgres::archive_stats_via_store(&app.store).await {
            Ok(v) => Json(v).into_response(),
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::archive_stats(&lock.0) {
        Ok(archive_stats) => Json(archive_stats).into_response(),
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

/// Request body for `POST /api/v1/archive` — S29 explicit archive.
#[derive(Debug, Deserialize)]
pub struct ArchiveByIdsBody {
    pub ids: Vec<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// POST /api/v1/archive — explicit archive of the given memory ids
/// (S29). For each id:
///   1. Call `db::archive_memory` locally to soft-move the row.
///   2. If federation is configured, broadcast via
///      `broadcast_archive_quorum` so peers land in the same terminal
///      state (row out of `memories`, row into `archived_memories`).
///
/// On a quorum miss for ANY id, short-circuit with 503 via the shared
/// `fanout_or_503`-style payload. This matches the posture of the
/// delete + consolidate fanout endpoints.
///
/// Response body:
/// ```json
/// {"archived": [id1, id2], "missing": [id3], "count": 2}
/// ```
/// where `missing` enumerates ids that had no live row locally (common
/// during retries). The response never includes content/metadata — use
/// `GET /api/v1/archive` to list archive entries.
#[allow(clippy::too_many_lines)]
pub async fn archive_by_ids(
    State(app): State<AppState>,
    Json(body): Json<ArchiveByIdsBody>,
) -> impl IntoResponse {
    // Bound the batch the same way bulk_create / sync_push do.
    if body.ids.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("archive limited to {} ids per request", MAX_BULK_SIZE)})),
        )
            .into_response();
    }
    // Validate all ids up-front so we never start mutating on a bad batch.
    for id in &body.ids {
        if let Err(e) = validate::validate_id(id) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid id {id}: {e}")})),
            )
                .into_response();
        }
    }
    let reason = body.reason.as_deref().unwrap_or("archive").to_string();
    let mut archived: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();

    // v0.7.0 Wave-3 Continuation 3 (Phase 19) — postgres-backed daemons
    // route through the SAL `archive_by_ids` trait method. The federation
    // fanout stays sqlite-only; postgres operators relying on multi-node
    // consistency should poll peers.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("http");
        // Run per-id so we can split archived vs missing — the trait
        // method bulk-archives but doesn't tell us which were missing,
        // so we probe each via the count delta.
        for id in &body.ids {
            match app
                .store
                .archive_by_ids(&ctx, std::slice::from_ref(id), Some(&reason))
                .await
            {
                Ok(1) => archived.push(id.clone()),
                Ok(_) => missing.push(id.clone()),
                Err(e) => return store_err_to_response(e),
            }
        }
        return (
            StatusCode::OK,
            Json(json!({
                "archived": archived,
                "missing": missing,
                "count": archived.len(),
                "reason": reason,
                "storage_backend": "postgres",
            })),
        )
            .into_response();
    }

    for id in &body.ids {
        // Local archive. Hold the lock only across this one call per id so
        // we can release it before a potentially slow network fanout.
        let moved = {
            let lock = app.db.lock().await;
            match db::archive_memory(&lock.0, id, Some(&reason)) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!("archive_by_ids: archive_memory({id}) failed: {e}");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "internal server error"})),
                    )
                        .into_response();
                }
            }
        };
        if !moved {
            // Row wasn't live locally — record as missing but keep going.
            // Do NOT fan out (peers can't know to archive from a row they
            // may have under a different state; the originator's local
            // state is the trigger).
            missing.push(id.clone());
            continue;
        }

        // Fanout. Mirror the shape used by the other
        // quorum-backed write endpoints (delete, consolidate) — on a
        // miss, surface the `quorum_not_met` payload with 503 + Retry-After.
        if let Some(fed) = app.federation.as_ref() {
            match crate::federation::broadcast_archive_quorum(fed, id).await {
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
                Err(e) => {
                    // Local commit already landed — sync-daemon catches
                    // stragglers. Same posture as `fanout_or_503`.
                    tracing::warn!("archive fanout error (local committed): {e:?}");
                }
            }
        }
        archived.push(id.clone());
    }

    (
        StatusCode::OK,
        Json(json!({
            "archived": archived,
            "missing": missing,
            "count": archived.len(),
            "reason": reason,
        })),
    )
        .into_response()
}
