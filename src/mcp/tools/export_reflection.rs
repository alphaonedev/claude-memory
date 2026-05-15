// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-1 — MCP `memory_export_reflection` handler.
//!
//! Renders the markdown / JSON envelope for a single reflection
//! memory and returns the rendered text plus a suggested filename
//! the agent can pass to the harness for `Write`-tool invocation.
//!
//! # Critical: this handler does NOT write to the filesystem.
//!
//! Two reasons:
//!
//! 1. **Capability isolation.** The MCP server is gated to the
//!    Semantic+ tier; the operator pre-authorised "agent reads and
//!    writes substrate memory" — they did NOT pre-authorise "agent
//!    writes arbitrary paths under `$HOME`". The CLI surface (which
//!    runs in the operator's user session) does the disk write.
//! 2. **Symmetry with `memory_skill_export`.** The L1-5 skill export
//!    tool follows the same contract: the substrate returns the
//!    content, the *agent harness* writes the file. The two tools
//!    must stay structurally aligned so the operator's mental model
//!    transfers.

use serde_json::{Value, json};

use crate::cli::commands::export_reflections::{self, ExportFormat};
use crate::db;
use crate::models::MemoryKind;

/// Wire shape:
///
/// ```json
/// {
///   "content": "---\nmemory_id: ...\n...",
///   "suggested_filename": "<namespace-with-slashes>/<id>.md"
/// }
/// ```
///
/// Errors:
/// * `memory_id is required` — caller omitted the parameter.
/// * `memory_id cannot be empty`.
/// * `memory not found: <id>` — substrate doesn't know this id.
/// * `memory is not a reflection: <id>` — caller passed an observation.
/// * `unsupported export format '<x>'` — `format` was neither
///   `md` nor `json`.
pub(super) fn handle_export_reflection(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let memory_id = params["memory_id"]
        .as_str()
        .ok_or("memory_id is required")?;
    if memory_id.is_empty() {
        return Err("memory_id cannot be empty".to_string());
    }
    let format_str = params["format"].as_str().unwrap_or("md");
    let format = parse_format_for_mcp(format_str)?;

    let mem = db::get(conn, memory_id)
        .map_err(|e| format!("memory_export_reflection substrate error: {e}"))?
        .ok_or_else(|| format!("memory not found: {memory_id}"))?;
    if !matches!(mem.memory_kind, MemoryKind::Reflection) {
        return Err(format!("memory is not a reflection: {memory_id}"));
    }

    let edges = collect_outbound_reflects_on(conn, memory_id)
        .map_err(|e| format!("reading reflects_on links: {e}"))?;
    let attest_level = export_reflections::summarise_attest_level(&edges);
    let content = export_reflections::render_payload(&mem, &edges, attest_level, format);
    let suggested = suggested_filename(&mem.namespace, &mem.id, format);
    Ok(json!({
        "content": content,
        "suggested_filename": suggested,
    }))
}

/// Local copy of the format parser — kept here so the MCP error
/// messages can be tuned independently of the CLI's `parse_format`
/// (which `anyhow::bail`s; MCP convention is plain `String` errors).
fn parse_format_for_mcp(spec: &str) -> Result<ExportFormat, String> {
    match spec.to_lowercase().as_str() {
        "md" | "markdown" => Ok(ExportFormat::Markdown),
        "json" => Ok(ExportFormat::Json),
        other => Err(format!(
            "unsupported export format '{other}' (expected 'md' or 'json')"
        )),
    }
}

/// Same SQL projection the CLI uses, locally re-issued because the
/// CLI's helper is `pub(crate)` and we want to keep the MCP handler
/// self-contained for clarity. The two queries are intentionally
/// byte-identical.
fn collect_outbound_reflects_on(
    conn: &rusqlite::Connection,
    memory_id: &str,
) -> Result<Vec<export_reflections::ReflectsOnEdge>, anyhow::Error> {
    let mut stmt = conn.prepare(
        "SELECT target_id, COALESCE(attest_level, 'unsigned'), created_at \
         FROM memory_links \
         WHERE source_id = ?1 AND relation = 'reflects_on' \
         ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![memory_id], |row| {
        Ok(export_reflections::ReflectsOnEdge {
            target_id: row.get(0)?,
            attest_level: row.get(1)?,
            created_at: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// `<namespace>/<id>.<ext>` — slashes in namespace stay slashes so
/// the agent can build nested directories under whatever root it
/// wants.
fn suggested_filename(namespace: &str, id: &str, format: ExportFormat) -> String {
    let ns_clean = namespace.trim_matches('/');
    if ns_clean.is_empty() {
        format!("{id}.{ext}", ext = format.extension())
    } else {
        format!("{ns_clean}/{id}.{ext}", ext = format.extension())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Memory, Tier};
    use chrono::Utc;
    use tempfile::TempDir;

    fn fresh_db() -> (rusqlite::Connection, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ai-memory.db");
        let conn = db::open(&path).unwrap();
        (conn, dir)
    }

    fn make_reflection(ns: &str, depth: i32, agent_id: &str) -> Memory {
        let now = Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: ns.to_string(),
            title: "rfl".into(),
            content: "body".into(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id": agent_id}),
            reflection_depth: depth,
            memory_kind: MemoryKind::Reflection,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        }
    }

    #[test]
    fn missing_memory_id_errors() {
        let (conn, _g) = fresh_db();
        let err = handle_export_reflection(&conn, &json!({})).unwrap_err();
        assert!(err.contains("memory_id"));
    }

    #[test]
    fn empty_memory_id_errors() {
        let (conn, _g) = fresh_db();
        let err = handle_export_reflection(&conn, &json!({"memory_id": ""})).unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn unknown_id_errors() {
        let (conn, _g) = fresh_db();
        let err = handle_export_reflection(
            &conn,
            &json!({"memory_id": "11111111-2222-3333-4444-555555555555"}),
        )
        .unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn observation_kind_errors() {
        let (conn, _g) = fresh_db();
        let mut obs = make_reflection("ns", 0, "ai:test");
        obs.memory_kind = MemoryKind::Observation;
        obs.reflection_depth = 0;
        let id = db::insert(&conn, &obs).unwrap();
        let err = handle_export_reflection(&conn, &json!({"memory_id": id})).unwrap_err();
        assert!(err.contains("not a reflection"));
    }

    #[test]
    fn unsupported_format_errors() {
        let (conn, _g) = fresh_db();
        let rfl = make_reflection("ns", 1, "ai:test");
        let id = db::insert(&conn, &rfl).unwrap();
        let err = handle_export_reflection(&conn, &json!({"memory_id": id, "format": "yaml"}))
            .unwrap_err();
        assert!(err.contains("unsupported export format"));
    }

    #[test]
    fn happy_path_md_returns_content_and_filename() {
        let (conn, _g) = fresh_db();
        let rfl = make_reflection("team/alpha", 1, "ai:bot");
        let id = db::insert(&conn, &rfl).unwrap();
        let out = handle_export_reflection(&conn, &json!({"memory_id": id})).unwrap();
        let content = out["content"].as_str().unwrap();
        assert!(content.starts_with("---\n"));
        assert!(content.contains(&format!("memory_id: {id}\n")));
        let fname = out["suggested_filename"].as_str().unwrap();
        assert_eq!(fname, format!("team/alpha/{id}.md"));
    }

    #[test]
    fn happy_path_json_returns_parsable_envelope() {
        let (conn, _g) = fresh_db();
        let rfl = make_reflection("ns", 2, "ai:bot");
        let id = db::insert(&conn, &rfl).unwrap();
        let out =
            handle_export_reflection(&conn, &json!({"memory_id": id, "format": "json"})).unwrap();
        let content = out["content"].as_str().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(content).unwrap();
        assert_eq!(parsed["memory_id"].as_str().unwrap(), id);
        assert_eq!(parsed["namespace"].as_str().unwrap(), "ns");
        assert_eq!(parsed["reflection_depth"].as_i64().unwrap(), 2);
        let fname = out["suggested_filename"].as_str().unwrap();
        assert!(fname.ends_with(".json"));
    }

    #[test]
    fn suggested_filename_strips_slashes() {
        assert_eq!(
            suggested_filename("/team/alpha/", "abc", ExportFormat::Markdown),
            "team/alpha/abc.md"
        );
        assert_eq!(
            suggested_filename("", "abc", ExportFormat::Json),
            "abc.json"
        );
    }
}
