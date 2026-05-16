// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Data models for the Agent Skills ingestion substrate (Pillar 1.5).
//!
//! [`SkillManifest`] is the parsed, validated in-memory representation
//! of a `SKILL.md` file. [`SkillRow`] mirrors the `skills` table row
//! returned by read-side queries.

use serde::{Deserialize, Serialize};

/// v0.7.0 L2-7 (issue #672) — a single entry in the SKILL.md
/// `composes_with_reflections` frontmatter list.
///
/// Skills declare which reflection namespaces the substrate should
/// surface when the `memory_skill_compositional_context` MCP tool is
/// invoked. Each entry pins a `namespace` and a `min_depth` floor: the
/// substrate filters out reflections shallower than the floor before
/// applying the per-namespace `max_reflection_depth` ceiling
/// (`GovernancePolicy::effective_max_reflection_depth`). The ceiling is
/// authoritative — composition cannot bypass the bounded-recursion
/// guarantee documented on `GovernancePolicy::max_reflection_depth`.
///
/// The struct is round-trip-stable through JSON: registration parses it
/// out of the YAML frontmatter, embeds it under
/// `metadata.composes_with_reflections` (so older clients that don't
/// know the field see it as opaque metadata per the v0.7.0 backward-
/// compat guarantee), and `compositional_context` reads it back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComposesWithReflectionEntry {
    /// The reflection-bearing namespace (e.g. `"foo/observations"`).
    pub namespace: String,
    /// Minimum `reflection_depth` (inclusive) a memory must carry to be
    /// surfaced for this entry. `0` admits caller-minted observations
    /// (rare for a reflection-composition flow but legal); typical use
    /// is `1+` to require at least one reflection pass.
    pub min_depth: u32,
}

/// Parsed, validated SKILL.md manifest.
///
/// Produced by [`crate::parsing::skill_md::parse`] and consumed by the
/// `memory_skill_register` handler to insert into the `skills` table.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillManifest {
    /// `namespace` field from the YAML frontmatter.
    pub namespace: String,
    /// `name` field — validated against `^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$`,
    /// length 1-64.
    pub name: String,
    /// `description` — 1-1024 chars.
    pub description: String,
    /// `license` — SPDX expression or free-form text.  Optional.
    pub license: Option<String>,
    /// `compatibility` — 1-500 chars when present.  Optional.
    pub compatibility: Option<String>,
    /// `allowed_tools` — list of MCP tool names.
    pub allowed_tools: Vec<String>,
    /// v0.7.0 L2-7 (issue #672) — declared composition with reflection
    /// namespaces. Empty vector when the frontmatter omits the field
    /// (the common case for non-composing skills). The field is also
    /// duplicated into the JSON `metadata` payload for opaque-metadata
    /// readability by older clients.
    pub composes_with_reflections: Vec<ComposesWithReflectionEntry>,
    /// Extra YAML keys not explicitly mapped above, serialised to JSON.
    /// L2-7: `composes_with_reflections` is re-injected here too so
    /// pre-L2-7 readers that only consult `metadata` still observe the
    /// declaration as opaque-but-present data.
    pub metadata: serde_json::Value,
    /// Markdown body after the closing `---` fence.
    pub body: String,
}

/// A row returned from the `skills` table.
///
/// Used by `memory_skill_list` (discovery payload, no `body_blob`) and
/// `memory_skill_get` (full activation payload including decompressed
/// body).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRow {
    pub id: String,
    pub namespace: String,
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<String>,
    pub metadata: String,
    /// Hex-encoded SHA-256 digest (populated by read helpers).
    pub digest_hex: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signing_agent: Option<String>,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

/// A row from the `skill_resources` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillResourceRow {
    pub skill_id: String,
    pub resource_path: String,
    pub resource_kind: String,
    /// Hex-encoded SHA-256 digest over the decompressed content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest_hex: Option<String>,
}
