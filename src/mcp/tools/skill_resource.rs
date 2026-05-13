// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_skill_resource` handler (L1-5 Agent Skills substrate).
//!
//! Returns the decompressed content of a `skill_resources` row,
//! verifying the SHA-256 digest on the way out. Returns an error if
//! the digest does not match (data corruption / tampering).

use rusqlite::Connection;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

pub(super) fn handle_skill_resource(conn: &Connection, params: &Value) -> Result<Value, String> {
    let skill_id = params["skill_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("memory_skill_resource requires 'skill_id'")?;

    let resource_path = params["resource_path"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("memory_skill_resource requires 'resource_path'")?;

    let row: Option<(String, Option<Vec<u8>>, Option<Vec<u8>>)> = conn
        .query_row(
            "SELECT resource_kind, content_blob, digest \
             FROM skill_resources \
             WHERE skill_id = ?1 AND resource_path = ?2",
            [skill_id, resource_path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok();

    let Some((kind, content_blob_opt, stored_digest_opt)) = row else {
        return Err(format!(
            "resource not found: skill_id={skill_id} path={resource_path}"
        ));
    };

    let content_blob = content_blob_opt.ok_or_else(|| {
        format!("resource '{resource_path}' has no inline content (reference-only)")
    })?;

    // Decompress.
    let content_bytes = zstd::decode_all(content_blob.as_slice())
        .map_err(|e| format!("zstd decompress resource: {e}"))?;

    // Verify digest.
    if let Some(ref stored_digest) = stored_digest_opt {
        let mut hasher = Sha256::new();
        hasher.update(&content_bytes);
        let computed: Vec<u8> = hasher.finalize().to_vec();
        if computed != *stored_digest {
            return Err(format!(
                "digest mismatch for resource '{resource_path}' in skill '{skill_id}': \
                 stored={} computed={}",
                hex_encode(stored_digest),
                hex_encode(&computed),
            ));
        }
    }

    // Return as UTF-8 text if possible, else base64.
    let (content_value, encoding) = match String::from_utf8(content_bytes.clone()) {
        Ok(text) => (json!(text), "utf-8"),
        Err(_) => {
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&content_bytes);
            (json!(b64), "base64")
        }
    };

    let digest_hex = stored_digest_opt
        .as_ref()
        .map(|d| hex_encode(d))
        .unwrap_or_default();

    Ok(json!({
        "skill_id": skill_id,
        "resource_path": resource_path,
        "resource_kind": kind,
        "content": content_value,
        "encoding": encoding,
        "digest": digest_hex,
        "digest_verified": stored_digest_opt.is_some(),
    }))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
