// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_skill_get` handler (L1-5 Agent Skills substrate).
//!
//! Returns the **activation payload** for a skill: full metadata plus the
//! decompressed markdown body. Durable history: `_get(<old_id>)` returns
//! the old version even after it has been superseded.

use rusqlite::Connection;
use serde_json::{Value, json};

/// MCP `memory_skill_get` substrate handler.
///
/// Promoted to `pub` for v0.7.0 Cluster E API-2 (issue #767) so the
/// CLI `ai-memory skill get` subcommand and the HTTP
/// `GET /api/v1/skill/{id}` route can dispatch into the same
/// implementation.
///
/// # Errors
/// Returns a substrate error string when `skill_id` is missing/invalid,
/// the skill is not found, or zstd body decompression fails.
pub fn handle_skill_get(conn: &Connection, params: &Value) -> Result<Value, String> {
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

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use sha2::{Digest as _, Sha256};

    fn open_db() -> rusqlite::Connection {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        let conn = crate::db::open(&path).expect("db::open");
        // Keep the tempdir alive by leaking it for the lifetime of this
        // test process (each test runs in its own DB).
        std::mem::forget(dir);
        conn
    }

    fn insert_min_skill(
        conn: &rusqlite::Connection,
        id: &str,
        ns: &str,
        name: &str,
        body: &str,
    ) -> Vec<u8> {
        let body_blob = zstd::encode_all(body.as_bytes(), 3).unwrap();
        let mut h = Sha256::new();
        h.update(body.as_bytes());
        let digest: Vec<u8> = h.finalize().to_vec();
        let now: i64 = 1_700_000_000;
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, metadata, body_blob, digest, created_at) \
             VALUES (?1, ?2, ?3, 'desc.', '{}', ?4, ?5, ?6)",
            params![id, ns, name, body_blob, digest, now],
        )
        .unwrap();
        digest
    }

    #[test]
    fn rejects_missing_skill_id() {
        let conn = open_db();
        let params = json!({});
        let err = handle_skill_get(&conn, &params).unwrap_err();
        assert!(err.contains("requires 'skill_id'"), "got: {err}");
    }

    #[test]
    fn rejects_empty_skill_id() {
        let conn = open_db();
        let params = json!({"skill_id": ""});
        let err = handle_skill_get(&conn, &params).unwrap_err();
        assert!(err.contains("requires 'skill_id'"), "got: {err}");
    }

    #[test]
    fn rejects_nonstring_skill_id() {
        let conn = open_db();
        let params = json!({"skill_id": 42});
        let err = handle_skill_get(&conn, &params).unwrap_err();
        assert!(err.contains("requires 'skill_id'"), "got: {err}");
    }

    #[test]
    fn returns_not_found_for_missing_id() {
        let conn = open_db();
        let params = json!({"skill_id": "no-such-skill"});
        let err = handle_skill_get(&conn, &params).unwrap_err();
        assert!(err.contains("skill not found"), "got: {err}");
        assert!(err.contains("no-such-skill"), "got: {err}");
    }

    #[test]
    fn returns_minimal_skill_payload() {
        let conn = open_db();
        let id = "11111111-1111-1111-1111-111111111111";
        insert_min_skill(&conn, id, "ns-1", "hello", "# Hello\nbody.");

        let v = handle_skill_get(&conn, &json!({"skill_id": id})).unwrap();
        assert_eq!(v["id"], json!(id));
        assert_eq!(v["namespace"], json!("ns-1"));
        assert_eq!(v["name"], json!("hello"));
        assert_eq!(v["description"], json!("desc."));
        assert_eq!(v["body"].as_str().unwrap(), "# Hello\nbody.");
        assert_eq!(v["current"], json!(true));
        // resources defaults to empty array.
        assert_eq!(v["resources"], json!([]));
        // digest is hex.
        let dig = v["digest"].as_str().unwrap();
        assert_eq!(dig.len(), 64);
        assert!(dig.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn includes_optional_fields_when_present() {
        let conn = open_db();
        let id = "22222222-2222-2222-2222-222222222222";
        let body_blob = zstd::encode_all(b"body".as_slice(), 3).unwrap();
        let digest = vec![0xab_u8; 32];
        let metadata = serde_json::json!({"author": "alice", "version": "1.0"}).to_string();
        let allowed_tools_json = serde_json::to_string(&vec!["tool_a", "tool_b"]).unwrap();
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, license, compatibility, \
                                  allowed_tools, metadata, body_blob, digest, signing_agent, \
                                  created_at) \
             VALUES (?1, 'ns', 'sk', 'D', 'MIT', 'v1', ?2, ?3, ?4, ?5, 'agent:alice', 0)",
            params![id, allowed_tools_json, metadata, body_blob, digest],
        )
        .unwrap();

        let v = handle_skill_get(&conn, &json!({"skill_id": id})).unwrap();
        assert_eq!(v["license"], json!("MIT"));
        assert_eq!(v["compatibility"], json!("v1"));
        assert_eq!(v["signing_agent"], json!("agent:alice"));
        assert_eq!(v["allowed_tools"], json!(["tool_a", "tool_b"]));
        assert_eq!(v["metadata"]["author"], json!("alice"));
        assert_eq!(v["metadata"]["version"], json!("1.0"));
    }

    #[test]
    fn ignores_malformed_allowed_tools_json() {
        let conn = open_db();
        let id = "33333333-3333-3333-3333-333333333333";
        let body_blob = zstd::encode_all(b"body".as_slice(), 3).unwrap();
        let digest = vec![0u8; 32];
        // Malformed allowed_tools — handler must not propagate the error.
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, allowed_tools, metadata, \
                                  body_blob, digest, created_at) \
             VALUES (?1, 'ns', 'sk', 'D', 'not-json', '{}', ?2, ?3, 0)",
            params![id, body_blob, digest],
        )
        .unwrap();

        let v = handle_skill_get(&conn, &json!({"skill_id": id})).unwrap();
        // allowed_tools should NOT appear in the response when parse fails.
        assert!(v.get("allowed_tools").is_none());
    }

    #[test]
    fn marks_superseded_when_chained() {
        let conn = open_db();
        let v1 = "v1aaaaaa-0000-0000-0000-000000000001";
        let v2 = "v2bbbbbb-0000-0000-0000-000000000002";
        insert_min_skill(&conn, v1, "ns", "chain", "body v1");
        insert_min_skill(&conn, v2, "ns", "chain", "body v2");
        conn.execute(
            "UPDATE skills SET superseded_by = ?1 WHERE id = ?2",
            params![v2, v1],
        )
        .unwrap();

        let r1 = handle_skill_get(&conn, &json!({"skill_id": v1})).unwrap();
        assert_eq!(r1["current"], json!(false));
        assert_eq!(r1["superseded_by"], json!(v2));
        let r2 = handle_skill_get(&conn, &json!({"skill_id": v2})).unwrap();
        assert_eq!(r2["current"], json!(true));
        assert!(r2.get("superseded_by").is_none());
    }

    #[test]
    fn includes_resource_list_paths_only() {
        let conn = open_db();
        let id = "44444444-4444-4444-4444-444444444444";
        insert_min_skill(&conn, id, "ns", "withres", "body");
        let rblob = zstd::encode_all(b"echo".as_slice(), 3).unwrap();
        let rdig = vec![0u8; 32];
        conn.execute(
            "INSERT INTO skill_resources (skill_id, resource_path, resource_kind, content_blob, digest) \
             VALUES (?1, 'scripts/run.sh', 'script', ?2, ?3)",
            params![id, rblob, rdig],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO skill_resources (skill_id, resource_path, resource_kind, content_blob, digest) \
             VALUES (?1, 'reference/notes.md', 'reference', ?2, ?3)",
            params![id, rblob, rdig],
        )
        .unwrap();

        let v = handle_skill_get(&conn, &json!({"skill_id": id})).unwrap();
        let resources = v["resources"].as_array().unwrap();
        assert_eq!(resources.len(), 2);
        // Sorted alphabetically by path.
        assert_eq!(resources[0]["path"], json!("reference/notes.md"));
        assert_eq!(resources[0]["kind"], json!("reference"));
        assert_eq!(resources[1]["path"], json!("scripts/run.sh"));
        assert_eq!(resources[1]["kind"], json!("script"));
    }

    #[test]
    fn rejects_corrupt_body_blob() {
        let conn = open_db();
        let id = "55555555-5555-5555-5555-555555555555";
        let bogus_blob: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff]; // not valid zstd
        let digest = vec![0u8; 32];
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, metadata, body_blob, digest, created_at) \
             VALUES (?1, 'ns', 'sk', 'D', '{}', ?2, ?3, 0)",
            params![id, bogus_blob, digest],
        )
        .unwrap();
        let err = handle_skill_get(&conn, &json!({"skill_id": id})).unwrap_err();
        assert!(err.contains("zstd decompress body"), "got: {err}");
    }

    #[test]
    fn malformed_metadata_string_skipped() {
        let conn = open_db();
        let id = "66666666-6666-6666-6666-666666666666";
        let body_blob = zstd::encode_all(b"body".as_slice(), 3).unwrap();
        let digest = vec![0u8; 32];
        // Insert with a non-JSON metadata string.
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, metadata, body_blob, digest, created_at) \
             VALUES (?1, 'ns', 'sk', 'D', 'not valid json', ?2, ?3, 0)",
            params![id, body_blob, digest],
        )
        .unwrap();
        let v = handle_skill_get(&conn, &json!({"skill_id": id})).unwrap();
        // metadata key should NOT be present in response (parse failure).
        assert!(v.get("metadata").is_none());
    }
}
