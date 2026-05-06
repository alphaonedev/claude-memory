// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7.0 — Track G, task G11 (R3).
//
// `auto-link-detector` is the reference `post_store` hook for the
// attested-cortex epic's R3 commitment: when a new memory lands,
// scan the recent neighbours in the same namespace, score textual
// overlap with a Jaccard heuristic, and propose `auto-related`
// links for any neighbour above the similarity threshold.
//
// # What it does
//
// 1. Reads a JSON `FireEnvelope` from stdin (the same shape
//    `src/hooks/executor.rs::FireEnvelope` writes to every hook
//    subprocess).
// 2. Recognises the event class (`post_store`) and pulls the new
//    memory's `id`, `namespace`, and `content` out of the payload.
// 3. Walks the optional `payload.recent_namespace_memories` bag
//    (an array the executor surfaces alongside the just-stored
//    row — capped at N=50 by the production wiring; the detector
//    re-enforces an internal cap here so a misconfigured executor
//    can't fan the heuristic out unbounded).
// 4. For each candidate, computes Jaccard similarity over
//    normalised lowercase tokens (no embedder dep — keep this
//    CI-friendly per the I5 precedent). If similarity exceeds the
//    threshold (`SIMILARITY_THRESHOLD`, default 0.4; overridable
//    via `AUTOLINK_SIMILARITY_THRESHOLD` env), emits a proposed
//    `memory_link` with `kind="auto-related"`, `attest_level="R3"`.
// 5. Returns `{"action":"modify","delta":{"metadata":{
//    "auto_related_links":[ ... ]}}}` — the proposals ride inside
//    the metadata bag so a follow-up production `post_store`
//    persister (post-G11) can walk them and call `db::create_link`
//    transactionally. Today's executor will degrade `Modify` on
//    `post_store` to `Allow` (per `decision.rs` rules); the
//    proposals nonetheless surface on stdout for the chain log
//    and the integration test harness.
//
// # Why R3 as `attest_level`
//
// The proposal is a *heuristic* with no agent identity behind it.
// The H-track attestation enum (`unsigned`/`self_signed`/
// `peer_attested`) is reserved for cryptographically attested
// links written via the `memory_link` API. The `R3` literal here
// names the commitment the proposal originated from so a downstream
// minter can decide whether to land it as `unsigned` (the safe
// default) or feed it into a higher-trust scoring step. The
// reference impl never claims `self_signed` or above — that
// would be a security regression.
//
// # Why no embedding-similarity scoring
//
// Same rationale as I5's `transcript-extractor`: pulling the
// embedder into a reference tool re-introduces the entire
// transitive `candle + tokenizers + ...` graph for a CI-friendly
// hook. Token-bag Jaccard is a deterministic baseline; a future
// production hop can swap in cosine similarity over the existing
// HNSW index without changing the wire contract on either end.
//
// # Modes
//
// * `--once` (default): read one envelope from stdin, write one
//   decision to stdout, exit. This matches the executor's
//   `ExecExecutor` mode.
//
// * `--daemon`: read newline-delimited envelopes from stdin in a
//   loop, write one decision per line on stdout. Matches the
//   `DaemonExecutor` framing in `src/hooks/executor.rs`.

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::io::{self, BufRead, Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Wire types — minimal mirror of the executor's payload shapes so the
// detector doesn't pull `ai-memory` into its dependency graph.
// ---------------------------------------------------------------------------

/// Mirrors `ai_memory::hooks::events::HookEvent` (`snake_case`
/// enum tag). Only the variants R3 cares about are listed; an
/// unknown tag deserialises into `Other` and the detector falls
/// through to `Allow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HookEventTag {
    PostStore,
    PostTranscriptStore,
    #[serde(other)]
    Other,
}

/// Envelope written by the executor on every fire. See
/// `src/hooks/executor.rs::FireEnvelope`.
#[derive(Debug, Deserialize)]
struct FireEnvelope {
    event: HookEventTag,
    #[serde(default)]
    payload: Value,
}

// ---------------------------------------------------------------------------
// Heuristic — Jaccard similarity over normalised tokens
// ---------------------------------------------------------------------------

/// Stop-word list — small, English-leaning. Mirrors I5's list so
/// the two reference hooks have a consistent baseline. A
/// production extractor would reach for `tantivy`'s analyser or an
/// LLM.
const STOP_WORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "if", "then", "is", "are", "was", "were", "be", "been",
    "being", "have", "has", "had", "do", "does", "did", "will", "would", "could", "should", "may",
    "might", "must", "can", "shall", "to", "of", "in", "on", "at", "by", "for", "with", "about",
    "as", "from", "into", "that", "this", "these", "those", "it", "its", "i", "you", "he", "she",
    "we", "they", "them", "his", "her", "our", "their", "my", "your", "me", "us", "him", "what",
    "which", "who", "when", "where", "why", "how", "not", "no", "yes", "so", "up", "down", "out",
    "over", "under", "again", "further", "than", "too", "very", "just", "now",
];

/// Tokenise `text` into a deduplicated bag of lowercase words,
/// stripped of punctuation and stop-words. ASCII-only tokenisation
/// keeps the reference impl free of `unicode-segmentation` —
/// good enough for English-shaped notes and obvious where to
/// swap in a real analyser later.
fn token_bag(text: &str) -> HashSet<String> {
    let mut bag = HashSet::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        let lc = raw.to_ascii_lowercase();
        if lc.len() < 3 {
            continue;
        }
        if STOP_WORDS.contains(&lc.as_str()) {
            continue;
        }
        bag.insert(lc);
    }
    bag
}

/// Default Jaccard similarity threshold above which the detector
/// proposes an `auto-related` link. The R3 task brief calls for a
/// "0.4-ish" baseline; that's the operational sweet spot where
/// short notes that share a few topical tokens get linked but
/// generic prose ("the thing was good") does not.
const SIMILARITY_THRESHOLD: f64 = 0.4;

/// Read the `AUTOLINK_SIMILARITY_THRESHOLD` env knob with the
/// [`SIMILARITY_THRESHOLD`] fallback. Out-of-range values
/// (NaN, ≤0.0, ≥1.0) collapse to the default — a misconfigured
/// hook can't disable the gate by setting it to 0.0.
fn similarity_threshold_from_env() -> f64 {
    std::env::var("AUTOLINK_SIMILARITY_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0 && *v < 1.0)
        .unwrap_or(SIMILARITY_THRESHOLD)
}

/// Default cap on how many candidate neighbours the detector will
/// score per fire. Mirrors the production executor's documented
/// N=50 cap so a misconfigured executor that surfaces an
/// unbounded `recent_namespace_memories` array can't fan the
/// detector out to thousands of comparisons.
const MAX_CANDIDATES: usize = 50;

/// Default cap on the number of links the detector will propose
/// per fire. Bounded so a single store of "highly generic" content
/// against a busy namespace can't mint dozens of links in one hop.
const MAX_PROPOSALS: usize = 16;

/// Compute Jaccard similarity between two token bags:
/// `|A ∩ B| / |A ∪ B|`. Returns `0.0` when either bag is empty
/// (a memory with no scored tokens has no measurable overlap with
/// anything; treating it as "0" keeps the gate from firing).
fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    if intersection == 0 {
        return 0.0;
    }
    let union = a.union(b).count();
    // `intersection ≤ union ≤ a.len() + b.len()` and both bags
    // are bounded by content length, so the cast carries no
    // precision-loss risk for any plausible memory.
    let inter_u32 = u32::try_from(intersection).unwrap_or(u32::MAX);
    let union_u32 = u32::try_from(union).unwrap_or(u32::MAX);
    f64::from(inter_u32) / f64::from(union_u32)
}

// ---------------------------------------------------------------------------
// Proposal shape — emitted in the Modify delta's metadata bag
// ---------------------------------------------------------------------------

/// One proposed `auto-related` link. The wire shape is the public
/// contract a downstream production minter (post-G11) consumes.
///
/// * `source` and `target` carry memory ids — the just-stored row
///   is always the source, the neighbour is the target.
/// * `kind` is the constant `"auto-related"` per the R3 brief.
/// * `attest_level` is the literal `"R3"` — a sentinel naming the
///   commitment that produced the proposal, *not* the H-track
///   cryptographic attestation enum. A minter that lands the
///   proposal as a real `memory_links` row maps `R3` →
///   `unsigned` (or whatever the operator's policy dictates).
/// * `score` carries the Jaccard value so a follow-up scorer
///   can rank or filter without recomputing.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct AutoRelatedLink {
    source: String,
    target: String,
    kind: &'static str,
    attest_level: &'static str,
    score: f64,
}

impl AutoRelatedLink {
    fn new(source: String, target: String, score: f64) -> Self {
        Self {
            source,
            target,
            kind: "auto-related",
            attest_level: "R3",
            score,
        }
    }
}

// ---------------------------------------------------------------------------
// Decision construction
// ---------------------------------------------------------------------------

/// Pull the new memory's id, namespace, and content from the
/// payload. Returns `None` if any required field is missing or
/// empty — the caller treats that as a "no work" signal.
fn extract_new_memory(payload: &Value) -> Option<(String, String, String)> {
    let id = payload.get("id").and_then(Value::as_str)?.to_string();
    let namespace = payload
        .get("namespace")
        .and_then(Value::as_str)?
        .to_string();
    let content = payload.get("content").and_then(Value::as_str)?.to_string();
    if id.is_empty() || namespace.is_empty() || content.trim().is_empty() {
        return None;
    }
    Some((id, namespace, content))
}

/// Score one candidate against the new memory's token bag and,
/// when above the threshold, return a proposal. Candidates that
/// are missing required fields (id / content) or that fall in a
/// different namespace silently drop — the heuristic only links
/// inside a namespace per the R3 brief.
fn score_candidate(
    new_id: &str,
    new_namespace: &str,
    new_bag: &HashSet<String>,
    candidate: &Value,
    threshold: f64,
) -> Option<AutoRelatedLink> {
    let cand_id = candidate.get("id").and_then(Value::as_str)?;
    if cand_id == new_id {
        // The just-stored row sometimes echoes back inside
        // `recent_namespace_memories`; never link to self.
        return None;
    }
    let cand_ns = candidate.get("namespace").and_then(Value::as_str)?;
    if cand_ns != new_namespace {
        return None;
    }
    let cand_content = candidate.get("content").and_then(Value::as_str)?;
    if cand_content.trim().is_empty() {
        return None;
    }
    let cand_bag = token_bag(cand_content);
    let score = jaccard(new_bag, &cand_bag);
    if score < threshold {
        return None;
    }
    Some(AutoRelatedLink::new(
        new_id.to_string(),
        cand_id.to_string(),
        score,
    ))
}

/// Build the JSON decision line emitted to stdout. Mirrors the
/// wire shape `src/hooks/decision.rs::HookDecision` parses.
///
/// Emitting `Allow` (the empty-object form) on every "no work"
/// path keeps the executor's `parse_decision_line` happy while
/// still being trivially greppable from a debug log.
fn build_decision(envelope: &FireEnvelope) -> Value {
    // Only PostStore-shaped events drive the detector. Anything
    // else falls through to Allow so the detector can be safely
    // wired to multiple chains without misbehaving on the wrong
    // event class.
    if !matches!(
        envelope.event,
        HookEventTag::PostStore | HookEventTag::PostTranscriptStore
    ) {
        return json!({ "action": "allow" });
    }

    let Some((new_id, new_namespace, new_content)) = extract_new_memory(&envelope.payload) else {
        return json!({ "action": "allow" });
    };

    let new_bag = token_bag(&new_content);
    if new_bag.is_empty() {
        return json!({ "action": "allow" });
    }

    let candidates = envelope
        .payload
        .get("recent_namespace_memories")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if candidates.is_empty() {
        return json!({ "action": "allow" });
    }

    let threshold = similarity_threshold_from_env();
    let mut proposals: Vec<AutoRelatedLink> = candidates
        .iter()
        .take(MAX_CANDIDATES)
        .filter_map(|c| score_candidate(&new_id, &new_namespace, &new_bag, c, threshold))
        .collect();

    if proposals.is_empty() {
        return json!({ "action": "allow" });
    }

    // Highest-scoring proposals first; cap the bag so a single
    // store can't mint an arbitrary number of links.
    proposals.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    proposals.truncate(MAX_PROPOSALS);

    let proposals_value = serde_json::to_value(&proposals).unwrap_or_else(|_| json!([]));

    // Merge the proposal bag into the existing metadata bag so a
    // hook upstream that already wrote `metadata.governance` (etc.)
    // doesn't lose its keys. The detector only writes one key:
    // `auto_related_links`.
    let mut metadata = envelope
        .payload
        .get("metadata")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !metadata.is_object() {
        metadata = json!({});
    }
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert("auto_related_links".to_string(), proposals_value);
    }

    json!({
        "action": "modify",
        "delta": {
            "metadata": metadata,
        }
    })
}

// ---------------------------------------------------------------------------
// I/O drivers
// ---------------------------------------------------------------------------

fn run_once<R: Read, W: Write>(mut reader: R, mut writer: W) -> io::Result<()> {
    let mut buf = String::new();
    reader.read_to_string(&mut buf)?;
    let envelope: FireEnvelope = match serde_json::from_str(&buf) {
        Ok(e) => e,
        Err(e) => {
            // Malformed input — emit Allow + a stderr breadcrumb so
            // the executor's chain runner doesn't deny the underlying
            // memory operation just because the hook can't parse.
            eprintln!("auto-link-detector: malformed envelope: {e}");
            writeln!(writer, "{}", json!({ "action": "allow" }))?;
            writer.flush()?;
            return Ok(());
        }
    };
    let decision = build_decision(&envelope);
    writeln!(writer, "{decision}")?;
    writer.flush()?;
    Ok(())
}

fn run_daemon<R: BufRead, W: Write>(mut reader: R, mut writer: W) -> io::Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            // EOF — parent closed stdin. Clean exit.
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let envelope: FireEnvelope = match serde_json::from_str(trimmed) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("auto-link-detector: malformed envelope: {e}");
                writeln!(writer, "{}", json!({ "action": "allow" }))?;
                writer.flush()?;
                continue;
            }
        };
        let decision = build_decision(&envelope);
        writeln!(writer, "{decision}")?;
        writer.flush()?;
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let daemon = args.iter().any(|a| a == "--daemon");
    if daemon {
        let stdin = io::stdin();
        let stdout = io::stdout();
        run_daemon(stdin.lock(), stdout.lock())
    } else {
        let stdin = io::stdin();
        let stdout = io::stdout();
        run_once(stdin.lock(), stdout.lock())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_envelope_post_store(payload: &Value) -> String {
        json!({
            "event": "post_store",
            "payload": payload,
        })
        .to_string()
    }

    // -----------------------------------------------------------------
    // Unit case 1 — high similarity → proposal emitted
    // -----------------------------------------------------------------
    #[test]
    fn high_similarity_emits_auto_related_link() {
        let envelope: FireEnvelope = serde_json::from_str(&make_envelope_post_store(&json!({
            "id": "mem-new",
            "namespace": "team/eng",
            "content": "Postgres autovacuum tuning: scale_factor and cost_delay knobs control bloat.",
            "recent_namespace_memories": [
                {
                    "id": "mem-neighbour",
                    "namespace": "team/eng",
                    "content": "Postgres autovacuum scale_factor and cost_delay tuning notes for production bloat control.",
                }
            ],
        })))
        .unwrap();
        let decision = build_decision(&envelope);
        assert_eq!(decision["action"], "modify");
        let links = decision["delta"]["metadata"]["auto_related_links"]
            .as_array()
            .expect("links array");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0]["source"], "mem-new");
        assert_eq!(links[0]["target"], "mem-neighbour");
        assert_eq!(links[0]["kind"], "auto-related");
        assert_eq!(links[0]["attest_level"], "R3");
        assert!(links[0]["score"].as_f64().unwrap() >= SIMILARITY_THRESHOLD);
    }

    // -----------------------------------------------------------------
    // Unit case 2 — low similarity → Allow (no proposal)
    // -----------------------------------------------------------------
    #[test]
    fn low_similarity_returns_allow() {
        let envelope: FireEnvelope = serde_json::from_str(&make_envelope_post_store(&json!({
            "id": "mem-new",
            "namespace": "team/eng",
            "content": "Reverted PR #555 because it broke v3 capabilities matrix.",
            "recent_namespace_memories": [
                {
                    "id": "mem-neighbour",
                    "namespace": "team/eng",
                    "content": "Postgres autovacuum scale_factor cost_delay bloat tuning.",
                }
            ],
        })))
        .unwrap();
        let decision = build_decision(&envelope);
        assert_eq!(decision["action"], "allow");
    }

    // -----------------------------------------------------------------
    // Unit case 3 — identical content → score=1.0, link emitted (and
    // the self-id case is suppressed separately)
    // -----------------------------------------------------------------
    #[test]
    fn identical_content_scores_one_and_links() {
        let same = "Postgres autovacuum scale_factor and cost_delay tuning notes.";
        let envelope: FireEnvelope = serde_json::from_str(&make_envelope_post_store(&json!({
            "id": "mem-new",
            "namespace": "team/eng",
            "content": same,
            "recent_namespace_memories": [
                {
                    "id": "mem-clone",
                    "namespace": "team/eng",
                    "content": same,
                }
            ],
        })))
        .unwrap();
        let decision = build_decision(&envelope);
        assert_eq!(decision["action"], "modify");
        let links = decision["delta"]["metadata"]["auto_related_links"]
            .as_array()
            .unwrap();
        assert_eq!(links.len(), 1);
        let score = links[0]["score"].as_f64().unwrap();
        assert!(
            (score - 1.0).abs() < 1e-9,
            "identical content must score 1.0, got {score}"
        );
    }

    // -----------------------------------------------------------------
    // Unit case 4 — empty content → Allow (no work to do)
    // -----------------------------------------------------------------
    #[test]
    fn empty_content_returns_allow() {
        let envelope: FireEnvelope = serde_json::from_str(&make_envelope_post_store(&json!({
            "id": "mem-new",
            "namespace": "team/eng",
            "content": "   ",
            "recent_namespace_memories": [
                {
                    "id": "mem-neighbour",
                    "namespace": "team/eng",
                    "content": "Postgres autovacuum scale_factor and cost_delay.",
                }
            ],
        })))
        .unwrap();
        let decision = build_decision(&envelope);
        assert_eq!(decision["action"], "allow");
    }

    // -----------------------------------------------------------------
    // Unit case 5 — malformed input → Allow + stderr breadcrumb
    // -----------------------------------------------------------------
    #[test]
    fn malformed_input_emits_allow() {
        let mut out: Vec<u8> = Vec::new();
        run_once(b"not-json".as_slice(), &mut out).expect("run_once ok");
        let line = String::from_utf8(out).unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["action"], "allow");
    }

    // -----------------------------------------------------------------
    // Edge cases — round out coverage of the heuristic + driver paths
    // -----------------------------------------------------------------

    #[test]
    fn unknown_event_falls_through_to_allow() {
        let envelope: FireEnvelope = serde_json::from_str(
            r#"{"event":"pre_store","payload":{"id":"x","namespace":"team/eng","content":"hi"}}"#,
        )
        .unwrap();
        let decision = build_decision(&envelope);
        assert_eq!(decision["action"], "allow");
    }

    #[test]
    fn self_id_in_candidates_is_skipped() {
        let envelope: FireEnvelope = serde_json::from_str(&make_envelope_post_store(&json!({
            "id": "mem-new",
            "namespace": "team/eng",
            "content": "Postgres autovacuum scale_factor and cost_delay tuning.",
            "recent_namespace_memories": [
                {
                    "id": "mem-new",
                    "namespace": "team/eng",
                    "content": "Postgres autovacuum scale_factor and cost_delay tuning.",
                }
            ],
        })))
        .unwrap();
        let decision = build_decision(&envelope);
        assert_eq!(decision["action"], "allow");
    }

    #[test]
    fn cross_namespace_candidate_is_skipped() {
        let envelope: FireEnvelope = serde_json::from_str(&make_envelope_post_store(&json!({
            "id": "mem-new",
            "namespace": "team/eng",
            "content": "Postgres autovacuum scale_factor and cost_delay tuning.",
            "recent_namespace_memories": [
                {
                    "id": "mem-other",
                    "namespace": "team/legal",
                    "content": "Postgres autovacuum scale_factor and cost_delay tuning.",
                }
            ],
        })))
        .unwrap();
        let decision = build_decision(&envelope);
        assert_eq!(decision["action"], "allow");
    }

    #[test]
    fn missing_recent_neighbours_returns_allow() {
        let envelope: FireEnvelope = serde_json::from_str(&make_envelope_post_store(&json!({
            "id": "mem-new",
            "namespace": "team/eng",
            "content": "Postgres autovacuum scale_factor and cost_delay tuning.",
        })))
        .unwrap();
        let decision = build_decision(&envelope);
        assert_eq!(decision["action"], "allow");
    }

    #[test]
    fn proposals_are_capped_at_max_proposals() {
        let mut neighbours = Vec::new();
        for i in 0..50 {
            neighbours.push(json!({
                "id": format!("mem-n-{i}"),
                "namespace": "team/eng",
                "content": "Postgres autovacuum scale_factor cost_delay tuning bloat control notes.",
            }));
        }
        let envelope: FireEnvelope = serde_json::from_str(&make_envelope_post_store(&json!({
            "id": "mem-new",
            "namespace": "team/eng",
            "content": "Postgres autovacuum scale_factor cost_delay tuning bloat control notes.",
            "recent_namespace_memories": neighbours,
        })))
        .unwrap();
        let decision = build_decision(&envelope);
        let links = decision["delta"]["metadata"]["auto_related_links"]
            .as_array()
            .unwrap();
        assert!(
            links.len() <= MAX_PROPOSALS,
            "proposal cap must hold (got {})",
            links.len()
        );
    }

    #[test]
    fn proposals_preserve_existing_metadata_keys() {
        let envelope: FireEnvelope = serde_json::from_str(&make_envelope_post_store(&json!({
            "id": "mem-new",
            "namespace": "team/eng",
            "content": "Postgres autovacuum scale_factor and cost_delay tuning.",
            "metadata": { "governance": "unrestricted", "agent_id": "host:test" },
            "recent_namespace_memories": [
                {
                    "id": "mem-neighbour",
                    "namespace": "team/eng",
                    "content": "Postgres autovacuum scale_factor and cost_delay tuning.",
                }
            ],
        })))
        .unwrap();
        let decision = build_decision(&envelope);
        let metadata = &decision["delta"]["metadata"];
        assert_eq!(metadata["governance"], "unrestricted");
        assert_eq!(metadata["agent_id"], "host:test");
        assert!(metadata["auto_related_links"].is_array());
    }

    #[test]
    fn similarity_threshold_default_is_zero_point_four() {
        // Pin the documented threshold so a future tweak that
        // changes it touches this assertion.
        assert!((SIMILARITY_THRESHOLD - 0.4).abs() < 1e-9);
    }

    #[test]
    fn jaccard_pair_known_value() {
        let a: HashSet<String> = ["alpha", "beta", "gamma", "delta"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let b: HashSet<String> = ["beta", "gamma", "epsilon"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        // intersection = {beta, gamma} = 2; union = 5; jaccard = 0.4
        let v = jaccard(&a, &b);
        assert!((v - 0.4).abs() < 1e-9, "got {v}");
    }

    #[test]
    fn jaccard_empty_either_side_is_zero() {
        let empty: HashSet<String> = HashSet::new();
        let nonempty: HashSet<String> = ["x"].iter().map(|s| (*s).to_string()).collect();
        assert!((jaccard(&empty, &nonempty)).abs() < 1e-9);
        assert!((jaccard(&nonempty, &empty)).abs() < 1e-9);
        assert!((jaccard(&empty, &empty)).abs() < 1e-9);
    }

    #[test]
    fn token_bag_drops_stopwords_and_short_tokens() {
        let bag = token_bag("The quick brown fox is by the lazy dog.");
        assert!(bag.contains("quick"));
        assert!(bag.contains("brown"));
        assert!(bag.contains("lazy"));
        assert!(!bag.contains("the"));
        assert!(!bag.contains("by"));
        assert!(!bag.contains("is"));
    }

    #[test]
    fn run_once_round_trips_a_post_store_envelope() {
        let envelope = make_envelope_post_store(&json!({
            "id": "mem-new",
            "namespace": "team/eng",
            "content": "Postgres autovacuum scale_factor and cost_delay tuning.",
            "recent_namespace_memories": [
                {
                    "id": "mem-neighbour",
                    "namespace": "team/eng",
                    "content": "Postgres autovacuum scale_factor and cost_delay tuning notes.",
                }
            ],
        }));
        let mut out: Vec<u8> = Vec::new();
        run_once(envelope.as_bytes(), &mut out).expect("run_once ok");
        let line = String::from_utf8(out).unwrap();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["action"], "modify");
        assert!(v["delta"]["metadata"]["auto_related_links"].is_array());
    }

    #[test]
    fn run_daemon_processes_multiple_envelopes_then_eofs() {
        let envelope = make_envelope_post_store(&json!({
            "id": "mem-new",
            "namespace": "team/eng",
            "content": "Postgres autovacuum scale_factor and cost_delay tuning.",
            "recent_namespace_memories": [
                {
                    "id": "mem-neighbour",
                    "namespace": "team/eng",
                    "content": "Postgres autovacuum scale_factor and cost_delay tuning notes.",
                }
            ],
        }));
        let input = format!("{envelope}\n{envelope}\n");
        let mut out: Vec<u8> = Vec::new();
        run_daemon(input.as_bytes(), &mut out).expect("run_daemon ok");
        let lines: Vec<&str> = std::str::from_utf8(&out)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 2, "one decision per envelope");
        for l in lines {
            let v: Value = serde_json::from_str(l).unwrap();
            assert_eq!(v["action"], "modify");
        }
    }
}
