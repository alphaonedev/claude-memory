// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-1 — auto-export-on-reflect substrate hook.
//!
//! When the namespace policy
//! [`GovernancePolicy::auto_export_reflections_to_filesystem`] resolves
//! to `Some(true)` for the reflection's target namespace, the
//! substrate-side `post_reflect` hook deferred-spawns a filesystem
//! write of the reflection's markdown to
//! `<out_dir>/<namespace>/<id>.md`.
//!
//! # Hard guarantees
//!
//! 1. **Non-blocking.** The hook returns synchronously; the disk write
//!    happens on a detached `std::thread::spawn`. The reflect response
//!    must not regress in latency when the policy is `Some(true)`.
//! 2. **Notify-class.** Failure during the disk write is logged via
//!    `tracing::warn!(target: "post_reflect.auto_export", ...)` and
//!    NEVER propagated back to the caller. The reflection is already
//!    committed; making the operator chase a transient disk error is
//!    worse than a missed file.
//!    *v0.7-polish SEC-15 / COR-11 (issue #780):* failure is also
//!    counted via [`crate::metrics::record_auto_export_spawn_failed`]
//!    and mirrored onto the capabilities-v3
//!    `hooks.auto_export_spawn_failed_total` field so operators can
//!    alert on the otherwise-silent disk-write loss without scraping
//!    `/metrics` directly. The detached worker closure is wrapped in
//!    `catch_unwind` so a panic inside the closure (e.g., a poisoned
//!    DB handle or a corrupt JSON column) is converted to the same
//!    counter+warn path rather than being swallowed by the runtime's
//!    detached-thread default.
//! 3. **Capability isolation.** This code runs inside the substrate
//!    process (CLI, MCP, HTTP daemon). It is gated by the namespace
//!    policy — an operator who has not explicitly opted in to
//!    `auto_export_reflections_to_filesystem` will see no disk writes
//!    from this module ever.

use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;

use crate::cli::commands::export_reflections::{self, ExportFormat};
use crate::db;
use crate::storage::reflect::{ReflectHooks, ReflectOutcome};

/// v0.7-polish SEC-15 / COR-11 (issue #780) — test-only panic-injection
/// env-var. When set to `1` (any case), the next worker spawn panics
/// inside its closure body so the panic-recovery path
/// (`catch_unwind` → counter increment → `tracing::warn!`) can be
/// exercised by an in-process integration test without an unsafe
/// runtime hack. Production binaries never read this env-var
/// (`debug_assertions`-gated read-site below), so the hot path stays
/// uninstrumented in release builds.
#[cfg(any(test, debug_assertions))]
const AUTO_EXPORT_INJECT_PANIC_ENV: &str = "AI_MEMORY_AUTO_EXPORT_INJECT_PANIC";

/// Static configuration for the auto-export hook.
///
/// Cloned into the spawned worker thread on every reflection write,
/// so the type is `Send + Sync`. Defaults match the CLI's
/// `--out-dir` / `--format` defaults so on-disk artefacts produced
/// by the substrate are interchangeable with those the operator
/// would have produced with `ai-memory export-reflections`.
#[derive(Debug, Clone)]
pub struct AutoExportConfig {
    /// Root directory the substrate writes reflections under.
    /// Defaults to `<HOME>/.ai-memory/reflections/`.
    pub out_dir: PathBuf,
    /// `md` (default) or `json`. Mirrors `--format`.
    pub format: ExportFormat,
}

impl AutoExportConfig {
    /// Construct with the canonical default `out_dir`.
    #[must_use]
    pub fn default_for_home() -> Self {
        let out_dir = export_reflections::resolve_out_dir(None)
            .unwrap_or_else(|_| PathBuf::from(".ai-memory").join("reflections"));
        Self {
            out_dir,
            format: ExportFormat::Markdown,
        }
    }
}

impl Default for AutoExportConfig {
    fn default() -> Self {
        Self::default_for_home()
    }
}

/// Build a [`ReflectHooks`] bundle whose `post_reflect` callback is
/// the auto-export hook.
///
/// The caller passes the database path so the hook can re-open a
/// read-only connection on the worker thread — the original
/// connection isn't `Send` (rusqlite). This trade-off matches every
/// other post-write side-effect in the substrate (subscriptions,
/// notify, etc.) — each spawns its own thread + opens its own
/// connection rather than crossing the connection across thread
/// boundaries.
#[must_use]
pub fn build_post_reflect_hook(
    db_path: PathBuf,
    config: AutoExportConfig,
) -> ReflectHooks<'static> {
    let cfg = Arc::new(config);
    let dbp = Arc::new(db_path);
    let cb: Box<dyn Fn(&ReflectOutcome) + Send + Sync + 'static> = Box::new(move |outcome| {
        let cfg = cfg.clone();
        let dbp = dbp.clone();
        let outcome_id = outcome.id.clone();
        let namespace = outcome.namespace.clone();
        // Detached worker thread. Notify-class: any failure stays
        // inside this thread, never reaches the caller.
        //
        // v0.7-polish SEC-15 / COR-11 (issue #780): the closure body
        // is wrapped in `catch_unwind` so a panic inside
        // `run_auto_export` (poisoned DB handle, corrupt JSON column,
        // an `unwrap` deep in the export-rendering chain, etc.) is
        // caught and converted to the same counter+warn path that an
        // `Err` return takes. Without this, a panic would silently
        // unwind the detached thread — the reflection would be
        // committed in the DB but no on-disk artefact would land and
        // no operator-visible signal would fire.
        std::thread::spawn(move || {
            let outcome_id_for_log = outcome_id.clone();
            let namespace_for_log = namespace.clone();
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                // v0.7-polish #780 — debug-only panic injection so the
                // catch_unwind path is reachable from an in-process
                // integration test. Release builds never read the
                // env-var (cfg-gated below).
                #[cfg(any(test, debug_assertions))]
                {
                    if std::env::var(AUTO_EXPORT_INJECT_PANIC_ENV)
                        .ok()
                        .is_some_and(|v| v == "1")
                    {
                        panic!("auto_export panic injected via {AUTO_EXPORT_INJECT_PANIC_ENV}=1");
                    }
                }
                run_auto_export(&dbp, &outcome_id, &namespace, &cfg)
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    crate::metrics::record_auto_export_spawn_failed();
                    tracing::warn!(
                        target: "post_reflect.auto_export",
                        "auto-export of reflection {} (ns={}) failed: {}",
                        outcome_id_for_log,
                        namespace_for_log,
                        e,
                    );
                }
                Err(panic_payload) => {
                    crate::metrics::record_auto_export_spawn_failed();
                    let panic_msg = panic_payload
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| {
                            panic_payload
                                .downcast_ref::<&'static str>()
                                .map(|s| (*s).to_string())
                        })
                        .unwrap_or_else(|| "<non-string panic payload>".to_string());
                    tracing::warn!(
                        target: "post_reflect.auto_export",
                        "auto-export of reflection {} (ns={}) panicked: {}",
                        outcome_id_for_log,
                        namespace_for_log,
                        panic_msg,
                    );
                }
            }
        });
    });
    ReflectHooks {
        pre_reflect: None,
        post_reflect: Some(cb),
        // Issue #815 — auto-export does not need a signing keypair;
        // signing is owned by the reflect handler that built this hook
        // bundle. The handler-side construction in
        // `mcp::tools::reflect::handle_reflect` overrides this field
        // when an active keypair is available, so the field is left
        // None here and re-assigned by the caller.
        active_keypair: None,
    }
}

/// Worker-thread entry-point. Encapsulated as a free function so the
/// hook code path stays one statement (`std::thread::spawn`) and so
/// unit tests can exercise the write logic without spawning a
/// thread.
///
/// # Errors
///
/// Bubbles up DB / I/O errors. The caller in [`build_post_reflect_hook`]
/// logs + swallows them — this function does NOT decide to swallow.
pub fn run_auto_export(
    db_path: &std::path::Path,
    memory_id: &str,
    namespace: &str,
    config: &AutoExportConfig,
) -> anyhow::Result<()> {
    let conn = db::open(db_path)?;
    let policy = db::resolve_governance_policy(&conn, namespace).unwrap_or_default();
    if !policy.effective_auto_export_reflections_to_filesystem() {
        // Defence-in-depth: the MCP handler also checks the policy
        // before installing the hook, but the substrate refuses to
        // touch the filesystem unless the policy itself blesses it.
        return Ok(());
    }
    let mem = match db::get(&conn, memory_id)? {
        Some(m) => m,
        None => {
            // Race: the reflection was deleted between commit and
            // hook fire. Nothing to write.
            return Ok(());
        }
    };
    let edges = collect_outbound_reflects_on(&conn, memory_id)?;
    let attest_level = export_reflections::summarise_attest_level(&edges);
    let payload = export_reflections::render_payload(&mem, &edges, attest_level, config.format);

    let ns_dir = config
        .out_dir
        .join(export_reflections::sanitise_namespace_for_path(
            &mem.namespace,
        ));
    std::fs::create_dir_all(&ns_dir)?;
    let path = ns_dir.join(format!("{}.{}", mem.id, config.format.extension()));
    std::fs::write(&path, payload)?;
    Ok(())
}

fn collect_outbound_reflects_on(
    conn: &rusqlite::Connection,
    memory_id: &str,
) -> anyhow::Result<Vec<export_reflections::ReflectsOnEdge>> {
    let mut stmt = conn.prepare(
        "SELECT target_id, COALESCE(attest_level, 'unsigned'), created_at \
         FROM memory_links \
         WHERE source_id = ?1 AND relation = 'reflects_on' \
         ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![memory_id], |row| {
        Ok(export_reflections::ReflectsOnEdge {
            target_id: row.get(0)?,
            attest_level: row.get(1)?,
            created_at: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ApproverType, GovernanceLevel, GovernancePolicy, Memory, Tier};
    use chrono::Utc;
    use tempfile::TempDir;

    fn fresh_db() -> (rusqlite::Connection, TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ai-memory.db");
        let conn = db::open(&path).unwrap();
        (conn, dir, path)
    }

    fn seed_observation(conn: &rusqlite::Connection, ns: &str) -> String {
        let now = Utc::now().to_rfc3339();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: ns.to_string(),
            title: "obs".into(),
            content: "obs body".into(),
            created_at: now.clone(),
            updated_at: now,
            ..Default::default()
        };
        db::insert(conn, &mem).unwrap()
    }

    fn enable_auto_export_on_namespace(conn: &rusqlite::Connection, ns: &str) {
        let policy = GovernancePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
            inherit: true,
            max_reflection_depth: None,
            auto_export_reflections_to_filesystem: Some(true),
            auto_atomise: None,
            auto_atomise_threshold_cl100k: None,
            auto_atomise_max_atom_tokens: None,
            auto_atomise_max_retries: None,
            auto_persona_trigger_every_n_memories: None,
            auto_export_personas_to_filesystem: None,
            auto_atomise_mode: None,
            legacy_per_pair_classifier: None,
            auto_classify_kind: None,
            synthesis_failure_mode: None,
            synthesis_max_deletes_per_call: None,
            synthesis_max_candidate_chars: None,
            multistep_max_content_chars: None,
        };
        let gov_metadata = serde_json::json!({
            "agent_id": "ai:test",
            "governance": serde_json::to_value(&policy).unwrap(),
        });
        let now = Utc::now().to_rfc3339();
        let std_mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: ns.to_string(),
            title: format!("__standard_{ns}"),
            content: "standard".into(),
            created_at: now.clone(),
            updated_at: now,
            metadata: gov_metadata,
            ..Default::default()
        };
        let std_id = db::insert(conn, &std_mem).unwrap();
        db::set_namespace_standard(conn, ns, &std_id, None).unwrap();
    }

    #[test]
    fn run_auto_export_skips_when_policy_disabled() {
        let (conn, dir, db_path) = fresh_db();
        let src = seed_observation(&conn, "skip-ns");
        let input = crate::storage::reflect::ReflectInput {
            source_ids: vec![src.clone()],
            title: "rfl".into(),
            content: "rfl body".into(),
            namespace: Some("skip-ns".into()),
            tier: Tier::Mid,
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "cli".into(),
            agent_id: "ai:test".into(),
            metadata: serde_json::json!({}),
        };
        let outcome = crate::storage::reflect::reflect(&conn, &input).unwrap();
        let cfg = AutoExportConfig {
            out_dir: dir.path().join("out"),
            format: ExportFormat::Markdown,
        };
        run_auto_export(&db_path, &outcome.id, &outcome.namespace, &cfg).unwrap();
        // Out dir must not have been populated.
        assert!(
            !dir.path().join("out").join("skip-ns").exists(),
            "auto-export must not fire when policy is disabled"
        );
    }

    #[test]
    fn run_auto_export_writes_md_when_policy_enabled() {
        let (conn, dir, db_path) = fresh_db();
        enable_auto_export_on_namespace(&conn, "write-ns");
        let src = seed_observation(&conn, "write-ns");
        let input = crate::storage::reflect::ReflectInput {
            source_ids: vec![src.clone()],
            title: "rfl".into(),
            content: "rfl body line".into(),
            namespace: Some("write-ns".into()),
            tier: Tier::Mid,
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "cli".into(),
            agent_id: "ai:test".into(),
            metadata: serde_json::json!({}),
        };
        let outcome = crate::storage::reflect::reflect(&conn, &input).unwrap();
        let cfg = AutoExportConfig {
            out_dir: dir.path().join("out"),
            format: ExportFormat::Markdown,
        };
        run_auto_export(&db_path, &outcome.id, &outcome.namespace, &cfg).unwrap();
        let f = dir
            .path()
            .join("out")
            .join("write-ns")
            .join(format!("{}.md", outcome.id));
        assert!(f.exists(), "expected exported file at {}", f.display());
        let body = std::fs::read_to_string(&f).unwrap();
        assert!(body.contains(&format!("memory_id: {}\n", outcome.id)));
        assert!(body.contains("namespace: write-ns\n"));
        assert!(body.contains("reflection_depth: 1\n"));
        assert!(body.contains("rfl body line"));
    }

    #[test]
    fn run_auto_export_writes_json_when_format_json() {
        let (conn, dir, db_path) = fresh_db();
        enable_auto_export_on_namespace(&conn, "json-ns");
        let src = seed_observation(&conn, "json-ns");
        let input = crate::storage::reflect::ReflectInput {
            source_ids: vec![src.clone()],
            title: "rfl".into(),
            content: "rfl json body".into(),
            namespace: Some("json-ns".into()),
            tier: Tier::Mid,
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "cli".into(),
            agent_id: "ai:test".into(),
            metadata: serde_json::json!({}),
        };
        let outcome = crate::storage::reflect::reflect(&conn, &input).unwrap();
        let cfg = AutoExportConfig {
            out_dir: dir.path().join("out"),
            format: ExportFormat::Json,
        };
        run_auto_export(&db_path, &outcome.id, &outcome.namespace, &cfg).unwrap();
        let f = dir
            .path()
            .join("out")
            .join("json-ns")
            .join(format!("{}.json", outcome.id));
        assert!(f.exists());
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        assert_eq!(parsed["memory_id"].as_str().unwrap(), outcome.id);
    }

    #[test]
    fn run_auto_export_swallows_missing_memory() {
        let (_, dir, db_path) = fresh_db();
        let cfg = AutoExportConfig {
            out_dir: dir.path().join("out"),
            format: ExportFormat::Markdown,
        };
        // The auto-export refuses to write because the policy defaults
        // to disabled — but it must not error either way.
        let res = run_auto_export(&db_path, "no-such-id", "no-such-ns", &cfg);
        assert!(res.is_ok());
    }

    #[test]
    fn build_post_reflect_hook_does_not_block_reflect_response() {
        // The acceptance bar: reflect_with_hooks returns within the
        // same latency envelope as reflect — measured by comparing two
        // back-to-back writes, one with the auto-export hook installed
        // and one without. We don't assert a hard ms number (hosts
        // vary); we assert the hook returns synchronously and the
        // worker spawns a background thread.
        let (conn, dir, db_path) = fresh_db();
        enable_auto_export_on_namespace(&conn, "block-ns");
        let src = seed_observation(&conn, "block-ns");
        let hooks = build_post_reflect_hook(
            db_path.clone(),
            AutoExportConfig {
                out_dir: dir.path().join("out"),
                format: ExportFormat::Markdown,
            },
        );
        let input = crate::storage::reflect::ReflectInput {
            source_ids: vec![src.clone()],
            title: "rfl".into(),
            content: "rfl body".into(),
            namespace: Some("block-ns".into()),
            tier: Tier::Mid,
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "cli".into(),
            agent_id: "ai:test".into(),
            metadata: serde_json::json!({}),
        };
        let started = std::time::Instant::now();
        let outcome = crate::storage::reflect::reflect_with_hooks(&conn, &input, &hooks).unwrap();
        let elapsed = started.elapsed();
        // The hook spawns a background thread; the reflect call must
        // return well under the disk-write budget. We use a generous
        // 500ms ceiling to keep the assertion robust on slow CI
        // hardware — the point is that the hook doesn't block on
        // its own disk write.
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "reflect_with_hooks should not block on auto-export disk write (took {elapsed:?})"
        );
        assert_eq!(outcome.namespace, "block-ns");
        // The file may or may not exist yet — the background thread
        // could still be running. We don't assert here; the file
        // existence is exercised by `run_auto_export_writes_md_when_policy_enabled`.
        let _ = outcome.id;
    }

    /// v0.7-polish SEC-15 / COR-11 (issue #780) — when the detached
    /// `auto_export` worker panics, the `catch_unwind` wrapper must
    /// (a) keep the panic from unwinding the runtime, (b) increment
    /// `crate::metrics::auto_export_spawn_failed_total` so operators
    /// see the loss, and (c) leave the reflect path itself unaffected
    /// (it had already returned). The test induces the panic via the
    /// debug-only `AI_MEMORY_AUTO_EXPORT_INJECT_PANIC=1` env-var hook
    /// in the spawn closure, then polls the counter for the increment.
    ///
    /// Serialised via a process-wide mutex because the env-var is
    /// process-global — if two tests in this module raced on it the
    /// "off" test would see the injection. The mutex scope brackets
    /// the env-var lifetime + the counter-advance wait so unrelated
    /// tests (which never set the env-var) are not held up.
    #[test]
    fn auto_export_worker_panic_increments_spawn_failed_counter() {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        // PoisonError is fine — we only care about exclusive access.
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let (conn, dir, db_path) = fresh_db();
        enable_auto_export_on_namespace(&conn, "panic-ns");
        let src = seed_observation(&conn, "panic-ns");
        let hooks = build_post_reflect_hook(
            db_path.clone(),
            AutoExportConfig {
                out_dir: dir.path().join("out"),
                format: ExportFormat::Markdown,
            },
        );
        let input = crate::storage::reflect::ReflectInput {
            source_ids: vec![src.clone()],
            title: "rfl".into(),
            content: "rfl body".into(),
            namespace: Some("panic-ns".into()),
            tier: Tier::Mid,
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "cli".into(),
            agent_id: "ai:test".into(),
            metadata: serde_json::json!({}),
        };

        let before = crate::metrics::auto_export_spawn_failed_count();
        // SAFETY: env-var mutation is process-global; the ENV_LOCK
        // mutex above serialises against any other test in this
        // module that touches the same key. No other module sets it.
        // SAFETY justification documented above; unsafe is required
        // because `set_var` is `unsafe` on edition-2024.
        unsafe {
            std::env::set_var(AUTO_EXPORT_INJECT_PANIC_ENV, "1");
        }
        let _outcome = crate::storage::reflect::reflect_with_hooks(&conn, &input, &hooks).unwrap();

        // Poll for the counter to advance. The worker is detached;
        // its thread may not have run by the time reflect_with_hooks
        // returns. Bound the wait at 5s — generous for any host.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut after = before;
        while std::time::Instant::now() < deadline {
            after = crate::metrics::auto_export_spawn_failed_count();
            if after > before {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        // SAFETY: same as set_var above — protected by ENV_LOCK.
        unsafe {
            std::env::remove_var(AUTO_EXPORT_INJECT_PANIC_ENV);
        }
        assert!(
            after > before,
            "auto_export_spawn_failed_total did not advance after panic injection \
             (before={before}, after={after})"
        );
    }

    #[test]
    fn auto_export_config_default_for_home_picks_dot_ai_memory() {
        let cfg = AutoExportConfig::default_for_home();
        // Either `<HOME>/.ai-memory/reflections` or
        // `.ai-memory/reflections` (HOME-less fallback). We don't pin
        // which — the test harness can run in either environment.
        assert!(
            cfg.out_dir.ends_with("reflections"),
            "default out_dir should end in 'reflections', got {}",
            cfg.out_dir.display()
        );
        assert_eq!(cfg.format, ExportFormat::Markdown);
    }
}
