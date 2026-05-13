// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_skill_get` handler (L1-5 Agent Skills substrate).
//!
//! Returns the **activation payload** for a skill: full metadata plus the
//! decompressed markdown body. Durable history: `_get(<old_id>)` returns
//! the old version even after it has been superseded.

use rusqlite::Connection;
use serde_json::{Value, json};

pub(super) fn handle_skill_get(conn: &Connection, params: &Value) -> Result<Value, String> {
    let skill_id = params["skill_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("memory_skill_get requires 'skill_id'")?;

    let row: Option<(
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
        Vec<u8>,
        Vec<u8>,
        Option<Vec<u8>>,
        Option<String>,
        i64,
        Option<String>,
    )> = conn
        .query_row(
            "SELECT id, namespace, name, description, license, compatibility, \
                    allowed_tools, metadata, body_blob, digest, signature, \
                    signing_agent, created_at, superseded_by \
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
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                ))
            },
        )
        .ok();

    let Some((
        id,
        namespace,
        name,
        description,
        license,
        compatibility,
        allowed_tools,
        metadata,
        body_blob,
        digest_bytes,
        _signature,
        signing_agent,
        created_at,
        superseded_by,
    )) = row
    else {
        return Err(format!("skill not found: {skill_id}"));
    };

    // Decompress body.
    let body_bytes =
        zstd::decode_all(body_blob.as_slice()).map_err(|e| format!("zstd decompress body: {e}"))?;
    let body = String::from_utf8_lossy(&body_bytes).into_owned();

    let digest_hex: String = digest_bytes.iter().map(|b| format!("{b:02x}")).collect();

    let mut response = json!({
        "id": id,
        "namespace": namespace,
        "name": name,
        "description": description,
        "digest": digest_hex,
        "created_at": created_at,
        "body": body,
        "current": superseded_by.is_none(),
    });

    if let Some(lic) = license {
        response["license"] = json!(lic);
    }
    if let Some(compat) = compatibility {
        response["compatibility"] = json!(compat);
    }
    if let Some(tools_json) = allowed_tools {
        if let Ok(v) = serde_json::from_str::<Value>(&tools_json) {
            response["allowed_tools"] = v;
        }
    }
    if let Some(agent) = signing_agent {
        response["signing_agent"] = json!(agent);
    }
    if let Some(sup_id) = superseded_by {
        response["superseded_by"] = json!(sup_id);
    }
    if let Ok(meta_val) = serde_json::from_str::<Value>(&metadata) {
        response["metadata"] = meta_val;
    }

    // Include resource list (paths only — content via memory_skill_resource).
    let mut res_stmt = conn
        .prepare(
            "SELECT resource_path, resource_kind FROM skill_resources \
             WHERE skill_id = ?1 ORDER BY resource_path",
        )
        .map_err(|e| format!("resources prepare: {e}"))?;

    let resources: Vec<Value> = res_stmt
        .query_map([&id], |row| {
            Ok(json!({
                "path": row.get::<_, String>(0)?,
                "kind": row.get::<_, String>(1)?,
            }))
        })
        .map_err(|e| format!("resources query: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    response["resources"] = json!(resources);

    Ok(response)
}
