// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_skill_compositional_context` handler (v0.7.0 L2-7, issue #672).
//!
//! Returns the **compositional activation payload** for a skill: the
//! decompressed `SKILL.md` body plus reflections drawn from the
//! namespaces declared in the skill's
//! `composes_with_reflections` frontmatter list.
//!
//! # Bounded by design
//!
//! Expansion is hard-bounded at two ends:
//!
//! 1. **Floor.** Per-entry `min_depth` filters out reflections whose
//!    `reflection_depth` is shallower than declared. A `min_depth = 1`
//!    entry excludes caller-minted observations (depth 0).
//! 2. **Ceiling.** Per-namespace `max_reflection_depth` resolved from
//!    [`crate::models::GovernancePolicy::effective_max_reflection_depth`]
//!    is the authoritative ceiling. Composition CANNOT bypass the
//!    bounded-recursion guarantee documented on
//!    `GovernancePolicy::max_reflection_depth`. This guarantees the
//!    operator's "refuse expansion > max_reflection_depth per declared
//!    namespace" requirement at the read side.
//!
//! # Ranking
//!
//! Reflections are ranked by **recency + recall_count**:
//!
//! ```text
//! score = recency_term(created_at) + recall_term(access_count)
//! ```
//!
//! - `recency_term` scales the [0, 1] normalised epoch position so a
//!   memory minted now scores ≈1 and one minted at the start of the
//!   substrate's lifetime scores ≈0.
//! - `recall_term` is `min(access_count, 50) / 50` — cl100k-style
//!   saturating bound so a single hot reflection cannot dominate.
//!
//! The token budget (`budget_tokens`, default 4000) caps cumulative
//! reflection content via `count_tokens_cl100k`. The skill body is NOT
//! counted against the budget (it's the entry point of the
//! composition — every caller wants it).

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::models::skill::ComposesWithReflectionEntry;

/// Default token budget for the composed reflection slice when the
/// caller does not pass `budget_tokens`. Mirrors the conservative-by-
/// default posture of the v0.6.3.1 P6 `memory_recall budget_tokens`
/// surface — small enough not to blow a 32k caller context, large
/// enough to admit ~5-10 typical reflections.
const DEFAULT_BUDGET_TOKENS: usize = 4_000;

/// Hard ceiling on `budget_tokens` accepted from MCP. The substrate is
/// a memory system, not a context-window manager; if the caller wants
/// a 100k-token dump they should iterate `memory_recall` instead.
const MAX_BUDGET_TOKENS: usize = 32_000;

/// Saturation bound on `access_count` for the recall-term scoring.
/// Pinned in module docs.
const RECALL_SATURATION: i64 = 50;

pub(super) fn handle_skill_compositional_context(
    conn: &Connection,
    params: &Value,
) -> Result<Value, String> {
    let skill_id = params["skill_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("memory_skill_compositional_context requires 'skill_id'")?;

    let budget_tokens = parse_budget_tokens(params);

    // -----------------------------------------------------------------------
    // 1) Load the skill row.
    // -----------------------------------------------------------------------
    let (body, metadata_json, namespace, name) = match conn.query_row(
        "SELECT body_blob, metadata, namespace, name FROM skills WHERE id = ?1",
        [skill_id],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        },
    ) {
        Ok(t) => t,
        Err(_) => return Err(format!("skill not found: {skill_id}")),
    };

    let body_bytes =
        zstd::decode_all(body.as_slice()).map_err(|e| format!("zstd decompress body: {e}"))?;
    let body_str = String::from_utf8_lossy(&body_bytes).into_owned();

    // -----------------------------------------------------------------------
    // 2) Read composes_with_reflections from metadata.
    //
    // The L2-7 contract: `parse_skill_md` mirrors the structured
    // declaration into `metadata.composes_with_reflections`, so any
    // skill registered post-L2-7 carries the data in the metadata blob.
    // Pre-L2-7 skills (no declaration) yield an empty Vec and the
    // response degrades cleanly to body-only — matching the
    // documented rollback path on issue #672.
    // -----------------------------------------------------------------------
    let composes = parse_composes_from_metadata(&metadata_json);

    // -----------------------------------------------------------------------
    // 3) For each declared namespace, gather reflections within
    //    [min_depth, max_reflection_depth_for_ns] and score them.
    // -----------------------------------------------------------------------
    let mut scored: Vec<ScoredReflection> = Vec::new();
    let mut bounded_namespaces: Vec<Value> = Vec::with_capacity(composes.len());

    // For recency normalisation we need the min/max created_at of the
    // candidate window. `now_epoch` is the upper anchor; the lower
    // anchor is the oldest matching row. Computed inline below to keep
    // the query plan single-pass.
    let now_epoch = chrono::Utc::now().timestamp();

    for entry in &composes {
        let ceiling = max_reflection_depth_for(conn, &entry.namespace);
        bounded_namespaces.push(json!({
            "namespace": entry.namespace,
            "min_depth": entry.min_depth,
            "max_reflection_depth": ceiling,
        }));

        // Skip when min_depth already exceeds the ceiling — substrate
        // refuses expansion `> max_reflection_depth`, so a floor above
        // the ceiling is a no-op rather than an error. The
        // `bounded_namespaces` entry above is still surfaced so callers
        // can see the floor/ceiling pair and understand why no
        // reflections were returned for this entry.
        if u64::from(entry.min_depth) > u64::from(ceiling) {
            continue;
        }

        let mut stmt = conn
            .prepare(
                "SELECT id, namespace, title, content, created_at, access_count, \
                        reflection_depth, memory_kind \
                 FROM memories \
                 WHERE namespace = ?1 \
                   AND memory_kind = 'reflection' \
                   AND reflection_depth >= ?2 \
                   AND reflection_depth <= ?3 \
                   AND (expires_at IS NULL OR expires_at > ?4) \
                 ORDER BY created_at DESC",
            )
            .map_err(|e| format!("reflections SELECT prepare: {e}"))?;

        let now_iso = chrono::Utc::now().to_rfc3339();
        let rows = stmt
            .query_map(
                rusqlite::params![
                    &entry.namespace,
                    i64::from(entry.min_depth),
                    i64::from(ceiling),
                    now_iso,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i32>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .map_err(|e| format!("reflections SELECT exec: {e}"))?;

        for row in rows {
            let (id, ns, title, content, created_at, access_count, depth, kind) =
                row.map_err(|e| format!("reflections row: {e}"))?;
            let recency = recency_score(&created_at, now_epoch);
            let recall = recall_score(access_count);
            let score = recency + recall;
            scored.push(ScoredReflection {
                id,
                namespace: ns,
                title,
                content,
                created_at,
                access_count,
                reflection_depth: depth,
                memory_kind: kind,
                score,
            });
        }
    }

    // -----------------------------------------------------------------------
    // 4) Sort by score (descending) and apply the token budget.
    // -----------------------------------------------------------------------
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut tokens_used: usize = 0;
    let mut dropped: usize = 0;
    let mut emitted: Vec<Value> = Vec::with_capacity(scored.len());
    for r in scored {
        let r_tokens = crate::db::count_tokens_cl100k(&r.content);
        if tokens_used.saturating_add(r_tokens) > budget_tokens {
            dropped += 1;
            continue;
        }
        tokens_used += r_tokens;
        emitted.push(json!({
            "id": r.id,
            "namespace": r.namespace,
            "title": r.title,
            "content": r.content,
            "created_at": r.created_at,
            "access_count": r.access_count,
            "reflection_depth": r.reflection_depth,
            "memory_kind": r.memory_kind,
            "score": r.score,
        }));
    }

    Ok(json!({
        "skill_id": skill_id,
        "skill_namespace": namespace,
        "skill_name": name,
        "body": body_str,
        "compositional_namespaces": bounded_namespaces,
        "reflections": emitted,
        "budget_tokens": budget_tokens,
        "tokens_used": tokens_used,
        "memories_dropped": dropped,
    }))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

struct ScoredReflection {
    id: String,
    namespace: String,
    title: String,
    content: String,
    created_at: String,
    access_count: i64,
    reflection_depth: i32,
    memory_kind: String,
    score: f64,
}

fn parse_budget_tokens(params: &Value) -> usize {
    let raw = params
        .get("budget_tokens")
        .and_then(serde_json::Value::as_u64);
    match raw {
        Some(n) => {
            let clamped =
                usize::try_from(n.min(u64::try_from(MAX_BUDGET_TOKENS).unwrap_or(u64::MAX)))
                    .unwrap_or(MAX_BUDGET_TOKENS);
            clamped.min(MAX_BUDGET_TOKENS)
        }
        None => DEFAULT_BUDGET_TOKENS,
    }
}

fn parse_composes_from_metadata(metadata_json: &str) -> Vec<ComposesWithReflectionEntry> {
    let Ok(value) = serde_json::from_str::<Value>(metadata_json) else {
        return Vec::new();
    };
    let Some(array) = value
        .get("composes_with_reflections")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    array
        .iter()
        .filter_map(|v| serde_json::from_value::<ComposesWithReflectionEntry>(v.clone()).ok())
        .collect()
}

/// Resolve the effective max-reflection-depth ceiling for a namespace.
/// Falls back to the compiled default (3) when no operator override is
/// present — matching the `GovernancePolicy::effective_max_reflection_depth`
/// contract documented in `src/models/namespace.rs`.
fn max_reflection_depth_for(conn: &Connection, namespace: &str) -> u32 {
    crate::db::resolve_governance_policy(conn, namespace)
        .map_or(3, |p| p.effective_max_reflection_depth())
}

/// Recency score in `[0, 1]`. The substrate stamps `created_at` as RFC
/// 3339; we parse to epoch seconds and scale against a one-year sliding
/// window: a memory minted in the past minute scores ≈1, one a year old
/// scores ≈0, monotonically interpolating in between. Parse failures
/// degrade to `0.0` — a documented-pre-existing memory ALWAYS receives
/// less recency credit than a freshly-minted one when the timestamp is
/// unreadable, never more.
fn recency_score(created_at: &str, now_epoch: i64) -> f64 {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(created_at) else {
        return 0.0;
    };
    let then = parsed.timestamp();
    let age_secs = (now_epoch.saturating_sub(then)).max(0);
    const YEAR_SECS: f64 = 365.25 * 24.0 * 3600.0;
    let normalised = 1.0 - (age_secs as f64 / YEAR_SECS).min(1.0);
    normalised.clamp(0.0, 1.0)
}

/// Recall score in `[0, 1]` via saturating-at-50 linear scaling.
/// Documented on `RECALL_SATURATION`.
fn recall_score(access_count: i64) -> f64 {
    let bounded = access_count.clamp(0, RECALL_SATURATION) as f64;
    bounded / RECALL_SATURATION as f64
}

// ---------------------------------------------------------------------------
// Unit tests — score / parse helpers only. End-to-end behaviour is
// pinned in tests/skill_composition_test.rs against a real DB.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_budget_tokens_defaults_when_absent() {
        let v = json!({});
        assert_eq!(parse_budget_tokens(&v), DEFAULT_BUDGET_TOKENS);
    }

    #[test]
    fn parse_budget_tokens_respects_request() {
        let v = json!({"budget_tokens": 1000});
        assert_eq!(parse_budget_tokens(&v), 1000);
    }

    #[test]
    fn parse_budget_tokens_clamps_to_ceiling() {
        let v = json!({"budget_tokens": 1_000_000});
        assert_eq!(parse_budget_tokens(&v), MAX_BUDGET_TOKENS);
    }

    #[test]
    fn recall_score_saturates_at_50() {
        assert!((recall_score(0) - 0.0).abs() < 1e-9);
        assert!((recall_score(50) - 1.0).abs() < 1e-9);
        // Anything beyond saturates.
        assert!((recall_score(10_000) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn recency_score_unreadable_timestamp_is_zero() {
        let now = 1_700_000_000;
        assert!((recency_score("not-an-rfc3339", now) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn parse_composes_from_metadata_handles_empty() {
        let composes = parse_composes_from_metadata("{}");
        assert!(composes.is_empty());
    }

    #[test]
    fn parse_composes_from_metadata_reads_mirror() {
        let meta = r#"{"composes_with_reflections":[{"namespace":"foo/obs","min_depth":1}]}"#;
        let composes = parse_composes_from_metadata(meta);
        assert_eq!(composes.len(), 1);
        assert_eq!(composes[0].namespace, "foo/obs");
        assert_eq!(composes[0].min_depth, 1);
    }

    #[test]
    fn parse_composes_from_metadata_tolerates_garbage() {
        // Pre-L2-7 readers may stuff entirely-unrelated metadata into the
        // blob. The parser must NOT panic on these.
        let composes = parse_composes_from_metadata(r#"{"composes_with_reflections":"oops"}"#);
        assert!(composes.is_empty());
    }
}
