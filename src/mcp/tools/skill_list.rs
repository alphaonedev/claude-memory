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

/// MCP `memory_skill_list` substrate handler.
///
/// Promoted to `pub` for v0.7.0 Cluster E API-2 (issue #767) so the
/// CLI `ai-memory skill list` subcommand and the HTTP
/// `GET /api/v1/skill/list` route can dispatch into the same
/// implementation without duplicating SQL.
///
/// # Errors
/// Returns the substrate's error string when SQL prepare/query fails.
pub fn handle_skill_list(conn: &Connection, params: &Value) -> Result<Value, String> {
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

    fn insert_skill(
        conn: &rusqlite::Connection,
        id: &str,
        ns: &str,
        name: &str,
        description: &str,
        created_at: i64,
    ) {
        let body_blob = zstd::encode_all(b"body".as_slice(), 3).unwrap();
        let digest = vec![0u8; 32];
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, metadata, body_blob, digest, created_at) \
             VALUES (?1, ?2, ?3, ?4, '{}', ?5, ?6, ?7)",
            params![id, ns, name, description, body_blob, digest, created_at],
        )
        .unwrap();
    }

    #[test]
    fn empty_db_returns_empty_list() {
        let conn = open_db();
        let v = handle_skill_list(&conn, &json!({})).unwrap();
        assert_eq!(v["count"], json!(0));
        assert_eq!(v["skills"], json!([]));
    }

    #[test]
    fn returns_only_current_non_superseded() {
        let conn = open_db();
        insert_skill(&conn, "id-old", "ns", "name", "old", 0);
        insert_skill(&conn, "id-new", "ns", "name", "new", 1);
        conn.execute(
            "UPDATE skills SET superseded_by = 'id-new' WHERE id = 'id-old'",
            [],
        )
        .unwrap();

        let v = handle_skill_list(&conn, &json!({})).unwrap();
        assert_eq!(v["count"], json!(1));
        let arr = v["skills"].as_array().unwrap();
        assert_eq!(arr[0]["id"], json!("id-new"));
    }

    #[test]
    fn filters_by_namespace() {
        let conn = open_db();
        insert_skill(&conn, "a", "ns-a", "ska", "a", 0);
        insert_skill(&conn, "b", "ns-b", "skb", "b", 1);

        let v = handle_skill_list(&conn, &json!({"namespace": "ns-a"})).unwrap();
        assert_eq!(v["count"], json!(1));
        assert_eq!(v["skills"][0]["namespace"], json!("ns-a"));
    }

    #[test]
    fn wildcard_namespace_returns_all() {
        let conn = open_db();
        insert_skill(&conn, "a", "ns-a", "ska", "a", 0);
        insert_skill(&conn, "b", "ns-b", "skb", "b", 1);

        let v = handle_skill_list(&conn, &json!({"namespace": "%"})).unwrap();
        assert_eq!(v["count"], json!(2));
    }

    #[test]
    fn no_namespace_defaults_to_wildcard() {
        let conn = open_db();
        insert_skill(&conn, "a", "ns-a", "ska", "a", 0);
        insert_skill(&conn, "b", "ns-b", "skb", "b", 1);

        let v = handle_skill_list(&conn, &json!({})).unwrap();
        assert_eq!(v["count"], json!(2));
    }

    #[test]
    fn filter_matches_name() {
        let conn = open_db();
        insert_skill(&conn, "a", "ns", "deploy-canary", "k8s canary deploy", 0);
        insert_skill(&conn, "b", "ns", "audit-logs", "fetch audit logs", 1);

        let v = handle_skill_list(&conn, &json!({"filter": "canary"})).unwrap();
        assert_eq!(v["count"], json!(1));
        assert_eq!(v["skills"][0]["id"], json!("a"));
    }

    #[test]
    fn filter_matches_description() {
        let conn = open_db();
        insert_skill(&conn, "a", "ns", "x", "kubernetes deploy notes", 0);
        insert_skill(&conn, "b", "ns", "y", "totally different", 1);

        let v = handle_skill_list(&conn, &json!({"filter": "kubernetes"})).unwrap();
        assert_eq!(v["count"], json!(1));
    }

    #[test]
    fn filter_no_match_returns_empty() {
        let conn = open_db();
        insert_skill(&conn, "a", "ns", "x", "k8s", 0);

        let v = handle_skill_list(&conn, &json!({"filter": "no-such-text"})).unwrap();
        assert_eq!(v["count"], json!(0));
    }

    #[test]
    fn includes_optional_columns_when_present() {
        let conn = open_db();
        let body_blob = zstd::encode_all(b"body".as_slice(), 3).unwrap();
        let digest = vec![0xab_u8; 32];
        let allowed_tools = serde_json::to_string(&vec!["tool1"]).unwrap();
        let metadata = serde_json::json!({"author": "alice"}).to_string();
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, license, compatibility, \
                                  allowed_tools, metadata, body_blob, digest, signing_agent, \
                                  created_at) \
             VALUES ('id1', 'ns', 'name', 'd', 'MIT', 'v1', ?1, ?2, ?3, ?4, 'agent:bob', 0)",
            params![allowed_tools, metadata, body_blob, digest],
        )
        .unwrap();

        let v = handle_skill_list(&conn, &json!({})).unwrap();
        let entry = &v["skills"][0];
        assert_eq!(entry["license"], json!("MIT"));
        assert_eq!(entry["compatibility"], json!("v1"));
        assert_eq!(entry["signing_agent"], json!("agent:bob"));
        assert_eq!(entry["allowed_tools"], json!(["tool1"]));
        assert_eq!(entry["metadata"]["author"], json!("alice"));
        // digest is hex.
        let dig = entry["digest"].as_str().unwrap();
        assert_eq!(dig.len(), 64);
    }

    #[test]
    fn omits_empty_metadata_object() {
        let conn = open_db();
        insert_skill(&conn, "a", "ns", "x", "d", 0);
        let v = handle_skill_list(&conn, &json!({})).unwrap();
        let entry = &v["skills"][0];
        // metadata `{}` is omitted from the response.
        assert!(entry.get("metadata").is_none());
    }

    #[test]
    fn ignores_malformed_allowed_tools_json() {
        let conn = open_db();
        let body_blob = zstd::encode_all(b"body".as_slice(), 3).unwrap();
        let digest = vec![0u8; 32];
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, allowed_tools, metadata, \
                                  body_blob, digest, created_at) \
             VALUES ('id1', 'ns', 'x', 'd', 'not-json', '{}', ?1, ?2, 0)",
            params![body_blob, digest],
        )
        .unwrap();
        let v = handle_skill_list(&conn, &json!({})).unwrap();
        // allowed_tools should be absent on parse failure.
        assert!(v["skills"][0].get("allowed_tools").is_none());
    }

    #[test]
    fn empty_filter_string_is_no_filter() {
        let conn = open_db();
        insert_skill(&conn, "a", "ns", "x", "d", 0);
        insert_skill(&conn, "b", "ns", "y", "e", 1);
        let v = handle_skill_list(&conn, &json!({"filter": ""})).unwrap();
        assert_eq!(v["count"], json!(2));
    }
}
