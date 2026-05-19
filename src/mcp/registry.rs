// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Tool registry: tool definitions, profile filtering, capabilities family dispatch.

use serde_json::{Value, json};

// --- Tool definitions ---

/// Version tag for the `tools/list` response schema. Bumped whenever
/// an existing tool's shape changes in a breaking way (renamed params,
/// tightened schemas, removed options). Adding a new tool is additive
/// and does NOT require a bump. Ultrareview #351.
///
/// v0.7 C4 — bumped to `2026-05-06` because `tools/list` now ships
/// the trimmed schema by default (optional params hidden unless the
/// caller passes `verbose=true` to `memory_capabilities`). The wire
/// shape of every existing tool's `inputSchema.properties` map is
/// strictly a subset of the prior version, which is a breaking change
/// for any client that was reading the long-tail optional params off
/// `tools/list` directly. The full schema is still reachable via
/// `memory_capabilities { family=<f>, include_schema=true, verbose=true }`.
const TOOLS_VERSION: &str = "2026-05-06";

/// v0.7 C4 — tools/list optional-param trim allow-list.
///
/// **Historical (pre-#859):** optional properties (those NOT in
/// `inputSchema.required`) were dropped from the default `tools/list`
/// payload UNLESS their name appeared here. This hid the long-tail
/// optionals (`max_depth`, `relation`, `confidence`, …) from MCP
/// clients reading the wire schema directly, breaking NHI runtime
/// discovery (issue #859).
///
/// **Current (#859 / v0.7.0 fix):** every property is preserved on
/// the wire; the allow-list is retained for narrative purposes (and
/// as a marker if a future tightening reintroduces a per-name gate)
/// but is no longer consulted by [`trim_optional_params`].
#[allow(dead_code)]
const C4_KEEP_OPTIONAL_PARAMS: &[&str] = &["namespace", "format"];

/// v0.7 C4 (rev #859) — wire-schema property pruner.
///
/// **What it does on the wire-form schema:**
/// - **Preserves** every `inputSchema.properties` entry, including
///   the long-tail optionals (`max_depth`, `relation`, `valid_at`,
///   `allowed_agents`, `limit`, `include_invalidated`, …). NHI
///   agents reading `tools/list` need to DISCOVER what knobs exist
///   to set them.
/// - **Preserves** every property's structural metadata: `type`,
///   `enum`, `minimum`, `maximum`, `default`, `items`, `minItems`,
///   `maxItems`, `oneOf`. These are load-bearing for argument
///   validation on the client side.
/// - **Preserves** the `required` array — clients still need to
///   know which params are mandatory.
/// - **Strips** per-property `description` text (the prose). The
///   long-form prose is reachable via `memory_capabilities {
///   family=<f>, include_schema=true, verbose=true }`. Callers
///   that just want to know "what params does this tool accept"
///   no longer pay for the prose on every `tools/list` request.
/// - **Strips** per-property `default` values that are non-trivial
///   strings (>32 chars). Numeric / boolean / short-string defaults
///   stay (they're tiny and load-bearing for client-side argument
///   construction).
///
/// Note: per-property `description` stripping is also performed by
/// [`strip_docs_from_tools`]; running both is idempotent. This
/// function is kept as a stable entry point so call sites that
/// historically invoked it (and the budget model in
/// [`crate::sizes`]) keep their semantics aligned with the wire.
///
/// **Why this changed (#859).** Pre-#859 the function dropped entire
/// optional property keys (everything not in `required` + the small
/// allow-list `[namespace, format]`), which produced
/// `memory_kg_query.inputSchema.properties = {source_id}` on the
/// wire — agents could not see that `max_depth`, `valid_at`,
/// `allowed_agents`, `limit`, `include_invalidated` were valid
/// params at all. The fix restores discovery by keeping every
/// property entry on the wire and trimming only the prose.
///
/// Returns the count of property entries whose `description` was
/// stripped — useful for telemetry / acceptance assertions in tests.
/// (Pre-#859 this counted dropped property entries; same shape,
/// different denominator.)
pub(crate) fn trim_optional_params(defs: &mut Value) -> usize {
    let Some(tools) = defs.get_mut("tools").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut stripped = 0_usize;
    for tool in tools.iter_mut() {
        let Some(input_schema) = tool.get_mut("inputSchema") else {
            continue;
        };
        let Some(properties) = input_schema
            .get_mut("properties")
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        for (_param_name, prop_value) in properties.iter_mut() {
            // Count `description` removals before the recursive
            // walker erases them, for telemetry.
            let had_desc = prop_value
                .as_object()
                .is_some_and(|o| o.contains_key("description"));
            strip_description_recursively(prop_value);
            if had_desc {
                stripped += 1;
            }
        }
    }
    stripped
}

/// v0.6.4-006 — Build the `families` overview included in the v2
/// `memory_capabilities` response. Each entry carries:
///
/// - `name` — family identifier (`core`, `graph`, …)
/// - `tool_count` — expected tool count per the family map
/// - `loaded` — whether the family is loaded under the active profile
/// - `tools` — the canonical tool-name list for that family
///
/// This is the v0.6.4 NHI runtime-discovery surface: an agent reading
/// the response sees which families are reachable AND can decide which
/// to opt into (via `memory_capabilities --include-schema family=<f>`)
/// without restarting the MCP server.
pub(crate) fn families_overview(profile: &crate::profile::Profile) -> Value {
    use crate::profile::Family;
    let defs = tool_definitions();
    let all_tools = defs
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let entries: Vec<Value> = Family::all()
        .iter()
        .map(|fam| {
            let tools_in_family: Vec<&str> = all_tools
                .iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str))
                .filter(|n| Family::for_tool(n) == Some(*fam))
                .collect();
            json!({
                "name": fam.name(),
                "tool_count": tools_in_family.len(),
                "loaded": profile.includes(*fam),
                "tools": tools_in_family,
            })
        })
        .collect();
    json!({
        "schema_version": "v0.6.4-families-1",
        "always_on": crate::profile::ALWAYS_ON_TOOLS,
        "families": entries,
    })
}

/// v0.6.4-006 — Handle `memory_capabilities` invocations that pass a
/// `family=<name>` parameter. When `include_schema=false` (default),
/// returns the canonical tool-name list. When `include_schema=true`,
/// returns the full MCP-style tool definitions for each tool — the
/// caller (an NHI agent or a host like Claude Code's deferred-tools
/// path) can register them at runtime without restarting the server.
///
/// v0.6.4-008 — when `include_schema=true` AND the daemon's
/// `[mcp.allowlist]` is configured, the requesting `agent_id` must be
/// permitted by the allowlist for the requested family. Permissive
/// (no-allowlist) default preserves Tier-1 single-process behavior —
/// operators opt into the gate by writing the table.
///
/// v0.7 C2 — `verbose` controls whether the per-tool `docs` field
/// (long-form description + examples) is preserved in the response.
/// When `verbose=false` (default), `docs` is stripped, matching the
/// always-on `tools/list` shape; when `verbose=true` AND
/// `include_schema=true`, callers receive the full documentation.
/// `verbose=true` without `include_schema=true` is a no-op (the
/// name-list response carries no `docs`).
///
/// v0.7 C4 — when `include_schema=true`, the returned tool schemas
/// are now trimmed by default (optional params hidden) to match the
/// `tools/list` shape. Pass `verbose=true` to opt into the full
/// schema — every optional param, every default, every per-property
/// description. The trim/keep allow-list lives in
/// [`C4_KEEP_OPTIONAL_PARAMS`]. C2's `docs`-field strip and C4's
/// `inputSchema.properties` trim are orthogonal and both governed by
/// the same `verbose` flag.
///
/// Errors:
/// - Unknown family → `Err` with diagnostic listing valid families.
/// - Empty family name → `Err`.
/// - Allowlist deny → `Err` with structured reason.
pub fn handle_capabilities_family(
    family_name: &str,
    include_schema: bool,
    verbose: bool,
    profile: &crate::profile::Profile,
    allowlist_cfg: Option<&crate::config::McpConfig>,
    agent_id: Option<&str>,
    audit_conn: Option<&rusqlite::Connection>,
) -> Result<Value, String> {
    use crate::profile::Family;
    if family_name.is_empty() {
        return Err("memory_capabilities: 'family' must not be empty".to_string());
    }
    let family = Family::all()
        .iter()
        .find(|f| f.name() == family_name)
        .copied()
        .ok_or_else(|| {
            let valid: Vec<&str> = Family::all().iter().map(|f| f.name()).collect();
            format!(
                "unknown family '{family_name}'. Valid families: {}.",
                valid.join(", ")
            )
        })?;

    // v0.6.4-008 — allowlist gate, only on the runtime-expansion path.
    if include_schema && let Some(mcp_cfg) = allowlist_cfg {
        use crate::config::AllowlistDecision;
        match mcp_cfg.allowlist_decision(agent_id, family.name()) {
            AllowlistDecision::Disabled | AllowlistDecision::Allow => {}
            AllowlistDecision::Deny => {
                // v0.6.4-009 — record the deny so operators can see
                // attempted-but-blocked expansion patterns.
                if let Some(conn) = audit_conn {
                    crate::db::record_capability_expansion(
                        conn,
                        agent_id,
                        family.name(),
                        false,
                        None,
                    );
                }
                return Err(format!(
                    "agent '{}' is not permitted to expand family '{}' under \
                     [mcp.allowlist]. Ask an operator to add a matching rule \
                     to config.toml or pass an allowed agent_id.",
                    agent_id.unwrap_or("<anonymous>"),
                    family.name()
                ));
            }
        }
    }

    // v0.6.4-009 — record the grant on the include_schema=true path.
    // Lightweight name-list calls are not audited (they're informational
    // only — no schema material released).
    if include_schema && let Some(conn) = audit_conn {
        crate::db::record_capability_expansion(conn, agent_id, family.name(), true, None);
    }

    let mut defs = tool_definitions();
    // v0.7 C4 — apply the optional-param trim BEFORE filtering by
    // family when the caller did not opt into verbose. Trimming is a
    // cheap pass over every tool's `inputSchema.properties` map, so
    // running it pre-filter is fine and keeps the call site simple.
    if !verbose {
        trim_optional_params(&mut defs);
    }
    let all_tools = defs
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut in_family: Vec<Value> = all_tools
        .into_iter()
        .filter(|t| {
            t.get("name")
                .and_then(Value::as_str)
                .and_then(Family::for_tool)
                == Some(family)
        })
        .collect();

    // v0.7 C2 — strip the verbose `docs` field unless the caller
    // explicitly opted into the long-form payload via `verbose=true`.
    // This keeps the family drilldown response consistent with the
    // bare `tools/list` shape by default.
    if !verbose {
        strip_docs_from_tools(&mut in_family);
    }

    if include_schema {
        Ok(json!({
            "schema_version": "v0.6.4-family-schemas-1",
            "family": family.name(),
            "loaded_under_active_profile": profile.includes(family),
            "verbose": verbose,
            "tools": in_family,
        }))
    } else {
        let names: Vec<&str> = in_family
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        Ok(json!({
            "schema_version": "v0.6.4-family-list-1",
            "family": family.name(),
            "loaded_under_active_profile": profile.includes(family),
            "tools": names,
        }))
    }
}

/// v0.6.4-002 — Filter `tool_definitions()` down to the tools loaded
/// under `profile`. Tools whose family is not in the profile's family
/// list are dropped from `tools[]`. `memory_capabilities` and any
/// other [`crate::profile::ALWAYS_ON_TOOLS`] are kept regardless of
/// profile so the runtime-discovery dance still works on
/// `--profile core`.
///
/// v0.7 C2 — the verbose `docs` field (long-form description + examples)
/// is stripped from each entry so the always-on `tools/list` payload
/// stays inside the C5 token budget. Callers that want the full docs
/// invoke `memory_capabilities { family=<f>, verbose: true }`, which
/// uses `tool_definitions()` directly without stripping.
///
/// v0.7 C4 — on top of the C2 docs strip, optional
/// `inputSchema.properties` are also stripped from each tool by
/// default (see [`trim_optional_params`]) so the `tools/list` payload
/// fits the v0.7 token budget. Callers that need the full schema
/// (every optional, every default) should call
/// [`tool_definitions_for_profile_verbose`] or, on the wire, pass
/// `verbose=true` to `memory_capabilities`. The C2 (description/docs)
/// trim and the C4 (optional-params) trim are orthogonal — both run
/// on the default path; both are skipped on the verbose path.
pub fn tool_definitions_for_profile(profile: &crate::profile::Profile) -> Value {
    let mut defs = tool_definitions_for_profile_verbose(profile);
    // Round-4 — honor `AI_MEMORY_TOOLS_VERBOSE=1` (or `=true`) as a
    // process-level opt-out from the C4 optional-params trim. Without
    // this escape hatch the trim was unconditional on `tools/list`
    // (the MCP method, not the `memory_capabilities` tool), so
    // operators who launched the daemon expecting the full schema —
    // e.g. for IDE autocomplete or plugin generators — got the
    // 10 766-byte trimmed payload regardless of CLI / env / profile
    // hints. The env var matches the existing convention used by
    // other AI_MEMORY_* tunables (`AI_MEMORY_NO_CONFIG`, `AI_MEMORY_DB`).
    if !tools_verbose_env_enabled() {
        trim_optional_params(&mut defs);
        // #859 — additionally compact the top-level tool description
        // on the wire form so the post-#859 payload (which now retains
        // every property metadata entry for client-side discovery)
        // still fits the C5 token budget. The full `description` is
        // reachable via `memory_capabilities { family=<f>,
        // include_schema=true, verbose=true }` (and via
        // [`tool_definitions_for_profile_verbose`] in-process). The
        // wire form keeps the `name` (the discovery key) and the full
        // `inputSchema` (the call surface); a one-sentence description
        // is preserved as the first 28 characters of the original short
        // description so display surfaces still have a label.
        wire_compact_descriptions(&mut defs);
    }
    defs
}

/// #859 helper — wire-form description compaction. After
/// [`trim_optional_params`] preserves every property entry on the
/// wire (so MCP clients can DISCOVER what knobs exist), the wire
/// payload still has to fit the C5 token budget. Two strategies are
/// applied, in order:
///
/// 1. **Truncate** the top-level tool `description` to the first
///    sentence (anything before `.` / `;` / first 28 characters,
///    whichever is shorter). The verbose drilldown
///    (`memory_capabilities { verbose=true }`) still carries the
///    full short-form description; the wire form is now even
///    shorter so the budget gate at 3500 cl100k tokens holds.
/// 2. **Strip** numeric / boolean schema defaults that match the
///    JSON-Schema validation no-op (e.g. `"default": 0` on an
///    `integer` with `minimum: 0`). Currently no-op; left as a
///    future-proofing seam so a future tightening doesn't require
///    a fresh trimmer entry point.
fn wire_compact_descriptions(defs: &mut Value) {
    let Some(tools) = defs.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };
    for tool in tools.iter_mut() {
        let Some(obj) = tool.as_object_mut() else {
            continue;
        };
        let Some(desc) = obj.get("description").and_then(Value::as_str) else {
            continue;
        };
        let compact = compact_description(desc);
        if compact.len() != desc.len() {
            obj.insert("description".to_string(), Value::String(compact));
        }
    }
}

/// Truncate a tool's short-form description to the first sentence
/// (or the first 32 characters at a word boundary), preserving at
/// least the verb-noun gist so display surfaces have a label.
///
/// Strategy:
/// 1. If the full description is ≤ 32 chars, keep it verbatim (cheap
///    enough to ship intact).
/// 2. If there's a sentence terminator (`.` / `;`) at or before the
///    32-char mark, cut just before it — that's the cleanest break.
/// 3. Otherwise cut at the last whitespace before 32 chars so we
///    never split a word in half. If no whitespace exists in the
///    first 32 chars, fall back to a char-boundary-safe truncation.
fn compact_description(s: &str) -> String {
    const MAX: usize = 32;
    if s.len() <= MAX {
        return s.to_string();
    }
    // Sentence-terminator path — preserves natural prose boundary.
    let slice = &s[..MAX.min(s.len())];
    if let Some(idx) = slice.find(['.', ';']) {
        return s[..idx].to_string();
    }
    // Word-boundary path — never split a word.
    if let Some(idx) = slice.rfind(char::is_whitespace) {
        return s[..idx].to_string();
    }
    // No whitespace in budget — char-boundary-safe truncation.
    let mut end = MAX.min(s.len());
    while !s.is_char_boundary(end) && end > 0 {
        end -= 1;
    }
    s[..end].to_string()
}

/// Round-4 — process-level escape hatch from the C4 trim used by
/// [`tool_definitions_for_profile`]. Reads `AI_MEMORY_TOOLS_VERBOSE`
/// once and accepts `1` or `true` (case-insensitive) as the truthy
/// values; anything else (including absent) is false. Cached behind a
/// `OnceLock` so the hot tools/list path doesn't re-stat the env on
/// every call.
fn tools_verbose_env_enabled() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("AI_MEMORY_TOOLS_VERBOSE")
            .ok()
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    })
}

/// v0.7 C4 — full-schema (verbose) variant of
/// [`tool_definitions_for_profile`]. Returns every optional param,
/// every default, every per-property description. Used by the
/// `memory_capabilities { verbose=true }` opt-in path so power users /
/// NHI agents can still set the long-tail knobs (`confidence`,
/// `priority`, `tier`, `metadata`, `agent_id`, …) without restarting
/// the MCP server with a different profile.
///
/// v0.7 C2 — note that `docs` (long-form prose) is still stripped on
/// the verbose path; the verbose flag controls whether
/// `inputSchema.properties` is trimmed (C4), not the top-level `docs`
/// field (C2). To recover the long-form docs, call
/// [`tool_definitions`] directly.
pub fn tool_definitions_for_profile_verbose(profile: &crate::profile::Profile) -> Value {
    let mut defs = tool_definitions();
    if let Some(arr) = defs.get_mut("tools").and_then(|t| t.as_array_mut()) {
        arr.retain(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| profile.loads(name))
        });
        strip_docs_from_tools(arr);
    }
    defs
}

/// v0.7 C2 — strip every long-form natural-language string from a
/// `tools[]` array so the bare `tools/list` payload stays inside the
/// C5 token budget (≤ 3500 cl100k tokens for 50 tools).
///
/// Removed:
/// - The top-level `docs` field (the long-form prose mirror of
///   `description`).
/// - Every `description` string nested under
///   `inputSchema.properties.*` — agents that need parameter prose
///   should re-fetch with `memory_capabilities { family=<f>,
///   include_schema: true, verbose: true }`, which calls
///   [`tool_definitions`] directly without stripping.
///
/// Preserved on the bare path:
/// - The top-level short `description` (≤ 50 cl100k tokens).
/// - The full `inputSchema` shape (`type`, `enum`, `default`,
///   `minimum`, `maximum`, `required`, `items`) so callers can still
///   construct valid argument objects without a verbose drilldown.
pub(crate) fn strip_docs_from_tools(tools: &mut Vec<Value>) {
    for tool in tools.iter_mut() {
        let Some(obj) = tool.as_object_mut() else {
            continue;
        };
        obj.remove("docs");
        if let Some(input_schema) = obj.get_mut("inputSchema").and_then(Value::as_object_mut)
            && let Some(props) = input_schema
                .get_mut("properties")
                .and_then(Value::as_object_mut)
        {
            for (_param_name, prop_value) in props.iter_mut() {
                strip_description_recursively(prop_value);
            }
        }
    }
}

/// #859 helper — walk a property value and drop every `description`
/// key encountered, including inside nested `properties` maps and
/// `oneOf` / `anyOf` / `allOf` branch arrays. Idempotent.
fn strip_description_recursively(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("description");
            // Drop long string defaults (>32 chars of prose) — short
            // numeric / boolean / enum defaults are load-bearing for
            // client-side argument construction so stay.
            if let Some(default) = map.get("default")
                && default.as_str().is_some_and(|s| s.len() > 32)
            {
                map.remove("default");
            }
            for (_, child) in map.iter_mut() {
                strip_description_recursively(child);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                strip_description_recursively(item);
            }
        }
        _ => {}
    }
}

/// v0.7 C2 — canonical tool catalog. Each tool entry carries a short
/// one-sentence `description` (≤ 50 cl100k_base tokens) and a
/// long-form `docs` field with the full prose + examples. The
/// always-on `tools/list` payload strips `docs` via
/// [`tool_definitions_for_profile`]; callers wanting the verbose form
/// invoke `memory_capabilities { family=<f>, verbose: true }` which
/// preserves `docs` so an NHI can drill in without reloading the
/// full-fat catalog into context.
#[allow(clippy::too_many_lines)]
pub fn tool_definitions() -> Value {
    json!({
        "toolsVersion": TOOLS_VERSION,
        "tools": [
            {
                "name": "memory_store",
                "description": "Store a memory; deduplicates by title+namespace.",
                "docs": "Store a memory. Dedupes by (title, namespace). Tier defaults to mid (7d TTL); long is permanent. on_conflict: error|merge|version. scope: Task 1.5 visibility. force (#519): bypass proactive contradiction detection on near-duplicate writes.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "Short title"},
                        "content": {"type": "string", "description": "Memory content"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"], "default": "mid"},
                        "namespace": {"type": "string", "description": "Namespace"},
                        "tags": {"type": "array", "items": {"type": "string"}, "default": []},
                        "priority": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0, "default": 1.0},
                        "source": {"type": "string", "enum": ["user", "claude", "hook", "api", "cli", "import", "consolidation", "system", "chaos"], "default": "claude"},
                        "metadata": {"type": "object", "description": "JSON metadata", "default": {}},
                        "agent_id": {"type": "string", "description": "NHI agent_id; synthesized if omitted."},
                        "scope": {"type": "string", "enum": ["private", "team", "unit", "org", "collective"], "description": "Task 1.5 visibility. Default private."},
                        "on_conflict": {"type": "string", "enum": ["error", "merge", "version"], "description": "P2/G6 (title,ns) collision: error=v2 default; merge=v1; version='(N)'."},
                        "kind": {"type": "string", "enum": ["observation", "reflection", "persona", "concept", "entity", "claim", "relation", "event", "conversation", "decision"], "description": "Form 6 (#759) memory-kind. Default observation."},
                        "force": {"type": "boolean", "default": false, "description": "#519 bypass proactive contradiction detection."},
                        "source_uri": {"type": "string", "description": "#885 Source URI (doc:/uri:/file:); indexed for #889."}
                    },
                    "required": ["title", "content"]
                }
            },
            {
                "name": "memory_recall",
                "description": "Recall memories relevant to a context (ranked).",
                "docs": "Fuzzy OR recall ranked by relevance + priority + access + tier. Optional: budget_tokens (cl100k cap), context_tokens (query-embed bias), session_id (+0.05 recency boost per #518), session_default (splice [agents.defaults.recall_scope]), include_archived, kinds filter. Default format toon_compact (~79% smaller).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "context": {"type": "string", "description": "What to recall"},
                        "namespace": {"type": "string", "description": "Namespace filter"},
                        "limit": {"type": "integer", "default": 10, "maximum": 50},
                        "tags": {"type": "string", "description": "Tag filter"},
                        "since": {"type": "string", "description": "RFC3339 lower bound on created_at"},
                        "until": {"type": "string", "description": "RFC3339 upper bound on created_at"},
                        "as_agent": {"type": "string", "description": "#151 scope-visibility agent."},
                        "budget_tokens": {"type": "integer", "minimum": 0, "description": "P6/R1 cl100k content cap. 0=empty; top kept (meta.budget_overflow=true)."},
                        "context_tokens": {"type": "array", "items": {"type": "string"}, "description": "Recent conversation tokens; biases query embedding 70/30 (v0.6.0.0)."},
                        "session_default": {"type": "boolean", "default": false, "description": "Splice [agents.defaults.recall_scope]. explicit > scope > defaults."},
                        "session_id": {"type": "string", "description": "#518 session id; +0.05 rerank boost for in-session ring (cap 50)."},
                        "include_archived": {"type": "boolean", "default": false, "description": "WT-1-E: include atomised sources alongside atoms."},
                        "has_citations": {"type": "boolean", "default": false, "description": "Form 4 (#757): require non-empty citations array."},
                        "source_uri_prefix": {"type": "string", "description": "Form 4 (#757): restrict by source_uri prefix (e.g. 'doc:', 'uri:https://')."},
                        "kinds": {
                            "oneOf": [
                                {"type": "array", "items": {"type": "string", "enum": ["observation", "reflection", "persona", "concept", "entity", "claim", "relation", "event", "conversation", "decision"]}},
                                {"type": "string"}
                            ],
                            "description": "Form 6 (#759) kind filter. Array/CSV. OR within; AND across."
                        },
                        "confidence_tier": {"type": "string", "enum": ["confirmed", "likely", "ambiguous"], "description": "Gap 4 (#887) tier filter."},
                        "verbose_provenance": {"type": "boolean", "default": true, "description": "Gap 7 (#890): per-row provenance decoration."},
                        "format": {"type": "string", "enum": ["json", "toon", "toon_compact"], "default": "toon_compact", "description": "Response format. toon_compact saves 79% vs json."}
                    },
                    "required": ["context"]
                }
            },
            {
                "name": "memory_recall_observations",
                "description": "List recall_observations (#886).",
                "docs": "Gap 3 (#886): recall-consumption ledger filter.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "recall_id": {"type": "string"},
                        "consumed": {"type": "boolean"},
                        "since": {"type": "string"},
                        "until": {"type": "string"},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 200}
                    }
                }
            },
            {
                "name": "memory_search",
                "description": "Search memories by exact keyword match (AND semantics).",
                "docs": "Exact keyword AND search. Deterministic; no fuzzy/semantic. Filters: namespace, tier, agent_id, as_agent (Task 1.5 scope). WT-1-E: atomised sources hidden by default.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "namespace": {"type": "string"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"]},
                        "limit": {"type": "integer", "default": 20, "maximum": 200},
                        "agent_id": {"type": "string", "description": "Exact metadata.agent_id filter."},
                        "as_agent": {"type": "string", "description": "#151 scope-visibility agent."},
                        "include_archived": {"type": "boolean", "default": false, "description": "WT-1-E: include atomised sources."},
                        "format": {"type": "string", "enum": ["json", "toon", "toon_compact"], "default": "toon_compact", "description": "Response format. toon_compact saves 79%."}
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "memory_list",
                "description": "List memories, optionally filtered by namespace or tier.",
                "docs": "Browse memories. Filters: namespace, tier, agent_id. Limit caps at 200.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"]},
                        "limit": {"type": "integer", "default": 20, "maximum": 200},
                        "agent_id": {"type": "string", "description": "Exact metadata.agent_id filter."},
                        "format": {"type": "string", "enum": ["json", "toon", "toon_compact"], "default": "toon_compact", "description": "Response format."}
                    }
                }
            },
            {
                "name": "memory_load_family",
                "description": "Load top-k recent + high-priority memories from a Family.",
                "docs": "B1: top-k by metadata.family. Always-on; alternative to memory_recall when family is known. Issue #864 — `family` here is the MCP tool family (8 groups: core/lifecycle/graph/governance/power/meta/archive/other), NOT the memory_kind taxonomy (Observation/Reflection/Plan/Decision/etc).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "family": {"type": "string", "enum": ["core", "lifecycle", "graph", "governance", "power", "meta", "archive", "other"], "description": "MCP tool family (8 groups) — NOT the memory_kind taxonomy. See #864."},
                        "namespace": {"type": "string", "description": "Namespace filter. Default all."},
                        "k": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20, "description": "Top-k cap 100."}
                    },
                    "required": ["family"]
                }
            },
            {
                "name": "memory_smart_load",
                "description": "Intent-routed loader: free-text intent picks the best Family.",
                "docs": "B2: pick best Family from free-text intent, then forward to memory_load_family. Issue #864 — `Family` here is the MCP tool family (8 groups: core/lifecycle/graph/governance/power/meta/archive/other), NOT the memory_kind taxonomy (Observation/Reflection/Plan/Decision/etc).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "intent": {"type": "string", "description": "Free-text goal."},
                        "namespace": {"type": "string", "description": "Namespace filter. Default all."},
                        "k": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20, "description": "Top-k cap 100."}
                    },
                    "required": ["intent"]
                }
            },
            {
                "name": "memory_get_taxonomy",
                "description": "Return a hierarchical tree of namespaces with memory counts.",
                "docs": "Pillar 1 / Stream A: namespace tree (live rows only). Each node has count + subtree_count. Response includes total_count and truncated flag.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace_prefix": {"type": "string", "description": "Restrict to this namespace + descendants. Trailing '/' tolerated."},
                        "depth": {"type": "integer", "minimum": 0, "maximum": 8, "default": 8, "description": "Max descent. Deeper rows roll up into boundary subtree_count."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 10000, "default": 1000, "description": "Row cap. Densest namespaces win on truncation."}
                    }
                }
            },
            {
                "name": "memory_check_duplicate",
                "description": "Pre-write near-duplicate check via cosine over stored embeddings.",
                "docs": "Pillar 2 / Stream D: pre-write near-dup check. Embeds title+content, returns highest-cosine match + is_duplicate + suggested_merge. Threshold floor 0.5. Requires semantic tier+.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "Candidate title."},
                        "content": {"type": "string", "description": "Candidate content."},
                        "namespace": {"type": "string", "description": "Namespace filter."},
                        "threshold": {"type": "number", "minimum": 0.5, "maximum": 1.0, "default": 0.85, "description": "Cosine threshold; floor 0.5. Default 0.85 tuned for MiniLM-L6-v2."}
                    },
                    "required": ["title", "content"]
                }
            },
            {
                "name": "memory_entity_register",
                "description": "Register an entity (canonical name + aliases) under a namespace.",
                "docs": "Pillar 2 / Stream B: register entity as long-tier memory (metadata.kind='entity'). Idempotent on (canonical_name, namespace); merges new aliases. Errors if name collides with a non-entity row.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "canonical_name": {"type": "string", "description": "Display name (entity memory title)."},
                        "namespace": {"type": "string", "description": "Entity namespace."},
                        "aliases": {"type": "array", "items": {"type": "string"}, "description": "Aliases; blanks skipped, deduped."},
                        "metadata": {"type": "object", "description": "Metadata; 'kind' is forced to 'entity'."},
                        "agent_id": {"type": "string", "description": "Override metadata.agent_id."}
                    },
                    "required": ["canonical_name", "namespace"]
                }
            },
            {
                "name": "memory_entity_get_by_alias",
                "description": "Resolve an alias to its registered entity.",
                "docs": "Pillar 2 / Stream B: resolve alias to entity. Without namespace, most-recently-created wins. Null when no match.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "alias": {"type": "string", "description": "Alias; whitespace trimmed."},
                        "namespace": {"type": "string", "description": "Namespace filter."}
                    },
                    "required": ["alias"]
                }
            },
            {
                "name": "memory_kg_timeline",
                "description": "Ordered fact timeline for an entity (outbound KG links by valid_from).",
                "docs": "Pillar 2 / Stream C: outbound links from source_id ordered valid_from ASC. Includes valid_from/valid_until/observed_by + target title/namespace. NULL valid_from rows excluded. Cross-namespace.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "Source memory ID (typically an entity_id)."},
                        "since": {"type": "string", "description": "RFC3339 inclusive lower bound on valid_from."},
                        "until": {"type": "string", "description": "RFC3339 inclusive upper bound on valid_from."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 200, "description": "Cap [1,1000]."}
                    },
                    "required": ["source_id"]
                }
            },
            {
                "name": "memory_kg_invalidate",
                "description": "Mark a KG link as superseded by setting its valid_until column.",
                "docs": "Pillar 2 / Stream C: set valid_until on (source_id, target_id, relation). valid_until defaults to now. Idempotent; response carries previous_valid_until. found:false when no match.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "Source memory ID."},
                        "target_id": {"type": "string", "description": "Target memory ID."},
                        "relation": {"type": "string", "description": "Relation label."},
                        "valid_until": {"type": "string", "description": "RFC3339 supersession instant. Default now."}
                    },
                    "required": ["source_id", "target_id", "relation"]
                }
            },
            {
                "name": "memory_kg_query",
                "description": "Outbound KG traversal from a source memory (<=5 hops).",
                "docs": "Pillar 2 / Stream C: BFS/CTE traversal with cycle detection. Each row carries valid_from/valid_until/observed_by + target title/namespace. Filters chain across every hop. max_depth ceiling 5.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "Source memory ID."},
                        "max_depth": {"type": "integer", "minimum": 1, "maximum": 5, "default": 1, "description": "Hops, 1..=5."},
                        "valid_at": {"type": "string", "description": "RFC3339; keep links valid at instant. Omit to skip temporal filter."},
                        "allowed_agents": {"type": "array", "items": {"type": "string"}, "description": "Observed-by allowlist. Empty array = zero rows."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 200, "description": "Cap across all depths [1,1000]."},
                        "include_invalidated": {"type": "boolean", "default": false, "description": "When true, traverse historically-invalidated edges."},
                        "by_source_uri": {"type": "string", "description": "#889 traverse by source_uri."},
                        "namespace": {"type": "string", "description": "Restrict to namespace."}
                    },
                    "required": ["source_id"]
                }
            },
            {
                "name": "memory_find_paths",
                "description": "Enumerate up to N paths through the KG between two memories (BFS, max_depth<=7).",
                "docs": "J7: undirected BFS over memory_links with cycle detection. Returns id chains source-first. max_depth<=7, max_results<=50.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "Path origin."},
                        "target_id": {"type": "string", "description": "Path destination."},
                        "max_depth": {"type": "integer", "minimum": 1, "maximum": 7, "default": 4, "description": "Max hops, default 4, ceiling 7."},
                        "max_results": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10, "description": "Max paths (shortest-first), default 10, ceiling 50."},
                        "include_invalidated": {"type": "boolean", "default": false, "description": "When true, include historically-invalidated edges."}
                    },
                    "required": ["source_id", "target_id"]
                }
            },
            {
                "name": "memory_delete",
                "description": "Delete a memory by ID.",
                "docs": "Hard-delete by id (removes row, embedding, FTS, links). Use memory_forget for bulk pattern delete (archives first).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_promote",
                "description": "Promote a memory to long (or chosen tier) / ancestor namespace.",
                "docs": "Default: bump to long (clears expiry); short->long and mid->long are single-call. #831: target_tier ('mid'|'long') stops on intermediate. Task 1.7: to_namespace clones to an ancestor + derived_from link.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "target_tier": {"type": "string", "enum": ["mid", "long"], "description": "#831: 'mid' keeps expires_at; 'long' clears it. Downgrades rejected."},
                        "to_namespace": {"type": "string", "description": "Task 1.7: clone target (must be a proper ancestor)."}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_forget",
                "description": "Bulk delete memories matching a pattern, namespace, or tier (archives first).",
                "docs": "Bulk delete by pattern/namespace/tier. Archives first (recover via memory_archive_restore). dry_run previews.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string"},
                        "pattern": {"type": "string"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"]},
                        "dry_run": {"type": "boolean", "default": false, "description": "Preview without deleting."}
                    }
                }
            },
            {
                "name": "memory_stats",
                "description": "Get memory store statistics (counts, tier breakdown, sizes).",
                "docs": "Totals, per-tier + namespace tallies, archive + DB size.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "memory_update",
                "description": "Update an existing memory by ID (only provided fields change).",
                "docs": "Partial update by id. Omitted fields preserved. Tier monotone-only. metadata.agent_id preserved.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Memory ID."},
                        "title": {"type": "string"},
                        "content": {"type": "string"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"]},
                        "namespace": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "priority": {"type": "integer", "minimum": 1, "maximum": 10},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                        "expires_at": {"type": "string", "description": "RFC3339 or null to clear."},
                        "metadata": {"type": "object", "description": "JSON metadata."},
                        "expected_version": {"type": "integer", "description": "#884 If-Match; mismatch → 409 envelope."},
                        "edit_source": {"type": "string", "enum": ["human", "llm", "hook"], "default": "human", "description": "#888 'human'=in-place; 'llm'/'hook'=archive+supersede."},
                        "source_uri": {"type": "string", "description": "#906 update source_uri."}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_get",
                "description": "Get a specific memory by ID, including its links.",
                "docs": "Memory row + linked ids (in+out). Use memory_get_links for full link rows with attestation.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Memory ID."}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_link",
                "description": "Create a typed link between two memories.",
                "docs": "Directional link. Relations: related_to | supersedes | contradicts | derived_from | reflects_on (Task 3/8). H-track signs with active Ed25519 (verify via memory_verify).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "Source memory ID."},
                        "target_id": {"type": "string", "description": "Target memory ID."},
                        "relation": {"type": "string", "enum": ["related_to", "supersedes", "contradicts", "derived_from", "reflects_on"], "default": "related_to"}
                    },
                    "required": ["source_id", "target_id"]
                }
            },
            {
                "name": "memory_get_links",
                "description": "Get all links for a memory (both directions).",
                "docs": "In + outbound links with relation, attest_level (unsigned/self_signed/peer_attested), valid_from/until/observed_by.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Memory ID."}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_verify",
                "description": "Re-verify a stored memory_links row's Ed25519 signature on demand.",
                "docs": "H4: re-verify link signature. Returns {signature_verified, attest_level, signed_by, signed_at}. Pass link_id composite ('source--relation-->target') or explicit triple.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "link_id": {"type": "string", "description": "Composite id 'source_id--relation-->target_id'."},
                        "source_id": {"type": "string", "description": "Required when link_id omitted."},
                        "target_id": {"type": "string", "description": "Required when link_id omitted."},
                        "relation": {"type": "string", "enum": ["related_to", "supersedes", "contradicts", "derived_from", "reflects_on"], "default": "related_to", "description": "Default related_to."}
                    }
                }
            },
            {
                "name": "memory_replay",
                "description": "Reconstruct the conversation transcript chain that produced a memory.",
                "docs": "I4: transcript chain (text + span metadata). verbose=false (default) truncates >100KB entries. L2-4 (#669): for reflections, walks reflects_on edges for transcript UNION; cap via depth (null=full, 0=self only).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_id": {"type": "string", "description": "Memory ID."},
                        "verbose": {"type": "boolean", "default": false, "description": "I4: when false, >100KB transcripts truncated=true."},
                        "depth": {"type": ["integer", "null"], "minimum": 0, "default": null, "description": "L2-4 reflects_on hops. null=full, 0=self, N=self+N."},
                        "agent_id": {"type": "string", "description": "#912 perm gate."}
                    },
                    "required": ["memory_id"]
                }
            },
            {
                "name": "memory_reflect",
                "description": "Persist a reflection memory plus reflects_on provenance links to each source.",
                "docs": "Task 4/8 (#655): substrate-native recursive-learning primitive. reflection_depth = max(source_depths)+1; gated by namespace governance.max_reflection_depth (Task 2/8) — refusal returns REFLECTION_DEPTH_EXCEEDED. New memory + N reflects_on links land in one atomic txn.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_ids": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "Sources reflected on; one reflects_on link per id."},
                        "title": {"type": "string", "description": "Reflection title."},
                        "content": {"type": "string", "description": "Reflection content."},
                        "namespace": {"type": "string", "description": "Target namespace. Defaults to first source's namespace."},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"], "default": "mid"},
                        "tags": {"type": "array", "items": {"type": "string"}, "default": []},
                        "priority": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0, "default": 1.0},
                        "agent_id": {"type": "string", "description": "Reflection writer NHI; default synthesized."},
                        "metadata": {"type": "object", "description": "Merged with system reflection_metadata; caller keys win."}
                    },
                    "required": ["source_ids", "title", "content"]
                }
            },
            {
                "name": "memory_export_reflection",
                "description": "Render a single reflection memory as markdown or JSON (no filesystem write).",
                "docs": "QW-1: render reflection + reflects_on provenance as YAML-frontmatter md (default) or JSON envelope. Returns {content, suggested_filename}. No FS write — harness owns disk I/O.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_id": {"type": "string", "description": "Reflection-kind memory id."},
                        "format": {"type": "string", "enum": ["md", "json"], "default": "md", "description": "md or json."}
                    },
                    "required": ["memory_id"]
                }
            },
            {
                "name": "memory_persona",
                "description": "Fetch the latest Persona artefact for an entity (read-only).",
                "docs": "QW-2: latest MemoryKind::Persona for (entity_id, namespace). Returns envelope {id, entity_id, namespace, body_md, sources, generated_at, version, attest_level}. null when none. Pair with memory_persona_generate.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "entity_id": {"type": "string", "description": "Persona subject."},
                        "namespace": {"type": "string", "description": "Default 'global'."}
                    },
                    "required": ["entity_id"]
                }
            },
            {
                "name": "memory_persona_generate",
                "description": "Generate/regen a Persona artefact for an entity.",
                "docs": "QW-2 / #848: synthesise MemoryKind::Persona from top-K Reflection memories. Omit namespace (or pass null) for cross-namespace aggregation (#848 — persona lands in 'global'); pass a namespace string for single-namespace scope. Response includes namespace_scope=single|cross_namespace.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "entity_id": {"type": "string", "description": "Persona subject (1-128 chars)."},
                        "namespace": {"type": ["string", "null"], "description": "Omit/null → cross-namespace (#848); string → single-namespace."}
                    },
                    "required": ["entity_id"]
                }
            },
            {
                "name": "memory_reflection_origin",
                "description": "Inspect the cross-peer provenance of a reflection memory.",
                "docs": "L2-2 (S6-M1): {memory_id, peer_origin, signing_agent, original_depth, local_depth_at_arrival, is_reflection}. Non-reflections return envelope with is_reflection=false. Unknown ids => error.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_id": {"type": "string", "description": "Memory ID."}
                    },
                    "required": ["memory_id"]
                }
            },
            {
                "name": "memory_dependents_of_invalidated",
                "description": "List dependents flagged by the L2-3 invalidation walker.",
                "docs": "L2-3 (#668): read-only list of memories with reflects_on->memory_id. Notification, NOT cascade — dependents are flagged for curator review. Returns {memory_id, count, dependents:[{id, namespace}]}. Unknown ids => empty.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_id": {"type": "string", "description": "Invalidated reflection id."}
                    },
                    "required": ["memory_id"]
                }
            },
            {
                "name": "memory_consolidate",
                "description": "Consolidate multiple memories into one long-term summary.",
                "docs": "Merge 2-100 sources into one long-tier memory; deletes sources, adds derived_from links. LLM auto-generates summary if omitted (smart/autonomous tier).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "ids": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 100, "description": "Source ids (2-100)."},
                        "title": {"type": "string", "description": "Consolidated title."},
                        "summary": {"type": "string", "description": "Optional summary; LLM auto-generates at smart/autonomous tier."},
                        "namespace": {"type": "string", "default": "global"},
                        "agent_id": {"type": "string", "description": "#908 consolidator agent_id."}
                    },
                    "required": ["ids", "title"]
                }
            },
            {
                "name": "memory_ingest_multistep",
                "description": "Form 3 multi-step ingest: deterministic helpers + LLM stages.",
                "docs": "Form 3 (#756): two_phase (FTS + Jaccard -> synthesise) or four_step (load_context -> classify -> enrich -> emit). Helpers run first; LLM stages receive helper output under explicit-trust banner + SHARED PREFIX for cache-key reuse. Response carries trace + cache-key set + final output. Smart+ tier only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "description": "Content to ingest."},
                        "namespace": {"type": "string", "description": "FTS classifier hint. Default 'global'."},
                        "pipeline_variant": {"type": "string", "enum": ["two_phase", "four_step"], "default": "two_phase", "description": "Named pipeline; ignored if pipeline_override set."},
                        "pipeline_override": {"type": "object", "description": "Custom Pipeline descriptor."}
                    },
                    "required": ["content"]
                }
            },
            {
                "name": "memory_atomise",
                "description": "Decompose a memory into 2-10 atomic propositions; source archived. Smart+ tier.",
                "docs": "WT-1-C: atomise via WT-1-B engine. Atoms = Observation memories with metadata.atom_source_id + derives_from link. Source archived (atomised_into=N). Returns {source_id, atom_ids, atom_count, archived_at}. Idempotent (use force_re_atomise to mint fresh). Too-small sources => {source_too_small:true}. Failures => CURATOR_FAILED / GOVERNANCE_REFUSED envelopes.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_id": {"type": "string", "description": "Source memory UUID."},
                        "max_atom_tokens": {"type": "integer", "minimum": 50, "maximum": 1000, "default": 200, "description": "Per-atom cl100k budget."},
                        "force_re_atomise": {"type": "boolean", "default": false, "description": "Skip idempotency; mint fresh atoms (old retained)."}
                    },
                    "required": ["memory_id"]
                }
            },
            {
                "name": "memory_share",
                "description": "Share a memory with another agent (copy into _shared/<from>→<to>/).",
                "docs": "#224/#311 MVP: point-to-point copy into `_shared/<from>→<to>/` with provenance.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_memory_id": {"type": "string", "description": "Memory id (full UUID or unique prefix) to share."},
                        "target_agent_id": {"type": "string", "description": "Recipient agent id; must satisfy validate_agent_id."}
                    },
                    "required": ["source_memory_id", "target_agent_id"]
                }
            },
            {
                "name": "memory_calibrate_confidence",
                "description": "Scan confidence_shadow_observations and emit per-source baselines (Form 5).",
                "docs": "Form 5 (#758): read-only calibration sweep over shadow-mode observations (AI_MEMORY_CONFIDENCE_SHADOW=1). Returns CalibrationReport {window_days, total_observations, baselines:[{namespace, source, count, median, mean, buckets}]}. Default window 30d. Family::Power — refuses on keyword tier.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "days": {"type": "integer", "minimum": 1, "maximum": 3650, "default": 30, "description": "Window days."},
                        "output_format": {"type": "string", "enum": ["json", "table"], "default": "json", "description": "json envelope or ASCII table."}
                    }
                }
            },
            {
                "name": "memory_capabilities",
                "description": "Discover runtime capabilities; family=<name> drills in.",
                "docs": "Caps-v3: tier, profile, summary, callable_now, agent_permitted_families, harness detection. family+include_schema drills one family. verbose=true restores full schema. NOTE per #864: `family` here = MCP tool-family (8 groups: core/lifecycle/graph/governance/power/meta/archive/other), NOT memory_kind taxonomy.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "accept": {
                            "type": "string",
                            "enum": ["v1", "v2"],
                            "default": "v2",
                            "description": "Schema version. v2 default; v1 legacy."
                        },
                        "family": {
                            "type": "string",
                            "enum": ["core", "lifecycle", "graph", "governance", "power", "meta", "archive", "other"],
                            "description": "Drill into one family."
                        },
                        "include_schema": {
                            "type": "boolean",
                            "default": false,
                            "description": "Return full tool schemas. Requires family."
                        },
                        "verbose": {
                            "type": "boolean",
                            "default": false,
                            "description": "C2/C4: preserve docs + every optional inputSchema property."
                        }
                    }
                }
            },
            {
                "name": "memory_expand_query",
                "description": "LLM-expand a search query into related terms (smart/autonomous tier).",
                "docs": "LLM query expansion. Smart/autonomous tier.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Query to expand."}
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "memory_auto_tag",
                "description": "LLM-generate tags for a memory (smart/autonomous tier).",
                "docs": "LLM auto-tagging. Smart/autonomous tier.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Memory ID."}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_detect_contradiction",
                "description": "LLM-check whether two memories contradict each other (smart/autonomous tier).",
                "docs": "LLM contradiction check. Smart/autonomous tier.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id_a": {"type": "string", "description": "First memory ID."},
                        "id_b": {"type": "string", "description": "Second memory ID."}
                    },
                    "required": ["id_a", "id_b"]
                }
            },
            {
                "name": "memory_archive_list",
                "description": "List archived (expired) memories.",
                "docs": "List archived memories. Filter by namespace; paginate via offset/limit.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Namespace filter."},
                        "limit": {"type": "integer", "description": "Default 50, max 1000."},
                        "offset": {"type": "integer", "description": "Pagination offset."}
                    }
                }
            },
            {
                "name": "memory_archive_restore",
                "description": "Restore an archived memory back to the active store.",
                "docs": "Restore archived row; expires_at cleared.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Archived memory id."}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_archive_purge",
                "description": "Permanently delete archived memories.",
                "docs": "Purge archive. Scope via older_than_days. Unrecoverable.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "older_than_days": {"type": "integer", "description": "Only purge entries older than N days."}
                    }
                }
            },
            {
                "name": "memory_archive_stats",
                "description": "Show archive statistics (total count and per-namespace breakdown).",
                "docs": "Archive total + per-namespace counts.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "memory_gc",
                "description": "Trigger garbage collection on expired memories (archives first).",
                "docs": "GC expired memories. Archives first when archive_on_gc is on (default). dry_run previews.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "dry_run": {"type": "boolean", "default": false, "description": "Preview without deleting."}
                    }
                }
            },
            {
                "name": "memory_session_start",
                "description": "Auto-recall recent memories on session start.",
                "docs": "Most-recently-accessed/updated. At smart/autonomous tier, includes LLM summary.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Namespace filter."},
                        "limit": {"type": "integer", "default": 10, "maximum": 50},
                        "format": {"type": "string", "enum": ["json", "toon", "toon_compact"], "default": "toon_compact"}
                    }
                }
            },
            {
                "name": "memory_namespace_set_standard",
                "description": "Set a memory as the standard/policy for a namespace.",
                "docs": "Standard memory auto-prepended to recall + session_start. Rule layering: global '*' + parent chain + namespace. Task 1.8: governance policy merged into metadata. P4/G1: inherit flag.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Namespace."},
                        "id": {"type": "string", "description": "Standard memory id."},
                        "parent": {"type": "string", "description": "Inherit-from namespace."},
                        "governance": {
                            "type": "object",
                            "description": "Task 1.8 policy in metadata.governance.",
                            "properties": {
                                "write":    {"type": "string", "enum": ["any", "registered", "owner", "approve"]},
                                "promote":  {"type": "string", "enum": ["any", "registered", "owner", "approve"]},
                                "delete":   {"type": "string", "enum": ["any", "registered", "owner", "approve"]},
                                "approver": {"description": "ApproverType: \"human\" | {\"agent\": \"<id>\"} | {\"consensus\": <n>}"},
                                "inherit":  {"type": "boolean", "default": true, "description": "P4/G1: parent-chain inheritance. Default on."}
                            }
                        }
                    },
                    "required": ["namespace", "id"]
                }
            },
            {
                "name": "memory_namespace_get_standard",
                "description": "Get the standard/policy memory for a namespace.",
                "docs": "Returns the standard. inherit=true (Task 1.6) returns the resolved chain (global '*' -> ancestors -> namespace).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Namespace."},
                        "inherit": {"type": "boolean", "default": false, "description": "Task 1.6: return full inheritance chain."}
                    },
                    "required": ["namespace"]
                }
            },
            {
                "name": "memory_namespace_clear_standard",
                "description": "Clear the standard/policy for a namespace.",
                "docs": "Clear the namespace standard.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Namespace."}
                    },
                    "required": ["namespace"]
                }
            },
            {
                "name": "memory_pending_list",
                "description": "List pending governance-queued actions.",
                "docs": "Task 1.9: list governance-queued actions. status filter (default pending). Limit cap 1000.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "status": {"type": "string", "enum": ["pending", "approved", "rejected"]},
                        "limit":  {"type": "integer", "default": 100, "maximum": 1000}
                    }
                }
            },
            {
                "name": "memory_pending_approve",
                "description": "Approve a pending action; `remember` auto-decides next time.",
                "docs": "Task 1.9 approve. decided_by = caller. K10: remember (once|session|forever) writes a synthetic permit rule.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Pending action id."},
                        "remember": {
                            "type": "string",
                            "enum": ["once", "session", "forever"],
                            "default": "once",
                            "description": "K10 persistence horizon."
                        }
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_pending_reject",
                "description": "Reject a pending action; `remember` auto-decides next time.",
                "docs": "Task 1.9 reject. decided_by = caller. K10: remember writes a synthetic deny rule.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Pending action id."},
                        "remember": {
                            "type": "string",
                            "enum": ["once", "session", "forever"],
                            "default": "once",
                            "description": "K10 persistence horizon."
                        }
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_agent_register",
                "description": "Register an agent in the reserved _agents namespace.",
                "docs": "Register agent (agent_type, capabilities) in _agents. Refreshes last_seen_at; preserves registered_at. agent_id is CLAIMED, not attested — pair with attestation for security boundary.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string", "description": "Agent id (same validation as metadata.agent_id)."},
                        // Round-2 F16 — agent_type is OPEN-form at the
                        // schema layer. validate::validate_agent_type
                        // accepts curated short-list + any ai:<name>
                        // (alnum/_-.) up to 64 chars.
                        "agent_type": {
                            "type": "string",
                            "description": "Curated: human, system, ai:<model>. Open-form: any ai:<name>."
                        },
                        "capabilities": {"type": "array", "items": {"type": "string"}, "default": [], "description": "Capability tags."}
                    },
                    "required": ["agent_id", "agent_type"]
                }
            },
            {
                "name": "memory_agent_list",
                "description": "List every registered agent.",
                "docs": "List agents (ordered by registered_at).",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "memory_notify",
                "description": "Send a message from the caller to another agent's inbox.",
                "docs": "Send message to _messages/<target>. Sender = caller agent_id. Read via memory_inbox.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "target_agent_id": {"type": "string", "description": "Recipient agent_id."},
                        "title": {"type": "string", "description": "Subject (<=200 chars)."},
                        "payload": {"type": "string", "description": "Body."},
                        "priority": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"], "default": "mid", "description": "short=6h, mid=7d, long=no expiry."}
                    },
                    "required": ["target_agent_id", "title", "payload"]
                }
            },
            {
                "name": "memory_inbox",
                "description": "List messages sent to an agent via memory_notify.",
                "docs": "Read _messages/<agent_id>. access_count==0 is the unread marker.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string", "description": "Recipient; default caller."},
                        "unread_only": {"type": "boolean", "default": false, "description": "access_count==0 only."},
                        "limit": {"type": "integer", "default": 50, "maximum": 500}
                    }
                }
            },
            {
                "name": "memory_subscribe",
                "description": "Register a webhook subscription for memory events.",
                "docs": "Webhook subscription. HMAC-SHA256 signed via X-Ai-Memory-Signature when secret supplied. https required (http only for loopback). Secret stored hashed only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "https URL (http only for loopback). SSRF guard rejects private IPs."},
                        "events": {"type": "string", "default": "*", "description": "Comma-list or *. Events: memory_store, memory_delete, memory_promote."},
                        "secret": {"type": "string", "description": "HMAC secret. Omit for unsigned."},
                        "namespace_filter": {"type": "string", "description": "Exact namespace match."},
                        "agent_filter": {"type": "string", "description": "agent_id filter."},
                        "event_types": {"type": "array", "items": {"type": "string"}, "description": "#912 event-type subset."}
                    },
                    "required": ["url"]
                }
            },
            {
                "name": "memory_unsubscribe",
                "description": "Delete a subscription by id.",
                "docs": "Delete subscription. DLQ rows retained for audit.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_list_subscriptions",
                "description": "List active webhook subscriptions.",
                "docs": "List subscriptions. Secrets never returned.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "memory_subscription_replay",
                "description": "Replay subscription_events since an RFC3339 timestamp.",
                "docs": "K7: replay events ordered by delivered_at asc.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "subscription_id": {"type": "string", "description": "Subscription id."},
                        "since": {"type": "string", "description": "RFC3339 inclusive lower bound."}
                    },
                    "required": ["subscription_id", "since"]
                }
            },
            {
                "name": "memory_subscription_dlq_list",
                "description": "List subscription_dlq rows (exhausted retry ladder).",
                "docs": "K7: DLQ inspector.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "subscription_id": {"type": "string", "description": "Restrict to one subscription."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 100}
                    }
                }
            },
            {
                "name": "memory_quota_status",
                "description": "Report per-agent quota usage. Operator-facing.",
                "docs": "K8: quota usage (memories/day, storage, links/day). Omit agent_id for all.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string", "description": "Restrict to one agent."}
                    }
                }
            },
            {
                "name": "memory_check_agent_action",
                "description": "Check action vs governance_rules (#691); Allow/Refuse/Warn.",
                "docs": "#691: read-only rule check. Harness PreToolUse hook calls on every Bash/Write/Edit. Rule MUTATION over MCP is disabled — use `ai-memory rules --sign` CLI or signed HTTP admin endpoints.",
                "inputSchema": {
                    "type": "object",
                    "required": ["kind"],
                    "properties": {
                        "kind": {"type": "string", "enum": ["bash", "filesystem_write", "network_request", "process_spawn", "custom"]},
                        "command": {"type": "string", "description": "kind=bash."},
                        "cwd": {"type": "string", "description": "kind=bash cwd."},
                        "path": {"type": "string", "description": "kind=filesystem_write."},
                        "byte_estimate": {"type": "integer", "description": "Bytes-to-write hint."},
                        "host": {"type": "string", "description": "kind=network_request."},
                        "scheme": {"type": "string", "description": "Default https."},
                        "binary": {"type": "string", "description": "kind=process_spawn."},
                        "args": {"type": "array", "items": {"type": "string"}, "description": "process_spawn argv."},
                        "custom_kind": {"type": "string", "description": "kind=custom."},
                        "agent_id": {"type": "string", "description": "Caller id (audit)."}
                    }
                }
            },
            {
                "name": "memory_rule_list",
                "description": "List substrate-level agent-action rules. Read-only (#691).",
                "docs": "#691: governance_rules read. Mutation operator-only (CLI/HTTP signed); MCP read-only by design 2026-05-13.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "description": "Restrict to one AgentAction kind."},
                        "enabled_only": {"type": "boolean", "description": "Skip disabled rules. Default false."}
                    }
                }
            },
            {
                "name": "memory_skill_register",
                "description": "Register an agentskills.io SKILL.md from a folder or inline text.",
                "docs": "L1-5: Ed25519-attested skill registration with version chaining. Re-register same (name, namespace) supersedes prior row.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "folder_path": {"type": "string", "description": "Dir containing SKILL.md + optional resources/."},
                        "inline_skill": {"type": "string", "description": "Raw SKILL.md text (frontmatter + body)."}
                    }
                }
            },
            {
                "name": "memory_skill_list",
                "description": "List current (non-superseded) skills; body not returned.",
                "docs": "L1-5: discovery (name, description, id, namespace, digest, metadata). Use memory_skill_get for body.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Namespace filter. Omit or '%' = all."},
                        "filter": {"type": "string", "description": "Text filter on name + description."}
                    }
                }
            },
            {
                "name": "memory_skill_get",
                "description": "Get full skill activation payload (metadata + body).",
                "docs": "L1-5: metadata + decompressed body (<5000 tok). Old version ids stay addressable.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill_id": {"type": "string", "description": "Skill UUID."}
                    },
                    "required": ["skill_id"]
                }
            },
            {
                "name": "memory_skill_resource",
                "description": "Fetch + digest-verify a skill resource.",
                "docs": "L1-5: SHA-256-verified resource fetch. Errors on mismatch.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill_id": {"type": "string", "description": "Parent skill UUID."},
                        "resource_path": {"type": "string", "description": "Relative path (e.g. 'scripts/run.sh')."}
                    },
                    "required": ["skill_id", "resource_path"]
                }
            },
            {
                "name": "memory_skill_export",
                "description": "Export a skill to a folder; re-register produces identical digest.",
                "docs": "L1-5: write SKILL.md + resources/ to target_folder. Round-trip identical SHA-256. Emits skill.exported signed_events row.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill_id": {"type": "string", "description": "Skill UUID."},
                        "target_folder": {"type": "string", "description": "Destination dir (created if absent)."}
                    },
                    "required": ["skill_id", "target_folder"]
                }
            },
            {
                "name": "memory_skill_promote_from_reflection",
                "description": "Promote a Reflection into a reusable Agent Skill.",
                "docs": "L2-6 (#671): reflection (depth>=namespace.governance.skill_promotion_min_depth, default 1) -> SKILL.md. Each reflects_on source -> references/source_{i}.md. Frontmatter preserves derived_from_reflection_id + original_reflection_depth. Promote->export->register => identical SHA-256. Refuses depth-0.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "reflection_id": {"type": "string", "description": "Reflection-kind memory UUID."},
                        "skill_name": {"type": "string", "description": "agentskills.io §3.1 name: ^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$, 1-64."},
                        "skill_description": {"type": "string", "description": "1-1024 char description."},
                        "parameters_schema": {"type": "object", "description": "Optional JSON schema spliced as Parameters section."}
                    },
                    "required": ["reflection_id", "skill_name", "skill_description"]
                }
            },
            {
                "name": "memory_skill_compositional_context",
                "description": "Skill body + composes_with_reflections (bounded by max_reflection_depth).",
                "docs": "L2-7 (#672): compose skill activation with reflections from SKILL.md composes_with_reflections list. Per-entry min_depth filter; per-namespace max_reflection_depth is the authoritative ceiling (CANNOT bypass bounded-recursion). Reflections ranked recency + recall_count; budget_tokens caps cumulative reflection content (default 4000, max 32000).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill_id": {"type": "string", "description": "Skill UUID."},
                        "budget_tokens": {"type": "integer", "minimum": 0, "description": "cl100k cap on reflection content. Default 4000, max 32000."}
                    },
                    "required": ["skill_id"]
                }
            },
            {
                "name": "memory_offload",
                "description": "Offload verbatim content; returns ref_id (Family::Power).",
                "docs": "QW-3 follow-up: store verbatim in offloaded_blobs. Returns {ref_id, content_sha256, stored_at}. Dereference via memory_deref. Semantic+ tier.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "description": "Verbatim content."},
                        "namespace": {"type": "string", "description": "Namespace bucket. Default 'auto'."},
                        "ttl_seconds": {"type": "integer", "minimum": 0, "description": "Retention hint (seconds)."}
                    },
                    "required": ["content"]
                }
            },
            {
                "name": "memory_deref",
                "description": "Dereference a memory_offload ref_id (Family::Power).",
                "docs": "QW-3 follow-up: sha256-verified lookup. Returns {ref_id, content, stored_at, sha256}. Refuses tampered rows. Semantic+ tier.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "ref_id": {"type": "string", "description": "Ref from memory_offload."}
                    },
                    "required": ["ref_id"]
                }
            }
        ]
    })
}
