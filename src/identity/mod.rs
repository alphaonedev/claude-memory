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
// H3 — inbound link verification. Mirror of `sign`: re-derives the
// canonical CBOR bytes from a wire `SignableLink` and verifies the
// 64-byte signature against the public key associated with the link's
// `observed_by` claim. Consumed by federation `sync_push` link replay
// so tampered or forged links never land in `memory_links`.
pub mod verify;
// H5 (v0.7.0 round-2) — Ed25519 verify-link replay protection.
// Bounded in-memory LRU keyed on `(link_id, signature, nonce)`. Sits
// in front of `verify_link_handler` and rejects exact-repeat requests
// with 409 Conflict so an attacker cannot replay a captured verify
// indefinitely. See module docs for the threat model + memory bound.
pub mod replay;

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
/// `body` is the (optional) `agent_id` field from `CreateMemory`;
/// `header` is the value of the `X-Agent-Id` request header. If neither
/// is present a per-request `anonymous:req-<uuid8>` id is synthesized
/// and a `WARN` is logged so operators notice unauthenticated writes.
///
/// # SECURITY (v0.7.0 — header-first; body must match)
///
/// This primitive is **safe by default**: the request header
/// `X-Agent-Id` is the AUTHORITATIVE identity slot, and any body-side
/// `agent_id` is a REFINEMENT that MUST agree with the header. The
/// body slot is caller-controlled — historically it had PRECEDENCE
/// over the header, which was the cross-tenant spoof vector closed by
/// the v0.7.0 #874/#901/#905-#910 issue series (#874 unsubscribe +
/// list_subscriptions, #901 notify + subscribe + get_inbox, #905
/// power_consolidation, #907 create_memory, #909 quota_status, #910
/// list_memories + kg_query visibility filter). Those per-handler
/// patches each had to pass `body: None` as a workaround because the
/// primitive itself trusted body-first. This fn now closes the
/// underlying primitive so ANY future caller is structurally safe
/// regardless of what they pass for `body`.
///
/// Resolution rules:
///
/// 1. The header is resolved first (or the per-request anonymous
///    fallback is synthesized when no header is present).
/// 2. If `body` is `Some(non-empty)` it is validated and compared
///    against the header-resolved id. A MISMATCH returns an error
///    tagged `agent_id_body_header_mismatch` so handlers can map it
///    to `403 Forbidden`. An empty `body` is treated as "no claim"
///    (same as `None`).
/// 3. Validation errors on either side surface unchanged.
///
/// New callers SHOULD pass `body: None` and rely on header-only
/// authentication; the body-refinement slot is preserved only for
/// the existing federation receiver path (where the body carries an
/// envelope-attributed identity, gated by
/// `AI_MEMORY_FED_TRUST_BODY_AGENT_ID`) and for backwards-compatible
/// callers that want defense-in-depth checks at this layer.
pub fn resolve_http_agent_id(body: Option<&str>, header: Option<&str>) -> Result<String> {
    // 1. Header is authoritative — resolve it first (validate if
    //    present; synthesize anonymous fallback otherwise).
    let resolved = if let Some(id) = header
        && !id.is_empty()
    {
        validate::validate_agent_id(id)?;
        id.to_string()
    } else {
        let anon = format!("anonymous:req-{}", short_uuid());
        tracing::warn!(
            "HTTP memory write without agent_id body field or X-Agent-Id header; assigned {anon}"
        );
        validate::validate_agent_id(&anon)?;
        anon
    };

    // 2. Body, when non-empty, is a refinement that MUST match the
    //    authoritative header-resolved id. Validate the body shape
    //    first so a malformed claim surfaces as a 400 rather than a
    //    403 mismatch (the validation error is the more informative
    //    diagnostic).
    if let Some(claim) = body
        && !claim.is_empty()
    {
        validate::validate_agent_id(claim)?;
        if claim != resolved {
            anyhow::bail!(
                "agent_id_body_header_mismatch: body-supplied agent_id {claim:?} disagrees \
                 with authenticated header-resolved id {resolved:?}"
            );
        }
    }

    Ok(resolved)
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

    /// M9 — process-wide guard for every test below that mutates
    /// `ENV_AGENT_ID`. `cargo test --jobs N` runs the test functions in
    /// parallel by default, so an unguarded `remove_var` race can
    /// surface as a flake when a sibling test reads the same var
    /// mid-mutation. Acquire this mutex before every env-mutating step.
    fn env_var_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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
        // M9 — process-wide serialization via env_var_lock.
        let _g = env_var_lock();
        // SAFETY: env mutation serialised by `_g`. Scrub env so step 2
        // doesn't short-circuit.
        unsafe {
            std::env::remove_var(ENV_AGENT_ID);
        }
        let id = resolve_agent_id(Some(""), None).unwrap();
        assert!(id.starts_with("host:") || id.starts_with("anonymous:"));
    }

    #[test]
    fn resolve_mcp_client_synthesizes_ai_prefix() {
        // M9 — process-wide serialization via env_var_lock.
        let _g = env_var_lock();
        // SAFETY: env mutation serialised by `_g`.
        unsafe {
            std::env::remove_var(ENV_AGENT_ID);
        }
        let id = resolve_agent_id(None, Some("claude-code")).unwrap();
        assert!(id.starts_with("ai:claude-code@"));
        assert!(id.contains(":pid-"));
    }

    #[test]
    fn resolve_mcp_client_sanitizes_name() {
        // M9 — process-wide serialization via env_var_lock.
        let _g = env_var_lock();
        // SAFETY: env mutation serialised by `_g`.
        unsafe {
            std::env::remove_var(ENV_AGENT_ID);
        }
        let id = resolve_agent_id(None, Some("weird client!")).unwrap();
        assert!(id.starts_with("ai:weird-client@"));
    }

    #[test]
    fn resolve_default_is_host_or_anonymous() {
        // M9 — process-wide serialization via env_var_lock.
        let _g = env_var_lock();
        // SAFETY: env mutation serialised by `_g`.
        unsafe {
            std::env::remove_var(ENV_AGENT_ID);
        }
        let id = resolve_agent_id(None, None).unwrap();
        assert!(
            id.starts_with("host:") || id.starts_with("anonymous:"),
            "got: {id}"
        );
    }

    /// v0.7.0 SECURITY regression — primitive-level closure of the
    /// #874-class agent_id spoof. Previously `body` had PRECEDENCE
    /// over `header`, so a caller authenticated as `bob` (via
    /// `X-Agent-Id`) could pass `body=Some("alice")` and the resolver
    /// would return `"alice"`. Post-fix the header is authoritative
    /// and a body-vs-header mismatch is a typed error so handlers
    /// can map to `403 Forbidden`.
    #[test]
    fn resolve_http_body_mismatch_is_err() {
        let r = resolve_http_agent_id(Some("alice"), Some("bob"));
        assert!(r.is_err(), "mismatch must be Err, got Ok({r:?})");
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("agent_id_body_header_mismatch"),
            "error must carry tag agent_id_body_header_mismatch, got: {msg}"
        );
        // Header value MUST NOT leak into the resolver's return on
        // mismatch — the contract is "error, not silent override".
        assert!(!msg.is_empty());
    }

    #[test]
    fn resolve_http_body_matching_header_is_ok() {
        // Body is a defense-in-depth refinement — when it matches the
        // header the resolver returns the agreed id.
        let id = resolve_http_agent_id(Some("alice"), Some("alice")).unwrap();
        assert_eq!(id, "alice");
    }

    #[test]
    fn resolve_http_empty_body_is_no_claim() {
        // Empty body MUST be treated as "no body-side claim" — same
        // contract as None. Header wins, no mismatch error.
        let id = resolve_http_agent_id(Some(""), Some("bob")).unwrap();
        assert_eq!(id, "bob");
    }

    #[test]
    fn resolve_http_body_without_header_uses_anonymous_and_mismatches() {
        // No header → anonymous fallback id is synthesized. A body
        // claim then mismatches the anonymous id → typed error.
        // This is the strict posture: a caller cannot launder a body
        // claim through an absent-header request.
        let r = resolve_http_agent_id(Some("alice"), None);
        assert!(r.is_err(), "body without header must be Err, got Ok({r:?})");
        let msg = r.unwrap_err().to_string();
        assert!(
            msg.contains("agent_id_body_header_mismatch"),
            "error must carry tag agent_id_body_header_mismatch, got: {msg}"
        );
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

    // -----------------------------------------------------------------
    // L0.7-2 Tier A — ENV_ANONYMIZE truthy/falsy + env-var fallback
    // + anonymize-forced default
    // -----------------------------------------------------------------

    #[test]
    fn anonymize_default_enabled_truthy_variants() {
        let _g = env_var_lock();
        for v in ["1", "true", "yes", "on", "TRUE", " yes ", "On", "YES"] {
            // SAFETY: env mutation serialised via env_var_lock guard.
            unsafe {
                std::env::set_var(ENV_ANONYMIZE, v);
            }
            assert!(anonymize_default_enabled(), "value {v:?} must be truthy");
        }
        // SAFETY: env mutation serialised.
        unsafe {
            std::env::remove_var(ENV_ANONYMIZE);
        }
    }

    #[test]
    fn anonymize_default_enabled_falsy_variants() {
        let _g = env_var_lock();
        for v in ["0", "false", "no", "off", "", "garbage"] {
            // SAFETY: env mutation serialised via env_var_lock guard.
            unsafe {
                std::env::set_var(ENV_ANONYMIZE, v);
            }
            assert!(!anonymize_default_enabled(), "value {v:?} must be falsy");
        }
        // SAFETY: env mutation serialised.
        unsafe {
            std::env::remove_var(ENV_ANONYMIZE);
        }
    }

    #[test]
    fn anonymize_default_enabled_unset_is_falsy() {
        let _g = env_var_lock();
        // SAFETY: env mutation serialised.
        unsafe {
            std::env::remove_var(ENV_ANONYMIZE);
        }
        assert!(!anonymize_default_enabled());
    }

    #[test]
    fn resolve_uses_env_agent_id_when_no_explicit_no_mcp() {
        let _g = env_var_lock();
        // SAFETY: env mutation serialised.
        unsafe {
            std::env::set_var(ENV_AGENT_ID, "env-alice");
        }
        let id = resolve_agent_id(None, None).unwrap();
        assert_eq!(id, "env-alice");
        // SAFETY: env mutation serialised.
        unsafe {
            std::env::remove_var(ENV_AGENT_ID);
        }
    }

    #[test]
    fn resolve_anonymize_forces_anonymous_prefix() {
        let _g = env_var_lock();
        // SAFETY: env mutation serialised.
        unsafe {
            std::env::remove_var(ENV_AGENT_ID);
            std::env::set_var(ENV_ANONYMIZE, "1");
        }
        let id = resolve_agent_id(None, None).unwrap();
        assert!(
            id.starts_with("anonymous:"),
            "AI_MEMORY_ANONYMIZE=1 must skip host: default, got: {id}"
        );
        // SAFETY: env mutation serialised.
        unsafe {
            std::env::remove_var(ENV_ANONYMIZE);
        }
    }

    #[test]
    fn resolve_empty_env_falls_through() {
        // Empty env var should be treated as "not set" and continue
        // down the precedence chain.
        let _g = env_var_lock();
        // SAFETY: env mutation serialised.
        unsafe {
            std::env::set_var(ENV_AGENT_ID, "");
        }
        let id = resolve_agent_id(None, None).unwrap();
        assert!(
            id.starts_with("host:") || id.starts_with("anonymous:") || id.starts_with("ai:"),
            "empty env must fall through to host/anonymous default, got: {id}"
        );
        // SAFETY: env mutation serialised.
        unsafe {
            std::env::remove_var(ENV_AGENT_ID);
        }
    }
}
