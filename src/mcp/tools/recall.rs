// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_recall` handler and namespace-chain helpers.

use crate::embeddings::Embed;
use crate::hnsw::VectorIndex;
use crate::models::{CandidateCounts, Memory, RecallMeta, RecallTelemetry};
use crate::reranker::BatchedReranker;
use crate::{db, validate};
use serde_json::{Value, json};

/// Build the standards-inheritance chain for a namespace, most-general
/// first. Task 1.6 extends this from the historical 3-level scheme
/// (global → parent → namespace) to N levels by walking the `/`-derived
/// ancestors from [`crate::models::namespace_ancestors`] plus any
/// `namespace_meta` explicit-parent chain rooted at the top of the
/// hierarchical path (which keeps legacy flat-namespace setups working).
///
/// Returned vector is top-down: `[*, org, unit, team, agent]` for a
/// 4-level hierarchical namespace. Cycle-safe and bounded.
/// Display-side wrapper around [`db::build_namespace_chain`].
///
/// v0.6.3.1 (P4, audit G1): the chain walker moved into `db.rs` so the
/// governance enforcement gate could share a single canonical
/// implementation with the recall/standard injection paths. This thin
/// shim keeps existing call sites compiling without re-routing every
/// invocation through `db::`.

pub async fn handle_recall_with_pre_recall_hook(
    conn: &rusqlite::Connection,
    params: &Value,
    embedder: Option<&dyn Embed>,
    vector_index: Option<&VectorIndex>,
    reranker: Option<&BatchedReranker>,
    archive_on_gc: bool,
    resolved_ttl: &crate::config::ResolvedTtl,
    resolved_scoring: &crate::config::ResolvedScoring,
    chain: &crate::hooks::HookChain,
    registry: &mut crate::hooks::ExecutorRegistry,
    // v0.7.0 (issue #518) — recall scope defaults; forwarded
    // unchanged to `handle_recall`.
    recall_scope: Option<&crate::config::RecallScope>,
) -> Result<Value, String> {
    // Resolve the (query, namespace, k) triple once so the hook
    // sees exactly what the recall would see.
    let context = params["context"].as_str().ok_or("context is required")?;
    let namespace = params["namespace"].as_str().unwrap_or("");
    let k = u32::try_from(params["limit"].as_u64().unwrap_or(10)).unwrap_or(u32::MAX);

    // Fire the hot-path chain. The chain runner enforces the 50ms
    // class deadline (G6); a hook that exceeds it converts to
    // fail-open Allow per the configured `FailMode`.
    let outcome =
        crate::hooks::apply_pre_recall_expand(context, namespace, k, chain, registry).await;

    if let crate::hooks::PreRecallOutcome::Denied { reason, code } = &outcome {
        // The recall is suppressed. Return the same envelope shape
        // a normal empty recall would produce, decorated with a
        // `meta.diagnostic.pre_recall_denied` block so the caller
        // can distinguish "no matches" from "blocked by hook".
        let mut resp = json!({
            "memories": [],
            "count": 0,
            "mode": "denied_by_hook",
        });
        let meta = resp
            .as_object_mut()
            .expect("recall response is always a JSON object")
            .entry("meta".to_string())
            .or_insert_with(|| json!({}));
        meta["diagnostic"] = json!({
            "pre_recall_denied": {
                "reason": reason,
                "code": code,
            }
        });
        return Ok(resp);
    }

    // Apply any Modify-side rewrites onto the params bag before
    // calling the sync recall path. We clone the input so the
    // caller's Value is left untouched.
    let mut effective = params.clone();
    if let crate::hooks::PreRecallOutcome::Modified {
        query: q,
        namespace: ns,
        k: nk,
    } = outcome
    {
        if let Some(obj) = effective.as_object_mut() {
            obj.insert("context".to_string(), json!(q));
            // Only inject `namespace` if the hook actually rewrote
            // it (vs leaving the original empty-string default).
            if !ns.is_empty() {
                obj.insert("namespace".to_string(), json!(ns));
            }
            obj.insert("limit".to_string(), json!(u64::from(nk)));
        }
    }

    handle_recall(
        conn,
        &effective,
        embedder,
        vector_index,
        reranker,
        archive_on_gc,
        resolved_ttl,
        resolved_scoring,
        recall_scope,
    )
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub fn handle_recall(
    conn: &rusqlite::Connection,
    params: &Value,
    embedder: Option<&dyn Embed>,
    vector_index: Option<&VectorIndex>,
    reranker: Option<&BatchedReranker>,
    archive_on_gc: bool,
    resolved_ttl: &crate::config::ResolvedTtl,
    resolved_scoring: &crate::config::ResolvedScoring,
    // v0.7.0 (issue #518) — operator-configured recall defaults.
    // When `session_default=true` is set on the request AND a given
    // filter axis is absent, the corresponding `recall_scope` field
    // is spliced into the request before the storage call. `None`
    // keeps v0.6.x recall semantics exactly.
    recall_scope: Option<&crate::config::RecallScope>,
) -> Result<Value, String> {
    // Helper: serialize scored memories with score field (#95)
    fn scored_memories(results: Vec<(Memory, f64)>) -> Vec<Value> {
        results
            .into_iter()
            .map(|(mem, score)| {
                let mut val = serde_json::to_value(&mem).unwrap_or_default();
                if let Some(obj) = val.as_object_mut() {
                    obj.insert(
                        "score".to_string(),
                        json!((score * 1000.0).round() / 1000.0),
                    );
                }
                val
            })
            .collect()
    }

    let _ = db::gc_if_needed(conn, archive_on_gc);
    let context = params["context"].as_str().ok_or("context is required")?;
    // v0.7.0 (issue #518) — when the caller passed
    // `session_default=true` AND a given filter axis is absent,
    // splice in the corresponding `[agents.defaults.recall_scope]`
    // value. Explicit args always win. Sqlite recall does not
    // expose a `tier` filter on the legacy `db::recall` /
    // `db::recall_hybrid` paths, so the `tier` axis is plumbed but
    // not consumed on this branch (the postgres SAL handler in
    // `handlers.rs::recall_response` applies it via
    // `Filter.tier`).
    let session_default = params["session_default"].as_bool().unwrap_or(false);
    let scope = if session_default { recall_scope } else { None };
    // Compute owned defaults so they outlive the parse step.
    let scope_namespace: Option<String> = scope
        .and_then(|s| s.namespaces.as_ref())
        .and_then(|v| v.first())
        .cloned();
    let scope_since: Option<String> = scope.and_then(|s| {
        s.since.as_deref().and_then(|d| {
            crate::config::parse_duration_string(d).map(|dur| {
                let cutoff = chrono::Utc::now() - dur;
                cutoff.to_rfc3339()
            })
        })
    });
    let explicit_namespace = params["namespace"].as_str();
    let explicit_since = params["since"].as_str();
    let namespace: Option<&str> = explicit_namespace.or(scope_namespace.as_deref());
    let explicit_limit_raw = params["limit"].as_u64();
    let limit = if let Some(v) = explicit_limit_raw {
        usize::try_from(v).unwrap_or(usize::MAX)
    } else if let Some(v) = scope.and_then(|s| s.limit) {
        usize::try_from(v).unwrap_or(usize::MAX)
    } else {
        10
    };
    let tags = params["tags"].as_str();
    let since: Option<&str> = explicit_since.or(scope_since.as_deref());
    let until = params["until"].as_str();
    // #151 visibility
    let as_agent = params["as_agent"].as_str();
    if let Some(a) = as_agent {
        validate::validate_namespace(a).map_err(|e| e.to_string())?;
    }
    // Task 1.11 / Phase P6 (R1): optional token budget. R1 semantics
    // permit `0` ("give me nothing") and return an empty result with
    // `meta.budget_overflow = false` — see the comment on
    // `db::apply_token_budget`. This supersedes the v0.6.3 Ultrareview
    // #348 hard-reject of 0; the meta block now disambiguates "user
    // asked for zero" from "buggy uninitialized counter" by always
    // round-tripping the requested budget.
    let budget_tokens = params["budget_tokens"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok());

    // v0.6.0.0 contextual recall — caller-supplied recent conversation tokens.
    let context_tokens: Vec<String> = params["context_tokens"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Helper: tack tokens_used / budget_tokens onto the response, plus
    // — when a budget was supplied — the Phase P6 RecallMeta-style
    // sub-block (`meta.budget_tokens_used`, `budget_tokens_remaining`,
    // `memories_dropped`, `budget_overflow`). The legacy top-level
    // `tokens_used` / `budget_tokens` fields are preserved verbatim so
    // pre-P6 callers continue to work byte-for-byte.
    //
    // NOTE on RecallMeta: Phase P3 introduces a top-level `meta` block
    // (recall_mode, reranker_used, candidate_counts, blend_weight). This
    // P6 worktree pre-dates the P3 merge, so we define the budget-mode
    // sub-block directly under `meta.budget` and let P3's rebase fold
    // its fields in alongside ours. See REMEDIATIONv0631.md L488-489.
    let decorate_budget = |resp: &mut Value, outcome: &db::BudgetOutcome| {
        resp["tokens_used"] = json!(outcome.tokens_used);
        if let Some(b) = budget_tokens {
            resp["budget_tokens"] = json!(b);
            // Phase P6 R1 meta block. Always emitted when a budget is
            // supplied so callers can rely on the field set. Kept under
            // a dedicated `meta` key so the top-level shape stays
            // backward-compatible — pre-P6 callers ignore unknown keys.
            let meta = resp
                .as_object_mut()
                .expect("recall response is always a JSON object")
                .entry("meta".to_string())
                .or_insert_with(|| json!({}));
            meta["budget_tokens_used"] = json!(outcome.tokens_used);
            meta["budget_tokens_remaining"] = json!(outcome.tokens_remaining.unwrap_or(0));
            meta["memories_dropped"] = json!(outcome.memories_dropped);
            meta["budget_overflow"] = json!(outcome.budget_overflow);
        }
    };

    // v0.6.3.1 (P3): build the per-request meta block from retrieval-stage
    // telemetry + the runtime reranker variant. The block is always
    // present in the response — clients that don't read it ignore unknown
    // fields per JSON-RPC convention. Closes audit gaps G2/G8/G11 by
    // making silent-degrade paths visible at request time.
    // v0.7.0 R3-S2 — distinguish *originally lexical* from
    // *degraded lexical* so the recall response surfaces an in-band
    // signal when the operator's configured neural cross-encoder
    // failed to load and fell back. Pre-R3 this was a tracing-event-
    // only signal; the G8 closure claim required a per-call field
    // and now has one. Wire shape:
    //   - "neural"          — configured + loaded
    //   - "lexical"         — operator chose lexical or never asked
    //                         for a neural cross-encoder
    //   - "degraded_lexical"— configured neural, runtime fell back
    //   - "none"            — no reranker plumbed at all
    let reranker_used = match reranker {
        Some(ce) if ce.is_neural() => "neural",
        Some(ce) if ce.is_degraded_lexical() => "degraded_lexical",
        Some(_) => "lexical",
        None => "none",
    };
    let attach_meta = |resp: &mut Value, recall_mode: &str, telemetry: &RecallTelemetry| {
        // Round blend_weight to 3 decimals — matches the score field
        // precision and keeps the wire shape stable regardless of f64
        // representation jitter.
        let blend_weight = (telemetry.blend_weight_avg * 1000.0).round() / 1000.0;
        let meta = RecallMeta {
            recall_mode: recall_mode.to_string(),
            reranker_used: reranker_used.to_string(),
            candidate_counts: CandidateCounts {
                fts: telemetry.fts_candidates,
                hnsw: telemetry.hnsw_candidates,
            },
            blend_weight,
        };
        // Merge into existing meta object rather than replacing — P6's
        // decorate_budget may have already populated budget_* keys here.
        if let Ok(Value::Object(p3_fields)) = serde_json::to_value(&meta) {
            let meta_obj = resp
                .as_object_mut()
                .expect("recall response is always a JSON object")
                .entry("meta".to_string())
                .or_insert_with(|| json!({}));
            if let Some(existing) = meta_obj.as_object_mut() {
                for (k, v) in p3_fields {
                    existing.insert(k, v);
                }
            }
        }
    };

    // Use hybrid recall if embedder is available
    if let Some(emb) = embedder {
        match emb.embed(context) {
            Ok(primary_emb) => {
                // v0.6.0.0: fuse primary query with context-token embedding
                // at 70/30 when caller supplied conversation tokens.
                let query_emb = if context_tokens.is_empty() {
                    primary_emb
                } else {
                    let joined = context_tokens.join(" ");
                    match emb.embed(&joined) {
                        Ok(ctx_emb) => {
                            crate::embeddings::Embedder::fuse(&primary_emb, &ctx_emb, 0.7)
                        }
                        Err(e) => {
                            tracing::warn!("context_tokens embed failed, using primary only: {e}");
                            primary_emb
                        }
                    }
                };
                let (results, outcome, telemetry) = db::recall_hybrid_with_telemetry(
                    conn,
                    context,
                    &query_emb,
                    namespace,
                    limit.min(50),
                    tags,
                    since,
                    until,
                    vector_index,
                    resolved_ttl.short_extend_secs,
                    resolved_ttl.mid_extend_secs,
                    as_agent,
                    budget_tokens,
                    resolved_scoring,
                )
                .map_err(|e| e.to_string())?;

                // Apply cross-encoder reranking if available
                if let Some(ce) = reranker {
                    let ce_reranked = ce.rerank(context, results);
                    let memories = scored_memories(ce_reranked);
                    let mut resp = json!({"memories": memories, "count": memories.len(), "mode": "hybrid+rerank"});
                    decorate_budget(&mut resp, &outcome);
                    attach_meta(&mut resp, "hybrid", &telemetry);
                    super::inject_namespace_standard(conn, namespace, &mut resp);
                    return Ok(resp);
                }

                let memories = scored_memories(results);
                let mut resp =
                    json!({"memories": memories, "count": memories.len(), "mode": "hybrid"});
                decorate_budget(&mut resp, &outcome);
                attach_meta(&mut resp, "hybrid", &telemetry);
                super::inject_namespace_standard(conn, namespace, &mut resp);
                return Ok(resp);
            }
            Err(e) => {
                // v0.6.3.1 (P3, G11): the embedder being present but the
                // per-query embed failing is a different silent-degrade
                // path than "embedder unavailable at startup" — preserve
                // the existing tracing event and fall through to
                // keyword_only mode below, which is what the meta block
                // will report.
                tracing::warn!("embedding failed, falling back to FTS: {}", e);
            }
        }
    }

    // Fallback to keyword-only recall
    let (results, outcome, telemetry) = db::recall_with_telemetry(
        conn,
        context,
        namespace,
        limit.min(50),
        tags,
        since,
        until,
        resolved_ttl.short_extend_secs,
        resolved_ttl.mid_extend_secs,
        as_agent,
        budget_tokens,
    )
    .map_err(|e| e.to_string())?;
    let memories = scored_memories(results);
    let mut resp = json!({"memories": memories, "count": memories.len(), "mode": "keyword"});
    decorate_budget(&mut resp, &outcome);
    attach_meta(&mut resp, "keyword_only", &telemetry);
    super::inject_namespace_standard(conn, namespace, &mut resp);
    Ok(resp)
}
