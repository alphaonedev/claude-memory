// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use anyhow::{bail, Result};

use crate::models::{CreateMemory, Memory, UpdateMemory, MAX_CONTENT_SIZE};

const MAX_TITLE_LEN: usize = 512;
const MAX_NAMESPACE_LEN: usize = 128;
const MAX_SOURCE_LEN: usize = 64;
const MAX_TAG_LEN: usize = 128;
const MAX_TAGS_COUNT: usize = 50;
const MAX_RELATION_LEN: usize = 64;
const MAX_ID_LEN: usize = 128;

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
    !s.contains('\0')
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

pub fn validate_namespace(ns: &str) -> Result<()> {
    let trimmed = ns.trim();
    if trimmed.is_empty() {
        bail!("namespace cannot be empty");
    }
    if trimmed.chars().count() > MAX_NAMESPACE_LEN {
        bail!("namespace exceeds max length of {MAX_NAMESPACE_LEN} characters");
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains('\0') {
        bail!("namespace cannot contain slashes or null bytes");
    }
    if trimmed.contains(' ') {
        bail!("namespace cannot contain spaces (use hyphens or underscores)");
    }
    Ok(())
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
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
            if dt < chrono::Utc::now() {
                bail!("expires_at is in the past");
            }
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
    if let Some(ref ts) = mem.last_accessed_at {
        if !is_valid_rfc3339(ts) {
            bail!("last_accessed_at is not valid RFC3339");
        }
    }
    // Don't reject past expires_at on import — may be importing historical data
    if let Some(ref ts) = mem.expires_at {
        if !is_valid_rfc3339(ts) {
            bail!("expires_at is not valid RFC3339");
        }
    }
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
    fn test_valid_namespace() {
        assert!(validate_namespace("my-project").is_ok());
        assert!(validate_namespace("global").is_ok());
        assert!(validate_namespace("under_score").is_ok());
        assert!(validate_namespace("").is_err());
        assert!(validate_namespace("has space").is_err());
        assert!(validate_namespace("has/slash").is_err());
        assert!(validate_namespace(&"x".repeat(129)).is_err());
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
}
