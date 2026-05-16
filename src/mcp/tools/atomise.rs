// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_atomise` handler — v0.7.0 WT-1-C.
//!
//! Wraps the [`crate::atomisation::Atomiser`] substrate primitive from
//! WT-1-B (the engine that decomposes a coarse-grained memory into
//! atomic propositions and re-writes them as first-class memories with
//! `derives_from` provenance) as a curator-pass MCP tool in the same
//! family / profile group as `memory_consolidate` and `memory_reflect`.
//!
//! # Tier gating
//!
//! Atomisation requires the curator LLM, so:
//!
//! * `Keyword` → returns the "tier-locked" advisory envelope
//!   (informational; not a JSON-RPC error). The advisory shape mirrors
//!   the rest of the v0.7 tier-gated tools.
//! * `Semantic` / `Smart` / `Autonomous` → forwarded to
//!   `atomiser.atomise()`. The atomiser itself short-circuits with
//!   `AtomiseError::TierLocked` when the engine's resolved tier is
//!   `Keyword`, but the dispatcher only calls into this handler with
//!   an Atomiser if the daemon has LLM, so the second guard is the
//!   defense-in-depth layer.
//!
//! # Error mapping
//!
//! Per the WT-1-C brief:
//!
//! * `NotFound` → `Err("MEMORY_NOT_FOUND: ...")` (collapses to MCP
//!   `isError: true` per the spec; the prefix is the wire-stable
//!   code clients branch on).
//! * `AlreadyAtomised` → 200 OK with `{already_atomised: true,
//!   existing_atom_ids: [...]}` — INFORMATIONAL, not an error. This is
//!   load-bearing: idempotent re-calls are the happy path on the
//!   curator-pass retry contract.
//! * `TierLocked` → 200 OK with a tier-locked advisory envelope
//!   (`{tier-locked: "memory_atomise requires smart tier or higher",
//!   current_tier, required_tier}`).
//! * `CuratorFailed` → `Err("CURATOR_FAILED: ...")` with the parser
//!   diagnostic.
//! * `SourceTooSmall` → 200 OK with `{source_too_small: true,
//!   message}`.
//! * `GovernanceRefused` → `Err("GOVERNANCE_REFUSED atom[N]: ...")`
//!   carrying the refused atom index. Prior atoms in the batch were
//!   committed; the caller can list them via `memory_atomise`'s next
//!   call with `force_re_atomise=false` (the AlreadyAtomised path) if
//!   it wants to see what made it through before the refusal.
//!
//! # MCP error-envelope convention
//!
//! Per the v0.7 MCP-spec compliance work (see `mcp::handle_request`'s
//! 2025-03-26 §"Tool result" comment), handler-level errors collapse
//! to a successful JSON-RPC result with `isError: true` and a text
//! body. The string codes (`MEMORY_NOT_FOUND`, `CURATOR_FAILED`,
//! `GOVERNANCE_REFUSED`) are the wire-stable discriminators clients
//! key off. The brief's reference to "JSON-RPC -32602/-32603" is the
//! pre-MCP-spec semantic intent; the on-wire shape follows
//! `crate::mcp::tools::reflect`'s `REFLECTION_DEPTH_EXCEEDED`
//! convention.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::atomisation::{AtomiseError, Atomiser};
use crate::config::FeatureTier;

/// Handler-side bundle. Keeps the [`Atomiser`] (the WT-1-B engine)
/// behind an `Arc` so the dispatcher can construct one at server boot
/// and re-use it across every `memory_atomise` call.
///
/// `tier` is the daemon's resolved feature tier. The handler consults
/// it BEFORE asking the atomiser to do any work so the keyword-tier
/// short-circuit doesn't need a DB read.
pub struct AtomiseToolHandler {
    pub atomiser: Arc<Atomiser>,
    /// Daemon's resolved feature tier. Retained as defense-in-depth
    /// so a future caller that wires the handler outside the
    /// MCP-server context (e.g. an HTTP daemon surface) still has the
    /// tier available without re-plumbing the resolver. The MCP path
    /// passes its own `tier` to [`handle_atomise`] which short-
    /// circuits BEFORE consulting the handler, so the two values are
    /// kept in sync by construction.
    #[allow(dead_code)]
    pub tier: FeatureTier,
}

impl AtomiseToolHandler {
    /// Construct a handler with the supplied atomiser and tier.
    #[must_use]
    pub fn new(atomiser: Arc<Atomiser>, tier: FeatureTier) -> Self {
        Self { atomiser, tier }
    }
}

/// Required-tier label for the tier-locked advisory envelope. Surfaced
/// in the response so an agent on the keyword tier knows which tier
/// hint to pass on restart.
const REQUIRED_TIER: &str = "smart";

/// Handle a `memory_atomise` MCP tool call.
///
/// The handler shape mirrors the other curator-pass tools
/// (`memory_consolidate`, `memory_reflect`): synchronous + threaded
/// `&rusqlite::Connection`, params bag is a `&Value`. Errors are
/// returned as `Err(String)`; the dispatcher wraps them into the
/// MCP `isError: true` envelope.
///
/// # Arguments
///
/// * `conn` — substrate connection (write path).
/// * `params` — the JSON-RPC `arguments` object. Schema:
///   `{ memory_id: string, max_atom_tokens?: int, force_re_atomise?: bool }`.
/// * `handler` — pre-built handler (or `None` when the daemon has no
///   LLM, which collapses to the tier-locked advisory).
/// * `tier` — fallback tier when `handler` is `None` (so the
///   tier-locked envelope still carries the correct `current_tier`).
/// * `mcp_client` — captured `clientInfo.name` for the calling-agent
///   resolution chain.
pub fn handle_atomise(
    conn: &rusqlite::Connection,
    params: &Value,
    handler: Option<&AtomiseToolHandler>,
    tier: FeatureTier,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    // ── Argument validation ─────────────────────────────────────────
    let memory_id = params
        .get("memory_id")
        .ok_or("memory_id is required")?
        .as_str()
        .ok_or("memory_id must be a string")?;
    if memory_id.is_empty() {
        return Err("memory_id must not be empty".to_string());
    }

    // max_atom_tokens — default 200, range [50, 1000]. We reject
    // non-integer (string, bool, null) values explicitly so the agent
    // sees a clean validation error rather than a silent default.
    let max_atom_tokens: u32 = if let Some(v) = params.get("max_atom_tokens") {
        if v.is_null() {
            200
        } else {
            let n = v
                .as_i64()
                .ok_or("max_atom_tokens must be an integer in [50, 1000] (default 200)")?;
            if !(50..=1000).contains(&n) {
                return Err(format!("max_atom_tokens out of range [50, 1000]: {n}"));
            }
            u32::try_from(n).expect("range-checked above")
        }
    } else {
        200
    };

    // force_re_atomise — default false. Type-strict: reject anything
    // that isn't a JSON bool (the brief calls out `force_re_atomise="yes"`
    // as an explicit rejection case).
    let force_re_atomise: bool = if let Some(v) = params.get("force_re_atomise") {
        if v.is_null() {
            false
        } else {
            v.as_bool().ok_or("force_re_atomise must be a boolean")?
        }
    } else {
        false
    };

    // ── Tier gate (keyword short-circuit) ───────────────────────────
    // Resolved BEFORE the handler dispatch so the keyword tier never
    // touches the DB. The advisory envelope is the substrate-wide
    // tier-locked shape (200 OK, NOT JSON-RPC error — per the brief
    // and the rest of the v0.7 tier-gated surface).
    if tier == FeatureTier::Keyword || handler.is_none() {
        return Ok(json!({
            "tier-locked": "memory_atomise requires smart tier or higher",
            "current_tier": tier.as_str(),
            "required_tier": REQUIRED_TIER,
        }));
    }
    let handler = handler.expect("checked above");

    // ── Calling agent resolution (NHI) ──────────────────────────────
    let explicit_agent_id = params.get("agent_id").and_then(Value::as_str);
    let calling_agent_id = crate::identity::resolve_agent_id(explicit_agent_id, mcp_client)
        .map_err(|e| e.to_string())?;

    // ── Engine dispatch ─────────────────────────────────────────────
    match handler.atomiser.atomise_sync(
        conn,
        memory_id,
        max_atom_tokens,
        force_re_atomise,
        &calling_agent_id,
    ) {
        Ok(result) => Ok(json!({
            "source_id": result.source_id,
            "atom_ids": result.atom_ids,
            "atom_count": result.atom_count,
            "archived_at": result.archived_at,
        })),
        Err(AtomiseError::NotFound) => Err(format!("MEMORY_NOT_FOUND: {memory_id}")),
        Err(AtomiseError::AlreadyAtomised {
            source_id,
            existing_atom_ids,
        }) => Ok(json!({
            "already_atomised": true,
            "source_id": source_id,
            "existing_atom_ids": existing_atom_ids,
            "atom_count": existing_atom_ids.len(),
        })),
        Err(AtomiseError::TierLocked) => Ok(json!({
            "tier-locked": "memory_atomise requires smart tier or higher",
            "current_tier": tier.as_str(),
            "required_tier": REQUIRED_TIER,
        })),
        Err(AtomiseError::CuratorFailed(detail)) => Err(format!("CURATOR_FAILED: {detail}")),
        Err(AtomiseError::SourceTooSmall) => Ok(json!({
            "source_too_small": true,
            "source_id": memory_id,
            "message": "source body is already at or under max_atom_tokens — no decomposition possible",
        })),
        Err(AtomiseError::GovernanceRefused(detail)) => {
            // The atomiser embeds the failing atom index in the
            // diagnostic as `atom[N]: <reason>` (see
            // `Atomiser::atomise_sync` step 8). We surface it
            // verbatim so the operator's log analyser sees the
            // index without a second roundtrip.
            Err(format!("GOVERNANCE_REFUSED: {detail}"))
        }
        Err(AtomiseError::SignerError(detail)) => Err(format!("SIGNER_ERROR: {detail}")),
        Err(AtomiseError::DbError(detail)) => Err(format!("DB_ERROR: {detail}")),
    }
}

// ---------------------------------------------------------------------------
// Unit tests — focus on the argument-parsing and tier-gate branches
// (which do NOT require a live atomiser / DB). The full happy-path
// and error-path coverage lives in the integration suite at
// `tests/wt1c_mcp_atomise.rs`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomisation::AtomiserConfig;
    use crate::atomisation::curator::{Atom, Curator, CuratorError};
    use crate::storage as db;
    use std::sync::Mutex;
    use tempfile::NamedTempFile;

    /// Deterministic mock — pops a canned response queue. Mirrors the
    /// shape used by `tests/atomisation/core.rs` so the engine sees a
    /// familiar trait object.
    struct MockCurator {
        responses: Mutex<Vec<Result<Vec<Atom>, CuratorError>>>,
    }

    impl MockCurator {
        fn new(responses: Vec<Result<Vec<Atom>, CuratorError>>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    impl Curator for MockCurator {
        fn decompose(
            &self,
            _body: &str,
            _max_atom_tokens: u32,
            _max_retries: u32,
        ) -> Result<Vec<Atom>, CuratorError> {
            let mut q = self.responses.lock().unwrap();
            if q.is_empty() {
                return Err(CuratorError::MalformedResponse(
                    "mock: queue exhausted".into(),
                ));
            }
            q.remove(0)
        }
    }

    fn fresh_db() -> (NamedTempFile, rusqlite::Connection) {
        let tmp = NamedTempFile::new().expect("tempfile");
        let conn = db::open(tmp.path()).expect("db::open");
        (tmp, conn)
    }

    fn handler(tier: FeatureTier) -> AtomiseToolHandler {
        let curator: Box<dyn Curator> = Box::new(MockCurator::new(vec![]));
        let atomiser = Arc::new(Atomiser::new(
            curator,
            None,
            AtomiserConfig::default(),
            tier,
        ));
        AtomiseToolHandler::new(atomiser, tier)
    }

    #[test]
    fn missing_memory_id_errors() {
        let (_tmp, conn) = fresh_db();
        let h = handler(FeatureTier::Smart);
        let err =
            handle_atomise(&conn, &json!({}), Some(&h), FeatureTier::Smart, None).unwrap_err();
        assert!(err.contains("memory_id is required"), "got: {err}");
    }

    #[test]
    fn non_string_memory_id_errors() {
        let (_tmp, conn) = fresh_db();
        let h = handler(FeatureTier::Smart);
        let err = handle_atomise(
            &conn,
            &json!({"memory_id": 42}),
            Some(&h),
            FeatureTier::Smart,
            None,
        )
        .unwrap_err();
        assert!(err.contains("must be a string"), "got: {err}");
    }

    #[test]
    fn empty_memory_id_errors() {
        let (_tmp, conn) = fresh_db();
        let h = handler(FeatureTier::Smart);
        let err = handle_atomise(
            &conn,
            &json!({"memory_id": ""}),
            Some(&h),
            FeatureTier::Smart,
            None,
        )
        .unwrap_err();
        assert!(err.contains("must not be empty"), "got: {err}");
    }

    #[test]
    fn max_atom_tokens_zero_rejected() {
        let (_tmp, conn) = fresh_db();
        let h = handler(FeatureTier::Smart);
        let err = handle_atomise(
            &conn,
            &json!({"memory_id": "11111111-2222-3333-4444-555555555555", "max_atom_tokens": 0}),
            Some(&h),
            FeatureTier::Smart,
            None,
        )
        .unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
    }

    #[test]
    fn max_atom_tokens_below_range_rejected() {
        let (_tmp, conn) = fresh_db();
        let h = handler(FeatureTier::Smart);
        let err = handle_atomise(
            &conn,
            &json!({"memory_id": "11111111-2222-3333-4444-555555555555", "max_atom_tokens": 49}),
            Some(&h),
            FeatureTier::Smart,
            None,
        )
        .unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
    }

    #[test]
    fn max_atom_tokens_above_range_rejected() {
        let (_tmp, conn) = fresh_db();
        let h = handler(FeatureTier::Smart);
        let err = handle_atomise(
            &conn,
            &json!({"memory_id": "11111111-2222-3333-4444-555555555555", "max_atom_tokens": 1001}),
            Some(&h),
            FeatureTier::Smart,
            None,
        )
        .unwrap_err();
        assert!(err.contains("out of range"), "got: {err}");
    }

    #[test]
    fn max_atom_tokens_non_int_rejected() {
        let (_tmp, conn) = fresh_db();
        let h = handler(FeatureTier::Smart);
        let err = handle_atomise(
            &conn,
            &json!({
                "memory_id": "11111111-2222-3333-4444-555555555555",
                "max_atom_tokens": "200"
            }),
            Some(&h),
            FeatureTier::Smart,
            None,
        )
        .unwrap_err();
        assert!(err.contains("integer"), "got: {err}");
    }

    #[test]
    fn force_re_atomise_string_rejected() {
        let (_tmp, conn) = fresh_db();
        let h = handler(FeatureTier::Smart);
        let err = handle_atomise(
            &conn,
            &json!({
                "memory_id": "11111111-2222-3333-4444-555555555555",
                "force_re_atomise": "yes"
            }),
            Some(&h),
            FeatureTier::Smart,
            None,
        )
        .unwrap_err();
        assert!(err.contains("boolean"), "got: {err}");
    }

    #[test]
    fn keyword_tier_returns_tier_locked_advisory() {
        let (_tmp, conn) = fresh_db();
        let resp = handle_atomise(
            &conn,
            &json!({"memory_id": "11111111-2222-3333-4444-555555555555"}),
            None,
            FeatureTier::Keyword,
            None,
        )
        .expect("tier-locked is informational, not an error");
        assert_eq!(
            resp["tier-locked"].as_str(),
            Some("memory_atomise requires smart tier or higher")
        );
        assert_eq!(resp["current_tier"].as_str(), Some("keyword"));
        assert_eq!(resp["required_tier"].as_str(), Some("smart"));
    }

    #[test]
    fn handler_none_at_higher_tier_still_tier_locked() {
        // Defense in depth — if the dispatcher fails to construct a
        // handler (no LLM available even at semantic tier), surface
        // the tier-locked envelope rather than a panic.
        let (_tmp, conn) = fresh_db();
        let resp = handle_atomise(
            &conn,
            &json!({"memory_id": "11111111-2222-3333-4444-555555555555"}),
            None,
            FeatureTier::Semantic,
            None,
        )
        .expect("absence of handler collapses to tier-locked, not error");
        assert!(resp["tier-locked"].is_string());
    }

    #[test]
    fn memory_not_found_returns_typed_error() {
        let (_tmp, conn) = fresh_db();
        let h = handler(FeatureTier::Smart);
        // No row inserted; the engine will return NotFound.
        let err = handle_atomise(
            &conn,
            &json!({"memory_id": "11111111-2222-3333-4444-555555555555"}),
            Some(&h),
            FeatureTier::Smart,
            None,
        )
        .unwrap_err();
        assert!(err.starts_with("MEMORY_NOT_FOUND:"), "got: {err}");
    }
}
