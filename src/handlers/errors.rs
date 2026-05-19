// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! HTTP error-sanitization helpers — issue #851 (Wave-2 Tier-A3 SECURITY).
//!
//! HTTP handler responses were echoing raw db/serde/federation error strings
//! to unauthenticated callers, exposing SQL fragments, constraint names,
//! peer URLs, agent_ids, and user-supplied ids paired with internal error
//! detail. The 3 helpers below replace the prior inline patterns at every
//! leak site (15 fixed, 3 deferred to operator review per the audit
//! finding doc).
//!
//! `sanitize_bulk_row_error` is invoked at per-row bulk-endpoint failure
//! sites (POST /import, POST /memories/bulk) where each row's error
//! previously surfaced verbatim in an `errors[]` array. It maps the raw
//! string to one of five allowlisted classification labels (validation /
//! conflict / not found / forbidden / replication unavailable) so the
//! caller still learns the failure CATEGORY (validation vs conflict vs
//! internal) without echoing the raw inner detail. The full detail is
//! always logged via `tracing::warn!` so operators can debug.
//!
//! `internal_error_response` is the analogue for top-level 5xx responses:
//! it logs the raw error at `error` level and returns the canonical
//! "internal server error" JSON body. Used at sites where the prior code
//! pushed an `e.to_string()` straight into the response body.
//!
//! `bad_request_opaque` is the 400 analogue for sites that previously
//! forwarded an `mcp::handle_*` `Result<_, String>` error verbatim, where
//! the inner string includes raw rusqlite text from
//! `db::insert(...).map_err(|e| e.to_string())` calls inside the MCP
//! handler.
//!
//! Wire compatibility is preserved: response shape stays
//! `{"error": "<message>"}` and HTTP status codes are unchanged. Only the
//! CONTENT of the message is hardened.

use axum::{Json, http::StatusCode, response::IntoResponse};
use serde_json::json;

/// Sanitize a per-row error message that originated in a bulk endpoint
/// (`bulk_create`, `import_memories`, federation fanout). Returns a short
/// classification string safe to echo to an unauthenticated caller.
///
/// The classifier matches on stable substrings produced by `validate::*`,
/// `db::*`, and `crate::federation::*`. Anything that doesn't match falls
/// back to `"internal error"`, which is the safe default.
///
/// Public (rather than `pub(crate)`) so the issue #851 regression test
/// crate (`tests/handler_error_sanitization.rs`) can pin the
/// classifier's allowlist directly without going through the router.
#[must_use]
pub fn sanitize_bulk_row_error(raw: &str) -> &'static str {
    let lower = raw.to_ascii_lowercase();
    // Validation errors are template strings the caller's input can
    // synthesise on the client side; they don't carry DB/path/peer
    // state. Keep them informative.
    if lower.contains("cannot be empty")
        || lower.contains("exceeds max")
        || lower.contains("invalid characters")
        || lower.contains("invalid control characters")
        || lower.contains("must be")
        || lower.contains("required")
    {
        return "validation failed";
    }
    if lower.contains("already exists in namespace") || lower.contains("unique constraint") {
        return "conflict: already exists";
    }
    if lower.contains("not found") {
        return "not found";
    }
    if lower.contains("denied by governance") || lower.contains("permission") {
        return "forbidden";
    }
    if lower.contains("quorum") || lower.contains("fanout") || lower.contains("peer") {
        return "replication unavailable";
    }
    "internal error"
}

/// Standard 500 response used at sites where the prior code leaked the
/// raw error into the body. Logs the underlying `err` at `error` level
/// (with the optional `context` tag) and returns a constant JSON body.
///
/// Currently used by the regression test scaffolding and reserved for
/// future remediation patches that need to swap a bespoke 500 site to
/// the canonical sanitized path; production call sites already use the
/// inline log-then-respond pattern that predated this helper.
#[allow(dead_code)]
pub(crate) fn internal_error_response(
    context: &'static str,
    err: &dyn std::fmt::Display,
) -> axum::response::Response {
    tracing::error!("{context}: {err}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "internal server error"})),
    )
        .into_response()
}

/// Standard 400 response for an opaque caller-side failure (mirror of
/// [`internal_error_response`] for sites that previously echoed an
/// arbitrary `String` error from an MCP handler back to the wire). The
/// raw error is logged at `warn` level (the request is the caller's
/// fault, not the server's) and a constant safe message is returned.
pub(crate) fn bad_request_opaque(
    context: &'static str,
    err: &dyn std::fmt::Display,
) -> axum::response::Response {
    tracing::warn!("{context}: {err}");
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": "invalid request"})),
    )
        .into_response()
}

/// #869 (2026-05-18) — serialise a value to `serde_json::Value` and,
/// on failure, return a 500 envelope instead of the silent
/// `unwrap_or_default()` that would have masked the error as
/// `Value::Null` (or worse, an empty `{}` body paired with a 201
/// Created envelope).
///
/// Returns:
/// - `Ok(value)` — the serialised JSON value; the caller wraps it in
///   the success status code of its choice.
/// - `Err(response)` — a 500 response the caller MUST return verbatim;
///   the error has already been logged at `error` level with the
///   `context` tag so operators can diagnose the encode failure.
///
/// `Memory` and most response structs derive `Serialize` over owned
/// `String`/`Vec`/`HashMap` fields and only fail on the adversarial
/// inputs that produce non-string map keys, NaN/Inf floats, or
/// recursion past `serde_json`'s recursion limit. For typed structs
/// the failure is therefore vanishingly rare in production — but the
/// silent `unwrap_or_default` returning `201 Created {}` was a true
/// correctness bug (#869), so a typed 500 envelope is the right
/// surface for the rare failure case.
pub(crate) fn to_value_or_500<T: serde::Serialize + ?Sized>(
    context: &'static str,
    value: &T,
) -> Result<serde_json::Value, axum::response::Response> {
    serde_json::to_value(value).map_err(|e| {
        tracing::error!("{context}: serialise to JSON failed: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "internal server error: response serialisation failed"})),
        )
            .into_response()
    })
}
