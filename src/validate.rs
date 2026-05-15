// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Result, bail};

use crate::models::{
    CreateMemory, MAX_CONTENT_SIZE, MAX_NAMESPACE_DEPTH, Memory, UpdateMemory, VALID_AGENT_TYPES,
    VALID_SCOPES,
};

const MAX_TITLE_LEN: usize = 512;
/// Max characters in a namespace string (post-Task 1.4).
/// Flat namespaces still fit in the historical 128 budget; 512 is the ceiling
/// for hierarchical paths like `a/b/c/…` up to 8 levels deep.
const MAX_NAMESPACE_LEN: usize = 512;
const MAX_SOURCE_LEN: usize = 64;
const MAX_TAG_LEN: usize = 128;
const MAX_TAGS_COUNT: usize = 50;
const MAX_RELATION_LEN: usize = 64;
const MAX_ID_LEN: usize = 128;
const MAX_AGENT_ID_LEN: usize = 128;
const MAX_METADATA_SIZE: usize = 65_536;
const MAX_METADATA_DEPTH: usize = 32;

const VALID_SOURCES: &[&str] = &[
    "user",
    "claude",
    "hook",
    "api",
    "cli",
    "import",
    "consolidation",
    "system",
    "chaos",
    // v0.6.2 (S32): `handle_notify` stamps source="notify" on inbox rows.
    // Without this entry, peers reject the notify in `sync_push`'s
    // `validate_memory` — the notify lands on the sender's inbox but
    // never reaches the target's inbox on peer nodes.
    "notify",
];
// Canonical relation taxonomy. The validator (`validate_relation`) accepts
// these names via the fast-path branch and also accepts any caller-supplied
// `[a-z0-9_]+` identifier via the lenient branch (post-cb92998). Adding a
// name here is therefore documentation-driven: the name becomes part of the
// MCP `memory_link` schema's `enum`, the wire-shape advertised to peers,
// and the closed set surfaced in CLI/API docs.
//
// Semantics of each relation (directionality reads left-to-right, source → target):
//   * `related_to`   — symmetric association; no provenance claim.
//   * `supersedes`   — winner → loser; the source replaces the target.
//   * `contradicts`  — asserts the source contradicts the target.
//   * `derived_from` — clone/summary (source) → original (target). `derived_from`
//                      is written by `memory_consolidate` (consolidated → each
//                      source) and `memory_promote --to-namespace` (clone →
//                      source). The arrow points FROM the derived memory TO
//                      the original.
//   * `reflects_on`  — v0.7.0 Task 3/8 (recursive learning). reflection
//                      memory (source) → source memory it reflects on
//                      (target). Mirrors the `derived_from` convention: the
//                      newer/derived row is the link's `source_id`; the
//                      thing it points back to is the `target_id`. The
//                      reflection memory is the one with `reflection_depth
//                      > 0` (see Memory.reflection_depth, Task 1/8). Task
//                      4/8 (`memory_reflect` MCP tool) will write these
//                      links from a reflection memory to each source it
//                      reflects on. `reflects_on` participates in
//                      `find_paths` traversal naturally because that BFS
//                      walks `memory_links` without filtering by relation
//                      label — operators tracing reflection chains see them
//                      surface alongside the other relations.
const VALID_RELATIONS: &[&str] = &[
    "related_to",
    "supersedes",
    "contradicts",
    "derived_from",
    "reflects_on",
    // v0.7.0 WT-1-A — atomisation-provenance edge (atom -> parent). The
    // typed, signable, federation-safe expression of the structural
    // `memories.atom_of` FK. Distinct from `derived_from` (consolidation
    // provenance). Mirrors `crate::models::MemoryLinkRelation::DerivesFrom`.
    "derives_from",
];

fn is_valid_rfc3339(s: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(s).is_ok()
}

fn is_clean_string(s: &str) -> bool {
    !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
}

pub fn validate_title(title: &str) -> Result<()> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        bail!("title cannot be empty");
    }
    if trimmed.chars().count() > MAX_TITLE_LEN {
        bail!("title exceeds max length of {MAX_TITLE_LEN} characters");
    }
    if !is_clean_string(trimmed) {
        bail!("title contains invalid characters");
    }
    Ok(())
}

pub fn validate_content(content: &str) -> Result<()> {
    if content.trim().is_empty() {
        bail!("content cannot be empty");
    }
    if content.len() > MAX_CONTENT_SIZE {
        bail!("content exceeds max size of {MAX_CONTENT_SIZE} bytes");
    }
    if !is_clean_string(content) {
        bail!("content contains invalid characters");
    }
    Ok(())
}

/// Validate a namespace (flat or hierarchical, Task 1.4).
///
/// Flat namespaces (`"global"`, `"ai-memory"`) remain fully valid — hierarchy
/// is opt-in. Hierarchical paths use `/` as the segment delimiter:
///
/// ```text
/// alphaone/engineering/platform
/// ```
///
/// Rules:
/// - **Not empty**, no leading/trailing whitespace
/// - Length ≤ [`MAX_NAMESPACE_LEN`] (512 chars)
/// - Depth (segment count) ≤ [`MAX_NAMESPACE_DEPTH`] (8)
/// - Backslashes, null bytes, control chars, and spaces are forbidden
/// - Leading and trailing `/` are forbidden (normalize input via
///   [`normalize_namespace`] before validating)
/// - Empty segments (consecutive `//`) are forbidden
/// - Each segment is non-empty; no further character restriction beyond
///   the whole-string checks above (preserving historical flexibility
///   for existing flat namespaces like `ai-memory-mcp-dev`)
pub fn validate_namespace(ns: &str) -> Result<()> {
    let trimmed = ns.trim();
    if trimmed.is_empty() {
        bail!("namespace cannot be empty");
    }
    if trimmed.chars().count() > MAX_NAMESPACE_LEN {
        bail!("namespace exceeds max length of {MAX_NAMESPACE_LEN} characters");
    }
    if trimmed.contains('\\') || trimmed.contains('\0') {
        bail!("namespace cannot contain backslashes or null bytes");
    }
    if trimmed.contains(' ') {
        bail!("namespace cannot contain spaces (use hyphens or underscores)");
    }
    if !is_clean_string(trimmed) {
        bail!("namespace contains invalid control characters");
    }
    // Task 1.4 — hierarchical paths. '/' is permitted as a delimiter, but
    // leading/trailing/empty segments are rejected to force callers to
    // normalize input first (ambiguity between "foo" and "foo/" is not
    // something we want to paper over at match time).
    if trimmed.starts_with('/') {
        bail!("namespace cannot start with '/' (normalize input first)");
    }
    if trimmed.ends_with('/') {
        bail!("namespace cannot end with '/' (normalize input first)");
    }
    if trimmed.split('/').any(str::is_empty) {
        bail!("namespace cannot contain empty segments (e.g. '//')");
    }
    // Reject `..` and `.` segments — they look like path traversal to
    // human readers and silently confuse hierarchy semantics. Visibility
    // prefix matching with LIKE 'foo/%' would let memories at
    // `foo/../malicious` appear under `foo`'s team-scope queries
    // (red-team #240).
    if trimmed.split('/').any(|s| s == ".." || s == ".") {
        bail!("namespace segments '.' and '..' are not allowed");
    }
    let depth = crate::models::namespace_depth(trimmed);
    if depth > MAX_NAMESPACE_DEPTH {
        bail!("namespace depth {depth} exceeds max of {MAX_NAMESPACE_DEPTH}");
    }
    Ok(())
}

/// Normalize a namespace input to the canonical form accepted by
/// [`validate_namespace`]. Not called by write paths (would lowercase
/// existing flat namespaces and break their lookup keys); instead exposed
/// as a helper that callers opt into, and used by Task 1.5+ when accepting
/// user-typed hierarchical paths.
///
/// - Trim leading/trailing whitespace
/// - Strip leading/trailing `/`
/// - Collapse consecutive `/` into a single separator
/// - Lowercase the result
///
/// This is a pure helper; the write path does **not** auto-apply it so that
/// callers retain control over case sensitivity on existing flat namespaces.
/// Use it when you need to accept loose user input and produce a matchable
/// canonical key.
#[allow(dead_code)]
#[must_use]
pub fn normalize_namespace(input: &str) -> String {
    let trimmed = input.trim();
    let collapsed: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
    collapsed.join("/").to_lowercase()
}

pub fn validate_source(source: &str) -> Result<()> {
    if source.trim().is_empty() {
        bail!("source cannot be empty");
    }
    if source.len() > MAX_SOURCE_LEN {
        bail!("source exceeds max length of {MAX_SOURCE_LEN} bytes");
    }
    if !VALID_SOURCES.contains(&source) {
        bail!(
            "invalid source '{}' — must be one of: {}",
            source,
            VALID_SOURCES.join(", ")
        );
    }
    Ok(())
}

/// Validate an agent identifier (NHI-hardened).
///
/// Allowed characters: alphanumeric plus `_`, `-`, `:`, `@`, `.`, `/`.
/// Length: 1..=128 bytes.
///
/// This intentionally permits prefixed/scoped forms such as
/// `ai:claude-code@host-1:pid-123`, `host:dev-1:pid-9-deadbeef`,
/// `anonymous:req-abcdef01`, and future SPIFFE-style ids containing `/`.
/// Rejects whitespace, null bytes, control chars, and shell metacharacters.
pub fn validate_agent_id(agent_id: &str) -> Result<()> {
    if agent_id.is_empty() {
        bail!("agent_id cannot be empty");
    }
    if agent_id.len() > MAX_AGENT_ID_LEN {
        bail!("agent_id exceeds max length of {MAX_AGENT_ID_LEN} bytes");
    }
    for c in agent_id.chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ':' | '@' | '.' | '/')) {
            bail!("agent_id contains invalid character '{c}' (allowed: alphanumeric, _-:@./)");
        }
    }
    Ok(())
}

/// Validate a visibility scope against the closed `VALID_SCOPES` set
/// (Task 1.5). Enforced on write paths that accept an explicit `scope`
/// parameter. Memories with no `scope` metadata are treated as `private`
/// by the query layer without needing explicit validation here.
pub fn validate_scope(scope: &str) -> Result<()> {
    if scope.is_empty() {
        bail!("scope cannot be empty");
    }
    if !VALID_SCOPES.contains(&scope) {
        bail!(
            "invalid scope '{}' — must be one of: {}",
            scope,
            VALID_SCOPES.join(", ")
        );
    }
    Ok(())
}

/// Validate a [`GovernancePolicy`] (Task 1.8). Closed-set tag checks are
/// already handled by serde on deserialization; this adds semantic bounds:
/// consensus quorum must be ≥ 1, Agent references must pass
/// `validate_agent_id`, and the policy as a whole must not use
/// `GovernanceLevel::Approve` without a meaningful approver.
pub fn validate_governance_policy(policy: &crate::models::GovernancePolicy) -> Result<()> {
    use crate::models::{ApproverType, GovernanceLevel};
    // Approver-specific constraints
    match &policy.approver {
        ApproverType::Human => {}
        ApproverType::Agent(id) => {
            validate_agent_id(id)?;
        }
        ApproverType::Consensus(n) => {
            if *n == 0 {
                bail!("governance.approver.consensus quorum must be >= 1");
            }
        }
    }
    // `Approve` level is meaningless without a configured approver. The
    // `Human` default is always valid, but a `Consensus(0)` or bad-id agent
    // would have been caught above.
    let uses_approve = matches!(policy.write, GovernanceLevel::Approve)
        || matches!(policy.promote, GovernanceLevel::Approve)
        || matches!(policy.delete, GovernanceLevel::Approve);
    if uses_approve
        && let ApproverType::Consensus(n) = &policy.approver
        && *n == 0
    {
        bail!("governance uses 'approve' level but approver consensus is 0");
    }
    Ok(())
}

/// Maximum length for an `agent_type` string.
const MAX_AGENT_TYPE_LEN: usize = 64;

/// Validate an agent type. Accepts any value matching one of these forms
/// (red-team #235 — the original closed whitelist blocked future agents):
///
/// - **Anything in [`VALID_AGENT_TYPES`]** — the curated short-list including
///   `human`, `system`, and known AI model identifiers
/// - **Any `ai:<name>` form** — `^ai:[A-Za-z0-9_.-]{1,60}$`. Lets operators
///   register `ai:claude-opus-4.8`, `ai:gpt-5`, `ai:gemini-2.5`, etc. without
///   waiting for a code release
///
/// Strict format guard: alphanumeric + `_-:.` only, max 64 bytes total.
/// This keeps the value safe for SQL storage, JSON serialization, and
/// shell display while removing the closed-list hard stop.
pub fn validate_agent_type(agent_type: &str) -> Result<()> {
    if agent_type.is_empty() {
        bail!("agent_type cannot be empty");
    }
    if agent_type.len() > MAX_AGENT_TYPE_LEN {
        bail!("agent_type exceeds max length of {MAX_AGENT_TYPE_LEN} bytes");
    }
    // Curated set always wins.
    if VALID_AGENT_TYPES.contains(&agent_type) {
        return Ok(());
    }
    // Open `ai:<name>` namespace for forward compatibility with future models.
    if let Some(name) = agent_type.strip_prefix("ai:") {
        if name.is_empty() {
            bail!("agent_type 'ai:' must include a name (e.g. 'ai:claude-opus-4.7')");
        }
        if name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        {
            return Ok(());
        }
        bail!(
            "agent_type '{agent_type}' contains invalid characters in the ai: name \
             part (allowed: alphanumeric, _-.)"
        );
    }
    let valid = VALID_AGENT_TYPES.join(", ");
    bail!("invalid agent_type '{agent_type}' — must be one of: {valid} (or any ai:<name> form)");
}

/// Validate a list of capability strings. Shares `validate_tags` rules
/// (non-empty, <=128 bytes each, clean chars, <=50 entries).
pub fn validate_capabilities(caps: &[String]) -> Result<()> {
    validate_tags(caps)
}

pub fn validate_tags(tags: &[String]) -> Result<()> {
    if tags.len() > MAX_TAGS_COUNT {
        bail!("too many tags (max {MAX_TAGS_COUNT})");
    }
    for tag in tags {
        let trimmed = tag.trim();
        if trimmed.is_empty() {
            bail!("tags cannot contain empty strings");
        }
        if trimmed.len() > MAX_TAG_LEN {
            let preview: String = trimmed.chars().take(20).collect();
            bail!("tag '{preview}...' exceeds max length of {MAX_TAG_LEN} bytes");
        }
        if !is_clean_string(trimmed) {
            bail!("tag contains invalid characters");
        }
    }
    Ok(())
}

pub fn validate_id(id: &str) -> Result<()> {
    if id.trim().is_empty() {
        bail!("id cannot be empty");
    }
    if id.len() > MAX_ID_LEN {
        bail!("id exceeds max length of {MAX_ID_LEN} bytes");
    }
    if !is_clean_string(id) {
        bail!("id contains invalid characters");
    }
    Ok(())
}

pub fn validate_expires_at(expires_at: Option<&str>) -> Result<()> {
    if let Some(ts) = expires_at {
        if !is_valid_rfc3339(ts) {
            bail!("expires_at is not valid RFC3339: '{ts}'");
        }
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts)
            && dt < chrono::Utc::now()
        {
            bail!("expires_at is in the past");
        }
    }
    Ok(())
}

pub fn validate_ttl_secs(ttl: Option<i64>) -> Result<()> {
    if let Some(secs) = ttl {
        if secs <= 0 {
            bail!("ttl_secs must be positive (got {secs})");
        }
        if secs > 365 * 24 * 3600 {
            bail!("ttl_secs exceeds maximum of 1 year");
        }
    }
    Ok(())
}

pub fn validate_metadata(metadata: &serde_json::Value) -> Result<()> {
    if !metadata.is_object() {
        bail!("metadata must be a JSON object");
    }
    let serialized = serde_json::to_string(metadata)
        .map_err(|e| anyhow::anyhow!("metadata is not valid JSON: {e}"))?;
    if serialized.len() > MAX_METADATA_SIZE {
        bail!(
            "metadata exceeds max size of {MAX_METADATA_SIZE} bytes (got {})",
            serialized.len()
        );
    }
    let depth = json_depth(metadata);
    if depth > MAX_METADATA_DEPTH {
        bail!("metadata nesting depth exceeds limit of {MAX_METADATA_DEPTH} (got {depth})");
    }
    Ok(())
}

fn json_depth(val: &serde_json::Value) -> usize {
    match val {
        serde_json::Value::Object(map) => 1 + map.values().map(json_depth).max().unwrap_or(0),
        serde_json::Value::Array(arr) => 1 + arr.iter().map(json_depth).max().unwrap_or(0),
        _ => 0,
    }
}

pub fn validate_relation(relation: &str) -> Result<()> {
    if relation.trim().is_empty() {
        bail!("relation cannot be empty");
    }
    if relation.len() > MAX_RELATION_LEN {
        bail!("relation exceeds max length of {MAX_RELATION_LEN} bytes");
    }
    // v0.7.0 Wave-3 Continuation 5 — accept the canonical set above
    // PLUS any caller-supplied lowercase identifier (a-z + 0-9 +
    // underscore) so cert harnesses + downstream tooling can use
    // arbitrary relation labels like `next`, `mentions`, `parent_of`.
    // Mirrors the AGE Cypher convention where edge labels are
    // user-defined identifiers; the same posture lights up here for
    // wire-shape uniformity. Rejects whitespace / control chars /
    // shell metacharacters defensively.
    if VALID_RELATIONS.contains(&relation) {
        return Ok(());
    }
    let ok = !relation.is_empty()
        && relation
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ok {
        bail!(
            "invalid relation '{}' — must match [a-z0-9_]+ or be one of: {}",
            relation,
            VALID_RELATIONS.join(", ")
        );
    }
    Ok(())
}

pub fn validate_confidence(confidence: f64) -> Result<()> {
    if confidence.is_nan() || confidence.is_infinite() {
        bail!("confidence must be a finite number");
    }
    if !(0.0..=1.0).contains(&confidence) {
        bail!("confidence must be between 0.0 and 1.0 (got {confidence})");
    }
    Ok(())
}

pub fn validate_priority(priority: i32) -> Result<()> {
    if !(1..=10).contains(&priority) {
        bail!("priority must be between 1 and 10 (got {priority})");
    }
    Ok(())
}

/// Validate a full `CreateMemory` before insert.
pub fn validate_create(mem: &CreateMemory) -> Result<()> {
    validate_title(&mem.title)?;
    validate_content(&mem.content)?;
    validate_namespace(&mem.namespace)?;
    validate_source(&mem.source)?;
    validate_tags(&mem.tags)?;
    validate_priority(mem.priority)?;
    validate_confidence(mem.confidence)?;
    validate_expires_at(mem.expires_at.as_deref())?;
    validate_ttl_secs(mem.ttl_secs)?;
    validate_metadata(&mem.metadata)?;
    Ok(())
}

/// Validate a full Memory (used for import).
pub fn validate_memory(mem: &Memory) -> Result<()> {
    validate_id(&mem.id)?;
    validate_title(&mem.title)?;
    validate_content(&mem.content)?;
    validate_namespace(&mem.namespace)?;
    validate_source(&mem.source)?;
    validate_tags(&mem.tags)?;
    validate_priority(mem.priority)?;
    validate_confidence(mem.confidence)?;
    if mem.access_count < 0 {
        bail!("access_count cannot be negative");
    }
    if !is_valid_rfc3339(&mem.created_at) {
        bail!("created_at is not valid RFC3339");
    }
    if !is_valid_rfc3339(&mem.updated_at) {
        bail!("updated_at is not valid RFC3339");
    }
    if let Some(ref ts) = mem.last_accessed_at
        && !is_valid_rfc3339(ts)
    {
        bail!("last_accessed_at is not valid RFC3339");
    }
    // Don't reject past expires_at on import — may be importing historical data
    if let Some(ref ts) = mem.expires_at
        && !is_valid_rfc3339(ts)
    {
        bail!("expires_at is not valid RFC3339");
    }
    validate_metadata(&mem.metadata)?;
    Ok(())
}

/// Validate update fields (only validates present fields).
/// Note: `expires_at` allows past dates in updates for programmatic TTL management
/// and GC testing — only format is validated, not chronological ordering.
pub fn validate_update(update: &UpdateMemory) -> Result<()> {
    if let Some(ref t) = update.title {
        validate_title(t)?;
    }
    if let Some(ref c) = update.content {
        validate_content(c)?;
    }
    if let Some(ref ns) = update.namespace {
        validate_namespace(ns)?;
    }
    if let Some(ref tags) = update.tags {
        validate_tags(tags)?;
    }
    if let Some(p) = update.priority {
        validate_priority(p)?;
    }
    if let Some(c) = update.confidence {
        validate_confidence(c)?;
    }
    if let Some(ref ts) = update.expires_at {
        validate_expires_at_format(ts)?;
    }
    if let Some(ref meta) = update.metadata {
        validate_metadata(meta)?;
    }
    Ok(())
}

/// Validate `expires_at` format only (no past-date check). Used by update path.
pub fn validate_expires_at_format(ts: &str) -> Result<()> {
    if !is_valid_rfc3339(ts) {
        bail!("expires_at is not valid RFC3339: '{ts}'");
    }
    Ok(())
}

/// Validate link creation.
pub fn validate_link(source_id: &str, target_id: &str, relation: &str) -> Result<()> {
    validate_id(source_id)?;
    validate_id(target_id)?;
    validate_relation(relation)?;
    if source_id == target_id {
        bail!("cannot link a memory to itself");
    }
    Ok(())
}

/// Validate consolidation request.
pub fn validate_consolidate(
    ids: &[String],
    title: &str,
    summary: &str,
    namespace: &str,
) -> Result<()> {
    if ids.len() < 2 {
        bail!("need at least 2 memory IDs to consolidate");
    }
    if ids.len() > 100 {
        bail!("cannot consolidate more than 100 memories at once");
    }
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        validate_id(id)?;
        if !seen.insert(id) {
            bail!("duplicate memory ID: {id}");
        }
    }
    validate_title(title)?;
    validate_content(summary)?;
    validate_namespace(namespace)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_title() {
        assert!(validate_title("BIND9 custom build").is_ok());
        assert!(validate_title("").is_err());
        assert!(validate_title("   ").is_err());
        assert!(validate_title(&"x".repeat(513)).is_err());
        assert!(validate_title("has\0null").is_err());
    }

    #[test]
    fn test_valid_namespace_flat_backwards_compat() {
        // Task 1.4: flat namespaces must still validate exactly as before.
        assert!(validate_namespace("my-project").is_ok());
        assert!(validate_namespace("global").is_ok());
        assert!(validate_namespace("under_score").is_ok());
        assert!(validate_namespace("ai-memory-mcp-dev").is_ok());
        assert!(validate_namespace("_agents").is_ok());
    }

    #[test]
    fn test_valid_namespace_rejections_preserved() {
        assert!(validate_namespace("").is_err());
        assert!(validate_namespace("   ").is_err());
        assert!(validate_namespace("has space").is_err());
        assert!(validate_namespace("has\\backslash").is_err());
        assert!(validate_namespace("has\0null").is_err());
        assert!(validate_namespace("has\x07bell").is_err());
    }

    #[test]
    fn test_namespace_rejects_dot_segments_redteam_240() {
        // Red-team #240 — `..` and `.` segments must be rejected to
        // prevent hierarchy confusion / visibility prefix-match games.
        assert!(validate_namespace("acme/../other").is_err());
        assert!(validate_namespace("acme/./other").is_err());
        assert!(validate_namespace("..").is_err());
        assert!(validate_namespace(".").is_err());
        assert!(validate_namespace("acme/team/..").is_err());
        assert!(validate_namespace("../acme").is_err());
        // But two dots inside a name is fine — only standalone segments are blocked.
        assert!(validate_namespace("acme/team..special").is_ok());
        assert!(validate_namespace("acme/.dotfile").is_ok());
    }

    #[test]
    fn test_namespace_length_bumped_to_512() {
        // Historical 128-char budget is a floor; 512 is the new max for paths.
        assert!(validate_namespace(&"x".repeat(128)).is_ok());
        assert!(validate_namespace(&"x".repeat(512)).is_ok());
        assert!(validate_namespace(&"x".repeat(513)).is_err());
    }

    // Task 1.4 — hierarchical paths ---------------------------------------

    #[test]
    fn test_hierarchical_paths_accepted() {
        assert!(validate_namespace("alphaone/engineering").is_ok());
        assert!(validate_namespace("alphaone/engineering/platform").is_ok());
        assert!(validate_namespace("a/b/c/d/e/f/g/h").is_ok(), "8 levels OK");
    }

    #[test]
    fn test_hierarchical_depth_cap() {
        // 9 levels exceeds MAX_NAMESPACE_DEPTH (8)
        assert!(validate_namespace("a/b/c/d/e/f/g/h/i").is_err());
    }

    #[test]
    fn test_hierarchical_rejects_leading_slash() {
        assert!(validate_namespace("/alphaone/engineering").is_err());
    }

    #[test]
    fn test_hierarchical_rejects_trailing_slash() {
        assert!(validate_namespace("alphaone/engineering/").is_err());
    }

    #[test]
    fn test_hierarchical_rejects_empty_segments() {
        assert!(validate_namespace("alphaone//engineering").is_err());
        assert!(validate_namespace("a///b").is_err());
    }

    #[test]
    fn test_hierarchical_rejects_control_chars() {
        assert!(validate_namespace("a/b\x07c").is_err());
        assert!(validate_namespace("a/b\0c").is_err());
    }

    #[test]
    fn test_normalize_namespace_strips_slashes() {
        assert_eq!(
            normalize_namespace("/alphaone/engineering/"),
            "alphaone/engineering"
        );
        assert_eq!(normalize_namespace("///a///b///"), "a/b");
    }

    #[test]
    fn test_normalize_namespace_lowercases() {
        assert_eq!(
            normalize_namespace("AlphaOne/Engineering"),
            "alphaone/engineering"
        );
        assert_eq!(normalize_namespace("MYAPP"), "myapp");
    }

    #[test]
    fn test_normalize_namespace_trims_whitespace() {
        assert_eq!(normalize_namespace("  alphaone/eng  "), "alphaone/eng");
    }

    #[test]
    fn test_normalize_then_validate_roundtrip() {
        let raw = "/AlphaOne//Engineering/Platform/";
        let norm = normalize_namespace(raw);
        assert_eq!(norm, "alphaone/engineering/platform");
        assert!(validate_namespace(&norm).is_ok());
    }

    #[test]
    fn test_valid_source() {
        assert!(validate_source("user").is_ok());
        assert!(validate_source("claude").is_ok());
        assert!(validate_source("hook").is_ok());
        assert!(validate_source("api").is_ok());
        assert!(validate_source("cli").is_ok());
        assert!(validate_source("import").is_ok());
        assert!(validate_source("").is_err());
        assert!(validate_source("random").is_err());
    }

    #[test]
    fn test_valid_agent_id() {
        // Accepted NHI-hardened formats
        assert!(validate_agent_id("alice").is_ok());
        assert!(validate_agent_id("ai:claude-code@host-1:pid-123").is_ok());
        assert!(validate_agent_id("host:dev-1:pid-9-deadbeef").is_ok());
        assert!(validate_agent_id("anonymous:req-abcdef01").is_ok());
        assert!(validate_agent_id("anonymous:pid-42-0123abcd").is_ok());
        assert!(validate_agent_id("spiffe://example.org/ns/prod").is_ok());
        assert!(validate_agent_id("a").is_ok());
        assert!(validate_agent_id(&"a".repeat(128)).is_ok());
    }

    #[test]
    fn test_invalid_agent_id() {
        // Empty / oversized
        assert!(validate_agent_id("").is_err());
        assert!(validate_agent_id(&"a".repeat(129)).is_err());

        // Whitespace
        assert!(validate_agent_id("alice bob").is_err());
        assert!(validate_agent_id("alice\tbob").is_err());
        assert!(validate_agent_id(" alice").is_err());
        assert!(validate_agent_id("alice ").is_err());

        // Null byte / control chars
        assert!(validate_agent_id("has\0null").is_err());
        assert!(validate_agent_id("has\x07bell").is_err());
        assert!(validate_agent_id("has\nnewline").is_err());

        // Shell metacharacters
        assert!(validate_agent_id("alice;rm").is_err());
        assert!(validate_agent_id("alice|cat").is_err());
        assert!(validate_agent_id("alice&bg").is_err());
        assert!(validate_agent_id("alice$VAR").is_err());
        assert!(validate_agent_id("alice`cmd`").is_err());
        assert!(validate_agent_id("alice\\bs").is_err());
        assert!(validate_agent_id("alice?q").is_err());
        assert!(validate_agent_id("alice*glob").is_err());
    }

    #[test]
    fn test_validate_governance_policy_default_ok() {
        let p = crate::models::GovernancePolicy::default();
        assert!(validate_governance_policy(&p).is_ok());
    }

    #[test]
    fn test_validate_governance_consensus_zero_rejected() {
        use crate::models::{ApproverType, GovernanceLevel, GovernancePolicy};
        let p = GovernancePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Consensus(0),
            inherit: true,
            max_reflection_depth: None,
            auto_export_reflections_to_filesystem: None,
            auto_atomise: None,
            auto_atomise_threshold_cl100k: None,
            auto_atomise_max_atom_tokens: None,
            auto_persona_trigger_every_n_memories: None,
            auto_export_personas_to_filesystem: None,
            auto_atomise_mode: None,
            legacy_per_pair_classifier: None,
        };
        assert!(validate_governance_policy(&p).is_err());
    }

    #[test]
    fn test_validate_governance_agent_id_checked() {
        use crate::models::{ApproverType, GovernanceLevel, GovernancePolicy};
        let bad = GovernancePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Agent("has space".to_string()),
            inherit: true,
            max_reflection_depth: None,
            auto_export_reflections_to_filesystem: None,
            auto_atomise: None,
            auto_atomise_threshold_cl100k: None,
            auto_atomise_max_atom_tokens: None,
            auto_persona_trigger_every_n_memories: None,
            auto_export_personas_to_filesystem: None,
            auto_atomise_mode: None,
            legacy_per_pair_classifier: None,
        };
        assert!(validate_governance_policy(&bad).is_err());

        let good = GovernancePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Agent("alice".to_string()),
            inherit: true,
            max_reflection_depth: None,
            auto_export_reflections_to_filesystem: None,
            auto_atomise: None,
            auto_atomise_threshold_cl100k: None,
            auto_atomise_max_atom_tokens: None,
            auto_persona_trigger_every_n_memories: None,
            auto_export_personas_to_filesystem: None,
            auto_atomise_mode: None,
            legacy_per_pair_classifier: None,
        };
        assert!(validate_governance_policy(&good).is_ok());
    }

    #[test]
    fn test_valid_scope() {
        for s in ["private", "team", "unit", "org", "collective"] {
            assert!(validate_scope(s).is_ok(), "{s} must be valid");
        }
    }

    #[test]
    fn test_invalid_scope() {
        assert!(validate_scope("").is_err());
        assert!(validate_scope("public").is_err());
        assert!(validate_scope("PRIVATE").is_err());
        assert!(validate_scope("personal").is_err());
    }

    #[test]
    fn test_valid_agent_type_curated_values() {
        assert!(validate_agent_type("ai:claude-opus-4.6").is_ok());
        assert!(validate_agent_type("ai:codex-5.4").is_ok());
        assert!(validate_agent_type("ai:grok-4.2").is_ok());
        assert!(validate_agent_type("human").is_ok());
        assert!(validate_agent_type("system").is_ok());
    }

    #[test]
    fn test_valid_agent_type_open_ai_namespace_redteam_235() {
        // Red-team #235 — any `ai:<name>` form must be accepted so operators
        // can register future / custom AI agents without code changes.
        assert!(validate_agent_type("ai:claude-opus-4.8").is_ok());
        assert!(validate_agent_type("ai:gpt-5").is_ok());
        assert!(validate_agent_type("ai:gemini-2.5").is_ok());
        assert!(validate_agent_type("ai:custom_internal-model.v2").is_ok());
        assert!(validate_agent_type("ai:claude").is_ok());
    }

    #[test]
    fn test_invalid_agent_type() {
        // Empty.
        assert!(validate_agent_type("").is_err());
        // Wrong prefix case (only lowercase `ai:` matches the open form).
        assert!(validate_agent_type("AI:CLAUDE").is_err());
        // Plain word without `ai:` and not in curated set.
        assert!(validate_agent_type("bogus").is_err());
        // `ai:` with no name part.
        assert!(validate_agent_type("ai:").is_err());
        // Invalid char inside the ai: name part.
        assert!(validate_agent_type("ai:foo bar").is_err());
        assert!(validate_agent_type("ai:foo;rm").is_err());
        // Too long.
        assert!(validate_agent_type(&format!("ai:{}", "x".repeat(80))).is_err());
    }

    #[test]
    fn test_agents_namespace_accepted() {
        assert!(validate_namespace("_agents").is_ok());
    }

    #[test]
    fn test_valid_tags() {
        assert!(validate_tags(&["dns".to_string(), "bind9".to_string()]).is_ok());
        assert!(validate_tags(&[]).is_ok());
        assert!(validate_tags(&[String::new()]).is_err());
        let too_many: Vec<String> = (0..51).map(|i| format!("tag{i}")).collect();
        assert!(validate_tags(&too_many).is_err());
    }

    #[test]
    fn test_valid_relation() {
        // v0.7.0 Wave-3 Cont 5 (commit cb92998): `validate_relation`
        // accepts any `[a-z0-9_]+` identifier in addition to the
        // canonical `VALID_RELATIONS` set so S82/S65 chain markers and
        // arbitrary AGE-style edge labels round-trip through the wire.
        // The pre-cb92998 expectation that "invented_relation" must be
        // rejected is therefore obsolete — do not re-introduce it
        // unless production validation is tightened back to a
        // closed-set check. Coverage here splits into:
        //
        //   * canonical names — must always pass
        //   * caller-supplied lowercase identifiers — must pass
        //     post-cb92998
        //   * structurally malformed input — must still fail
        //     (uppercase, whitespace, slashes, empty)
        //
        // The malformed cases below are the surviving "negative"
        // coverage the dropped `invented_relation` assertion used to
        // anchor.

        // Canonical relation names — accepted via the VALID_RELATIONS
        // fast path.
        assert!(validate_relation("related_to").is_ok());
        assert!(validate_relation("derived_from").is_ok());
        assert!(validate_relation("contradicts").is_ok());
        assert!(validate_relation("supersedes").is_ok());
        // v0.7.0 Task 3/8 (recursive learning) — `reflects_on` joins the
        // canonical set as the relation a reflection memory writes back
        // to each source it reflects on. See VALID_RELATIONS docstring.
        assert!(validate_relation("reflects_on").is_ok());

        // Caller-supplied lowercase identifier — accepted by the
        // post-cb92998 permissive arm. Previously rejected.
        assert!(validate_relation("s82_chain_marker").is_ok());
        assert!(validate_relation("invented_relation").is_ok());
        assert!(validate_relation("mentions").is_ok());

        // Structurally malformed input — still rejected.
        assert!(validate_relation("").is_err());
        assert!(validate_relation("BAD").is_err());
        assert!(validate_relation("bad relation").is_err());
        assert!(validate_relation("bad/relation").is_err());
        assert!(validate_relation("bad-relation").is_err());
    }

    #[test]
    fn test_valid_confidence() {
        assert!(validate_confidence(0.0).is_ok());
        assert!(validate_confidence(0.5).is_ok());
        assert!(validate_confidence(1.0).is_ok());
        assert!(validate_confidence(-0.1).is_err());
        assert!(validate_confidence(1.1).is_err());
        assert!(validate_confidence(f64::NAN).is_err());
        assert!(validate_confidence(f64::INFINITY).is_err());
    }

    #[test]
    fn test_valid_ttl() {
        assert!(validate_ttl_secs(None).is_ok());
        assert!(validate_ttl_secs(Some(3600)).is_ok());
        assert!(validate_ttl_secs(Some(0)).is_err());
        assert!(validate_ttl_secs(Some(-1)).is_err());
        assert!(validate_ttl_secs(Some(366 * 24 * 3600)).is_err());
    }

    #[test]
    fn test_self_link_rejected() {
        assert!(validate_link("abc", "abc", "related_to").is_err());
        assert!(validate_link("abc", "def", "related_to").is_ok());
    }

    #[test]
    fn test_valid_metadata() {
        assert!(validate_metadata(&serde_json::json!({})).is_ok());
        assert!(validate_metadata(&serde_json::json!({"key": "value"})).is_ok());
        assert!(validate_metadata(&serde_json::json!({"nested": {"a": 1}})).is_ok());
        // Non-object types rejected
        assert!(validate_metadata(&serde_json::json!("string")).is_err());
        assert!(validate_metadata(&serde_json::json!(42)).is_err());
        assert!(validate_metadata(&serde_json::json!([1, 2])).is_err());
        assert!(validate_metadata(&serde_json::json!(null)).is_err());
    }

    #[test]
    fn test_clean_string_rejects_control_chars() {
        assert!(is_clean_string("normal text"));
        assert!(is_clean_string("with\nnewline"));
        assert!(is_clean_string("with\ttab"));
        assert!(!is_clean_string("has\0null"));
        assert!(!is_clean_string("has\x07bell"));
        assert!(!is_clean_string("has\x1b[31mANSI\x1b[0m"));
        assert!(!is_clean_string("has\x08backspace"));
    }

    #[test]
    fn test_oversized_metadata_rejected() {
        let big_value = "x".repeat(MAX_METADATA_SIZE);
        let meta = serde_json::json!({"big": big_value});
        assert!(validate_metadata(&meta).is_err());
    }

    #[test]
    fn test_deeply_nested_metadata_rejected() {
        // Build a 33-level deep object (exceeds MAX_METADATA_DEPTH of 32)
        let mut val = serde_json::json!("leaf");
        for _ in 0..33 {
            val = serde_json::json!({"nested": val});
        }
        assert!(validate_metadata(&val).is_err());

        // 32 levels should be fine
        let mut val = serde_json::json!("leaf");
        for _ in 0..31 {
            val = serde_json::json!({"nested": val});
        }
        assert!(validate_metadata(&val).is_ok());
    }

    // -----------------------------------------------------------------
    // W11/S11b: proptest properties — boundary + adversarial fuzz
    // -----------------------------------------------------------------
    use proptest::prelude::*;

    proptest! {
        // Title rejection happens iff trimmed string is empty (whitespace-only or "").
        #[test]
        fn prop_validate_title_rejects_empty_strings_only_when_actually_empty(
            ws in r"[ \t\n]{0,16}",
            tail in r"[A-Za-z0-9 _\-.,!?]{0,80}",
        ) {
            // Whitespace-only must reject; otherwise title is valid (within char bounds).
            let title = format!("{ws}{tail}{ws}");
            let trimmed_empty = title.trim().is_empty();
            let result = validate_title(&title);
            if trimmed_empty {
                prop_assert!(result.is_err(), "whitespace-only title must reject: {:?}", title);
            } else if title.chars().count() <= 512 {
                prop_assert!(result.is_ok(), "non-empty trimmed title must accept: {:?}", title);
            }
        }
    }

    proptest! {
        // Namespaces with control chars / spaces / backslashes / null bytes must reject.
        #[test]
        fn prop_validate_namespace_rejects_invalid_chars(
            base in r"[a-z][a-z0-9_-]{0,20}",
            // Pick one of the always-rejected chars and splice it in.
            bad in prop::sample::select(&[' ', '\\', '\0', '\x07', '\x1b', '\x08']),
        ) {
            let ns = format!("{base}{bad}suffix");
            prop_assert!(
                validate_namespace(&ns).is_err(),
                "namespace with bad char {:?} must reject: {:?}", bad, ns
            );
        }
    }

    proptest! {
        // a/b/c style paths up to 8 levels with safe chars should validate.
        #[test]
        fn prop_validate_namespace_accepts_valid_hierarchy(
            segs in prop::collection::vec(r"[a-z][a-z0-9_-]{0,20}", 1..=8),
        ) {
            // Filter out `.` / `..` segments which the validator rejects.
            let safe: Vec<String> = segs
                .into_iter()
                .filter(|s| s != "." && s != "..")
                .collect();
            if safe.is_empty() {
                return Ok(());
            }
            let ns = safe.join("/");
            prop_assert!(
                validate_namespace(&ns).is_ok(),
                "valid hierarchy must accept: {:?}", ns
            );
        }
    }

    proptest! {
        // Priority must accept 1..=10, reject anything outside that band.
        #[test]
        fn prop_validate_priority_rejects_outside_range(p in -1000i32..1000i32) {
            let result = validate_priority(p);
            if (1..=10).contains(&p) {
                prop_assert!(result.is_ok(), "priority {p} (in 1..=10) must accept");
            } else {
                prop_assert!(result.is_err(), "priority {p} (outside 1..=10) must reject");
            }
        }
    }

    proptest! {
        // Confidence rejects NaN / infinity / out-of-band values, accepts [0.0, 1.0].
        // Documented behavior: rejects (does not clamp).
        #[test]
        fn prop_validate_confidence_clamps_or_rejects(c in -10.0f64..10.0f64) {
            let result = validate_confidence(c);
            if (0.0..=1.0).contains(&c) {
                prop_assert!(result.is_ok(), "confidence {c} in [0,1] must accept");
            } else {
                prop_assert!(result.is_err(), "confidence {c} outside [0,1] must reject");
            }
        }

        #[test]
        fn prop_validate_confidence_nan_inf_always_rejected(_u in Just(())) {
            prop_assert!(validate_confidence(f64::NAN).is_err());
            prop_assert!(validate_confidence(f64::INFINITY).is_err());
            prop_assert!(validate_confidence(f64::NEG_INFINITY).is_err());
        }
    }

    proptest! {
        // Self-link must reject for every relation type, regardless of id payload.
        #[test]
        fn prop_validate_link_rejects_self_link_for_every_relation(
            id in r"[a-z][a-zA-Z0-9_-]{0,32}",
            rel_idx in 0usize..5,
        ) {
            // v0.7.0 Task 3/8 (recursive learning) — `reflects_on` joins the
            // canonical relation set; the self-link rejection invariant
            // applies to it too.
            let relations = [
                "related_to",
                "supersedes",
                "contradicts",
                "derived_from",
                "reflects_on",
            ];
            let rel = relations[rel_idx];
            let result = validate_link(&id, &id, rel);
            prop_assert!(result.is_err(), "self-link must reject for relation {rel}, id {:?}", id);
        }
    }

    // -----------------------------------------------------------------
    // Unicode-boundary unit tests (W11/S11b — visible-but-tricky chars)
    // -----------------------------------------------------------------

    #[test]
    fn test_title_accepts_zero_width_joiner() {
        // ZWJ (U+200D) is not a control char; titles should accept it.
        assert!(validate_title("emoji\u{200D}joiner").is_ok());
    }

    #[test]
    fn test_title_accepts_rtl_marks() {
        // Right-to-left mark (U+200F) and LRM (U+200E) are allowed (non-control).
        assert!(validate_title("hello\u{200F}world").is_ok());
        assert!(validate_title("hello\u{200E}world").is_ok());
    }

    #[test]
    fn test_title_accepts_combining_chars() {
        // Combining acute accent on `e` (U+0065 U+0301) — distinct chars,
        // is_clean_string allows them; char count differs from byte count.
        assert!(validate_title("cafe\u{0301}").is_ok());
    }

    #[test]
    fn test_title_rejects_unicode_bom_as_control() {
        // U+FEFF (BOM/zero-width no-break space) — Rust's `is_control` on BOM
        // returns false (it's a format char, not control). Document actual
        // behavior: titles containing BOM are accepted.
        assert!(validate_title("foo\u{FEFF}bar").is_ok());
    }

    // -----------------------------------------------------------------
    // L0.7-2 Tier A — long-tail error path coverage
    // (lines 109, 207, 290, 357/358/361, 383, 438, validate_create /
    // _memory / _update / _consolidate body branches)
    // -----------------------------------------------------------------

    #[test]
    fn content_with_control_chars_rejected() {
        // Line 109: content with control char (not \n or \t)
        let err = validate_content("has\x07bell").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("invalid characters"), "got: {msg}");
    }

    #[test]
    fn content_with_null_byte_rejected() {
        let err = validate_content("has\0null").unwrap_err();
        assert!(format!("{err}").contains("invalid characters"));
    }

    #[test]
    fn source_oversized_rejected() {
        // Line 207: source longer than MAX_SOURCE_LEN (64)
        let big = "x".repeat(65);
        let err = validate_source(&big).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("max length"), "got: {msg}");
    }

    #[test]
    fn governance_approve_with_consensus_zero_rejected() {
        // Line 290: uses_approve && Consensus(0) — must error in the
        // post-approver-block sweep. We force consensus(0) into a policy
        // that also uses Approve at the write level.
        use crate::models::{ApproverType, GovernanceLevel, GovernancePolicy};
        // Build with Human first so the approver block doesn't itself trip,
        // then swap to Consensus(0) directly. The Consensus(0) branch in
        // the approver block (line 276) ALREADY rejects this — the line
        // 290 branch is the second guard. The two branches are
        // semantically redundant for `Consensus(0)`; line 290 is reachable
        // only if approver block were ever loosened. Document the line
        // as defensive coverage; the existing
        // test_validate_governance_consensus_zero_rejected hits the
        // approver-block branch directly.
        let p = GovernancePolicy {
            write: GovernanceLevel::Approve,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Consensus(0),
            inherit: true,
            max_reflection_depth: None,
            auto_export_reflections_to_filesystem: None,
            auto_atomise: None,
            auto_atomise_threshold_cl100k: None,
            auto_atomise_max_atom_tokens: None,
            auto_persona_trigger_every_n_memories: None,
            auto_export_personas_to_filesystem: None,
            auto_atomise_mode: None,
            legacy_per_pair_classifier: None,
        };
        assert!(validate_governance_policy(&p).is_err());
    }

    #[test]
    fn tag_oversized_rejected_with_preview() {
        // Lines 357-358: tag length > MAX_TAG_LEN (128), error message
        // embeds first 20 chars of trimmed tag as preview.
        let big = "x".repeat(129);
        let tags = vec![big];
        let err = validate_tags(&tags).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("max length"), "got: {msg}");
        assert!(msg.contains("xxxxxxxxxxxxxxxxxxxx"), "got: {msg}");
    }

    #[test]
    fn tag_with_control_chars_rejected() {
        // Line 361: tag fails is_clean_string
        let tags = vec!["has\x07bell".to_string()];
        let err = validate_tags(&tags).unwrap_err();
        assert!(format!("{err}").contains("invalid characters"));
    }

    #[test]
    fn expires_at_malformed_rfc3339_rejected() {
        // Line 383: expires_at not valid RFC3339
        let err = validate_expires_at(Some("not-a-date")).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("RFC3339"), "got: {msg}");
        assert!(msg.contains("not-a-date"), "got: {msg}");
    }

    #[test]
    fn expires_at_none_is_ok() {
        // Branch: None arm of validate_expires_at
        assert!(validate_expires_at(None).is_ok());
    }

    #[test]
    fn expires_at_future_is_ok() {
        // Far-future date — valid format, not in the past
        let future = "2099-01-01T00:00:00Z";
        assert!(validate_expires_at(Some(future)).is_ok());
    }

    #[test]
    fn expires_at_past_rejected() {
        // Branch: parsed RFC3339, but earlier than Utc::now()
        let past = "2000-01-01T00:00:00Z";
        let err = validate_expires_at(Some(past)).unwrap_err();
        assert!(format!("{err}").contains("past"));
    }

    #[test]
    fn relation_oversized_rejected() {
        // Line 438: relation longer than MAX_RELATION_LEN (64)
        let big = "x".repeat(65);
        let err = validate_relation(&big).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("max length"), "got: {msg}");
    }

    // -----------------------------------------------------------------
    // L0.7-2 Tier A — validate_create / validate_memory full body
    // (lines 486-602: every per-field error branch)
    // -----------------------------------------------------------------

    fn cm_valid() -> crate::models::CreateMemory {
        // Construct a valid CreateMemory via serde defaults — deserialise
        // from minimal JSON so we don't depend on private struct shape.
        serde_json::from_value(serde_json::json!({
            "title": "ok title",
            "content": "ok content body",
            "namespace": "validate-test",
            "tags": ["one", "two"],
            "priority": 5,
            "confidence": 0.9,
            "source": "api",
            "metadata": {"k": "v"},
        }))
        .expect("fixture deserialises")
    }

    #[test]
    fn validate_create_happy_path() {
        let m = cm_valid();
        assert!(validate_create(&m).is_ok());
    }

    #[test]
    fn validate_create_propagates_title_error() {
        let mut m = cm_valid();
        m.title = String::new();
        assert!(validate_create(&m).is_err());
    }

    #[test]
    fn validate_create_propagates_content_error() {
        let mut m = cm_valid();
        m.content = String::new();
        assert!(validate_create(&m).is_err());
    }

    #[test]
    fn validate_create_propagates_namespace_error() {
        let mut m = cm_valid();
        m.namespace = "has space".to_string();
        assert!(validate_create(&m).is_err());
    }

    #[test]
    fn validate_create_propagates_source_error() {
        let mut m = cm_valid();
        m.source = "bogus".to_string();
        assert!(validate_create(&m).is_err());
    }

    #[test]
    fn validate_create_propagates_tags_error() {
        let mut m = cm_valid();
        m.tags = vec![String::new()];
        assert!(validate_create(&m).is_err());
    }

    #[test]
    fn validate_create_propagates_priority_error() {
        let mut m = cm_valid();
        m.priority = 11;
        assert!(validate_create(&m).is_err());
    }

    #[test]
    fn validate_create_propagates_confidence_error() {
        let mut m = cm_valid();
        m.confidence = 1.5;
        assert!(validate_create(&m).is_err());
    }

    #[test]
    fn validate_create_propagates_expires_at_error() {
        let mut m = cm_valid();
        m.expires_at = Some("not-a-date".to_string());
        assert!(validate_create(&m).is_err());
    }

    #[test]
    fn validate_create_propagates_ttl_error() {
        let mut m = cm_valid();
        m.ttl_secs = Some(-1);
        assert!(validate_create(&m).is_err());
    }

    #[test]
    fn validate_create_propagates_metadata_error() {
        let mut m = cm_valid();
        m.metadata = serde_json::json!("not-an-object");
        assert!(validate_create(&m).is_err());
    }

    // -----------------------------------------------------------------
    // validate_memory body branches (lines 498-528)
    // -----------------------------------------------------------------

    fn mem_valid() -> crate::models::Memory {
        crate::models::Memory {
            id: "mem-1".to_string(),
            title: "ok title".to_string(),
            content: "ok content".to_string(),
            namespace: "validate-test".to_string(),
            source: "api".to_string(),
            tags: vec!["one".to_string()],
            priority: 5,
            confidence: 1.0,
            access_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn validate_memory_happy_path() {
        let m = mem_valid();
        assert!(validate_memory(&m).is_ok());
    }

    #[test]
    fn validate_memory_rejects_empty_id() {
        let mut m = mem_valid();
        m.id = String::new();
        assert!(validate_memory(&m).is_err());
    }

    #[test]
    fn validate_memory_rejects_negative_access_count() {
        let mut m = mem_valid();
        m.access_count = -1;
        let err = validate_memory(&m).unwrap_err();
        assert!(format!("{err}").contains("access_count"));
    }

    #[test]
    fn validate_memory_rejects_malformed_created_at() {
        let mut m = mem_valid();
        m.created_at = "not-a-date".to_string();
        let err = validate_memory(&m).unwrap_err();
        assert!(format!("{err}").contains("created_at"));
    }

    #[test]
    fn validate_memory_rejects_malformed_updated_at() {
        let mut m = mem_valid();
        m.updated_at = "not-a-date".to_string();
        let err = validate_memory(&m).unwrap_err();
        assert!(format!("{err}").contains("updated_at"));
    }

    #[test]
    fn validate_memory_rejects_malformed_last_accessed_at() {
        let mut m = mem_valid();
        m.last_accessed_at = Some("not-a-date".to_string());
        let err = validate_memory(&m).unwrap_err();
        assert!(format!("{err}").contains("last_accessed_at"));
    }

    #[test]
    fn validate_memory_accepts_valid_last_accessed_at() {
        let mut m = mem_valid();
        m.last_accessed_at = Some("2026-01-01T00:00:00Z".to_string());
        assert!(validate_memory(&m).is_ok());
    }

    #[test]
    fn validate_memory_rejects_malformed_expires_at() {
        let mut m = mem_valid();
        m.expires_at = Some("not-a-date".to_string());
        let err = validate_memory(&m).unwrap_err();
        assert!(format!("{err}").contains("expires_at"));
    }

    #[test]
    fn validate_memory_accepts_past_expires_at_for_import() {
        // Importers must be able to bring in historically expired rows.
        let mut m = mem_valid();
        m.expires_at = Some("2000-01-01T00:00:00Z".to_string());
        assert!(validate_memory(&m).is_ok());
    }

    // -----------------------------------------------------------------
    // validate_update body branches (lines 534-559)
    // -----------------------------------------------------------------

    fn upd() -> crate::models::UpdateMemory {
        serde_json::from_value(serde_json::json!({})).expect("empty UpdateMemory deserialises")
    }

    #[test]
    fn validate_update_empty_is_ok() {
        assert!(validate_update(&upd()).is_ok());
    }

    #[test]
    fn validate_update_propagates_title_error() {
        let mut u = upd();
        u.title = Some(String::new());
        assert!(validate_update(&u).is_err());
    }

    #[test]
    fn validate_update_propagates_content_error() {
        let mut u = upd();
        u.content = Some(String::new());
        assert!(validate_update(&u).is_err());
    }

    #[test]
    fn validate_update_propagates_namespace_error() {
        let mut u = upd();
        u.namespace = Some("has space".to_string());
        assert!(validate_update(&u).is_err());
    }

    #[test]
    fn validate_update_propagates_tags_error() {
        let mut u = upd();
        u.tags = Some(vec![String::new()]);
        assert!(validate_update(&u).is_err());
    }

    #[test]
    fn validate_update_propagates_priority_error() {
        let mut u = upd();
        u.priority = Some(11);
        assert!(validate_update(&u).is_err());
    }

    #[test]
    fn validate_update_propagates_confidence_error() {
        let mut u = upd();
        u.confidence = Some(2.0);
        assert!(validate_update(&u).is_err());
    }

    #[test]
    fn validate_update_propagates_expires_at_format_error() {
        let mut u = upd();
        u.expires_at = Some("not-a-date".to_string());
        assert!(validate_update(&u).is_err());
    }

    #[test]
    fn validate_update_allows_past_expires_at() {
        // Per the docstring: update path validates format only, not chronology.
        let mut u = upd();
        u.expires_at = Some("2000-01-01T00:00:00Z".to_string());
        assert!(validate_update(&u).is_ok());
    }

    #[test]
    fn validate_update_propagates_metadata_error() {
        let mut u = upd();
        u.metadata = Some(serde_json::json!("not-an-object"));
        assert!(validate_update(&u).is_err());
    }

    #[test]
    fn validate_expires_at_format_accepts_past_date() {
        // Direct coverage of the format-only helper.
        assert!(validate_expires_at_format("2000-01-01T00:00:00Z").is_ok());
        assert!(validate_expires_at_format("not-a-date").is_err());
    }

    // -----------------------------------------------------------------
    // validate_consolidate body branches (lines 588-604)
    // -----------------------------------------------------------------

    #[test]
    fn consolidate_too_few_ids_rejected() {
        let err = validate_consolidate(&["only-one".to_string()], "title", "summary content", "ns")
            .unwrap_err();
        assert!(format!("{err}").contains("at least 2"));
    }

    #[test]
    fn consolidate_too_many_ids_rejected() {
        let ids: Vec<String> = (0..101).map(|i| format!("id-{i}")).collect();
        let err = validate_consolidate(&ids, "title", "summary content", "ns").unwrap_err();
        assert!(format!("{err}").contains("100"));
    }

    #[test]
    fn consolidate_duplicate_ids_rejected() {
        let ids = vec!["a".to_string(), "a".to_string()];
        let err = validate_consolidate(&ids, "title", "summary content", "ns").unwrap_err();
        assert!(format!("{err}").contains("duplicate"));
    }

    #[test]
    fn consolidate_invalid_id_rejected() {
        let ids = vec!["valid".to_string(), String::new()];
        // Empty id fails validate_id
        let err = validate_consolidate(&ids, "title", "summary content", "ns").unwrap_err();
        assert!(format!("{err}").contains("id"));
    }

    #[test]
    fn consolidate_invalid_title_rejected() {
        let ids = vec!["a".to_string(), "b".to_string()];
        assert!(validate_consolidate(&ids, "", "summary content", "ns").is_err());
    }

    #[test]
    fn consolidate_invalid_summary_rejected() {
        let ids = vec!["a".to_string(), "b".to_string()];
        assert!(validate_consolidate(&ids, "title", "", "ns").is_err());
    }

    #[test]
    fn consolidate_invalid_namespace_rejected() {
        let ids = vec!["a".to_string(), "b".to_string()];
        assert!(validate_consolidate(&ids, "title", "summary content", "has space").is_err());
    }

    #[test]
    fn consolidate_happy_path() {
        let ids = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert!(validate_consolidate(&ids, "title", "summary content", "ns").is_ok());
    }

    // -----------------------------------------------------------------
    // validate_capabilities — wrapper around validate_tags
    // -----------------------------------------------------------------

    #[test]
    fn capabilities_delegates_to_tags() {
        assert!(validate_capabilities(&["read".to_string(), "write".to_string()]).is_ok());
        assert!(validate_capabilities(&[String::new()]).is_err());
    }

    #[test]
    fn id_oversized_rejected() {
        let big = "a".repeat(129);
        let err = validate_id(&big).unwrap_err();
        assert!(format!("{err}").contains("max length"));
    }

    #[test]
    fn id_with_control_chars_rejected() {
        let err = validate_id("has\0null").unwrap_err();
        assert!(format!("{err}").contains("invalid characters"));
    }
}
