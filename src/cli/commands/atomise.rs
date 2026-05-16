// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 WT-1-F — `ai-memory atomise` CLI subcommand.
//!
//! Operator-side wrapper over [`crate::atomisation::Atomiser`]. Wraps
//! tier gating, curator construction, keypair loading, and result /
//! error rendering (human-readable by default, structured JSON with
//! `--json`). Exit codes are stable wire and documented in the
//! [`exit_code`] mapper below.
//!
//! ## Wire shape
//!
//! ```bash
//! ai-memory atomise <memory_id> \
//!     --max-atom-tokens 200 \
//!     --force \
//!     --json \
//!     --quiet
//! ```
//!
//! ## Exit codes
//!
//! | Code | Variant                  | Meaning                                       |
//! |-----:|--------------------------|-----------------------------------------------|
//! |   0  | success                  | atoms minted, archived_at stamped             |
//! |   1  | informational            | `AlreadyAtomised` / `SourceTooSmall`          |
//! |   2  | not_found                | source memory id does not exist               |
//! |   3  | tier_locked              | daemon tier is `keyword`                      |
//! |   4  | curator_failed           | LLM round-trip exhausted retries              |
//! |   5  | governance_refused       | pre_store hook refused atom mid-batch         |
//! |   6  | db_error                 | DB / signer / I/O failure                     |
//!
//! ## Test injection
//!
//! [`run`] accepts an optional `curator_override` so the integration
//! tests can plug in a deterministic [`MockCurator`]. Production paths
//! pass `None` and the runner constructs an [`LlmCurator`] backed by
//! `OllamaClient` from the resolved tier.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use crate::atomisation::curator::{Curator, LlmCurator};
use crate::atomisation::{AtomiseError, Atomiser, AtomiserConfig};
use crate::cli::CliOutput;
use crate::config::{AppConfig, FeatureTier};
use crate::db;
use crate::identity::keypair as identity_keypair;
use crate::llm::OllamaClient;

/// Args for `ai-memory atomise`.
#[derive(Args, Debug, Clone)]
pub struct AtomiseArgs {
    /// Source memory id (UUID string). ai-memory uses UUID strings for
    /// memory ids, never integer rowids — accept the wire shape verbatim.
    pub memory_id: String,

    /// Per-atom token budget (cl100k). Defaults to 200, matching the
    /// substrate's [`AtomiserConfig::default_max_atom_tokens`]. Pass
    /// 0 to defer to the substrate default explicitly.
    #[arg(long, default_value_t = 200)]
    pub max_atom_tokens: u32,

    /// Re-atomise even if the source already carries an atom set.
    /// Old atoms are NOT deleted — their `atom_of` pointer remains
    /// valid, and `atomised_into` is bumped to the new fresh count.
    #[arg(long, default_value_t = false)]
    pub force: bool,

    /// Emit the result as a JSON envelope on stdout instead of the
    /// human-readable summary. Errors land on stderr verbatim.
    #[arg(long, default_value_t = false)]
    pub json: bool,

    /// Suppress per-step progress output. The final success / error
    /// summary still prints — `--quiet` only silences interstitial
    /// progress lines (currently a no-op; reserved for future stretch).
    #[arg(long, default_value_t = false)]
    pub quiet: bool,
}

/// JSON envelope emitted on success when `--json` is passed.
///
/// Field order is stable and mirrors the [`AtomiseResult`] struct so a
/// downstream consumer can deserialise into it without aliasing.
#[derive(Debug, Serialize)]
struct SuccessEnvelope<'a> {
    source_id: &'a str,
    atom_ids: &'a [String],
    atom_count: usize,
    archived_at: &'a str,
}

/// JSON envelope emitted on error when `--json` is passed.
#[derive(Debug, Serialize)]
struct ErrorEnvelope<'a> {
    /// Stable error code (matches the variant slug under [`exit_code`]).
    error: &'static str,
    /// Human-readable message — identical to the stderr line in the
    /// non-`--json` path.
    message: String,
    /// Exit code the process will terminate with.
    exit_code: i32,
    /// Per-variant structured payload. Only populated for variants
    /// that carry side data (existing atom ids, atom index, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<serde_json::Value>,
    /// Source id we attempted to atomise — useful for log post-processing.
    source_id: &'a str,
}

/// Map an [`AtomiseError`] variant to its stable exit code.
///
/// Visible-for-test so the unit suite below can assert on the mapping
/// without round-tripping through `run`.
#[must_use]
pub fn exit_code(err: &AtomiseError) -> i32 {
    match err {
        AtomiseError::AlreadyAtomised { .. } | AtomiseError::SourceTooSmall => 1,
        AtomiseError::NotFound => 2,
        AtomiseError::TierLocked => 3,
        AtomiseError::CuratorFailed(_) => 4,
        AtomiseError::GovernanceRefused(_) => 5,
        AtomiseError::DbError(_) | AtomiseError::SignerError(_) => 6,
    }
}

/// Stable error-code slug for the `--json` envelope. Mirrors the variant
/// names lower-snake-cased so downstream consumers can switch on a
/// fixed string set without parsing the prose message.
#[must_use]
pub fn error_slug(err: &AtomiseError) -> &'static str {
    match err {
        AtomiseError::AlreadyAtomised { .. } => "already_atomised",
        AtomiseError::SourceTooSmall => "source_too_small",
        AtomiseError::NotFound => "not_found",
        AtomiseError::TierLocked => "tier_locked",
        AtomiseError::CuratorFailed(_) => "curator_failed",
        AtomiseError::GovernanceRefused(_) => "governance_refused",
        AtomiseError::DbError(_) => "db_error",
        AtomiseError::SignerError(_) => "signer_error",
    }
}

/// Render a human-readable error message for a given variant. Matches
/// the [`AtomiseError::Display`] prose where the wire is stable, and
/// enriches it with extra operator-facing context (e.g. existing atom
/// ids, upgrade hint for the tier-locked path).
#[must_use]
pub fn human_error_message(err: &AtomiseError, source_id: &str) -> String {
    match err {
        AtomiseError::NotFound => format!("Memory ID {source_id} not found"),
        AtomiseError::AlreadyAtomised {
            source_id: sid,
            existing_atom_ids,
        } => {
            let ids = existing_atom_ids.join(", ");
            format!(
                "Memory {sid} already atomised into {n} atoms. Use --force to re-atomise. \
                 Existing atom IDs: {ids}",
                n = existing_atom_ids.len()
            )
        }
        AtomiseError::TierLocked => {
            "memory_atomise requires smart tier or higher. Current tier: keyword. \
             Upgrade your deployment or use --tier semantic when running ai-memory mcp."
                .to_string()
        }
        AtomiseError::CuratorFailed(detail) => {
            format!("Curator pass failed: {detail}. Check Ollama availability or retry.")
        }
        AtomiseError::SourceTooSmall => format!(
            "Memory {source_id} body already at or under max_atom_tokens. \
             No atomisation needed."
        ),
        AtomiseError::GovernanceRefused(detail) => {
            format!("Atomisation refused: {detail}")
        }
        AtomiseError::SignerError(detail) => format!("Signer error: {detail}"),
        AtomiseError::DbError(detail) => format!("Database error: {detail}"),
    }
}

/// Render structured per-variant `details` for the `--json` envelope.
///
/// Returns `None` when the variant carries no side payload beyond the
/// human message (the envelope's `details` field is then omitted).
#[must_use]
fn error_details(err: &AtomiseError) -> Option<serde_json::Value> {
    match err {
        AtomiseError::AlreadyAtomised {
            existing_atom_ids, ..
        } => Some(serde_json::json!({
            "existing_atom_ids": existing_atom_ids,
            "existing_atom_count": existing_atom_ids.len(),
        })),
        _ => None,
    }
}

/// Dispatch entry-point for `ai-memory atomise`.
///
/// `curator_override` is only set by the integration tests — production
/// passes `None` and we synthesise an [`LlmCurator`] from the resolved
/// tier. Returns the process exit code so the caller (the
/// `daemon_runtime::run` dispatcher) can `std::process::exit` cleanly
/// without panicking through `Err` propagation and skipping the post-run
/// WAL checkpoint.
///
/// # Errors
///
/// Propagates only fatal I/O errors (writing to stdout/stderr). Every
/// atomisation failure is mapped to an exit code via [`exit_code`] and
/// returned as `Ok(code)`.
pub fn run(
    db_path: &Path,
    args: &AtomiseArgs,
    app_config: &AppConfig,
    cli_agent_id: Option<&str>,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    run_with_curator(db_path, args, app_config, cli_agent_id, out, None)
}

/// Visible-for-test entry point. Production passes
/// `curator_override = None`; the integration suite injects a mock.
///
/// # Errors
///
/// Propagates only fatal I/O errors. See [`run`] for the full contract.
pub fn run_with_curator(
    db_path: &Path,
    args: &AtomiseArgs,
    app_config: &AppConfig,
    cli_agent_id: Option<&str>,
    out: &mut CliOutput<'_>,
    curator_override: Option<Box<dyn Curator>>,
) -> Result<i32> {
    // Resolve the effective tier from config (no per-call --tier flag
    // on the atomise subcommand — daemon-level resolution is the
    // source of truth, mirroring the `mcp --tier <x>` discipline).
    let tier = app_config.effective_tier(None);

    // Tier check at CLI layer: surface a clear operator-facing message
    // before we even open the DB. Substrate also enforces this, but
    // catching it here yields a better diagnostic.
    if tier == FeatureTier::Keyword {
        let err = AtomiseError::TierLocked;
        return emit_error(&err, &args.memory_id, args.json, out);
    }

    // Resolve calling agent_id — same precedence as the rest of the
    // CLI surface (explicit flag / env / synthesised host fallback).
    let calling_agent_id = match crate::identity::resolve_agent_id(cli_agent_id, None) {
        Ok(id) => id,
        Err(e) => {
            let err = AtomiseError::DbError(format!("agent_id resolution failed: {e}"));
            return emit_error(&err, &args.memory_id, args.json, out);
        }
    };

    // Open the DB. Failure here lands on the db_error track.
    let conn = match db::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            let err = AtomiseError::DbError(format!("open {}: {e}", db_path.display()));
            return emit_error(&err, &args.memory_id, args.json, out);
        }
    };

    // Build the curator. Tests inject; production constructs an
    // LlmCurator backed by the tier-resolved Ollama model.
    let curator: Box<dyn Curator> = if let Some(c) = curator_override {
        c
    } else {
        match build_llm_curator(tier) {
            Ok(c) => c,
            Err(e) => {
                let err = AtomiseError::CuratorFailed(e);
                return emit_error(&err, &args.memory_id, args.json, out);
            }
        }
    };

    // Best-effort keypair load — atoms can land unsigned if no key on
    // disk, matching the curator-pass / reflection-pass discipline.
    let keypair = load_keypair_best_effort(&calling_agent_id);

    let atomiser = Atomiser::new(curator, keypair, AtomiserConfig::default(), tier);

    match atomiser.atomise_sync(
        &conn,
        &args.memory_id,
        args.max_atom_tokens,
        args.force,
        &calling_agent_id,
    ) {
        Ok(result) => emit_success(&result, args.json, out),
        Err(e) => emit_error(&e, &args.memory_id, args.json, out),
    }
}

/// Build an [`LlmCurator`] backed by Ollama for the supplied tier.
///
/// Returns an error string when the tier has no curator LLM configured
/// (only possible for `Keyword`, which the caller has already gated)
/// or when `OllamaClient::new` fails (network bind, missing model, …).
fn build_llm_curator(tier: FeatureTier) -> std::result::Result<Box<dyn Curator>, String> {
    let llm_model = tier
        .config()
        .llm_model
        .ok_or_else(|| format!("tier '{tier}' has no curator LLM configured"))?;
    let model_id = llm_model.ollama_model_id().to_string();
    let client =
        OllamaClient::new(&model_id).map_err(|e| format!("OllamaClient::new({model_id}): {e}"))?;
    Ok(Box::new(LlmCurator::new(client)))
}

/// Best-effort keypair load — returns `None` if no key exists or the
/// load fails. Atoms then land unsigned. The CLI never refuses to run
/// solely because a keypair is missing; operators who want strict
/// signing can run `ai-memory identity generate <agent_id>` first and
/// re-invoke.
fn load_keypair_best_effort(agent_id: &str) -> Option<Arc<crate::identity::keypair::AgentKeypair>> {
    let dir = identity_keypair::default_key_dir().ok()?;
    identity_keypair::load(agent_id, &dir).ok().map(Arc::new)
}

/// Emit a success result (human or JSON), return exit code 0.
fn emit_success(
    result: &crate::atomisation::AtomiseResult,
    json: bool,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    if json {
        let env = SuccessEnvelope {
            source_id: &result.source_id,
            atom_ids: &result.atom_ids,
            atom_count: result.atom_count,
            archived_at: &result.archived_at,
        };
        writeln!(out.stdout, "{}", serde_json::to_string(&env)?)?;
    } else {
        let ids = result.atom_ids.join(", ");
        writeln!(
            out.stdout,
            "Atomised memory {src} into {n} atoms. Source archived at {ts}. Atom IDs: {ids}",
            src = result.source_id,
            n = result.atom_count,
            ts = result.archived_at,
        )?;
    }
    Ok(0)
}

/// Emit an error variant (human or JSON), return the variant's exit code.
fn emit_error(
    err: &AtomiseError,
    source_id: &str,
    json: bool,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    let code = exit_code(err);
    let message = human_error_message(err, source_id);
    if json {
        let env = ErrorEnvelope {
            error: error_slug(err),
            message: message.clone(),
            exit_code: code,
            details: error_details(err),
            source_id,
        };
        writeln!(out.stderr, "{}", serde_json::to_string(&env)?)?;
    } else {
        writeln!(out.stderr, "{message}")?;
    }
    Ok(code)
}

// ---------------------------------------------------------------------------
// Unit tests — pure-logic surface that doesn't require a live DB / Ollama.
// Full integration tests live at `tests/cli/atomise.rs`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_maps_every_variant() {
        assert_eq!(exit_code(&AtomiseError::NotFound), 2);
        assert_eq!(exit_code(&AtomiseError::TierLocked), 3);
        assert_eq!(exit_code(&AtomiseError::CuratorFailed("x".into())), 4);
        assert_eq!(exit_code(&AtomiseError::GovernanceRefused("x".into())), 5);
        assert_eq!(exit_code(&AtomiseError::SourceTooSmall), 1);
        assert_eq!(exit_code(&AtomiseError::DbError("x".into())), 6);
        assert_eq!(exit_code(&AtomiseError::SignerError("x".into())), 6);
        assert_eq!(
            exit_code(&AtomiseError::AlreadyAtomised {
                source_id: "s".into(),
                existing_atom_ids: vec!["a".into()]
            }),
            1
        );
    }

    #[test]
    fn error_slug_maps_every_variant() {
        assert_eq!(error_slug(&AtomiseError::NotFound), "not_found");
        assert_eq!(error_slug(&AtomiseError::TierLocked), "tier_locked");
        assert_eq!(
            error_slug(&AtomiseError::CuratorFailed("x".into())),
            "curator_failed"
        );
        assert_eq!(
            error_slug(&AtomiseError::GovernanceRefused("x".into())),
            "governance_refused"
        );
        assert_eq!(
            error_slug(&AtomiseError::SourceTooSmall),
            "source_too_small"
        );
        assert_eq!(error_slug(&AtomiseError::DbError("x".into())), "db_error");
        assert_eq!(
            error_slug(&AtomiseError::SignerError("x".into())),
            "signer_error"
        );
        assert_eq!(
            error_slug(&AtomiseError::AlreadyAtomised {
                source_id: "s".into(),
                existing_atom_ids: vec!["a".into()]
            }),
            "already_atomised"
        );
    }

    #[test]
    fn human_error_message_tier_locked_carries_upgrade_hint() {
        let msg = human_error_message(&AtomiseError::TierLocked, "src");
        assert!(msg.contains("requires smart tier"));
        assert!(msg.contains("keyword"));
        assert!(msg.contains("Upgrade your deployment"));
    }

    #[test]
    fn human_error_message_not_found_carries_source_id() {
        let msg = human_error_message(&AtomiseError::NotFound, "src-123");
        assert!(msg.contains("src-123"), "got: {msg}");
        assert!(msg.contains("not found"));
    }

    #[test]
    fn human_error_message_already_atomised_lists_existing_ids() {
        let err = AtomiseError::AlreadyAtomised {
            source_id: "src-9".into(),
            existing_atom_ids: vec!["a1".into(), "a2".into(), "a3".into()],
        };
        let msg = human_error_message(&err, "src-9");
        assert!(msg.contains("src-9"));
        assert!(msg.contains("3 atoms"));
        assert!(msg.contains("--force"));
        assert!(msg.contains("a1, a2, a3"));
    }

    #[test]
    fn human_error_message_source_too_small_carries_source_id() {
        let msg = human_error_message(&AtomiseError::SourceTooSmall, "src-x");
        assert!(msg.contains("src-x"));
        assert!(msg.contains("max_atom_tokens"));
    }

    #[test]
    fn human_error_message_curator_failed_carries_detail() {
        let msg = human_error_message(&AtomiseError::CuratorFailed("ollama down".into()), "src");
        assert!(msg.contains("ollama down"));
        assert!(msg.contains("Ollama"));
    }

    #[test]
    fn human_error_message_governance_refused_carries_detail() {
        let msg = human_error_message(
            &AtomiseError::GovernanceRefused("atom[2]: policy".into()),
            "src",
        );
        assert!(msg.contains("policy"));
        assert!(msg.contains("atom[2]"));
    }

    #[test]
    fn error_details_already_atomised_carries_payload() {
        let err = AtomiseError::AlreadyAtomised {
            source_id: "s".into(),
            existing_atom_ids: vec!["a".into(), "b".into()],
        };
        let det = error_details(&err).expect("details populated");
        assert_eq!(det["existing_atom_ids"][0].as_str().unwrap(), "a");
        assert_eq!(det["existing_atom_count"].as_i64().unwrap(), 2);
    }

    #[test]
    fn error_details_other_variants_are_none() {
        assert!(error_details(&AtomiseError::NotFound).is_none());
        assert!(error_details(&AtomiseError::TierLocked).is_none());
        assert!(error_details(&AtomiseError::SourceTooSmall).is_none());
        assert!(error_details(&AtomiseError::CuratorFailed("x".into())).is_none());
    }

    #[test]
    fn emit_error_writes_human_message_to_stderr() {
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let code = emit_error(&AtomiseError::NotFound, "src-xyz", false, &mut out).unwrap();
        assert_eq!(code, 2);
        assert!(stdout.is_empty());
        let s = String::from_utf8(stderr).unwrap();
        assert!(s.contains("src-xyz"));
        assert!(s.contains("not found"));
    }

    #[test]
    fn emit_error_writes_json_envelope_to_stderr() {
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let err = AtomiseError::AlreadyAtomised {
            source_id: "src-1".into(),
            existing_atom_ids: vec!["a".into(), "b".into()],
        };
        let code = emit_error(&err, "src-1", true, &mut out).unwrap();
        assert_eq!(code, 1);
        let s = String::from_utf8(stderr).unwrap();
        let v: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
        assert_eq!(v["error"], "already_atomised");
        assert_eq!(v["exit_code"], 1);
        assert_eq!(v["source_id"], "src-1");
        assert_eq!(v["details"]["existing_atom_count"], 2);
    }

    #[test]
    fn emit_success_writes_human_summary_to_stdout() {
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let r = crate::atomisation::AtomiseResult {
            source_id: "src-1".into(),
            atom_ids: vec!["a1".into(), "a2".into()],
            atom_count: 2,
            archived_at: "2026-05-14T00:00:00Z".into(),
        };
        let code = emit_success(&r, false, &mut out).unwrap();
        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        let s = String::from_utf8(stdout).unwrap();
        assert!(s.contains("src-1"));
        assert!(s.contains("2 atoms"));
        assert!(s.contains("2026-05-14T00:00:00Z"));
        assert!(s.contains("a1, a2"));
    }

    #[test]
    fn emit_success_writes_json_envelope_to_stdout() {
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let r = crate::atomisation::AtomiseResult {
            source_id: "src-1".into(),
            atom_ids: vec!["a1".into(), "a2".into()],
            atom_count: 2,
            archived_at: "2026-05-14T00:00:00Z".into(),
        };
        let code = emit_success(&r, true, &mut out).unwrap();
        assert_eq!(code, 0);
        assert!(stderr.is_empty());
        let s = String::from_utf8(stdout).unwrap();
        let v: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
        assert_eq!(v["source_id"], "src-1");
        assert_eq!(v["atom_count"], 2);
        assert_eq!(v["atom_ids"][0], "a1");
        assert_eq!(v["archived_at"], "2026-05-14T00:00:00Z");
    }
}
