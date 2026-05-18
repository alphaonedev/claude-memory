// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! System-level HTTP handlers — capabilities, health, metrics.
//!
//! Extracted from `src/handlers/mod.rs` as part of the issue #650
//! file-architecture cleanup.

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde_json::json;

use super::transport::AppState;

// --- /api/v1/capabilities (GET) -------------------------------------------

pub async fn get_capabilities(
    State(app): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Mirrors `mcp::handle_capabilities_with_conn`. Reranker state isn't
    // tracked on the HTTP AppState (HTTP daemons that wire a cross-encoder
    // record it via the tier config's `cross_encoder` flag, which is
    // enough for scenario S30's equivalence check).
    //
    // v0.6.2 (S18): forward the *runtime* embedder state so
    // `features.embedder_loaded` reports whether the HF model actually
    // materialized at serve startup (not just whether the tier config
    // asked for one). An offline CI runner can fail the model fetch and
    // end up with `semantic_search=true` (from config) but no embedder in
    // the AppState — setup scripts need this signal to refuse to start
    // scenarios that depend on semantic recall.
    //
    // v0.6.3 (capabilities schema v2): hold the DB lock briefly so the
    // dynamic blocks (active_rules, registered_count, pending_requests)
    // can be filled from live counts. Each query is a single COUNT(*) so
    // the lock window stays sub-millisecond.
    //
    // v0.6.3.1 (P1 honesty patch): honour the `Accept-Capabilities`
    // header. `v1` returns the legacy pre-v0.6.3.1 shape; anything else
    // (including absent) returns v2.
    let accept = headers
        .get("accept-capabilities")
        .and_then(|v| v.to_str().ok())
        .map_or(crate::mcp::CapabilitiesAccept::V3, |raw| {
            crate::mcp::CapabilitiesAccept::parse(raw)
        });
    // v0.7.0 A5 — HTTP path now serves v3 by default (A5 flips the
    // default + threads `Profile` + `McpConfig` through `AppState`).
    // Old clients that pinned `Accept-Capabilities: v2` keep getting
    // the v2 shape unchanged; everyone else gets v3 (additive over
    // v2, so reading-by-name stays compatible).
    //
    // v0.7.0 A4 — `agent_permitted_families` requires an `agent_id`.
    // HTTP doesn't yet thread one (it would come from a future
    // session-bound auth header); for now pass None and the field is
    // omitted from the wire per the A4 contract.
    let embedder_loaded = app.embedder.as_ref().is_some();
    let lock = app.db.lock().await;
    let conn = &lock.0;
    let result = match accept {
        crate::mcp::CapabilitiesAccept::V3 => crate::mcp::handle_capabilities_with_conn_v3(
            app.tier_config.as_ref(),
            None,
            embedder_loaded,
            Some(conn),
            app.profile.as_ref(),
            app.mcp_config.as_ref().as_ref(),
            None,
            // v0.7.0 B4 — HTTP path has no MCP `initialize` handshake,
            // so harness is always None here. The
            // `your_harness_supports_deferred_registration` field is
            // omitted on the wire via `skip_serializing_if`.
            None,
        ),
        _ => crate::mcp::handle_capabilities_with_conn(
            app.tier_config.as_ref(),
            None,
            embedder_loaded,
            Some(conn),
            accept,
        ),
    };
    drop(lock);
    // v0.7.0.1 S75 — capture the live DB schema-migration version
    // BEFORE we land in the response-shaping match so a SAL error
    // surfaces as a logged warning + a `0` fallback rather than a
    // 500 over the whole capabilities endpoint. Operators reading
    // this field consult it as a live progress indicator versus the
    // binary's expected `CURRENT_SCHEMA_VERSION` (28 at v0.7.0); a
    // mismatch is meaningful, but a transient SAL hiccup must not
    // hide every other capability bit.
    #[cfg(feature = "sal")]
    let db_schema_version: i64 = match app.store.schema_version().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target = "capabilities",
                error = %e,
                "schema_version lookup via SAL failed; reporting 0"
            );
            0
        }
    };
    #[cfg(not(feature = "sal"))]
    let db_schema_version: i64 = 0;

    match result {
        Ok(mut v) => {
            // v0.7.0 Wave-3 — surface the resolved storage backend so
            // operators can confirm which adapter their daemon is
            // running against without reading the launch log. Always
            // emitted (sqlite | postgres) so polling clients can rely
            // on the field shape.
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "storage_backend".to_string(),
                    serde_json::Value::String(app.storage_backend.as_str().to_string()),
                );
                // v0.7.0.1 S75 — surface the live DB schema-migration
                // version (`MAX(version)` from the `schema_version`
                // table) so operators can confirm their deployed
                // daemon's database is on the schema the binary
                // expects. Distinct from the wire-format
                // `schema_version` discriminator (which is the
                // capabilities-document version, currently `"3"`); the
                // new `db_schema_version` is the integer migration
                // ladder of the underlying store. Always emitted so
                // polling clients can branch on it without parsing
                // magic strings.
                obj.insert(
                    "db_schema_version".to_string(),
                    serde_json::Value::Number(serde_json::Number::from(db_schema_version)),
                );
            }
            (StatusCode::OK, Json(v)).into_response()
        }
        Err(e) => {
            tracing::error!("capabilities: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}
