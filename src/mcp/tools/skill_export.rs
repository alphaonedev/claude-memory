// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_skill_export` handler (L1-5 Agent Skills substrate).
//!
//! Writes a skill back to a `target_folder` as a round-trip-compatible
//! SKILL.md file (plus any attached resource files under `resources/`).
//! Re-registering the exported folder via `memory_skill_register` produces
//! the **identical SHA-256 digest** — the round-trip guarantee.
//!
//! A `signed_events` row is appended for the export action (Bucket 1
//! attestation).

use std::path::Path;

use rusqlite::Connection;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::identity::keypair::AgentKeypair;
use crate::signed_events::{SignedEvent, append_signed_event, payload_hash};

pub fn handle_skill_export(
    conn: &Connection,
    params: &Value,
    active_keypair: Option<&AgentKeypair>,
) -> Result<Value, String> {
    let skill_id = params["skill_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("memory_skill_export requires 'skill_id'")?;

    let target_str = params["target_folder"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("memory_skill_export requires 'target_folder'")?;

    let target = Path::new(target_str);

    // -----------------------------------------------------------------------
    // Load skill row
    // -----------------------------------------------------------------------
    let row: Option<(
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        Vec<u8>,
        Vec<u8>,
        Option<String>,
        i64,
    )> = conn
        .query_row(
            "SELECT namespace, name, license, compatibility, allowed_tools, \
                    metadata, body_blob, digest, signing_agent, created_at \
             FROM skills WHERE id = ?1",
            [skill_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            },
        )
        .ok();

    let Some((
        namespace,
        name,
        license,
        compatibility,
        allowed_tools,
        metadata,
        body_blob,
        digest_bytes,
        signing_agent,
        _created_at,
    )) = row
    else {
        return Err(format!("skill not found: {skill_id}"));
    };

    // -----------------------------------------------------------------------
    // Decompress body
    // -----------------------------------------------------------------------
    let body_bytes =
        zstd::decode_all(body_blob.as_slice()).map_err(|e| format!("zstd decompress body: {e}"))?;
    let body = String::from_utf8_lossy(&body_bytes);

    // -----------------------------------------------------------------------
    // Build SKILL.md text (round-trip-stable)
    // -----------------------------------------------------------------------
    let mut fm_lines: Vec<String> = Vec::new();
    fm_lines.push(format!("namespace: {namespace}"));
    fm_lines.push(format!("name: {name}"));

    // Minimal YAML quoting: quote the string if it contains special chars.
    let desc_row: Option<String> = conn
        .query_row(
            "SELECT description FROM skills WHERE id = ?1",
            [skill_id],
            |row| row.get(0),
        )
        .ok();
    if let Some(ref desc) = desc_row {
        fm_lines.push(format!("description: {}", yaml_quote(desc)));
    }

    if let Some(ref lic) = license {
        fm_lines.push(format!("license: {}", yaml_quote(lic)));
    }
    if let Some(ref compat) = compatibility {
        fm_lines.push(format!("compatibility: {}", yaml_quote(compat)));
    }
    if let Some(ref tools_json) = allowed_tools {
        if let Ok(tools_val) = serde_json::from_str::<Vec<String>>(tools_json) {
            if !tools_val.is_empty() {
                fm_lines.push("allowed_tools:".to_string());
                for t in &tools_val {
                    fm_lines.push(format!("  - {t}"));
                }
            }
        }
    }
    // Include non-empty metadata keys (extra frontmatter fields).
    if let Ok(meta_val) = serde_json::from_str::<serde_json::Value>(&metadata) {
        if let Some(obj) = meta_val.as_object() {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    fm_lines.push(format!("{k}: {}", yaml_quote(s)));
                }
            }
        }
    }

    let skill_md_content = format!("---\n{}\n---\n\n{}", fm_lines.join("\n"), body);

    // -----------------------------------------------------------------------
    // Write SKILL.md
    // -----------------------------------------------------------------------
    // v0.7.0 (issue #691 fold-1) — wire the FilesystemWrite gate
    // BEFORE the std::fs::write call. The closure installed by the
    // daemon's bootstrap_serve consults the operator-signed
    // governance_rules table for a refusal verdict (R001/R002/R003
    // glob-based filesystem rules); a refusal short-circuits the
    // export cleanly before any directory is created.
    let skill_md_path = target.join("SKILL.md");
    let skill_md_action = crate::governance::agent_action::AgentAction::FilesystemWrite {
        path: skill_md_path.clone(),
        byte_estimate: Some(skill_md_content.len() as u64),
    };
    if let Err(refusal) = crate::governance::wire_check::check(&skill_md_action) {
        return Err(format!(
            "governance refused SKILL.md write: {}",
            refusal.reason
        ));
    }
    std::fs::create_dir_all(target).map_err(|e| format!("create_dir_all '{target_str}': {e}"))?;
    std::fs::write(&skill_md_path, skill_md_content.as_bytes())
        .map_err(|e| format!("write SKILL.md: {e}"))?;

    // -----------------------------------------------------------------------
    // Export resources
    // -----------------------------------------------------------------------
    let mut res_stmt = conn
        .prepare(
            "SELECT resource_path, resource_kind, content_blob \
             FROM skill_resources WHERE skill_id = ?1",
        )
        .map_err(|e| format!("resources prepare: {e}"))?;

    let mut exported_resources: Vec<String> = Vec::new();
    let rows = res_stmt
        .query_map([skill_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
            ))
        })
        .map_err(|e| format!("resources query: {e}"))?;

    for row in rows {
        let (res_path, _kind, content_blob_opt) = row.map_err(|e| format!("row: {e}"))?;
        if let Some(blob) = content_blob_opt {
            let content = zstd::decode_all(blob.as_slice())
                .map_err(|e| format!("decompress resource '{res_path}': {e}"))?;
            let res_file = target.join("resources").join(&res_path);
            // v0.7.0 (issue #691 fold-1) — per-resource FilesystemWrite
            // gate. Same uniform wire_check shape as the SKILL.md write
            // above; a refusal on any resource halts the export at that
            // file (prior writes are kept — partial exports are visible
            // and recoverable by re-running with a less-restrictive
            // ruleset).
            let res_action = crate::governance::agent_action::AgentAction::FilesystemWrite {
                path: res_file.clone(),
                byte_estimate: Some(content.len() as u64),
            };
            if let Err(refusal) = crate::governance::wire_check::check(&res_action) {
                return Err(format!(
                    "governance refused resource '{res_path}' write: {}",
                    refusal.reason
                ));
            }
            if let Some(parent) = res_file.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create_dir_all for resource: {e}"))?;
            }
            std::fs::write(&res_file, &content)
                .map_err(|e| format!("write resource '{res_path}': {e}"))?;
            exported_resources.push(res_path);
        }
    }

    // -----------------------------------------------------------------------
    // Signed event for export
    // -----------------------------------------------------------------------
    let event_payload = json!({
        "skill_id": skill_id,
        "namespace": namespace,
        "name": name,
        "action": "export",
        "target_folder": target_str,
    });
    let ev_bytes = serde_json::to_vec(&event_payload).unwrap_or_default();
    let ev_hash = payload_hash(&ev_bytes);
    let agent_id = active_keypair
        .map(|kp| kp.agent_id.clone())
        .or(signing_agent.clone())
        .unwrap_or_else(|| "anonymous".to_string());
    let event = SignedEvent {
        id: Uuid::new_v4().to_string(),
        agent_id: agent_id.clone(),
        event_type: "skill.exported".to_string(),
        payload_hash: ev_hash,
        signature: None,
        attest_level: "unsigned".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    let _ = append_signed_event(conn, &event);

    let digest_hex: String = digest_bytes.iter().map(|b| format!("{b:02x}")).collect();

    Ok(json!({
        "exported": true,
        "skill_id": skill_id,
        "target_folder": target_str,
        "digest": digest_hex,
        "resources_exported": exported_resources.len(),
        "files": exported_resources,
    }))
}

/// Minimal YAML quoting: wrap in double quotes if the value contains
/// `:`, `#`, `"`, `'`, `\n`, or leading/trailing whitespace.
fn yaml_quote(s: &str) -> String {
    let needs_quoting = s.contains(':')
        || s.contains('#')
        || s.contains('"')
        || s.contains('\'')
        || s.contains('\n')
        || s.starts_with(' ')
        || s.ends_with(' ');
    if needs_quoting {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}
