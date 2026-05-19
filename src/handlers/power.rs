// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! "Power" HTTP handlers — LLM-backed and computation-heavy endpoints:
//! contradiction detection, taxonomy, check-duplicate, consolidate,
//! auto-tag, expand-query, namespace listing, and family loader.
//!
//! Extracted from [`super::http`] under issue #650 follow-up 2. The
//! handler bodies are unchanged; only the module-routing import surface
//! moved. Wire compatibility preserved via `pub use power::*` in
//! [`super`].

#![allow(clippy::too_many_lines)]

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::json;

use crate::db;
use crate::models::Memory;
use crate::validate;

use super::AppState;
use super::MAX_BULK_SIZE;
#[cfg(feature = "sal")]
use super::StorageBackend;
#[cfg(feature = "sal")]
use super::store_err_to_response;

#[derive(Deserialize)]
pub struct ContradictionsQuery {
    /// Topic to group candidate memories by. Resolved via (in order):
    /// `metadata.topic` exact match, then `title` exact match, then FTS
    /// content substring. At least one of `topic` or `namespace` is required.
    pub topic: Option<String>,
    /// Namespace to scope the search. Optional — default is cross-namespace.
    pub namespace: Option<String>,
    /// Pagination cap. Defaults to 50, hard max 200.
    pub limit: Option<usize>,
}

/// HTTP handler for v0.6.0.1 issue #321 — surfaces contradiction candidates
/// over the same REST surface scenarios use, so a2a-gate scenario-6 and any
/// future federation-level contradiction probe don't have to go through the
/// MCP stdio path.
///
/// Returns `{memories, links}` where:
/// - `memories` are the candidates grouped by topic/title (respecting the
///   UPSERT (title, namespace) invariant: if writers collided, only the LWW
///   survivor is returned — callers should use distinct titles per writer).
/// - `links` includes any existing `contradicts` rows from the `memory_links`
///   table PLUS a heuristic synthesis: when ≥2 candidates share a topic/title
///   but have materially different content, emit a synthetic `contradicts`
///   relation between each pair. The synthesized links carry
///   `relation:"contradicts"` and a `synthesized:true` flag so callers can
///   distinguish them from LLM-detected or operator-authored links.
///
/// Heuristic-only intentionally — LLM-backed detection (the existing MCP
/// `memory_detect_contradiction` tool) stays MCP-scoped so the HTTP surface
/// has no runtime LLM dependency. A follow-up issue can add opt-in LLM
/// resolution when `config.tier == Smart | Autonomous`.
#[allow(clippy::too_many_lines)]
pub async fn detect_contradictions(
    State(app): State<AppState>,
    Query(q): Query<ContradictionsQuery>,
) -> impl IntoResponse {
    if q.topic.is_none() && q.namespace.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "at least one of `topic` or `namespace` is required"})),
        )
            .into_response();
    }
    if let Some(ref ns) = q.namespace
        && let Err(e) = validate::validate_namespace(ns)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    // v0.6.2 (S40): raise to `MAX_BULK_SIZE` so a detect-contradictions
    // sweep over a bulk-populated namespace isn't silently capped at 200.
    let limit = q.limit.unwrap_or(50).min(MAX_BULK_SIZE);

    // v0.7.0 Wave-3 Continuation 3 (Phase 15) — postgres-backed daemons
    // route through the SAL trait. The non-LLM (rule-based +
    // heuristic-pairwise) contradictions detector works on both backends
    // because it's purely metadata-driven; this branch lists candidates
    // through `app.store.list` then runs the same pairwise heuristic.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("http");
        let filter = crate::store::Filter {
            namespace: q.namespace.clone(),
            limit,
            ..Default::default()
        };
        let all = match app.store.list(&ctx, &filter).await {
            Ok(v) => v,
            Err(e) => return store_err_to_response(e),
        };
        let candidates: Vec<Memory> = match q.topic.as_deref() {
            Some(t) => all
                .into_iter()
                .filter(|m| {
                    m.metadata
                        .get("topic")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| s == t)
                        || m.title == t
                })
                .collect(),
            None => all,
        };
        // Existing contradicts links via SAL — list all then filter by
        // (source ∈ candidates ∧ target ∈ candidates ∧ relation contains
        // "contradict"). We could narrow `list_links` by namespace when
        // q.namespace is set; for cross-namespace topic queries we need
        // the full set anyway.
        let candidate_ids: std::collections::HashSet<String> =
            candidates.iter().map(|m| m.id.clone()).collect();
        let mut existing_links: Vec<serde_json::Value> = Vec::new();
        if let Ok(all_links) = app.store.list_links(q.namespace.as_deref()).await {
            for link in all_links {
                // v0.7.0 fix campaign R1-M4 — relation is now typed.
                // Historic substring match tightened to a precise
                // variant compare.
                if matches!(
                    link.relation,
                    crate::models::MemoryLinkRelation::Contradicts
                ) && candidate_ids.contains(&link.source_id)
                    && candidate_ids.contains(&link.target_id)
                {
                    existing_links.push(json!({
                        "source_id": link.source_id,
                        "target_id": link.target_id,
                        "relation": link.relation,
                        "synthesized": false,
                    }));
                }
            }
        }
        existing_links.sort_by_key(|v| {
            (
                v.get("source_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("target_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("relation")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
            )
        });
        existing_links.dedup_by_key(|v| {
            (
                v.get("source_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("target_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("relation")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
            )
        });
        let mut synth_links: Vec<serde_json::Value> = Vec::new();
        for (i, a) in candidates.iter().enumerate() {
            for b in candidates.iter().skip(i + 1) {
                let same_topic = match q.topic.as_deref() {
                    Some(_) => true,
                    None => a.title == b.title,
                };
                if same_topic && a.content != b.content && a.id != b.id {
                    synth_links.push(json!({
                        "source_id": a.id,
                        "target_id": b.id,
                        "relation": "contradicts",
                        "synthesized": true,
                    }));
                }
            }
        }
        let mut links = existing_links;
        links.extend(synth_links);
        return Json(json!({
            "memories": candidates,
            "links": links,
            "storage_backend": "postgres",
        }))
        .into_response();
    }

    let lock = app.db.lock().await;
    let all = match db::list(
        &lock.0,
        q.namespace.as_deref(),
        None,
        limit,
        0,
        None,
        None,
        None,
        None,
        None,
    ) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("detect_contradictions list error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };

    // Topic match: metadata.topic == topic OR title == topic. Kept as a
    // retained filter rather than pushing to SQL because metadata is JSON
    // and the match predicate may evolve.
    let candidates: Vec<Memory> = match q.topic.as_deref() {
        Some(t) => all
            .into_iter()
            .filter(|m| {
                m.metadata
                    .get("topic")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s == t)
                    || m.title == t
            })
            .collect(),
        None => all,
    };

    // Existing contradicts links involving any candidate.
    let candidate_ids: std::collections::HashSet<String> =
        candidates.iter().map(|m| m.id.clone()).collect();
    let mut existing_links: Vec<serde_json::Value> = Vec::new();
    for id in &candidate_ids {
        if let Ok(links) = db::get_links(&lock.0, id) {
            for link in links {
                // v0.7.0 fix campaign R1-M4 — relation is now typed.
                // The historic substring match on "contradict" is
                // tightened to a precise variant compare.
                if matches!(
                    link.relation,
                    crate::models::MemoryLinkRelation::Contradicts
                ) && candidate_ids.contains(&link.source_id)
                    && candidate_ids.contains(&link.target_id)
                {
                    existing_links.push(json!({
                        "source_id": link.source_id,
                        "target_id": link.target_id,
                        "relation": link.relation,
                        "synthesized": false,
                    }));
                }
            }
        }
    }
    // Dedup — each (source,target,relation) appears at most once.
    existing_links.sort_by_key(|v| {
        (
            v.get("source_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            v.get("target_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            v.get("relation")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
        )
    });
    existing_links.dedup_by_key(|v| {
        (
            v.get("source_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            v.get("target_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
            v.get("relation")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string(),
        )
    });

    // Heuristic: when ≥2 candidates share a topic/title but content
    // differs, synthesize pairwise contradicts links. Marked
    // synthesized:true so callers can treat operator-authored links as
    // higher-confidence than this fallback.
    let mut synth_links: Vec<serde_json::Value> = Vec::new();
    for (i, a) in candidates.iter().enumerate() {
        for b in candidates.iter().skip(i + 1) {
            let same_topic = match q.topic.as_deref() {
                Some(_) => true,
                None => a.title == b.title,
            };
            if same_topic && a.content != b.content && a.id != b.id {
                synth_links.push(json!({
                    "source_id": a.id,
                    "target_id": b.id,
                    "relation": "contradicts",
                    "synthesized": true,
                }));
            }
        }
    }

    let mut links = existing_links;
    links.extend(synth_links);

    Json(json!({
        "memories": candidates,
        "links": links,
    }))
    .into_response()
}

pub async fn list_namespaces(State(app): State<AppState>) -> impl IntoResponse {
    // v0.7.0 Wave-3 Continuation — postgres-backed daemons aggregate the
    // distinct namespaces from `memories` via the SAL `list` method.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("daemon");
        let filter = crate::store::Filter {
            limit: 1_000_000,
            ..Default::default()
        };
        return match app.store.list(&ctx, &filter).await {
            Ok(memories) => {
                let mut ns: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
                for m in memories {
                    ns.insert(m.namespace);
                }
                let v: Vec<String> = ns.into_iter().collect();
                Json(json!({"namespaces": v})).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::list_namespaces(&lock.0) {
        Ok(ns) => Json(json!({"namespaces": ns})).into_response(),
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

/// Query parameters for `GET /api/v1/taxonomy` (Pillar 1 / Stream A).
#[derive(Debug, Deserialize)]
pub struct TaxonomyQuery {
    /// Restrict to memories at this namespace OR any descendant. Trailing
    /// `/` is tolerated. Omit to walk the whole tree.
    pub prefix: Option<String>,
    /// Alias for `prefix` — the cert harness (S44) uses `?root=…`. Both
    /// forms route to the same code path; `prefix` wins when both are
    /// supplied.
    #[serde(default)]
    pub root: Option<String>,
    /// Max levels to descend below the prefix (defaults to 8 — the
    /// hierarchy hard cap).
    pub depth: Option<usize>,
    /// Cap on the number of `(namespace, count)` rows we walk into the
    /// tree. Densest namespaces win when truncated. Defaults to 1000.
    pub limit: Option<usize>,
}

/// `GET /api/v1/taxonomy` — REST mirror of the MCP `memory_get_taxonomy`
/// tool. Returns the prefix's hierarchical tree with per-node and
/// subtree counts, plus an honest `total_count` and a `truncated`
/// flag when `limit` dropped rows from the walk.
pub async fn get_taxonomy(
    State(app): State<AppState>,
    Query(p): Query<TaxonomyQuery>,
) -> impl IntoResponse {
    let prefix_owned: Option<String> = p
        .prefix
        .as_deref()
        .or(p.root.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_string());
    if let Some(pref) = prefix_owned.as_deref()
        && let Err(e) = validate::validate_namespace(pref)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid namespace_prefix: {e}")})),
        )
            .into_response();
    }
    let depth = p
        .depth
        .unwrap_or(crate::models::MAX_NAMESPACE_DEPTH)
        .min(crate::models::MAX_NAMESPACE_DEPTH);
    let limit = p.limit.unwrap_or(1000).clamp(1, 10_000);

    // v0.7.0 Wave-3 Continuation 4 (Bucket E / S44) — full hierarchical
    // taxonomy walk for postgres-backed daemons. Uses
    // `taxonomy_namespaces_via_store` to project a single `GROUP BY
    // namespace` aggregate (so we don't pull every memory row into
    // memory), then assembles the hierarchical tree with honest
    // `subtree_count` so the cert oracle can detect dishonest
    // truncation.
    #[cfg(feature = "sal-postgres")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let pairs = match crate::store::postgres::taxonomy_namespaces_via_store(
            &app.store,
            prefix_owned.as_deref(),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => return store_err_to_response(e),
        };
        // Collapse the SQL-aggregated `(namespace, count)` rows into a
        // hierarchical tree whose nodes carry both their direct
        // `count` (memories whose namespace exactly matches this node)
        // and the transitive `subtree_count` (sum across the node and
        // all descendants).
        let total_count: usize = pairs
            .iter()
            .map(|(_, c)| usize::try_from(*c).unwrap_or(0))
            .sum();

        // Node:
        //   key = full namespace path
        //   own_count = memories at this exact namespace
        //   subtree_count = own_count + sum over descendant subtree_counts
        // Build by ensuring every ancestor node exists (own_count = 0
        // for synthesised intermediates), then accumulating subtree
        // counts bottom-up via stable iteration.
        let mut nodes: std::collections::BTreeMap<String, (usize /* own */, usize /* subtree */)> =
            std::collections::BTreeMap::new();
        for (ns, cnt) in &pairs {
            let cnt_us = usize::try_from(*cnt).unwrap_or(0);
            // Ensure each prefix-segment ancestor exists (above prefix_owned
            // if any). For example, namespace `a/b/c/d` under prefix `a/b`
            // creates nodes for `a/b/c` and `a/b/c/d`.
            let segments: Vec<&str> = ns.split('/').collect();
            for i in 1..=segments.len() {
                let path = segments[..i].join("/");
                nodes.entry(path).or_insert((0, 0));
            }
            // Stamp own_count on the leaf node.
            nodes
                .entry(ns.clone())
                .and_modify(|v| v.0 = cnt_us)
                .or_insert((cnt_us, 0));
        }
        // Compute subtree_count: walk paths longest-first so children
        // are summed before their parents. Since BTreeMap orders by
        // string, walk in reverse-sorted order.
        // First pass: seed each node's subtree_count = own_count.
        for (_k, v) in nodes.iter_mut() {
            v.1 = v.0;
        }
        // Second pass: collect parent->child pairs, then accumulate.
        let keys: Vec<String> = nodes.keys().cloned().collect();
        for k in keys.iter().rev() {
            // Find immediate parent by trimming trailing `/segment`.
            if let Some(pos) = k.rfind('/') {
                let parent = &k[..pos];
                if let Some(parent_node) = nodes.get(parent).copied() {
                    let child_subtree = nodes.get(k).map(|v| v.1).unwrap_or(0);
                    if let Some(p) = nodes.get_mut(parent) {
                        p.1 = parent_node.1 + child_subtree;
                    }
                }
            }
        }

        // Project the prefix-rooted tree at the requested depth. When
        // no prefix is supplied, treat the synthesized "" root as the
        // top of the world; otherwise root the tree at prefix_owned.
        // #869 audit (Category B — safe default): empty root is the
        // documented "no prefix" sentinel for the tree projection.
        let root_ns = prefix_owned.clone().unwrap_or_default();
        let truncated = pairs.len() > limit;

        // Recursive node builder. `current_depth` counts levels below
        // root_ns (root_ns is depth 0). We bound the recursion by
        // `depth` to mirror the v0.6.3 SQLite contract.
        fn build_node(
            node_ns: &str,
            nodes: &std::collections::BTreeMap<String, (usize, usize)>,
            depth_left: usize,
        ) -> serde_json::Value {
            let (own, subtree) = nodes.get(node_ns).copied().unwrap_or((0, 0));
            let mut children: Vec<serde_json::Value> = Vec::new();
            if depth_left > 0 {
                // A child is any node whose namespace starts with
                // `<node_ns>/` AND has exactly one extra segment.
                let prefix_match = if node_ns.is_empty() {
                    String::new()
                } else {
                    format!("{node_ns}/")
                };
                let parent_segs = if node_ns.is_empty() {
                    0
                } else {
                    node_ns.split('/').count()
                };
                for k in nodes.keys() {
                    if k == node_ns {
                        continue;
                    }
                    if !node_ns.is_empty() && !k.starts_with(&prefix_match) {
                        continue;
                    }
                    if k.split('/').count() == parent_segs + 1 {
                        children.push(build_node(k, nodes, depth_left - 1));
                    }
                }
            }
            serde_json::json!({
                "namespace": node_ns,
                "count": own,
                "subtree_count": subtree,
                "children": children,
            })
        }
        let root_node = build_node(&root_ns, &nodes, depth);
        return Json(json!({
            "tree": root_node,
            "total_count": total_count,
            "truncated": truncated,
            "storage_backend": "postgres",
        }))
        .into_response();
    }

    // Suppress unused-warning when sal feature is enabled (prefix_owned moves above).
    let _ = depth;

    let lock = app.db.lock().await;
    match db::get_taxonomy(&lock.0, prefix_owned.as_deref(), depth, limit) {
        Ok(tax) => Json(json!({
            "tree": tax.tree,
            "total_count": tax.total_count,
            "truncated": tax.truncated,
        }))
        .into_response(),
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

/// Request body for `POST /api/v1/check_duplicate` (Pillar 2 / Stream D).
#[derive(Debug, Deserialize)]
pub struct CheckDuplicateBody {
    pub title: String,
    pub content: String,
    /// Restrict the duplicate scan to this namespace. Omit to scan all
    /// namespaces.
    pub namespace: Option<String>,
    /// Cosine similarity threshold for declaring a duplicate. Clamped
    /// to >= 0.5 inside `db::check_duplicate`. Defaults to the tuned
    /// `DUPLICATE_THRESHOLD_DEFAULT` when omitted.
    pub threshold: Option<f32>,
}

/// `POST /api/v1/check_duplicate` — REST mirror of the MCP
/// `memory_check_duplicate` tool. Embeds `title + content`, scans
/// embedded live memories, and returns the highest-cosine match plus
/// `is_duplicate`/`suggested_merge` derived from the (clamped)
/// threshold.
pub async fn check_duplicate(
    State(app): State<AppState>,
    Json(body): Json<CheckDuplicateBody>,
) -> impl IntoResponse {
    if let Err(e) = validate::validate_title(&body.title) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid title: {e}")})),
        )
            .into_response();
    }
    if let Err(e) = validate::validate_content(&body.content) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid content: {e}")})),
        )
            .into_response();
    }
    let namespace = body
        .namespace
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(ns) = namespace
        && let Err(e) = validate::validate_namespace(ns)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid namespace: {e}")})),
        )
            .into_response();
    }
    let threshold = body.threshold.unwrap_or(db::DUPLICATE_THRESHOLD_DEFAULT);

    // v0.7.0 Wave-3 Continuation 4 (Bucket E / S48) — postgres-backed
    // daemons now perform an exact-content sweep through the SAL
    // `list` projection. When an embedder is loaded the call also
    // computes the query embedding and hands it to
    // `recall_hybrid`; the highest-cosine match becomes the nearest
    // candidate. Without an embedder the fallback walks the
    // namespace via `list` and surfaces any row whose
    // `(title, content)` tuple matches exactly (the same content-hash
    // short-circuit `db::check_duplicate_with_text` uses on sqlite,
    // before the embedding pass).
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let ctx = crate::store::CallerContext::for_agent("daemon");
        let filter = crate::store::Filter {
            namespace: namespace.map(str::to_string),
            limit: 1000,
            ..Default::default()
        };
        let mut nearest: Option<(crate::models::Memory, f64)> = None;
        let mut scanned = 0_u64;
        // Exact-content sweep first — cheap, deterministic, no embed.
        match app.store.list(&ctx, &filter).await {
            Ok(rows) => {
                for m in rows {
                    scanned += 1;
                    if m.content == body.content && m.title == body.title {
                        nearest = Some((m, 1.0));
                        break;
                    }
                }
            }
            Err(e) => return store_err_to_response(e),
        }
        // If exact match didn't surface, optionally try embedding-based
        // hybrid recall with the title+content as the query.
        if nearest.is_none()
            && let Some(emb) = app.embedder.as_ref().as_ref()
        {
            let embedding_text = format!("{} {}", body.title, body.content);
            if let Ok(qe) = emb.embed(&embedding_text) {
                let recall_filter = crate::store::Filter {
                    namespace: namespace.map(str::to_string),
                    limit: 5,
                    ..Default::default()
                };
                if let Ok(scored_pairs) = app
                    .store
                    .recall_hybrid(&ctx, &embedding_text, Some(&qe), &recall_filter)
                    .await
                {
                    if let Some((m, s)) = scored_pairs.into_iter().next() {
                        nearest = Some((m, s));
                    }
                }
                drop(qe);
            }
        }
        let (is_duplicate, near_json) = if let Some((m, score)) = nearest {
            let is_dup = score >= f64::from(threshold);
            (
                is_dup,
                json!({
                    "id": m.id,
                    "title": m.title,
                    "namespace": m.namespace,
                    "score": score,
                }),
            )
        } else {
            (false, serde_json::Value::Null)
        };
        return Json(json!({
            "is_duplicate": is_duplicate,
            "threshold": threshold,
            "nearest": near_json,
            "suggested_merge": is_duplicate,
            "candidates_scanned": scanned,
            "storage_backend": "postgres",
        }))
        .into_response();
    }

    // Embed before taking the DB lock — same rationale as create_memory
    // (issue #219). The embedder call is 10-200ms; we don't want it
    // serialised behind the connection mutex.
    let embedding_text = format!("{} {}", body.title, body.content);
    let query_embedding = match app.embedder.as_ref().as_ref() {
        Some(emb) => match emb.embed(&embedding_text) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("embedding generation failed: {e}");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"error": "embedder failed to encode input"})),
                )
                    .into_response();
            }
        },
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": "memory_check_duplicate requires the embedder; daemon must be started with semantic tier or above"
                })),
            )
                .into_response();
        }
    };

    let lock = app.db.lock().await;
    // Round-2 F18 — short-circuit on raw-content hash equality before
    // falling through to embedding cosine similarity (parity with MCP
    // path).
    let check = match db::check_duplicate_with_text(
        &lock.0,
        &query_embedding,
        &embedding_text,
        namespace,
        threshold,
    ) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("handler error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };

    let nearest_json = check.nearest.as_ref().map(|m| {
        json!({
            "id": m.id,
            "title": m.title,
            "namespace": m.namespace,
            "similarity": (m.similarity * 1000.0).round() / 1000.0,
        })
    });
    let suggested_merge = if check.is_duplicate {
        check.nearest.as_ref().map(|m| m.id.clone())
    } else {
        None
    };

    Json(json!({
        "is_duplicate": check.is_duplicate,
        "threshold": check.threshold,
        "nearest": nearest_json,
        "suggested_merge": suggested_merge,
        "candidates_scanned": check.candidates_scanned,
    }))
    .into_response()
}
