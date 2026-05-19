// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-2 — `ai-memory persona` CLI command.
//!
//! Two-mode subcommand:
//!
//!   * Default (read-only): print the most recent Persona artefact
//!     for `(entity_id, namespace)` to stdout, either as
//!     YAML-frontmatter Markdown (default) or JSON (`--json`).
//!   * `--regenerate`: spawn the curator synthesis, persist a fresh
//!     Persona row, and print the new artefact.
//!
//! Mirrors the QW-1 `ai-memory export-reflections` shape — same
//! exit-code convention, same `CliOutput` wiring.

use std::path::Path;

use anyhow::{Result, anyhow};
use clap::Args;

use crate::autonomy::AutonomyLlm;
use crate::cli::CliOutput;
use crate::db;
use crate::persona::{
    PersonaConfig, PersonaError, PersonaGenerator, get_latest_persona, render_persona_json,
    render_persona_md,
};

/// CLI args for `ai-memory persona <entity_id> [...]`.
#[derive(Args, Debug, Clone)]
pub struct PersonaArgs {
    /// The entity to fetch / regenerate the persona for.
    #[arg(value_name = "ENTITY_ID")]
    pub entity_id: String,

    /// Namespace the persona lives under. Defaults to `global`.
    #[arg(long, value_name = "NS", default_value = "global")]
    pub namespace: String,

    /// When set, force a fresh curator synthesis and persist a new
    /// row before reading back the latest persona. Requires the
    /// daemon LLM to be available (smart+autonomous tier).
    #[arg(long, default_value_t = false)]
    pub regenerate: bool,

    /// Emit a structured JSON envelope instead of the YAML-frontmatter
    /// Markdown body.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

/// Dispatch entry-point called from `daemon_runtime::run`.
///
/// `active_keypair` carries the daemon signing keypair when available;
/// the CLI dispatch at `daemon_runtime` currently passes `None`
/// (matching the documented "CLI runs unsigned by design" posture,
/// see #809 comment in `daemon_runtime::run`). The parameter is here
/// so a future operator-driven "ai-memory persona --regenerate
/// --sign" flag can drop a keypair in without changing the function
/// surface again.
///
/// # Errors
///
/// Propagates DB errors. Returns `Ok(0)` on success, `Ok(1)` when no
/// persona exists and `--regenerate` was not passed, `Ok(2)` when
/// generation failed.
pub fn run(
    db_path: &Path,
    args: &PersonaArgs,
    llm: Option<&dyn AutonomyLlm>,
    active_keypair: Option<&crate::identity::keypair::AgentKeypair>,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    let conn = db::open(db_path)?;

    if args.regenerate {
        let Some(llm) = llm else {
            writeln!(
                out.stderr,
                "persona --regenerate requires an LLM client; \
                 install Ollama and re-run on the smart tier or higher.",
            )?;
            return Ok(2);
        };
        // v0.7.0 issue #811 / #813 — when a keypair is wired the link
        // path + the persona body get signed; when it's `None` the
        // behaviour matches v0.7.0.x (unsigned), which is what the
        // CLI dispatch sends today.
        let generator = PersonaGenerator::new(&conn, llm, active_keypair, PersonaConfig::default());
        match generator.generate(&args.entity_id, &args.namespace) {
            Ok(persona) => {
                render_to_stdout(out, &persona, args.json)?;
                return Ok(0);
            }
            Err(PersonaError::NoReflections {
                entity_id,
                namespace,
            }) => {
                writeln!(
                    out.stderr,
                    "no reflections found for entity '{entity_id}' in namespace '{namespace}'"
                )?;
                return Ok(2);
            }
            Err(e) => {
                writeln!(out.stderr, "persona generation failed: {e}")?;
                return Ok(2);
            }
        }
    }

    let persona = get_latest_persona(&conn, &args.entity_id, &args.namespace)?
        .ok_or_else(|| {
            anyhow!(
                "no persona has been minted for '{}' in namespace '{}' — pass --regenerate to create one",
                args.entity_id,
                args.namespace,
            )
        })?;
    render_to_stdout(out, &persona, args.json)?;
    Ok(0)
}

fn render_to_stdout(
    out: &mut CliOutput<'_>,
    persona: &crate::persona::Persona,
    json: bool,
) -> Result<()> {
    let text = if json {
        render_persona_json(persona)
    } else {
        render_persona_md(persona)
    };
    writeln!(out.stdout, "{text}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::CliOutput;
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
            Ok(format!("CLI persona body ({} sources)", mems.len()))
        }
    }

    fn fresh_db() -> (Connection, TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("ai-memory.db");
        let conn = db::open(&path).unwrap();
        (conn, dir, path)
    }

    /// v0.7.0 polish PERF-8 (issue #781) — test seeder now tags
    /// `metadata.entity_id = "alice"` so the indexed
    /// `mentioned_entity_id` lookup matches. See the analogous comment
    /// in `mcp::tools::persona::tests::seed_reflection` for the
    /// rationale: the fuzzy content-LIKE matcher is gone; tests must
    /// supply the structured entity tag (or a `[entity:X]` title
    /// marker) explicitly.
    fn seed_reflection(conn: &Connection, namespace: &str, body: &str) -> String {
        let now = Utc::now().to_rfc3339();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: namespace.to_string(),
            title: format!("ref about alice {}", &now[..19]),
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
    fn cli_persona_read_with_no_persona_errors() {
        let (_conn, _dir, db_path) = fresh_db();
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = PersonaArgs {
            entity_id: "alice".into(),
            namespace: "team/alpha".into(),
            regenerate: false,
            json: false,
        };
        let res = run(&db_path, &args, None, None, &mut out);
        assert!(res.is_err());
    }

    #[test]
    fn cli_persona_regenerate_creates_and_prints_md() {
        let (conn, _dir, db_path) = fresh_db();
        seed_reflection(&conn, "team/alpha", "alice is methodical");
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let llm = StubLlm;
        let args = PersonaArgs {
            entity_id: "alice".into(),
            namespace: "team/alpha".into(),
            regenerate: true,
            json: false,
        };
        let code = run(&db_path, &args, Some(&llm), None, &mut out).unwrap();
        assert_eq!(code, 0);
        drop(out);
        let text = String::from_utf8(stdout).unwrap();
        assert!(text.contains("entity_id: alice"));
        assert!(text.contains("persona_version: 1"));
    }

    #[test]
    fn cli_persona_regenerate_requires_llm() {
        let (_conn, _dir, db_path) = fresh_db();
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = PersonaArgs {
            entity_id: "alice".into(),
            namespace: "team/alpha".into(),
            regenerate: true,
            json: false,
        };
        let code = run(&db_path, &args, None, None, &mut out).unwrap();
        assert_eq!(code, 2);
        drop(out);
        let text = String::from_utf8(stderr).unwrap();
        assert!(text.contains("requires an LLM"));
    }

    #[test]
    fn cli_persona_regenerate_json_envelope() {
        let (conn, _dir, db_path) = fresh_db();
        seed_reflection(&conn, "team/alpha", "alice notes");
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let llm = StubLlm;
        let args = PersonaArgs {
            entity_id: "alice".into(),
            namespace: "team/alpha".into(),
            regenerate: true,
            json: true,
        };
        let code = run(&db_path, &args, Some(&llm), None, &mut out).unwrap();
        assert_eq!(code, 0);
        drop(out);
        let text = String::from_utf8(stdout).unwrap();
        let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(v["entity_id"], "alice");
        assert_eq!(v["persona_version"], 1);
    }
}
