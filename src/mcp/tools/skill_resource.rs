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

/// MCP `memory_skill_resource` substrate handler.
///
/// Promoted to `pub` for v0.7.0 Cluster E API-2 (issue #767) so the
/// CLI `ai-memory skill resource` and HTTP routes can dispatch into
/// the same implementation.
///
/// # Errors
/// Returns a substrate error string when `skill_id` / `resource_path`
/// are missing, the row is not found, zstd decompression fails, or
/// the SHA-256 digest mismatches the stored digest (tampering check).
pub fn handle_skill_resource(conn: &Connection, params: &Value) -> Result<Value, String> {
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

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn open_db() -> rusqlite::Connection {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        let conn = crate::db::open(&path).expect("db::open");
        std::mem::forget(dir);
        conn
    }

    fn insert_min_skill(conn: &rusqlite::Connection, id: &str) {
        let body_blob = zstd::encode_all(b"body".as_slice(), 3).unwrap();
        let digest = vec![0u8; 32];
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, metadata, body_blob, digest, created_at) \
             VALUES (?1, 'ns', 'skill', 'D', '{}', ?2, ?3, 0)",
            params![id, body_blob, digest],
        )
        .unwrap();
    }

    fn insert_resource(
        conn: &rusqlite::Connection,
        skill_id: &str,
        path: &str,
        kind: &str,
        content: &[u8],
        digest: Option<&[u8]>,
    ) {
        let blob = zstd::encode_all(content, 3).unwrap();
        let dig_opt: Option<Vec<u8>> = digest.map(<[u8]>::to_vec);
        conn.execute(
            "INSERT INTO skill_resources (skill_id, resource_path, resource_kind, content_blob, digest) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![skill_id, path, kind, blob, dig_opt],
        )
        .unwrap();
    }

    #[test]
    fn rejects_missing_skill_id() {
        let conn = open_db();
        let err = handle_skill_resource(&conn, &json!({"resource_path": "x"})).unwrap_err();
        assert!(err.contains("requires 'skill_id'"));
    }

    #[test]
    fn rejects_empty_skill_id() {
        let conn = open_db();
        let err = handle_skill_resource(&conn, &json!({"skill_id": "", "resource_path": "x"}))
            .unwrap_err();
        assert!(err.contains("requires 'skill_id'"));
    }

    #[test]
    fn rejects_missing_resource_path() {
        let conn = open_db();
        let err = handle_skill_resource(&conn, &json!({"skill_id": "sk"})).unwrap_err();
        assert!(err.contains("requires 'resource_path'"));
    }

    #[test]
    fn rejects_empty_resource_path() {
        let conn = open_db();
        let err = handle_skill_resource(&conn, &json!({"skill_id": "sk", "resource_path": ""}))
            .unwrap_err();
        assert!(err.contains("requires 'resource_path'"));
    }

    #[test]
    fn returns_not_found_for_missing_resource() {
        let conn = open_db();
        insert_min_skill(&conn, "sk1");
        let err = handle_skill_resource(
            &conn,
            &json!({"skill_id": "sk1", "resource_path": "no-such.sh"}),
        )
        .unwrap_err();
        assert!(err.contains("resource not found"));
        assert!(err.contains("sk1"));
        assert!(err.contains("no-such.sh"));
    }

    #[test]
    fn rejects_resource_without_content_blob() {
        let conn = open_db();
        insert_min_skill(&conn, "sk1");
        // Insert resource with NULL content_blob (reference-only).
        conn.execute(
            "INSERT INTO skill_resources (skill_id, resource_path, resource_kind, content_blob, digest) \
             VALUES ('sk1', 'ref.md', 'reference', NULL, NULL)",
            [],
        )
        .unwrap();
        let err = handle_skill_resource(
            &conn,
            &json!({"skill_id": "sk1", "resource_path": "ref.md"}),
        )
        .unwrap_err();
        assert!(err.contains("no inline content"));
    }

    #[test]
    fn returns_utf8_content_with_verified_digest() {
        let conn = open_db();
        insert_min_skill(&conn, "sk1");
        let content = b"#!/bin/bash\necho hello\n";
        let mut h = sha2::Sha256::new();
        h.update(content);
        let dig: Vec<u8> = h.finalize().to_vec();
        insert_resource(
            &conn,
            "sk1",
            "scripts/run.sh",
            "script",
            content,
            Some(&dig),
        );

        let v = handle_skill_resource(
            &conn,
            &json!({"skill_id": "sk1", "resource_path": "scripts/run.sh"}),
        )
        .unwrap();
        assert_eq!(v["skill_id"], json!("sk1"));
        assert_eq!(v["resource_path"], json!("scripts/run.sh"));
        assert_eq!(v["resource_kind"], json!("script"));
        assert_eq!(v["encoding"], json!("utf-8"));
        assert_eq!(v["content"].as_str().unwrap(), "#!/bin/bash\necho hello\n");
        assert_eq!(v["digest_verified"], json!(true));
        let hex_dig = v["digest"].as_str().unwrap();
        assert_eq!(hex_dig.len(), 64);
    }

    #[test]
    fn returns_base64_for_binary_content() {
        let conn = open_db();
        insert_min_skill(&conn, "sk1");
        // Invalid UTF-8 bytes.
        let content: Vec<u8> = vec![0xff, 0xfe, 0xfd, 0x00, 0x01];
        let mut h = sha2::Sha256::new();
        h.update(&content);
        let dig: Vec<u8> = h.finalize().to_vec();
        insert_resource(&conn, "sk1", "asset.bin", "asset", &content, Some(&dig));

        let v = handle_skill_resource(
            &conn,
            &json!({"skill_id": "sk1", "resource_path": "asset.bin"}),
        )
        .unwrap();
        assert_eq!(v["encoding"], json!("base64"));
        // Decode and verify round-trip.
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(v["content"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, content);
    }

    #[test]
    fn detects_digest_mismatch() {
        let conn = open_db();
        insert_min_skill(&conn, "sk1");
        let content = b"original";
        let wrong_dig = vec![0u8; 32]; // wrong digest
        insert_resource(&conn, "sk1", "x.txt", "asset", content, Some(&wrong_dig));

        let err =
            handle_skill_resource(&conn, &json!({"skill_id": "sk1", "resource_path": "x.txt"}))
                .unwrap_err();
        assert!(err.contains("digest mismatch"));
        assert!(err.contains("stored="));
        assert!(err.contains("computed="));
    }

    #[test]
    fn no_digest_returns_unverified() {
        let conn = open_db();
        insert_min_skill(&conn, "sk1");
        let content = b"unsigned content";
        insert_resource(&conn, "sk1", "u.txt", "asset", content, None);

        let v = handle_skill_resource(&conn, &json!({"skill_id": "sk1", "resource_path": "u.txt"}))
            .unwrap();
        assert_eq!(v["digest_verified"], json!(false));
        assert_eq!(v["digest"], json!(""));
        assert_eq!(v["content"].as_str().unwrap(), "unsigned content");
    }

    #[test]
    fn rejects_corrupt_content_blob() {
        let conn = open_db();
        insert_min_skill(&conn, "sk1");
        let bogus: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        conn.execute(
            "INSERT INTO skill_resources (skill_id, resource_path, resource_kind, content_blob, digest) \
             VALUES ('sk1', 'bad.bin', 'asset', ?1, NULL)",
            params![bogus],
        )
        .unwrap();
        let err = handle_skill_resource(
            &conn,
            &json!({"skill_id": "sk1", "resource_path": "bad.bin"}),
        )
        .unwrap_err();
        assert!(err.contains("zstd decompress resource"));
    }

    #[test]
    fn hex_encode_round_trip() {
        // Tests the small hex_encode helper directly.
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_encode(&[0x00, 0xff, 0xab, 0x12]), "00ffab12");
    }
}
