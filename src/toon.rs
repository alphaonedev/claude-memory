// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! TOON (Token-Oriented Object Notation) serializer for ai-memory.
//!
//! TOON is a token-efficient alternative to JSON designed for LLM communication.
//! Arrays of objects declare field names once as a header, then list values row
//! by row using pipe delimiters — eliminating 40-60% of repeated field-name tokens.
//!
//! Reference: <https://www.tensorlake.ai/blog-posts/toon-vs-json>

use serde_json::Value;
use std::fmt::Write;

/// Standard memory fields in TOON column order.
const MEMORY_FIELDS: &[&str] = &[
    "id",
    "title",
    "tier",
    "namespace",
    "priority",
    "confidence",
    "score",
    "access_count",
    "tags",
    "source",
    "created_at",
    "updated_at",
    "metadata",
];

/// Compact memory fields — omits timestamps for tighter output.
/// Includes `agent_id` (pulled out of `metadata.agent_id`) so AI clients using
/// the default compact format can see provenance without switching to
/// non-compact TOON or JSON. See issue #199.
const MEMORY_FIELDS_COMPACT: &[&str] = &[
    "id",
    "title",
    "tier",
    "namespace",
    "priority",
    "score",
    "tags",
    "agent_id",
];

/// Serialize a recall/list/search response to TOON format.
///
/// Input: a JSON object with `"memories"` (array of objects) and optional metadata fields.
/// Output: TOON string with header + pipe-delimited rows.
///
/// Example output:
/// ```text
/// count:3|mode:hybrid
/// memories[id|title|tier|namespace|priority|confidence|score|access_count|tags|source|created_at|updated_at]:
/// abc123|PostgreSQL 16|long|infra|9|1.0|0.763|2|postgres,database|claude|2026-04-03T15:00:00+00:00|2026-04-03T15:00:00+00:00
/// def456|Redis cache|long|infra|8|1.0|0.541|0|redis,cache|claude|2026-04-03T15:01:00+00:00|2026-04-03T15:01:00+00:00
/// ```
pub fn memories_to_toon(response: &Value, compact: bool) -> String {
    let fields = if compact {
        MEMORY_FIELDS_COMPACT
    } else {
        MEMORY_FIELDS
    };
    let mut out = String::with_capacity(1024);

    // Metadata line — key:value pairs for non-array fields
    let mut meta = Vec::new();
    if let Some(count) = response.get("count") {
        meta.push(format!("count:{count}"));
    }
    if let Some(mode) = response.get("mode").and_then(|v| v.as_str()) {
        meta.push(format!("mode:{mode}"));
    }
    // Task 1.11: surface token budget info in the meta line when present.
    if let Some(used) = response.get("tokens_used") {
        meta.push(format!("tokens_used:{used}"));
    }
    if let Some(budget) = response.get("budget_tokens") {
        meta.push(format!("budget_tokens:{budget}"));
    }
    if !meta.is_empty() {
        out.push_str(&meta.join("|"));
        out.push('\n');
    }

    // Namespace standards — separate section if present
    let mut std_list: Vec<&Value> = Vec::new();
    if let Some(standard) = response.get("standard") {
        std_list.push(standard);
    }
    if let Some(standards) = response.get("standards").and_then(|v| v.as_array()) {
        std_list.extend(standards.iter());
    }
    if !std_list.is_empty() {
        out.push_str("standards[id|title|content]:\n");
        for standard in &std_list {
            let id = format_value(standard.get("id"));
            let title = format_value(standard.get("title"));
            let content = format_value(standard.get("content"));
            let _ = writeln!(out, "{id}|{title}|{content}");
        }
    }

    // Header line — field names declared once
    out.push_str("memories[");
    out.push_str(&fields.join("|"));
    out.push_str("]:\n");

    // Data rows — one per memory
    if let Some(memories) = response.get("memories").and_then(|v| v.as_array()) {
        for mem in memories {
            let row: Vec<String> = fields
                .iter()
                .map(|&field| {
                    // #199: `agent_id` is nested inside metadata in the Memory struct.
                    // Surface it as a top-level TOON column by digging into metadata.
                    if field == "agent_id" {
                        format_value(mem.get("metadata").and_then(|m| m.get("agent_id")))
                    } else {
                        format_value(mem.get(field))
                    }
                })
                .collect();
            out.push_str(&row.join("|"));
            out.push('\n');
        }
    }

    out
}

/// Serialize a search response (which uses "results" key) to TOON.
pub fn search_to_toon(response: &Value, compact: bool) -> String {
    // Search uses "results" instead of "memories" — normalize
    if response.get("results").is_some() && response.get("memories").is_none() {
        let mut normalized = response.clone();
        if let Some(results) = response.get("results") {
            normalized["memories"] = results.clone();
        }
        return memories_to_toon(&normalized, compact);
    }
    memories_to_toon(response, compact)
}

/// Format a single JSON value for TOON output.
fn format_value(val: Option<&Value>) -> String {
    match val {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => escape_toon(s),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => {
            if *b {
                "1".to_string()
            } else {
                "0".to_string()
            }
        }
        Some(Value::Array(arr)) => {
            // Tags: join with comma
            let items: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
            escape_toon(&items.join(","))
        }
        Some(obj @ Value::Object(m)) => {
            if m.is_empty() {
                String::new()
            } else {
                escape_toon(&serde_json::to_string(obj).unwrap_or_default())
            }
        }
    }
}

/// Escape special characters in TOON values.
fn escape_toon(s: &str) -> String {
    if s.contains('|')
        || s.contains('\n')
        || s.contains('\r')
        || s.contains('\\')
        || s.contains(':')
    {
        s.replace('\\', "\\\\")
            .replace('|', "\\|")
            .replace(':', "\\:")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_memories() {
        let resp = json!({"memories": [], "count": 0, "mode": "keyword"});
        let toon = memories_to_toon(&resp, false);
        assert!(toon.contains("count:0"));
        assert!(toon.contains("mode:keyword"));
        assert!(toon.contains("memories["));
        // No data rows
        let lines: Vec<&str> = toon.lines().collect();
        assert_eq!(lines.len(), 2); // meta + header
    }

    #[test]
    fn single_memory() {
        let resp = json!({
            "memories": [{
                "id": "abc-123",
                "title": "PostgreSQL config",
                "tier": "long",
                "namespace": "infra",
                "priority": 9,
                "confidence": 1.0,
                "score": 0.763,
                "access_count": 2,
                "tags": ["postgres", "database"],
                "source": "claude",
                "created_at": "2026-04-03T15:00:00+00:00",
                "updated_at": "2026-04-03T15:00:00+00:00"
            }],
            "count": 1,
            "mode": "hybrid"
        });
        let toon = memories_to_toon(&resp, false);
        let lines: Vec<&str> = toon.lines().collect();
        assert_eq!(lines.len(), 3); // meta + header + 1 row
        assert!(
            lines[2].starts_with("abc-123|PostgreSQL config|long|infra|9|"),
            "got: {}",
            lines[2]
        );
        assert!(lines[2].contains("postgres,database"));
        assert!(lines[2].contains("claude"));
    }

    #[test]
    fn compact_mode_fewer_fields() {
        let resp = json!({
            "memories": [{"id": "x", "title": "Test", "tier": "mid", "namespace": "test", "priority": 5, "score": 0.5, "tags": []}],
            "count": 1
        });
        let toon = memories_to_toon(&resp, true);
        // #199: agent_id is in the compact header; it's empty when metadata is absent
        assert!(toon.contains("memories[id|title|tier|namespace|priority|score|tags|agent_id]:"));
        assert!(!toon.contains("created_at"));
        assert!(!toon.contains("confidence"));
    }

    #[test]
    fn compact_mode_surfaces_agent_id_from_metadata() {
        let resp = json!({
            "memories": [{
                "id": "x",
                "title": "Test",
                "tier": "mid",
                "namespace": "test",
                "priority": 5,
                "score": 0.5,
                "tags": [],
                "metadata": {"agent_id": "alice"}
            }],
            "count": 1
        });
        let toon = memories_to_toon(&resp, true);
        let row = toon.lines().last().unwrap();
        assert!(
            row.ends_with("|alice"),
            "agent_id must be the last compact column; row: {row}"
        );
    }

    #[test]
    fn pipe_in_title_escaped() {
        let resp = json!({"memories": [{"id": "x", "title": "A|B", "tier": "mid"}], "count": 1});
        let toon = memories_to_toon(&resp, true);
        assert!(toon.contains("A\\|B"));
    }

    #[test]
    fn multiple_memories_token_savings() {
        // Demonstrate: 3 memories, field names appear only once
        let resp = json!({
            "memories": [
                {"id": "a", "title": "Memory 1", "tier": "long", "namespace": "test", "priority": 9, "score": 0.9, "tags": ["t1"]},
                {"id": "b", "title": "Memory 2", "tier": "mid", "namespace": "test", "priority": 7, "score": 0.7, "tags": ["t2"]},
                {"id": "c", "title": "Memory 3", "tier": "short", "namespace": "test", "priority": 5, "score": 0.5, "tags": ["t3"]}
            ],
            "count": 3,
            "mode": "hybrid"
        });
        let toon = memories_to_toon(&resp, true);
        let json_str = serde_json::to_string(&resp).unwrap();
        // TOON should be significantly shorter than JSON
        assert!(
            toon.len() < json_str.len(),
            "TOON ({}) should be shorter than JSON ({})",
            toon.len(),
            json_str.len()
        );
    }

    #[test]
    fn search_results_key() {
        let resp = json!({"results": [{"id": "x", "title": "Found", "tier": "mid"}], "count": 1});
        let toon = search_to_toon(&resp, true);
        assert!(toon.contains("memories["));
        assert!(toon.contains("Found"));
    }

    // -----------------------------------------------------------------
    // W11/S11b — token-savings size invariant + round-trip-ish check
    // -----------------------------------------------------------------

    /// Build a fixed 5-memory fixture so the size invariant is reproducible.
    fn five_memory_fixture() -> Value {
        json!({
            "memories": [
                {
                    "id": "01",
                    "title": "PostgreSQL config",
                    "tier": "long",
                    "namespace": "infra",
                    "priority": 9,
                    "confidence": 1.0,
                    "score": 0.91,
                    "access_count": 4,
                    "tags": ["postgres", "database"],
                    "source": "claude",
                    "created_at": "2026-04-03T15:00:00+00:00",
                    "updated_at": "2026-04-03T15:00:00+00:00",
                    "metadata": {"agent_id": "alice"}
                },
                {
                    "id": "02",
                    "title": "Redis cache strategy",
                    "tier": "long",
                    "namespace": "infra",
                    "priority": 8,
                    "confidence": 0.95,
                    "score": 0.84,
                    "access_count": 2,
                    "tags": ["redis", "cache"],
                    "source": "claude",
                    "created_at": "2026-04-03T15:01:00+00:00",
                    "updated_at": "2026-04-03T15:01:00+00:00",
                    "metadata": {"agent_id": "alice"}
                },
                {
                    "id": "03",
                    "title": "BIND9 custom build",
                    "tier": "mid",
                    "namespace": "infra/dns",
                    "priority": 7,
                    "confidence": 0.9,
                    "score": 0.71,
                    "access_count": 1,
                    "tags": ["bind", "dns"],
                    "source": "user",
                    "created_at": "2026-04-03T15:02:00+00:00",
                    "updated_at": "2026-04-03T15:02:00+00:00",
                    "metadata": {"agent_id": "bob"}
                },
                {
                    "id": "04",
                    "title": "Kubernetes pod recovery",
                    "tier": "mid",
                    "namespace": "platform/k8s",
                    "priority": 6,
                    "confidence": 0.85,
                    "score": 0.62,
                    "access_count": 0,
                    "tags": ["k8s", "ops"],
                    "source": "hook",
                    "created_at": "2026-04-03T15:03:00+00:00",
                    "updated_at": "2026-04-03T15:03:00+00:00",
                    "metadata": {"agent_id": "carol"}
                },
                {
                    "id": "05",
                    "title": "Vault secrets rotation",
                    "tier": "short",
                    "namespace": "security",
                    "priority": 5,
                    "confidence": 0.8,
                    "score": 0.55,
                    "access_count": 3,
                    "tags": ["vault", "secrets"],
                    "source": "api",
                    "created_at": "2026-04-03T15:04:00+00:00",
                    "updated_at": "2026-04-03T15:04:00+00:00",
                    "metadata": {"agent_id": "dave"}
                }
            ],
            "count": 5,
            "mode": "hybrid"
        })
    }

    #[test]
    fn test_toon_size_invariant_5_memories_under_threshold() {
        // Published claim: TOON shaves ~40-79% off JSON for memory rows.
        // We pin a lenient 65% upper bound (≤ 0.65 * JSON_BYTES) for the
        // compact format on a fixed 5-memory fixture. Catches regressions
        // without being so tight that minor format tweaks break CI.
        let fixture = five_memory_fixture();
        let json_bytes = serde_json::to_string(&fixture).unwrap().len();
        let toon_bytes = memories_to_toon(&fixture, true).len();

        let ratio = (toon_bytes as f64) / (json_bytes as f64);
        assert!(
            ratio < 0.65,
            "TOON size invariant violated: toon={toon_bytes} json={json_bytes} \
             ratio={ratio:.3} (must be < 0.65 for 5-memory compact fixture)"
        );

        // Lower-bound sanity: TOON output must be non-empty and contain
        // at least all 5 ids.
        let toon = memories_to_toon(&fixture, true);
        for id in ["01", "02", "03", "04", "05"] {
            assert!(toon.contains(id), "TOON output missing id `{id}`");
        }
    }

    #[test]
    fn test_toon_round_trip_preserves_visible_fields() {
        // No bidirectional parser exists in-tree (TOON is one-way for
        // LLM output). Instead we assert "round-trip-ish": every input
        // field that maps to a TOON column appears verbatim in the output
        // for the non-compact format on a single memory.
        let resp = json!({
            "memories": [{
                "id": "abc-xyz",
                "title": "Round-trip test",
                "tier": "long",
                "namespace": "test",
                "priority": 9,
                "confidence": 1.0,
                "score": 0.5,
                "access_count": 7,
                "tags": ["alpha", "beta"],
                "source": "claude",
                "created_at": "2026-04-03T15:00:00+00:00",
                "updated_at": "2026-04-03T15:00:30+00:00",
                "metadata": {"agent_id": "alice"}
            }],
            "count": 1
        });
        let toon = memories_to_toon(&resp, false);
        // Header lists every non-compact column.
        for col in [
            "id",
            "title",
            "tier",
            "namespace",
            "priority",
            "confidence",
            "score",
            "access_count",
            "tags",
            "source",
            "created_at",
            "updated_at",
            "metadata",
        ] {
            assert!(
                toon.contains(col),
                "TOON header must list column `{col}`; got:\n{toon}"
            );
        }
        // Data row preserves visible string values.
        assert!(toon.contains("abc-xyz"));
        assert!(toon.contains("Round-trip test"));
        assert!(toon.contains("alpha,beta")); // tag array joined w/ comma
        // Timestamps contain `:` which TOON escapes as `\:`. Both forms ship
        // the same logical value; check the escaped variant emitted by
        // `escape_toon` when ':' triggers the escape branch.
        assert!(
            toon.contains(r"2026-04-03T15\:00\:00+00\:00"),
            "TOON should contain timestamp (with escaped ':'): {toon}"
        );
    }
}
