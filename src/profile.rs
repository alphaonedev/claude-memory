// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.6.4-001 — `Profile` resolution for the MCP tool surface.
//!
//! A profile is a set of tool *families* (`Family`) that the MCP server
//! advertises in its `tools/list` response. v0.6.4 collapses the default
//! surface from 43 tools (full) to 5 (core) so eager-loading harnesses
//! stop pre-paying ~6,000 input tokens of tool schemas per request. The
//! 38 tools outside `core` remain reachable via runtime expansion through
//! `memory_capabilities --include-schema family=<name>` (Track C —
//! v0.6.4-006), so no functionality is lost; only the eager prefix cost
//! goes away.
//!
//! ## Resolution order
//!
//! `CLI flag > AI_MEMORY_PROFILE env > [mcp].profile config > "core"`.
//!
//! `clap` natively handles "CLI > env" with `#[arg(env = "...")]`, so
//! the daemon-runtime side only needs to call
//! [`AppConfig::effective_profile`] with the resolved CLI/env value
//! (already merged by clap) plus the config-file value (read by
//! `serde`).
//!
//! ## Profile vocabulary
//!
//! - `core` — 7 tools, the new v0.6.4 default (v0.7 B1 added
//!   `memory_load_family`; v0.7 B2 added `memory_smart_load`).
//!   Always loaded.
//! - `graph` — adds the 11 KG/entity/replay/verify/find_paths tools. ~18 tools.
//! - `admin` — adds lifecycle (5) + governance (8). ~20 tools.
//! - `power` — adds the 8 LLM-augmented + operator tools (consolidate,
//!   auto_tag, …, plus the v0.7 K7 subscription-reliability pair).
//!   ~15 tools.
//! - `full` — every family. 51 tools (v0.6.3 baseline 43 + v0.7.0 I4 `memory_replay` + v0.7 H4 `memory_verify` + v0.7 B1 `memory_load_family` + v0.7 B2 `memory_smart_load` + v0.7 K7 `memory_subscription_replay` + `memory_subscription_dlq_list` + v0.7 J7 `memory_find_paths` + v0.7 K8 `memory_quota_status`).
//! - `custom` — comma-separated family list (`core,graph,archive` …).
//!   `core` is implicitly added if missing — there's no profile that
//!   ships *less than* the 5 core tools.
//!
//! ## Custom-profile parsing edge cases
//!
//! Documented in this RFC + pinned by unit tests:
//!
//! - empty string → `Profile::core()` (default)
//! - `core,core` → dedupe silently
//! - `core,xyz` → `ProfileParseError::UnknownFamily("xyz")` listing
//!   every valid family name
//! - mixed-case (`Core`) → `ProfileParseError::CaseMismatch`. Profiles
//!   are case-sensitive lowercase. Rejecting mixed case prevents
//!   `Profile` vs `profile` config-file divergence from creating two
//!   different surfaces in production.
//! - whitespace-only token (`core, ,graph`) → silently skipped
//! - `core,full` → `Profile::full()` (full subsumes everything; not an
//!   error)
//! - duplicates across the named-then-custom path (`full,core`) → also
//!   resolves to full.

use std::str::FromStr;

/// A tool family. Source-anchored at `src/mcp.rs::tool_definitions()`
/// 2026-05-05. Counts must sum to 51 (the v0.6.3.1 baseline of 43 +
/// v0.7.0 I4 `memory_replay` + v0.7 H4 `memory_verify` (both in
/// `Family::Graph`) + v0.7 B1 `memory_load_family` and v0.7 B2
/// `memory_smart_load` in `Family::Core` +
/// v0.7 K7 `memory_subscription_replay` and `memory_subscription_dlq_list`
/// in `Family::Power` + v0.7 J7 `memory_find_paths` in `Family::Graph` +
/// v0.7 K8 `memory_quota_status` in `Family::Power`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Family {
    /// store, recall, list, get, search, load_family, smart_load — 7
    /// (load_family added in v0.7 B1 — always-on family loader that
    /// returns the top-k recent + high-priority memories whose
    /// `metadata.family` matches one of the eight enum names;
    /// smart_load added in v0.7 B2 — intent-routed front door that
    /// picks the best Family from a free-text intent and forwards to
    /// `memory_load_family`.)
    Core,
    /// update, delete, forget, gc, promote — 5
    Lifecycle,
    /// kg_query, kg_timeline, kg_invalidate, link, get_links,
    /// entity_register, entity_get_by_alias, get_taxonomy, replay,
    /// verify, find_paths — 11 (replay added in v0.7.0 I4 — joins to the I2
    /// transcript-link substrate to reconstruct a memory's source
    /// transcript chain; verify added in v0.7 H4 — re-checks the
    /// Ed25519 signature on a stored memory_links row.)
    Graph,
    /// pending_list/approve/reject, namespace_set/get/clear_standard,
    /// subscribe, unsubscribe — 8
    Governance,
    /// consolidate, detect_contradiction, check_duplicate, auto_tag,
    /// expand_query, inbox, subscription_replay, subscription_dlq_list,
    /// quota_status — 9 (v0.7 K7 added the two
    /// operator/governance subscription-reliability tools — replay
    /// events from the audit log + inspect the DLQ; v0.7 K8 added
    /// `memory_quota_status` for the per-agent rate-limit + storage-cap
    /// substrate.)
    Power,
    /// capabilities, agent_register, agent_list, session_start, stats — 5
    Meta,
    /// archive_list, archive_purge, archive_restore, archive_stats — 4
    Archive,
    /// list_subscriptions, notify — 2
    Other,
}

/// Tool names that are loaded in every profile, regardless of which
/// families it includes. v0.6.4 reserves `memory_capabilities` as the
/// always-on bootstrap so the runtime-discovery dance works out of the
/// box on `--profile core`. Per RFC S27 and the v0.6.4-002 acceptance
/// criteria.
pub const ALWAYS_ON_TOOLS: &[&str] = &["memory_capabilities"];

impl Family {
    /// Lookup the family that owns a given tool name. Source-anchored
    /// at `src/mcp.rs::tool_definitions()` 2026-05-04. Every name listed
    /// in the v0.6.3.1 baseline is covered; `None` means the tool is
    /// either unknown to this enumeration or moved out of bounds (which
    /// should make `tool_definitions_returns_43_tools` red and force a
    /// reconciliation).
    #[must_use]
    pub fn for_tool(name: &str) -> Option<Self> {
        match name {
            // core (7 — v0.7 B1 added memory_load_family as the always-on
            // alternative to memory_recall when the agent already knows
            // which family taxonomy it wants; v0.7 B2 added
            // memory_smart_load as the intent-routed front door that
            // picks the best family for the caller).
            "memory_store" | "memory_recall" | "memory_list" | "memory_get" | "memory_search"
            | "memory_load_family" | "memory_smart_load" => Some(Self::Core),
            // lifecycle (5)
            "memory_update" | "memory_delete" | "memory_forget" | "memory_gc"
            | "memory_promote" => Some(Self::Lifecycle),
            // graph (11 — v0.7.0 I4 added memory_replay; v0.7 H4 added memory_verify;
            // v0.7 J7 added memory_find_paths)
            "memory_kg_query"
            | "memory_kg_timeline"
            | "memory_kg_invalidate"
            | "memory_link"
            | "memory_get_links"
            | "memory_entity_register"
            | "memory_entity_get_by_alias"
            | "memory_get_taxonomy"
            | "memory_replay"
            | "memory_verify"
            | "memory_find_paths" => Some(Self::Graph),
            // governance (8)
            "memory_pending_list"
            | "memory_pending_approve"
            | "memory_pending_reject"
            | "memory_namespace_set_standard"
            | "memory_namespace_get_standard"
            | "memory_namespace_clear_standard"
            | "memory_subscribe"
            | "memory_unsubscribe" => Some(Self::Governance),
            // power (10 — v0.7 K7 added the subscription-reliability pair:
            // `memory_subscription_replay` + `memory_subscription_dlq_list`;
            // v0.7 K8 added `memory_quota_status` for the per-agent quota
            // substrate; v0.7.0 Task 4/8 added `memory_reflect` — the
            // substrate-native recursive-learning primitive. All
            // operator/governance, not data-plane.)
            "memory_consolidate"
            | "memory_detect_contradiction"
            | "memory_check_duplicate"
            | "memory_auto_tag"
            | "memory_expand_query"
            | "memory_inbox"
            | "memory_subscription_replay"
            | "memory_subscription_dlq_list"
            | "memory_quota_status"
            | "memory_reflect"
            | "memory_reflection_origin"
            // v0.7.0 (issue #691) — substrate-level agent-action rules
            // engine. Both tools live in Family::Power (governance /
            // operator-facing, not data-plane). Mutation tools are
            // explicitly NOT registered over MCP per design revision
            // 2026-05-13 — operator uses CLI / HTTP with signed key.
            | "memory_check_agent_action"
            | "memory_rule_list" => Some(Self::Power),
            // meta (5)
            "memory_capabilities"
            | "memory_agent_register"
            | "memory_agent_list"
            | "memory_session_start"
            | "memory_stats" => Some(Self::Meta),
            // archive (4)
            "memory_archive_list"
            | "memory_archive_purge"
            | "memory_archive_restore"
            | "memory_archive_stats" => Some(Self::Archive),
            // other (2)
            "memory_list_subscriptions" | "memory_notify" => Some(Self::Other),
            _ => None,
        }
    }

    /// Lowercase canonical name as used in CLI/env/config.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Lifecycle => "lifecycle",
            Self::Graph => "graph",
            Self::Governance => "governance",
            Self::Power => "power",
            Self::Meta => "meta",
            Self::Archive => "archive",
            Self::Other => "other",
        }
    }

    /// All eight families in declaration order. Useful for `--profile full`
    /// and for the `ProfileParseError::UnknownFamily` diagnostic.
    #[must_use]
    pub const fn all() -> &'static [Family] {
        &[
            Self::Core,
            Self::Lifecycle,
            Self::Graph,
            Self::Governance,
            Self::Power,
            Self::Meta,
            Self::Archive,
            Self::Other,
        ]
    }

    /// Expected tool count for this family. v0.6.4-002 will assert
    /// that the actual `register_<family>` matches this constant.
    #[must_use]
    pub const fn expected_tool_count(self) -> usize {
        match self {
            // Core: 5 baseline + memory_load_family (v0.7 B1) +
            // memory_smart_load (v0.7 B2) = 7.
            Self::Core => 7,
            Self::Lifecycle | Self::Meta => 5,
            // Graph: 8 baseline + memory_replay (v0.7.0 I4) + memory_verify (v0.7 H4) +
            // memory_find_paths (v0.7 J7) = 11.
            Self::Graph => 11,
            Self::Governance => 8,
            // Power: 6 baseline + 2 (v0.7 K7) + 1 (v0.7 K8 quota_status) +
            // 1 (v0.7.0 Task 4/8 — memory_reflect substrate primitive) +
            // 1 (v0.7.0 L2-2 / S6-M1 — memory_reflection_origin) +
            // 2 (v0.7.0 issue #691 — memory_check_agent_action +
            // memory_rule_list, substrate-level agent-action rules) = 13.
            Self::Power => 13,
            Self::Archive => 4,
            Self::Other => 2,
        }
    }

    /// v0.7.0 A2 — tool names belonging to this family. Forward of the
    /// `Family::for_tool` reverse map; source-anchored at
    /// `src/mcp.rs::tool_definitions()` 2026-05-04 (same anchor as
    /// [`Family::for_tool`] and [`Family::expected_tool_count`]).
    /// Order is the order each tool appears in
    /// `tool_definitions_for_profile`'s registration walk, so an
    /// LLM-facing preview ("the first three tools loaded") aligns with
    /// the actual `tools/list` output.
    ///
    /// The slice length must match [`Family::expected_tool_count`]; the
    /// `family_tool_names_match_expected_count` unit test pins both in
    /// sync.
    #[must_use]
    pub const fn tool_names(self) -> &'static [&'static str] {
        match self {
            Self::Core => &[
                "memory_store",
                "memory_recall",
                "memory_list",
                "memory_get",
                "memory_search",
                // v0.7 B1 — always-on alternative to memory_recall when
                // the agent already knows the Family taxonomy it wants.
                "memory_load_family",
                // v0.7 B2 — intent-routed front door. Caller passes a
                // free-text intent; the handler picks the best family
                // from the cached descriptors and forwards to
                // `memory_load_family`.
                "memory_smart_load",
            ],
            Self::Lifecycle => &[
                "memory_update",
                "memory_delete",
                "memory_forget",
                "memory_gc",
                "memory_promote",
            ],
            Self::Graph => &[
                "memory_kg_query",
                "memory_kg_timeline",
                "memory_kg_invalidate",
                "memory_link",
                "memory_get_links",
                "memory_entity_register",
                "memory_entity_get_by_alias",
                "memory_get_taxonomy",
                // v0.7.0 I4 — traverses memory_transcript_links (I2) to
                // reconstruct the source-transcript chain for a memory.
                "memory_replay",
                // v0.7 H4 — re-verifies a stored link's Ed25519
                // signature on demand, returning attest_level.
                "memory_verify",
                // v0.7 J7 — enumerate up to N paths between two memories
                // (BFS with cycle detection over memory_links).
                "memory_find_paths",
            ],
            Self::Governance => &[
                "memory_pending_list",
                "memory_pending_approve",
                "memory_pending_reject",
                "memory_namespace_set_standard",
                "memory_namespace_get_standard",
                "memory_namespace_clear_standard",
                "memory_subscribe",
                "memory_unsubscribe",
            ],
            Self::Power => &[
                "memory_consolidate",
                "memory_detect_contradiction",
                "memory_check_duplicate",
                "memory_auto_tag",
                "memory_expand_query",
                "memory_inbox",
                // v0.7 K7 — operator/governance subscription-reliability
                // tools. Replay reads back the audit row series for one
                // subscription since an RFC3339 cursor; dlq_list inspects
                // payloads that exhausted the [200ms, 1s, 5s] retry ladder.
                "memory_subscription_replay",
                "memory_subscription_dlq_list",
                // v0.7 K8 — per-agent quota status (memories/day, storage
                // bytes, links/day). Operator-facing inspector for the K8
                // rate-limit substrate.
                "memory_quota_status",
                // v0.7.0 Task 4/8 (recursive learning, issue #655) —
                // substrate-native reflection primitive. Inserts a
                // reflection memory plus N `reflects_on` provenance
                // links in a single atomic transaction.
                "memory_reflect",
                // v0.7.0 L2-2 (S6-M1) — cross-peer reflection origin
                // inspector. Returns peer_origin / signing_agent /
                // original_depth / local_depth_at_arrival for a row.
                "memory_reflection_origin",
                // v0.7.0 (issue #691) — substrate-level agent-action
                // rules engine. Read-side surface; mutation tools are
                // NOT registered over MCP (operator uses CLI / HTTP).
                "memory_check_agent_action",
                "memory_rule_list",
            ],
            Self::Meta => &[
                "memory_capabilities",
                "memory_agent_register",
                "memory_agent_list",
                "memory_session_start",
                "memory_stats",
            ],
            Self::Archive => &[
                "memory_archive_list",
                "memory_archive_purge",
                "memory_archive_restore",
                "memory_archive_stats",
            ],
            Self::Other => &["memory_list_subscriptions", "memory_notify"],
        }
    }
}

impl FromStr for Family {
    type Err = ProfileParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Reject mixed case explicitly. Lowercase form below.
        if s.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(ProfileParseError::CaseMismatch(s.to_string()));
        }
        match s {
            "core" => Ok(Self::Core),
            "lifecycle" => Ok(Self::Lifecycle),
            "graph" => Ok(Self::Graph),
            "governance" => Ok(Self::Governance),
            "power" => Ok(Self::Power),
            "meta" => Ok(Self::Meta),
            "archive" => Ok(Self::Archive),
            "other" => Ok(Self::Other),
            unknown => Err(ProfileParseError::UnknownFamily(unknown.to_string())),
        }
    }
}

/// A resolved tool profile — the set of families to register on the
/// MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    families: Vec<Family>,
}

impl Profile {
    /// `core` — 7 tools (`store, recall, list, get, search,
    /// load_family, smart_load`). The new v0.6.4 default; v0.7 B1
    /// added `memory_load_family` as the always-on family loader and
    /// v0.7 B2 added `memory_smart_load` as the intent-routed front
    /// door. Registers exactly the `Core` family.
    ///
    /// **Design note (v0.6.4-002 hook):** `memory_capabilities` is
    /// **always-on** regardless of profile per RFC scenario S27. It is
    /// NOT in this family list because the registration filter
    /// (v0.6.4-002) injects it as a bootstrap tool outside the
    /// profile-driven path. That keeps the "core profile = 5 tools"
    /// claim accurate while still making the runtime-discovery dance
    /// reachable.
    #[must_use]
    pub fn core() -> Self {
        Self {
            families: vec![Family::Core],
        }
    }

    /// `graph` — core + graph. 18 tools (v0.7.0 I4 added `memory_replay`;
    /// v0.7 H4 added `memory_verify`; v0.7 B1 added `memory_load_family`
    /// to core; v0.7 B2 added `memory_smart_load` to core; v0.7 J7
    /// added `memory_find_paths`).
    #[must_use]
    pub fn graph() -> Self {
        Self {
            families: vec![Family::Core, Family::Graph],
        }
    }

    /// `admin` — core + lifecycle + governance. 20 tools (v0.7 B1
    /// added `memory_load_family` to core; v0.7 B2 added
    /// `memory_smart_load` to core).
    #[must_use]
    pub fn admin() -> Self {
        Self {
            families: vec![Family::Core, Family::Lifecycle, Family::Governance],
        }
    }

    /// `power` — core + power. 15 tools (v0.7 B1 added
    /// `memory_load_family` to core; v0.7 B2 added `memory_smart_load`
    /// to core; v0.7 K7 added the two subscription-reliability tools
    /// to `Family::Power`).
    #[must_use]
    pub fn power() -> Self {
        Self {
            families: vec![Family::Core, Family::Power],
        }
    }

    /// `full` — every family. 51 tools (v0.6.3 baseline 43 + v0.7.0 I4
    /// `memory_replay` + v0.7 H4 `memory_verify` + v0.7 B1
    /// `memory_load_family` + v0.7 B2 `memory_smart_load` + v0.7 K7
    /// `memory_subscription_replay` + `memory_subscription_dlq_list` +
    /// v0.7 J7 `memory_find_paths` + v0.7 K8 `memory_quota_status`).
    #[must_use]
    pub fn full() -> Self {
        Self {
            families: Family::all().to_vec(),
        }
    }

    /// Family list, sorted in declaration order, deduplicated.
    #[must_use]
    pub fn families(&self) -> &[Family] {
        &self.families
    }

    /// `true` if this profile would register tools from `family`.
    #[must_use]
    pub fn includes(&self, family: Family) -> bool {
        self.families.contains(&family)
    }

    /// Sum of expected tool counts. v0.6.4-002 will assert that the
    /// runtime registration matches.
    #[must_use]
    pub fn expected_tool_count(&self) -> usize {
        self.families.iter().map(|f| f.expected_tool_count()).sum()
    }

    /// `true` if a tool with this name is loaded under this profile.
    /// Treats every name in [`ALWAYS_ON_TOOLS`] as loaded regardless of
    /// the family map (per RFC S27 — `memory_capabilities` is the
    /// bootstrap tool for runtime discovery).
    #[must_use]
    pub fn loads(&self, tool_name: &str) -> bool {
        if ALWAYS_ON_TOOLS.contains(&tool_name) {
            return true;
        }
        Family::for_tool(tool_name).is_some_and(|f| self.includes(f))
    }

    /// Parse a profile name. Accepts the named profiles plus
    /// comma-separated family lists. Empty or whitespace-only input
    /// resolves to [`Profile::core`]. See module docs for full edge-case
    /// matrix.
    ///
    /// # Errors
    ///
    /// - [`ProfileParseError::UnknownFamily`] if a comma-separated
    ///   token is neither a known profile nor a known family.
    /// - [`ProfileParseError::CaseMismatch`] if any token contains an
    ///   uppercase letter.
    pub fn parse(s: &str) -> Result<Self, ProfileParseError> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Ok(Self::core());
        }

        // Reject mixed case at the whole-string level so `Core` doesn't
        // sneak past as a family (Family::from_str would also catch it,
        // but the diagnostic is clearer here).
        if trimmed.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(ProfileParseError::CaseMismatch(trimmed.to_string()));
        }

        // Single named profile?
        match trimmed {
            "core" => return Ok(Self::core()),
            "graph" => return Ok(Self::graph()),
            "admin" => return Ok(Self::admin()),
            "power" => return Ok(Self::power()),
            "full" => return Ok(Self::full()),
            _ => {}
        }

        // Comma-separated. Could mix profile names and family names.
        // `core,graph` registers core+meta (from `core`) plus graph
        // (from the family). `core,full` is full because full subsumes.
        let mut families = Vec::with_capacity(8);
        for raw_token in trimmed.split(',') {
            let token = raw_token.trim();
            if token.is_empty() {
                continue;
            }
            // Each token is either a profile or a family.
            match token {
                "core" => merge(&mut families, Self::core().families()),
                "graph" => merge(&mut families, Self::graph().families()),
                "admin" => merge(&mut families, Self::admin().families()),
                "power" => merge(&mut families, Self::power().families()),
                "full" => return Ok(Self::full()),
                _ => {
                    let f = Family::from_str(token)?;
                    if !families.contains(&f) {
                        families.push(f);
                    }
                }
            }
        }

        // Every profile implicitly includes `core` — there is no
        // legitimate use case for a profile smaller than the 5
        // core tools.
        if !families.contains(&Family::Core) {
            families.insert(0, Family::Core);
        }

        // Sort into declaration order so two equivalent profile
        // strings (`graph,core` vs `core,graph`) resolve to the same
        // value.
        families.sort_unstable();
        families.dedup();

        Ok(Self { families })
    }
}

impl Default for Profile {
    fn default() -> Self {
        Self::core()
    }
}

fn merge(dst: &mut Vec<Family>, src: &[Family]) {
    for f in src {
        if !dst.contains(f) {
            dst.push(*f);
        }
    }
}

/// Errors produced by [`Profile::parse`] / [`Family::from_str`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileParseError {
    /// A custom-profile token was neither a known profile nor a family.
    UnknownFamily(String),
    /// A token contained an uppercase letter. Profile vocabulary is
    /// case-sensitive lowercase.
    CaseMismatch(String),
}

impl std::fmt::Display for ProfileParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownFamily(name) => {
                let valid: Vec<&str> = Family::all().iter().map(|f| f.name()).collect();
                let profiles = "core, graph, admin, power, full";
                write!(
                    f,
                    "unknown profile or family '{name}'. \
                     Valid profiles: {profiles}. \
                     Valid families: {valid}.",
                    valid = valid.join(", ")
                )
            }
            Self::CaseMismatch(s) => {
                write!(
                    f,
                    "profile '{s}' contains uppercase letters; \
                     profile vocabulary is case-sensitive lowercase \
                     (e.g. 'core', not 'Core')"
                )
            }
        }
    }
}

impl std::error::Error for ProfileParseError {}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- Family ----------

    #[test]
    fn family_all_has_eight_entries() {
        assert_eq!(Family::all().len(), 8);
    }

    #[test]
    fn family_expected_tool_counts_sum_to_51() {
        let total: usize = Family::all().iter().map(|f| f.expected_tool_count()).sum();
        assert_eq!(
            total, 55,
            "v0.6.3.1 baseline (43) + v0.7.0 I4 `memory_replay` + v0.7 H4 \
             `memory_verify` + v0.7 B1 `memory_load_family` + v0.7 B2 \
             `memory_smart_load` + v0.7 K7 `memory_subscription_replay` \
             + `memory_subscription_dlq_list` + v0.7 J7 `memory_find_paths` \
             + v0.7 K8 `memory_quota_status` + v0.7.0 Task 4/8 \
             `memory_reflect` + v0.7.0 L2-2 `memory_reflection_origin` + \
             v0.7.0 issue #691 `memory_check_agent_action` + \
             `memory_rule_list` = 55. If this drifts, update \
             Family::expected_tool_count and the family map docs together."
        );
    }

    #[test]
    fn family_from_str_lowercase_canonical() {
        assert_eq!(Family::from_str("core").unwrap(), Family::Core);
        assert_eq!(Family::from_str("meta").unwrap(), Family::Meta);
        assert_eq!(Family::from_str("graph").unwrap(), Family::Graph);
    }

    #[test]
    fn family_from_str_rejects_mixed_case() {
        assert!(matches!(
            Family::from_str("Core"),
            Err(ProfileParseError::CaseMismatch(_))
        ));
        assert!(matches!(
            Family::from_str("CORE"),
            Err(ProfileParseError::CaseMismatch(_))
        ));
    }

    #[test]
    fn family_from_str_unknown_returns_diagnostic() {
        let err = Family::from_str("xyz").unwrap_err();
        match err {
            ProfileParseError::UnknownFamily(s) => assert_eq!(s, "xyz"),
            _ => panic!("expected UnknownFamily, got {err:?}"),
        }
    }

    // ---------- Profile named ----------

    #[test]
    fn profile_core_has_seven_tools() {
        let p = Profile::core();
        // v0.7 B1 — Core ships memory_load_family; v0.7 B2 — Core
        // ships memory_smart_load. Total 7 (5 baseline + 2 always-on
        // discovery tools).
        assert_eq!(p.expected_tool_count(), 7);
        assert!(p.includes(Family::Core));
        // meta is NOT in core's family list — `memory_capabilities`
        // is bootstrapped separately as always-on per RFC S27. The
        // other meta tools (agent_register/list/session_start/stats)
        // are NOT advertised by the core profile.
        assert!(!p.includes(Family::Meta));
        assert!(!p.includes(Family::Lifecycle));
    }

    #[test]
    fn profile_graph_has_eighteen_tools() {
        let p = Profile::graph();
        // v0.7 J7 — Graph now ships 11 tools (8 baseline + memory_replay
        // [I4] + memory_verify [H4] + memory_find_paths [J7]); v0.7 B1
        // added memory_load_family to core; v0.7 B2 added
        // memory_smart_load to core (7 instead of 5).
        assert_eq!(p.expected_tool_count(), 7 + 11);
        assert!(p.includes(Family::Graph));
    }

    #[test]
    fn profile_admin_has_twenty_tools() {
        let p = Profile::admin();
        // admin = core (7, with v0.7 B1 memory_load_family + v0.7 B2
        // memory_smart_load) + lifecycle (5) + governance (8) = 20.
        // Graph isn't in admin so the v0.7.0 I4 memory_replay addition
        // doesn't change this count.
        assert_eq!(p.expected_tool_count(), 7 + 5 + 8);
    }

    #[test]
    fn profile_power_has_sixteen_tools() {
        let p = Profile::power();
        // v0.7 B1 + v0.7 B2 — Core now ships 7 tools (was 5).
        // v0.7 K7 — Power got the subscription-reliability pair (+2 → 8).
        // v0.7 K8 — Power got memory_quota_status (+1 → 9).
        // v0.7.0 Task 4/8 — Power got memory_reflect (+1 → 10).
        // v0.7.0 L2-2 — Power got memory_reflection_origin (+1 → 11).
        // v0.7.0 issue #691 — Power got memory_check_agent_action +
        // memory_rule_list (+2 → 13).
        assert_eq!(p.expected_tool_count(), 7 + 13);
    }

    #[test]
    fn profile_full_has_fifty_one_tools() {
        let p = Profile::full();
        // v0.7.0 issue #691 (post-L2-2) — full surface = 43 baseline +
        // memory_replay (I4) + memory_verify (H4) + memory_load_family (B1)
        // + memory_smart_load (B2) + memory_subscription_replay (K7) +
        // memory_subscription_dlq_list (K7) + memory_find_paths (J7) +
        // memory_quota_status (K8) + memory_reflect (Task 4/8) +
        // memory_reflection_origin (L2-2) + memory_check_agent_action +
        // memory_rule_list (#691) = 55.
        assert_eq!(p.expected_tool_count(), 55);

        // The K7+K8 + Task 4/8 + L2-2 + #691 additions live in
        // Family::Power (operator/governance), so the `power` profile
        // picks them up too.
        assert_eq!(Profile::power().expected_tool_count(), 7 + 13);
    }

    // ---------- Profile::parse ----------

    #[test]
    fn parse_empty_returns_core() {
        assert_eq!(Profile::parse("").unwrap(), Profile::core());
        assert_eq!(Profile::parse("   ").unwrap(), Profile::core());
    }

    #[test]
    fn parse_named_profiles() {
        assert_eq!(Profile::parse("core").unwrap(), Profile::core());
        assert_eq!(Profile::parse("graph").unwrap(), Profile::graph());
        assert_eq!(Profile::parse("admin").unwrap(), Profile::admin());
        assert_eq!(Profile::parse("power").unwrap(), Profile::power());
        assert_eq!(Profile::parse("full").unwrap(), Profile::full());
    }

    #[test]
    fn parse_custom_comma_list_dedup() {
        // `core,graph` → core (7, after v0.7 B1 + B2) + graph (11,
        // after v0.7 J7) = 18 tools. Meta is NOT included —
        // `memory_capabilities` is always-on bootstrapped outside the
        // family map (v0.6.4-002).
        let p = Profile::parse("core,graph").unwrap();
        assert!(p.includes(Family::Core));
        assert!(!p.includes(Family::Meta));
        assert!(p.includes(Family::Graph));
        assert_eq!(p.expected_tool_count(), 18);
    }

    #[test]
    fn parse_custom_dedupes_repeated_token() {
        let p = Profile::parse("core,core").unwrap();
        assert_eq!(p, Profile::core());
    }

    #[test]
    fn parse_custom_with_full_subsumes() {
        let p = Profile::parse("graph,full").unwrap();
        assert_eq!(p, Profile::full());
    }

    #[test]
    fn parse_custom_implicitly_includes_core() {
        // Asking for just `archive` should still load core because
        // there is no legitimate profile smaller than the 5 core tools.
        let p = Profile::parse("archive").unwrap();
        assert!(p.includes(Family::Core));
        assert!(p.includes(Family::Archive));
    }

    #[test]
    fn parse_custom_unknown_family_errors() {
        let err = Profile::parse("core,xyz").unwrap_err();
        match err {
            ProfileParseError::UnknownFamily(s) => assert_eq!(s, "xyz"),
            _ => panic!("expected UnknownFamily, got {err:?}"),
        }
    }

    #[test]
    fn parse_rejects_mixed_case() {
        assert!(matches!(
            Profile::parse("Core"),
            Err(ProfileParseError::CaseMismatch(_))
        ));
        assert!(matches!(
            Profile::parse("core,Graph"),
            Err(ProfileParseError::CaseMismatch(_))
        ));
    }

    #[test]
    fn parse_skips_whitespace_only_tokens() {
        // `core, ,graph` should resolve to graph not error.
        let p = Profile::parse("core, ,graph").unwrap();
        assert_eq!(p, Profile::graph());
    }

    #[test]
    fn parse_order_independence() {
        // `graph,core` resolves identically to `core,graph`.
        let a = Profile::parse("core,graph").unwrap();
        let b = Profile::parse("graph,core").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn parse_diagnostic_error_lists_valid_options() {
        let err = Profile::parse("xyz").unwrap_err();
        let msg = err.to_string();
        // The diagnostic must mention the valid profiles and families
        // so a confused operator can self-correct.
        assert!(msg.contains("core"));
        assert!(msg.contains("graph"));
        assert!(msg.contains("full"));
        assert!(msg.contains("xyz"));
    }

    #[test]
    fn default_is_core() {
        assert_eq!(Profile::default(), Profile::core());
    }

    // ---------- Tool name → family / loads ----------

    #[test]
    fn family_for_tool_resolves_every_baseline_name() {
        // Source-anchored at src/mcp.rs::tool_definitions() — if any
        // tool here is missing from `for_tool`, the family map is
        // out of sync and `--profile <family>` would silently miss it.
        let baseline = [
            // core
            "memory_store",
            "memory_recall",
            "memory_list",
            "memory_get",
            "memory_search",
            // core (v0.7 B1 addition)
            "memory_load_family",
            // core (v0.7 B2 addition)
            "memory_smart_load",
            // lifecycle
            "memory_update",
            "memory_delete",
            "memory_forget",
            "memory_gc",
            "memory_promote",
            // graph
            "memory_kg_query",
            "memory_kg_timeline",
            "memory_kg_invalidate",
            "memory_link",
            "memory_get_links",
            "memory_entity_register",
            "memory_entity_get_by_alias",
            "memory_get_taxonomy",
            // graph (v0.7.0 I4 addition)
            "memory_replay",
            // graph (v0.7 H4 addition)
            "memory_verify",
            // graph (v0.7 J7 addition)
            "memory_find_paths",
            // governance
            "memory_pending_list",
            "memory_pending_approve",
            "memory_pending_reject",
            "memory_namespace_set_standard",
            "memory_namespace_get_standard",
            "memory_namespace_clear_standard",
            "memory_subscribe",
            "memory_unsubscribe",
            // power
            "memory_consolidate",
            "memory_detect_contradiction",
            "memory_check_duplicate",
            "memory_auto_tag",
            "memory_expand_query",
            "memory_inbox",
            // power (v0.7 K7 additions — subscription reliability)
            "memory_subscription_replay",
            "memory_subscription_dlq_list",
            // power (v0.7 K8 addition — per-agent quota status)
            "memory_quota_status",
            // meta
            "memory_capabilities",
            "memory_agent_register",
            "memory_agent_list",
            "memory_session_start",
            "memory_stats",
            // archive
            "memory_archive_list",
            "memory_archive_purge",
            "memory_archive_restore",
            "memory_archive_stats",
            // other
            "memory_list_subscriptions",
            "memory_notify",
        ];
        assert_eq!(
            baseline.len(),
            51,
            "baseline list = 43 (v0.6.3.1) + 1 (v0.7.0 I4 memory_replay) + \
             1 (v0.7 H4 memory_verify) + 1 (v0.7 B1 memory_load_family) + \
             1 (v0.7 B2 memory_smart_load) + \
             2 (v0.7 K7 memory_subscription_replay + memory_subscription_dlq_list) + \
             1 (v0.7 J7 memory_find_paths) + 1 (v0.7 K8 memory_quota_status) = 51"
        );
        for name in baseline {
            assert!(
                Family::for_tool(name).is_some(),
                "Family::for_tool({name}) returned None — update the family map"
            );
        }
    }

    #[test]
    fn family_for_tool_returns_none_for_unknown() {
        assert!(Family::for_tool("memory_does_not_exist").is_none());
        assert!(Family::for_tool("").is_none());
    }

    #[test]
    fn loads_includes_core_tools_under_core_profile() {
        let p = Profile::core();
        assert!(p.loads("memory_store"));
        assert!(p.loads("memory_recall"));
        assert!(!p.loads("memory_kg_query"));
        // memory_capabilities is always-on bootstrap.
        assert!(p.loads("memory_capabilities"));
    }

    #[test]
    fn loads_full_profile_includes_every_tool() {
        let p = Profile::full();
        // Every tool in the baseline must load under full.
        for name in [
            "memory_store",
            "memory_kg_query",
            "memory_consolidate",
            "memory_archive_list",
            "memory_notify",
            "memory_capabilities",
        ] {
            assert!(p.loads(name), "full profile should load {name}");
        }
    }

    #[test]
    fn loads_unknown_tool_returns_false() {
        let p = Profile::full();
        assert!(!p.loads("memory_does_not_exist"));
    }

    #[test]
    fn always_on_tools_loaded_in_every_profile() {
        for p in [
            Profile::core(),
            Profile::graph(),
            Profile::admin(),
            Profile::power(),
            Profile::full(),
        ] {
            for name in ALWAYS_ON_TOOLS {
                assert!(p.loads(name), "{name} must load in every profile");
            }
        }
    }
}
