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
];
const VALID_RELATIONS: &[&str] = &["related_to", "supersedes", "contradicts", "derived_from"];

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

/// Validate an agent type against the closed `VALID_AGENT_TYPES` set.
pub fn validate_agent_type(agent_type: &str) -> Result<()> {
    if agent_type.is_empty() {
        bail!("agent_type cannot be empty");
    }
    if !VALID_AGENT_TYPES.contains(&agent_type) {
        bail!(
            "invalid agent_type '{}' — must be one of: {}",
            agent_type,
            VALID_AGENT_TYPES.join(", ")
        );
    }
    Ok(())
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
    if !VALID_RELATIONS.contains(&relation) {
        bail!(
            "invalid relation '{}' — must be one of: {}",
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
        };
        assert!(validate_governance_policy(&bad).is_err());

        let good = GovernancePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Agent("alice".to_string()),
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
    fn test_valid_agent_type() {
        assert!(validate_agent_type("ai:claude-opus-4.6").is_ok());
        assert!(validate_agent_type("ai:codex-5.4").is_ok());
        assert!(validate_agent_type("ai:grok-4.2").is_ok());
        assert!(validate_agent_type("human").is_ok());
        assert!(validate_agent_type("system").is_ok());
    }

    #[test]
    fn test_invalid_agent_type() {
        assert!(validate_agent_type("").is_err());
        assert!(validate_agent_type("bogus").is_err());
        assert!(validate_agent_type("AI:CLAUDE").is_err());
        assert!(validate_agent_type("ai:claude").is_err());
    }

    #[test]
    fn test_agents_namespace_accepted() {
        assert!(validate_namespace("_agents").is_ok());
    }

    #[test]
    fn test_valid_tags() {
        assert!(validate_tags(&["dns".to_string(), "bind9".to_string()]).is_ok());
        assert!(validate_tags(&[]).is_ok());
        assert!(validate_tags(&["".to_string()]).is_err());
        let too_many: Vec<String> = (0..51).map(|i| format!("tag{i}")).collect();
        assert!(validate_tags(&too_many).is_err());
    }

    #[test]
    fn test_valid_relation() {
        assert!(validate_relation("related_to").is_ok());
        assert!(validate_relation("supersedes").is_ok());
        assert!(validate_relation("").is_err());
        assert!(validate_relation("invented_relation").is_err());
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
}
