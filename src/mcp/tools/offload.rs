// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-3 — MCP handlers for the context-offload substrate
//! primitive.
//!
//! Ships two tools' worth of plumbing:
//!   * `memory_offload(content, namespace?, ttl_seconds?)` — semantic
//!     tier+ surface for offloading verbatim content into the
//!     `offloaded_blobs` substrate. Returns the `ref_id` callers
//!     keep in their working window.
//!   * `memory_deref(ref_id)` — semantic tier+ surface for
//!     dereferencing a previously-offloaded `ref_id`. Refuses
//!     tampered rows.
//!
//! The handlers are registered for v0.7.0 as substrate-only — the
//! v0.8.0 short-term-context-compression patch wires them into
//! `tool_definitions_for_profile` once the surrounding profile-count
//! test fleet is rolled forward. Until then, callers can drive these
//! handlers directly from the daemon's MCP dispatcher (or from
//! integration tests via `pub use`).

use serde_json::{Value, json};

use crate::offload::{ContextOffloader, OffloadConfig};

/// Resolve the namespace for an offload call. Falls back to
/// `"auto"` so a tier-gated MCP caller that omits the field gets a
/// non-empty, audit-friendly bucket rather than a NULL violation.
fn resolve_namespace(params: &Value) -> String {
    params
        .get("namespace")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map_or_else(|| "auto".to_string(), str::to_string)
}

/// `memory_offload(content, namespace?, ttl_seconds?)`.
///
/// The handler is intentionally signer-free at v0.7.0 — the daemon
/// composes the agent's [`crate::identity::keypair::AgentKeypair`]
/// when v0.8.0 wires this through the MCP dispatcher. Substrate
/// plumbing only.
pub fn handle_offload(
    conn: &rusqlite::Connection,
    params: &Value,
    agent_id: &str,
) -> Result<Value, String> {
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .ok_or("content is required")?;
    let namespace = resolve_namespace(params);
    let ttl_seconds = params.get("ttl_seconds").and_then(Value::as_u64);

    let off = ContextOffloader::new(conn, None, OffloadConfig::default());
    let result = off
        .offload(content, &namespace, ttl_seconds, agent_id)
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "ref_id": result.ref_id,
        "content_sha256": result.content_sha256,
        "stored_at": result.stored_at,
        "namespace": namespace,
    }))
}

/// `memory_deref(ref_id)`.
///
/// SEC-4 (Cluster D, issue #767) — IDOR fix. The handler now requires
/// the caller's authenticated `agent_id` and forwards it to
/// [`ContextOffloader::deref`] which refuses with `NotFound` (leak-
/// resistant) when the caller is not the row's stored owner. Mirrors
/// the `handle_offload` signer-aware contract.
pub fn handle_deref(
    conn: &rusqlite::Connection,
    params: &Value,
    agent_id: &str,
) -> Result<Value, String> {
    let ref_id = params
        .get("ref_id")
        .and_then(Value::as_str)
        .ok_or("ref_id is required")?;

    let off = ContextOffloader::new(conn, None, OffloadConfig::default());
    let result = off
        .deref(ref_id, Some(agent_id))
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "ref_id": ref_id,
        "content": result.content,
        "stored_at": result.stored_at,
        "sha256": result.sha256,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage as db;
    use std::path::Path;

    fn fresh_conn() -> rusqlite::Connection {
        db::open(Path::new(":memory:")).expect("open in-memory db")
    }

    #[test]
    fn handle_offload_requires_content() {
        let conn = fresh_conn();
        let err = handle_offload(&conn, &json!({}), "ai:alice").unwrap_err();
        assert!(err.contains("content"));
    }

    #[test]
    fn handle_deref_requires_ref_id() {
        let conn = fresh_conn();
        let err = handle_deref(&conn, &json!({}), "ai:alice").unwrap_err();
        assert!(err.contains("ref_id"));
    }

    #[test]
    fn handle_offload_then_deref_round_trip() {
        let conn = fresh_conn();
        let off = handle_offload(
            &conn,
            &json!({"content": "hello mcp", "namespace": "mcp/test"}),
            "ai:alice",
        )
        .expect("offload");
        let ref_id = off["ref_id"].as_str().expect("ref_id").to_string();
        let back = handle_deref(&conn, &json!({"ref_id": ref_id}), "ai:alice").expect("deref");
        assert_eq!(back["content"].as_str(), Some("hello mcp"));
    }

    /// SEC-4 (Cluster D, issue #767) — MCP-level IDOR pin: bob cannot
    /// deref a blob alice offloaded; the error must look like a
    /// not-found rather than a permission error so probing cannot
    /// enumerate ref_ids by message differentiation.
    #[test]
    fn handle_deref_refuses_cross_agent_caller_mcp_layer() {
        let conn = fresh_conn();
        let off = handle_offload(
            &conn,
            &json!({"content": "alice secret", "namespace": "mcp/test"}),
            "ai:alice",
        )
        .expect("offload");
        let ref_id = off["ref_id"].as_str().expect("ref_id").to_string();
        let err = handle_deref(&conn, &json!({"ref_id": ref_id}), "ai:bob")
            .expect_err("cross-agent deref must reject");
        assert!(
            err.contains("not found"),
            "leak-resistant deref error must look like not-found, got: {err}"
        );
    }

    #[test]
    fn handle_offload_defaults_namespace_when_omitted() {
        let conn = fresh_conn();
        let resp = handle_offload(&conn, &json!({"content": "x"}), "ai:alice").expect("ok");
        assert_eq!(resp["namespace"].as_str(), Some("auto"));
    }

    #[test]
    fn handle_offload_passes_through_ttl() {
        let conn = fresh_conn();
        let resp = handle_offload(
            &conn,
            &json!({"content": "ttl-payload", "ttl_seconds": 3600}),
            "ai:alice",
        )
        .expect("ok");
        let ref_id = resp["ref_id"].as_str().unwrap();
        let ttl: Option<i64> = conn
            .query_row(
                "SELECT ttl_seconds FROM offloaded_blobs WHERE ref_id = ?1",
                rusqlite::params![ref_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ttl, Some(3600));
    }
}
