// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_load_family` and `memory_smart_load` handlers and routing helpers.

use crate::embeddings::{Embed, Embedder};
use crate::models::Memory;
use crate::{db, validate};
use serde_json::{Value, json};
/// v0.7 B1 — `memory_load_family(family, namespace?, k?)`.
///
/// Always-on alternative to `memory_recall` for the case where the agent
/// already knows which `Family` taxonomy bucket it wants. Returns the
/// top-k recent + high-priority memories whose `metadata.family` matches
/// the requested enum, ordered by `priority DESC, updated_at DESC`,
/// optionally restricted to a single namespace.
///
/// Conventions:
///
/// - `family` is required. Validated against the eight-family enum
///   (core/lifecycle/graph/governance/power/meta/archive/other) — anything
///   else returns the same `ProfileParseError::UnknownFamily` diagnostic
///   the rest of the codebase uses, so the error message lists the valid
///   options.
/// - `namespace` is optional. When omitted the query spans every
///   namespace; this matches `memory_list`'s "no namespace = all"
///   convention.
/// - `k` defaults to 20 (mirroring `memory_list`'s default `limit`) and
///   is capped at 100 to bound the response payload. Values outside
///   `[1, 100]` are clamped silently rather than rejected — the cap is
///   for response budget, not for correctness.
///
/// Filter shape: `json_extract(memories.metadata, '$.family') = ?` —
/// no schema change is needed because v0.7 B1 stores the family tag in
/// the existing free-form `metadata` JSON column. Memories that don't
/// carry a `metadata.family` are invisible to this tool by design (the
/// caller would use `memory_list` or `memory_recall` for the unfiltered
/// case).
///
/// Response shape:
/// ```json
/// {
///   "family": "core",
///   "namespace": "projects/alpha",   // or null when omitted
///   "k": 20,
///   "count": 3,
///   "memories": [<MemoryRow>, ...]
/// }

pub fn handle_load_family(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    use crate::profile::Family;
    use std::str::FromStr;

    let family_raw = params["family"].as_str().ok_or("family is required")?;
    // Reuse the canonical enum parser so the diagnostic on a bad
    // `family` value lists the valid options verbatim. Lowercase only,
    // matching the rest of the family vocabulary.
    let family = Family::from_str(family_raw).map_err(|e| e.to_string())?;
    let family_name = family.name();

    let namespace = params.get("namespace").and_then(Value::as_str);
    if let Some(ns) = namespace {
        validate::validate_namespace(ns).map_err(|e| e.to_string())?;
    }

    // Default 20, cap at 100 (per spec). Anything below 1 collapses to 1
    // — calling `memory_load_family(k=0)` is almost always a bug, and
    // the always-return-at-least-one shape lines up with R1's recall
    // budget guarantee.
    let k_raw = params.get("k").and_then(Value::as_u64).unwrap_or(20);
    let k = usize::try_from(k_raw).unwrap_or(usize::MAX).clamp(1, 100);

    let now = chrono::Utc::now().to_rfc3339();
    let mut stmt = conn
        .prepare(
            "SELECT id, tier, namespace, title, content, tags, priority, confidence, source, \
                    access_count, created_at, updated_at, last_accessed_at, expires_at, metadata \
             FROM memories \
             WHERE (?1 IS NULL OR namespace = ?1) \
               AND json_extract(metadata, '$.family') = ?2 \
               AND (expires_at IS NULL OR expires_at > ?3) \
             ORDER BY priority DESC, updated_at DESC \
             LIMIT ?4",
        )
        .map_err(|e| format!("prepare memory_load_family failed: {e}"))?;

    let rows = stmt
        .query_map(
            rusqlite::params![namespace, family_name, now, k],
            db::row_to_memory,
        )
        .map_err(|e| format!("query memory_load_family failed: {e}"))?;
    let memories: Vec<Memory> = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| format!("collect memory_load_family rows failed: {e}"))?;

    Ok(json!({
        "family": family_name,
        "namespace": namespace,
        "k": k,
        "count": memories.len(),
        "memories": memories,
    }))
}

/// v0.7 B2 — `memory_smart_load(intent, namespace?, k?)`.
///
/// Always-on intent-routed loader. Caller passes a free-text intent
/// (e.g. "I'm about to debug a flaky test"); the handler picks the best
/// `Family` and forwards to [`handle_load_family`]. The agent does not
/// need to know the family taxonomy — it only describes what it's
/// about to do.
///
/// Routing strategy:
///
/// - **Embedder available (B3 wiring, future):** when an `Embedder` is
///   provided, embed the intent and score it against the cached family
///   descriptor embeddings via cosine similarity. The family with the
///   top score wins; the score is reported alongside the answer.
/// - **Fallback (no embedder, e.g. keyword tier):** a deterministic
///   keyword-overlap scorer maps the intent to the family with the
///   highest descriptor-token overlap. The score is the normalized
///   overlap ratio in `[0.0, 1.0]`. When no descriptor matches at all
///   (e.g. an empty or wholly off-topic intent), the routing falls back
///   to `Family::Core` and `chosen_family_source` is reported as
///   `"fallback"` so the caller can detect the no-signal case.
///
/// Response shape:
/// ```json
/// {
///   "chosen_family": "graph",
///   "score": 0.62,
///   "chosen_family_source": "embedder" | "keyword" | "fallback",
///   "intent": "<echoed input>",
///   "namespace": "projects/alpha", // or null
///   "k": 20,
///   "count": 3,
///   "memories": [<MemoryRow>, ...]
/// }
/// ```
///
/// `k` defaults to 20 (mirroring `memory_load_family`) and is capped at
/// 100. `intent` is required and may not be empty after trimming.
pub fn handle_smart_load(
    conn: &rusqlite::Connection,
    params: &Value,
    embedder: Option<&dyn Embed>,
) -> Result<Value, String> {
    let intent_raw = params["intent"].as_str().ok_or("intent is required")?;
    let intent = intent_raw.trim();
    if intent.is_empty() {
        // Empty intent is the canonical "no signal" case — route to
        // Core and surface `chosen_family_source: "fallback"` so the
        // caller can detect it. `handle_load_family` then runs the same
        // DB query memory_load_family(family=core) would.
        let resp = forward_to_load_family(
            conn,
            crate::profile::Family::Core,
            0.0,
            "fallback",
            intent,
            params,
        )?;
        return Ok(resp);
    }

    // Round-4 — keyword-veto strategy. Always run the deterministic
    // keyword scorer first. If it produces a non-fallback signal (i.e.
    // at least one intent token overlapped some family's descriptor
    // or tool-name segments), let the embedder vote — but veto the
    // embedder when it disagrees. Rationale: the embedder's cosine
    // similarity over ~80-word descriptors is noisy for short
    // imperative intents like "store a new memory" or "verify a
    // memory's signature" — Round-3 measured 8/10 routing accuracy,
    // Round-4 measured 4–5/10 with the embedder winning on common
    // verbs but mis-routing them to `archive`. The keyword path is
    // hand-tuned (F14) and deterministic; treat it as ground truth
    // when it has a signal, fall back to the embedder only when it
    // returns `"fallback"` (no token overlap anywhere).
    let kw_pick = fallback_via_keywords(intent);
    let (family, score, source) = match embedder {
        Some(emb) => match best_family_via_embedder(emb, intent) {
            Some((emb_family, emb_score)) => {
                if kw_pick.2 == "keyword" && kw_pick.0 != emb_family {
                    // Keyword scored a non-fallback hit AND disagreed with
                    // the embedder — trust the deterministic scorer.
                    kw_pick
                } else {
                    (emb_family, emb_score, "embedder")
                }
            }
            None => kw_pick,
        },
        None => kw_pick,
    };

    forward_to_load_family(conn, family, score, source, intent, params)
}

/// Build the `memory_smart_load` response by forwarding to
/// [`handle_load_family`] with the chosen family. The forwarded JSON is
/// flattened into the smart_load response shape so callers see one
/// payload, not a nested `load_family_response` blob.
fn forward_to_load_family(
    conn: &rusqlite::Connection,
    family: crate::profile::Family,
    score: f32,
    source: &str,
    intent: &str,
    params: &Value,
) -> Result<Value, String> {
    let family_name = family.name();
    tracing::info!(
        target: "memory_smart_load",
        chosen_family = family_name,
        score = score,
        source = source,
        intent_len = intent.len(),
        "smart_load routed intent to family"
    );

    // Build the payload memory_load_family expects: family + the
    // forwarded namespace + k from the caller.
    let mut forward = json!({"family": family_name});
    if let Some(ns) = params.get("namespace").and_then(Value::as_str) {
        forward["namespace"] = json!(ns);
    }
    if let Some(k) = params.get("k").and_then(Value::as_u64) {
        forward["k"] = json!(k);
    }

    let inner = handle_load_family(conn, &forward)?;
    let memories = inner.get("memories").cloned().unwrap_or_else(|| json!([]));
    let count = inner.get("count").cloned().unwrap_or_else(|| json!(0));
    let k = inner.get("k").cloned().unwrap_or_else(|| json!(20));
    let namespace = inner.get("namespace").cloned().unwrap_or(Value::Null);

    // Round score to 3 decimals at the wire — keeps the JSON readable
    // without leaking f32 quantisation noise (same convention as
    // memory_check_duplicate).
    let score_rounded = (f64::from(score) * 1000.0).round() / 1000.0;

    Ok(json!({
        "chosen_family": family_name,
        "score": score_rounded,
        "chosen_family_source": source,
        "intent": intent,
        "namespace": namespace,
        "k": k,
        "count": count,
        "memories": memories,
    }))
}

/// Embedder-driven family pick. Embeds the intent, scores it against
/// the cached descriptor for each family, and returns the top-scoring
/// family + similarity. Returns `None` when the embedder fails
/// (network blip, model not loaded, etc.) so the caller can fall back
/// to the keyword scorer.
///
/// **B3 forward-compat note:** when B3 lands and `AppState` carries
/// `family_embeddings: Arc<RwLock<Option<Vec<(Family, Vec<f32>)>>>>`
/// plus `best_family_match(intent)`, this helper should be replaced
/// with a call into `state.best_family_match(intent)`. Until then, the
/// descriptors are embedded inline on every call — accurate but not
/// the production caching shape.
fn best_family_via_embedder(
    emb: &dyn Embed,
    intent: &str,
) -> Option<(crate::profile::Family, f32)> {
    use crate::profile::Family;

    let intent_vec = emb.embed(intent).ok()?;
    let mut best: Option<(Family, f32)> = None;
    for family in Family::all() {
        let descriptor = family_descriptor(*family);
        let Ok(desc_vec) = emb.embed(descriptor) else {
            continue;
        };
        let score = Embedder::cosine_similarity(&intent_vec, &desc_vec);
        if best.is_none_or(|(_, s)| score > s) {
            best = Some((*family, score));
        }
    }
    best
}

/// Deterministic keyword-overlap scorer used when no embedder is
/// available (e.g. the `keyword` feature tier) or when the embedder
/// returns an error mid-call. Splits `intent` on ASCII non-alphanumeric
/// boundaries, lowercases, and counts how many tokens overlap each
/// family's combined token set (descriptor ∪ tool-name tokens).
///
/// Round-2 F14 — the family token set is the union of:
/// 1. The family's descriptor (free-text intent vocabulary).
/// 2. The family's tool names tokenised on underscore boundaries
///    (`memory_notify` → `memory`, `notify`).
/// 3. The family's tool names as full identifiers (`memory_notify`
///    kept as a single token so an intent that names the tool
///    verbatim still scores).
///
/// Tool-name overlaps are weighted 2x descriptor overlaps: a tool
/// name is a stronger signal than a generic intent vocabulary
/// keyword. Without this, intents like "send a notification to
/// another agent" mis-routed to `meta` (because "agent" appears in
/// the meta descriptor) instead of `other` (where `memory_notify`
/// lives).
///
/// The universal `memory` prefix on every tool name is excluded
/// from the tool-name overlap count so it doesn't dominate the
/// score across all families.
///
/// Ties broken by family declaration order so routing is stable.
/// When no token matches at all, routing falls back to
/// `Family::Core` with score 0.0 and source `"fallback"`.
fn fallback_via_keywords(intent: &str) -> (crate::profile::Family, f32, &'static str) {
    use crate::profile::Family;

    let intent_tokens: Vec<String> = intent
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    if intent_tokens.is_empty() {
        return (Family::Core, 0.0, "fallback");
    }

    let mut best: Option<(Family, f32)> = None;
    for family in Family::all() {
        let descriptor = family_descriptor(*family).to_ascii_lowercase();
        let desc_tokens: Vec<String> = descriptor
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        // Round-2 F14 — for each tool in the family, compute the
        // count of DISTINCT intent tokens that match the tool's
        // segments (or its full identifier). A tool whose name
        // encodes BOTH intent keywords (e.g. `expand_query` matches
        // intent="expand a query" on both `expand` AND `query`)
        // contributes a stronger signal than a tool whose name only
        // matches one keyword (e.g. `kg_query` only matches `query`).
        // The per-tool distinct-token count is summed across the
        // family AND tracked as a max so a single highly-specific
        // tool can pull a family above one that matches via several
        // weak tools.
        //
        // Match relation = exact token equality OR shared 5+ char
        // prefix when both tokens are ≥ 5 chars long. The prefix
        // relaxation lets "notification" match `notify` segment
        // (shared "notif" prefix), which the strict-equality form
        // missed. Without this, intents that use a different
        // English surface form than the tool name's stem
        // (notify/notification, subscribe/subscription, etc.) only
        // matched via the wider descriptor vocabulary.
        let token_matches = |a: &str, b: &str| -> bool {
            if a == b {
                return true;
            }
            // Prefix relaxation guard: both tokens ≥ 5 chars and
            // share a 5-char prefix. Threshold 5 keeps "store"/
            // "stories", "task"/"taskbar" from cross-matching.
            if a.len() >= 5 && b.len() >= 5 && a[..5] == b[..5] {
                return true;
            }
            false
        };

        let mut tool_distinct_sum: usize = 0;
        let mut tool_distinct_max: usize = 0;
        let mut full_id_hits: usize = 0;
        for tool_name in family.tool_names() {
            let lower = tool_name.to_ascii_lowercase();
            // Segments from underscore-split. Skip the universal
            // `memory` prefix (it's noise — every tool has it).
            let segments: Vec<&str> = lower
                .split('_')
                .filter(|s| !s.is_empty() && *s != "memory")
                .collect();
            // Distinct intent tokens that match any segment of THIS
            // tool name (exact OR 5-char-prefix relaxed match).
            let distinct = intent_tokens
                .iter()
                .filter(|t| segments.iter().any(|seg| token_matches(seg, t.as_str())))
                .count();
            tool_distinct_sum += distinct;
            if distinct > tool_distinct_max {
                tool_distinct_max = distinct;
            }
            // Full-identifier hit — when an intent token EQUALS the
            // full tool name (with underscores), the caller has
            // named the tool verbatim. Strongest signal.
            if intent_tokens.iter().any(|t| t.as_str() == lower) {
                full_id_hits += 1;
            }
        }

        let desc_overlap = intent_tokens
            .iter()
            .filter(|t| desc_tokens.iter().any(|d| d == *t))
            .count();

        if desc_overlap == 0 && tool_distinct_sum == 0 && full_id_hits == 0 {
            continue;
        }

        // Round-2 F14 — composite score:
        //   2.0 * descriptor overlap (curated intent vocabulary —
        //         each family's descriptor is hand-tuned to capture
        //         the family's purpose, so a hit is high-signal)
        // + 1.0 * sum of distinct-intent-tokens-per-tool (boost
        //         when a tool name's segments encode intent
        //         keywords — broader, more false-positive prone
        //         than the descriptor)
        // + 2.0 * tool_distinct_max (strong extra boost when ONE
        //         tool name matches MULTIPLE intent keywords —
        //         distinguishes `expand_query` matching both
        //         "expand" and "query" from `kg_query` matching
        //         only "query")
        // + 4.0 * full-identifier hits (caller named the tool
        //         verbatim — overwhelming signal)
        // Normalised by intent token count so single-token intents
        // still produce a sensible score in [0, ~1+].
        #[allow(clippy::cast_precision_loss)]
        let score = (2.0 * desc_overlap as f32
            + tool_distinct_sum as f32
            + 2.0 * tool_distinct_max as f32
            + 4.0 * full_id_hits as f32)
            / (intent_tokens.len() as f32);
        if best.is_none_or(|(_, s)| score > s) {
            best = Some((*family, score));
        }
    }

    best.map_or((Family::Core, 0.0, "fallback"), |(f, s)| (f, s, "keyword"))
}

/// Per-family descriptor used as the embedding/keyword target. Each
/// descriptor is a short paragraph of intent-style language that
/// captures what an agent might say when about to act on that family's
/// tools. Source-anchored at the family enum in `src/profile.rs`.
///
/// **B3 forward-compat note:** when B3 lands these strings will be
/// embedded once at startup and cached on `AppState::family_embeddings`,
/// rather than re-embedded per smart_load call. The strings themselves
/// stay anchored here so the cache and the keyword fallback share one
/// vocabulary.
fn family_descriptor(family: crate::profile::Family) -> &'static str {
    use crate::profile::Family;
    match family {
        Family::Core => {
            "store remember save record memory note write recall fetch get \
             search find list browse read load family core baseline"
        }
        Family::Lifecycle => {
            "update edit modify change delete remove forget purge garbage \
             collect promote upgrade downgrade migrate refresh rotate"
        }
        Family::Graph => {
            "graph link relation entity knowledge kg query timeline replay \
             verify path traverse find_paths connect taxonomy alias debug \
             flaky test investigate trace ancestry"
        }
        Family::Governance => {
            "approve reject pending policy permission rule namespace \
             standard subscribe unsubscribe governance review audit"
        }
        Family::Power => {
            "consolidate merge contradiction duplicate auto tag expand \
             query inbox subscription replay dlq dead letter retry power \
             llm augment"
        }
        Family::Meta => {
            "capabilities agent register session start stats meta info \
             discovery introspection bootstrap"
        }
        Family::Archive => {
            "archive backup restore purge old historical retention cold \
             storage"
        }
        // Round-2 F14 — extended with "notification message send
        // dm direct another recipient inbox" so an intent like "I
        // want to send a notification to another agent" routes to
        // `other` (where `memory_notify` lives). The tool-name boost
        // (2x weight on `memory_notify` → `notify`) plus the wider
        // vocabulary covers both "notify" and "notification" surface
        // forms.
        Family::Other => {
            "subscription notify subscribe webhook event other miscellaneous \
             notification message send dm direct another recipient inbox"
        }
    }
}
