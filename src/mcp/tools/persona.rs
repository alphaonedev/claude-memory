// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-2 — MCP handlers for the Persona-as-artifact surface.
//!
//! Two tools live here:
//!
//!   * [`handle_persona`] — `memory_persona(entity_id, namespace?)`.
//!     Read-only lookup of the most recent Persona row for the
//!     `(entity_id, namespace)` tuple. Returns `null` when no
//!     persona has been generated yet.
//!   * [`handle_persona_generate`] — `memory_persona_generate(entity_id,
//!     namespace?)`. Smart+autonomous tier only. Spawns a curator
//!     synthesis over the entity's reflection cluster and persists a
//!     new `MemoryKind::Persona` row with `entity_id` +
//!     `persona_version` populated, plus one `derives_from` link per
//!     source.
//!
//! # Tier gate
//!
//! The MCP daemon's compiled-in tier is exposed via `crate::config::
//! FeatureTier`. The write path refuses on any tier below `Smart`
//! because the curator depends on the LLM trait being wired (Ollama
//! in production, `MockOllamaClient` in tests). Read-only
//! `memory_persona` is available at Semantic+.

use serde_json::{json, Value};

use crate::autonomy::AutonomyLlm;
use crate::config::FeatureTier;
use crate::persona::{get_latest_persona, PersonaConfig, PersonaError, PersonaGenerator};

/// Wire shape (read-only):
///
/// ```json
/// {
///   "persona": {
///     "id": "<uuid>",
///     "entity_id": "alice",
///     "namespace": "team/alpha",
///     "body_md": "...",
///     "sources": ["<reflection-id>", ...],
///     "generated_at": "2026-05-15T00:00:00Z",
///     "version": 2,
///     "attest_level": "unsigned"
///   }
/// }
/// ```
///
/// Returns `{"persona": null}` when no persona has been minted yet.
///
/// Errors:
/// * `entity_id is required` — caller omitted the parameter.
/// * `entity_id cannot be empty`.
pub(super) fn handle_persona(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    let entity_id = params["entity_id"]
        .as_str()
        .ok_or("entity_id is required")?;
    if entity_id.is_empty() {
        return Err("entity_id cannot be empty".to_string());
    }
    let namespace = params["namespace"].as_str().unwrap_or("global");

    let persona = get_latest_persona(conn, entity_id, namespace)
        .map_err(|e| format!("memory_persona substrate error: {e}"))?;
    Ok(json!({ "persona": persona }))
}

/// Wire shape (write):
///
/// ```json
/// {
///   "persona": { /* same shape as memory_persona */ },
///   "regenerated": true
/// }
/// ```
///
/// Errors (in addition to the read-only errors):
/// * `memory_persona_generate requires smart tier or higher` — tier gate.
/// * `no reflections found for entity ...` — refuses to mint a persona
///   without source reflections (audit-trail invariant).
/// * `curator synthesis failed: ...` — LLM returned an error.
// Issue #809 — promoted from pub(super) to pub so the
// model-agnostic NHI-self-persona regression test
// (tests/issue_809_nhi_self_persona_any_agent.rs) can drive this
// handler directly without spawning the full MCP-stdio JSON-RPC
// envelope.
pub fn handle_persona_generate(
    conn: &rusqlite::Connection,
    params: &Value,
    llm: Option<&dyn AutonomyLlm>,
    tier: FeatureTier,
    active_keypair: Option<&crate::identity::keypair::AgentKeypair>,
) -> Result<Value, String> {
    // Tier gate — refuse below smart so we never blow the budget by
    // accidentally firing curator synthesis on a keyword-only daemon.
    if !matches!(tier, FeatureTier::Smart | FeatureTier::Autonomous) {
        return Err(format!(
            "memory_persona_generate requires smart tier or higher (current: {tier:?})"
        ));
    }
    let llm = llm.ok_or(
        "memory_persona_generate requires an LLM client; none is wired into this dispatch",
    )?;

    let entity_id = params["entity_id"]
        .as_str()
        .ok_or("entity_id is required")?;
    if entity_id.is_empty() {
        return Err("entity_id cannot be empty".to_string());
    }
    let namespace = params["namespace"].as_str().unwrap_or("global");

    // v0.7.0 issue #811 / #813 — the prior implementation passed `None`
    // for the signer here even though the MCP dispatch already had the
    // daemon keypair as `active_keypair`. That regression produced
    // unsigned `derived_from` links + an unsigned persona artifact
    // even when the operator had a keypair on disk. We now forward
    // `active_keypair` through `PersonaGenerator::new` so the link
    // path AND the persona-body signing path see the same identity.
    let generator = PersonaGenerator::new(conn, llm, active_keypair, PersonaConfig::default());
    let persona = generator
        .generate(entity_id, namespace)
        .map_err(persona_error_to_string)?;

    Ok(json!({
        "persona": persona,
        "regenerated": true,
    }))
}

fn persona_error_to_string(e: PersonaError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autonomy::AutonomyLlm;
    use crate::models::{Memory, MemoryKind, Tier};
    use crate::storage as db;
    use chrono::Utc;
    use rusqlite::Connection;
    use tempfile::TempDir;

    struct StubLlm;
    impl AutonomyLlm for StubLlm {
        fn auto_tag(&self, _t: &str, _c: &str) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
        fn detect_contradiction(&self, _a: &str, _b: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn summarize_memories(&self, mems: &[(String, String)]) -> anyhow::Result<String> {
            Ok(format!("Stub persona body [{} sources]", mems.len()))
        }
    }

    fn fresh_db() -> (Connection, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ai-memory.db");
        let conn = db::open(&path).unwrap();
        (conn, dir)
    }

    /// v0.7.0 polish PERF-8 (issue #781) — test seeder now tags
    /// `metadata.entity_id = "alice"` so the indexed
    /// `mentioned_entity_id` lookup matches. Pre-PERF-8 the matcher
    /// scanned `(title|content|metadata) LIKE '%alice%'`, which
    /// surfaced reflections whose content merely contained the entity
    /// name without explicit tagging. The fix replaces that scan with
    /// an indexed equality lookup; tests that previously relied on the
    /// fuzzy fallback now seed the structured tag explicitly. The
    /// `entity_id` Memory field stays None (that's the QW-2 Persona-
    /// row attribution column; orthogonal to the matcher's
    /// `mentioned_entity_id` denormalisation).
    fn seed_reflection(conn: &Connection, namespace: &str, title: &str, body: &str) -> String {
        let now = Utc::now().to_rfc3339();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: namespace.to_string(),
            title: title.to_string(),
            content: body.to_string(),
            tags: vec!["reflection".into()],
            priority: 5,
            confidence: 1.0,
            source: "test".into(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id": "ai:test", "entity_id": "alice"}),
            reflection_depth: 1,
            memory_kind: MemoryKind::Reflection,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
            confidence_source: crate::models::ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
        };
        db::insert(conn, &mem).unwrap()
    }

    #[test]
    fn handle_persona_returns_null_when_unminted() {
        let (conn, _dir) = fresh_db();
        let out = handle_persona(
            &conn,
            &json!({"entity_id": "alice", "namespace": "team/alpha"}),
        )
        .unwrap();
        assert!(out["persona"].is_null());
    }

    #[test]
    fn handle_persona_rejects_empty_entity_id() {
        let (conn, _dir) = fresh_db();
        let err = handle_persona(&conn, &json!({"entity_id": ""})).unwrap_err();
        assert!(err.contains("entity_id cannot be empty"));
    }

    #[test]
    fn handle_persona_generate_refuses_below_smart_tier() {
        let (conn, _dir) = fresh_db();
        let llm = StubLlm;
        let err = handle_persona_generate(
            &conn,
            &json!({"entity_id": "alice"}),
            Some(&llm),
            FeatureTier::Keyword,
            None,
        )
        .unwrap_err();
        assert!(err.contains("requires smart tier"));
    }

    #[test]
    fn handle_persona_generate_writes_and_handle_persona_returns_it() {
        let (conn, _dir) = fresh_db();
        seed_reflection(
            &conn,
            "team/alpha",
            "obs about alice",
            "alice is methodical alice is patient",
        );
        let llm = StubLlm;
        let gen_res = handle_persona_generate(
            &conn,
            &json!({"entity_id": "alice", "namespace": "team/alpha"}),
            Some(&llm),
            FeatureTier::Smart,
            None,
        )
        .unwrap();
        assert_eq!(gen_res["regenerated"], true);
        let p = &gen_res["persona"];
        assert_eq!(p["entity_id"], "alice");
        assert_eq!(p["version"], 1);

        let got = handle_persona(
            &conn,
            &json!({"entity_id": "alice", "namespace": "team/alpha"}),
        )
        .unwrap();
        assert_eq!(got["persona"]["entity_id"], "alice");
        assert_eq!(got["persona"]["version"], 1);
    }
}
