// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `memory_store` input validation + `on_conflict` resolution.
//!
//! #881 (PR-4 extraction): split out of the monolithic
//! `src/mcp/tools/store.rs` so the cheapest gate fires in its own
//! ~120-LOC module. Wire-compat preserved verbatim: every error message
//! and `OnConflict` variant is byte-identical to the pre-#881 inline
//! code path.

/// v0.6.3.1 P2 (G6) — `on_conflict` modes for `memory_store`.
///
/// * `Error`   — refuse the write with a typed CONFLICT error. This is
///               the new default for v2-aware clients.
/// * `Merge`   — keep the v0.6.3 silent-merge upsert behaviour. Default
///               for v1 / unknown clients to preserve backward
///               compatibility.
/// * `Version` — auto-suffix the title with `(2)`, `(3)`, ... to write
///               a distinct row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OnConflict {
    Error,
    Merge,
    Version,
}

impl OnConflict {
    /// # Errors
    ///
    /// Returns the wire-compatible `"invalid on_conflict '...'..."`
    /// error string surfaced to MCP callers when an unknown value
    /// appears in the params.
    pub(super) fn parse(s: &str) -> Result<Self, String> {
        match s {
            "error" => Ok(Self::Error),
            "merge" => Ok(Self::Merge),
            "version" => Ok(Self::Version),
            other => Err(format!(
                "invalid on_conflict '{other}' (expected error|merge|version)"
            )),
        }
    }
}

/// Capability profile detection. v2-aware clients default to `Error`;
/// v1 / unknown clients default to `Merge` to preserve the v0.6.3
/// contract. The determination keys off the MCP client name (captured
/// at `initialize` from `clientInfo.name`). Known v2 clients are
/// listed explicitly so the policy is auditable. The list is
/// intentionally narrow — adding a name here is a deliberate decision
/// that "this client knows how to handle a CONFLICT response from
/// memory_store".
pub(super) fn default_on_conflict_for_client(mcp_client: Option<&str>) -> OnConflict {
    let Some(client) = mcp_client else {
        return OnConflict::Merge;
    };
    // Match on the prefix before any '@' — `ai:foo@host:pid-N` style ids.
    let head = client.split('@').next().unwrap_or(client);
    let normalized = head.to_ascii_lowercase();
    // v2-capable clients (explicitly opted-in via known name).
    const V2_CLIENT_PREFIXES: &[&str] = &["ai:claude-code", "ai:ai-memory-cli/v2"];
    for prefix in V2_CLIENT_PREFIXES {
        if normalized.starts_with(prefix) {
            return OnConflict::Error;
        }
    }
    OnConflict::Merge
}

/// #881 — input-parse + validation + memory-construction extracted
/// from the monolithic `handle_store`. Returns the parsed
/// `(memory, on_conflict, agent_id, explicit_scope)` tuple ready for
/// the governance gate, or a wire-compatible error string on the
/// first validation failure.
///
/// Wire-compat preserved verbatim: every error message is
/// byte-identical to the pre-#881 inline path.
///
/// # Errors
///
/// Returns the typed validation error string surfaced to MCP callers
/// (`"title is required"` / `"invalid tier: ..."` / etc.) when the
/// params fail any of the [`crate::validate`] checks, or
/// `"invalid on_conflict ..."` when an unknown on_conflict mode
/// appears.
#[allow(clippy::too_many_lines)]
pub(super) fn parse_and_build_memory(
    params: &serde_json::Value,
    mcp_client: Option<&str>,
    resolved_ttl: &crate::config::ResolvedTtl,
    conn: &rusqlite::Connection,
) -> Result<(crate::models::Memory, OnConflict, String, Option<String>), String> {
    use crate::models::{ConfidenceSource, Memory, Tier};
    use crate::{db, validate};

    let title = params["title"].as_str().ok_or("title is required")?;
    let content = params["content"].as_str().ok_or("content is required")?;
    let tier_str = params["tier"].as_str().unwrap_or("mid");
    let tier = Tier::from_str(tier_str).ok_or(format!("invalid tier: {tier_str}"))?;
    let namespace = params["namespace"].as_str().unwrap_or("global").to_string();
    let source = params["source"].as_str().unwrap_or("claude").to_string();
    // v0.6.3.1 P2 (G6) — explicit `on_conflict` overrides the per-client default.
    let on_conflict = if let Some(s) = params["on_conflict"].as_str() {
        OnConflict::parse(s)?
    } else {
        default_on_conflict_for_client(mcp_client)
    };
    // B4 (R2-LOW) — clamp to i32 range instead of panicking on out-of-range
    // JSON. A maliciously-crafted `"priority": 9999999999` would have crashed
    // the stdio MCP server pre-fix. `validate_priority` below enforces the
    // semantic 1-10 range, so the clamp is purely a panic guard.
    let priority = i32::try_from(params["priority"].as_i64().unwrap_or(5)).unwrap_or(i32::MAX);
    let confidence = params["confidence"].as_f64().unwrap_or(1.0);
    let tags: Vec<String> = params["tags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    validate::validate_title(title).map_err(|e| e.to_string())?;
    validate::validate_content(content).map_err(|e| e.to_string())?;
    validate::validate_namespace(&namespace).map_err(|e| e.to_string())?;
    validate::validate_source(&source).map_err(|e| e.to_string())?;
    validate::validate_tags(&tags).map_err(|e| e.to_string())?;
    validate::validate_priority(priority).map_err(|e| e.to_string())?;
    validate::validate_confidence(confidence).map_err(|e| e.to_string())?;

    let mut metadata = if params["metadata"].is_object() {
        params["metadata"].clone()
    } else {
        serde_json::json!({})
    };
    // Resolve agent_id via the NHI-hardened precedence chain and merge into
    // metadata. Explicit values win in this order:
    //   1. top-level `agent_id` param
    //   2. embedded `metadata.agent_id` (backward compatible with callers
    //      that supply it inline)
    //   3. env / MCP clientInfo / host / anonymous (handled inside `identity`)
    let explicit_agent_id = params["agent_id"]
        .as_str()
        .or_else(|| metadata.get("agent_id").and_then(serde_json::Value::as_str));
    let agent_id = crate::identity::resolve_agent_id(explicit_agent_id, mcp_client)
        .map_err(|e| e.to_string())?;
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String(agent_id.clone()),
        );
    }
    // #151 scope: top-level `scope` param OR inline metadata.scope
    let explicit_scope = params["scope"]
        .as_str()
        .or_else(|| metadata.get("scope").and_then(serde_json::Value::as_str))
        .map(str::to_string);
    if let Some(ref s) = explicit_scope {
        validate::validate_scope(s).map_err(|e| e.to_string())?;
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("scope".to_string(), serde_json::Value::String(s.clone()));
        }
    }
    validate::validate_metadata(&metadata).map_err(|e| e.to_string())?;

    let now = chrono::Utc::now();
    let expires_at = resolved_ttl
        .ttl_for_tier(&tier)
        .map(|s| (now + chrono::Duration::seconds(s)).to_rfc3339());

    // v0.6.3.1 P2 (G6) — apply the conflict policy BEFORE building the
    // canonical Memory. `Version` mode rewrites `title` to a free suffix;
    // `Error` mode short-circuits with a typed error if the row already
    // exists; `Merge` defers to the legacy code path below.
    let resolved_title = match on_conflict {
        OnConflict::Error => {
            if let Some(existing_id) =
                db::find_by_title_namespace(conn, title, &namespace).map_err(|e| e.to_string())?
            {
                return Err(format!(
                    "CONFLICT: memory with title '{title}' already exists in namespace \
                     '{namespace}' (existing id: {existing_id}). Pass \
                     on_conflict='merge' to update in place or 'version' to suffix the title."
                ));
            }
            title.to_string()
        }
        OnConflict::Version => {
            db::next_versioned_title(conn, title, &namespace).map_err(|e| e.to_string())?
        }
        OnConflict::Merge => title.to_string(),
    };

    // v0.7.x Form 6 (issue #759) — caller-supplied `kind` parameter.
    // Recognised values match the [`crate::models::MemoryKind`] enum.
    // Unknown values are ignored (treated as omission) for forward-compat.
    // `None` means the auto-classify hook (if enabled) decides.
    let caller_kind = params["kind"]
        .as_str()
        .and_then(crate::models::MemoryKind::from_str);

    let source_uri = match params["source_uri"].as_str().map(str::trim) {
        Some(s) if !s.is_empty() => {
            crate::validate::validate_source_uri(s).map_err(|e| e.to_string())?;
            Some(s.to_string())
        }
        _ => None,
    };

    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier,
        namespace,
        title: resolved_title,
        content: content.to_string(),
        tags,
        priority: priority.clamp(1, 10),
        confidence: confidence.clamp(0.0, 1.0),
        source,
        access_count: 0,
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
        last_accessed_at: None,
        expires_at,
        metadata,
        reflection_depth: 0,
        memory_kind: caller_kind.unwrap_or(crate::models::MemoryKind::Observation),
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    };

    Ok((mem, on_conflict, agent_id, explicit_scope))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_conflict_parse_variants() {
        assert_eq!(OnConflict::parse("error").unwrap(), OnConflict::Error);
        assert_eq!(OnConflict::parse("merge").unwrap(), OnConflict::Merge);
        assert_eq!(OnConflict::parse("version").unwrap(), OnConflict::Version);
        assert!(OnConflict::parse("nope").is_err());
    }

    #[test]
    fn default_on_conflict_for_client_matrix() {
        assert_eq!(default_on_conflict_for_client(None), OnConflict::Merge);
        assert_eq!(
            default_on_conflict_for_client(Some("ai:claude-code@host:pid-1")),
            OnConflict::Error
        );
        assert_eq!(
            default_on_conflict_for_client(Some("AI:Claude-Code@whatever")),
            OnConflict::Error,
            "case-insensitive prefix match"
        );
        assert_eq!(
            default_on_conflict_for_client(Some("ai:ai-memory-cli/v2-something")),
            OnConflict::Error
        );
        assert_eq!(
            default_on_conflict_for_client(Some("ai:unknown-client@host:pid-1")),
            OnConflict::Merge
        );
    }
}
