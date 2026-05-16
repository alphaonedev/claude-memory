// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-2 — auto-persona-regeneration substrate hook.
//!
//! When the namespace policy
//! [`crate::models::GovernancePolicy::auto_persona_trigger_every_n_memories`]
//! resolves to `Some(N)` for the reflection's target namespace, the
//! substrate-side `post_reflect` hook deferred-spawns a Persona
//! regeneration once the per-(entity, namespace) reflection counter
//! crosses a multiple of `N`.
//!
//! # Hard guarantees
//!
//! 1. **Non-blocking.** The hook returns synchronously; the curator
//!    synthesis happens on a detached `std::thread::spawn`. Reflect
//!    response latency stays inside the existing envelope when the
//!    cadence is set.
//! 2. **Notify-class.** Any failure during the worker thread (curator
//!    timeout, LLM unavailable, etc.) is logged via
//!    `tracing::warn!(target: "post_reflect.auto_persona", ...)` and
//!    NEVER propagated back. The reflection is already committed; the
//!    Persona regeneration is a deferred best-effort artefact.
//! 3. **Cadence trigger.** Regeneration only fires when the entity's
//!    in-namespace reflection count becomes an integer multiple of the
//!    policy's `N`. Operators flipping the cadence from `None` to
//!    `Some(5)` will see the first regeneration on the 5th reflection,
//!    the second on the 10th, etc. — bounded by `count % N == 0`.
//!
//! # Why post_reflect and not post_store
//!
//! Personas distil reflections, not raw observations. A `post_store`
//! hook would fire on every memory write (including notifications,
//! transcripts, the agent's own self-reports), which would either
//! triple-count the cadence or require us to filter out
//! non-Reflection rows. Tying the hook to the reflect path makes the
//! "what counts toward cadence" question trivial.

use std::path::PathBuf;
use std::sync::Arc;

use rusqlite::OptionalExtension;

use crate::autonomy::AutonomyLlm;
use crate::db;
use crate::identity::keypair::AgentKeypair;
use crate::persona::{PersonaConfig, PersonaGenerator, render_persona_md};
use crate::storage::reflect::{ReflectHooks, ReflectOutcome};

/// Static configuration for the auto-persona hook bundle.
///
/// Cloned into the spawned worker thread on every reflection write,
/// so the type is `Send + Sync`. The `out_dir` defaults match the
/// CLI's `--out-dir` for the optional filesystem export companion.
#[derive(Debug, Clone)]
pub struct AutoPersonaConfig {
    /// Root directory the substrate writes persona Markdown files
    /// under (when the namespace policy opts in). Defaults to
    /// `<HOME>/.ai-memory/personas/`.
    pub out_dir: PathBuf,
}

impl AutoPersonaConfig {
    /// Construct with the canonical default `out_dir`.
    #[must_use]
    pub fn default_for_home() -> Self {
        let base = dirs::home_dir()
            .map(|h| h.join(".ai-memory").join("personas"))
            .unwrap_or_else(|| PathBuf::from(".ai-memory").join("personas"));
        Self { out_dir: base }
    }
}

impl Default for AutoPersonaConfig {
    fn default() -> Self {
        Self::default_for_home()
    }
}

/// Build a [`ReflectHooks`] bundle whose `post_reflect` callback runs
/// the auto-persona cadence check.
///
/// The LLM trait is bound at hook-build time (the substrate's
/// daemon-runtime owns the `OllamaClient` and clones an `Arc` of it
/// into the closure). Tests pass a `StubLlm`. The worker thread opens
/// its own SQLite connection because rusqlite handles aren't `Send`.
/// v0.7.0 issue #811 / #813 — `keypair` carries the daemon signing
/// keypair so the spawned worker forwards it into
/// [`PersonaGenerator::new`]. When `None` the worker stays on the
/// pre-#811 unsigned path; when `Some` every link + the persona
/// artifact get signed end-to-end (BUG-B + BUG-C fix in one place).
#[must_use]
pub fn build_post_reflect_hook<L>(
    db_path: PathBuf,
    config: AutoPersonaConfig,
    llm: Arc<L>,
    keypair: Option<Arc<AgentKeypair>>,
) -> ReflectHooks<'static>
where
    L: AutonomyLlm + Send + Sync + 'static,
{
    let cfg = Arc::new(config);
    let dbp = Arc::new(db_path);
    let kp = keypair;
    let cb: Box<dyn Fn(&ReflectOutcome) + Send + Sync + 'static> = Box::new(move |outcome| {
        let cfg = cfg.clone();
        let dbp = dbp.clone();
        let llm = llm.clone();
        let kp = kp.clone();
        let outcome_id = outcome.id.clone();
        let namespace = outcome.namespace.clone();
        std::thread::spawn(move || {
            if let Err(e) = run_auto_persona(
                &dbp,
                &outcome_id,
                &namespace,
                &cfg,
                llm.as_ref(),
                kp.as_deref(),
            ) {
                tracing::warn!(
                    target: "post_reflect.auto_persona",
                    "auto-persona for reflection {} (ns={}) failed: {}",
                    outcome_id,
                    namespace,
                    e,
                );
            }
        });
    });
    ReflectHooks {
        pre_reflect: None,
        post_reflect: Some(cb),
    }
}

/// Worker-thread entry-point.
///
/// 1. Re-open the SQLite connection.
/// 2. Resolve the namespace policy (walks ancestors leaf-first).
/// 3. Bail when the cadence is unset.
/// 4. Resolve `entity_id` from the reflection's content / metadata.
///    Falls back to scanning the reflection's title for a `[entity:X]`
///    marker; when neither matches we no-op (the operator has not
///    yet tagged the reflection with an entity to distil for).
/// 5. Count same-entity reflections in the namespace; bail unless
///    `count % cadence == 0`.
/// 6. Run [`PersonaGenerator::generate`].
/// 7. When the namespace policy enables file-backed export, write
///    the rendered Markdown to the configured `out_dir`.
///
/// # Errors
///
/// Bubbles up SQL / I/O / curator errors. The caller in
/// [`build_post_reflect_hook`] logs + swallows them.
pub fn run_auto_persona(
    db_path: &std::path::Path,
    reflection_id: &str,
    namespace: &str,
    config: &AutoPersonaConfig,
    llm: &dyn AutonomyLlm,
    keypair: Option<&AgentKeypair>,
) -> anyhow::Result<()> {
    let conn = db::open(db_path)?;
    let policy = db::resolve_governance_policy(&conn, namespace).unwrap_or_default();
    let Some(cadence) = policy.effective_auto_persona_trigger_every_n_memories() else {
        return Ok(());
    };
    if cadence == 0 {
        return Ok(());
    }

    // Resolve the entity_id off the reflection's metadata; fall back
    // to the agent_id when no explicit `entity` key is present.
    let Some(entity_id) = resolve_entity_id(&conn, reflection_id)? else {
        tracing::debug!(
            target: "post_reflect.auto_persona",
            "reflection {reflection_id} has no resolvable entity tag — skipping cadence"
        );
        return Ok(());
    };

    let count = count_entity_reflections(&conn, &entity_id, namespace)?;
    if count == 0 || count % i64::from(cadence) != 0 {
        return Ok(());
    }

    // v0.7.0 issue #811 / #813 — forward the daemon's signing keypair
    // into the generator so both the `derived_from` link writes AND
    // the persona body get signed end-to-end. Pre-#811 this passed
    // `None` unconditionally, leaving every auto-persona-generated
    // row unsigned even on daemons whose `[identity]` was wired.
    let generator = PersonaGenerator::new(&conn, llm, keypair, PersonaConfig::default());
    let persona = match generator.generate(&entity_id, namespace) {
        Ok(p) => p,
        Err(crate::persona::PersonaError::NoReflections { .. }) => return Ok(()),
        Err(e) => return Err(anyhow::anyhow!("auto-persona generation failed: {e}")),
    };

    if policy.effective_auto_export_personas_to_filesystem() {
        write_persona_export(&persona, &config.out_dir)?;
    }
    Ok(())
}

/// Resolve the entity_id off a reflection memory's metadata. Returns
/// `None` when neither `metadata.entity_id` nor a `[entity:X]` token
/// inside the title yields a match.
pub(crate) fn resolve_entity_id(
    conn: &rusqlite::Connection,
    reflection_id: &str,
) -> anyhow::Result<Option<String>> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT title, metadata FROM memories WHERE id = ?1",
            rusqlite::params![reflection_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((title, metadata_str)) = row else {
        return Ok(None);
    };
    let metadata: serde_json::Value =
        serde_json::from_str(&metadata_str).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(eid) = metadata.get("entity_id").and_then(|v| v.as_str()) {
        return Ok(Some(eid.to_string()));
    }
    // `[entity:X]` marker in the title — operators frequently tag
    // reflections this way when no structured `entity_id` exists yet.
    if let Some(start) = title.find("[entity:") {
        let rest = &title[start + "[entity:".len()..];
        if let Some(end) = rest.find(']') {
            let extracted = rest[..end].trim();
            if !extracted.is_empty() {
                return Ok(Some(extracted.to_string()));
            }
        }
    }
    Ok(None)
}

/// Count reflections about `entity_id` in `namespace`.
///
/// v0.7.0 polish PERF-8 (issue #781): previously this scanned every
/// reflection in the namespace with `(title|content|metadata) LIKE
/// '%<entity>%'` — O(N * avg_content_len) per post-reflect hook fire.
/// The fix replaces the LIKE scan with an indexed equality lookup on
/// the schema-v42 `mentioned_entity_id` column (populated at write
/// time by [`crate::storage::extract_mentioned_entity_id`]; backfilled
/// for legacy rows by the v42 migration). The partial index
/// `idx_memories_mentioned_entity` covers the `(mentioned_entity_id,
/// namespace)` predicate so the planner serves this query from the
/// index, scanning only the matching rows.
///
/// Mirrors [`crate::persona::load_reflections_for_entity`] so cadence
/// accounting agrees with the generator's source pool.
fn count_entity_reflections(
    conn: &rusqlite::Connection,
    entity_id: &str,
    namespace: &str,
) -> anyhow::Result<i64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memories
         WHERE namespace = ?1
           AND memory_kind = 'reflection'
           AND mentioned_entity_id = ?2",
        rusqlite::params![namespace, entity_id],
        |r| r.get(0),
    )?;
    Ok(count)
}

/// Write `<out_dir>/<namespace>/<entity_id>.md` for the resolved
/// persona. Sanitises namespace path components the way QW-1's
/// reflection export does — replaces every `/` with `_` to keep the
/// path flat.
fn write_persona_export(
    persona: &crate::persona::Persona,
    out_dir: &std::path::Path,
) -> anyhow::Result<()> {
    let ns_safe = persona.namespace.replace('/', "_");
    let ns_dir = out_dir.join(&ns_safe);
    std::fs::create_dir_all(&ns_dir)?;
    let entity_safe = persona.entity_id.replace('/', "_");
    let path = ns_dir.join(format!("{entity_safe}.md"));
    let body = render_persona_md(persona);
    std::fs::write(&path, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        ApproverType, GovernanceLevel, GovernancePolicy, Memory, MemoryKind, Tier,
    };
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
            Ok(format!("Auto persona body ({} sources)", mems.len()))
        }
    }

    fn fresh_db() -> (Connection, TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ai-memory.db");
        let conn = db::open(&path).unwrap();
        (conn, dir, path)
    }

    fn seed_reflection(
        conn: &Connection,
        namespace: &str,
        title: &str,
        body: &str,
        entity_id: Option<&str>,
    ) -> String {
        let now = Utc::now().to_rfc3339();
        let mut metadata = serde_json::json!({"agent_id": "ai:test"});
        if let Some(eid) = entity_id {
            metadata["entity_id"] = serde_json::Value::String(eid.to_string());
        }
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
            metadata,
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

    fn enable_cadence(conn: &Connection, ns: &str, n: u32, export: bool) {
        let policy = GovernancePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
            inherit: true,
            max_reflection_depth: None,
            auto_export_reflections_to_filesystem: None,
            auto_atomise: None,
            auto_atomise_threshold_cl100k: None,
            auto_atomise_max_atom_tokens: None,
            auto_atomise_max_retries: None,
            auto_persona_trigger_every_n_memories: Some(n),
            auto_export_personas_to_filesystem: if export { Some(true) } else { None },
            auto_atomise_mode: None,
            legacy_per_pair_classifier: None,
            auto_classify_kind: None,
            synthesis_failure_mode: None,
            synthesis_max_deletes_per_call: None,
            synthesis_max_candidate_chars: None,
            multistep_max_content_chars: None,
        };
        let now = Utc::now().to_rfc3339();
        let gov_meta = serde_json::json!({
            "agent_id": "ai:test",
            "governance": serde_json::to_value(&policy).unwrap(),
        });
        let std_mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: ns.to_string(),
            title: format!("__standard_{ns}"),
            content: "standard".into(),
            created_at: now.clone(),
            updated_at: now,
            metadata: gov_meta,
            ..Default::default()
        };
        let std_id = db::insert(conn, &std_mem).unwrap();
        db::set_namespace_standard(conn, ns, &std_id, None).unwrap();
    }

    #[test]
    fn run_auto_persona_skips_when_cadence_unset() {
        let (conn, _dir, db_path) = fresh_db();
        let id = seed_reflection(
            &conn,
            "team/alpha",
            "obs about alice",
            "alice did X",
            Some("alice"),
        );
        let cfg = AutoPersonaConfig::default();
        let llm = StubLlm;
        run_auto_persona(&db_path, &id, "team/alpha", &cfg, &llm, None).unwrap();
        // No persona row should exist.
        let cnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE memory_kind = 'persona'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt, 0);
    }

    #[test]
    fn run_auto_persona_skips_when_count_not_multiple() {
        let (conn, _dir, db_path) = fresh_db();
        enable_cadence(&conn, "team/alpha", 3, false);
        // Only one reflection — 1 % 3 != 0.
        let id = seed_reflection(
            &conn,
            "team/alpha",
            "obs about alice",
            "alice did X",
            Some("alice"),
        );
        let cfg = AutoPersonaConfig::default();
        let llm = StubLlm;
        run_auto_persona(&db_path, &id, "team/alpha", &cfg, &llm, None).unwrap();
        let cnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE memory_kind = 'persona'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt, 0);
    }

    #[test]
    fn run_auto_persona_fires_when_count_hits_cadence() {
        let (conn, _dir, db_path) = fresh_db();
        enable_cadence(&conn, "team/alpha", 2, false);
        let _a = seed_reflection(&conn, "team/alpha", "obs1 alice", "alice X", Some("alice"));
        let b = seed_reflection(&conn, "team/alpha", "obs2 alice", "alice Y", Some("alice"));
        let cfg = AutoPersonaConfig::default();
        let llm = StubLlm;
        run_auto_persona(&db_path, &b, "team/alpha", &cfg, &llm, None).unwrap();
        let cnt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE memory_kind = 'persona'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt, 1);
    }

    #[test]
    fn run_auto_persona_writes_file_when_export_enabled() {
        let (conn, dir, db_path) = fresh_db();
        enable_cadence(&conn, "team/alpha", 1, true);
        let id = seed_reflection(
            &conn,
            "team/alpha",
            "obs alice",
            "alice did Z",
            Some("alice"),
        );
        let out = dir.path().join("personas-out");
        let cfg = AutoPersonaConfig {
            out_dir: out.clone(),
        };
        let llm = StubLlm;
        run_auto_persona(&db_path, &id, "team/alpha", &cfg, &llm, None).unwrap();
        let f = out.join("team_alpha").join("alice.md");
        assert!(f.exists(), "expected persona file at {}", f.display());
        let body = std::fs::read_to_string(&f).unwrap();
        assert!(body.contains("entity_id: alice\n"));
        assert!(body.contains("Auto persona body"));
    }

    #[test]
    fn resolve_entity_id_from_metadata() {
        let (conn, _dir, _db_path) = fresh_db();
        let id = seed_reflection(&conn, "team/alpha", "obs", "body", Some("entity-from-meta"));
        let resolved = resolve_entity_id(&conn, &id).unwrap();
        assert_eq!(resolved.as_deref(), Some("entity-from-meta"));
    }

    #[test]
    fn resolve_entity_id_from_title_marker() {
        let (conn, _dir, _db_path) = fresh_db();
        let id = seed_reflection(
            &conn,
            "team/alpha",
            "Reflection on [entity:bob] notes",
            "body",
            None,
        );
        let resolved = resolve_entity_id(&conn, &id).unwrap();
        assert_eq!(resolved.as_deref(), Some("bob"));
    }

    #[test]
    fn resolve_entity_id_returns_none_when_absent() {
        let (conn, _dir, _db_path) = fresh_db();
        let id = seed_reflection(&conn, "team/alpha", "plain title", "body", None);
        let resolved = resolve_entity_id(&conn, &id).unwrap();
        assert!(resolved.is_none());
    }

    /// v0.7.0 polish PERF-8 (issue #781) regression test — the
    /// `count_entity_reflections` query must hit the
    /// `idx_memories_mentioned_entity` partial index, NOT the previous
    /// full-table `(title|content|metadata) LIKE '%X%'` scan.
    ///
    /// The assertion pins the SQLite query plan so a future rewrite
    /// (e.g. dropping the indexed column predicate, or accidentally
    /// regressing back to a LIKE pattern) is loud rather than silent.
    /// EXPLAIN QUERY PLAN row 3 carries the human-readable plan text;
    /// we assert the partial-index name appears in it.
    #[test]
    fn count_entity_reflections_uses_indexed_column() {
        let (conn, _dir, _db_path) = fresh_db();
        // Seed enough rows + ANALYZE so the SQLite planner's cost
        // model picks the partial index over a full scan. Mirrors the
        // shape used in
        // `storage::tests::scope_index_used_for_direct_scope_filter`.
        for i in 0..120 {
            seed_reflection(
                &conn,
                "team/alpha",
                &format!("obs-a-{i}"),
                "body",
                Some("alice"),
            );
        }
        for i in 0..120 {
            seed_reflection(
                &conn,
                "team/alpha",
                &format!("obs-b-{i}"),
                "body",
                Some("bob"),
            );
        }
        conn.execute("ANALYZE", []).unwrap();

        // Functional check: the indexed lookup must agree with the
        // ground-truth row count for the entity.
        let count = count_entity_reflections(&conn, "alice", "team/alpha").unwrap();
        assert_eq!(count, 120, "expected 120 reflections about alice");
        let count_bob = count_entity_reflections(&conn, "bob", "team/alpha").unwrap();
        assert_eq!(count_bob, 120, "expected 120 reflections about bob");

        // Query-plan check: the SELECT must resolve via the partial
        // index rather than a sequential scan over `memories`. The
        // matcher's WHERE clause matches the partial index's literal
        // `memory_kind = 'reflection'` predicate so the planner can
        // pick the partial index deterministically.
        let plan: Vec<String> = conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT COUNT(*) FROM memories \
                 WHERE namespace = ?1 \
                   AND memory_kind = 'reflection' \
                   AND mentioned_entity_id = ?2",
            )
            .unwrap()
            .query_map(rusqlite::params!["team/alpha", "alice"], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let joined = plan.join("\n");
        assert!(
            joined.contains("idx_memories_mentioned_entity"),
            "PERF-8: count_entity_reflections must hit the \
             idx_memories_mentioned_entity partial index; got plan:\n{joined}"
        );
        assert!(
            !joined.contains("SCAN memories"),
            "PERF-8: count_entity_reflections must NOT fall back to a \
             SCAN memories full-table scan; got plan:\n{joined}"
        );
    }

    /// v0.7.0 polish PERF-8 (issue #781) — `mentioned_entity_id` must
    /// be populated at insert time from `metadata.entity_id` so the
    /// matcher's indexed lookup sees freshly-minted reflections without
    /// waiting for a migration backfill.
    #[test]
    fn mentioned_entity_id_populated_from_metadata_on_insert() {
        let (conn, _dir, _db_path) = fresh_db();
        seed_reflection(
            &conn,
            "team/alpha",
            "first obs",
            "content body",
            Some("carol"),
        );
        let stored: Option<String> = conn
            .query_row(
                "SELECT mentioned_entity_id FROM memories \
                 WHERE namespace = 'team/alpha' AND memory_kind = 'reflection' \
                 LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored.as_deref(),
            Some("carol"),
            "PERF-8: insert(...) must denormalise metadata.entity_id into \
             the mentioned_entity_id column at write time"
        );
    }

    /// v0.7.0 polish PERF-8 (issue #781) — title-marker fallback. When
    /// no structured `metadata.entity_id` is present, the extractor
    /// scans the title for `[entity:X]` and populates the indexed
    /// column from that.
    #[test]
    fn mentioned_entity_id_populated_from_title_marker_on_insert() {
        let (conn, _dir, _db_path) = fresh_db();
        // No structured entity_id; title-marker fallback path.
        seed_reflection(
            &conn,
            "team/alpha",
            "Reflection on [entity:dave] notes",
            "body",
            None,
        );
        let stored: Option<String> = conn
            .query_row(
                "SELECT mentioned_entity_id FROM memories \
                 WHERE namespace = 'team/alpha' AND memory_kind = 'reflection' \
                 LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored.as_deref(),
            Some("dave"),
            "PERF-8: insert(...) must fall back to [entity:X] title-marker \
             extraction when metadata.entity_id is absent"
        );
    }
}
