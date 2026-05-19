// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP-to-HTTP forwarding helpers for `memory_store`.
//!
//! #881 (PR-4 extraction): extracted from the monolithic
//! `src/mcp/tools/store.rs` (~3009 LOC) so the federation-forward path
//! lives in its own ~80-LOC module with focused error-message
//! contracts. Wire compatibility preserved verbatim.
//!
//! The two helpers in this module are the bridge that makes MCP-stdio
//! writes participate in the HTTP daemon's federation fanout
//! (`broadcast_store_quorum`, `broadcast_link_quorum`,
//! `broadcast_delete_quorum`). Closes the MCP-stdio-vs-federation gap
//! surfaced by a2a-gate v0.6.0 r6 (#318).
//!
//! `forward_to_http` is the generic HTTP request helper — used by the
//! store handler today, but reusable for any future MCP→HTTP bridge
//! that needs the same timeout + structured-error envelope.
//!
//! `forward_store_to_http` is the store-specific wrapper that
//! translates MCP params into the HTTP daemon's `CreateMemoryRequest`
//! shape and surfaces the response in the MCP `memory_store` envelope
//! callers expect.

use serde_json::Value;

/// Forward an MCP write call to a local HTTP daemon so the daemon's
/// federation fanout coordinator (`broadcast_store_quorum` /
/// `broadcast_link_quorum` / `broadcast_delete_quorum`) takes over
/// replication. Closes the MCP-stdio-vs-federation gap surfaced by
/// a2a-gate v0.6.0 r6 (#318).
///
/// # Errors
///
/// Returns the daemon's JSON body on 2xx, or a structured error string
/// that the MCP layer surfaces as a JSON-RPC `result.error`. On 5xx /
/// transport failure the caller gets a clear message naming the
/// forward URL so operators can distinguish "fanout daemon down"
/// from "quorum not met".
pub(super) fn forward_to_http(
    method: reqwest::Method,
    url: &str,
    body: Option<&Value>,
    extra_headers: &[(&str, String)],
) -> Result<Value, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("federation_forward: build client: {e}"))?;
    let mut req = client.request(method, url);
    for (k, v) in extra_headers {
        req = req.header(*k, v);
    }
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = req
        .send()
        .map_err(|e| format!("federation_forward: POST {url}: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .map_err(|e| format!("federation_forward: read body from {url}: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "federation_forward: {url} returned {status}: {text}"
        ));
    }
    serde_json::from_str::<Value>(&text)
        .map_err(|e| format!("federation_forward: parse body from {url}: {e} (raw: {text})"))
}

/// MCP `memory_store` → HTTP `POST {forward_url}/api/v1/memories`.
/// Translates the MCP params (which mirror the HTTP request body field
/// names verbatim, with the exception of how `metadata.agent_id` is
/// surfaced) into the HTTP daemon's `CreateMemoryRequest` shape, then
/// reshapes the 201 response into the MCP `memory_store` envelope
/// callers expect (`{id, tier, title, namespace, agent_id, ...}`).
///
/// # Errors
///
/// Forwards transport / encode failures from [`forward_to_http`].
pub(super) fn forward_store_to_http(
    forward_url: &str,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let url = format!("{}/api/v1/memories", forward_url.trim_end_matches('/'));

    // Resolve agent_id with the same precedence chain the local path
    // uses, then surface it as an X-Agent-Id header (the HTTP handler's
    // canonical resolution channel for daemon-mode multi-tenancy).
    let explicit_agent_id = params["agent_id"]
        .as_str()
        .or_else(|| params["metadata"]["agent_id"].as_str());
    let agent_id = crate::identity::resolve_agent_id(explicit_agent_id, mcp_client)
        .map_err(|e| e.to_string())?;

    // The HTTP request body mirrors the MCP params; pass them through
    // and let the HTTP handler do all validation, governance, quota,
    // dedup, embedding, audit, and federation broadcast.
    let body = params.clone();
    let headers: &[(&str, String)] = &[("X-Agent-Id", agent_id)];

    forward_to_http(reqwest::Method::POST, &url, Some(&body), headers)
}
