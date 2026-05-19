// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.x Form 1 synthesis batch-action dispatch + verdict honouring.
//!
//! #881 (PR-4 extraction): split out of the monolithic
//! `src/mcp/tools/store.rs` so the synthesis curator branch lives in
//! its own ~250-LOC module. Wire-compat preserved verbatim: every
//! tracing label, error string, and SynthesisCounts shape matches the
//! pre-#881 inline code path.
//!
//! The synthesis pass runs at `memory_store` time when:
//!
//! * `autonomous_hooks = true`
//! * an LLM client is wired
//! * content meets the [`AUTONOMY_MIN_CONTENT_LEN`] threshold
//! * namespace is not internal (`_*`)
//! * the namespace policy has NOT opted in to the legacy per-pair
//!   classifier (`legacy_per_pair_classifier`)
//!
//! On success, the curator returns a single batch of per-candidate
//! verdicts (`add`/`update`/`delete`/`no_op`). The store handler
//! consumes the verdicts in two phases:
//!
//! 1. [`apply_synthesis_updates_and_deletes`] (this module) applies
//!    every update + delete the verdict elected and returns the
//!    primary-update echo response when one exists. The store
//!    handler short-circuits on a non-`None` return.
//! 2. The remaining `add` / `no_op` verdicts fall through to the
//!    standard `db::insert` path in `mod.rs`.

use serde_json::{Value, json};

use crate::llm::OllamaClient;
use crate::models::{GovernancePolicy, Memory};
use crate::{db, hnsw::VectorIndex};

use super::AUTONOMY_MIN_CONTENT_LEN;

/// Outcome of the synthesis pass that the store handler needs to
/// thread through the rest of the write path.
pub(super) struct SynthesisOutcome {
    pub counts: Option<crate::synthesis::SynthesisCounts>,
    pub updates: Vec<(String, String)>,
    pub deletes: Vec<String>,
    /// `Some(reason)` when synthesis fell through (COR-6). The store
    /// handler surfaces this on the response envelope as
    /// `synthesis_failed: true` + `synthesis_failed_reason`.
    pub failed_reason: Option<String>,
}

impl SynthesisOutcome {
    pub(super) fn empty() -> Self {
        Self {
            counts: None,
            updates: Vec::new(),
            deletes: Vec::new(),
            failed_reason: None,
        }
    }
}

/// v0.7.x Form 1 — single batch action-emitting synthesis call.
///
/// Eligibility, K9 re-check on delete verdicts, delete-cap refusal,
/// and failure-mode handling are encapsulated here so the store
/// handler reads the outcome as a single struct.
///
/// # Errors
///
/// Returns `Err(reason)` when:
///
/// * The verdict's delete count exceeds the namespace's
///   `synthesis_max_deletes_per_call` cap (SEC-1 refusal — surfaced
///   as `GOVERNANCE_REFUSED: synthesis batch attempted ...` per the
///   pre-#881 wire shape).
/// * The namespace's `synthesis_failure_mode` is `BlockWrite` and the
///   curator round-trip failed (COR-6 refusal — surfaced as
///   `SYNTHESIS_FAILED: namespace policy 'block_write' refuses ...`
///   per the pre-#881 wire shape).
pub(super) fn run_synthesis_pass(
    llm: &OllamaClient,
    mem: &Memory,
    agent_id: &str,
    existing: &[Memory],
    ns_policy: &GovernancePolicy,
) -> Result<SynthesisOutcome, String> {
    // Cluster-F PERF-14 — borrow the candidates as `&[&Memory]` so
    // the recall hit-set is NOT cloned just to feed the synthesiser.
    let cands: Vec<&Memory> = existing
        .iter()
        .filter(|c| c.id != mem.id && c.title != mem.title)
        .collect();
    if cands.is_empty() {
        return Ok(SynthesisOutcome::empty());
    }

    // PERF-7 — resolve the per-namespace prompt cap once.
    let cap = ns_policy.effective_synthesis_max_candidate_chars();
    match crate::synthesis::synthesise_with_cap(llm, &mem.title, &mem.content, &cands, cap) {
        Ok(resp) => {
            let counts = crate::synthesis::SynthesisCounts::from_response(&resp);
            tracing::info!(
                target: "synthesis",
                namespace = %mem.namespace,
                add = counts.add,
                update = counts.update,
                delete = counts.delete,
                no_op = counts.no_op,
                "synthesis batch decision",
            );

            // SEC-1 — refuse batches whose delete count exceeds the
            // namespace's per-call cap. This is the unbounded-delete
            // refusal point: the curator may not mass-delete without
            // an explicit K10 approval flow. Audit-honest WARN log.
            let delete_cap = ns_policy.effective_synthesis_max_deletes_per_call() as usize;
            if counts.delete > delete_cap {
                tracing::warn!(
                    target: "synthesis",
                    namespace = %mem.namespace,
                    requested = counts.delete,
                    cap = delete_cap,
                    "synthesis.refused_unbounded_delete",
                );
                return Err(format!(
                    "GOVERNANCE_REFUSED: synthesis batch attempted {} \
                     deletes, exceeding namespace cap of {} (K10 approval \
                     required for unbounded-delete; raise \
                     `synthesis_max_deletes_per_call` to opt in per-namespace)",
                    counts.delete, delete_cap
                ));
            }

            // COR-5 — honour ALL update verdicts in sequence. Emit a
            // WARN when more than one update verb appears so operators
            // can spot the case in telemetry.
            if counts.update > 1 {
                tracing::warn!(
                    target: "synthesis",
                    namespace = %mem.namespace,
                    update_count = counts.update,
                    "synthesis_decisions.update_count > 1; honouring all updates in sequence",
                );
            }
            let mut updates: Vec<(String, String)> = Vec::new();
            let mut deletes: Vec<String> = Vec::new();
            for v in &resp.verdicts {
                match v.verb {
                    crate::synthesis::SynthesisVerb::Update => {
                        let merged = v
                            .merged_content
                            .clone()
                            .unwrap_or_else(|| mem.content.clone());
                        updates.push((v.candidate_id.clone(), merged));
                    }
                    crate::synthesis::SynthesisVerb::Delete => {
                        // SEC-1 — re-check K9 per delete verdict. The
                        // curator's verdict is advice; the K9 pipeline
                        // remains authoritative.
                        if k9_allows_synthesis_delete(&mem.namespace, agent_id, &v.candidate_id) {
                            deletes.push(v.candidate_id.clone());
                        }
                    }
                    crate::synthesis::SynthesisVerb::Add
                    | crate::synthesis::SynthesisVerb::NoOp => {}
                }
            }
            Ok(SynthesisOutcome {
                counts: Some(counts),
                updates,
                deletes,
                failed_reason: None,
            })
        }
        Err(e) => {
            let reason = e.to_string();
            // COR-6 — observe the failure on the response envelope so
            // callers don't silently inherit the legacy fall-through
            // path. Then consult the namespace's `synthesis_failure_mode`
            // policy to decide whether to fall through or block.
            tracing::warn!(
                target: "synthesis",
                namespace = %mem.namespace,
                "synthesis call failed: {reason}",
            );
            match ns_policy.effective_synthesis_failure_mode() {
                crate::models::SynthesisFailureMode::BlockWrite => Err(format!(
                    "SYNTHESIS_FAILED: namespace policy `block_write` refuses \
                     the store while the curator is unavailable: {reason}"
                )),
                crate::models::SynthesisFailureMode::FallThrough => Ok(SynthesisOutcome {
                    counts: None,
                    updates: Vec::new(),
                    deletes: Vec::new(),
                    failed_reason: Some(reason),
                }),
            }
        }
    }
}

/// SEC-1 helper — consult the K9 permission pipeline on a synthesis
/// delete verdict. Returns `true` when K9 allows (Allow / Modify);
/// `false` when K9 denies or asks for approval (the synthesis path
/// has no operator UI to surface a prompt). The store handler's
/// audit-honest WARN logs the deny/ask reason verbatim — preserved
/// here so the call sites stay aligned with the pre-#881 trace
/// output.
fn k9_allows_synthesis_delete(namespace: &str, agent_id: &str, candidate_id: &str) -> bool {
    use crate::permissions::{Decision, Op, PermissionContext, Permissions};
    let payload = json!({
        "id": candidate_id,
        "via": "synthesis_verdict",
    });
    let ctx = PermissionContext {
        op: Op::MemoryDelete,
        namespace: namespace.to_string(),
        agent_id: agent_id.to_string(),
        payload,
    };
    match Permissions::evaluate(&ctx, &[]) {
        Decision::Allow | Decision::Modify(_) => true,
        Decision::Deny(reason) => {
            tracing::warn!(
                target: "synthesis",
                namespace = %namespace,
                candidate_id = %candidate_id,
                "synthesis delete verdict denied by K9: {reason}",
            );
            false
        }
        Decision::Ask(reason) => {
            // Ask outside K10 flow → treat as deny on the synthesis
            // path (no operator UI to surface the prompt).
            // Curator-driven deletes that need approval must be
            // promoted to an explicit `memory_delete` call.
            tracing::warn!(
                target: "synthesis",
                namespace = %namespace,
                candidate_id = %candidate_id,
                "synthesis delete verdict held for approval (ask): {reason}; \
                 skipping in this batch",
            );
            false
        }
    }
}

/// v0.7.x Form 1 verdict honouring — apply every queued update +
/// delete from the synthesis pass and return the primary-update
/// response envelope when one exists.
///
/// Returns `Some(response)` when the synthesiser elected to UPDATE an
/// existing candidate (the merge subsumes the incoming fact, the new
/// row insert is skipped, and the response echoes the merged
/// candidate's id). Returns `None` when no updates ran, in which case
/// the standard insert path proceeds in the store handler.
///
/// Queued deletes that target the primary-update id are skipped so
/// the store handler does not delete the very row it just merged the
/// incoming fact into.
pub(super) fn apply_synthesis_updates_and_deletes(
    conn: &rusqlite::Connection,
    mem: &Memory,
    existing: &[Memory],
    embedder: Option<&dyn crate::embeddings::Embed>,
    vector_index: Option<&VectorIndex>,
    outcome: &SynthesisOutcome,
) -> Option<Value> {
    let primary_update = outcome.updates.first().cloned();
    let (primary_id, _) = primary_update.as_ref()?;

    // Apply every queued update in sequence.
    for (cand_id, merged_content) in &outcome.updates {
        let Some(target) = existing.iter().find(|c| c.id == *cand_id).cloned() else {
            tracing::warn!(
                target: "synthesis",
                "synthesis update target {cand_id} not found in candidate set",
            );
            continue;
        };
        let preserved_metadata =
            crate::identity::preserve_agent_id(&target.metadata, &mem.metadata);
        let upd = db::update(
            conn,
            cand_id,
            None,
            Some(merged_content.as_str()),
            Some(&mem.tier),
            None,
            Some(&mem.tags),
            Some(mem.priority),
            Some(mem.confidence),
            None,
            Some(&preserved_metadata),
        );
        let (_found, content_changed) = match upd {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "synthesis",
                    "synthesis update failed for {cand_id}: {e}",
                );
                continue;
            }
        };
        if content_changed && let Some(emb) = embedder {
            let text = format!("{} {}", target.title, merged_content);
            if let Ok(embedding) = emb.embed(&text) {
                let _ = db::set_embedding(conn, cand_id, &embedding);
                if let Some(idx) = vector_index {
                    idx.remove(cand_id);
                    idx.insert(cand_id.to_string(), embedding);
                }
            }
        }
    }

    // Apply queued deletes from the same batch (skip the primary
    // update target so we don't delete the very row we just merged
    // the incoming fact into).
    for del_id in &outcome.deletes {
        if del_id == primary_id {
            continue;
        }
        if let Err(e) = db::delete(conn, del_id) {
            tracing::warn!(
                target: "synthesis",
                "synthesis delete failed for {del_id}: {e}",
            );
        }
    }

    // Construct the response from the PRIMARY update's target.
    let target = existing.iter().find(|c| c.id == *primary_id).cloned()?;
    let preserved_metadata = crate::identity::preserve_agent_id(&target.metadata, &mem.metadata);
    let echoed_agent_id = preserved_metadata
        .get("agent_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let mut resp = json!({
        "id": target.id,
        "tier": mem.tier,
        "title": target.title,
        "namespace": mem.namespace,
        "agent_id": echoed_agent_id,
        "duplicate": true,
        "action": "synthesised: update existing memory",
    });
    if let Some(c) = &outcome.counts {
        resp["synthesis_decisions"] = c.to_json();
    }
    if let Some(reason) = &outcome.failed_reason {
        resp["synthesis_failed"] = json!(true);
        resp["synthesis_failed_reason"] = json!(reason);
    }
    Some(resp)
}

/// Apply pending delete verdicts when no update fired — the store
/// handler runs the standard `db::insert` afterward.
pub(super) fn apply_pending_synthesis_deletes(
    conn: &rusqlite::Connection,
    outcome: &SynthesisOutcome,
) {
    if !outcome.updates.is_empty() {
        return;
    }
    for del_id in &outcome.deletes {
        if let Err(e) = db::delete(conn, del_id) {
            tracing::warn!(
                target: "synthesis",
                "synthesis delete failed for {del_id}: {e}",
            );
        }
    }
}

/// Eligibility predicate for the synthesis pass. Lifted from the
/// inline guard in `handle_store` so the store handler reads a
/// single boolean.
pub(super) fn synthesis_eligible(
    autonomous_hooks: bool,
    llm_present: bool,
    content_len: usize,
    namespace: &str,
    ns_policy: &GovernancePolicy,
) -> bool {
    autonomous_hooks
        && llm_present
        && content_len >= AUTONOMY_MIN_CONTENT_LEN
        && !namespace.starts_with('_')
        && !ns_policy.effective_legacy_per_pair_classifier()
}
