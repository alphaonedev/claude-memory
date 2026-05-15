// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_recall` handler and namespace-chain helpers.

use crate::embeddings::Embed;
use crate::hnsw::VectorIndex;
use crate::models::{CandidateCounts, Memory, MemoryKind, RecallMeta, RecallTelemetry};
use crate::reranker::BatchedReranker;
use crate::{db, validate};
use serde_json::{Value, json};

/// v0.7.x Form 6 — parse the `kinds` recall-filter parameter from the
/// MCP params bag. Accepts:
///   * an array of strings: `["concept", "claim"]`
///   * a single comma-separated string: `"concept,claim"`
///   * the literal string `"all"` (any-of-all, equivalent to omission)
/// Returns `None` when the field is absent or syntactically empty so
/// callers treat that as "no kind filter". Unknown tokens are dropped
/// silently — a future variant emitted by a newer client should not
/// break recall on an older binary.
fn parse_kinds_filter(params: &Value) -> Option<Vec<MemoryKind>> {
    let raw = params.get("kinds")?;
    if let Some(s) = raw.as_str() {
        if s.trim().eq_ignore_ascii_case("all") {
            return None;
        }
        return MemoryKind::parse_csv(s);
    }
    if let Some(arr) = raw.as_array() {
        let mut out: Vec<MemoryKind> = Vec::new();
        for v in arr {
            if let Some(name) = v.as_str()
                && let Some(k) = MemoryKind::from_str(name.trim())
                && !out.contains(&k)
            {
                out.push(k);
            }
        }
        if out.is_empty() { None } else { Some(out) }
    } else {
        None
    }
}

/// v0.7.x Form 6 — apply the parsed kinds filter to a recall result
/// set in-place. No-op when `kinds == None`. OR-of-kinds semantics:
/// a memory passes when `kinds.contains(&memory.memory_kind)`.
fn apply_kinds_filter(
    results: Vec<(Memory, f64)>,
    kinds: Option<&[MemoryKind]>,
) -> Vec<(Memory, f64)> {
    match kinds {
        None => results,
        Some(allowed) => results
            .into_iter()
            .filter(|(m, _)| allowed.contains(&m.memory_kind))
            .collect(),
    }
}

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

    // v0.7.x Form 6 — Batman-taxonomy `kinds` filter. Parsed once
    // and applied to every result vector below (keyword, hybrid,
    // hybrid+rerank). OR-of-kinds within the param, AND with the
    // other filters (namespace, tags, time window, visibility).
    let kinds_filter = parse_kinds_filter(params);

    // v0.7.0 WT-1-E — atom-preference recall semantics.
    //
    // By default recall surfaces atoms in place of archived sources
    // (the WT-1-B atomiser sets `atomised_into > 0` AND
    // `metadata.atomisation_archived_at` on the parent row when atoms
    // exist). Auditors and the forensic-export path opt in via
    // `include_archived=true` to see both atoms AND the archived
    // source for the same query — the substrate read is the same;
    // only the WHERE clause changes.
    //
    // Composes with namespace, memory_kind (via storage filter),
    // time-window, tier, and the existing visibility predicate.
    let include_archived = params["include_archived"].as_bool().unwrap_or(false);

    // v0.7.0 Form 4 (issue #757) — fact-provenance post-filters.
    // `has_citations` keeps only memories with a non-empty citations
    // array; `source_uri_prefix` keeps only memories whose
    // `source_uri` column begins with the supplied string. Both
    // compose with the existing SQL-side filters; we run them in
    // Rust after the recall returns so the substrate signature
    // doesn't grow another two positional args. Tool-count baseline
    // preserved (no new MCP tool).
    let has_citations_filter = params["has_citations"].as_bool().unwrap_or(false);
    let source_uri_prefix: Option<String> = params["source_uri_prefix"]
        .as_str()
        .map(std::string::ToString::to_string);

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
                    include_archived,
                )
                .map_err(|e| e.to_string())?;
                let results = crate::cli::recall::apply_form4_recall_filters(
                    results,
                    has_citations_filter,
                    source_uri_prefix.as_deref(),
                );

                // Apply cross-encoder reranking if available
                if let Some(ce) = reranker {
                    let ce_reranked = ce.rerank(context, results);
                    let ce_reranked = apply_kinds_filter(ce_reranked, kinds_filter.as_deref());
                    let memories = scored_memories(ce_reranked);
                    let mut resp = json!({"memories": memories, "count": memories.len(), "mode": "hybrid+rerank"});
                    decorate_budget(&mut resp, &outcome);
                    attach_meta(&mut resp, "hybrid", &telemetry);
                    super::inject_namespace_standard(conn, namespace, &mut resp);
                    return Ok(resp);
                }

                let results = apply_kinds_filter(results, kinds_filter.as_deref());
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
        include_archived,
    )
    .map_err(|e| e.to_string())?;
    let results = crate::cli::recall::apply_form4_recall_filters(
        results,
        has_citations_filter,
        source_uri_prefix.as_deref(),
    );
    let results = apply_kinds_filter(results, kinds_filter.as_deref());
    let memories = scored_memories(results);
    let mut resp = json!({"memories": memories, "count": memories.len(), "mode": "keyword"});
    decorate_budget(&mut resp, &outcome);
    attach_meta(&mut resp, "keyword_only", &telemetry);
    super::inject_namespace_standard(conn, namespace, &mut resp);
    Ok(resp)
}

#[cfg(test)]
mod tests {
    //! L0.7-3 Tier B chunk-A — coverage tests for `handle_recall`
    //! and `handle_recall_with_pre_recall_hook`.
    //!
    //! Six-category template:
    //! A. happy path — keyword + hybrid + reranker
    //! B. validation — missing context
    //! D. state-dependent — empty result, namespace filter miss
    //! Embedder-bound: BOTH None and Some(&dyn Embed) paths.

    use super::*;
    use crate::config::{RecallScope, ResolvedScoring, ResolvedTtl};
    use crate::embeddings::test_support::MockEmbedder;
    use crate::hnsw::VectorIndex;
    use crate::models::{Memory, Tier};
    use crate::reranker::{BatchedReranker, CrossEncoder};
    use crate::storage as db;

    fn fresh_conn() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn make_mem(title: &str, content: &str, ns: &str) -> Memory {
        let now = chrono::Utc::now().to_rfc3339();
        Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: content.to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({"agent_id": "ai:test"}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        }
    }

    fn seed(conn: &rusqlite::Connection) {
        db::insert(
            conn,
            &make_mem(
                "Rust ownership",
                "Rust ownership rules prevent data races",
                "test",
            ),
        )
        .unwrap();
        db::insert(
            conn,
            &make_mem(
                "Python typing",
                "Python typing is dynamic with hints",
                "test",
            ),
        )
        .unwrap();
        db::insert(conn, &make_mem("Other topic", "Unrelated content", "other")).unwrap();
    }

    // B. validation — missing context
    #[test]
    fn missing_context_errors() {
        let conn = fresh_conn();
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let err = handle_recall(
            &conn,
            &json!({}),
            None,
            None,
            None,
            false,
            &ttl,
            &scoring,
            None,
        )
        .unwrap_err();
        assert!(err.contains("context"));
    }

    // A. happy path — keyword-only (embedder=None)
    #[test]
    fn keyword_only_path() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let resp = handle_recall(
            &conn,
            &json!({"context": "ownership", "namespace": "test"}),
            None,
            None,
            None,
            false,
            &ttl,
            &scoring,
            None,
        )
        .expect("ok");
        assert_eq!(resp["mode"].as_str(), Some("keyword"));
        assert_eq!(resp["meta"]["recall_mode"].as_str(), Some("keyword_only"));
    }

    // A. happy path — hybrid (embedder=Some)
    #[test]
    fn hybrid_path_with_embedder() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let mock = MockEmbedder::new_local().expect("mock");
        let resp = handle_recall(
            &conn,
            &json!({"context": "ownership rules", "namespace": "test"}),
            Some(&mock as &dyn crate::embeddings::Embed),
            None,
            None,
            false,
            &ttl,
            &scoring,
            None,
        )
        .expect("ok");
        assert_eq!(resp["mode"].as_str(), Some("hybrid"));
        assert_eq!(resp["meta"]["recall_mode"].as_str(), Some("hybrid"));
    }

    // A. happy path — hybrid + reranker
    #[test]
    fn hybrid_with_reranker_path() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let mock = MockEmbedder::new_local().expect("mock");
        let lex = CrossEncoder::new();
        let batched = BatchedReranker::new(lex);
        let resp = handle_recall(
            &conn,
            &json!({"context": "ownership rules", "namespace": "test"}),
            Some(&mock as &dyn crate::embeddings::Embed),
            None,
            Some(&batched),
            false,
            &ttl,
            &scoring,
            None,
        )
        .expect("ok");
        assert_eq!(resp["mode"].as_str(), Some("hybrid+rerank"));
        assert_eq!(resp["meta"]["reranker_used"].as_str(), Some("lexical"));
    }

    // hybrid with vector_index Some-path
    #[test]
    fn hybrid_with_vector_index() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let mock = MockEmbedder::new_local().expect("mock");
        let idx = VectorIndex::empty();
        let resp = handle_recall(
            &conn,
            &json!({"context": "ownership", "namespace": "test"}),
            Some(&mock as &dyn crate::embeddings::Embed),
            Some(&idx),
            None,
            false,
            &ttl,
            &scoring,
            None,
        )
        .expect("ok");
        assert_eq!(resp["mode"].as_str(), Some("hybrid"));
    }

    // budget_tokens path
    #[test]
    fn budget_tokens_meta_emitted() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let resp = handle_recall(
            &conn,
            &json!({"context": "ownership", "namespace": "test", "budget_tokens": 100u64}),
            None,
            None,
            None,
            false,
            &ttl,
            &scoring,
            None,
        )
        .expect("ok");
        assert!(resp["meta"]["budget_tokens_used"].is_number());
        assert_eq!(resp["budget_tokens"].as_u64(), Some(100));
    }

    // budget_tokens=0 (R1 semantic: allow zero)
    #[test]
    fn budget_tokens_zero_returns_empty() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let resp = handle_recall(
            &conn,
            &json!({"context": "ownership", "namespace": "test", "budget_tokens": 0u64}),
            None,
            None,
            None,
            false,
            &ttl,
            &scoring,
            None,
        )
        .expect("ok");
        assert!(resp["meta"]["budget_overflow"].is_boolean());
    }

    // session_default + recall_scope splice
    #[test]
    fn session_default_recall_scope_splices_defaults() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let scope = RecallScope {
            namespaces: Some(vec!["test".to_string()]),
            since: Some("24h".to_string()),
            tier: None,
            limit: Some(2),
        };
        let resp = handle_recall(
            &conn,
            &json!({"context": "ownership", "session_default": true}),
            None,
            None,
            None,
            false,
            &ttl,
            &scoring,
            Some(&scope),
        )
        .expect("ok");
        // Should match the spliced namespace ("test")
        assert!(resp["count"].as_u64().unwrap() <= 2);
    }

    // context_tokens fusion path (with embedder)
    #[test]
    fn context_tokens_fusion_path() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let mock = MockEmbedder::new_local().expect("mock");
        let resp = handle_recall(
            &conn,
            &json!({
                "context": "ownership",
                "namespace": "test",
                "context_tokens": ["rust", "memory"]
            }),
            Some(&mock as &dyn crate::embeddings::Embed),
            None,
            None,
            false,
            &ttl,
            &scoring,
            None,
        )
        .expect("ok");
        assert_eq!(resp["mode"].as_str(), Some("hybrid"));
    }

    // as_agent path (visibility filter)
    #[test]
    fn as_agent_validated() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let resp = handle_recall(
            &conn,
            &json!({"context": "ownership", "namespace": "test", "as_agent": "ai:viewer"}),
            None,
            None,
            None,
            false,
            &ttl,
            &scoring,
            None,
        )
        .expect("ok");
        assert!(resp["count"].is_number());
    }

    // as_agent invalid
    #[test]
    fn as_agent_invalid_errors() {
        let conn = fresh_conn();
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let err = handle_recall(
            &conn,
            &json!({"context": "ownership", "as_agent": "has space"}),
            None,
            None,
            None,
            false,
            &ttl,
            &scoring,
            None,
        )
        .unwrap_err();
        assert!(!err.is_empty());
    }

    // archive_on_gc=true exercises gc_if_needed branch
    #[test]
    fn archive_on_gc_true_runs_gc() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let resp = handle_recall(
            &conn,
            &json!({"context": "ownership", "namespace": "test"}),
            None,
            None,
            None,
            true,
            &ttl,
            &scoring,
            None,
        )
        .expect("ok");
        assert!(resp["memories"].is_array());
    }

    // until + since explicit filters
    #[test]
    fn since_until_filters_applied() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let resp = handle_recall(
            &conn,
            &json!({
                "context": "ownership",
                "namespace": "test",
                "since": "2000-01-01T00:00:00Z",
                "until": "2100-01-01T00:00:00Z",
                "tags": "rust",
            }),
            None,
            None,
            None,
            false,
            &ttl,
            &scoring,
            None,
        )
        .expect("ok");
        assert!(resp["memories"].is_array());
    }

    // limit huge → saturate
    #[test]
    fn limit_overflow_saturates() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let resp = handle_recall(
            &conn,
            &json!({"context": "ownership", "namespace": "test", "limit": u64::MAX}),
            None,
            None,
            None,
            false,
            &ttl,
            &scoring,
            None,
        )
        .expect("ok");
        assert!(resp["memories"].is_array());
    }

    // Failing embedder — drives the per-query embed-error fallback
    // (lines 357/364) and the context_tokens embed-error fallback
    // (lines 314-316).
    struct FailEmbedder {
        fail_first: bool,
        fail_second: bool,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl FailEmbedder {
        fn primary_fail() -> Self {
            Self {
                fail_first: true,
                fail_second: false,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn secondary_fail() -> Self {
            Self {
                fail_first: false,
                fail_second: true,
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }
    impl crate::embeddings::Embed for FailEmbedder {
        fn embed(&self, _: &str) -> anyhow::Result<Vec<f32>> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if (n == 0 && self.fail_first) || (n >= 1 && self.fail_second) {
                anyhow::bail!("FailEmbedder: synthetic failure on call {n}");
            }
            Ok(vec![0.1_f32; 384])
        }
    }

    #[test]
    fn primary_embedder_error_falls_back_to_keyword() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let fe = FailEmbedder::primary_fail();
        let resp = handle_recall(
            &conn,
            &json!({"context": "ownership", "namespace": "test"}),
            Some(&fe as &dyn crate::embeddings::Embed),
            None,
            None,
            false,
            &ttl,
            &scoring,
            None,
        )
        .expect("ok");
        assert_eq!(resp["mode"].as_str(), Some("keyword"));
        assert_eq!(resp["meta"]["recall_mode"].as_str(), Some("keyword_only"));
    }

    #[test]
    fn context_tokens_embedder_error_uses_primary_only() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let fe = FailEmbedder::secondary_fail();
        let resp = handle_recall(
            &conn,
            &json!({
                "context": "ownership",
                "namespace": "test",
                "context_tokens": ["rust", "memory"]
            }),
            Some(&fe as &dyn crate::embeddings::Embed),
            None,
            None,
            false,
            &ttl,
            &scoring,
            None,
        )
        .expect("ok");
        // hybrid mode still — primary succeeded, context_tokens failed
        assert_eq!(resp["mode"].as_str(), Some("hybrid"));
    }

    // Pre-recall hook variant: empty chain → falls through
    #[tokio::test]
    async fn pre_recall_hook_empty_chain_passes_through() {
        let conn = fresh_conn();
        seed(&conn);
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let chain = crate::hooks::HookChain::new(vec![]);
        let mut registry = crate::hooks::ExecutorRegistry::default();
        let resp = handle_recall_with_pre_recall_hook(
            &conn,
            &json!({"context": "ownership", "namespace": "test"}),
            None,
            None,
            None,
            false,
            &ttl,
            &scoring,
            &chain,
            &mut registry,
            None,
        )
        .await
        .expect("ok");
        assert_eq!(resp["mode"].as_str(), Some("keyword"));
    }

    // Pre-recall hook variant: context missing
    #[tokio::test]
    async fn pre_recall_hook_missing_context_errors() {
        let conn = fresh_conn();
        let ttl = ResolvedTtl::default();
        let scoring = ResolvedScoring::default();
        let chain = crate::hooks::HookChain::new(vec![]);
        let mut registry = crate::hooks::ExecutorRegistry::default();
        let err = handle_recall_with_pre_recall_hook(
            &conn,
            &json!({}),
            None,
            None,
            None,
            false,
            &ttl,
            &scoring,
            &chain,
            &mut registry,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.contains("context"));
    }
}
