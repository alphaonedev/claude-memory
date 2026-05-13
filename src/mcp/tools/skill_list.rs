// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_skill_list` handler (L1-5 Agent Skills substrate).
//!
//! Returns the **discovery payload** for all current (non-superseded)
//! skills in a given namespace. Each entry carries ~100 tokens of
//! metadata (name, description, id, namespace, created_at, digest_hex)
//! but does NOT decompress or return the `body_blob`.

use rusqlite::Connection;
use serde_json::{Value, json};

pub(super) fn handle_skill_list(conn: &Connection, params: &Value) -> Result<Value, String> {
    let namespace = params["namespace"].as_str().unwrap_or("%");
    let filter = params["filter"].as_str().unwrap_or("");

    // Only return current (non-superseded) skills.
    let mut stmt = conn
        .prepare(
            "SELECT id, namespace, name, description, license, compatibility, \
                    allowed_tools, metadata, digest, signing_agent, created_at \
             FROM skills \
             WHERE superseded_by IS NULL \
               AND (namespace = ?1 OR ?1 = '%') \
             ORDER BY namespace, name, created_at DESC",
        )
        .map_err(|e| format!("skill_list prepare: {e}"))?;

    let mut skills: Vec<Value> = Vec::new();
    let rows = stmt
        .query_map([namespace], |row| {
            Ok((
                row.get::<_, String>(0)?,         // id
                row.get::<_, String>(1)?,         // namespace
                row.get::<_, String>(2)?,         // name
                row.get::<_, String>(3)?,         // description
                row.get::<_, Option<String>>(4)?, // license
                row.get::<_, Option<String>>(5)?, // compatibility
                row.get::<_, Option<String>>(6)?, // allowed_tools
                row.get::<_, String>(7)?,         // metadata
                row.get::<_, Vec<u8>>(8)?,        // digest
                row.get::<_, Option<String>>(9)?, // signing_agent
                row.get::<_, i64>(10)?,           // created_at
            ))
        })
        .map_err(|e| format!("skill_list query: {e}"))?;

    for row in rows {
        let (
            id,
            ns,
            name,
            description,
            license,
            compatibility,
            allowed_tools,
            metadata,
            digest_bytes,
            signing_agent,
            created_at,
        ) = row.map_err(|e| format!("skill_list row: {e}"))?;

        // Apply optional text filter on name or description.
        if !filter.is_empty() && !name.contains(filter) && !description.contains(filter) {
            continue;
        }

        let digest_hex: String = digest_bytes.iter().map(|b| format!("{b:02x}")).collect();

        let mut entry = json!({
            "id": id,
            "namespace": ns,
            "name": name,
            "description": description,
            "digest": digest_hex,
            "created_at": created_at,
        });

        if let Some(lic) = license {
            entry["license"] = json!(lic);
        }
        if let Some(compat) = compatibility {
            entry["compatibility"] = json!(compat);
        }
        if let Some(tools_json) = allowed_tools {
            if let Ok(v) = serde_json::from_str::<Value>(&tools_json) {
                entry["allowed_tools"] = v;
            }
        }
        if let Some(agent) = signing_agent {
            entry["signing_agent"] = json!(agent);
        }
        // metadata is a JSON string — include it parsed.
        if let Ok(meta_val) = serde_json::from_str::<Value>(&metadata) {
            if !meta_val.as_object().map_or(true, |m| m.is_empty()) {
                entry["metadata"] = meta_val;
            }
        }

        skills.push(entry);
    }

    Ok(json!({
        "count": skills.len(),
        "skills": skills,
    }))
}
