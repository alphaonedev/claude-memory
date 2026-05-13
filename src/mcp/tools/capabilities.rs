// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_capabilities` handlers, CapabilitiesAccept, and capability-summary helpers.

use crate::config::{RerankerMode, TierConfig};
use crate::db;
use crate::reranker::BatchedReranker;
use serde_json::Value;

/// Capabilities schema selector (v0.6.3.1 P1 honesty patch; extended
/// through v0.7.0 A1–A5).
///
/// HTTP callers send `Accept-Capabilities: v1`/`v2`/`v3` to request a
/// shape; MCP callers pass `accept: "v1"`/`"v2"`/`"v3"` to
/// `memory_capabilities`. **As of v0.7.0 A5, the default is v3.** v2
/// stays supported indefinitely for backward compat — clients that
/// pin v2 explicitly continue to get the v2 shape unchanged.
///
/// v3 carries pre-computed calibration fields stacked from the A1–A4
/// increments (top-level `summary` from A1; `to_describe_to_user`
/// from A2; per-tool `tools[].callable_now` from A3;
/// `agent_permitted_families` from A4). v3 is **additive** over v2 —
/// no v2 fields are removed or retyped — so v0.6.4 SDK clients
/// reading v3 by name still resolve every field they used to. The
/// `schema_version` discriminator does change from `"2"` to `"3"`,
/// which is why clients that strict-equality-asserted on it must
/// either relax that or pin `accept="v2"` explicitly.
///
/// v3 requires the live `Profile` (and optionally `McpConfig` +
/// `agent_id`) for the new pre-computed fields, so callers that opt
/// in must reach for [`handle_capabilities_with_conn_v3`] instead of
/// the v1/v2 entry point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilitiesAccept {
    V1,
    V2,
    /// v0.7.0 A1–A4 — additive on top of v2: `summary`,
    /// `to_describe_to_user`, per-tool `tools[].callable_now`,
    /// optional `agent_permitted_families`. **Default since A5.**
    V3,
}

impl CapabilitiesAccept {
    /// Parse the wire value sent by the client. Unknown / missing
    /// values fall back to v3 (the default since v0.7.0 A5).
    /// Whitespace and case insensitive. Explicit `"v2"`/`"2"` still
    /// returns `V2`; explicit `"v1"`/`"1"` still returns `V1`.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "v1" | "1" => Self::V1,
            "v2" | "2" => Self::V2,
            // v0.7.0 A5 — unknown / missing default flips from V2 → V3.
            // Explicit `"v2"` above keeps the v2 wire shape for clients
            // that pin it; everyone else gets v3 (additive over v2).
            _ => Self::V3,
        }
    }
}

/// v0.6.3 (capabilities schema v2 / P1 honesty patch): the canonical
/// capabilities entry point.
///
/// **Live overlays.** When the wrapper has access to the corresponding
/// runtime handle, it overlays:
/// - `features.embedder_loaded` from `embedder_loaded`,
/// - `features.recall_mode_active` from `embedder_loaded` (loaded ⇒
///   `Hybrid`; not loaded but configured ⇒ `KeywordOnly`; configured
///   but failed ⇒ `Degraded`; tier == keyword ⇒ `Disabled`),
/// - `features.reranker_active` from the `CrossEncoder` enum variant
///   (`Neural` / `LexicalFallback` / `Off`),
/// - `features.cross_encoder_reranking` flips to `false` when the
///   neural reranker fell back to lexical (the v1 honesty fix #93),
/// - `models.cross_encoder` annotated with `lexical-fallback` when the
///   neural download failed.
///
/// **Live DB counts.** When `conn` is `Some`, the dynamic blocks
/// (`permissions.active_rules`, `hooks.registered_count`,
/// `approval.pending_requests`) are populated from live counts. DB
/// errors are non-fatal — the report falls back to zero-state so a
/// transient blip cannot 500 the capabilities endpoint.
///
/// **Schema selection.** `accept` controls the wire shape. `V2` is the
/// default and recommended; `V1` projects the v2 report down to the
/// legacy shape for backward compat (see [`Capabilities::to_v1`]).
pub fn handle_capabilities_with_conn(
    tier_config: &TierConfig,
    reranker: Option<&BatchedReranker>,
    embedder_loaded: bool,
    conn: Option<&rusqlite::Connection>,
    accept: CapabilitiesAccept,
) -> Result<Value, String> {
    let caps = build_capabilities_overlay(tier_config, reranker, embedder_loaded, conn);

    // --- Schema selection ---
    match accept {
        CapabilitiesAccept::V2 => serde_json::to_value(caps).map_err(|e| e.to_string()),
        CapabilitiesAccept::V1 => serde_json::to_value(caps.to_v1()).map_err(|e| e.to_string()),
        CapabilitiesAccept::V3 => Err(
            "capabilities v3 requires profile context — call handle_capabilities_with_conn_v3"
                .to_string(),
        ),
    }
}

/// v0.7.0 A1 — the v3-shaped capabilities entry point.
///
/// Same overlay logic as [`handle_capabilities_with_conn`] (factored
/// into [`build_capabilities_overlay`]); additionally computes the
/// top-level `summary` string from the live `profile` state so the
/// LLM gets a pre-computed, plain-language description of its
/// operational tool surface (loaded count, total count, the three
/// named recovery paths for unloaded families).
///
/// HTTP callers reach this path through `Accept-Capabilities: v3`;
/// MCP callers via `accept: "v3"`. The HTTP wire-up is deferred until
/// A5 (which flips the default and threads the profile through
/// `AppState`); A1 lights up the MCP dispatch path only.
pub fn handle_capabilities_with_conn_v3(
    tier_config: &TierConfig,
    reranker: Option<&BatchedReranker>,
    embedder_loaded: bool,
    conn: Option<&rusqlite::Connection>,
    profile: &crate::profile::Profile,
    mcp_config: Option<&crate::config::McpConfig>,
    agent_id: Option<&str>,
    // v0.7.0 B4 — the harness detected from `initialize.clientInfo.name`
    // at MCP handshake time. `None` when no handshake has happened
    // (HTTP callers, or a malformed MCP session that issued
    // `memory_capabilities` before `initialize`); the resulting
    // `your_harness_supports_deferred_registration` field is omitted
    // from the wire via `skip_serializing_if = Option::is_none`.
    harness: Option<&crate::harness::Harness>,
) -> Result<Value, String> {
    let caps = build_capabilities_overlay(tier_config, reranker, embedder_loaded, conn);
    let summary = build_capabilities_summary(profile);
    let describe = build_capabilities_describe_to_user(profile);
    let tools = build_capabilities_tools(profile, mcp_config, agent_id);
    let permitted = build_agent_permitted_families(mcp_config, agent_id);
    // B4 — present only when we know the harness; otherwise omit so
    // unaware callers and HTTP callers see no schema drift.
    let deferred = harness.map(crate::harness::Harness::supports_deferred_registration);
    let mut value = serde_json::to_value(caps.to_v3(summary, describe, tools, permitted, deferred))
        .map_err(|e| e.to_string())?;
    // v0.7.0 (issue #691) — substrate-level agent-action rules engine
    // surface. Stamps two top-level keys onto the `governance` object
    // in the v3 capabilities payload. Operator UI can inspect these
    // without inferring from tool registration order.
    //
    // `agent_action_check` is the honest enforcement label:
    //   "substrate-authoritative-for-internal-ops" — substrate
    //   gates are mechanical at the K9 write path; agent-external
    //   ops are harness-mediated (PreToolUse hook calls
    //   memory_check_agent_action).
    //
    // `rules_immutable_seed` reflects the seed-rules-at-enabled=0
    // posture per design revision 2026-05-13.
    if let Some(obj) = value.as_object_mut() {
        let gov = obj
            .entry("governance".to_string())
            .or_insert_with(|| serde_json::json!({}));
        if let Some(gov_obj) = gov.as_object_mut() {
            gov_obj.insert(
                "agent_action_check".to_string(),
                serde_json::Value::String("substrate-authoritative-for-internal-ops".to_string()),
            );
            gov_obj.insert(
                "rules_immutable_seed".to_string(),
                serde_json::Value::Bool(true),
            );
        }
    }
    Ok(value)
}

/// Build the runtime-overlaid [`Capabilities`] document. Shared between
/// the v1/v2 entry point [`handle_capabilities_with_conn`] and the v3
/// entry point [`handle_capabilities_with_conn_v3`] so the overlay
/// logic stays single-sourced.
fn build_capabilities_overlay(
    tier_config: &TierConfig,
    reranker: Option<&BatchedReranker>,
    embedder_loaded: bool,
    conn: Option<&rusqlite::Connection>,
) -> crate::config::Capabilities {
    let mut caps = tier_config.capabilities();

    // --- Reranker live state (P1) ---
    caps.features.reranker_active = match reranker {
        Some(ce) if ce.is_neural() => RerankerMode::Neural,
        Some(_) => {
            // Lexical fallback — neural download or load failed.
            caps.features.cross_encoder_reranking = false;
            caps.models.cross_encoder = "lexical-fallback (neural download failed)".to_string();
            RerankerMode::LexicalFallback
        }
        None => RerankerMode::Off,
    };

    // --- Embedder live state (P1, S18) ---
    caps.features.embedder_loaded = embedder_loaded;
    caps.features.recall_mode_active = compute_recall_mode(tier_config, embedder_loaded);

    // --- HNSW eviction surface (P3, G2) ---
    caps.hnsw.evictions_total = crate::hnsw::index_evictions_total();
    caps.hnsw.evicted_recently = crate::hnsw::evicted_recently(60);

    // --- Live DB-count overlays ---
    if let Some(c) = conn {
        if let Ok(n) = db::count_active_governance_rules(c) {
            caps.permissions.active_rules = n;
        }
        // v0.7.0 K5 — populate `permissions.rule_summary` with a
        // one-line summary per active governance policy, sorted lex by
        // namespace. The DB layer returns the rows already sorted, so
        // the format pass preserves order. Failure is silent (best-
        // effort): a malformed policy must not take down the whole
        // capabilities response. `Vec::is_empty` + `skip_serializing_if`
        // means an unconfigured deployment sees the field omitted from
        // the wire entirely (matching the v0.6.3.1 honesty disclosure
        // that the field was previously dropped because no per-rule
        // serializer existed).
        if let Ok(rules) = db::list_active_governance_policies(c) {
            caps.permissions.rule_summary = rules
                .into_iter()
                .map(|(ns, p)| format_rule_summary(&ns, &p))
                .collect();
        }
        if let Ok(n) = db::count_subscriptions(c) {
            caps.hooks.registered_count = n;
        }
        if let Ok(n) = db::count_pending_actions_by_status(c, "pending") {
            caps.approval.pending_requests = n;
        }
    }

    caps
}

/// v0.7.0 K5 — format a single [`GovernancePolicy`] as a one-line
/// human-readable summary, prefixed with the namespace it governs.
///
/// Output shape:
/// ```text
/// "alphaone/eng — write=approve, promote=any, delete=owner, approver=human, inherit=true"
/// ```
///
/// The `approver` rendering follows the [`ApproverType`] discriminator
/// tag (`human` / `agent:<id>` / `consensus:<n>`) so an operator can tell
/// apart a `Human` policy from a `Consensus(3)` policy without fanning
/// out to `memory_namespace_get_standard`. `inherit` is rendered as a
/// boolean string so the line stays scan-friendly.
///
/// Public so the capabilities-v3 integration tests (track A, K5) can
/// pin the exact wire shape without re-implementing the formatter.
#[must_use]
pub fn format_rule_summary(namespace: &str, policy: &crate::models::GovernancePolicy) -> String {
    use crate::models::ApproverType;
    let approver = match &policy.approver {
        ApproverType::Human => "human".to_string(),
        ApproverType::Agent(id) => format!("agent:{id}"),
        ApproverType::Consensus(n) => format!("consensus:{n}"),
    };
    format!(
        "{namespace} — write={write}, promote={promote}, delete={delete}, approver={approver}, inherit={inherit}",
        write = policy.write.as_str(),
        promote = policy.promote.as_str(),
        delete = policy.delete.as_str(),
        inherit = policy.inherit,
    )
}

/// v0.7.0 A1 — build the capabilities-v3 `summary` string from the live
/// `Profile` state.
///
/// The summary names: how many tools are advertised in `tools/list`
/// under the active profile vs how many exist in total, and the three
/// recovery paths an LLM can take to reach unloaded tools (`--profile`
/// CLI flag, [`memory_load_family`](#) — landing in B1, and
/// [`memory_smart_load`](#) — landing in B2).
///
/// The result is a single plain-language string, intentionally written
/// for an LLM to repeat verbatim when an end-user asks "what tools do
/// you have?" — see the A2 increment for the explicit
/// `to_describe_to_user` field.
#[must_use]
pub fn build_capabilities_summary(profile: &crate::profile::Profile) -> String {
    use crate::profile::{ALWAYS_ON_TOOLS, Family};

    // Round-2 F13 — substantive memory-tool count, EXCLUDING the
    // always-on bootstrap (`memory_capabilities`). Reconciles with
    // `build_capabilities_describe_to_user`'s "{n_loaded} memory
    // tool{s}" phrasing so the summary number agrees with the
    // user-facing sentence (e.g. both report 50 for `--profile full`,
    // not "51 of 51 tools" in the summary alongside "50 memory
    // tools" in the describe — which was the F13 off-by-one).
    let total: usize = Family::all()
        .iter()
        .map(|f| f.expected_tool_count())
        .sum::<usize>()
        .saturating_sub(ALWAYS_ON_TOOLS.len());

    // Visible memory tools = profile-loaded family tools, minus any
    // always-on bootstrap that lives in a family the profile loads
    // (otherwise `memory_capabilities` would be double-counted for
    // profiles that load `Meta`). The bootstrap still appears in
    // `tools/list` — it just isn't a "memory tool" in the user-facing
    // sense.
    let from_families: usize = profile.expected_tool_count();
    let always_on_in_loaded_family: usize = ALWAYS_ON_TOOLS
        .iter()
        .filter(|name| Family::for_tool(name).is_some_and(|f| profile.includes(f)))
        .count();
    let visible = from_families.saturating_sub(always_on_in_loaded_family);
    let unloaded = total.saturating_sub(visible);
    let label = profile_summary_label(profile);

    format!(
        "{visible} of {total} memory tools are advertised in tools/list under the current \
         profile ({label}). The other {unloaded} are listed in this manifest but NOT directly \
         callable. To use any unloaded tool, choose one of: \
         (a) restart the server with --profile <family> or --profile full, \
         (b) call memory_load_family(family=<name>) — preferred, \
         (c) call memory_smart_load(intent='<plain language>') — easiest, \
         (d) call the tool by name and recover from JSON-RPC -32601."
    )
}

/// v0.7.0 A2 — build the capabilities-v3 `to_describe_to_user` string.
///
/// This is the canonical plain-language sentence the LLM should repeat
/// (verbatim) when an end-user asks "what tools do you have?". It
/// names how many tools are loaded right now, lists the first few by
/// short name (without the `memory_` prefix, since the prefix is MCP
/// jargon a user doesn't care about), reports how many are unloaded,
/// and gives an end-user-friendly recovery hint ("I can load them on
/// demand, or you can restart the server with a different profile").
///
/// Tone constraint (per A2 spec): NO MCP jargon. No mention of
/// `tools/list`, `JSON-RPC`, or `--profile <family>`. Reads like a
/// normal sentence a person would write.
///
/// The always-on bootstrap (`memory_capabilities`) is intentionally
/// excluded from the loaded-tool preview — to a user, it's plumbing,
/// not a feature.
#[must_use]
pub fn build_capabilities_describe_to_user(profile: &crate::profile::Profile) -> String {
    use crate::profile::Family;

    // Loaded vs unloaded by family membership. The always-on bootstrap
    // sits in `Family::Meta`; under e.g. `--profile core` Meta isn't
    // loaded, so `memory_capabilities` would normally count as
    // unloaded. We strip it from BOTH sides — the user-facing sentence
    // talks about the substantive tool surface, not the
    // runtime-discovery bootstrap.
    let loaded_tools: Vec<&'static str> = Family::all()
        .iter()
        .filter(|f| profile.includes(**f))
        .flat_map(|f| f.tool_names().iter().copied())
        .filter(|name| !crate::profile::ALWAYS_ON_TOOLS.contains(name))
        .collect();
    let unloaded_tools: Vec<&'static str> = Family::all()
        .iter()
        .filter(|f| !profile.includes(**f))
        .flat_map(|f| f.tool_names().iter().copied())
        .filter(|name| !crate::profile::ALWAYS_ON_TOOLS.contains(name))
        .collect();

    let n_loaded = loaded_tools.len();
    let n_unloaded = unloaded_tools.len();

    // Preview the first 5 loaded tools by short name (strip the
    // `memory_` prefix). Five matches the canonical example in the
    // A2 NHI prompt and lines up with the size of the smallest
    // (`core`) profile so the preview is a complete enumeration there.
    let preview_loaded = loaded_tools
        .iter()
        .take(5)
        .map(|name| short_tool_name(name))
        .collect::<Vec<_>>()
        .join(", ");
    let loaded_more_marker = if n_loaded > 5 { ", ..." } else { "" };

    if n_unloaded == 0 {
        format!(
            "I can directly use all {n_loaded} memory tools right now \
             ({preview_loaded}{loaded_more_marker}). Nothing more to load — \
             the full memory surface is already active."
        )
    } else {
        // Preview 4 unloaded tool names — the canonical example uses 4
        // (link, kg_query, consolidate, delete) followed by ", etc.".
        let preview_unloaded = unloaded_tools
            .iter()
            .take(4)
            .map(|name| short_tool_name(name))
            .collect::<Vec<_>>()
            .join(", ");
        let plural_loaded = if n_loaded == 1 { "" } else { "s" };
        format!(
            "I can directly use {n_loaded} memory tool{plural_loaded} right now \
             ({preview_loaded}{loaded_more_marker}). {n_unloaded} more \
             ({preview_unloaded}, etc.) are available on demand — I can load them \
             if you ask for something that needs them, or you can restart the \
             server with a different profile."
        )
    }
}

/// Strip the `memory_` prefix from a tool name for end-user-facing
/// previews. v0.7.0 A2 — the prefix is MCP jargon; a user doesn't care
/// that every tool name starts with the same five characters.
fn short_tool_name(name: &'static str) -> &'static str {
    name.strip_prefix("memory_").unwrap_or(name)
}

/// v0.7.0 A3 — build the per-tool array carried in the
/// capabilities-v3 `tools` field.
///
/// Each entry's `loaded` mirrors `Profile::loads(name)`. Each entry's
/// `callable_now` is `loaded && agent_can_call(agent_id, family)` —
/// when the `[mcp.allowlist]` is disabled (no table or empty), the
/// allowlist gate is `Disabled` and the AND collapses to just
/// `loaded`. When the allowlist is active and the requesting agent
/// has no entry granting the tool's family, `callable_now == false`
/// even though `loaded == true`.
///
/// The order of the returned vector matches `crate::mcp::tool_definitions()`'s
/// registration walk so a sequential reader gets a stable
/// presentation matching the order in `tools/list`.
#[must_use]
pub fn build_capabilities_tools(
    profile: &crate::profile::Profile,
    mcp_config: Option<&crate::config::McpConfig>,
    agent_id: Option<&str>,
) -> Vec<crate::config::ToolEntry> {
    use crate::config::{AllowlistDecision, ToolEntry};
    use crate::profile::{ALWAYS_ON_TOOLS, Family};

    let mut entries: Vec<ToolEntry> = Vec::with_capacity(50);

    for fam in Family::all() {
        let family_name = fam.name();
        let loaded = profile.includes(*fam);
        // Whether THIS agent can call tools in this family — disabled
        // allowlist falls through to `loaded`. When the allowlist is
        // configured but denies the family, callable_now collapses to
        // false regardless of loaded.
        let allowed = match mcp_config {
            Some(cfg) => match cfg.allowlist_decision(agent_id, family_name) {
                AllowlistDecision::Disabled | AllowlistDecision::Allow => true,
                AllowlistDecision::Deny => false,
            },
            None => true,
        };
        for name in fam.tool_names() {
            entries.push(ToolEntry {
                name: (*name).to_string(),
                family: family_name.to_string(),
                loaded,
                callable_now: loaded && allowed,
            });
        }
    }

    // Always-on bootstraps that are NOT counted via a normal family
    // walk (in v0.6.4, ALWAYS_ON_TOOLS only contains
    // `memory_capabilities` which already lives in `Family::Meta`, so
    // it's already in `entries`. Future bootstraps that don't sit in a
    // family at all would be appended here with family="always_on" and
    // unconditionally loaded/callable.)
    for name in ALWAYS_ON_TOOLS {
        if !entries.iter().any(|e| e.name == *name) {
            entries.push(ToolEntry {
                name: (*name).to_string(),
                family: "always_on".to_string(),
                loaded: true,
                callable_now: true,
            });
        }
    }

    entries
}

/// v0.7.0 A4 — compute the optional `agent_permitted_families` field
/// for a v3 capabilities response.
///
/// Returns:
/// - `Some(Vec<...>)` (possibly empty) when `[mcp.allowlist]` is
///   configured AND an `agent_id` was provided. The vector lists the
///   canonical family names the agent is permitted to access (per the
///   `Family::all()` registration order).
/// - `None` when the allowlist is disabled (no table, empty table, or
///   `mcp_config = None`) OR when no `agent_id` was provided.
///   `serde(skip_serializing_if = "Option::is_none")` on the field
///   means a `None` value drops the field from the wire entirely so
///   v2-shaped consumers don't see drift from A4 alone.
///
/// The wildcard pattern `"*"` participates in the per-family
/// allowlist_decision call — this matches the existing v0.6.4-008
/// resolution semantics, so a `"*" = ["core"]` row grants every agent
/// access to `core` even when their explicit row is missing.
#[must_use]
pub fn build_agent_permitted_families(
    mcp_config: Option<&crate::config::McpConfig>,
    agent_id: Option<&str>,
) -> Option<Vec<String>> {
    use crate::config::AllowlistDecision;
    use crate::profile::Family;

    // A4 spec: omit the field when allowlist disabled OR no agent_id.
    let cfg = mcp_config?;
    let aid = agent_id?;
    let table = cfg.allowlist.as_ref()?;
    if table.is_empty() {
        // Allowlist Disabled (per the v0.6.4-008 contract): omit.
        return None;
    }

    let permitted: Vec<String> = Family::all()
        .iter()
        .filter(|fam| {
            matches!(
                cfg.allowlist_decision(Some(aid), fam.name()),
                AllowlistDecision::Allow
            )
        })
        .map(|fam| fam.name().to_string())
        .collect();

    Some(permitted)
}

/// Return a stable label for a profile's summary string. Named profiles
/// (core/graph/admin/power/full) use their canonical name; custom
/// profiles use the comma-joined family list (matches the
/// `--profile core,graph,archive` CLI form).
fn profile_summary_label(profile: &crate::profile::Profile) -> String {
    use crate::profile::Profile;
    if *profile == Profile::full() {
        "full".to_string()
    } else if *profile == Profile::core() {
        "core".to_string()
    } else if *profile == Profile::graph() {
        "graph".to_string()
    } else if *profile == Profile::admin() {
        "admin".to_string()
    } else if *profile == Profile::power() {
        "power".to_string()
    } else {
        profile
            .families()
            .iter()
            .map(|f| f.name())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Round-2 F13 — derive the runtime-effective tier label from the
/// presence of the LLM, embedder, and reranker handles. Mirrors the
/// boot banner string emitted by `serve_mcp` so the
/// `memory_capabilities` response and the daemon log agree on what
/// the daemon is actually doing — independent of `tier_config.tier`,
/// which only reflects the configured (build-time) tier and can lag
/// the runtime when an embedder/LLM fails to load.
#[must_use]
pub fn effective_tier_label(has_llm: bool, has_embedder: bool, has_reranker: bool) -> &'static str {
    if has_llm && has_embedder && has_reranker {
        "autonomous"
    } else if has_llm && has_embedder {
        "smart"
    } else if has_embedder {
        "semantic"
    } else {
        "keyword"
    }
}

/// Round-2 F13 — overlay per-tool `inputSchema` and/or `docstring`
/// onto the top-level `tools[]` array of a v2/v3 capabilities
/// response. Called on the no-family path when `include_schema=true`
/// and/or `verbose=true` is set on the top-level
/// `memory_capabilities` invocation. Without an overlay, those
/// flags were inert at the top level (only the family drilldown
/// honoured them).
///
/// `include_schema=true` — inject the canonical
/// `crate::mcp::tool_definitions()[name].inputSchema` for every tool entry.
/// `verbose=true` — inject `docstring` (sourced from the long-form
/// `docs` field on `crate::mcp::tool_definitions()`).
///
/// Tools that aren't currently loaded under the active profile (i.e.
/// `loaded=false` in the v3 `tools[]`) get the same overlay so a
/// caller can decide whether to drill in via
/// `memory_load_family`/`memory_smart_load`.
pub fn overlay_tool_payloads(
    obj: &mut serde_json::Map<String, Value>,
    _profile: &crate::profile::Profile,
    include_schema: bool,
    verbose: bool,
) {
    if !include_schema && !verbose {
        return;
    }

    // Build a name → (docs, inputSchema) lookup from the canonical
    // tool catalog. Done once per call; cheap (~50 entries).
    let defs = crate::mcp::tool_definitions();
    let lookup: std::collections::HashMap<String, (Option<Value>, Option<Value>)> = defs
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| {
                    let name = t.get("name").and_then(Value::as_str)?.to_string();
                    let docs = t.get("docs").cloned();
                    let schema = t.get("inputSchema").cloned();
                    Some((name, (docs, schema)))
                })
                .collect()
        })
        .unwrap_or_default();

    // The v3 response carries a top-level `tools` array of
    // `ToolEntry` objects; the v2 response does not. For v2 callers
    // passing include_schema/verbose, synthesize a parallel
    // `tool_payloads` array so the overlay is still discoverable
    // without disturbing the v2 wire shape.
    if let Some(tools) = obj.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools.iter_mut() {
            let Some(tool_obj) = tool.as_object_mut() else {
                continue;
            };
            let Some(name) = tool_obj.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some((docs, schema)) = lookup.get(name) else {
                continue;
            };
            if include_schema && let Some(s) = schema {
                tool_obj.insert("inputSchema".to_string(), s.clone());
            }
            if verbose && let Some(d) = docs {
                tool_obj.insert("docstring".to_string(), d.clone());
            }
        }
    } else {
        // v2 path — no `tools` field exists. Synthesize a flat
        // `tool_payloads` array so the overlay is still on the wire.
        let payloads: Vec<Value> = lookup
            .iter()
            .map(|(name, (docs, schema))| {
                let mut entry = serde_json::Map::new();
                entry.insert("name".to_string(), Value::String(name.clone()));
                if include_schema && let Some(s) = schema {
                    entry.insert("inputSchema".to_string(), s.clone());
                }
                if verbose && let Some(d) = docs {
                    entry.insert("docstring".to_string(), d.clone());
                }
                Value::Object(entry)
            })
            .collect();
        obj.insert("tool_payloads".to_string(), Value::Array(payloads));
    }
}

/// Compute the live `recall_mode_active` tag from the configured tier
/// and the runtime embedder-loaded signal. P1 honesty patch.
///
/// - Tier configured no embedder (keyword tier) → `Disabled`.
/// - Tier configured an embedder and it loaded → `Hybrid`.
/// - Tier configured an embedder but it did not load → `Degraded`.
/// - (Reserved) `KeywordOnly` is returned only when the daemon has an
///   embedder configured but the operator explicitly disabled hybrid
///   blending — not possible in v0.6.3.1, so unreachable today.
fn compute_recall_mode(
    tier_config: &TierConfig,
    embedder_loaded: bool,
) -> crate::config::RecallMode {
    use crate::config::RecallMode;
    if tier_config.embedding_model.is_none() {
        RecallMode::Disabled
    } else if embedder_loaded {
        RecallMode::Hybrid
    } else {
        RecallMode::Degraded
    }
}
