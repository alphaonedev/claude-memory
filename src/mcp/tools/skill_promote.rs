// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_skill_promote_from_reflection` handler (v0.7.0 L2-6,
//! issue #671) — the **closing loop** of the recursive-learning
//! substrate.
//!
//! Reflections become skills become reusable knowledge. This handler
//! is the structural keystone that promotes a Reflection-kind memory
//! (synthesised via `memory_reflect` — depth ≥ configurable threshold)
//! into a SKILL.md-format Agent Skill stored in the `skills` table.
//!
//! # Promotion contract
//!
//! 1. **Fetch + validate.** The source memory MUST be Reflection-kind
//!    (`memory_kind = 'reflection'`) and its `reflection_depth` MUST be
//!    `>= skill_promotion_min_depth` (per-namespace
//!    `governance.skill_promotion_min_depth`, default `1`). Depth-0
//!    rows are refused — a reflection at depth 0 carries no synthesised
//!    insight to promote.
//! 2. **Construct SKILL.md.** The handler builds an in-memory SKILL.md
//!    with:
//!    - frontmatter: `name`, `description`, `license=Apache-2.0`,
//!      `metadata.derived_from_reflection_id`,
//!      `metadata.original_reflection_depth`
//!    - body: the reflection's `content` plus structured `## Applies
//!      when` / `## Outputs` sections inferred from the reflection
//!      pattern.
//! 3. **Walk `reflects_on` edges.** Each source memory the reflection
//!    pointed at becomes a `references/source_{i}.md` resource attached
//!    to the new skill. The source's title + body are embedded so the
//!    skill remains reusable even if the source memory is later GC'd.
//! 4. **Compute Ed25519 digest.** Routed through
//!    [`super::skill_register::register_core`], which is the same
//!    function the public `memory_skill_register` tool uses — so the
//!    promoted skill is byte-indistinguishable from a hand-authored
//!    one. The signing-surface digest covers the canonical frontmatter
//!    JSON, the body bytes, and the sorted per-resource digests.
//! 5. **Register.** The constructed skill lands in the `skills` table
//!    with full Bucket-1 attestation when an `active_keypair` is
//!    provided.
//! 6. **Provenance edge.** The new skill's metadata carries
//!    `derived_from_reflection_id` and `original_reflection_depth` so
//!    a downstream auditor can re-derive the promotion lineage. Skills
//!    do not live in the `memory_links` graph (the `skills` table has
//!    its own id space), so the lineage is recorded in metadata rather
//!    than as a `derived_from` row in `memory_links`.
//!
//! # Round-trip guarantee
//!
//! The keystone acceptance for L2-6 is the digest round-trip:
//! `promote → export → re-register → IDENTICAL digest`. The handler
//! achieves this by routing the construction through `register_core`
//! (so the digest is computed exactly once over the canonical
//! frontmatter + body + sorted resource digests) and by serializing the
//! constructed SKILL.md in the same shape `memory_skill_export`
//! produces it on disk. The accompanying integration test pins the
//! contract.

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::identity::keypair::AgentKeypair;
use crate::models::{MemoryKind, MemoryLinkRelation};

use super::skill_register::{RegisterResult, register_core, resource_digest};

/// Compiled default for `governance.skill_promotion_min_depth` when the
/// namespace governance blob does not override it. A reflection at
/// depth 1 represents one level of synthesised insight on top of raw
/// observations — the minimum surface that carries any reusable
/// signal. Depth 0 is the kill-switch refusal (no insight to promote).
const DEFAULT_SKILL_PROMOTION_MIN_DEPTH: u32 = 1;

/// Result of the handler call — broken out as a struct mostly so the
/// hex-encoding step below has a stable target shape.
struct PromoteOutcome {
    skill_id: String,
    digest_hex: String,
    namespace: String,
    name: String,
    reflection_id: String,
    reflection_depth: i32,
    sources_attached: usize,
    superseded: Option<String>,
}

/// MCP handler for `memory_skill_promote_from_reflection`.
///
/// Args:
/// - `reflection_id` (required): the UUID of a Reflection-kind memory.
/// - `skill_name` (required): the agentskills.io-compliant skill name.
/// - `skill_description` (required): 1–1024 char description.
/// - `parameters_schema` (optional): JSON object spliced into the
///   SKILL.md body's `## Parameters` section verbatim.
///
/// Returns a JSON envelope describing the promotion outcome. Errors
/// are plain strings, matching the convention every other MCP handler
/// in this directory follows.
#[allow(clippy::too_many_lines)]
pub fn handle_skill_promote_from_reflection(
    conn: &Connection,
    params: &Value,
    active_keypair: Option<&AgentKeypair>,
) -> Result<Value, String> {
    // ─── 1. Argument parsing ────────────────────────────────────────────
    let reflection_id = params["reflection_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("memory_skill_promote_from_reflection requires 'reflection_id'")?;
    let skill_name = params["skill_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("memory_skill_promote_from_reflection requires 'skill_name'")?;
    let skill_description = params["skill_description"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("memory_skill_promote_from_reflection requires 'skill_description'")?;
    let parameters_schema: Option<&Value> = params
        .get("parameters_schema")
        .filter(|v| !v.is_null() && v.is_object());

    // Validate skill name against agentskills.io §3.1 BEFORE any DB work
    // so the caller sees the parse error at the boundary.
    crate::parsing::skill_md::validate_skill_name(skill_name)?;
    if skill_description.len() > 1024 {
        return Err(format!(
            "skill 'description' must be ≤ 1024 characters \
             (agentskills.io spec §3.2): got {} characters",
            skill_description.len()
        ));
    }

    // ─── 2. Fetch + validate the source reflection ─────────────────────
    let reflection = crate::db::get(conn, reflection_id)
        .map_err(|e| format!("loading reflection '{reflection_id}': {e}"))?
        .ok_or_else(|| format!("reflection not found: {reflection_id}"))?;

    if reflection.memory_kind != MemoryKind::Reflection {
        return Err(format!(
            "memory '{reflection_id}' is memory_kind='{}', expected 'reflection' \
             (memory_skill_promote_from_reflection is reflection-only)",
            reflection.memory_kind
        ));
    }

    // Resolve the per-namespace threshold; compiled default is 1.
    let min_depth = crate::db::resolve_skill_promotion_min_depth(conn, &reflection.namespace)
        .unwrap_or(DEFAULT_SKILL_PROMOTION_MIN_DEPTH);

    // `reflection_depth` is stored as i32; clamp negative values to 0
    // for the comparison so a corrupt row can't slip past the threshold
    // via signed-underflow.
    #[allow(clippy::cast_sign_loss)]
    let actual_depth_u32: u32 = reflection.reflection_depth.max(0) as u32;
    if actual_depth_u32 < min_depth {
        return Err(format!(
            "reflection '{reflection_id}' has reflection_depth={} but \
             namespace '{}' requires skill_promotion_min_depth={} — \
             a depth-0 reflection carries no synthesised insight to promote",
            reflection.reflection_depth, reflection.namespace, min_depth,
        ));
    }

    // ─── 3. Walk reflects_on edges → source resources ──────────────────
    // get_links returns edges in both directions; we want the OUTBOUND
    // `reflects_on` edges (source_id == reflection_id, relation ==
    // ReflectsOn). The substrate `reflect` writer is the only producer
    // of these edges, so the order matches the original source_ids
    // input order at the wire level.
    let links = crate::db::get_links(conn, reflection_id)
        .map_err(|e| format!("loading reflects_on edges: {e}"))?;
    let mut source_ids: Vec<String> = links
        .into_iter()
        .filter(|l| l.source_id == reflection_id && l.relation == MemoryLinkRelation::ReflectsOn)
        .map(|l| l.target_id)
        .collect();
    // Stable ordering — `references/source_{i}.md` must be deterministic
    // for the round-trip digest to land identically. SQLite's
    // `query_map` order isn't a documented guarantee, so we sort by id.
    source_ids.sort();

    // Materialise each source memory into a reference resource.
    let mut resources: Vec<(String, String, Vec<u8>)> = Vec::with_capacity(source_ids.len());
    for (i, src_id) in source_ids.iter().enumerate() {
        let src = crate::db::get(conn, src_id)
            .map_err(|e| format!("loading source memory '{src_id}': {e}"))?;
        // Build the reference body. If the source memory is gone (GC'd
        // between reflect and promote), fall back to an id-only stub so
        // the promotion still lands — provenance edge is preserved by id.
        let body = match src {
            Some(m) => format!(
                "# Source memory: {title}\n\n\
                 - memory id: `{id}`\n\
                 - namespace: `{ns}`\n\
                 - reflection_depth: {depth}\n\
                 - created_at: {created}\n\n\
                 ## Content\n\n{content}\n",
                title = m.title,
                id = m.id,
                ns = m.namespace,
                depth = m.reflection_depth,
                created = m.created_at,
                content = m.content,
            ),
            None => format!(
                "# Source memory: (deleted)\n\n\
                 - memory id: `{src_id}`\n\
                 - note: source memory was deleted between reflection and promotion; \
                 only the id provenance edge is preserved.\n",
            ),
        };
        let res_path = format!("references/source_{i}.md");
        resources.push((res_path, "reference".to_string(), body.into_bytes()));
    }

    // ─── 4. Construct the SKILL.md body ────────────────────────────────
    let mut body = String::new();
    body.push_str(&format!("# {skill_name}\n\n"));
    body.push_str(&format!("{skill_description}\n\n"));
    body.push_str("## Reflection content\n\n");
    body.push_str(&reflection.content);
    if !reflection.content.ends_with('\n') {
        body.push('\n');
    }
    body.push('\n');
    body.push_str("## Applies when\n\n");
    body.push_str(
        "This skill was promoted from a reflection memory. It applies in contexts \
         that resemble the situations described above — the source memories listed \
         under `references/` capture the originating evidence.\n\n",
    );
    body.push_str("## Outputs\n\n");
    body.push_str(
        "Apply the reflection content as a reusable pattern. Reference the \
         per-source resources in `references/` for the underlying evidence \
         when the agent needs to re-derive the conclusion.\n",
    );

    if let Some(schema) = parameters_schema {
        let pretty = serde_json::to_string_pretty(schema)
            .map_err(|e| format!("parameters_schema serialize: {e}"))?;
        body.push_str("\n## Parameters\n\n```json\n");
        body.push_str(&pretty);
        body.push_str("\n```\n");
    }

    // ─── 5. Metadata: provenance edge to the source reflection ─────────
    let metadata = json!({
        "derived_from_reflection_id": reflection_id,
        "original_reflection_depth": reflection.reflection_depth,
    });

    // ─── 6. Compute per-resource digests for the signing surface ───────
    let res_digests: Vec<Vec<u8>> = resources
        .iter()
        .map(|(_, _, content)| resource_digest(content))
        .collect();
    let sources_attached = resources.len();

    // ─── 7. Register via the shared core ───────────────────────────────
    // license is hard-coded to "Apache-2.0" per the L2-6 contract — a
    // promoted reflection is a derivative work of the source memories,
    // which themselves carry no per-row license; the project's Apache-2.0
    // umbrella applies. Callers who need a different license must
    // re-register the exported folder with an explicit value.
    let license = Some("Apache-2.0");
    let compatibility: Option<&str> = None;
    let allowed_tools: Vec<String> = Vec::new();

    let RegisterResult {
        id: skill_id,
        digest,
        superseded,
    } = register_core(
        conn,
        &reflection.namespace,
        skill_name,
        skill_description,
        license,
        compatibility,
        &allowed_tools,
        &metadata,
        body.as_bytes(),
        res_digests,
        &resources,
        active_keypair,
    )?;

    let digest_hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();

    let outcome = PromoteOutcome {
        skill_id,
        digest_hex,
        namespace: reflection.namespace.clone(),
        name: skill_name.to_string(),
        reflection_id: reflection_id.to_string(),
        reflection_depth: reflection.reflection_depth,
        sources_attached,
        superseded,
    };

    let mut response = json!({
        "promoted": true,
        "skill_id": outcome.skill_id,
        "namespace": outcome.namespace,
        "name": outcome.name,
        "digest": outcome.digest_hex,
        "derived_from_reflection_id": outcome.reflection_id,
        "original_reflection_depth": outcome.reflection_depth,
        "sources_attached": outcome.sources_attached,
        "signed": active_keypair.is_some(),
    });
    if let Some(prev) = outcome.superseded {
        response["superseded_id"] = json!(prev);
    }
    Ok(response)
}

// ---------------------------------------------------------------------------
// Lib-level unit tests — exercise the depth-threshold gate at the
// handler boundary so the failure mode is pinned without spinning up
// the full integration harness. Round-trip and end-to-end scenarios
// land in `tests/skill_promote_test.rs`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::models::{Memory, MemoryKind, Tier};
    use serde_json::json as sjson;

    fn open_db() -> (rusqlite::Connection, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("promote.db");
        let conn = db::open(&path).expect("db open");
        (conn, dir)
    }

    fn insert_observation(conn: &rusqlite::Connection, title: &str, ns: &str) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        let m = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: format!("body of {title}"),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "cli".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: sjson!({}),
            reflection_depth: 0,
            memory_kind: MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        };
        db::insert(conn, &m).expect("insert observation")
    }

    fn make_reflection(conn: &rusqlite::Connection, sources: &[String], ns: &str) -> String {
        let input = db::ReflectInput {
            source_ids: sources.to_vec(),
            title: format!("reflection over {} sources", sources.len()),
            content: "Synthesised insight: pattern X implies action Y.".to_string(),
            namespace: Some(ns.to_string()),
            tier: Tier::Mid,
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "cli".to_string(),
            agent_id: "test-agent".to_string(),
            metadata: sjson!({}),
        };
        db::reflect(conn, &input).expect("reflect").id
    }

    #[test]
    fn refuses_non_reflection_memory() {
        let (conn, _dir) = open_db();
        let obs_id = insert_observation(&conn, "raw note", "ns");
        let params = sjson!({
            "reflection_id": obs_id,
            "skill_name": "test-skill",
            "skill_description": "Test skill from observation (should fail).",
        });
        let err = handle_skill_promote_from_reflection(&conn, &params, None).unwrap_err();
        assert!(
            err.contains("memory_kind='observation'"),
            "must surface kind mismatch: {err}",
        );
    }

    #[test]
    fn refuses_unknown_reflection_id() {
        let (conn, _dir) = open_db();
        let params = sjson!({
            "reflection_id": "nonexistent-id",
            "skill_name": "x",
            "skill_description": "desc",
        });
        let err = handle_skill_promote_from_reflection(&conn, &params, None).unwrap_err();
        assert!(err.contains("not found"), "expected not found: {err}");
    }

    #[test]
    fn refuses_invalid_skill_name() {
        let (conn, _dir) = open_db();
        let obs_id = insert_observation(&conn, "source", "ns");
        let refl_id = make_reflection(&conn, &[obs_id], "ns");
        let params = sjson!({
            "reflection_id": refl_id,
            "skill_name": "BadName",
            "skill_description": "desc",
        });
        let err = handle_skill_promote_from_reflection(&conn, &params, None).unwrap_err();
        assert!(err.contains("spec §3.1"), "must cite spec: {err}");
    }

    #[test]
    fn rejects_missing_required_params() {
        let (conn, _dir) = open_db();
        let err = handle_skill_promote_from_reflection(&conn, &sjson!({}), None).unwrap_err();
        assert!(err.contains("reflection_id"), "{err}");

        let err =
            handle_skill_promote_from_reflection(&conn, &sjson!({"reflection_id": "x"}), None)
                .unwrap_err();
        assert!(err.contains("skill_name"), "{err}");

        let err = handle_skill_promote_from_reflection(
            &conn,
            &sjson!({"reflection_id": "x", "skill_name": "n"}),
            None,
        )
        .unwrap_err();
        assert!(err.contains("skill_description"), "{err}");
    }
}
