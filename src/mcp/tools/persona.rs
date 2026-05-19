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

use serde_json::{Value, json};

use crate::autonomy::AutonomyLlm;
use crate::config::FeatureTier;
use crate::persona::{PersonaConfig, PersonaError, PersonaGenerator, get_latest_persona};

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
    // v0.7.0 issue #848 — namespace handling.
    //
    // Pre-#848 contract: `namespace` omitted → silently defaulted to
    // `"global"`. That was the surprise that bit the pm-v29 NHI
    // session (an agent with reflections in `global/policies` AND
    // `ai-memory/v0.7.0-nhi-testing` got "no reflections found for
    // ... namespace 'global'" because neither stash lived in bare
    // `global`).
    //
    // New contract:
    // - `namespace` present and a non-empty string → single-namespace
    //   scope (back-compat for callers that opt in explicitly).
    // - `namespace` missing OR JSON null OR explicit empty string →
    //   cross-namespace scope. The substrate aggregates reflections
    //   across every namespace the entity has touched, and the new
    //   persona row lands in `"global"` so subsequent
    //   `memory_persona(entity_id)` calls have a deterministic find.
    let scoped_single: Option<&str> = match params.get("namespace") {
        None => None,
        Some(v) if v.is_null() => None,
        Some(v) => match v.as_str() {
            Some(s) if s.is_empty() => None,
            Some(s) => Some(s),
            None => {
                return Err("namespace must be a string or null".to_string());
            }
        },
    };

    // v0.7.0 issue #811 / #813 — forward `active_keypair` through
    // `PersonaGenerator::new` so the link path AND the persona-body
    // signing path see the same identity.
    let generator = PersonaGenerator::new(conn, llm, active_keypair, PersonaConfig::default());
    let (persona, scope_label) = match scoped_single {
        Some(ns) => (
            generator
                .generate(entity_id, ns)
                .map_err(persona_error_to_string)?,
            "single".to_string(),
        ),
        None => (
            generator
                .generate_cross_namespace(entity_id, "global")
                .map_err(persona_error_to_string)?,
            "cross_namespace".to_string(),
        ),
    };

    Ok(json!({
        "persona": persona,
        "regenerated": true,
        "namespace_scope": scope_label,
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
            version: 1,
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
        assert_eq!(
            gen_res["namespace_scope"], "single",
            "explicit namespace must report single-namespace scope per #848"
        );
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

    /// v0.7.0 issue #848 — cross-namespace aggregation regression.
    ///
    /// Reproduces the pm-v29 NHI session failure: an entity has
    /// reflections in `global/policies` AND
    /// `ai-memory/v0.7.0-nhi-testing` but none in bare `global`.
    /// Pre-#848 the MCP handler silently defaulted `namespace` to
    /// `"global"` and returned "no reflections found ... namespace
    /// 'global'". The fix: omitting `namespace` triggers cross-
    /// namespace aggregation; persona lands in `"global"` with both
    /// source reflections as `derives_from` parents.
    #[test]
    fn issue_848_handle_persona_generate_omitted_namespace_aggregates_cross_namespace() {
        let (conn, _dir) = fresh_db();
        let id_a = seed_reflection(
            &conn,
            "global/policies",
            "discipline reflection",
            "alice keeps the tree clean across rounds",
        );
        let id_b = seed_reflection(
            &conn,
            "ai-memory/v0.7.0-nhi-testing",
            "campaign reflection",
            "alice closed the L1-6 governance gap end-to-end",
        );
        let llm = StubLlm;

        let gen_res = handle_persona_generate(
            &conn,
            &json!({"entity_id": "alice"}),
            Some(&llm),
            FeatureTier::Smart,
            None,
        )
        .expect("cross-namespace generate must succeed when sources exist in any namespace");

        assert_eq!(gen_res["regenerated"], true);
        assert_eq!(
            gen_res["namespace_scope"], "cross_namespace",
            "namespace omitted → handler must report cross_namespace scope"
        );

        let p = &gen_res["persona"];
        assert_eq!(p["entity_id"], "alice");
        assert_eq!(
            p["namespace"], "global",
            "cross-namespace persona must land in 'global' per #848 default"
        );

        let sources = p["sources"]
            .as_array()
            .expect("sources must serialise as an array");
        let source_set: std::collections::HashSet<String> = sources
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert!(
            source_set.contains(&id_a),
            "cross-namespace aggregation must include global/policies reflection {id_a}; got {sources:?}"
        );
        assert!(
            source_set.contains(&id_b),
            "cross-namespace aggregation must include ai-memory reflection {id_b}; got {sources:?}"
        );
    }

    /// v0.7.0 issue #848 — explicit JSON null on `namespace` must
    /// also route to the cross-namespace path.
    #[test]
    fn issue_848_handle_persona_generate_null_namespace_routes_to_cross_namespace() {
        let (conn, _dir) = fresh_db();
        seed_reflection(
            &conn,
            "scoped/ns",
            "single source",
            "alice closes loops with audit-honest discipline",
        );
        let llm = StubLlm;

        let gen_res = handle_persona_generate(
            &conn,
            &json!({"entity_id": "alice", "namespace": null}),
            Some(&llm),
            FeatureTier::Smart,
            None,
        )
        .expect("null namespace must aggregate cross-namespace");
        assert_eq!(gen_res["namespace_scope"], "cross_namespace");
        assert_eq!(gen_res["persona"]["namespace"], "global");
    }

    /// v0.7.0 issue #848 — cross-namespace path with zero matching
    /// reflections surfaces the broadened sentinel error message.
    #[test]
    fn issue_848_cross_namespace_with_no_reflections_reports_any_namespace_sentinel() {
        let (conn, _dir) = fresh_db();
        let llm = StubLlm;
        let err = handle_persona_generate(
            &conn,
            &json!({"entity_id": "alice"}),
            Some(&llm),
            FeatureTier::Smart,
            None,
        )
        .unwrap_err();
        assert!(
            err.contains("<any namespace>"),
            "#848 — empty cross-namespace scan must reference the cross-namespace sentinel; got: {err}"
        );
    }
}
