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
        ..SignedEvent::default()
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

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn open_db() -> (rusqlite::Connection, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("test.db");
        let conn = crate::db::open(&path).expect("db::open");
        (conn, dir)
    }

    fn insert_skill_full(
        conn: &rusqlite::Connection,
        id: &str,
        ns: &str,
        name: &str,
        description: &str,
        body: &str,
    ) {
        let body_blob = zstd::encode_all(body.as_bytes(), 3).unwrap();
        let digest = vec![0xab_u8; 32];
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, metadata, body_blob, digest, created_at) \
             VALUES (?1, ?2, ?3, ?4, '{}', ?5, ?6, 0)",
            params![id, ns, name, description, body_blob, digest],
        )
        .unwrap();
    }

    // ---- input validation ------------------------------------------------

    #[test]
    fn rejects_missing_skill_id() {
        let (conn, _dir) = open_db();
        let err =
            handle_skill_export(&conn, &json!({"target_folder": "/tmp/x"}), None).unwrap_err();
        assert!(err.contains("requires 'skill_id'"));
    }

    #[test]
    fn rejects_empty_skill_id() {
        let (conn, _dir) = open_db();
        let err = handle_skill_export(
            &conn,
            &json!({"skill_id": "", "target_folder": "/tmp/x"}),
            None,
        )
        .unwrap_err();
        assert!(err.contains("requires 'skill_id'"));
    }

    #[test]
    fn rejects_missing_target_folder() {
        let (conn, _dir) = open_db();
        let err = handle_skill_export(&conn, &json!({"skill_id": "sk"}), None).unwrap_err();
        assert!(err.contains("requires 'target_folder'"));
    }

    #[test]
    fn rejects_empty_target_folder() {
        let (conn, _dir) = open_db();
        let err = handle_skill_export(&conn, &json!({"skill_id": "sk", "target_folder": ""}), None)
            .unwrap_err();
        assert!(err.contains("requires 'target_folder'"));
    }

    #[test]
    fn returns_not_found_for_missing_skill() {
        let (conn, dir) = open_db();
        let target = dir.path().join("out");
        let err = handle_skill_export(
            &conn,
            &json!({"skill_id": "no-such", "target_folder": target.to_str().unwrap()}),
            None,
        )
        .unwrap_err();
        assert!(err.contains("skill not found"));
    }

    // ---- happy path ------------------------------------------------------

    #[test]
    fn exports_skill_md_with_minimal_frontmatter() {
        let (conn, dir) = open_db();
        let id = "1aaaaaaa-0000-0000-0000-000000000001";
        insert_skill_full(
            &conn,
            id,
            "ns-a",
            "my-skill",
            "A short description.",
            "Body content here.\n",
        );

        let target = dir.path().join("export-min");
        let v = handle_skill_export(
            &conn,
            &json!({"skill_id": id, "target_folder": target.to_str().unwrap()}),
            None,
        )
        .unwrap();
        assert_eq!(v["exported"], json!(true));
        assert_eq!(v["skill_id"], json!(id));
        assert_eq!(v["resources_exported"], json!(0));
        assert_eq!(v["files"], json!([]));

        let skill_md = std::fs::read_to_string(target.join("SKILL.md")).unwrap();
        assert!(skill_md.starts_with("---\n"));
        assert!(skill_md.contains("namespace: ns-a"));
        assert!(skill_md.contains("name: my-skill"));
        assert!(skill_md.contains("description: A short description."));
        assert!(skill_md.contains("Body content here."));
    }

    #[test]
    fn exports_skill_with_optional_fields() {
        let (conn, dir) = open_db();
        let body_blob = zstd::encode_all(b"body".as_slice(), 3).unwrap();
        let digest = vec![0u8; 32];
        let allowed_tools = serde_json::to_string(&vec!["tool_a", "tool_b"]).unwrap();
        let metadata = serde_json::json!({"author": "alice"}).to_string();
        let id = "2bbbbbbb-0000-0000-0000-000000000002";
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, license, compatibility, \
                                  allowed_tools, metadata, body_blob, digest, signing_agent, \
                                  created_at) \
             VALUES (?1, 'ns', 'name', 'desc', 'MIT', 'v1', ?2, ?3, ?4, ?5, 'agent:x', 0)",
            params![id, allowed_tools, metadata, body_blob, digest],
        )
        .unwrap();

        let target = dir.path().join("export-opt");
        let v = handle_skill_export(
            &conn,
            &json!({"skill_id": id, "target_folder": target.to_str().unwrap()}),
            None,
        )
        .unwrap();
        assert_eq!(v["exported"], json!(true));
        let md = std::fs::read_to_string(target.join("SKILL.md")).unwrap();
        assert!(md.contains("license: MIT"));
        assert!(md.contains("compatibility: v1"));
        assert!(md.contains("allowed_tools:"));
        assert!(md.contains("- tool_a"));
        assert!(md.contains("- tool_b"));
        assert!(md.contains("author: alice"));
    }

    #[test]
    fn exports_resources_to_subdir() {
        let (conn, dir) = open_db();
        let id = "3cccc-0000-0000-0000-000000000003";
        insert_skill_full(&conn, id, "ns", "name", "d", "body");
        let blob1 = zstd::encode_all(b"echo hi\n".as_slice(), 3).unwrap();
        let blob2 = zstd::encode_all(b"# Notes\n".as_slice(), 3).unwrap();
        let dig = vec![0u8; 32];
        conn.execute(
            "INSERT INTO skill_resources (skill_id, resource_path, resource_kind, content_blob, digest) \
             VALUES (?1, 'scripts/run.sh', 'script', ?2, ?3)",
            params![id, blob1, dig],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO skill_resources (skill_id, resource_path, resource_kind, content_blob, digest) \
             VALUES (?1, 'reference/notes.md', 'reference', ?2, ?3)",
            params![id, blob2, dig],
        )
        .unwrap();
        // A reference-only resource (no inline content) — must be silently skipped on export.
        conn.execute(
            "INSERT INTO skill_resources (skill_id, resource_path, resource_kind, content_blob, digest) \
             VALUES (?1, 'placeholder.md', 'reference', NULL, NULL)",
            params![id],
        )
        .unwrap();

        let target = dir.path().join("export-res");
        let v = handle_skill_export(
            &conn,
            &json!({"skill_id": id, "target_folder": target.to_str().unwrap()}),
            None,
        )
        .unwrap();
        assert_eq!(v["resources_exported"], json!(2));
        let files = v["files"].as_array().unwrap();
        assert_eq!(files.len(), 2);

        let script_body = std::fs::read(target.join("resources/scripts/run.sh")).unwrap();
        assert_eq!(script_body, b"echo hi\n");
        let ref_body = std::fs::read(target.join("resources/reference/notes.md")).unwrap();
        assert_eq!(ref_body, b"# Notes\n");
    }

    #[test]
    fn exports_with_active_keypair_uses_agent_id() {
        // Build an AgentKeypair (public-only) and run export. We're not
        // checking the signed_events row directly; this path simply
        // exercises the `active_keypair.map(|kp| kp.agent_id.clone())`
        // branch versus the fallback.
        let (conn, dir) = open_db();
        let id = "4dddd-0000-0000-0000-000000000004";
        insert_skill_full(&conn, id, "ns", "name", "d", "body");

        use ed25519_dalek::{SigningKey, VerifyingKey};
        let mut rng = rand_core::OsRng;
        let sk = SigningKey::generate(&mut rng);
        let vk: VerifyingKey = (&sk).into();
        let kp = crate::identity::keypair::AgentKeypair {
            agent_id: "test:agent-1".to_string(),
            public: vk,
            private: Some(sk),
        };

        let target = dir.path().join("export-kp");
        let v = handle_skill_export(
            &conn,
            &json!({"skill_id": id, "target_folder": target.to_str().unwrap()}),
            Some(&kp),
        )
        .unwrap();
        assert_eq!(v["exported"], json!(true));
    }

    #[test]
    fn export_with_signing_agent_in_db_uses_that() {
        // When no keypair is supplied, agent_id falls back to the
        // skill's signing_agent column (when present).
        let (conn, dir) = open_db();
        let body_blob = zstd::encode_all(b"body".as_slice(), 3).unwrap();
        let digest = vec![0u8; 32];
        let id = "5eeee-0000-0000-0000-000000000005";
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, metadata, body_blob, digest, signing_agent, created_at) \
             VALUES (?1, 'ns', 'name', 'd', '{}', ?2, ?3, 'agent:from-db', 0)",
            params![id, body_blob, digest],
        )
        .unwrap();

        let target = dir.path().join("export-dbagent");
        let v = handle_skill_export(
            &conn,
            &json!({"skill_id": id, "target_folder": target.to_str().unwrap()}),
            None,
        )
        .unwrap();
        assert_eq!(v["exported"], json!(true));
    }

    // ---- corrupt blob path ----------------------------------------------

    #[test]
    fn rejects_corrupt_body_blob() {
        let (conn, dir) = open_db();
        let id = "6ffff-0000-0000-0000-000000000006";
        let bogus: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        let digest = vec![0u8; 32];
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, metadata, body_blob, digest, created_at) \
             VALUES (?1, 'ns', 'name', 'd', '{}', ?2, ?3, 0)",
            params![id, bogus, digest],
        )
        .unwrap();
        let target = dir.path().join("export-corrupt");
        let err = handle_skill_export(
            &conn,
            &json!({"skill_id": id, "target_folder": target.to_str().unwrap()}),
            None,
        )
        .unwrap_err();
        assert!(err.contains("zstd decompress body"));
    }

    #[test]
    fn rejects_corrupt_resource_blob() {
        let (conn, dir) = open_db();
        let id = "7gggg-0000-0000-0000-000000000007";
        insert_skill_full(&conn, id, "ns", "name", "d", "body");
        let bogus: Vec<u8> = vec![0xff, 0xff, 0xff, 0xff];
        let dig = vec![0u8; 32];
        conn.execute(
            "INSERT INTO skill_resources (skill_id, resource_path, resource_kind, content_blob, digest) \
             VALUES (?1, 'bad.bin', 'asset', ?2, ?3)",
            params![id, bogus, dig],
        )
        .unwrap();
        let target = dir.path().join("export-bad-res");
        let err = handle_skill_export(
            &conn,
            &json!({"skill_id": id, "target_folder": target.to_str().unwrap()}),
            None,
        )
        .unwrap_err();
        assert!(err.contains("decompress resource"));
    }

    // ---- yaml_quote helper ----------------------------------------------

    #[test]
    fn yaml_quote_plain_string_unchanged() {
        assert_eq!(yaml_quote("simple"), "simple");
        assert_eq!(yaml_quote("a-b_c.d"), "a-b_c.d");
    }

    #[test]
    fn yaml_quote_special_chars_wrapped() {
        assert_eq!(yaml_quote("a:b"), "\"a:b\"");
        assert_eq!(yaml_quote("a#b"), "\"a#b\"");
        assert_eq!(yaml_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(yaml_quote("a'b"), "\"a'b\"");
        assert_eq!(yaml_quote("a\nb"), "\"a\nb\"");
    }

    #[test]
    fn yaml_quote_leading_trailing_whitespace_wrapped() {
        assert_eq!(yaml_quote(" leading"), "\" leading\"");
        assert_eq!(yaml_quote("trailing "), "\"trailing \"");
    }

    #[test]
    fn export_with_malformed_metadata_skips_extra_fields() {
        let (conn, dir) = open_db();
        let body_blob = zstd::encode_all(b"body".as_slice(), 3).unwrap();
        let digest = vec![0u8; 32];
        let id = "8hhhh-0000-0000-0000-000000000008";
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, metadata, body_blob, digest, created_at) \
             VALUES (?1, 'ns', 'name', 'd', 'not-json', ?2, ?3, 0)",
            params![id, body_blob, digest],
        )
        .unwrap();
        let target = dir.path().join("export-bad-meta");
        let v = handle_skill_export(
            &conn,
            &json!({"skill_id": id, "target_folder": target.to_str().unwrap()}),
            None,
        )
        .unwrap();
        assert_eq!(v["exported"], json!(true));
    }

    #[test]
    fn export_with_malformed_allowed_tools_json_skips() {
        let (conn, dir) = open_db();
        let body_blob = zstd::encode_all(b"body".as_slice(), 3).unwrap();
        let digest = vec![0u8; 32];
        let id = "9iiii-0000-0000-0000-000000000009";
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, allowed_tools, metadata, body_blob, digest, created_at) \
             VALUES (?1, 'ns', 'name', 'd', 'not-json-array', '{}', ?2, ?3, 0)",
            params![id, body_blob, digest],
        )
        .unwrap();
        let target = dir.path().join("export-bad-tools");
        let v = handle_skill_export(
            &conn,
            &json!({"skill_id": id, "target_folder": target.to_str().unwrap()}),
            None,
        )
        .unwrap();
        assert_eq!(v["exported"], json!(true));
        let md = std::fs::read_to_string(target.join("SKILL.md")).unwrap();
        // No allowed_tools section should appear (parse failed).
        assert!(!md.contains("allowed_tools:"));
    }

    #[test]
    fn export_with_empty_allowed_tools_array_omits_section() {
        let (conn, dir) = open_db();
        let body_blob = zstd::encode_all(b"body".as_slice(), 3).unwrap();
        let digest = vec![0u8; 32];
        let id = "aiiii-0000-0000-0000-00000000000a";
        let empty_tools = serde_json::to_string(&Vec::<String>::new()).unwrap();
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, allowed_tools, metadata, body_blob, digest, created_at) \
             VALUES (?1, 'ns', 'name', 'd', ?2, '{}', ?3, ?4, 0)",
            params![id, empty_tools, body_blob, digest],
        )
        .unwrap();
        let target = dir.path().join("export-empty-tools");
        let _ = handle_skill_export(
            &conn,
            &json!({"skill_id": id, "target_folder": target.to_str().unwrap()}),
            None,
        )
        .unwrap();
        let md = std::fs::read_to_string(target.join("SKILL.md")).unwrap();
        assert!(!md.contains("allowed_tools:"));
    }

    #[test]
    fn export_with_metadata_array_value_skipped() {
        // Metadata fields with non-string values are skipped in the
        // frontmatter — only string-valued keys are exported.
        let (conn, dir) = open_db();
        let body_blob = zstd::encode_all(b"body".as_slice(), 3).unwrap();
        let digest = vec![0u8; 32];
        let id = "bjjjj-0000-0000-0000-00000000000b";
        let meta = serde_json::json!({"author": "alice", "version_int": 7, "tags": ["a", "b"]})
            .to_string();
        conn.execute(
            "INSERT INTO skills (id, namespace, name, description, metadata, body_blob, digest, created_at) \
             VALUES (?1, 'ns', 'name', 'd', ?2, ?3, ?4, 0)",
            params![id, meta, body_blob, digest],
        )
        .unwrap();
        let target = dir.path().join("export-meta-array");
        let _ = handle_skill_export(
            &conn,
            &json!({"skill_id": id, "target_folder": target.to_str().unwrap()}),
            None,
        )
        .unwrap();
        let md = std::fs::read_to_string(target.join("SKILL.md")).unwrap();
        assert!(md.contains("author: alice"));
        assert!(!md.contains("version_int:")); // integer skipped
        assert!(!md.contains("tags:")); // array skipped
    }
}
