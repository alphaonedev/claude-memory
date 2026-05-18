// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! HTTP parity helpers shared across handler modules.
//!
//! `fanout_or_503` — fan out a locally-committed memory to peers via
//! quorum store. Used by `create_memory`, `update_memory`, and the bulk
//! endpoints in `handlers::http`.
//!
//! `resolve_caller_agent_id` — the HTTP precedence chain for caller
//! `agent_id` resolution (body → query → header → anonymous fallback).
//! Used by every HTTP handler that needs an identified caller.
//!
//! Both helpers were extracted from `src/handlers/mod.rs` as part of the
//! issue #650 file-architecture cleanup.

use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};

use super::transport::AppState;
use crate::models::Memory;
use crate::validate;

/// Fan out a locally-committed memory to peers via quorum store. On success,
/// returns `None`; on quorum miss, returns `Some(503_response)` for the
/// caller to short-circuit with. Network errors are logged and swallowed —
/// the local commit already landed and the sync-daemon catches stragglers.
pub(crate) async fn fanout_or_503(
    app: &AppState,
    mem: &Memory,
) -> Option<axum::response::Response> {
    let fed = app.federation.as_ref().as_ref()?;
    match crate::federation::broadcast_store_quorum(fed, mem).await {
        Ok(tracker) => match crate::federation::finalise_quorum(&tracker) {
            Ok(_) => None,
            Err(err) => {
                let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                Some(
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [("Retry-After", "2")],
                        Json(serde_json::to_value(&payload).unwrap_or_default()),
                    )
                        .into_response(),
                )
            }
        },
        Err(e) => {
            tracing::warn!("fanout error (local committed): {e:?}");
            None
        }
    }
}

/// Helper — resolve the caller's `agent_id` using the HTTP precedence chain,
/// accepting an optional body value, the `X-Agent-Id` header, and an optional
/// `?agent_id=` query param. Returns a 400 on invalid input; synthesizes an
/// anonymous id on miss.
pub(crate) fn resolve_caller_agent_id(
    body: Option<&str>,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<String, String> {
    // Body → query → header (body wins, query next, header last). Matches the
    // precedence already used by `register_agent` / `create_memory` with
    // query inserted at the same tier as body for handlers that read from
    // the querystring (e.g. GET /inbox?agent_id=...).
    if let Some(id) = body
        && !id.is_empty()
    {
        validate::validate_agent_id(id).map_err(|e| format!("invalid agent_id: {e}"))?;
        return Ok(id.to_string());
    }
    if let Some(id) = query
        && !id.is_empty()
    {
        validate::validate_agent_id(id).map_err(|e| format!("invalid agent_id: {e}"))?;
        return Ok(id.to_string());
    }
    let header_val = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    crate::identity::resolve_http_agent_id(None, header_val)
        .map_err(|e| format!("invalid agent_id: {e}"))
}
