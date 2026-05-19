// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Recall HTTP handlers — `/api/v1/recall` GET + POST + the inner
//! response-builder + the request-scope-defaulter helper.
//!
//! Extracted from [`super::http`] under issue #650 follow-up 2. The
//! handler bodies are unchanged; only the module-routing import surface
//! moved. Wire compatibility preserved via `pub use recall::*` in
//! [`super`].

#![allow(clippy::too_many_lines)]

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde_json::json;

use crate::db;
use crate::models::{RecallBody, RecallQuery};
use crate::validate;

use super::AppState;
#[cfg(feature = "sal")]
use super::StorageBackend;
#[cfg(feature = "sal")]
use super::store_err_to_response;

/// v0.7.0 (issue #518) — when `session_default == true` AND the
/// caller omitted a given filter axis, splice in the configured
/// `[agents.defaults.recall_scope]` value. Always returns the
/// (namespace, since, tier, limit) tuple that subsequent handler
/// code uses, regardless of whether the splice fired. The
/// `recall_scope_tier` value is plumbed through to the postgres
/// SAL path (which carries a `Filter.tier`) — sqlite recall does
/// not currently expose a tier filter, so this field is a no-op on
/// the legacy path.
///
/// Resolution: explicit args > recall_scope defaults > compiled
/// defaults.
#[allow(clippy::type_complexity)]
fn apply_recall_scope_defaults(
    app: &AppState,
    session_default: Option<bool>,
    explicit_namespace: Option<String>,
    explicit_since: Option<String>,
    explicit_limit: Option<usize>,
) -> (Option<String>, Option<String>, Option<String>, usize) {
    let want_splice = session_default.unwrap_or(false);
    let scope_opt: Option<&crate::config::RecallScope> = if want_splice {
        app.recall_scope.as_ref().as_ref()
    } else {
        None
    };

    let namespace = explicit_namespace.or_else(|| {
        scope_opt
            .and_then(|s| s.namespaces.as_ref())
            .and_then(|v| v.first())
            .cloned()
    });

    let since = explicit_since.or_else(|| {
        scope_opt.and_then(|s| {
            s.since.as_deref().and_then(|d| {
                crate::config::parse_duration_string(d).map(|dur| {
                    let cutoff = chrono::Utc::now() - dur;
                    cutoff.to_rfc3339()
                })
            })
        })
    });

    let tier = scope_opt.and_then(|s| s.tier.clone());

    let limit_explicit = explicit_limit;
    let resolved_limit = match limit_explicit {
        Some(v) => v,
        None => match scope_opt.and_then(|s| s.limit) {
            Some(v) => v as usize,
            None => 10,
        },
    };
    let resolved_limit = resolved_limit.min(50);

    (namespace, since, tier, resolved_limit)
}

pub async fn recall_memories_get(
    State(app): State<AppState>,
    Query(p): Query<RecallQuery>,
) -> impl IntoResponse {
    // Accept `context` (canonical), `query` (cert harness alias —
    // S79 uses `?query=…`), or `q` (search-style alias — the parity
    // suite uses `?q=…`). Cert oracles continue to work.
    //
    // #869 audit (Category B — safe default): empty `String` collapses
    // straight into the `is_empty()` guard below, which returns a typed
    // 400 with "context (or query) is required".
    let ctx = p
        .context
        .clone()
        .or_else(|| p.query.clone())
        .or_else(|| p.q.clone())
        .unwrap_or_default();
    if ctx.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "context (or query) is required"})),
        )
            .into_response();
    }
    // Phase P6 (R1): `budget_tokens=0` is now a valid request meaning
    // "return zero memories" — see `db::apply_token_budget`. The
    // earlier Ultrareview #348 hard-reject is replaced by always
    // round-tripping the requested budget in the response so a
    // genuinely buggy uninitialised counter is still observable.
    if let Some(ref a) = p.as_agent
        && let Err(e) = validate::validate_namespace(a)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid as_agent: {e}")})),
        )
            .into_response();
    }
    // v0.7.0 (issue #518) — splice `[agents.defaults.recall_scope]`
    // when `session_default=true` AND the caller omitted the
    // matching filter axis. Resolution: explicit args win.
    let (ns_resolved, since_resolved, tier_resolved, limit) = apply_recall_scope_defaults(
        &app,
        p.session_default,
        p.namespace.clone(),
        p.since.clone(),
        p.limit,
    );
    let kinds = p.resolved_kinds();
    recall_response(
        &app,
        &ctx,
        ns_resolved.as_deref(),
        limit,
        p.tags.as_deref(),
        since_resolved.as_deref(),
        p.until.as_deref(),
        p.as_agent.as_deref(),
        p.budget_tokens,
        tier_resolved.as_deref(),
        p.has_citations.unwrap_or(false),
        p.source_uri_prefix.as_deref(),
        kinds.as_deref(),
        p.session_id.as_deref(),
    )
    .await
}

pub async fn recall_memories_post(
    State(app): State<AppState>,
    Json(body): Json<RecallBody>,
) -> impl IntoResponse {
    // Accept either `context` (canonical) or `query` (cert harness
    // alias used by S79). Reject only when both are missing/empty.
    let ctx_val = body.resolved_query();
    if ctx_val.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "context (or query) is required"})),
        )
            .into_response();
    }
    // Phase P6 (R1): `budget_tokens=0` is now a valid request — see
    // the matching note on the GET handler above.
    if let Some(ref a) = body.as_agent
        && let Err(e) = validate::validate_namespace(a)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid as_agent: {e}")})),
        )
            .into_response();
    }
    // v0.7.0 (issue #518) — see GET handler for the resolution rule.
    let (ns_resolved, since_resolved, tier_resolved, limit) = apply_recall_scope_defaults(
        &app,
        body.session_default,
        body.namespace.clone(),
        body.since.clone(),
        body.limit,
    );
    let kinds = body.resolved_kinds();
    recall_response(
        &app,
        &ctx_val,
        ns_resolved.as_deref(),
        limit,
        body.tags.as_deref(),
        since_resolved.as_deref(),
        body.until.as_deref(),
        body.as_agent.as_deref(),
        body.budget_tokens,
        tier_resolved.as_deref(),
        body.has_citations.unwrap_or(false),
        body.source_uri_prefix.as_deref(),
        kinds.as_deref(),
        body.session_id.as_deref(),
    )
    .await
}

/// v0.6.2 (S18): shared HTTP recall implementation. Uses `db::recall_hybrid`
/// (semantic + FTS adaptive blend) when the embedder is loaded — matching
/// how the MCP `memory_recall` handler wires recall at src/mcp.rs:1157.
/// Gracefully falls back to `db::recall` (keyword-only) when the embedder
/// is not present or embedding the query fails. Closes the gap where the
/// HTTP surface was keyword-only regardless of server tier — scenario-18
/// surfaced the black-hole on peers that fanned out memories but never
/// exercised the semantic recall path.
///
/// v0.7.0 Wave-3 Continuation — when `app.storage_backend` is
/// `Postgres`, dispatch through `app.store.search` for keyword recall.
/// The full hybrid (FTS + semantic + adaptive blend + reranker + touch
/// ops) pipeline remains sqlite-only in v0.7.0; postgres deployments
/// fall back to keyword-only recall through the postgres `to_tsvector`
/// FTS surface, which is functionally equivalent for the keyword half
/// and surfaces a `mode=keyword` envelope so clients can detect the
/// degraded mode without an out-of-band feature probe.
#[allow(clippy::too_many_arguments)]
async fn recall_response(
    app: &AppState,
    context: &str,
    namespace: Option<&str>,
    limit: usize,
    tags: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
    as_agent: Option<&str>,
    budget_tokens: Option<usize>,
    // v0.7.0 (issue #518) — spliced
    // `[agents.defaults.recall_scope].tier` when the caller passed
    // `session_default=true`. Applied on the postgres SAL path
    // (`Filter.tier`); ignored on the sqlite path because the legacy
    // `db::recall` / `db::recall_hybrid` functions do not expose a
    // tier filter parameter.
    recall_scope_tier: Option<&str>,
    // v0.7.0 Form 4 (issue #757) — fact-provenance post-filters.
    // Applied in Rust after the substrate-level recall returns so
    // the existing `db::recall` / `db::recall_hybrid` signatures
    // stay stable. Composes with every other filter.
    has_citations: bool,
    source_uri_prefix: Option<&str>,
    // v0.7.x Form 6 (issue #759) — Batman-taxonomy memory-kind
    // filter. Applied post-fetch on both the sqlite and postgres
    // branches. `None` preserves the pre-Form-6 "no kind filter"
    // semantics.
    kinds_filter: Option<&[crate::models::MemoryKind]>,
    // v0.7.0 (issue #518) — per-session recently-accessed boost.
    // When `Some(non-empty)`, the rerank post-step adds +0.05 to any
    // recall candidate already in this session's ring (cap 50 ids,
    // FIFO eviction) and appends the post-boost hit set back into the
    // ring so subsequent recalls in the same session reuse the new
    // context. `None`/empty preserves pre-#518 recall semantics.
    session_id: Option<&str>,
) -> axum::response::Response {
    let session_tracker = crate::reranker::global_session_recall_tracker();
    // `recall_scope_tier` is consumed only on the postgres SAL branch
    // (line 3026). Suppress the unused-variable lint when the sal
    // feature is off — same idiom as `url_was_synthesized` in
    // hook_subscribers.rs.
    #[cfg(not(feature = "sal"))]
    let _ = recall_scope_tier;
    // v0.7.0 Wave-3 Continuation 2 (Phase 10) — postgres-backed
    // hybrid recall via the SAL trait. Embeds the query AND dispatches
    // through `app.store.recall_hybrid` so the postgres adapter applies
    // the FTS + semantic + adaptive blend pipeline (mirror of
    // db::recall_hybrid in sqlite). Touch ops fire after the response
    // payload is assembled so access_count + TTL extension + auto-
    // promotion + priority ladders apply on postgres exactly as on
    // sqlite.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        // Embed the query before issuing the trait call. None when the
        // embedder is unavailable; the trait's recall_hybrid degrades
        // to the FTS-only pool with a synthetic semantic component.
        let query_emb: Option<Vec<f32>> = if let Some(emb) = app.embedder.as_ref().as_ref() {
            match emb.embed(context) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!("recall (postgres): embed failed, keyword-only: {e}");
                    None
                }
            }
        } else {
            None
        };
        let mode = if query_emb.is_some() {
            "hybrid"
        } else {
            "keyword"
        };

        let ctx_caller =
            crate::store::CallerContext::for_agent(as_agent.unwrap_or("daemon").to_string());
        let mut filter = crate::store::Filter {
            namespace: namespace.map(str::to_string),
            limit,
            ..Default::default()
        };
        // v0.7.0 (issue #518) — splice `recall_scope.tier` when the
        // caller passed `session_default=true` and omitted an
        // explicit tier filter on the request. The HTTP recall
        // surface today carries no `tier` query parameter, so an
        // explicit-vs-default conflict cannot arise yet — the splice
        // is unconditional when present.
        if let Some(t) = recall_scope_tier
            && let Some(parsed) = crate::models::Tier::from_str(t)
        {
            filter.tier = Some(parsed);
        }
        if let Some(t) = tags {
            filter.tags_any = t
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
        }
        if let Some(s) = since
            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s)
        {
            filter.since = Some(dt.into());
        }
        if let Some(u) = until
            && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(u)
        {
            filter.until = Some(dt.into());
        }
        return match app
            .store
            .recall_hybrid(&ctx_caller, context, query_emb.as_deref(), &filter)
            .await
        {
            Ok(scored_pairs) => {
                // v0.7.0 Form 4 (issue #757) — fact-provenance post-filter
                // applies on the postgres SAL path too. Touch ops fire on
                // the FILTERED set so a memory the caller filtered out by
                // provenance does not leak through to the access_count
                // ladder.
                let scored_pairs = crate::cli::recall::apply_form4_recall_filters(
                    scored_pairs,
                    has_citations,
                    source_uri_prefix,
                );
                // v0.7.x Form 6 — apply post-fetch kinds filter on the
                // postgres SAL branch. OR-of-kinds within the param.
                let scored_pairs: Vec<_> = match kinds_filter {
                    None => scored_pairs,
                    Some(allowed) => scored_pairs
                        .into_iter()
                        .filter(|(m, _)| allowed.contains(&m.memory_kind))
                        .collect(),
                };
                // v0.7.0 (issue #518) — per-session recency boost +
                // post-recall record. No-op when `session_id` is None
                // or empty.
                let scored_pairs = crate::reranker::apply_session_recency_boost(
                    scored_pairs,
                    session_id,
                    session_tracker,
                );
                let touch_ids: Vec<String> =
                    scored_pairs.iter().map(|(m, _)| m.id.clone()).collect();
                // #869 — `serde_json::to_value(m).unwrap_or_default()`
                // would have surfaced a `Value::Null` row in the recall
                // payload on a Memory-serialise failure, which the
                // client would parse as a real memory with every field
                // null. `filter_map` + log preserves the rest of the
                // batch and lets operators investigate the bad row.
                let scored: Vec<serde_json::Value> = scored_pairs
                    .iter()
                    .filter_map(|(m, s)| match serde_json::to_value(m) {
                        Ok(mut v) => {
                            if let Some(obj) = v.as_object_mut() {
                                obj.insert(
                                    "score".to_string(),
                                    json!((*s * 1000.0).round() / 1000.0),
                                );
                            }
                            Some(v)
                        }
                        Err(e) => {
                            tracing::error!(
                                memory_id = %m.id,
                                "recall (postgres): serialise Memory failed, skipping row: {e}"
                            );
                            None
                        }
                    })
                    .collect();
                // Touch ops AFTER assembling the response payload so the
                // observable response is what the caller wanted (access_count
                // pre-touch); the touch fires inside the trait call's own
                // transaction.
                if let Err(e) = app.store.touch_after_recall(&touch_ids).await {
                    tracing::warn!("recall (postgres): touch_after_recall failed: {e}");
                }
                let mut resp = json!({
                    "memories": scored,
                    "count": scored.len(),
                    "tokens_used": 0,
                    "mode": mode,
                    "storage_backend": "postgres",
                });
                if let Some(b) = budget_tokens {
                    resp["budget_tokens"] = json!(b);
                }
                Json(resp).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    // Embed the query BEFORE grabbing the DB lock — embed() is CPU-heavy
    // and holding the SQLite mutex across it serialises unrelated writes.
    let query_emb: Option<Vec<f32>> = if let Some(emb) = app.embedder.as_ref().as_ref() {
        match emb.embed(context) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("recall: embedder query failed, falling back to keyword-only: {e}");
                None
            }
        }
    } else {
        None
    };

    let lock = app.db.lock().await;
    let short_extend = lock.2.short_extend_secs;
    let mid_extend = lock.2.mid_extend_secs;

    let (result, mode) = if let Some(ref qe) = query_emb {
        let vi_guard = app.vector_index.lock().await;
        let vi_ref = vi_guard.as_ref();
        let r = db::recall_hybrid(
            &lock.0,
            context,
            qe,
            namespace,
            limit,
            tags,
            since,
            until,
            vi_ref,
            short_extend,
            mid_extend,
            as_agent,
            budget_tokens,
            app.scoring.as_ref(),
            false,
            // v0.7.0 Cluster-A PERF-3 — push the prefix into SQL on
            // both FTS and semantic branches so the partial
            // idx_memories_source_uri index covers the lookup; the
            // post-fetch apply_form4_recall_filters below remains for
            // the `has_citations` axis.
            source_uri_prefix,
        );
        drop(vi_guard);
        (r, "hybrid")
    } else {
        let r = db::recall(
            &lock.0,
            context,
            namespace,
            limit,
            tags,
            since,
            until,
            short_extend,
            mid_extend,
            as_agent,
            budget_tokens,
            false,
            // v0.7.0 Cluster-A PERF-3 — see hybrid branch above.
            source_uri_prefix,
        );
        (r, "keyword")
    };

    match result {
        Ok((r, outcome)) => {
            // v0.7.0 Form 4 (issue #757) — fact-provenance post-filter.
            let r =
                crate::cli::recall::apply_form4_recall_filters(r, has_citations, source_uri_prefix);
            // v0.7.x Form 6 — apply post-fetch kinds filter on the
            // sqlite branch. Cheap because recall already capped
            // r.len() at limit.min(50).
            let r: Vec<_> = match kinds_filter {
                None => r,
                Some(allowed) => r
                    .into_iter()
                    .filter(|(m, _)| allowed.contains(&m.memory_kind))
                    .collect(),
            };
            // v0.7.0 (issue #518) — per-session recency boost +
            // post-recall record on the sqlite branch.
            let r = crate::reranker::apply_session_recency_boost(r, session_id, session_tracker);
            // #869 — same `Value::Null` masking fix as the postgres
            // branch above; sqlite branch needs the identical
            // filter_map + log so an encoder regression cannot silently
            // drop fields from a recall row to look like a real null.
            let scored: Vec<serde_json::Value> = r
                .iter()
                .filter_map(|(m, s)| match serde_json::to_value(m) {
                    Ok(mut v) => {
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert("score".to_string(), json!((*s * 1000.0).round() / 1000.0));
                        }
                        Some(v)
                    }
                    Err(e) => {
                        tracing::error!(
                            memory_id = %m.id,
                            "recall (sqlite): serialise Memory failed, skipping row: {e}"
                        );
                        None
                    }
                })
                .collect();
            let mut resp = json!({
                "memories": scored,
                "count": scored.len(),
                "tokens_used": outcome.tokens_used,
                "mode": mode,
            });
            if let Some(b) = budget_tokens {
                resp["budget_tokens"] = json!(b);
                // Phase P6 (R1) meta block — same shape as the MCP path.
                resp["meta"] = json!({
                    "budget_tokens_used": outcome.tokens_used,
                    "budget_tokens_remaining": outcome.tokens_remaining.unwrap_or(0),
                    "memories_dropped": outcome.memories_dropped,
                    "budget_overflow": outcome.budget_overflow,
                });
            }
            Json(resp).into_response()
        }
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}
