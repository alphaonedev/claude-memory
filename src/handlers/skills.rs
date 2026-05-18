// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! HTTP handlers for the v0.7.0 skills surface (#650 follow-up
//! per-domain split). Each handler is a thin Axum-layer wrapper that
//! transforms request data into the canonical JSON params the
//! underlying MCP `handle_skill_*` substrate functions expect, then
//! shapes their `Result<Value, String>` into the appropriate HTTP
//! status code.
//!
//! All handlers were extracted verbatim from `src/handlers/http.rs`
//! (commit 88d9a96, lines 7591-7782); wire compatibility is preserved
//! via the `pub use skills::*` re-export from `src/handlers/mod.rs`.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;

/// `POST /api/v1/skill` — register a new skill from an inline body.
pub async fn skill_register_route(
    State(app): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let lock = app.db.lock().await;
    let kp = (*app.active_keypair).as_ref();
    match crate::mcp::handle_skill_register(&lock.0, &body, kp) {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    }
}

/// `GET /api/v1/skill/list?namespace=<ns>&filter=<text>`.
///
/// Query params mirror the MCP `namespace` and `filter` keys.
#[derive(Deserialize)]
pub struct SkillListQuery {
    pub namespace: Option<String>,
    pub filter: Option<String>,
}

pub async fn skill_list_route(
    State(app): State<AppState>,
    Query(q): Query<SkillListQuery>,
) -> impl IntoResponse {
    let mut params = json!({});
    if let Some(ns) = q.namespace {
        params["namespace"] = json!(ns);
    }
    if let Some(f) = q.filter {
        params["filter"] = json!(f);
    }
    let lock = app.db.lock().await;
    match crate::mcp::handle_skill_list(&lock.0, &params) {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

/// `GET /api/v1/skill/{id}` — full activation payload (body included).
pub async fn skill_get_route(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let params = json!({"skill_id": id});
    let lock = app.db.lock().await;
    match crate::mcp::handle_skill_get(&lock.0, &params) {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => {
            // Substrate uses a "skill not found:" prefix for the missing
            // case; surface that as 404. Everything else is 500.
            if e.starts_with("skill not found") {
                (StatusCode::NOT_FOUND, Json(json!({"error": e}))).into_response()
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response()
            }
        }
    }
}

/// `GET /api/v1/skill/{id}/resource?path=<resource_path>`.
#[derive(Deserialize)]
pub struct SkillResourceQuery {
    pub path: String,
}

pub async fn skill_resource_route(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<SkillResourceQuery>,
) -> impl IntoResponse {
    let params = json!({
        "skill_id": id,
        "resource_path": q.path,
    });
    let lock = app.db.lock().await;
    match crate::mcp::handle_skill_resource(&lock.0, &params) {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => {
            if e.starts_with("resource not found") {
                (StatusCode::NOT_FOUND, Json(json!({"error": e}))).into_response()
            } else {
                (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response()
            }
        }
    }
}

/// `POST /api/v1/skill/{id}/export`.
///
/// Body: `{ "target_folder": "<path>" }`. The path is resolved on the
/// daemon host, so the operator must ensure it's writable by the
/// daemon user.
#[derive(Deserialize)]
pub struct SkillExportBody {
    pub target_folder: String,
}

pub async fn skill_export_route(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SkillExportBody>,
) -> impl IntoResponse {
    let params = json!({
        "skill_id": id,
        "target_folder": body.target_folder,
    });
    let lock = app.db.lock().await;
    let kp = (*app.active_keypair).as_ref();
    match crate::mcp::handle_skill_export(&lock.0, &params, kp) {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => {
            if e.starts_with("skill not found") {
                (StatusCode::NOT_FOUND, Json(json!({"error": e}))).into_response()
            } else {
                (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response()
            }
        }
    }
}

/// `POST /api/v1/skill/{id}/promote`.
///
/// Path `{id}` is the source **reflection** id (not a skill id — the
/// promote verb consumes a reflection and produces a skill). Body
/// carries the new skill's `name`, `description`, and optional
/// `parameters_schema`.
#[derive(Deserialize)]
pub struct SkillPromoteBody {
    pub name: String,
    pub description: String,
    pub parameters_schema: Option<serde_json::Value>,
}

pub async fn skill_promote_route(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SkillPromoteBody>,
) -> impl IntoResponse {
    let mut params = json!({
        "reflection_id": id,
        "skill_name": body.name,
        "skill_description": body.description,
    });
    if let Some(ps) = body.parameters_schema {
        params["parameters_schema"] = ps;
    }
    let lock = app.db.lock().await;
    let kp = (*app.active_keypair).as_ref();
    match crate::mcp::handle_skill_promote_from_reflection(&lock.0, &params, kp) {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => {
            if e.contains("not found") {
                (StatusCode::NOT_FOUND, Json(json!({"error": e}))).into_response()
            } else {
                (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response()
            }
        }
    }
}

/// `POST /api/v1/skill/{id}/compose`.
///
/// Body: `{ "budget_tokens": <N?> }`. Returns the skill body plus the
/// reflections declared in its `composes_with_reflections` frontmatter.
#[derive(Deserialize, Default)]
pub struct SkillComposeBody {
    pub budget_tokens: Option<u64>,
}

pub async fn skill_compose_route(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<SkillComposeBody>>,
) -> impl IntoResponse {
    let Json(body) = body.unwrap_or(Json(SkillComposeBody::default()));
    let mut params = json!({"skill_id": id});
    if let Some(b) = body.budget_tokens {
        params["budget_tokens"] = json!(b);
    }
    let lock = app.db.lock().await;
    match crate::mcp::handle_skill_compositional_context(&lock.0, &params) {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => {
            if e.starts_with("skill not found") {
                (StatusCode::NOT_FOUND, Json(json!({"error": e}))).into_response()
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response()
            }
        }
    }
}
