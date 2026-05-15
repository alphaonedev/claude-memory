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
/// Optional properties (those NOT in `inputSchema.required`) are
/// stripped from the default `tools/list` payload UNLESS their name
/// appears here. This keeps the most-used knobs (`namespace`,
/// `format`) visible by default — they're load-bearing for routing
/// (namespace) and token-budget control (format=toon_compact) — while
/// hiding the long tail (`confidence`, `priority`, `tier`, `metadata`,
/// `agent_id`, `as_agent`, `since`/`until`, etc.).
///
/// Callers that need the full schema can pass `verbose=true` to
/// `memory_capabilities` (see [`tool_definitions_for_profile`] +
/// [`handle_capabilities_family`]).
const C4_KEEP_OPTIONAL_PARAMS: &[&str] = &["namespace", "format"];

/// v0.7 C4 — strip optional `inputSchema.properties` entries from a
/// `tool_definitions()`-shaped value, keeping only required params and
/// the [`C4_KEEP_OPTIONAL_PARAMS`] allow-list. Idempotent: re-running
/// on an already-trimmed value is a no-op.
///
/// This is the load-bearing token-budget shrink for the default
/// `tools/list` response. The full schema (every optional param,
/// every default, every per-property description) is still available
/// via `memory_capabilities { family=<f>, include_schema=true,
/// verbose=true }` so power users / NHI agents that *do* want to set
/// `confidence=0.7` or pin `tier="long"` can opt back in at runtime.
///
/// Returns the count of properties stripped across all tools, which
/// is useful for telemetry / acceptance assertions in tests.
pub(crate) fn trim_optional_params(defs: &mut Value) -> usize {
    let Some(tools) = defs.get_mut("tools").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut stripped = 0_usize;
    for tool in tools.iter_mut() {
        let Some(input_schema) = tool.get_mut("inputSchema") else {
            continue;
        };
        // Snapshot the required list (clone the names) before we
        // borrow `properties` mutably.
        let required: Vec<String> = input_schema
            .get("required")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let Some(properties) = input_schema
            .get_mut("properties")
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        let drop_keys: Vec<String> = properties
            .keys()
            .filter(|k| {
                !required.iter().any(|r| r == *k)
                    && !C4_KEEP_OPTIONAL_PARAMS
                        .iter()
                        .any(|kept| *kept == k.as_str())
            })
            .cloned()
            .collect();
        for key in &drop_keys {
            properties.remove(key);
        }
        stripped += drop_keys.len();
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
    }
    defs
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
                if let Some(prop_obj) = prop_value.as_object_mut() {
                    prop_obj.remove("description");
                    // Sub-objects (e.g. governance.properties on
                    // memory_namespace_set_standard) — recurse one
                    // level so nested prose is also dropped.
                    if let Some(nested) = prop_obj
                        .get_mut("properties")
                        .and_then(Value::as_object_mut)
                    {
                        for (_, nested_value) in nested.iter_mut() {
                            if let Some(nested_obj) = nested_value.as_object_mut() {
                                nested_obj.remove("description");
                            }
                        }
                    }
                }
            }
        }
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
                "docs": "Store a new memory. Deduplicates by title+namespace. Tier defaults to mid (7d TTL); long is permanent. Caller-supplied agent_id is honored, otherwise an NHI-hardened default is synthesized. Use `on_conflict` to choose between error / merge / version policies for (title, namespace) collisions. Scope (private/team/unit/org/collective) gates Task 1.5 visibility.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "Short descriptive title"},
                        "content": {"type": "string", "description": "Full memory content"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"], "default": "mid"},
                        "namespace": {"type": "string", "description": "Project/topic namespace"},
                        "tags": {"type": "array", "items": {"type": "string"}, "default": []},
                        "priority": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0, "default": 1.0},
                        "source": {"type": "string", "enum": ["user", "claude", "hook", "api", "cli", "import", "consolidation", "system", "chaos"], "default": "claude"},
                        "metadata": {"type": "object", "description": "Arbitrary JSON metadata", "default": {}},
                        "agent_id": {"type": "string", "description": "Agent identifier. If omitted, the server synthesizes an NHI-hardened default (ai:<client>@<host>:pid-<pid>, host:<host>:pid-<pid>-<uuid8>, or anonymous:pid-<pid>-<uuid8>)."},
                        "scope": {"type": "string", "enum": ["private", "team", "unit", "org", "collective"], "description": "Task 1.5 visibility scope. Defaults to private when unset. Stored as metadata.scope."},
                        "on_conflict": {"type": "string", "enum": ["error", "merge", "version"], "description": "v0.6.3.1 P2 (G6) — collision policy when (title, namespace) already exists. 'error' returns CONFLICT (default for v2-capable clients). 'merge' updates the existing row in place (legacy v0.6.3 behaviour, default for v1 clients). 'version' appends a monotonic suffix to the title — 'My Memory (2)', '(3)', ..."}
                    },
                    "required": ["title", "content"]
                }
            },
            {
                "name": "memory_recall",
                "description": "Recall memories relevant to a context (ranked).",
                "docs": "Recall memories relevant to a context. Uses fuzzy OR matching, ranks by relevance + priority + access frequency + tier. Optional context-budget-aware mode (`budget_tokens`, Phase P6 R1) returns the highest-ranked memories whose cumulative cl100k_base content tokens fit in N, with an always-return-at-least-one guarantee. Optional `context_tokens` biases the query embedding 70/30 toward recent conversation (v0.6.0.0). v0.7.0 (issue #518) — pass `session_default=true` to splice the operator-configured `[agents.defaults.recall_scope]` defaults (namespace / since / tier / limit) for any filter field not explicitly set; resolution is explicit args > recall_scope > compiled defaults. v0.7.0 WT-1-E — by default, atomised sources are excluded (atoms surface in their place). Use include_archived=true to retrieve archived sources alongside atoms. Default response format is `toon_compact` (~79% smaller than JSON).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "context": {"type": "string", "description": "What you're trying to remember"},
                        "namespace": {"type": "string", "description": "Filter by namespace"},
                        "limit": {"type": "integer", "default": 10, "maximum": 50},
                        "tags": {"type": "string", "description": "Filter by tag"},
                        "since": {"type": "string", "description": "Only memories created after this RFC3339 timestamp"},
                        "until": {"type": "string", "description": "Only memories created before this RFC3339 timestamp"},
                        "as_agent": {"type": "string", "description": "Querying agent's namespace position (Task 1.5). Enables scope-based visibility filtering — results include private memories at this namespace, team/unit/org memories at ancestor subtrees, and collective memories globally."},
                        "budget_tokens": {"type": "integer", "minimum": 0, "description": "Phase P6 (R1) — context-budget-aware recall. Return the highest-ranked memories whose cumulative content tokens (deterministic cl100k_base BPE; matches Claude/GPT context accounting) fit in N. If the top-ranked memory alone exceeds the budget, it is returned anyway with meta.budget_overflow=true (R1 always-return-at-least-one guarantee). budget_tokens=0 returns zero memories with overflow=false. Response meta block: budget_tokens_used, budget_tokens_remaining, memories_dropped, budget_overflow."},
                        "context_tokens": {"type": "array", "items": {"type": "string"}, "description": "v0.6.0.0 contextual recall — recent conversation tokens used to bias the query embedding at 70/30 (primary/context). Pulls results toward memories that match both the explicit query and nearby conversation topics."},
                        "session_default": {"type": "boolean", "default": false, "description": "When true, splice defaults from [agents.defaults.recall_scope] in config.toml for any filter field not explicitly set. Resolution: explicit args > recall_scope > compiled defaults."},
                        "include_archived": {"type": "boolean", "default": false, "description": "v0.7.0 WT-1-E — by default, atomised sources are excluded (atoms surface in their place). Use include_archived=true to retrieve archived sources alongside atoms (forensic / auditor recall)."},
                        "has_citations": {"type": "boolean", "default": false, "description": "v0.7.0 Form 4 (issue #757) — when true, restrict results to memories whose `citations` array is non-empty (fact-provenance filter)."},
                        "source_uri_prefix": {"type": "string", "description": "v0.7.0 Form 4 (issue #757) — when set, restrict results to memories whose `source_uri` column begins with this exact prefix. Typical use: `doc:` to surface atoms or memories pointing at substrate docs; `uri:https://` to surface memories citing an HTTP source."},
                        "format": {"type": "string", "enum": ["json", "toon", "toon_compact"], "default": "toon_compact", "description": "Response format. Default 'toon_compact' saves 79% tokens vs JSON. 'toon' includes timestamps. 'json' for structured parsing."}
                    },
                    "required": ["context"]
                }
            },
            {
                "name": "memory_search",
                "description": "Search memories by exact keyword match (AND semantics).",
                "docs": "Search memories by exact keyword match (AND semantics). Faster and more deterministic than memory_recall but no fuzzy/semantic match. Filterable by namespace, tier, and agent_id; supports Task 1.5 scope-aware visibility via `as_agent`. v0.7.0 WT-1-E — by default, atomised sources are excluded (atoms surface in their place). Use include_archived=true to retrieve archived sources alongside atoms. Default response format is `toon_compact`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "namespace": {"type": "string"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"]},
                        "limit": {"type": "integer", "default": 20, "maximum": 200},
                        "agent_id": {"type": "string", "description": "Filter by metadata.agent_id (exact match)."},
                        "as_agent": {"type": "string", "description": "Querying agent's namespace position (Task 1.5) for scope-based visibility filtering."},
                        "include_archived": {"type": "boolean", "default": false, "description": "v0.7.0 WT-1-E — by default, atomised sources are excluded (atoms surface in their place). Use include_archived=true to retrieve archived sources alongside atoms (forensic / auditor search)."},
                        "format": {"type": "string", "enum": ["json", "toon", "toon_compact"], "default": "toon_compact", "description": "Response format. Default 'toon_compact' saves 79% tokens. 'json' for structured parsing."}
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "memory_list",
                "description": "List memories, optionally filtered by namespace or tier.",
                "docs": "List memories, optionally filtered by namespace, tier, or agent_id. Browse mode for inspection; default response format is `toon_compact`. Limit caps at 200.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"]},
                        "limit": {"type": "integer", "default": 20, "maximum": 200},
                        "agent_id": {"type": "string", "description": "Filter by metadata.agent_id (exact match)."},
                        "format": {"type": "string", "enum": ["json", "toon", "toon_compact"], "default": "toon_compact", "description": "Response format. Default 'toon_compact' saves 79% tokens. 'json' for structured parsing."}
                    }
                }
            },
            {
                "name": "memory_load_family",
                "description": "Load top-k recent + high-priority memories from a Family.",
                "docs": "v0.7 B1 — load top-k recent + high-priority memories tagged with metadata.family=<family>. Always-on alternative to memory_recall when the family is known. Family enum: core / lifecycle / graph / governance / power / meta / archive / other.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "family": {"type": "string", "enum": ["core", "lifecycle", "graph", "governance", "power", "meta", "archive", "other"], "description": "Family taxonomy enum (one of the eight)."},
                        "namespace": {"type": "string", "description": "Restrict to this namespace. Defaults to all namespaces when omitted."},
                        "k": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20, "description": "Top-k to return. Capped at 100."}
                    },
                    "required": ["family"]
                }
            },
            {
                "name": "memory_smart_load",
                "description": "Intent-routed loader: free-text intent picks the best Family.",
                "docs": "v0.7 B2 — pick the best Family from a free-text intent and forward to memory_load_family. Always-on intent-routed loader. Useful when the agent knows the goal (\"debug a flaky test\") but not the Family taxonomy.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "intent": {"type": "string", "description": "Free-text description of what you're about to do."},
                        "namespace": {"type": "string", "description": "Restrict to this namespace. Defaults to all namespaces when omitted."},
                        "k": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20, "description": "Top-k to return. Capped at 100."}
                    },
                    "required": ["intent"]
                }
            },
            {
                "name": "memory_get_taxonomy",
                "description": "Return a hierarchical tree of namespaces with memory counts.",
                "docs": "Pillar 1 / Stream A — return a hierarchical tree of namespaces with memory counts. Walks the `/`-delimited namespace paths grouped from live memories (expired rows excluded). Each node carries `count` (memories at exactly that namespace) and `subtree_count` (count plus all descendants visible within `depth`); the response also exposes `total_count` for the prefix and a `truncated` flag set when `limit` forced rows to be dropped from the tree.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace_prefix": {"type": "string", "description": "Restrict the tree to memories at this namespace OR any descendant. Omit to walk the full tree. Trailing '/' is tolerated."},
                        "depth": {"type": "integer", "minimum": 0, "maximum": 8, "default": 8, "description": "Max levels to descend below the prefix. Memories deeper than this still contribute to `subtree_count` of the boundary ancestor."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 10000, "default": 1000, "description": "Cap on `(namespace, count)` rows walked when assembling the tree. Densest namespaces win when truncated."}
                    }
                }
            },
            {
                "name": "memory_check_duplicate",
                "description": "Pre-write near-duplicate check via cosine over stored embeddings.",
                "docs": "Pillar 2 / Stream D — pre-write near-duplicate check. Embeds `title + content`, scans live memories with stored embeddings (optionally restricted to `namespace`), and returns the highest-cosine match. `is_duplicate` is `nearest.similarity >= threshold`; the response also surfaces `suggested_merge` (the nearest memory's id) when the threshold is met. Threshold is clamped to a hard floor of 0.5 so permissive callers can't dress unrelated content as a merge candidate. Requires the embedder to be loaded (semantic tier or above).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "Title of the candidate memory. Combined with `content` to form the embedding input, matching memory_store's encoding."},
                        "content": {"type": "string", "description": "Content of the candidate memory."},
                        "namespace": {"type": "string", "description": "Restrict the duplicate scan to this namespace. Omit to scan all namespaces."},
                        "threshold": {"type": "number", "minimum": 0.5, "maximum": 1.0, "default": 0.85, "description": "Cosine similarity threshold for declaring a duplicate. Clamped to >= 0.5. Default 0.85 is tuned for MiniLM-L6-v2 — near-paraphrases land at 0.88+."}
                    },
                    "required": ["title", "content"]
                }
            },
            {
                "name": "memory_entity_register",
                "description": "Register an entity (canonical name + aliases) under a namespace.",
                "docs": "Pillar 2 / Stream B — register an entity (canonical name + aliases) under a namespace. Entities are stored as long-tier memories tagged 'entity' with metadata.kind='entity', so the (title, namespace) coordinate is shared with regular memories without ambiguity. Idempotent: re-registering the same canonical_name+namespace reuses the existing entity_id and merges any new aliases. Errors when the namespace+canonical_name already names a non-entity memory.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "canonical_name": {"type": "string", "description": "Display name for the entity. Stored as the entity memory's title."},
                        "namespace": {"type": "string", "description": "Namespace under which the entity lives. Hierarchy paths are accepted."},
                        "aliases": {"type": "array", "items": {"type": "string"}, "description": "Aliases that should resolve to this entity. Blank entries are skipped; duplicates are de-duped via the entity_aliases primary key."},
                        "metadata": {"type": "object", "description": "Arbitrary metadata to attach to the entity memory. Caller-supplied 'kind' is overwritten with 'entity'; agent_id is stamped from the NHI caller when not specified."},
                        "agent_id": {"type": "string", "description": "Override the caller's resolved NHI for the entity memory's metadata.agent_id."}
                    },
                    "required": ["canonical_name", "namespace"]
                }
            },
            {
                "name": "memory_entity_get_by_alias",
                "description": "Resolve an alias to its registered entity.",
                "docs": "Pillar 2 / Stream B — resolve an alias to its registered entity. When 'namespace' is provided, only entities in that namespace are returned. When omitted, the most recently created matching entity wins. Returns null when no entity claims the alias under the given filter.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "alias": {"type": "string", "description": "Alias string to resolve. Whitespace is trimmed."},
                        "namespace": {"type": "string", "description": "Restrict the resolution to this namespace. Omit to search all namespaces."}
                    },
                    "required": ["alias"]
                }
            },
            {
                "name": "memory_kg_timeline",
                "description": "Ordered fact timeline for an entity (outbound KG links by valid_from).",
                "docs": "Pillar 2 / Stream C — ordered fact timeline for an entity. Returns outbound links from `source_id` (e.g. an entity registered via memory_entity_register) with their temporal-validity columns (valid_from, valid_until, observed_by) and the target memory's title/namespace. Events are ordered by valid_from ASC; rows with NULL valid_from are excluded. Cross-namespace by design — callers can post-filter by target_namespace if needed.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "Memory ID whose outbound assertions form the timeline. Typically an entity_id from memory_entity_register, but any memory works."},
                        "since": {"type": "string", "description": "RFC3339 timestamp; events with valid_from earlier than this are excluded (inclusive boundary)."},
                        "until": {"type": "string", "description": "RFC3339 timestamp; events with valid_from later than this are excluded (inclusive boundary)."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 200, "description": "Max events returned. Clamped to [1, 1000]."}
                    },
                    "required": ["source_id"]
                }
            },
            {
                "name": "memory_kg_invalidate",
                "description": "Mark a KG link as superseded by setting its valid_until column.",
                "docs": "Pillar 2 / Stream C — mark a KG link as superseded by setting its `valid_until` column. The link is identified by the (source_id, target_id, relation) triple (memory_links has no separate id column). When `valid_until` is omitted, the current wall-clock time is used. Idempotent: repeated calls overwrite the prior value and the response reports `previous_valid_until` so callers can detect the overwrite. Returns `found: false` when no link matches the triple.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "Source memory ID of the link to invalidate."},
                        "target_id": {"type": "string", "description": "Target memory ID of the link to invalidate."},
                        "relation": {"type": "string", "description": "Relation label of the link. Must be a recognized relation."},
                        "valid_until": {"type": "string", "description": "RFC3339 timestamp marking when the assertion stops being valid. Defaults to the current time when omitted."}
                    },
                    "required": ["source_id", "target_id", "relation"]
                }
            },
            {
                "name": "memory_kg_query",
                "description": "Outbound KG traversal from a source memory (≤5 hops).",
                "docs": "Pillar 2 / Stream C — outbound KG traversal from a source memory. Returns one node per link reachable from `source_id` within `max_depth` hops, with the link's temporal-validity columns (valid_from, valid_until, observed_by) and the target memory's title/namespace. Multi-hop traversal uses a recursive CTE with cycle detection — chains only extend through links that pass every filter on every hop. Filters: `valid_at` keeps only links valid at that instant; `allowed_agents` keeps only links observed by an agent in the set (empty list returns zero rows by design — empty allowlist means 'no agents are trusted'). Ordered by depth ASC, then COALESCE(valid_from, created_at) ASC, for stable shallow-first display. `max_depth` ceiling is 5 (matches the published performance budget); larger values return an explicit error.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "Memory ID whose outbound links form the traversal frontier. Typically an entity_id from memory_entity_register, but any memory works."},
                        "max_depth": {"type": "integer", "minimum": 1, "maximum": 5, "default": 1, "description": "Hops from the source. Supported range: 1..=5 (matches the published performance budget for `memory_kg_query`). Larger values return an explicit error."},
                        "valid_at": {"type": "string", "description": "RFC3339 timestamp; only links valid at this instant (valid_from <= valid_at AND (valid_until IS NULL OR valid_until > valid_at)) are returned. Omit to skip the temporal filter (NULL valid_from rows are then included)."},
                        "allowed_agents": {"type": "array", "items": {"type": "string"}, "description": "If provided, only links whose observed_by is in this set are returned. An empty array returns zero rows. Omit to skip the agent filter."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 200, "description": "Max nodes returned across all depths. Clamped to [1, 1000]."},
                        "include_invalidated": {"type": "boolean", "default": false, "description": "When false (default), excludes edges whose valid_until lies in the past — i.e. edges invalidated via memory_kg_invalidate are dropped from the 'current view'. Pass true to traverse the full historical link graph (memory_kg_timeline always returns the full history regardless)."}
                    },
                    "required": ["source_id"]
                }
            },
            {
                "name": "memory_find_paths",
                "description": "Enumerate up to N paths through the KG between two memories. Undirected BFS with cycle detection; max_depth ceiling 7.",
                "docs": "v0.7 J7 — enumerate up to N paths through the KG between two memories. BFS with cycle detection over `memory_links` (treated as undirected). Returns paths as id chains, source first, target last. `max_depth` ≤ 7, `max_results` ≤ 50. By default the BFS skips edges invalidated via `memory_kg_invalidate`; pass `include_invalidated=true` to traverse the full historical link graph.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "Path origin memory ID."},
                        "target_id": {"type": "string", "description": "Path destination memory ID."},
                        "max_depth": {"type": "integer", "minimum": 1, "maximum": 7, "default": 4, "description": "Maximum hops between source and target. Default 4, ceiling 7."},
                        "max_results": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10, "description": "Maximum paths returned (shortest-first). Default 10, ceiling 50."},
                        "include_invalidated": {"type": "boolean", "default": false, "description": "When false (default), excludes edges whose valid_until lies in the past. Pass true to enumerate paths through the full historical link graph."}
                    },
                    "required": ["source_id", "target_id"]
                }
            },
            {
                "name": "memory_delete",
                "description": "Delete a memory by ID.",
                "docs": "Hard-delete a memory by ID. Removes the row, its embedding, FTS entry, and any links. Use memory_forget for bulk pattern-based deletion (which archives first).",
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
                "description": "Promote a memory to long-term, or clone to an ancestor namespace.",
                "docs": "Promote a memory. Default: bump tier to long-term (permanent, clears expiry). Task 1.7: when 'to_namespace' is supplied, clone the memory to a hierarchical-ancestor namespace and link clone → source with 'derived_from'. Original is untouched.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "to_namespace": {"type": "string", "description": "Task 1.7: hierarchical-ancestor namespace to clone this memory into. Must be a proper ancestor (per namespace_ancestors()). Original memory stays put; a new memory with derived_from link is created at the target namespace."}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_forget",
                "description": "Bulk delete memories matching a pattern, namespace, or tier (archives first).",
                "docs": "Bulk delete memories matching a pattern, namespace, or tier. Archives before deletion so memory_archive_restore can recover. Use dry_run to preview the affected set without mutating.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string"},
                        "pattern": {"type": "string"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"]},
                        "dry_run": {"type": "boolean", "default": false, "description": "If true, report what would be deleted without deleting"}
                    }
                }
            },
            {
                "name": "memory_stats",
                "description": "Get memory store statistics (counts, tier breakdown, sizes).",
                "docs": "Get memory store statistics — total counts, per-tier breakdown, namespace tallies, archive size, and DB file size.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "memory_update",
                "description": "Update an existing memory by ID (only provided fields change).",
                "docs": "Update an existing memory by ID. Only provided fields are changed; omitted fields preserve their existing values. Tier may be raised but not silently downgraded by an update path that doesn't explicitly request it. metadata.agent_id is preserved across updates.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Memory ID to update"},
                        "title": {"type": "string"},
                        "content": {"type": "string"},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"]},
                        "namespace": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}},
                        "priority": {"type": "integer", "minimum": 1, "maximum": 10},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
                        "expires_at": {"type": "string", "description": "Expiry timestamp (RFC3339), or null to clear"},
                        "metadata": {"type": "object", "description": "Arbitrary JSON metadata"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_get",
                "description": "Get a specific memory by ID, including its links.",
                "docs": "Get a specific memory by ID. Response includes the memory row plus all linked memory IDs (both inbound and outbound). Use memory_get_links for the full link rows with relation labels and signature attestation.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Memory ID to retrieve"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_link",
                "description": "Create a typed link between two memories.",
                "docs": "Create a directional link between two memories with one of five canonical relations: related_to, supersedes, contradicts, derived_from, reflects_on. v0.7.0 Task 3/8 (recursive learning) added `reflects_on` — a reflection memory (with reflection_depth > 0) writes this link back to each source memory it reflects on (matches the `derived_from` directionality: newer/derived row is source_id, the thing it points back to is target_id). v0.7 H-track signs the link with the active Ed25519 keypair when one is configured (verifiable via memory_verify).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_id": {"type": "string", "description": "Source memory ID"},
                        "target_id": {"type": "string", "description": "Target memory ID"},
                        "relation": {"type": "string", "enum": ["related_to", "supersedes", "contradicts", "derived_from", "reflects_on"], "default": "related_to"}
                    },
                    "required": ["source_id", "target_id"]
                }
            },
            {
                "name": "memory_get_links",
                "description": "Get all links for a memory (both directions).",
                "docs": "Get all links for a memory (both inbound and outbound). Returns relation labels, attestation level (unsigned/self_signed/peer_attested), and temporal validity columns (valid_from, valid_until, observed_by) per link.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Memory ID to get links for"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_verify",
                "description": "Re-verify a stored memory_links row's Ed25519 signature on demand.",
                "docs": "v0.7 Track H4 — re-verify a stored memory_links row's Ed25519 signature on demand. Returns {signature_verified, attest_level, signed_by, signed_at}. attest_level is one of unsigned/self_signed/peer_attested. Pass either the link_id composite ('source--relation-->target') or the explicit source_id+target_id (+optional relation, default related_to).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "link_id": {"type": "string", "description": "Composite link identifier in the form 'source_id--relation-->target_id'. Equivalent to passing source_id+target_id+relation explicitly."},
                        "source_id": {"type": "string", "description": "Source memory ID. Required when link_id is omitted."},
                        "target_id": {"type": "string", "description": "Target memory ID. Required when link_id is omitted."},
                        "relation": {"type": "string", "enum": ["related_to", "supersedes", "contradicts", "derived_from", "reflects_on"], "default": "related_to", "description": "Link relation. Defaults to related_to when omitted (matches memory_link's default). v0.7.0 Task 3/8 added `reflects_on`."}
                    }
                }
            },
            {
                "name": "memory_replay",
                "description": "Reconstruct the conversation transcript chain that produced a memory.",
                "docs": "Reconstruct the conversation transcript chain that produced this memory. Returns decompressed text + span metadata for each linked transcript. v0.7.0 I4 — when verbose=false (default), transcripts >100KB have content omitted with truncated=true; opt into verbose=true for the full multi-MB dump. v0.7.0 L2-4 (issue #669) — when the input memory is a reflection (memory_kind='reflection'), the replay returns the UNION of transcripts reachable by walking `reflects_on` edges to the source observations. Cap the walk with `depth=N` (full chain by default; `0` returns the reflection's own transcripts only — the pre-L2-4 shape). Each entry carries a `source_memory_id` so callers can see which ancestor anchored each transcript. Non-reflection memories ignore `depth`; their reply shape is unchanged from the pre-L2-4 I4 behaviour.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_id": {"type": "string", "description": "Memory ID whose transcript chain should be reconstructed."},
                        "verbose": {"type": "boolean", "default": false, "description": "v0.7.0 I4 — when false (default), any single transcript exceeding 100KB has its content omitted and is flagged with truncated=true. Set to true to opt into a full dump (use with care: transcripts can be multi-MB)."},
                        "depth": {"type": ["integer", "null"], "minimum": 0, "default": null, "description": "v0.7.0 L2-4 — optional cap on the reflection-union walk over `reflects_on` edges. `null` (default) walks the full chain; `0` returns the reflection's own transcripts only (matches the pre-L2-4 I4 shape); `N>=1` returns self plus N hops of ancestors. Ignored for non-reflection memories."}
                    },
                    "required": ["memory_id"]
                }
            },
            {
                "name": "memory_reflect",
                "description": "Persist a reflection memory plus reflects_on provenance links to each source.",
                "docs": "v0.7.0 Task 4/8 (recursive learning, issue #655) — the substrate-native primitive for recursive learning. An agent reads one or more memories, synthesises a higher-order reflection (a lesson, pattern, contradiction-resolution, etc.), and persists it with cryptographic-grade provenance to the sources it derives from. The reflection memory's `reflection_depth` is `max(source_depths) + 1`; the namespace cap on `governance.max_reflection_depth` (Task 2/8) gates the depth — refusal returns the structured `REFLECTION_DEPTH_EXCEEDED` error so callers and the Task 5/8 audit emitter can branch on it. The new memory plus N `reflects_on` link writes are a single atomic transaction — any link-insert failure rolls back the entire reflection. The reflection memory's `metadata.reflection_metadata` records the source-id list, the resolved depth, and the RFC3339 creation timestamp (caller-supplied metadata keys win on collision).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source_ids": {"type": "array", "items": {"type": "string"}, "minItems": 1, "description": "Memory IDs this reflection reflects on. Must be non-empty; one reflects_on link is written from the new memory back to each id."},
                        "title": {"type": "string", "description": "Short descriptive title for the reflection memory."},
                        "content": {"type": "string", "description": "Full reflection content."},
                        "namespace": {"type": "string", "description": "Target namespace for the reflection memory. Defaults to the namespace of the first source memory when omitted."},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"], "default": "mid"},
                        "tags": {"type": "array", "items": {"type": "string"}, "default": []},
                        "priority": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5},
                        "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0, "default": 1.0},
                        "agent_id": {"type": "string", "description": "Agent identifier for the reflection writer. Defaults to the NHI-hardened resolution chain when omitted (matches memory_store)."},
                        "metadata": {"type": "object", "description": "Free-form metadata; merged with system-generated reflection_metadata fields. Caller-supplied keys win on collision."}
                    },
                    "required": ["source_ids", "title", "content"]
                }
            },
            {
                "name": "memory_export_reflection",
                "description": "Render a single reflection memory as markdown or JSON (no filesystem write).",
                "docs": "v0.7.0 QW-1 — render a reflection memory plus its `reflects_on` provenance as a YAML-frontmatter markdown document (default) or as a structured JSON envelope. Returns `{content, suggested_filename}`. The handler does NOT write to the filesystem — the agent harness owns disk I/O so the substrate stays under the operator's capability gate. Pair with the `ai-memory export-reflections` CLI when operator-driven bulk export is wanted. Errors: `memory not found`, `memory is not a reflection`, `unsupported export format`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_id": {"type": "string", "description": "Memory ID of a Reflection-kind memory (created via memory_reflect)."},
                        "format": {"type": "string", "enum": ["md", "json"], "default": "md", "description": "Output format. `md` is YAML-frontmatter markdown; `json` is a structured envelope mirroring the same fields."}
                    },
                    "required": ["memory_id"]
                }
            },
            {
                "name": "memory_persona",
                "description": "Fetch the latest Persona artefact for an entity (read-only).",
                "docs": "v0.7.0 QW-2 — read the most recent `MemoryKind::Persona` row for `(entity_id, namespace)`. Returns the structured envelope `{id, entity_id, namespace, body_md, sources, generated_at, version, attest_level}` rendered from the SQL row and its `metadata.persona` envelope. The body_md is a 300–500 word Markdown distillation produced by the reflection-pass curator from a cluster of `MemoryKind::Reflection` memories; every claim is footnoted with a `[^N]: <reflection-id>` citation. Returns `null` when no persona has ever been generated for the entity. Indexed lookup via `idx_personas_by_entity` (schema v36). Pair with `memory_persona_generate` to mint or refresh the artefact.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "entity_id": {"type": "string", "description": "Subject of the persona — the entity identifier passed to memory_persona_generate."},
                        "namespace": {"type": "string", "description": "Namespace the persona was minted under. Defaults to `global`."}
                    },
                    "required": ["entity_id"]
                }
            },
            {
                "name": "memory_persona_generate",
                "description": "Generate (or regenerate) a Persona artefact for an entity via the reflection-pass curator.",
                "docs": "v0.7.0 QW-2 — synthesise a `MemoryKind::Persona` memory by loading the top-K Reflection-kind memories about `entity_id` in `namespace`, running them through the substrate's reflection-pass curator (Gemma 4 via Ollama in production; mock LLM in tests), and persisting a 300–500 word Markdown profile with `[^ref]` footnotes citing the source reflections. Writes a new row per call — the substrate never overwrites a persona in place; each generation bumps `persona_version`. One `derived_from` `memory_link` edge lands per source reflection so the KG walker (`memory_find_paths`, `memory_kg_query`) can follow the Persona → Reflection → Observation chain end-to-end. Append-only `persona_generated` row written to `signed_events` for the H5 audit chain. Smart+autonomous tier only — refuses on semantic-tier and below.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "entity_id": {"type": "string", "description": "Subject of the persona. Must be 1–128 characters."},
                        "namespace": {"type": "string", "description": "Namespace to mint the persona under. Defaults to `global`."}
                    },
                    "required": ["entity_id"]
                }
            },
            {
                "name": "memory_reflection_origin",
                "description": "Inspect the cross-peer provenance of a reflection memory.",
                "docs": "v0.7.0 L2-2 (S6-M1) — returns the structured `{memory_id, peer_origin, signing_agent, original_depth, local_depth_at_arrival, is_reflection}` envelope describing where a reflection row originated. `peer_origin` is the substrate identity of the peer that pushed the row to this host via `sync_push`; `signing_agent` is the original author (NHI agent_id) preserved across federation; `original_depth` is the `reflection_depth` column value as delivered; `local_depth_at_arrival` is the receiver's effective `max_reflection_depth` cap at the moment the row arrived. Non-reflection memories (depth == 0) return a well-formed envelope with `is_reflection = false` rather than a 404. Unknown ids → error.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_id": {"type": "string", "description": "Memory ID whose reflection-origin record should be returned."}
                    },
                    "required": ["memory_id"]
                }
            },
            {
                "name": "memory_dependents_of_invalidated",
                "description": "List dependents flagged by the L2-3 invalidation walker.",
                "docs": "v0.7.0 L2-3 (issue #668) — returns the set of memories whose `reflects_on` edge points at a given reflection — i.e. the dependents that are (or would be) notified when that reflection is superseded by another reflection. Pure read-only — does not trigger the walker or mutate the DB. The walker itself fires from the `memory_link` handler when a Reflection→Reflection `supersedes` edge lands: it writes one notification memory per dependent under `<dependent.namespace>/_invalidations` with `metadata.notification_kind = 'reflection_invalidation'` and the four-tuple `{dependent_id, invalidated_id, invalidating_id, timestamp}`. Notification, NOT cascade — dependents are flagged for operator/curator review, never auto-superseded. Returns `{memory_id, count, dependents: [{id, namespace}]}`. Unknown ids return an empty `dependents` array with `count = 0`.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_id": {"type": "string", "description": "ID of the (potentially) invalidated reflection. Returns the inbound `reflects_on` dependents."}
                    },
                    "required": ["memory_id"]
                }
            },
            {
                "name": "memory_consolidate",
                "description": "Consolidate multiple memories into one long-term summary.",
                "docs": "Consolidate multiple memories into one long-term summary. Deletes source memories and creates derived_from links from the consolidated memory back to each source. If summary is omitted and an LLM is available (smart/autonomous tier), the summary is auto-generated. Minimum 2, maximum 100 source ids per call.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "ids": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 100, "description": "Memory IDs to consolidate (minimum 2, maximum 100)"},
                        "title": {"type": "string", "description": "Title for the consolidated memory"},
                        "summary": {"type": "string", "description": "Summary content (optional — auto-generated via LLM if omitted at smart/autonomous tier)"},
                        "namespace": {"type": "string", "default": "global"}
                    },
                    "required": ["ids", "title"]
                }
            },
            {
                "name": "memory_atomise",
                "description": "Decompose a coarse-grained memory into 2-10 atomic propositions. Each atom is independently retrievable with provenance back to the source. Source is archived. Available at smart and autonomous tiers.",
                "docs": "v0.7.0 WT-1-C — curator-pass atomisation tool. Decomposes the supplied memory's body into 2-10 atomic propositions via the v0.7.0 WT-1-B Atomiser engine. Each atom is written as a first-class memory (memory_kind=Observation) carrying `metadata.atom_source_id` and connected to the source via a `derives_from` memory_link edge. The source memory is archived (`atomised_into=N`, `metadata.atomisation_archived_at` set) in a separate post-atom transaction so the per-atom hook chain (pre_store/post_store/pre_link/post_link) fires on live writes. Returns `{source_id, atom_ids, atom_count, archived_at}` on success. Idempotency: a second call without `force_re_atomise=true` returns the existing `atom_ids` as a 200 OK informational envelope `{already_atomised: true, existing_atom_ids: [...]}`. Source bodies at or under `max_atom_tokens` return `{source_too_small: true, message}` (also 200 OK — informational). Curator failures and governance refusals collapse to MCP `isError: true` envelopes with `CURATOR_FAILED:` / `GOVERNANCE_REFUSED:` discriminators. The keyword tier short-circuits with a tier-locked advisory envelope before any DB read.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "memory_id": {"type": "string", "description": "UUID of the source memory to atomise."},
                        "max_atom_tokens": {"type": "integer", "minimum": 50, "maximum": 1000, "default": 200, "description": "Per-atom token budget (cl100k_base). Out-of-range values are rejected by the input validator."},
                        "force_re_atomise": {"type": "boolean", "default": false, "description": "When true, skip the idempotency check and mint a fresh set of atoms. Old atoms are retained (their `atom_of` pointer remains valid); `atomised_into` is bumped to the new atom_count."}
                    },
                    "required": ["memory_id"]
                }
            },
            {
                "name": "memory_capabilities",
                "description": "Discover runtime capabilities; family=<name> drills in.",
                "docs": "Capabilities-v3 (v0.7 default, always-on): tier, profile, summary, to_describe_to_user, callable_now per tool, agent_permitted_families, harness detection. family=<name> (+include_schema) enumerates one family; accept=v2/v1 for legacy clients. v0.7 C2 — pass verbose=true (with family=<name>+include_schema=true) to receive the long-form `docs` field on each tool entry, which the bare `tools/list` payload omits to stay inside the C5 token budget. v0.7 C4 — verbose=true also restores the FULL inputSchema (every optional param) instead of the trimmed default; the C2 docs strip and the C4 optional-params trim are both governed by the same `verbose` flag.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "accept": {
                            "type": "string",
                            "enum": ["v1", "v2"],
                            "default": "v2",
                            "description": "Capabilities-schema version. v2 is the honest, runtime-overlaid shape (default). v1 returns the legacy pre-v0.6.3.1 shape for backward compat."
                        },
                        "family": {
                            "type": "string",
                            "enum": ["core", "lifecycle", "graph", "governance", "power", "meta", "archive", "other"],
                            "description": "v0.6.4 — when set, returns the tool list (or full schemas with include_schema=true) for that family instead of the global capabilities document. Used by NHI agents to opt into a tool family at runtime without restarting the MCP server."
                        },
                        "include_schema": {
                            "type": "boolean",
                            "default": false,
                            "description": "v0.6.4 — when true, return full MCP-style tool definitions for each tool in the requested family. Requires family=<name>."
                        },
                        "verbose": {
                            "type": "boolean",
                            "default": false,
                            "description": "v0.7 C2/C4 — when true (with family=<name>+include_schema=true), preserve BOTH the per-tool `docs` field (long-form description + examples; C2) AND every optional `inputSchema` property (confidence, priority, tier, metadata, agent_id, …; C4). When false (default), `docs` is stripped and `inputSchema.properties` is trimmed to required + a small allow-list of high-traffic optionals (namespace, format), matching the always-on `tools/list` shape."
                        }
                    }
                }
            },
            {
                "name": "memory_expand_query",
                "description": "LLM-expand a search query into related terms (smart/autonomous tier).",
                "docs": "Use LLM to expand a search query into additional semantically related terms. Requires smart or autonomous tier (Ollama backend configured).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "The search query to expand"}
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "memory_auto_tag",
                "description": "LLM-generate tags for a memory (smart/autonomous tier).",
                "docs": "Use LLM to auto-generate tags for a memory. Requires smart or autonomous tier (Ollama backend configured).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Memory ID to auto-tag"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_detect_contradiction",
                "description": "LLM-check whether two memories contradict each other (smart/autonomous tier).",
                "docs": "Use LLM to check whether two memories contradict each other. Requires smart or autonomous tier (Ollama backend configured).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id_a": {"type": "string", "description": "First memory ID"},
                        "id_b": {"type": "string", "description": "Second memory ID"}
                    },
                    "required": ["id_a", "id_b"]
                }
            },
            {
                "name": "memory_archive_list",
                "description": "List archived (expired) memories.",
                "docs": "List archived (expired) memories. Archived memories are preserved before GC deletion so memory_archive_restore can recover them. Filter by namespace, paginate via offset/limit.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Filter by namespace"},
                        "limit": {"type": "integer", "description": "Max results (default 50, max 1000)"},
                        "offset": {"type": "integer", "description": "Pagination offset"}
                    }
                }
            },
            {
                "name": "memory_archive_restore",
                "description": "Restore an archived memory back to the active store.",
                "docs": "Restore an archived memory back to the active memory store. expires_at is cleared so the restored memory does not immediately re-expire.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "ID of the archived memory to restore"}
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_archive_purge",
                "description": "Permanently delete archived memories.",
                "docs": "Permanently delete archived memories. Pass `older_than_days` to scope the purge; omit to purge every archived row. This is unrecoverable.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "older_than_days": {"type": "integer", "description": "Only purge entries archived more than N days ago. Omit to purge all."}
                    }
                }
            },
            {
                "name": "memory_archive_stats",
                "description": "Show archive statistics (total count and per-namespace breakdown).",
                "docs": "Show archive statistics: total count and per-namespace breakdown of archived memories.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "memory_gc",
                "description": "Trigger garbage collection on expired memories (archives first).",
                "docs": "Trigger garbage collection on expired memories. Archives them before deletion when archive_on_gc is enabled (default). dry_run reports the affected set without mutating.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "dry_run": {"type": "boolean", "default": false, "description": "If true, report what would be collected without deleting"}
                    }
                }
            },
            {
                "name": "memory_session_start",
                "description": "Auto-recall recent memories on session start.",
                "docs": "Auto-recall recent memories on session start. Returns the most recently accessed/updated memories. If an LLM is available (smart/autonomous tier), the response also includes an AI-generated summary of the recalled set.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Optional namespace to scope recall"},
                        "limit": {"type": "integer", "default": 10, "maximum": 50},
                        "format": {"type": "string", "enum": ["json", "toon", "toon_compact"], "default": "toon_compact"}
                    }
                }
            },
            {
                "name": "memory_namespace_set_standard",
                "description": "Set a memory as the standard/policy for a namespace.",
                "docs": "Set a memory as the standard/policy for a namespace. The standard is auto-prepended to recall and session_start results. Supports rule layering (global '*' + parent chain + namespace). Task 1.8: accepts an optional `governance` policy object merged into the standard memory's metadata, with v0.6.3.1 P4/G1 inheritance flag.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Namespace to set the standard for"},
                        "id": {"type": "string", "description": "Memory ID to use as the standard"},
                        "parent": {"type": "string", "description": "Optional parent namespace to inherit standards from (rule layering)"},
                        "governance": {
                            "type": "object",
                            "description": "Task 1.8 governance policy. Stored in metadata.governance on the standard memory. Consumed by Task 1.9 enforcement + 1.10 approver types. v0.6.3.1 (P4, G1): adds `inherit` flag controlling parent-namespace policy bubbling.",
                            "properties": {
                                "write":    {"type": "string", "enum": ["any", "registered", "owner", "approve"]},
                                "promote":  {"type": "string", "enum": ["any", "registered", "owner", "approve"]},
                                "delete":   {"type": "string", "enum": ["any", "registered", "owner", "approve"]},
                                "approver": {"description": "ApproverType: \"human\" | {\"agent\": \"<id>\"} | {\"consensus\": <n>}"},
                                "inherit":  {"type": "boolean", "default": true, "description": "v0.6.3.1 (P4, G1): when true (default), missing policy at this namespace falls through to parent in the chain. Set false to opt this subtree out of parent inheritance."}
                            }
                        }
                    },
                    "required": ["namespace", "id"]
                }
            },
            {
                "name": "memory_namespace_get_standard",
                "description": "Get the standard/policy memory for a namespace.",
                "docs": "Get the standard/policy memory for a namespace, if one is set. With inherit=true (Task 1.6) returns the full N-level resolved chain (global * → ancestors → namespace) instead of the single namespace's standard.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Namespace to get the standard for"},
                        "inherit": {"type": "boolean", "default": false, "description": "Task 1.6: when true, return the full inheritance chain (global * → ancestors → namespace) as a list instead of the single namespace's standard."}
                    },
                    "required": ["namespace"]
                }
            },
            {
                "name": "memory_namespace_clear_standard",
                "description": "Clear the standard/policy for a namespace.",
                "docs": "Clear the standard/policy for a namespace. Future recall + session_start in that namespace stop auto-prepending the standard memory.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Namespace to clear the standard for"}
                    },
                    "required": ["namespace"]
                }
            },
            {
                "name": "memory_pending_list",
                "description": "List pending governance-queued actions.",
                "docs": "List pending governance-queued actions (Task 1.9). Filter by status: pending (default) / approved / rejected. Limit caps at 1000.",
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
                "docs": "Approve a pending action by id (Task 1.9). Caller identity is stamped as decided_by. v0.7 K10 — optional `remember` (\"once\"|\"session\"|\"forever\") records a synthetic permission rule so the same context auto-decides next time.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Pending action id"},
                        "remember": {
                            "type": "string",
                            "enum": ["once", "session", "forever"],
                            "default": "once",
                            "description": "v0.7 K10 — persistence horizon for the decision"
                        }
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_pending_reject",
                "description": "Reject a pending action; `remember` auto-decides next time.",
                "docs": "Reject a pending action by id (Task 1.9). Caller identity is stamped as decided_by. v0.7 K10 — optional `remember` (\"once\"|\"session\"|\"forever\") records a synthetic deny rule so the same context auto-rejects next time.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Pending action id"},
                        "remember": {
                            "type": "string",
                            "enum": ["once", "session", "forever"],
                            "default": "once",
                            "description": "v0.7 K10 — persistence horizon for the decision"
                        }
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "memory_agent_register",
                "description": "Register an agent in the reserved _agents namespace.",
                "docs": "Register an agent in the reserved _agents namespace. Stores agent_type and capabilities, refreshes last_seen_at on re-registration while preserving registered_at. agent_id is *claimed*, not attested — pair with attestation if you need a security boundary.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string", "description": "Agent identifier (same validation as metadata.agent_id)"},
                        // Round-2 F16 — agent_type is OPEN-form at
                        // the schema layer. The daemon's
                        // `validate::validate_agent_type` accepts
                        // the curated short-list (human, system,
                        // ai:claude-opus-*, ai:codex-*, ai:grok-*)
                        // PLUS any `ai:<name>` form (alnum/_-.) up
                        // to 64 chars, so the closed wire enum was
                        // lagging the daemon's forward-compat
                        // surface. Document the canonical labels
                        // in prose so well-behaved clients pick
                        // sensible defaults, but don't reject
                        // `ai:<future-model>` at the schema layer.
                        "agent_type": {
                            "type": "string",
                            "description": "Agent type label. Curated: human, system, ai:claude-opus-4.6, ai:claude-opus-4.7, ai:codex-5.4, ai:grok-4.2. Open-form: any `ai:<name>` (alnum/_-.) up to 64 chars — register e.g. ai:claude-opus-4.8 without a code release. Anything outside the curated list and the `ai:` namespace is rejected by the handler with a 400."
                        },
                        "capabilities": {"type": "array", "items": {"type": "string"}, "default": [], "description": "Optional capability tags"}
                    },
                    "required": ["agent_id", "agent_type"]
                }
            },
            {
                "name": "memory_agent_list",
                "description": "List every registered agent.",
                "docs": "List every agent registered via memory_agent_register, ordered by registered_at.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "memory_notify",
                "description": "Send a message from the caller to another agent's inbox.",
                "docs": "v0.6.0.0 — send a message from the caller to another agent. Stored as a memory in the reserved `_messages/<target>` namespace with sender metadata. The sender is the caller's resolved agent_id. Target agent reads via `memory_inbox`. Payload is a free-form string.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "target_agent_id": {"type": "string", "description": "Recipient agent_id (same validation as metadata.agent_id)"},
                        "title": {"type": "string", "description": "Short subject (≤ 200 chars, required)"},
                        "payload": {"type": "string", "description": "Message body (required)"},
                        "priority": {"type": "integer", "minimum": 1, "maximum": 10, "default": 5},
                        "tier": {"type": "string", "enum": ["short", "mid", "long"], "default": "mid", "description": "short TTL default = 6h, mid = 7d, long = no expiry"}
                    },
                    "required": ["target_agent_id", "title", "payload"]
                }
            },
            {
                "name": "memory_inbox",
                "description": "List messages sent to an agent via memory_notify.",
                "docs": "v0.6.0.0 — list messages sent to an agent via memory_notify. Reads the reserved `_messages/<agent_id>` namespace. `access_count == 0` is the conventional unread marker; recalling/reading a memory increments access_count via the normal touch path.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string", "description": "Recipient agent_id. Defaults to the caller's resolved agent_id."},
                        "unread_only": {"type": "boolean", "default": false, "description": "When true, return only messages with access_count == 0."},
                        "limit": {"type": "integer", "default": 50, "maximum": 500}
                    }
                }
            },
            {
                "name": "memory_subscribe",
                "description": "Register a webhook subscription for memory events.",
                "docs": "v0.6.0.0 — register a webhook subscription. Events fire on memory_store today and additional events in v0.6.1+. Payload is a JSON body signed with HMAC-SHA256 when a secret is supplied (header: X-Ai-Memory-Signature: sha256=<hex>). URL must be https unless the host is a loopback address. The shared secret is stored hashed only; the plaintext the operator supplies is what they verify signatures with.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "url": {"type": "string", "description": "https:// endpoint (or http:// for loopback). SSRF guard rejects private-range IPs."},
                        "events": {"type": "string", "default": "*", "description": "Comma-separated event whitelist or `*` for all. Known events: memory_store, memory_delete, memory_promote."},
                        "secret": {"type": "string", "description": "Optional shared secret for HMAC signing. If omitted, payload is unsigned."},
                        "namespace_filter": {"type": "string", "description": "Optional exact namespace match."},
                        "agent_filter": {"type": "string", "description": "Optional agent_id filter — only events whose stored agent_id matches this value will fire."}
                    },
                    "required": ["url"]
                }
            },
            {
                "name": "memory_unsubscribe",
                "description": "Delete a subscription by id.",
                "docs": "v0.6.0.0 — delete a webhook subscription by id. Stops further deliveries; existing DLQ rows are retained for audit.",
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
                "docs": "v0.6.0.0 — list active webhook subscriptions. Secrets are not exposed; only `secret_hash` is stored server-side and even that is not returned.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "memory_subscription_replay",
                "description": "Replay subscription_events since an RFC3339 timestamp.",
                "docs": "v0.7 K7 — replay subscription_events for one subscription since an RFC3339 timestamp. Returns ordered audit envelope (delivered_at asc). Operator/governance tool.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "subscription_id": {"type": "string", "description": "Subscription id from memory_subscribe."},
                        "since": {"type": "string", "description": "RFC3339 lower bound on delivered_at (inclusive)."}
                    },
                    "required": ["subscription_id", "since"]
                }
            },
            {
                "name": "memory_subscription_dlq_list",
                "description": "List subscription_dlq rows (exhausted retry ladder).",
                "docs": "v0.7 K7 — list subscription_dlq rows (deliveries that exhausted the retry ladder). Filter by subscription_id; cap with limit. Operator/governance inspector.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "subscription_id": {"type": "string", "description": "Optional — restrict to one subscription."},
                        "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 100}
                    }
                }
            },
            {
                "name": "memory_quota_status",
                "description": "Report per-agent quota usage. Operator-facing.",
                "docs": "v0.7 K8 — report per-agent quota usage (memories/day, storage bytes, links/day). Omit agent_id to list all agents. Operator-facing.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "agent_id": {"type": "string", "description": "Optional — restrict to one agent. When omitted, returns every quota row."}
                    }
                }
            },
            {
                "name": "memory_check_agent_action",
                "description": "Substrate-rule-bound, harness-mediated check of a proposed agent action (Bash / FilesystemWrite / NetworkRequest / ProcessSpawn / Custom) against the governance_rules table. Returns Allow / Refuse / Warn (issue #691).",
                "docs": "v0.7.0 (issue #691) — substrate-level agent-action rules engine. Read-only check of one proposed action against every enabled rule of matching kind. The harness's PreToolUse hook (type=mcp_tool) calls this on every Bash/Write/Edit dispatch and honors the decision. This is the read-side MCP surface; rule MUTATION over MCP is explicitly disabled (return governance.not_available_over_mcp) — use the CLI `ai-memory rules` with --sign or the HTTP admin endpoints with X-AI-Memory-Operator-Signature header.",
                "inputSchema": {
                    "type": "object",
                    "required": ["kind"],
                    "properties": {
                        "kind": {"type": "string", "enum": ["bash", "filesystem_write", "network_request", "process_spawn", "custom"]},
                        "command": {"type": "string", "description": "Required when kind=bash."},
                        "cwd": {"type": "string", "description": "Optional cwd for kind=bash."},
                        "path": {"type": "string", "description": "Required when kind=filesystem_write."},
                        "byte_estimate": {"type": "integer", "description": "Optional bytes-to-write hint."},
                        "host": {"type": "string", "description": "Required when kind=network_request."},
                        "scheme": {"type": "string", "description": "Optional scheme; defaults to https."},
                        "binary": {"type": "string", "description": "Required when kind=process_spawn."},
                        "args": {"type": "array", "items": {"type": "string"}, "description": "Optional argv tail for process_spawn."},
                        "custom_kind": {"type": "string", "description": "Required when kind=custom."},
                        "agent_id": {"type": "string", "description": "Optional caller id (audit-row provenance)."}
                    }
                }
            },
            {
                "name": "memory_rule_list",
                "description": "List substrate-level agent-action rules. Read-only (issue #691).",
                "docs": "v0.7.0 (issue #691) — list every rule in the governance_rules table. Optional `kind` filter and `enabled_only` flag. Rule mutation is operator-only (CLI/HTTP with signed operator key); MCP cannot add/remove/enable/disable rules per design revision 2026-05-13.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "description": "Optional — restrict to one AgentAction kind."},
                        "enabled_only": {"type": "boolean", "description": "Optional — when true, skip disabled rules. Default false."}
                    }
                }
            },
            {
                "name": "memory_skill_register",
                "description": "Register an agentskills.io-compliant SKILL.md skill from a folder or inline text.",
                "docs": "v0.7.0 L1-5 — Register a SKILL.md skill into the skills table with Ed25519 attestation and version chaining. Accepts either folder_path (directory containing SKILL.md) or inline_skill (raw SKILL.md text). Re-registering the same name+namespace creates a new version row; the previous row is superseded.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "folder_path": {"type": "string", "description": "Path to a directory containing SKILL.md (and optional resources/ sub-directory)."},
                        "inline_skill": {"type": "string", "description": "Raw SKILL.md text including YAML frontmatter and markdown body."}
                    }
                }
            },
            {
                "name": "memory_skill_list",
                "description": "List current (non-superseded) skills — discovery payload (~100 tokens/skill, body not returned).",
                "docs": "v0.7.0 L1-5 — Discovery endpoint. Returns name, description, id, namespace, digest and metadata for all current skills. Body is NOT decompressed/returned — use memory_skill_get for the activation payload.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "namespace": {"type": "string", "description": "Filter to this namespace. Omit (or pass '%') for all namespaces."},
                        "filter": {"type": "string", "description": "Optional text filter applied to name and description."}
                    }
                }
            },
            {
                "name": "memory_skill_get",
                "description": "Get the full activation payload for a skill (metadata + decompressed body). Old versions still accessible by id.",
                "docs": "v0.7.0 L1-5 — Returns the full activation payload: all metadata plus the decompressed SKILL.md body (<5000 tokens). Durable history: old version ids remain addressable after supersession.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill_id": {"type": "string", "description": "The UUID of the skill to retrieve."}
                    },
                    "required": ["skill_id"]
                }
            },
            {
                "name": "memory_skill_resource",
                "description": "Fetch and digest-verify a skill resource (script, reference, or asset).",
                "docs": "v0.7.0 L1-5 — Returns the decompressed content of a skill_resources row after verifying its SHA-256 digest. Returns an error on digest mismatch.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill_id": {"type": "string", "description": "The UUID of the parent skill."},
                        "resource_path": {"type": "string", "description": "The relative path of the resource (e.g. 'scripts/run.sh')."}
                    },
                    "required": ["skill_id", "resource_path"]
                }
            },
            {
                "name": "memory_skill_export",
                "description": "Export a skill to a folder as a round-trip-compatible SKILL.md (re-registering produces identical digest).",
                "docs": "v0.7.0 L1-5 — Writes SKILL.md + resources/ sub-directory to target_folder. Re-registering from the exported folder via memory_skill_register produces the IDENTICAL SHA-256 digest — the round-trip guarantee. Appends a skill.exported signed_events row.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill_id": {"type": "string", "description": "The UUID of the skill to export."},
                        "target_folder": {"type": "string", "description": "Path to the destination directory (created if absent)."}
                    },
                    "required": ["skill_id", "target_folder"]
                }
            },
            {
                "name": "memory_skill_promote_from_reflection",
                "description": "Promote a Reflection-kind memory into a reusable Agent Skill (closes the recursive-learning loop).",
                "docs": "v0.7.0 L2-6 (issue #671) — the closing loop. Promotes a reflection memory (memory_kind='reflection', depth ≥ namespace.governance.skill_promotion_min_depth, default 1) into a SKILL.md-format Agent Skill stored in the skills table. Each reflects_on source becomes a references/source_{i}.md resource. Frontmatter carries metadata.derived_from_reflection_id and metadata.original_reflection_depth so the lineage is preserved. The constructed skill is digest-equivalent to a hand-authored SKILL.md — promote → export → re-register produces the IDENTICAL SHA-256 digest. Refuses depth-0 reflections (no synthesised insight to promote).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "reflection_id": {"type": "string", "description": "The UUID of a Reflection-kind memory (created via memory_reflect)."},
                        "skill_name": {"type": "string", "description": "agentskills.io §3.1-compliant name: ^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$, 1-64 chars."},
                        "skill_description": {"type": "string", "description": "1-1024 char description for the promoted skill."},
                        "parameters_schema": {"type": "object", "description": "Optional JSON schema for the skill's parameters; spliced into the SKILL.md body as a Parameters section."}
                    },
                    "required": ["reflection_id", "skill_name", "skill_description"]
                }
            },
            {
                "name": "memory_skill_compositional_context",
                "description": "Get a skill's body plus reflections from the namespaces declared in its composes_with_reflections frontmatter, ranked by recency + recall_count and bounded by max_reflection_depth.",
                "docs": "v0.7.0 L2-7 (issue #672) — Compose a skill activation with the reflection memories the skill declares an affinity for via its SKILL.md `composes_with_reflections` frontmatter list. Per-entry `min_depth` filters out shallower reflections; per-namespace `max_reflection_depth` (GovernancePolicy::effective_max_reflection_depth) is the authoritative ceiling — composition CANNOT bypass the substrate's bounded-recursion guarantee. Returns body + reflections ranked by (recency + saturating recall_count); applies a `budget_tokens` cap (default 4000, max 32000) to the cumulative reflection content. Skills without a composes_with_reflections declaration return body only.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "skill_id": {"type": "string", "description": "The UUID of the skill to load with composed reflections."},
                        "budget_tokens": {"type": "integer", "minimum": 0, "description": "Optional cl100k_base token cap on the cumulative reflection content (the skill body is NOT counted). Default 4000, hard-clamped to 32000."}
                    },
                    "required": ["skill_id"]
                }
            },
            {
                "name": "memory_offload",
                "description": "Offload verbatim content into the context-offload substrate; returns a ref_id to keep in-window (Family::Power).",
                "docs": "v0.7.0 QW-3 follow-up — context-offload substrate primitive. Stores `content` verbatim in the `offloaded_blobs` substrate table under the caller's namespace (defaults to `auto`) with an optional `ttl_seconds` retention hint, and returns a `ref_id` plus the row's `content_sha256` + `stored_at` timestamp. The caller keeps the short `ref_id` in their working window; the body is dereferenced on demand via `memory_deref`. Semantic-tier+ surface (registered under Family::Power) so the keyword-tier `core` profile stays at its 7-tool minimum. Substrate-only at v0.7.0; the v0.8.0 short-term-context-compression patch wires the pair into the auto-compaction loop.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "content": {"type": "string", "description": "Verbatim content to offload. The substrate stores it as a single row; recall is exact-byte via memory_deref."},
                        "namespace": {"type": "string", "description": "Namespace bucket for the offloaded row. Defaults to `auto` so a tier-gated MCP caller that omits this field still produces a non-empty, audit-friendly bucket rather than a NULL violation."},
                        "ttl_seconds": {"type": "integer", "minimum": 0, "description": "Optional retention hint in seconds. The QW-3 substrate's TTL sweep picks this up; absence means substrate-default retention."}
                    },
                    "required": ["content"]
                }
            },
            {
                "name": "memory_deref",
                "description": "Dereference a memory_offload ref_id and return the verbatim content (Family::Power).",
                "docs": "v0.7.0 QW-3 follow-up — companion to `memory_offload`. Looks up the `ref_id` in the `offloaded_blobs` substrate, verifies the row has not been tampered with (sha256 check), and returns `{ref_id, content, stored_at, sha256}`. Refuses tampered rows. Semantic-tier+ surface (registered under Family::Power).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "ref_id": {"type": "string", "description": "The opaque reference returned by a prior memory_offload call."}
                    },
                    "required": ["ref_id"]
                }
            }
        ]
    })
}
