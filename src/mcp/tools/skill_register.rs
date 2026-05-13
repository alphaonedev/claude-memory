// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_skill_register` handler (L1-5 Agent Skills substrate).
//!
//! Registers a SKILL.md-format skill into the `skills` table. Accepts
//! either:
//! - `folder_path` — a directory containing `SKILL.md` plus optional
//!   resource files, **or**
//! - `inline_skill` — the raw SKILL.md text as a string.
//!
//! Registration is idempotent with respect to digest: re-registering
//! the same content produces the same SHA-256 digest and creates a new
//! row (version chain). The previous current row's `superseded_by` is
//! set to the new row's id.
//!
//! # Ed25519 attestation
//!
//! When an `active_keypair` is provided the digest is signed with the
//! agent's private key and the `signing_agent` column is populated.
//! The matching `signed_events` row is appended for the Bucket 1
//! attestation chain.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::identity::keypair::AgentKeypair;
use crate::parsing::skill_md;
use crate::signed_events::{SignedEvent, append_signed_event, payload_hash};

// ---------------------------------------------------------------------------
// Digest computation
// ---------------------------------------------------------------------------

/// Compute the canonical SHA-256 digest over the skill's signing surface:
///   `canonical_frontmatter_json_bytes || body_bytes || sorted_resource_digests`
///
/// `resource_digests` is a sorted list of per-resource SHA-256 hashes
/// (empty when no resources are attached).
pub(super) fn compute_skill_digest(
    canonical_fm: &[u8],
    body_bytes: &[u8],
    mut resource_digests: Vec<Vec<u8>>,
) -> Vec<u8> {
    resource_digests.sort();
    let mut hasher = Sha256::new();
    hasher.update(canonical_fm);
    hasher.update(body_bytes);
    for rd in &resource_digests {
        hasher.update(rd);
    }
    hasher.finalize().to_vec()
}

/// Compute a per-resource SHA-256 over decompressed bytes.
pub(super) fn resource_digest(content: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(content);
    hasher.finalize().to_vec()
}

// ---------------------------------------------------------------------------
// zstd helpers
// ---------------------------------------------------------------------------

fn compress(data: &[u8]) -> Result<Vec<u8>, String> {
    zstd::encode_all(data, 3).map_err(|e| format!("zstd compress error: {e}"))
}

// ---------------------------------------------------------------------------
// Internal registration core
// ---------------------------------------------------------------------------

/// Outcome of a successful skill registration.
pub(super) struct RegisterResult {
    pub id: String,
    pub digest: Vec<u8>,
    pub superseded: Option<String>,
}

/// Core registration logic shared by the folder and inline paths.
///
/// `canonical_fm_json` is the sorted JSON encoding of the frontmatter
/// fields that go into the digest surface.
pub(super) fn register_core(
    conn: &Connection,
    namespace: &str,
    name: &str,
    description: &str,
    license: Option<&str>,
    compatibility: Option<&str>,
    allowed_tools: &[String],
    metadata: &Value,
    body_bytes: &[u8],
    resource_digests: Vec<Vec<u8>>,
    resources: &[(String, String, Vec<u8>)], // (path, kind, content)
    active_keypair: Option<&AgentKeypair>,
) -> Result<RegisterResult, String> {
    // Build canonical frontmatter JSON for digest computation.
    let canonical_fm = serde_json::to_vec(&json!({
        "namespace": namespace,
        "name": name,
        "description": description,
        "license": license,
        "compatibility": compatibility,
        "allowed_tools": allowed_tools,
    }))
    .map_err(|e| format!("frontmatter JSON error: {e}"))?;

    let digest = compute_skill_digest(&canonical_fm, body_bytes, resource_digests);

    // Sign if keypair available.
    let (signature_bytes, signing_agent_str): (Option<Vec<u8>>, Option<String>) =
        if let Some(kp) = active_keypair {
            use ed25519_dalek::Signer as _;
            let sig = kp.private.as_ref().map(|sk| {
                let signing_key = ed25519_dalek::SigningKey::from_bytes(
                    sk.as_bytes()
                        .try_into()
                        .expect("ed25519 signing key is always 32 bytes"),
                );
                signing_key.sign(&digest).to_bytes().to_vec()
            });
            (sig, Some(kp.agent_id.clone()))
        } else {
            (None, None)
        };

    let allowed_tools_json =
        serde_json::to_string(allowed_tools).map_err(|e| format!("allowed_tools JSON: {e}"))?;
    let metadata_json =
        serde_json::to_string(metadata).map_err(|e| format!("metadata JSON: {e}"))?;

    let body_blob = compress(body_bytes)?;

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let new_id = Uuid::new_v4().to_string();

    // Find the current (non-superseded) row for this (namespace, name).
    let prev_id: Option<String> = conn
        .query_row(
            "SELECT id FROM skills WHERE namespace = ?1 AND name = ?2 AND superseded_by IS NULL",
            params![namespace, name],
            |row| row.get(0),
        )
        .ok();

    // Insert new row.
    conn.execute(
        "INSERT INTO skills \
            (id, namespace, name, description, license, compatibility, \
             allowed_tools, metadata, body_blob, digest, signature, \
             signing_agent, created_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            new_id,
            namespace,
            name,
            description,
            license,
            compatibility,
            allowed_tools_json,
            metadata_json,
            body_blob,
            digest,
            signature_bytes,
            signing_agent_str,
            now_secs,
        ],
    )
    .map_err(|e| format!("skills INSERT: {e}"))?;

    // Insert resources.
    for (res_path, res_kind, res_content) in resources {
        let res_digest = resource_digest(res_content);
        let res_blob = compress(res_content)?;
        conn.execute(
            "INSERT INTO skill_resources \
                (skill_id, resource_path, resource_kind, content_blob, digest) \
             VALUES (?1,?2,?3,?4,?5)",
            params![new_id, res_path, res_kind, res_blob, res_digest],
        )
        .map_err(|e| format!("skill_resources INSERT ({res_path}): {e}"))?;
    }

    // Update previous row's superseded_by.
    let superseded = if let Some(ref prev) = prev_id {
        conn.execute(
            "UPDATE skills SET superseded_by = ?1 WHERE id = ?2",
            params![new_id, prev],
        )
        .map_err(|e| format!("superseded_by UPDATE: {e}"))?;
        Some(prev.clone())
    } else {
        None
    };

    // Append signed_events audit row.
    let event_payload = json!({
        "skill_id": new_id,
        "namespace": namespace,
        "name": name,
        "action": if superseded.is_some() { "supersede" } else { "register" },
    });
    let event_bytes = serde_json::to_vec(&event_payload).unwrap_or_default();
    let ev_hash = payload_hash(&event_bytes);
    let attest = if signature_bytes.is_some() {
        "self_signed"
    } else {
        "unsigned"
    };
    let event = SignedEvent {
        id: Uuid::new_v4().to_string(),
        agent_id: signing_agent_str
            .clone()
            .unwrap_or_else(|| "anonymous".to_string()),
        event_type: "skill.registered".to_string(),
        payload_hash: ev_hash,
        signature: signature_bytes.clone(),
        attest_level: attest.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    let _ = append_signed_event(conn, &event); // best-effort; don't fail registration on audit err

    Ok(RegisterResult {
        id: new_id,
        digest,
        superseded,
    })
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

pub(super) fn handle_skill_register(
    conn: &Connection,
    params: &Value,
    active_keypair: Option<&AgentKeypair>,
) -> Result<Value, String> {
    // -----------------------------------------------------------------------
    // Input: folder_path or inline_skill
    // -----------------------------------------------------------------------
    let (skill_md_text, resource_files): (String, Vec<(String, String, Vec<u8>)>) =
        if let Some(folder_str) = params["folder_path"].as_str() {
            let folder = Path::new(folder_str);
            if !folder.is_dir() {
                return Err(format!(
                    "folder_path '{folder_str}' is not a directory or does not exist"
                ));
            }
            let md_path = folder.join("SKILL.md");
            let text = std::fs::read_to_string(&md_path)
                .map_err(|e| format!("cannot read SKILL.md in '{folder_str}': {e}"))?;

            // Collect resource files from a 'resources/' sub-directory.
            let mut res: Vec<(String, String, Vec<u8>)> = Vec::new();
            let res_dir = folder.join("resources");
            if res_dir.is_dir() {
                collect_resources(&res_dir, &res_dir, &mut res)?;
            }
            (text, res)
        } else if let Some(inline) = params["inline_skill"].as_str() {
            (inline.to_string(), Vec::new())
        } else {
            return Err(
                "memory_skill_register requires either 'folder_path' or 'inline_skill'".to_string(),
            );
        };

    // -----------------------------------------------------------------------
    // Parse + validate SKILL.md
    // -----------------------------------------------------------------------
    let manifest = skill_md::parse(&skill_md_text)?;

    let body_bytes = manifest.body.as_bytes();

    // Compute resource digests for the signing surface.
    let res_digests: Vec<Vec<u8>> = resource_files
        .iter()
        .map(|(_, _, content)| resource_digest(content))
        .collect();

    let result = register_core(
        conn,
        &manifest.namespace,
        &manifest.name,
        &manifest.description,
        manifest.license.as_deref(),
        manifest.compatibility.as_deref(),
        &manifest.allowed_tools,
        &manifest.metadata,
        body_bytes,
        res_digests,
        &resource_files,
        active_keypair,
    )?;

    let digest_hex = hex::encode(&result.digest);
    let mut response = json!({
        "registered": true,
        "id": result.id,
        "namespace": manifest.namespace,
        "name": manifest.name,
        "digest": digest_hex,
        "signed": active_keypair.is_some(),
    });
    if let Some(prev) = result.superseded {
        response["superseded_id"] = json!(prev);
    }
    Ok(response)
}

// ---------------------------------------------------------------------------
// Recursive resource directory walker
// ---------------------------------------------------------------------------

fn collect_resources(
    base: &Path,
    dir: &Path,
    out: &mut Vec<(String, String, Vec<u8>)>,
) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("read_dir '{}': {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("dir entry error: {e}"))?;
        let path = entry.path();
        if path.is_dir() {
            collect_resources(base, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(base)
                .map_err(|_| "path prefix error".to_string())?
                .to_string_lossy()
                .into_owned();
            let content = std::fs::read(&path)
                .map_err(|e| format!("read resource '{}': {e}", path.display()))?;
            // Determine kind from sub-directory name or file extension.
            let kind = infer_kind(&rel);
            out.push((rel, kind, content));
        }
    }
    Ok(())
}

fn infer_kind(rel_path: &str) -> String {
    if rel_path.starts_with("scripts/") || rel_path.ends_with(".sh") || rel_path.ends_with(".py") {
        "script".to_string()
    } else if rel_path.starts_with("reference/") || rel_path.starts_with("references/") {
        "reference".to_string()
    } else {
        "asset".to_string()
    }
}

// ---------------------------------------------------------------------------
// hex helper (inline — avoids adding hex dep)
// ---------------------------------------------------------------------------

mod hex {
    pub(super) fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
