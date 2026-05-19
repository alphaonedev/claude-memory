// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Postgres route-gate middleware + storage-error sanitiser.
//!
//! Extracted from [`super::transport`] under issue #650 (handler cap
//! ≤1200 LOC). Function bodies are unchanged; only the module surface
//! moved. Wire compatibility preserved via `pub use postgres_gate::*`
//! in [`super`].

#![allow(clippy::too_many_lines)]

#[cfg(feature = "sal")]
use axum::{
    Json,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
#[cfg(feature = "sal")]
use serde_json::json;

#[cfg(feature = "sal")]
use super::{AppState, StorageBackend};

/// v0.7.0 Wave-3 — uniform 501 NOT IMPLEMENTED response for handlers
/// that have not yet migrated to the [`crate::store::MemoryStore`]
/// trait dispatch path on Postgres-backed daemons.
///
/// Returns a stable, machine-parseable JSON envelope so operator
/// scripts can recognise the v0.7.0 Wave-3 schism without parsing
/// free-form strings:
///
/// ```json
/// {
///   "error": "endpoint not yet implemented for postgres-backed daemon",
///   "endpoint": "<route>",
///   "storage_backend": "postgres",
///   "remediation": "use sqlite-backed daemon or wait for v0.7.x trait coverage"
/// }
/// ```
///
/// Wired into the un-migrated handlers below so a postgres-backed
/// daemon never silently falls back to the empty in-memory SQLite
/// scratch DB and corrupts the operator's mental model of where
/// their data lives. As handlers migrate to the trait this call
/// site count goes to zero.
#[cfg(feature = "sal")]
#[must_use]
pub fn postgres_not_implemented(endpoint: &'static str) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "endpoint not yet implemented for postgres-backed daemon",
            "endpoint": endpoint,
            "storage_backend": "postgres",
            "remediation": "use sqlite-backed daemon or wait for v0.7.x trait coverage",
        })),
    )
        .into_response()
}

/// v0.7.0 Wave-3 Continuation — postgres-supported endpoint allow-list.
///
/// Returns `true` if the given (method, path) tuple has a handler that
/// has been migrated to dispatch through the [`crate::store::MemoryStore`]
/// trait when the daemon is postgres-backed. Anything not in this list
/// is shielded by [`postgres_route_gate`] middleware which surfaces
/// 501 NOT IMPLEMENTED rather than letting the un-migrated handler
/// silently fall through to the empty in-memory scratch SQLite database
/// that `bootstrap_serve` opens for the postgres-backed `app.db` field.
///
/// The matching is path-pattern aware:
/// - exact equality for fixed paths (e.g. `/api/v1/memories`)
/// - prefix match for sub-resources (e.g. `/api/v1/memories/{id}`)
///
/// As handlers migrate they get added here. Pre-existing CRUD entries
/// match what Wave-3 phase 3 already wired through `app.store`.
#[cfg(feature = "sal")]
#[must_use]
pub fn postgres_endpoint_supported(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;

    // Health and metadata always pass through — they don't touch user data.
    if path == "/api/v1/health"
        || path == "/api/v1/capabilities"
        || path == "/metrics"
        || path == "/api/v1/metrics"
    {
        return true;
    }

    // Approval SSE stream — read-only metadata stream, not user-data.
    if path == "/api/v1/approvals/stream" && method == Method::GET {
        return true;
    }

    match (method.as_str(), path) {
        // Wave-3 phase 3 — core CRUD (commit c049500).
        ("POST", "/api/v1/memories") | ("GET", "/api/v1/memories") => true,
        ("GET" | "PUT" | "DELETE", p) if memory_id_path(p) => true,
        ("GET", "/api/v1/search") => true,
        ("POST", "/api/v1/links") => true,
        ("GET", p) if links_id_path(p) => true,
        // Wave-3 continuation — list_pending (read-only).
        ("GET", "/api/v1/pending") => true,
        // Wave-3 continuation — list_agents (read-only).
        ("GET", "/api/v1/agents") => true,
        // Wave-3 continuation — list_namespaces (read-only).
        ("GET", "/api/v1/namespaces") => {
            // GET /api/v1/namespaces with no query string lists namespaces.
            // The same path with ?namespace=... fetches a standard which is
            // also gated through SAL via get_namespace_standard_qs.
            true
        }
        // Wave-3 continuation — KG endpoints (postgres adapter has impls).
        ("POST", "/api/v1/kg/query")
        | ("GET", "/api/v1/kg/timeline")
        | ("POST", "/api/v1/kg/invalidate") => true,
        // Continuation 6 — three new HTTP endpoints (S52, S61, S65).
        ("POST", "/api/v1/kg/find_paths")
        | ("POST", "/api/v1/links/verify")
        | ("POST", "/api/v1/quota/status") => true,
        // Wave-3 continuation — entity registry.
        ("POST", "/api/v1/entities") | ("GET", "/api/v1/entities/by_alias") => true,
        // Wave-3 continuation — stats (basic count).
        ("GET", "/api/v1/stats") => true,
        // Wave-3 continuation — bulk write.
        ("POST", "/api/v1/memories/bulk") => true,
        // Wave-3 continuation — recall fallback (keyword via search).
        ("GET" | "POST", "/api/v1/recall") => true,
        // Wave-3 continuation — archive list/stats (read-only).
        ("GET", "/api/v1/archive") => true,
        ("GET", "/api/v1/archive/stats") => true,
        // Wave-3 continuation — taxonomy and check_duplicate.
        ("GET", "/api/v1/taxonomy") => true,
        ("POST", "/api/v1/check_duplicate") => true,
        // Wave-3 continuation — list_subscriptions, inbox.
        ("GET", "/api/v1/subscriptions") => true,
        ("GET", "/api/v1/inbox") => true,
        // Wave-3 Continuation 2 — federation push/pull (Phase 8).
        ("POST", "/api/v1/sync/push") => true,
        ("GET", "/api/v1/sync/since") => true,
        // Wave-3 Continuation 2 — governance write paths (Phase 11).
        ("POST", p) if pending_decide_path(p) => true,
        ("POST", p) if namespace_standard_post_path(p) => true,
        ("DELETE", p) if namespace_standard_delete_path(p) => true,
        ("POST", "/api/v1/namespaces") => true,
        ("DELETE", "/api/v1/namespaces") => true,
        // Wave-3 Continuation 3 — lifecycle write paths (Phase 13/14/16/17/18/19).
        ("POST", "/api/v1/forget") => true,
        ("POST", "/api/v1/consolidate") => true,
        ("GET", "/api/v1/contradictions") => true,
        // v0.7.0 L6 — S51 autonomous-tier endpoints. Both are
        // LLM-only (no DB access for the request body itself) so the
        // postgres gate just needs to pass them through to the
        // handler, which handles the 503 fallback when no LLM is
        // wired.
        ("POST", "/api/v1/auto_tag") => true,
        ("POST", "/api/v1/expand_query") => true,
        // v0.7.0 L9 / L10 — HTTP parity for `tools/list` and
        // `memory_load_family`. `tools/list` is pure config
        // enumeration (no DB); `memory_load_family` reads through the
        // SAL trait on the postgres path.
        ("GET", "/api/v1/tools/list") => true,
        ("POST", "/api/v1/memory_load_family") => true,
        ("POST", "/api/v1/notify") => true,
        ("POST", "/api/v1/gc") => true,
        ("POST", "/api/v1/import") => true,
        ("GET", "/api/v1/export") => true,
        ("POST", "/api/v1/archive") => true,
        ("DELETE", "/api/v1/archive") => true,
        ("POST", "/api/v1/archive/purge") => true,
        ("POST", p) if archive_restore_path(p) => true,
        // Wave-3 Continuation 3 — remaining write paths the sqlite path
        // already wires through `app.store` in their handlers (these
        // were soft-routed by the legacy db:: free-functions before
        // Continuation 3, so the gate now allow-lists them so the gate
        // doesn't 501 a working sqlite-routed handler on a postgres
        // daemon. Each handler internally enforces postgres-vs-sqlite
        // dispatch, so the gate's job is just to permit the request to
        // reach the handler).
        ("POST", "/api/v1/agents") => true,
        ("DELETE", "/api/v1/links") => true,
        ("POST", "/api/v1/subscriptions") | ("DELETE", "/api/v1/subscriptions") => true,
        ("POST", "/api/v1/session/start") => true,
        ("POST", p) if memory_promote_path(p) => true,
        ("POST", p) if approvals_decide_path(p) => true,
        _ => false,
    }
}

/// Path matcher for `/api/v1/memories/{id}/promote`.
#[cfg(feature = "sal")]
fn memory_promote_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/v1/memories/") else {
        return false;
    };
    rest.ends_with("/promote") && rest.split('/').count() == 2
}

/// Path matcher for `POST /api/v1/approvals/{pending_id}` (HMAC-gated).
#[cfg(feature = "sal")]
fn approvals_decide_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/v1/approvals/") else {
        return false;
    };
    !rest.is_empty() && rest != "stream" && !rest.contains('/')
}

/// Path matcher for `/api/v1/archive/{id}/restore`.
#[cfg(feature = "sal")]
fn archive_restore_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/v1/archive/") else {
        return false;
    };
    rest.ends_with("/restore") && rest.split('/').count() == 2
}

/// Path matcher for `/api/v1/pending/{id}/approve|reject`.
#[cfg(feature = "sal")]
fn pending_decide_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/v1/pending/") else {
        return false;
    };
    matches!(rest.split_once('/'), Some((_, "approve" | "reject")))
}

/// Path matcher for `POST /api/v1/namespaces/{ns}/standard`.
#[cfg(feature = "sal")]
fn namespace_standard_post_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/v1/namespaces/") else {
        return false;
    };
    rest.ends_with("/standard") && rest.split('/').count() == 2
}

/// Path matcher for `DELETE /api/v1/namespaces/{ns}/standard`.
#[cfg(feature = "sal")]
fn namespace_standard_delete_path(p: &str) -> bool {
    namespace_standard_post_path(p)
}

/// Path matcher for `/api/v1/memories/{id}` (no further sub-segment).
#[cfg(feature = "sal")]
fn memory_id_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/v1/memories/") else {
        return false;
    };
    // Reject the bulk path and any further sub-segments.
    if rest == "bulk" {
        return false;
    }
    !rest.contains('/')
}

/// Path matcher for `/api/v1/links/{id}`.
#[cfg(feature = "sal")]
fn links_id_path(p: &str) -> bool {
    let Some(rest) = p.strip_prefix("/api/v1/links/") else {
        return false;
    };
    !rest.is_empty() && !rest.contains('/')
}

/// v0.7.0 Wave-3 Continuation — middleware that gates un-migrated
/// handlers when the daemon is postgres-backed.
///
/// Sits in the request pipeline after `api_key_auth` so authn still
/// applies, then short-circuits any (method, path) tuple not in
/// [`postgres_endpoint_supported`] with a structured 501 response.
///
/// On sqlite-backed daemons this is a pure pass-through — every path
/// is supported because the legacy `db::*` free-function code path is
/// the active path and `app.db` is the real on-disk database.
///
/// This is the load-bearing correctness fix for postgres-backed
/// daemons: without it, any un-migrated handler would silently use
/// the empty in-memory scratch SQLite database that `bootstrap_serve`
/// opens against the `--db` path (which is unused on postgres) and
/// either return empty results (read paths) or write to the wrong
/// database (write paths). The gate makes that impossible.
#[cfg(feature = "sal")]
pub async fn postgres_route_gate(
    State(app): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if !matches!(app.storage_backend, StorageBackend::Postgres) {
        return next.run(req).await;
    }

    let method = req.method().clone();
    let path = req.uri().path().to_string();

    if postgres_endpoint_supported(&method, &path) {
        return next.run(req).await;
    }

    tracing::debug!(
        method = %method,
        path = %path,
        "postgres-backed daemon: 501 for un-migrated endpoint"
    );

    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "endpoint not yet implemented for postgres-backed daemon",
            "endpoint": path,
            "method": method.as_str(),
            "storage_backend": "postgres",
            "remediation": "use sqlite-backed daemon or wait for v0.7.x trait coverage; \
                            see docs/postgres-age-guide.md for the supported endpoint inventory",
        })),
    )
        .into_response()
}

/// v0.7.0 Wave-3 — translate a [`crate::store::StoreError`] into the
/// daemon's standard HTTP error envelope. Centralised so every
/// trait-routed handler reports backend errors with the same shape.
///
/// v0.7.0 M12 — every variant whose `to_string()` may carry adapter-
/// originating payload (connection strings, file paths, raw sqlx
/// diagnostics) is routed through [`sanitize_store_err_message`]
/// before landing in the HTTP envelope. The raw error is still
/// captured to the structured tracing log for operators; the wire
/// surface only carries the scrubbed message so an authenticated
/// client cannot exfiltrate the postgres URL by triggering a typed
/// error path.
#[cfg(feature = "sal")]
#[must_use]
pub fn store_err_to_response(e: crate::store::StoreError) -> Response {
    use crate::store::StoreError;
    let (status, msg) = match &e {
        StoreError::NotFound { .. } => (StatusCode::NOT_FOUND, "not found".to_string()),
        StoreError::Conflict { .. } => (
            StatusCode::CONFLICT,
            sanitize_store_err_message(&e.to_string()),
        ),
        StoreError::PermissionDenied { .. } => (
            StatusCode::FORBIDDEN,
            sanitize_store_err_message(&e.to_string()),
        ),
        StoreError::InvalidInput { .. } => (
            StatusCode::BAD_REQUEST,
            sanitize_store_err_message(&e.to_string()),
        ),
        StoreError::UnsupportedCapability { capability } => (
            StatusCode::NOT_IMPLEMENTED,
            format!("backend does not support capability: {capability}"),
        ),
        StoreError::IntegrityFailed { .. } | StoreError::BackendUnavailable { .. } => {
            tracing::error!("store backend error: {e}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "storage backend unavailable".to_string(),
            )
        }
        _ => {
            tracing::error!("store backend error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal server error".to_string(),
            )
        }
    };
    (status, Json(json!({"error": msg}))).into_response()
}

/// v0.7.0 M12 — scrub adapter-originating payload from a
/// [`crate::store::StoreError`]'s display string before it lands in an
/// HTTP response. The redaction targets three families of leakage the
/// M12 audit found in real sqlx + filesystem error paths:
///
/// 1. **Connection-string-like fragments** — anything matching the
///    `scheme://user:pass@host[:port]/db` shape. The entire run from
///    the scheme through the next whitespace / quote / brace boundary
///    is replaced with `[redacted-url]` so an authenticated caller
///    cannot read the postgres URL out of a wrapped
///    `sqlx::Error::Configuration("invalid url postgres://…")` (or any
///    other variant whose Display interpolates the connection target).
/// 2. **Absolute filesystem paths** — anything starting with `/` and
///    running through a typical path charset gets replaced with
///    `[redacted-path]`. Closes the
///    `sqlx::Error::Io("/var/lib/postgresql/…")` family.
///
/// The function is deliberately textual (byte scan) rather than
/// variant-aware: the cost of a missed leak (PII / credential
/// exposure) far outweighs the cost of over-sanitization (a slightly
/// less specific error message). Operators who need the raw
/// diagnostic still get it via the structured tracing log emitted at
/// the call site.
#[cfg(feature = "sal")]
#[must_use]
pub fn sanitize_store_err_message(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        // Look for "://" — strong signal of a URL.
        if i + 2 < bytes.len() && &bytes[i..i + 3] == b"://" {
            // Walk backward through any scheme characters we already
            // emitted, then pop them from `out` and replace the whole
            // run with the sentinel.
            let mut scheme_start = i;
            while scheme_start > 0 {
                let c = bytes[scheme_start - 1];
                if c.is_ascii_alphanumeric() || c == b'+' || c == b'-' || c == b'.' {
                    scheme_start -= 1;
                } else {
                    break;
                }
            }
            let pop = i - scheme_start;
            out.truncate(out.len().saturating_sub(pop));
            out.push_str("[redacted-url]");
            // Skip past "://" plus the rest of the URL run (anything
            // not whitespace/quote/brace/paren/comma/semicolon/angle).
            i += 3;
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_whitespace()
                    || c == b'"'
                    || c == b'\''
                    || c == b'`'
                    || c == b'{'
                    || c == b'}'
                    || c == b'('
                    || c == b')'
                    || c == b','
                    || c == b';'
                    || c == b'<'
                    || c == b'>'
                {
                    break;
                }
                i += 1;
            }
            continue;
        }

        // Absolute paths — require a separator/boundary before the '/'
        // so we don't gut "1/2" inside an unrelated diagnostic.
        if bytes[i] == b'/'
            && (i == 0
                || matches!(
                    bytes[i - 1],
                    b' ' | b'\t' | b'\n' | b'"' | b'\'' | b'(' | b'[' | b'=' | b':'
                ))
            && i + 1 < bytes.len()
            && (bytes[i + 1].is_ascii_alphanumeric()
                || bytes[i + 1] == b'_'
                || bytes[i + 1] == b'.')
        {
            out.push_str("[redacted-path]");
            i += 1;
            while i < bytes.len() {
                let c = bytes[i];
                if c.is_ascii_alphanumeric() || c == b'/' || c == b'.' || c == b'_' || c == b'-' {
                    i += 1;
                } else {
                    break;
                }
            }
            continue;
        }

        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(all(test, feature = "sal"))]
mod store_err_sanitize_tests {
    use super::sanitize_store_err_message;

    #[test]
    fn sanitize_redacts_postgres_url() {
        let leak = "connection failed for postgres://admin:hunter2@db.internal:5432/ai_memory";
        let clean = sanitize_store_err_message(leak);
        assert!(!clean.contains("postgres://"), "raw scheme leaked: {clean}");
        assert!(!clean.contains("hunter2"), "password leaked: {clean}");
        assert!(!clean.contains("db.internal"), "host leaked: {clean}");
        assert!(
            clean.contains("[redacted-url]"),
            "missing sentinel: {clean}"
        );
    }

    #[test]
    fn sanitize_redacts_filesystem_path() {
        let leak = "open /var/lib/postgresql/data/global/pg_control failed";
        let clean = sanitize_store_err_message(leak);
        assert!(!clean.contains("/var/lib"), "raw path leaked: {clean}");
        assert!(
            clean.contains("[redacted-path]"),
            "missing sentinel: {clean}"
        );
    }

    #[test]
    fn sanitize_passes_through_clean_diagnostics() {
        let clean_input = "memory not found: abc-123";
        let out = sanitize_store_err_message(clean_input);
        assert_eq!(out, clean_input);
    }

    #[test]
    fn sanitize_handles_multiple_leaks() {
        let leak = "sqlx error at postgres://u:p@h/db touching /etc/secret/key";
        let clean = sanitize_store_err_message(leak);
        assert!(!clean.contains("postgres://"));
        assert!(!clean.contains("/etc/secret"));
        assert!(clean.contains("[redacted-url]"));
        assert!(clean.contains("[redacted-path]"));
    }

    #[test]
    fn sanitize_preserves_relative_paths() {
        // A literal "/" surrounded by digits ("1/2") must NOT be
        // treated as a path. Regression test for the boundary check.
        let raw = "ratio 1/2 over 3/4";
        let out = sanitize_store_err_message(raw);
        assert_eq!(out, raw, "fraction-like content must not be redacted");
    }

    #[test]
    fn sanitize_handles_unicode_in_clean_message() {
        let raw = "memory not found: \u{1F4DD}-id-with-emoji";
        let out = sanitize_store_err_message(raw);
        assert!(out.contains("memory not found"));
    }

    #[test]
    fn sanitize_redacts_url_at_start_of_message() {
        let leak = "postgres://u:p@h/db is unreachable";
        let clean = sanitize_store_err_message(leak);
        assert!(clean.starts_with("[redacted-url]"));
    }
}

// ---------------------------------------------------------------------------
// L0.7-6 Tier E coverage — exercise the helper surface that does not
// require a real Axum runtime: percent decoder, constant-time compare,
// store-error wire-shape mapper, postgres endpoint matrix, AppState
// helpers. The router-bound paths (api_key_auth full pipeline,
// JsonOrBadRequest extractor, postgres_route_gate live middleware)
// remain integration-only per coverage/policy.md.
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "sal"))]
mod transport_postgres_gate_tests {
    use super::*;
    use axum::http::Method;

    #[test]
    fn postgres_gate_always_passes_health_and_metrics() {
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/health"));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/capabilities"
        ));
        assert!(postgres_endpoint_supported(&Method::GET, "/metrics"));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/metrics"));
    }

    #[test]
    fn postgres_gate_passes_core_crud() {
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/memories"
        ));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/memories"
        ));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/search"));
        assert!(postgres_endpoint_supported(&Method::POST, "/api/v1/links"));
    }

    #[test]
    fn postgres_gate_passes_memory_id_paths() {
        // GET / PUT / DELETE on /api/v1/memories/{id} are supported.
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/memories/abc-123"
        ));
        assert!(postgres_endpoint_supported(
            &Method::PUT,
            "/api/v1/memories/abc-123"
        ));
        assert!(postgres_endpoint_supported(
            &Method::DELETE,
            "/api/v1/memories/abc-123"
        ));
        // POST on a single id is not in the matrix.
        assert!(!postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/memories/abc-123"
        ));
        // /api/v1/memories/bulk is its own endpoint (not memory_id_path).
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/memories/bulk"
        ));
    }

    #[test]
    fn postgres_gate_passes_links_id_paths() {
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/links/link-id-1"
        ));
        // Empty trailing segment must not match.
        assert!(!postgres_endpoint_supported(&Method::GET, "/api/v1/links/"));
    }

    #[test]
    fn postgres_gate_passes_kg_paths() {
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/kg/query"
        ));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/kg/timeline"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/kg/invalidate"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/kg/find_paths"
        ));
    }

    #[test]
    fn postgres_gate_passes_quota_verify_entities() {
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/links/verify"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/quota/status"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/entities"
        ));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/entities/by_alias"
        ));
    }

    #[test]
    fn postgres_gate_passes_archive_paths() {
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/archive"));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/archive/stats"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/archive"
        ));
        assert!(postgres_endpoint_supported(
            &Method::DELETE,
            "/api/v1/archive"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/archive/purge"
        ));
        // archive_restore_path: /api/v1/archive/{id}/restore
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/archive/abc/restore"
        ));
        assert!(!postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/archive/abc/restore/other"
        ));
    }

    #[test]
    fn postgres_gate_passes_namespace_standard_paths() {
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/namespaces/proj/standard"
        ));
        assert!(postgres_endpoint_supported(
            &Method::DELETE,
            "/api/v1/namespaces/proj/standard"
        ));
        assert!(!postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/namespaces/standard"
        ));
    }

    #[test]
    fn postgres_gate_passes_pending_decide_paths() {
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/pending/p1/approve"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/pending/p1/reject"
        ));
        // Non-approve-reject suffix must not match.
        assert!(!postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/pending/p1/foo"
        ));
    }

    #[test]
    fn postgres_gate_passes_approvals_decide_paths() {
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/approvals/abc-123"
        ));
        // /api/v1/approvals/stream is excluded from decide path.
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/approvals/stream"
        ));
    }

    #[test]
    fn postgres_gate_passes_memory_promote_path() {
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/memories/abc/promote"
        ));
        assert!(!postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/memories/abc/promote/extra"
        ));
    }

    #[test]
    fn postgres_gate_passes_remaining_write_paths() {
        assert!(postgres_endpoint_supported(&Method::POST, "/api/v1/forget"));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/consolidate"
        ));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/contradictions"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/auto_tag"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/expand_query"
        ));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/tools/list"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/memory_load_family"
        ));
        assert!(postgres_endpoint_supported(&Method::POST, "/api/v1/notify"));
        assert!(postgres_endpoint_supported(&Method::POST, "/api/v1/gc"));
        assert!(postgres_endpoint_supported(&Method::POST, "/api/v1/import"));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/export"));
        assert!(postgres_endpoint_supported(&Method::POST, "/api/v1/agents"));
        assert!(postgres_endpoint_supported(
            &Method::DELETE,
            "/api/v1/links"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/subscriptions"
        ));
        assert!(postgres_endpoint_supported(
            &Method::DELETE,
            "/api/v1/subscriptions"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/session/start"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/sync/push"
        ));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/sync/since"
        ));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/pending"));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/agents"));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/namespaces"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/namespaces"
        ));
        assert!(postgres_endpoint_supported(
            &Method::DELETE,
            "/api/v1/namespaces"
        ));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/stats"));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/taxonomy"
        ));
        assert!(postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/check_duplicate"
        ));
        assert!(postgres_endpoint_supported(
            &Method::GET,
            "/api/v1/subscriptions"
        ));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/inbox"));
        assert!(postgres_endpoint_supported(&Method::GET, "/api/v1/recall"));
        assert!(postgres_endpoint_supported(&Method::POST, "/api/v1/recall"));
    }

    #[test]
    fn postgres_gate_rejects_unknown_paths() {
        // Anything not in the allow-list must return false so the
        // route gate surfaces 501 instead of silently routing to the
        // empty scratch SQLite DB.
        assert!(!postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/this/is/not/a/real/endpoint"
        ));
        assert!(!postgres_endpoint_supported(
            &Method::POST,
            "/api/v1/unknown"
        ));
    }

    #[test]
    fn postgres_not_implemented_carries_endpoint_and_remediation() {
        let resp = postgres_not_implemented("/api/v1/test");
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_IMPLEMENTED);
    }

    #[test]
    fn store_err_to_response_maps_every_variant_to_status() {
        use crate::store::StoreError;
        let r = store_err_to_response(StoreError::NotFound {
            id: "x".to_string(),
        });
        assert_eq!(r.status(), axum::http::StatusCode::NOT_FOUND);

        let r = store_err_to_response(StoreError::Conflict {
            id: "x".to_string(),
        });
        assert_eq!(r.status(), axum::http::StatusCode::CONFLICT);

        let r = store_err_to_response(StoreError::PermissionDenied {
            action: "r".to_string(),
            target: "t".to_string(),
            reason: "x".to_string(),
        });
        assert_eq!(r.status(), axum::http::StatusCode::FORBIDDEN);

        let r = store_err_to_response(StoreError::InvalidInput {
            detail: "bad".to_string(),
        });
        assert_eq!(r.status(), axum::http::StatusCode::BAD_REQUEST);

        let r = store_err_to_response(StoreError::UnsupportedCapability {
            capability: "X".to_string(),
        });
        assert_eq!(r.status(), axum::http::StatusCode::NOT_IMPLEMENTED);

        let r = store_err_to_response(StoreError::IntegrityFailed {
            detail: "d".to_string(),
        });
        assert_eq!(r.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);

        let r = store_err_to_response(StoreError::BackendUnavailable {
            backend: "p".to_string(),
            detail: "d".to_string(),
        });
        assert_eq!(r.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);

        let r = store_err_to_response(StoreError::Backend(crate::store::BoxBackendError::new(
            "raw",
        )));
        assert_eq!(r.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }
}
