// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Non-Human Identity (NHI) resolution for `agent_id`.
//!
//! Every stored memory carries `metadata.agent_id` — a best-effort identifier
//! for the agent (AI, human, or system) that wrote it. This module encapsulates
//! the precedence chain and default-id synthesis for all three entry points
//! (CLI, MCP, HTTP) so that the identity format is uniform.
//!
//! # Precedence (CLI / MCP)
//!
//! 1. Explicit id passed by the caller (`--agent-id`, MCP tool param)
//! 2. `AI_MEMORY_AGENT_ID` environment variable
//! 3. (MCP only) `initialize.clientInfo.name` captured at handshake time
//!    → `ai:<client>@<hostname>:pid-<pid>`
//! 4. `host:<hostname>:pid-<pid>-<uuid8>` — stable per-process
//! 5. `anonymous:pid-<pid>-<uuid8>` — fallback if hostname is unavailable
//!
//! # Precedence (HTTP)
//!
//! HTTP `serve` is multi-tenant; no process-level default is ever cached.
//!
//! 1. Request body `agent_id` field
//! 2. `X-Agent-Id` request header
//! 3. Per-request `anonymous:req-<uuid8>` (emits a `WARN` log line)
//!
//! # Trust
//!
//! `agent_id` is a *claimed* identity, not an *attested* one. Do not use it
//! for security decisions without pairing it with agent registration (Task
//! 1.3) and, eventually, signed attestations.

use std::sync::OnceLock;

use anyhow::Result;

use crate::validate;

// v0.7 Track H — Ed25519 attested identity. The keypair lifecycle
// (generate / save / load / list / export-pub) lives in its own
// submodule so this file stays focused on `agent_id` resolution. H2+
// will plumb the loaded `AgentKeypair` through `AppState` for outbound
// link signing.
pub mod keypair;
// H2 — outbound link signing. Canonical CBOR + Ed25519 sign over the
// six signable link fields. Consumed by `db::create_link_signed` to
// fill the previously-dead `signature` BLOB column on `memory_links`.
pub mod sign;

/// Environment variable override for `agent_id` (used by CLI via clap's
/// `env = "AI_MEMORY_AGENT_ID"`; read directly for MCP fallback).
const ENV_AGENT_ID: &str = "AI_MEMORY_AGENT_ID";

/// Environment variable opt-out for the hostname-revealing default (#198).
/// When truthy (`1`, `true`, `yes`, `on`), the `host:<hostname>:pid-...`
/// fallback is skipped and `anonymous:pid-...` is used instead.
/// `AppConfig::effective_anonymize_default()` mirrors the same semantics
/// from the config file, and CLI startup maps config → this env var so
/// the downstream resolution stays env-only.
const ENV_ANONYMIZE: &str = "AI_MEMORY_ANONYMIZE";

/// Returns true when the hostname-revealing default should be suppressed.
fn anonymize_default_enabled() -> bool {
    let Ok(v) = std::env::var(ENV_ANONYMIZE) else {
        return false;
    };
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Returns a stable-for-this-process discriminator of the form
/// `<pid>-<uuid8>`. Used to make process-level defaults collision-free
/// when many agents share a host (e.g., 25 MCP clients on one machine).
pub fn process_discriminator() -> &'static str {
    static DISCRIMINATOR: OnceLock<String> = OnceLock::new();
    DISCRIMINATOR.get_or_init(|| {
        let pid = std::process::id();
        let uuid_short = short_uuid();
        format!("pid-{pid}-{uuid_short}")
    })
}

/// Returns the machine hostname (OS-reported) or `None` when unavailable.
/// Errors or empty hostnames collapse to `None`.
fn hostname_opt() -> Option<String> {
    let os = gethostname::gethostname();
    let s = os.to_string_lossy().to_string();
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// 8 lowercase hex characters derived from a fresh `UUIDv4`.
fn short_uuid() -> String {
    let id = uuid::Uuid::new_v4();
    let simple = id.simple().to_string(); // 32 hex chars, no hyphens
    simple[..8].to_string()
}

/// Sanitize a string for embedding into an `agent_id`.
///
/// Replaces any character not in the allowlist with `-` and collapses runs.
/// This lets us fold arbitrary client names or hostnames (which may contain
/// dots, spaces, etc.) into valid `agent_id` components without rejecting them.
fn sanitize_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = false;
    for c in input.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    // Trim leading/trailing dashes
    out.trim_matches('-').to_string()
}

/// Resolve `agent_id` for CLI and MCP paths.
///
/// See module docs for precedence. Returned id is always valid per
/// [`validate::validate_agent_id`].
pub fn resolve_agent_id(explicit: Option<&str>, mcp_client: Option<&str>) -> Result<String> {
    // 1. Explicit caller value (already env-merged by clap for CLI)
    if let Some(id) = explicit
        && !id.is_empty()
    {
        validate::validate_agent_id(id)?;
        return Ok(id.to_string());
    }

    // 2. AI_MEMORY_AGENT_ID env var (for MCP path; CLI clap merges this already,
    //    but MCP callers that don't pass it explicitly need this fallback)
    if let Ok(v) = std::env::var(ENV_AGENT_ID)
        && !v.is_empty()
    {
        validate::validate_agent_id(&v)?;
        return Ok(v);
    }

    // 3. MCP clientInfo-synthesized id (only when the MCP server captured it)
    if let Some(client) = mcp_client
        && !client.is_empty()
    {
        let client_s = sanitize_component(client);
        let host_s =
            hostname_opt().map_or_else(|| "unknown".to_string(), |h| sanitize_component(&h));
        let pid = std::process::id();
        let id = format!("ai:{client_s}@{host_s}:pid-{pid}");
        if validate::validate_agent_id(&id).is_ok() {
            return Ok(id);
        }
        // Fall through to host: default if the synthesized id is somehow invalid
    }

    // 4. host:<hostname>:<discriminator> — unless operator opted out (#198).
    if !anonymize_default_enabled()
        && let Some(host) = hostname_opt()
    {
        let host_s = sanitize_component(&host);
        if !host_s.is_empty() {
            let discriminator = process_discriminator();
            let id = format!("host:{host_s}:{discriminator}");
            if validate::validate_agent_id(&id).is_ok() {
                return Ok(id);
            }
        }
    }

    // 5. anonymous:<discriminator>
    let discriminator = process_discriminator();
    let id = format!("anonymous:{discriminator}");
    validate::validate_agent_id(&id)?;
    Ok(id)
}

/// Resolve `agent_id` for a single HTTP request.
///
/// `body` is the `agent_id` field from `CreateMemory`; `header` is the value
/// of the `X-Agent-Id` request header. If neither is present a per-request
/// `anonymous:req-<uuid8>` id is synthesized and a `WARN` is logged so
/// operators notice unauthenticated writes.
pub fn resolve_http_agent_id(body: Option<&str>, header: Option<&str>) -> Result<String> {
    if let Some(id) = body
        && !id.is_empty()
    {
        validate::validate_agent_id(id)?;
        return Ok(id.to_string());
    }
    if let Some(id) = header
        && !id.is_empty()
    {
        validate::validate_agent_id(id)?;
        return Ok(id.to_string());
    }
    let id = format!("anonymous:req-{}", short_uuid());
    tracing::warn!(
        "HTTP memory write without agent_id body field or X-Agent-Id header; assigned {id}"
    );
    validate::validate_agent_id(&id)?;
    Ok(id)
}

/// Preserve `existing.agent_id` through update/dedup.
///
/// Returns a `serde_json::Value` equal to `incoming` with one override:
/// if `existing` carries `metadata.agent_id`, that value is copied into the
/// result (`agent_id` is provenance — immutable after first write).
pub fn preserve_agent_id(
    existing: &serde_json::Value,
    incoming: &serde_json::Value,
) -> serde_json::Value {
    let mut merged = if incoming.is_object() {
        incoming.clone()
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    };
    if let (Some(existing_id), Some(obj)) =
        (existing.get("agent_id").cloned(), merged.as_object_mut())
    {
        obj.insert("agent_id".to_string(), existing_id);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_discriminator_is_stable() {
        let a = process_discriminator();
        let b = process_discriminator();
        assert_eq!(
            a, b,
            "discriminator must be stable for the process lifetime"
        );
        assert!(a.starts_with("pid-"));
        assert!(a.len() >= "pid-1-0000000a".len());
    }

    #[test]
    fn short_uuid_is_8_hex_chars() {
        let s = short_uuid();
        assert_eq!(s.len(), 8);
        assert!(
            s.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }

    #[test]
    fn sanitize_component_preserves_safe_chars() {
        assert_eq!(sanitize_component("claude-code"), "claude-code");
        assert_eq!(sanitize_component("host.example.com"), "host.example.com");
        assert_eq!(sanitize_component("devbox_1"), "devbox_1");
    }

    #[test]
    fn sanitize_component_replaces_unsafe_chars() {
        assert_eq!(sanitize_component("my host"), "my-host");
        assert_eq!(sanitize_component("a/b"), "a-b");
        assert_eq!(sanitize_component("a   b"), "a-b"); // collapses runs
        assert_eq!(sanitize_component("a;b|c"), "a-b-c");
        assert_eq!(sanitize_component("---foo---"), "foo");
    }

    #[test]
    fn resolve_explicit_caller_wins() {
        let id = resolve_agent_id(Some("alice"), Some("claude-code")).unwrap();
        assert_eq!(id, "alice");
    }

    #[test]
    fn resolve_validates_explicit_caller() {
        assert!(resolve_agent_id(Some("alice bob"), None).is_err());
        assert!(resolve_agent_id(Some("a\0null"), None).is_err());
    }

    #[test]
    fn resolve_empty_explicit_falls_through() {
        // Empty explicit should be treated as "not provided" and fall through
        // to the MCP client / host / anonymous branches.
        // SAFETY: test only, no threads concurrent-modify env here.
        // Scrub env so step 2 doesn't short-circuit.
        // SAFETY: single-threaded test block.
        unsafe {
            std::env::remove_var(ENV_AGENT_ID);
        }
        let id = resolve_agent_id(Some(""), None).unwrap();
        assert!(id.starts_with("host:") || id.starts_with("anonymous:"));
    }

    #[test]
    fn resolve_mcp_client_synthesizes_ai_prefix() {
        // SAFETY: single-threaded test block.
        unsafe {
            std::env::remove_var(ENV_AGENT_ID);
        }
        let id = resolve_agent_id(None, Some("claude-code")).unwrap();
        assert!(id.starts_with("ai:claude-code@"));
        assert!(id.contains(":pid-"));
    }

    #[test]
    fn resolve_mcp_client_sanitizes_name() {
        // SAFETY: single-threaded test block.
        unsafe {
            std::env::remove_var(ENV_AGENT_ID);
        }
        let id = resolve_agent_id(None, Some("weird client!")).unwrap();
        assert!(id.starts_with("ai:weird-client@"));
    }

    #[test]
    fn resolve_default_is_host_or_anonymous() {
        // SAFETY: single-threaded test block.
        unsafe {
            std::env::remove_var(ENV_AGENT_ID);
        }
        let id = resolve_agent_id(None, None).unwrap();
        assert!(
            id.starts_with("host:") || id.starts_with("anonymous:"),
            "got: {id}"
        );
    }

    #[test]
    fn resolve_http_body_wins() {
        let id = resolve_http_agent_id(Some("alice"), Some("bob")).unwrap();
        assert_eq!(id, "alice");
    }

    #[test]
    fn resolve_http_header_used_when_body_missing() {
        let id = resolve_http_agent_id(None, Some("bob")).unwrap();
        assert_eq!(id, "bob");
    }

    #[test]
    fn resolve_http_fallback_is_anonymous_req() {
        let id = resolve_http_agent_id(None, None).unwrap();
        assert!(id.starts_with("anonymous:req-"), "got: {id}");
        // Two calls produce distinct request-scoped ids
        let id2 = resolve_http_agent_id(None, None).unwrap();
        assert_ne!(id, id2);
    }

    #[test]
    fn resolve_http_validates_caller_input() {
        assert!(resolve_http_agent_id(Some("has space"), None).is_err());
        assert!(resolve_http_agent_id(None, Some("has\0null")).is_err());
    }

    #[test]
    fn preserve_agent_id_copies_existing() {
        let existing = serde_json::json!({"agent_id": "alice", "foo": "old"});
        let incoming = serde_json::json!({"agent_id": "bob", "foo": "new", "bar": 1});
        let merged = preserve_agent_id(&existing, &incoming);
        assert_eq!(merged["agent_id"], "alice");
        assert_eq!(merged["foo"], "new");
        assert_eq!(merged["bar"], 1);
    }

    #[test]
    fn preserve_agent_id_no_op_when_existing_has_none() {
        let existing = serde_json::json!({"foo": "x"});
        let incoming = serde_json::json!({"agent_id": "bob"});
        let merged = preserve_agent_id(&existing, &incoming);
        assert_eq!(merged["agent_id"], "bob");
    }

    #[test]
    fn preserve_agent_id_handles_non_object_incoming() {
        let existing = serde_json::json!({"agent_id": "alice"});
        let incoming = serde_json::json!("not-an-object");
        let merged = preserve_agent_id(&existing, &incoming);
        assert!(merged.is_object());
        assert_eq!(merged["agent_id"], "alice");
    }
}
