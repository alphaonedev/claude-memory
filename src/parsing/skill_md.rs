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

use crate::models::skill::{ComposesWithReflectionEntry, SkillManifest};

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
    /// v0.7.0 L2-7 (issue #672) — declared composition with reflection
    /// namespaces. Each entry pairs a namespace with a minimum reflection
    /// depth floor. Absent for non-composing skills.
    #[serde(default)]
    composes_with_reflections: Vec<RawComposesEntry>,
    /// Catch-all for unknown top-level keys in the frontmatter YAML.
    #[serde(flatten)]
    extra: std::collections::HashMap<String, serde_yaml::Value>,
}

/// Raw deserialization shape for a single `composes_with_reflections`
/// entry. Decoupled from [`ComposesWithReflectionEntry`] so we can
/// surface targeted validation errors instead of opaque serde messages.
#[derive(Debug, Deserialize)]
struct RawComposesEntry {
    namespace: Option<String>,
    /// Default `0` mirrors the doc-comment on
    /// `ComposesWithReflectionEntry::min_depth`: `0` admits all
    /// reflections at or above the floor.
    #[serde(default)]
    min_depth: Option<u32>,
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
    // L2-7: Validate + materialise composes_with_reflections.
    // -----------------------------------------------------------------------
    let mut composes_with_reflections: Vec<ComposesWithReflectionEntry> =
        Vec::with_capacity(raw.composes_with_reflections.len());
    for (idx, raw_entry) in raw.composes_with_reflections.iter().enumerate() {
        let entry_ns = raw_entry
            .namespace
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                format!(
                    "composes_with_reflections[{idx}] missing required field \
                     'namespace' (v0.7.0 L2-7 issue #672)"
                )
            })?;
        let min_depth = raw_entry.min_depth.unwrap_or(0);
        composes_with_reflections.push(ComposesWithReflectionEntry {
            namespace: entry_ns.to_string(),
            min_depth,
        });
    }

    // -----------------------------------------------------------------------
    // Build extra metadata from remaining YAML keys.
    //
    // L2-7: when the frontmatter declared `composes_with_reflections`, mirror
    // it back into the JSON metadata blob so pre-L2-7 readers that only
    // consult `metadata` still see the declaration as opaque-but-present
    // data. The structured `SkillManifest::composes_with_reflections` field
    // remains the authoritative parsed form for L2-7-aware code paths.
    // -----------------------------------------------------------------------
    let mut metadata = if raw.extra.is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::to_value(&raw.extra)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()))
    };
    if !composes_with_reflections.is_empty() {
        if let serde_json::Value::Object(ref mut map) = metadata {
            // Only insert when the key isn't already present in extras;
            // an out-of-band metadata override would be surprising but
            // is technically supported by the YAML schema.
            map.entry("composes_with_reflections".to_string())
                .or_insert_with(|| {
                    serde_json::to_value(&composes_with_reflections)
                        .unwrap_or(serde_json::Value::Array(Vec::new()))
                });
        }
    }

    Ok(SkillManifest {
        namespace,
        name,
        description,
        license: raw.license,
        compatibility,
        allowed_tools: raw.allowed_tools,
        composes_with_reflections,
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

    // ---------------------------------------------------------------------
    // v0.7.0 L2-7 (issue #672) — composes_with_reflections frontmatter.
    // ---------------------------------------------------------------------

    #[test]
    fn parse_composes_with_reflections() {
        let doc = "---\n\
            namespace: skills\n\
            name: composer\n\
            description: A composing skill.\n\
            composes_with_reflections:\n  \
              - namespace: foo/observations\n    \
                min_depth: 1\n  \
              - namespace: foo/decisions\n    \
                min_depth: 2\n\
            ---\n\nBody.\n";
        let m = parse(doc).expect("composes-aware skill parses");
        assert_eq!(m.composes_with_reflections.len(), 2);
        assert_eq!(m.composes_with_reflections[0].namespace, "foo/observations");
        assert_eq!(m.composes_with_reflections[0].min_depth, 1);
        assert_eq!(m.composes_with_reflections[1].namespace, "foo/decisions");
        assert_eq!(m.composes_with_reflections[1].min_depth, 2);

        // The declaration is also mirrored into JSON metadata so pre-L2-7
        // readers that only consult `metadata` see it as opaque data.
        let mirrored = m.metadata.get("composes_with_reflections").expect(
            "L2-7 backward-compat: declaration must be mirrored into metadata for pre-L2-7 readers",
        );
        assert!(mirrored.is_array(), "metadata mirror is an array");
        assert_eq!(mirrored.as_array().unwrap().len(), 2);
    }

    #[test]
    fn parse_composes_default_min_depth_zero() {
        let doc = "---\n\
            namespace: skills\n\
            name: composer\n\
            description: A composing skill.\n\
            composes_with_reflections:\n  \
              - namespace: foo/observations\n\
            ---\n\nBody.\n";
        let m = parse(doc).expect("missing min_depth defaults to 0");
        assert_eq!(m.composes_with_reflections.len(), 1);
        assert_eq!(m.composes_with_reflections[0].min_depth, 0);
    }

    #[test]
    fn reject_composes_entry_missing_namespace() {
        let doc = "---\n\
            namespace: skills\n\
            name: composer\n\
            description: A composing skill.\n\
            composes_with_reflections:\n  \
              - min_depth: 1\n\
            ---\n\nBody.\n";
        let err = parse(doc).expect_err("entry without namespace must fail");
        assert!(
            err.contains("composes_with_reflections[0]") && err.contains("namespace"),
            "error must identify offending entry: {err}"
        );
    }

    /// L2-7 backward-compat regression pin: a SKILL.md that does NOT
    /// declare `composes_with_reflections` must still parse and produce
    /// an empty Vec — older skills MUST round-trip unchanged.
    #[test]
    fn backward_compat_old_skill_md_parses_without_composition() {
        let doc = "---\n\
            namespace: skills\n\
            name: legacy-skill\n\
            description: A pre-L2-7 skill.\n\
            license: Apache-2.0\n\
            ---\n\nLegacy body.\n";
        let m = parse(doc).expect("legacy SKILL.md must parse");
        assert!(m.composes_with_reflections.is_empty());
        // Metadata must not gain a phantom `composes_with_reflections` key.
        assert!(m.metadata.get("composes_with_reflections").is_none());
    }

    /// L2-7 backward-compat regression pin: when `composes_with_reflections`
    /// lives ONLY inside an opaque-metadata object (the "older client wrote
    /// the field by hand" shape) the parser still accepts the document.
    /// Older readers that don't recognise the field simply see it as one
    /// more metadata key — this test pins that contract by writing the
    /// declaration under both the dedicated YAML key (so the structured
    /// vector populates) and at least one extra opaque key.
    #[test]
    fn backward_compat_extra_metadata_preserved_alongside_composition() {
        let doc = "---\n\
            namespace: skills\n\
            name: hybrid\n\
            description: A skill with extras.\n\
            owner: alice\n\
            composes_with_reflections:\n  \
              - namespace: foo/observations\n    \
                min_depth: 1\n\
            ---\n\nBody.\n";
        let m = parse(doc).expect("parse hybrid");
        assert_eq!(m.composes_with_reflections.len(), 1);
        assert_eq!(
            m.metadata.get("owner").and_then(|v| v.as_str()),
            Some("alice"),
            "L2-7 must not drop other opaque metadata keys when composing"
        );
        assert!(
            m.metadata.get("composes_with_reflections").is_some(),
            "L2-7 mirror into metadata must be present"
        );
    }
}
