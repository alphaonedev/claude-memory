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
//! `quorum_not_met_response` — issue #869: build the canonical 503 +
//! `Retry-After: 2` response from a `QuorumNotMetPayload`. Collapses
//! the ~30 inline `Json(serde_json::to_value(&payload).unwrap_or_default())`
//! sites scattered across the per-domain handler modules into a single
//! typed helper so a future encoder regression cannot silently degrade
//! the 503 envelope to `null`. `QuorumNotMetPayload` is a flat struct
//! with `&'static str` + `usize` + `usize` + `String` fields, so
//! serialisation is mathematically infallible at runtime; the helper
//! still routes through [`super::to_value_or_500`] so that if a future
//! payload-shape change introduces a fallible serialise path the
//! handlers fail-closed with a typed 500 instead of `null` (the prior
//! `unwrap_or_default` would have produced `serde_json::Value::Null`).
//!
//! All three helpers were extracted from `src/handlers/mod.rs` as part
//! of the issue #650 file-architecture cleanup.

use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};

use super::transport::AppState;
use crate::federation::QuorumNotMetPayload;
use crate::models::Memory;
use crate::validate;

/// Build the canonical `503 Service Unavailable` + `Retry-After: 2`
/// response from a `QuorumNotMetPayload`. Issue #869.
///
/// Wire-compatibility: emits the same `{error, got, needed, reason}`
/// shape that the inline call sites previously produced. The status,
/// header, and body are unchanged from the pre-#869 inline pattern.
pub(crate) fn quorum_not_met_response(payload: &QuorumNotMetPayload) -> axum::response::Response {
    let body = super::to_value_or_500("quorum_not_met_response", payload);
    match body {
        Ok(v) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [("Retry-After", "2")],
            Json(v),
        )
            .into_response(),
        Err(resp) => resp,
    }
}

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
                // #869 — route through the shared helper so a future
                // serialise regression cannot mask the quorum failure
                // with a `Value::Null` body paired with a 503.
                let payload = QuorumNotMetPayload::from_err(&err);
                Some(quorum_not_met_response(&payload))
            }
        },
        Err(e) => {
            tracing::warn!("fanout error (local committed): {e:?}");
            None
        }
    }
}

/// Helper — resolve the caller's `agent_id` using the HTTP precedence chain.
///
/// # SECURITY (v0.7.0 — header-first; body and query must match)
///
/// The `X-Agent-Id` request header is the AUTHORITATIVE identity slot.
/// The optional `body` and `query` slots are caller-controlled and so
/// cannot be trusted as precedence inputs; they are accepted as
/// REFINEMENTS that MUST agree with the header-resolved id. A mismatch
/// returns a `agent_id_body_header_mismatch` / `agent_id_query_header_mismatch`
/// error so handlers can map it to `403 Forbidden`.
///
/// Pre-v0.7.0 precedence was `body → query → header` (body wins),
/// which was the #874-class spoof vector that the v0.7.0 fix series
/// closed at every CALLER. The structural fix lives in
/// [`crate::identity::resolve_http_agent_id`]; this wrapper mirrors
/// the same posture for the additional `query` slot some handlers
/// accept (e.g. `GET /inbox?agent_id=...`).
///
/// Returns a 400-mapped string error on invalid input; a 403-mapped
/// string error tagged `agent_id_*_header_mismatch` on body/query
/// disagreement; synthesizes an anonymous `anonymous:req-…` id on
/// total miss (no body, no query, no header) so the upstream handler
/// can decide whether anonymous writes are allowed.
pub(crate) fn resolve_caller_agent_id(
    body: Option<&str>,
    headers: &HeaderMap,
    query: Option<&str>,
) -> Result<String, String> {
    // 1. Header (or anonymous fallback) is authoritative. Delegate to
    //    the identity primitive so the body-match check there runs once.
    let header_val = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let resolved = crate::identity::resolve_http_agent_id(body, header_val)
        .map_err(|e| format!("invalid agent_id: {e}"))?;

    // 2. Query refinement — same posture as body: when non-empty it
    //    MUST match the authoritative resolved id. Validate first so a
    //    malformed query surfaces as the more informative validation
    //    error rather than as a mismatch.
    if let Some(claim) = query
        && !claim.is_empty()
    {
        validate::validate_agent_id(claim).map_err(|e| format!("invalid agent_id: {e}"))?;
        if claim != resolved {
            return Err(format!(
                "agent_id_query_header_mismatch: query-supplied agent_id {claim:?} disagrees \
                 with authenticated header-resolved id {resolved:?}"
            ));
        }
    }

    Ok(resolved)
}
