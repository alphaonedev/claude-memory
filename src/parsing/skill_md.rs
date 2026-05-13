// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! SKILL.md parser for the Agent Skills ingestion substrate (L1-5).
//!
//! A `SKILL.md` file has this shape:
//!
//! ```text
//! ---
//! namespace: global
//! name: my-skill
//! description: "Does something useful."
//! license: Apache-2.0          # optional
//! compatibility: ">=0.7.0"     # optional, 1-500 chars
//! allowed_tools:               # optional list
//!   - memory_recall
//!   - memory_store
//! ---
//!
//! Markdown body follows the closing fence.
//! ```
//!
//! # Validation rules (agentskills.io spec)
//!
//! | Field           | Constraint                                                  |
//! |-----------------|-------------------------------------------------------------|
//! | `name`          | Regex `^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$`, length 1-64   |
//! | `description`   | 1-1024 chars, non-empty                                    |
//! | `compatibility` | 1-500 chars when present                                   |
//! | `namespace`     | Required, non-empty                                        |

use serde::Deserialize;

use crate::models::skill::SkillManifest;

// ---------------------------------------------------------------------------
// Internal frontmatter shape (raw deserialization target)
// ---------------------------------------------------------------------------

/// Raw deserialized YAML frontmatter. Loose types so every field that is
/// absent in the document deserializes to `None` rather than an error.
#[derive(Debug, Deserialize)]
struct RawFrontmatter {
    namespace: Option<String>,
    name: Option<String>,
    description: Option<String>,
    license: Option<String>,
    compatibility: Option<String>,
    #[serde(default)]
    allowed_tools: Vec<String>,
    /// Catch-all for unknown top-level keys in the frontmatter YAML.
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_yaml::Value>,
}

// ---------------------------------------------------------------------------
// Name validation
// ---------------------------------------------------------------------------

/// Agentskills.io name regex: `^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$` with
/// length in [1, 64].
///
/// Equivalent rules checked manually (avoids pulling in `regex` just for
/// this pattern):
/// - Not empty.
/// - Length ≤ 64.
/// - First character: lowercase ASCII letter or ASCII digit.
/// - Last character (if len > 1): lowercase ASCII letter or ASCII digit.
/// - Interior characters: lowercase ASCII letter, ASCII digit, or hyphen.
/// - No consecutive hyphens.
pub fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(
            "skill name must be 1-64 lowercase alphanumeric/hyphen characters \
             (agentskills.io spec §3.1): got empty string"
                .to_string(),
        );
    }
    if name.len() > 64 {
        return Err(format!(
            "skill name must be ≤ 64 characters (agentskills.io spec §3.1): \
             got {} characters",
            name.len()
        ));
    }

    let bytes = name.as_bytes();
    let first = bytes[0];
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(format!(
            "skill name must begin with a lowercase letter or digit \
             (agentskills.io spec §3.1): got {name:?}"
        ));
    }
    if bytes.len() > 1 {
        let last = bytes[bytes.len() - 1];
        if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
            return Err(format!(
                "skill name must end with a lowercase letter or digit \
                 (agentskills.io spec §3.1): got {name:?}"
            ));
        }
    }

    // Iterate over interior bytes (indices 1..len-1). When len ≤ 2 the
    // range is empty and the loop body never executes.
    let interior_end = if bytes.len() > 1 { bytes.len() - 1 } else { 1 };
    let mut prev_hyphen = false;
    for &b in &bytes[1..interior_end] {
        let ok = b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
        if !ok {
            return Err(format!(
                "skill name may only contain lowercase letters, digits, and \
                 hyphens (agentskills.io spec §3.1): got {name:?}"
            ));
        }
        if b == b'-' {
            if prev_hyphen {
                return Err(format!(
                    "skill name must not contain consecutive hyphens \
                     (agentskills.io spec §3.1): got {name:?}"
                ));
            }
            prev_hyphen = true;
        } else {
            prev_hyphen = false;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Public parser
// ---------------------------------------------------------------------------

/// Parse and validate a `SKILL.md` document.
///
/// Expects the file to begin with `---\n` (YAML frontmatter), followed by
/// another `---\n` fence, followed by the markdown body.  Returns a
/// validated [`SkillManifest`] or a human-readable error string.
///
/// # Errors
///
/// Returns a `String` describing the first validation failure encountered.
pub fn parse(source: &str) -> Result<SkillManifest, String> {
    // -----------------------------------------------------------------------
    // Split frontmatter from body
    // -----------------------------------------------------------------------
    let inner = source
        .strip_prefix("---")
        .map(|s| s.trim_start_matches('\r').trim_start_matches('\n'))
        .ok_or_else(|| "SKILL.md must begin with a '---' YAML frontmatter fence".to_string())?;

    // Find the closing fence.
    let fence_pos = inner
        .find("\n---")
        .ok_or_else(|| "SKILL.md frontmatter is not closed with a '---' fence".to_string())?;

    let yaml_str = &inner[..fence_pos];
    let body_raw = &inner[fence_pos + 4..]; // skip "\n---"
    let body = body_raw.trim_start_matches('\n').to_string();

    // -----------------------------------------------------------------------
    // Deserialize frontmatter
    // -----------------------------------------------------------------------
    let raw: RawFrontmatter = serde_yaml::from_str(yaml_str)
        .map_err(|e| format!("SKILL.md frontmatter YAML parse error: {e}"))?;

    // -----------------------------------------------------------------------
    // Required field: namespace
    // -----------------------------------------------------------------------
    let namespace = raw
        .namespace
        .filter(|s| !s.is_empty())
        .ok_or("SKILL.md frontmatter missing required field 'namespace'")?;

    // -----------------------------------------------------------------------
    // Required field: name (+ validation)
    // -----------------------------------------------------------------------
    let name = raw
        .name
        .filter(|s| !s.is_empty())
        .ok_or("SKILL.md frontmatter missing required field 'name'")?;
    validate_skill_name(&name)?;

    // -----------------------------------------------------------------------
    // Required field: description (1-1024 chars)
    // -----------------------------------------------------------------------
    let description = raw
        .description
        .filter(|s| !s.is_empty())
        .ok_or("SKILL.md frontmatter missing required field 'description'")?;
    if description.len() > 1024 {
        return Err(format!(
            "skill 'description' must be ≤ 1024 characters \
             (agentskills.io spec §3.2): got {} characters",
            description.len()
        ));
    }

    // -----------------------------------------------------------------------
    // Optional field: compatibility (1-500 chars when present)
    // -----------------------------------------------------------------------
    let compatibility = match raw.compatibility {
        Some(c) if c.is_empty() => None,
        Some(c) if c.len() > 500 => {
            return Err(format!(
                "skill 'compatibility' must be ≤ 500 characters \
                 (agentskills.io spec §3.3): got {} characters",
                c.len()
            ));
        }
        other => other,
    };

    // -----------------------------------------------------------------------
    // Build extra metadata from remaining YAML keys
    // -----------------------------------------------------------------------
    let metadata = if raw.extra.is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::to_value(&raw.extra)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()))
    };

    Ok(SkillManifest {
        namespace,
        name,
        description,
        license: raw.license,
        compatibility,
        allowed_tools: raw.allowed_tools,
        metadata,
        body,
    })
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_skill(name: &str) -> String {
        format!(
            "---\nnamespace: global\nname: {name}\ndescription: A test skill.\n---\n\nBody text.\n"
        )
    }

    #[test]
    fn parse_minimal_skill() {
        let doc = minimal_skill("my-skill");
        let m = parse(&doc).expect("should parse");
        assert_eq!(m.name, "my-skill");
        assert_eq!(m.namespace, "global");
        assert_eq!(m.description, "A test skill.");
        assert_eq!(m.body.trim(), "Body text.");
        assert!(m.license.is_none());
        assert!(m.compatibility.is_none());
        assert!(m.allowed_tools.is_empty());
    }

    #[test]
    fn parse_full_skill() {
        let doc = "---\n\
            namespace: skills\n\
            name: fetch-data\n\
            description: Fetches data from an API endpoint.\n\
            license: Apache-2.0\n\
            compatibility: \">=0.7.0\"\n\
            allowed_tools:\n  \
              - memory_recall\n  \
              - memory_store\n\
            ---\n\n\
            # Fetch Data\n\nInstructions here.\n";
        let m = parse(doc).expect("full skill parses");
        assert_eq!(m.name, "fetch-data");
        assert_eq!(m.license.as_deref(), Some("Apache-2.0"));
        assert_eq!(m.compatibility.as_deref(), Some(">=0.7.0"));
        assert_eq!(m.allowed_tools, vec!["memory_recall", "memory_store"]);
    }

    // ---- name validation ----

    #[test]
    fn reject_uppercase_name() {
        let doc = minimal_skill("MySkill");
        let err = parse(&doc).unwrap_err();
        assert!(err.contains("spec §3.1"), "must cite spec: {err}");
    }

    #[test]
    fn reject_leading_hyphen() {
        let err = validate_skill_name("-bad").unwrap_err();
        assert!(err.contains("spec §3.1"));
    }

    #[test]
    fn reject_trailing_hyphen() {
        let err = validate_skill_name("bad-").unwrap_err();
        assert!(err.contains("spec §3.1"));
    }

    #[test]
    fn reject_consecutive_hyphens() {
        let err = validate_skill_name("bad--name").unwrap_err();
        assert!(err.contains("consecutive"));
    }

    #[test]
    fn reject_empty_name() {
        let err = validate_skill_name("").unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn reject_name_too_long() {
        let long = "a".repeat(65);
        let err = validate_skill_name(&long).unwrap_err();
        assert!(err.contains("64"));
    }

    #[test]
    fn accept_name_at_max_length() {
        let at_max = "a".repeat(64);
        validate_skill_name(&at_max).expect("64 chars is fine");
    }

    #[test]
    fn accept_single_char_name() {
        validate_skill_name("a").expect("single char ok");
        validate_skill_name("9").expect("single digit ok");
    }

    // ---- description validation ----

    #[test]
    fn reject_description_over_1024() {
        let long_desc = "x".repeat(1025);
        let doc =
            format!("---\nnamespace: ns\nname: ok\ndescription: \"{long_desc}\"\n---\n\nBody.\n");
        let err = parse(&doc).unwrap_err();
        assert!(err.contains("1024"));
    }

    #[test]
    fn accept_description_at_1024() {
        let at_limit = "x".repeat(1024);
        let doc =
            format!("---\nnamespace: ns\nname: ok\ndescription: \"{at_limit}\"\n---\n\nBody.\n");
        parse(&doc).expect("1024 chars ok");
    }

    // ---- compatibility validation ----

    #[test]
    fn reject_compatibility_over_500() {
        let long_compat = "x".repeat(501);
        let doc = format!(
            "---\nnamespace: ns\nname: ok\ndescription: Desc.\ncompatibility: \"{long_compat}\"\n---\n\nBody.\n"
        );
        let err = parse(&doc).unwrap_err();
        assert!(err.contains("500"));
    }

    // ---- frontmatter errors ----

    #[test]
    fn reject_missing_fence() {
        let err = parse("namespace: foo\nname: bar\n").unwrap_err();
        assert!(err.contains("---"));
    }

    #[test]
    fn reject_unclosed_frontmatter() {
        let err = parse("---\nnamespace: foo\nname: bar\n").unwrap_err();
        assert!(err.contains("closed"));
    }

    #[test]
    fn reject_missing_namespace() {
        let doc = "---\nname: ok\ndescription: Desc.\n---\n\nBody.\n";
        let err = parse(doc).unwrap_err();
        assert!(err.contains("namespace"));
    }
}
