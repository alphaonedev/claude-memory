// Copyright (c) 2026 AlphaOne LLC. All rights reserved.
// Licensed under the MIT License. See LICENSE file in the project root.

//! TOON (Token-Oriented Object Notation) serializer for ai-memory.
//!
//! TOON is a token-efficient alternative to JSON designed for LLM communication.
//! Arrays of objects declare field names once as a header, then list values row
//! by row using pipe delimiters — eliminating 40-60% of repeated field-name tokens.
//!
//! Reference: <https://www.tensorlake.ai/blog-posts/toon-vs-json>

use serde_json::Value;

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
];

/// Compact memory fields — omits timestamps for tighter output.
const MEMORY_FIELDS_COMPACT: &[&str] = &[
    "id",
    "title",
    "tier",
    "namespace",
    "priority",
    "score",
    "tags",
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
            out.push_str(&format!("{id}|{title}|{content}\n"));
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
                .map(|&field| format_value(mem.get(field)))
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
        Some(Value::Object(_)) => "[object]".to_string(),
    }
}

/// Escape special characters in TOON values.
fn escape_toon(s: &str) -> String {
    if s.contains('|') || s.contains('\n') || s.contains('\r') || s.contains('\\') {
        s.replace('\\', "\\\\")
            .replace('|', "\\|")
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
        assert!(toon.contains("memories[id|title|tier|namespace|priority|score|tags]:"));
        assert!(!toon.contains("created_at"));
        assert!(!toon.contains("confidence"));
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
}
