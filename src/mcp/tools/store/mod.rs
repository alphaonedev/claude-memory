// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_store` handler.
//!
//! #881 (PR-4): the entry-point handler + dispatch. Per-stage logic
//! lives in sibling sub-modules:
//!
//! * [`validation`]        — `OnConflict` enum + client-default
//!                           resolution.
//! * [`transport`]         — MCP → HTTP federation forward bridge
//!                           (`forward_to_http`, `forward_store_to_http`).
//! * [`synthesis`]         — Form 1 batch-action synthesis call +
//!                           verdict honouring (update / delete).
//! * [`legacy_classifier`] — v0.6.x per-pair contradiction loop +
//!                           post-store autonomy-hook metadata update.
//! * [`embed`]             — source-embed pipeline + HNSW warm-up.
//!
//! Wire compatibility preserved verbatim. Every response field,
//! error message, and tracing label is byte-identical to the
//! pre-#881 monolithic [`handle_store`].

mod embed;
mod legacy_classifier;
mod synthesis;
mod transport;
mod validation;

use crate::db;
use crate::embeddings::Embed;
use crate::hnsw::VectorIndex;
use crate::llm::OllamaClient;
use serde_json::{Value, json};
use std::path::Path;

use self::validation::OnConflict;

// --- Tool handlers ---

/// Minimum content length (bytes) before the post-store autonomy hook
/// will invoke LLM `auto_tag` / `detect_contradiction`. Below this the
/// LLM round-trip cost exceeds the informational payoff. Shared
/// across the per-stage sub-modules.
pub(super) const AUTONOMY_MIN_CONTENT_LEN: usize = 50;

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_store(
    conn: &rusqlite::Connection,
    db_path: &Path,
    params: &Value,
    embedder: Option<&dyn Embed>,
    llm: Option<&OllamaClient>,
    vector_index: Option<&VectorIndex>,
    resolved_ttl: &crate::config::ResolvedTtl,
    autonomous_hooks: bool,
    mcp_client: Option<&str>,
    federation_forward_url: Option<&str>,
) -> Result<Value, String> {
    // v0.7.0 (issue #318) — when operators have configured a federation
    // forward URL, every MCP write routes through the local HTTP daemon
    // so its `broadcast_store_quorum` fanout runs. Direct-SQLite path
    // below is the legacy single-node behaviour, preserved as default
    // for environments without a sibling `ai-memory serve` process.
    if let Some(url) = federation_forward_url {
        return transport::forward_store_to_http(url, params, mcp_client);
    }

    // #881 — input parse + validation + Memory construction extracted
    // to `super::validation::parse_and_build_memory`. Returns the
    // fully-built Memory plus the resolved `OnConflict`, `agent_id`,
    // and `explicit_scope` ready for the governance gate.
    let (mut mem, on_conflict, agent_id, explicit_scope) =
        validation::parse_and_build_memory(params, mcp_client, resolved_ttl, conn)?;

    // v0.7.x Form 6 — substrate-side auto-classify pre_store hook.
    // Consults the namespace `auto_classify_kind` policy (None ⇒ Off).
    // Caller-supplied non-default kind always wins (preserved inside
    // the hook), so this is a no-op when the caller passed an explicit
    // `kind`. The regex pass is allocation-light and runs in tens of
    // microseconds; the optional LLM round-trip is opt-in via the
    // `RegexThenLlm` policy.
    // #880 — `auto_classify_kind` lives on `policy.kind_class` after
    // the governance decomposition.
    let auto_classify_policy = db::resolve_governance_policy(conn, &mem.namespace)
        .and_then(|p| p.kind_class.auto_classify_kind);
    crate::hooks::pre_store::maybe_auto_classify(&mut mem, auto_classify_policy);

    // v0.7.0 K9 — unified permission pipeline. The K9 evaluator
    // composes declarative `[permissions.rules]` matchers + the K3
    // `[permissions].mode` knob + (when wired) hook decisions into
    // a single `Decision`. Deny-first: if a rule denies, we
    // short-circuit before the K3 governance gate ever resolves a
    // policy. Allow falls through to the existing K3 / governance
    // gate so legacy `[governance]` policies continue to work.
    {
        use crate::permissions::{Op, PermissionContext, Permissions};
        let payload = serde_json::to_value(&mem).unwrap_or_default();
        let ctx = PermissionContext {
            op: Op::MemoryStore,
            namespace: mem.namespace.clone(),
            agent_id: agent_id.clone(),
            payload,
        };
        match Permissions::evaluate(&ctx, &[]) {
            crate::permissions::Decision::Allow | crate::permissions::Decision::Modify(_) => {}
            crate::permissions::Decision::Deny(reason) => {
                return Err(format!("store denied by permission rule: {reason}"));
            }
            crate::permissions::Decision::Ask(prompt) => {
                return Ok(json!({
                    "status": "ask",
                    "reason": prompt,
                    "action": "store",
                    "namespace": mem.namespace,
                }));
            }
        }
    }

    // Task 1.9: governance enforcement (store-side).
    {
        use crate::models::{GovernanceDecision, GovernedAction};
        let payload = serde_json::to_value(&mem).unwrap_or_default();
        match db::enforce_governance(
            conn,
            GovernedAction::Store,
            &mem.namespace,
            &agent_id,
            None,
            None,
            &payload,
        )
        .map_err(|e| e.to_string())?
        {
            GovernanceDecision::Allow => {}
            GovernanceDecision::Deny(reason) => {
                return Err(format!("store denied by governance: {reason}"));
            }
            GovernanceDecision::Pending(pending_id) => {
                // v0.7.0 K4 — surface the new pending row through the
                // subscription dispatcher so K10's Approval API sees a
                // uniform stream of `approval_requested` events
                // regardless of which transport (MCP / HTTP) created
                // the row. Best-effort, fire-and-forget: a dispatch
                // failure must not roll back the pending row.
                crate::subscriptions::dispatch_approval_requested(conn, &pending_id, db_path);
                return Ok(json!({
                    "status": "pending",
                    "pending_id": pending_id,
                    "reason": "governance requires approval",
                    "action": "store",
                    "namespace": mem.namespace,
                }));
            }
        }
    }

    // True dedup: check for exact title+namespace match (#97).
    //
    // v0.6.3.1 P2 (G6) — only the Merge policy enters the dedup-then-update
    // branch. `Error` mode already short-circuited above; `Version` mode
    // already rewrote the title to a free suffix so an exact dup cannot
    // exist. Both still call `find_contradictions` so the response can
    // surface `potential_contradictions` (similar-title fuzzy matches).
    let existing = db::find_contradictions(conn, &mem.title, &mem.namespace).unwrap_or_default();

    // v0.7.x Form 1 (#754) — Resolve namespace policy ONCE up-front so
    // both the synthesis path (Form 1) and the synchronous-atomise mode
    // (Form 2) share a single resolution. Falls back to defaults when
    // no namespace standard is configured.
    let ns_policy = db::resolve_governance_policy(conn, &mem.namespace).unwrap_or_default();

    // v0.7.x Form 1 — single batch action-emitting synthesis call
    // BEFORE the SQL write. Gated on: autonomous_hooks + LLM wired +
    // content meets threshold + namespace not internal + the namespace
    // policy has NOT opted in to the legacy per-pair classifier.
    //
    // On success the synthesis verdict drives the per-candidate
    // {add, update, delete, no_op} branch. `update` SKIPs the new-row
    // insert (the merge subsumed the incoming fact). `delete` removes
    // the candidate then proceeds with the standard insert. `add` /
    // `no_op` are pass-throughs to the existing path.
    //
    // v0.7.0 Cluster-B (issue #767):
    //
    // * SEC-1 — every delete verdict is re-checked against K9
    //   `MemoryDelete` BEFORE the row is touched. K9-denied candidates
    //   are dropped from the delete list, never silently applied.
    // * SEC-1 — the per-batch delete count is capped at the namespace's
    //   `synthesis_max_deletes_per_call` (default 1). Over-cap
    //   batches refuse with `synthesis.refused_unbounded_delete`.
    // * COR-5 — every `update` verdict is honoured (not just the
    //   first). A WARN logs when >1 update verbs appear; the
    //   per-batch tally feeds telemetry.
    // * COR-6 — failure surfaces in the response envelope as
    //   `synthesis_failed: true` + reason. The `synthesis_failure_mode`
    //   namespace policy controls whether failure falls through to the
    //   legacy path (default, backward-compatible) or refuses the
    //   write outright.
    // * PERF-7 — per-candidate content is truncated to the namespace's
    //   `synthesis_max_candidate_chars` (default 1500) before being
    //   inlined into the LLM prompt.
    // #881 — Form 1 synthesis pass extracted to `super::synthesis`.
    // Returns the per-candidate update/delete queue + the counts the
    // response envelope echoes back. SEC-1 / COR-5 / COR-6 contracts
    // are encapsulated inside the helper.
    let synthesis_outcome = if synthesis::synthesis_eligible(
        autonomous_hooks,
        llm.is_some(),
        mem.content.len(),
        &mem.namespace,
        &ns_policy,
    ) {
        let llm_client = llm.expect("synthesis_eligible guarantees llm.is_some()");
        synthesis::run_synthesis_pass(llm_client, &mem, &agent_id, &existing, &ns_policy)?
    } else {
        synthesis::SynthesisOutcome::empty()
    };

    // v0.7.x Form 1 — verdict honouring: when the synthesiser elected
    // to UPDATE existing candidates, apply each merge in place.
    //
    // v0.7.0 Cluster-B (COR-5) — HONOUR ALL updates. The first update
    // we apply is the "primary" — the one that subsumes the incoming
    // fact and skips the new-row insert (the response carries that
    // candidate's id back to the caller). Subsequent updates are still
    // applied so the curator's merges actually land in the substrate
    // instead of being silently dropped. A WARN log fired upstream
    // recorded the multi-update case.
    // #881 — verdict honouring extracted to `super::synthesis`. When
    // the synthesiser elected an UPDATE, the helper applies every
    // queued merge + delete and returns the echo response (the new
    // row insert is then skipped — the merge subsumed the incoming
    // fact).
    if let Some(resp) = synthesis::apply_synthesis_updates_and_deletes(
        conn,
        &mem,
        &existing,
        embedder,
        vector_index,
        &synthesis_outcome,
    ) {
        return Ok(resp);
    }
    // When no update fired, apply any queued deletes before the
    // standard insert path proceeds.
    synthesis::apply_pending_synthesis_deletes(conn, &synthesis_outcome);

    let exact_dup = if matches!(on_conflict, OnConflict::Merge) {
        existing
            .iter()
            .find(|c| c.title == mem.title && c.namespace == mem.namespace)
    } else {
        None
    };
    if let Some(dup) = exact_dup {
        // Update existing memory instead of creating a duplicate.
        // Preserve the original agent_id (provenance is immutable) — the
        // existing memory's metadata.agent_id wins over anything in the
        // incoming store.
        let preserved_metadata = crate::identity::preserve_agent_id(&dup.metadata, &mem.metadata);
        let (_found, content_changed) = db::update(
            conn,
            &dup.id,
            None,                       // title (unchanged)
            Some(mem.content.as_str()), // content (update)
            Some(&mem.tier),            // tier
            None,                       // namespace (unchanged)
            Some(&mem.tags),            // tags
            Some(mem.priority),         // priority
            Some(mem.confidence),       // confidence
            None,                       // expires_at
            Some(&preserved_metadata),  // metadata (agent_id preserved)
        )
        .map_err(|e| e.to_string())?;
        // Regenerate embedding if content changed during dedup update
        if content_changed && let Some(emb) = embedder {
            let text = format!("{} {}", mem.title, mem.content);
            if let Ok(embedding) = emb.embed(&text) {
                let _ = db::set_embedding(conn, &dup.id, &embedding);
                if let Some(idx) = vector_index {
                    idx.remove(&dup.id);
                    idx.insert(dup.id.clone(), embedding);
                }
            }
        }
        // #196: echo the preserved agent_id (original on dedup, not the caller's)
        let echoed_agent_id = preserved_metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        return Ok(json!({
            "id": dup.id,
            "tier": mem.tier,
            "title": mem.title,
            "namespace": mem.namespace,
            "agent_id": echoed_agent_id,
            "duplicate": true,
            "action": "updated existing memory"
        }));
    }

    // v0.7.0 (issue #519) — proactive contradiction detection. When
    // an embedder is wired AND the caller did NOT pass `force=true`,
    // scan the top-K most similar live memories in the namespace and
    // refuse the write if any near-duplicate (≥ 0.95 cosine) has a
    // differing content body (deterministic substrate-layer
    // contradiction signal — see `db::proactive_conflict_check`).
    //
    // Bypass with `force=true` for callers that explicitly want the
    // conflicting fact to land alongside the existing one (e.g. a
    // curator pass that intends to revise an earlier claim).
    let force_write = params["force"].as_bool().unwrap_or(false);
    if !force_write && let Some(emb) = embedder {
        let text = format!("{} {}", mem.title, mem.content);
        if let Ok(query_embedding) = emb.embed(&text)
            && let Ok(Some(conflict)) = db::proactive_conflict_check(conn, &mem, &query_embedding)
        {
            tracing::info!(
                target: "memory_store",
                namespace = %mem.namespace,
                existing_id = %conflict.existing_id,
                similarity = conflict.similarity,
                reason = conflict.reason,
                "memory_store refused by proactive conflict detection (#519); \
                 pass force=true to override",
            );
            return Err(format!(
                "CONFLICT: memory near-duplicates an existing memory in namespace \
                 '{}' (existing id: {}, title: '{}', similarity: {:.3}, reason: {}). \
                 Pass force=true to insert anyway.",
                mem.namespace,
                conflict.existing_id,
                conflict.existing_title,
                conflict.similarity,
                conflict.reason,
            ));
        }
    }

    // v0.7 K8 — per-agent quota gate. Pre-write check; on exceeded
    // limit returns a `QUOTA_EXCEEDED` diagnostic naming the limit
    // hit. Bytes counted = (title + content + serialized metadata)
    // to match the post-write `record_op` accounting below.
    let payload_bytes = i64::try_from(
        mem.title.len()
            + mem.content.len()
            + serde_json::to_string(&mem.metadata)
                .map(|s| s.len())
                .unwrap_or(0),
    )
    .unwrap_or(i64::MAX);
    // H12 (#628 blocker): combine the quota check + counter
    // increment in a single atomic transaction so concurrent writers
    // cannot each pass the check and then both bump the counter past
    // the cap.
    if let Err(e) = crate::quotas::check_and_record(
        conn,
        &agent_id,
        crate::quotas::QuotaOp::Memory {
            bytes: payload_bytes,
        },
    ) {
        return Err(e.to_string());
    }

    let actual_id = match db::insert(conn, &mem) {
        Ok(id) => id,
        Err(e) => {
            // Insert failed AFTER we committed quota — refund so the
            // counter reflects only successful stores.
            if let Err(re) = crate::quotas::refund_op(
                conn,
                &agent_id,
                crate::quotas::QuotaOp::Memory {
                    bytes: payload_bytes,
                },
            ) {
                tracing::warn!("quota refund_op failed for agent {}: {}", &agent_id, re);
            }
            // v0.7.0 L1-6 Deliverable E — surface the substrate
            // governance pre-write hook's refusal with a clearly-
            // identifiable wire prefix so MCP clients can distinguish
            // a policy refusal from a database error. The
            // `GOVERNANCE_REFUSED:` prefix mirrors the HTTP layer's
            // `code` field; the operator-authored reason follows
            // verbatim. Refusals on the MCP path are NOT logged at
            // ERROR (it's the documented policy outcome, not a fault).
            if let Some(refusal) = e.downcast_ref::<crate::storage::GovernanceRefusal>() {
                tracing::info!(
                    "mcp store refused by substrate governance: {}",
                    refusal.reason
                );
                return Err(format!("GOVERNANCE_REFUSED: {}", refusal.reason));
            }
            return Err(e.to_string());
        }
    };

    // PR-5 (issue #487): security audit trail. No-op when disabled.
    crate::audit::emit(crate::audit::EventBuilder::new(
        crate::audit::AuditAction::Store,
        crate::audit::actor(
            agent_id.clone(),
            mcp_client.map_or("host_fallback", |_| "mcp_client_info"),
            explicit_scope.clone(),
        ),
        crate::audit::target_memory(
            actual_id.clone(),
            mem.namespace.clone(),
            Some(mem.title.clone()),
            Some(mem.tier.to_string()),
            explicit_scope.clone(),
        ),
    ));

    // Exclude self-ID from contradictions (both proposed and actual, since upsert may reuse existing ID)
    let contradiction_ids: Vec<String> = existing
        .iter()
        .filter(|c| c.id != mem.id && c.id != actual_id)
        .map(|c| c.id.clone())
        .collect();

    // v0.7.x Form 2 (#755) — resolve atomisation execution mode. When
    // policy is `Synchronous`, SKIP source embedding (atoms get their
    // own embed-on-insert path); the synchronous atomise pass runs
    // BELOW after the post-store autonomy hooks. `Deferred` (legacy
    // WT-1-D) and `Off` modes keep the source-embed step.
    let atomise_mode = ns_policy.effective_auto_atomise_mode();
    // #881 — embed pipeline extracted to `super::embed`.
    if !embed::skip_source_embed_for_synchronous_atomise(atomise_mode, mem.content.len())
        && let Some(emb) = embedder
    {
        embed::store_source_embedding(conn, emb, &mem, &actual_id, vector_index);
    }

    // v0.6.0.0 post-store autonomy hooks. When enabled via
    // `AI_MEMORY_AUTONOMOUS_HOOKS=1` or `autonomous_hooks = true` in
    // config.toml AND an LLM is wired AND the content is long enough
    // to be meaningfully taggable, fire `auto_tag` + `detect_contradiction`
    // synchronously and persist the results into the memory's metadata.
    // Best-effort: any LLM error is logged and does not fail the store.
    // Skipped for internal/system namespaces to avoid feedback loops.
    //
    // #881 — extracted to `super::legacy_classifier`.
    let hooks_skipped_reason = legacy_classifier::autonomy_skip_reason(
        autonomous_hooks,
        llm.is_some(),
        mem.content.len(),
        &mem.namespace,
    );
    let autonomy_outcome = if hooks_skipped_reason.is_none()
        && let Some(llm_client) = llm
    {
        legacy_classifier::maybe_run_autonomy_hooks(
            conn, llm_client, &mem, &actual_id, &existing, &ns_policy,
        )
    } else {
        legacy_classifier::AutonomyHookOutcome {
            auto_tags: Vec::new(),
            confirmed_contradictions: Vec::new(),
        }
    };

    // v0.6.0.0: fire webhook subscribers on successful store. Best-effort
    // fire-and-forget — each subscriber gets its own OS thread; the
    // response here does not wait on any webhook dispatch.
    crate::subscriptions::dispatch_event(
        conn,
        "memory_store",
        &actual_id,
        &mem.namespace,
        Some(&agent_id),
        db_path,
    );

    // v0.7.0 WT-1-D — auto-atomisation pre_store substrate hook. The
    // call resolves the namespace policy, token-counts the body, and
    // spawns a detached worker thread when the threshold is exceeded.
    // NEVER blocks the response on the `Deferred` path.
    //
    // v0.7.x Form 2 (#755) — the `Synchronous` mode runs the atomiser
    // INSIDE this handler so atoms surface in recall before the
    // response returns. Source embedding was skipped above; the
    // atomiser archives the parent with `atomised_into > 0` BEFORE
    // the response returns.
    //
    // Refused-store path: this hook is unreachable on a Deny because
    // the governance gate above already short-circuited via Err(...)
    // before we reached `db::insert`. The store-side governance refusal
    // ensures a denied write never feeds the curator.
    let mut atomise_outcome: Option<&'static str> = None;
    {
        // Cluster-F PERF-10 — pass the in-flight Memory by reference
        // along with the resolved `actual_id` (which may differ from
        // `mem.id` under merge-mode upserts). Avoids cloning the
        // multi-KB content / tags / metadata blob just to swap the id.
        match atomise_mode {
            crate::models::AutoAtomiseMode::Synchronous => {
                // Form 2 — synchronous atomise-before-the-response.
                atomise_outcome = Some(crate::hooks::pre_store::run_synchronous_auto_atomise(
                    conn, &mem, &actual_id, &agent_id,
                ));
            }
            crate::models::AutoAtomiseMode::Deferred => {
                // Cluster-F PERF-1 — reuse the caller's connection
                // for policy resolution; the worker thread spawns
                // inside the hook still opens its own connection.
                let _outcome = crate::hooks::pre_store::maybe_enqueue_auto_atomise(
                    conn, &mem, &actual_id, &agent_id,
                );
                // Outcome is for telemetry only; the response shape
                // does NOT surface it (the curator pass is
                // fire-and-forget by design).
            }
            crate::models::AutoAtomiseMode::Off => {
                // Substrate stays quiet for this namespace.
            }
        }
    }

    // #196: echo the resolved agent_id
    let mut response = json!({
        "id": actual_id,
        "tier": mem.tier,
        "title": mem.title,
        "namespace": mem.namespace,
        "agent_id": agent_id,
    });
    if !contradiction_ids.is_empty() {
        response["potential_contradictions"] = json!(contradiction_ids);
    }
    // #881 — autonomy-hook echo extracted to `super::legacy_classifier`.
    legacy_classifier::merge_autonomy_outcome_into_response(&mut response, &autonomy_outcome);
    if let Some(reason) = hooks_skipped_reason
        && autonomous_hooks
    {
        response["autonomy_hook_skipped"] = json!(reason);
    }
    if let Some(counts) = &synthesis_outcome.counts {
        response["synthesis_decisions"] = counts.to_json();
    }
    if let Some(reason) = &synthesis_outcome.failed_reason {
        // v0.7.0 Cluster-B (COR-6) — surface curator failure to the
        // caller. The namespace policy chose to fall through, but the
        // caller still observes that the new write did not benefit
        // from the synthesis pass.
        response["synthesis_failed"] = json!(true);
        response["synthesis_failed_reason"] = json!(reason);
    }
    if let Some(outcome) = atomise_outcome {
        response["atomise_mode"] = json!("synchronous");
        response["atomise_outcome"] = json!(outcome);
    }

    // v0.7.0 Gap 3 (#886) — recall-consumption hook.
    //
    // When the request body cites a prior `recall_id` plus a list
    // of `cited_memory_ids` the caller used to compose this store
    // request, flip the matching `recall_observations` rows to
    // `consumed = TRUE` with `consumed_by_memory_id = actual_id`.
    // Best-effort; a substrate error here does NOT roll back the
    // store (audit-trail discipline: never let the ledger block
    // the underlying write).
    crate::observations::try_mark_consumed_from_params(conn, params, &actual_id);

    Ok(response)
}

// #881 — `handle_store` test scaffold extracted to the sibling
// `tests.rs` file so this module stays focused on production-path
// orchestration. Tests still resolve `super::*` (this module's
// public + private surface) since they live in a child mod.
#[cfg(test)]
#[path = "tests.rs"]
mod tests;
