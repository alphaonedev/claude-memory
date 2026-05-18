// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// Bench/runner scaffolding — pedantic relaxations that carry no
// behavioural meaning. Each is justified at its declaration site.
#![allow(
    // Doc strings here describe scenario shapes and benchmark
    // mechanics; running them through clippy::doc_markdown adds noise
    // (e.g. complaints about `LLM`, `JSON`, `RFC3339`, `IDs`) without
    // catching anything load-bearing.
    clippy::doc_markdown,
    // Deterministic PRNG step uses `as u32` / `as usize` casts that
    // are part of the algorithm's wrap-around contract. The clipped
    // bits are intentional.
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    // Bench fixtures derive `Memory` values whose owned `String`
    // fields are unavoidable — the per-field clone is the cheapest
    // shape clippy will accept without churning the model surface.
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]

//! v0.7.0 Layer 3 Task L3-1 — LongMemEval-Reflection benchmark dataset.
//!
//! ## What this module produces
//!
//! Fifty deterministic synthetic scenarios. Each scenario carries:
//!
//!   * **20 observations** sharing a topical vocabulary tight enough to
//!     pass the curator's Jaccard-threshold clustering gate.
//!   * One **ground-truth depth-1 reflection** — a canonical
//!     pattern string keyed by scenario id, the substrate output a
//!     correct reflection pass MUST produce when summarising those 20
//!     observations.
//!   * One **ground-truth depth-2 reflection** — a canonical
//!     meta-pattern combining the depth-1 reflections from 2-3 sibling
//!     scenarios. This lets the runner exercise the recursive cap
//!     (`max_reflection_depth ≥ 2`) and confirm the substrate links
//!     depth-2 reflections back to the depth-1 sources via
//!     `reflects_on` edges.
//!
//! ## Determinism
//!
//! The dataset is generated from a single `u64` seed
//! (`L3_LME_REFLECTION_SEED`). A tiny inline xorshift64\* PRNG drives
//! topic + content selection; no external `rand` dep is pulled. The
//! materialised `data/scenarios.jsonl` snapshot in this directory is
//! the on-disk record an auditor replays via `--load-snapshot` to
//! confirm the in-memory generator agrees with the committed file
//! byte-for-byte.
//!
//! ## Why a synthetic dataset (not real LongMemEval)
//!
//! The real LongMemEval dataset measures **recall**, not reflection.
//! There's no public dataset with ground-truth multi-level reflection
//! patterns; the closest analogue is the L2-1 30-observation fixture
//! that the curator's reflection-pass unit tests already exercise.
//! L3-1 generalises that fixture to fifty independent scenarios so the
//! coverage / accuracy metrics have meaningful spread.

use ai_memory::models::ConfidenceSource;
use ai_memory::models::{Memory, MemoryKind, Tier};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// The single fixed seed that drives the entire dataset. Bumping this
/// value reshuffles every scenario; the on-disk snapshot
/// (`data/scenarios.jsonl`) MUST be re-materialised in lock-step
/// (`cargo bench --bench longmemeval_reflection -- --regenerate`) and
/// the snapshot hash re-recorded.
pub const L3_LME_REFLECTION_SEED: u64 = 0x4C33_4C4D_4552_4546; // "L3LMEREF"

/// Number of scenarios in the dataset. Spec ≥50 (issue #674); we hold
/// at exactly 50 so the throughput floor (≥10 scenarios/min) is
/// comparable across runs.
pub const SCENARIO_COUNT: usize = 50;

/// Observations per scenario. Spec = 20.
pub const OBSERVATIONS_PER_SCENARIO: usize = 20;

/// Twenty topical anchors. Each scenario rotates through this list
/// modulo `SCENARIO_COUNT`, so every anchor appears in 2–3 scenarios
/// (the depth-2 meta-pattern groupings hinge on this rotation).
const TOPICS: &[&str] = &[
    "kubernetes rolling deploy canary strategy",
    "rust async tokio runtime executor concurrency",
    "sqlite wal mode transaction durability fsync",
    "postgres logical replication slot wal lag",
    "redis cluster gossip failover quorum",
    "kafka consumer group rebalance offset commit",
    "tls handshake certificate pinning rotation",
    "oauth2 device code grant refresh token",
    "linux cgroup memory limit oom killer",
    "btrfs snapshot subvolume rollback compression",
    "neovim lua plugin lazy load startup",
    "tmux session detach reattach socket",
    "git rebase interactive squash fixup signoff",
    "github actions workflow matrix concurrency",
    "docker buildkit cache mount layer",
    "ansible playbook idempotent handler notify",
    "terraform state lock dynamodb backend",
    "prometheus scrape interval relabel rule",
    "grafana dashboard variable templating provisioning",
    "opentelemetry collector exporter pipeline batch",
];

/// Mid-content vocabulary fragments mixed into observations alongside
/// the topic anchor. Each scenario picks three fragments using the
/// PRNG; the surviving vocabulary keeps the Jaccard signal strong.
const FRAGMENTS: &[&str] = &[
    "we hit a regression on the staging cluster after the bump",
    "the operator runbook says to drain the node first",
    "this is the third incident of the same shape this month",
    "the SLA window does not allow a full rollback",
    "we added a smoke test before the gate to catch this earlier",
    "the alert fired at 03:14 UTC on the on-call rotation",
    "the post-mortem action item landed in the q4 backlog",
    "we confirmed the fix held through four deploy cycles",
    "the telemetry confirms the latency tail collapsed",
    "the new graph hasn't shown any anomalies in 72h",
];

/// One scenario's data — twenty observations + the two ground-truth
/// reflections. `serde_json::to_string(&s)?` is the JSONL row written
/// to `data/scenarios.jsonl`.
// `Memory` (from `ai_memory::models`) doesn't impl `PartialEq` / `Eq`
// — the substrate's identity is the database row, not the struct.
// Derive only what serde needs; equality checks in tests use the
// individual fields we care about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    /// Stable scenario id, `"l3-lme-refl-<00..49>"`. The runner uses
    /// this both as the namespace and as the lookup key for the
    /// canonical depth-1 / depth-2 reflection patterns.
    pub id: String,
    /// Topic anchor string. Twenty rows share this vocabulary.
    pub topic: String,
    /// `OBSERVATIONS_PER_SCENARIO` observation memories.
    pub observations: Vec<Memory>,
    /// Canonical depth-1 pattern the LLM stub returns for this
    /// scenario. A correct reflection MUST surface this string (or
    /// a strict superset of its token bag — the judge compares
    /// token-set inclusion ≥ 0.8 Jaccard).
    pub ground_truth_depth_1: String,
    /// Canonical depth-2 meta-pattern. A correct depth-2 reflection
    /// MUST surface this string when the curator clusters two
    /// sibling depth-1 reflections from `siblings`.
    pub ground_truth_depth_2: String,
    /// Sibling scenario ids whose depth-1 reflections combine into
    /// the depth-2 meta-pattern. Length 1 or 2; the runner uses
    /// `len == 2` scenarios for the depth-2 evaluation pass.
    pub siblings: Vec<String>,
}

/// Build the full 50-scenario dataset deterministically from
/// [`L3_LME_REFLECTION_SEED`]. Same seed → same scenarios bit-for-bit.
#[must_use]
pub fn generate_scenarios() -> Vec<Scenario> {
    let mut rng = XorShift64::new(L3_LME_REFLECTION_SEED);
    let mut scenarios = Vec::with_capacity(SCENARIO_COUNT);
    // Deterministic baseline timestamp — FROZEN. `Utc::now()` would
    // make `data/scenarios.jsonl` re-materialise to a different byte
    // sequence on every run and break the audit-replay contract. We
    // anchor at 2026-01-01T00:00:00Z (after the v0.7.0 cutover and
    // before any scheduled v0.8.0 work) and step observations
    // backwards by one minute each. The curator's TEMPORAL_WINDOW
    // gate measures relative deltas, so a frozen anchor is fine.
    let base: DateTime<Utc> = Utc
        .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
        .single()
        .expect("2026-01-01T00:00:00Z is a valid instant");

    for i in 0..SCENARIO_COUNT {
        let topic = TOPICS[i % TOPICS.len()];
        let scenario_id = format!("l3-lme-refl-{i:02}");
        let namespace = scenario_id.clone();

        let mut observations = Vec::with_capacity(OBSERVATIONS_PER_SCENARIO);
        for j in 0..OBSERVATIONS_PER_SCENARIO {
            // Pull three fragments per observation; rotation by `j`
            // keeps each row's content unique enough for the (title,
            // namespace) unique key.
            let f1 = FRAGMENTS[rng.gen_range(FRAGMENTS.len())];
            let f2 = FRAGMENTS[rng.gen_range(FRAGMENTS.len())];
            let f3 = FRAGMENTS[rng.gen_range(FRAGMENTS.len())];
            let content = format!("{topic} {topic} {topic} {f1}. {f2}. {f3}. observation #{j}");
            // Per-row timestamp = base − j minutes; deterministic
            // ordering and tight temporal window.
            // `j` is bounded by OBSERVATIONS_PER_SCENARIO (= 20); the
            // i64 cast cannot wrap. We use `i64::try_from` regardless
            // so clippy::cast_possible_wrap is satisfied without an
            // attribute on the call site.
            let j_i64 = i64::try_from(j).expect("OBSERVATIONS_PER_SCENARIO fits in i64");
            let ts = (base - Duration::minutes(j_i64)).to_rfc3339();
            observations.push(Memory {
                // Deterministic id derived from scenario + j so a
                // re-materialised snapshot is byte-identical.
                id: format!("{scenario_id}-obs-{j:02}"),
                tier: Tier::Long,
                namespace: namespace.clone(),
                title: format!("{topic} note #{j} ({scenario_id})"),
                content,
                tags: vec!["bench".to_string(), "l3-lme-refl".to_string()],
                priority: 5,
                confidence: 1.0,
                // The substrate validator only enforces `VALID_SOURCES`
                // on the reflect / link write paths (`src/validate.rs`).
                // `db::insert` is lenient — `"system"` is the cleanest
                // canonical match for synthesised bench fixtures.
                source: "system".to_string(),
                // access_count >= MIN_RECALL_COUNT so the curator's
                // clusterable-observation gate (access_count >= 1)
                // accepts every row.
                access_count: 2,
                created_at: ts.clone(),
                updated_at: ts,
                last_accessed_at: None,
                expires_at: None,
                metadata: serde_json::json!({
                    "agent_id": "bench-l3-lme-refl",
                    "scenario_id": scenario_id,
                }),
                reflection_depth: 0,
                memory_kind: MemoryKind::Observation,
                entity_id: None,
                persona_version: None,
                citations: Vec::new(),
                source_uri: None,
                source_span: None,
                confidence_source: ConfidenceSource::CallerProvided,
                confidence_signals: None,
                confidence_decayed_at: None,
                version: 1,
            });
        }

        // Depth-1 pattern: canonical sentence keyed by topic. Built
        // from the topic anchor alone so the LLM stub's output (which
        // re-uses the same template) matches exactly.
        let ground_truth_depth_1 = format!(
            "pattern: recurring {topic} observations across {OBSERVATIONS_PER_SCENARIO} entries"
        );

        // Depth-2 sibling grouping: pair scenarios `i` and `i+1`
        // when `i` is even. Odd `i` lands as the trailing member of
        // the previous pair (`siblings = [i-1]`).
        let siblings = if i % 2 == 0 && i + 1 < SCENARIO_COUNT {
            vec![format!("l3-lme-refl-{:02}", i + 1)]
        } else if i % 2 == 1 {
            vec![format!("l3-lme-refl-{:02}", i - 1)]
        } else {
            vec![]
        };

        // Depth-2 pattern: meta-summary combining the two sibling
        // topics. The runner feeds the two depth-1 reflections as
        // sources; the LLM stub returns this string.
        let sibling_topic = siblings.first().map_or(topic, |s| {
            let sib_idx: usize = s.trim_start_matches("l3-lme-refl-").parse().unwrap_or(0);
            TOPICS[sib_idx % TOPICS.len()]
        });
        let ground_truth_depth_2 =
            format!("meta-pattern: cross-domain recurrence between {topic} and {sibling_topic}");

        scenarios.push(Scenario {
            id: scenario_id,
            topic: topic.to_string(),
            observations,
            ground_truth_depth_1,
            ground_truth_depth_2,
            siblings,
        });
    }

    scenarios
}

/// Serialise the dataset as JSONL (one scenario per line). Round-trips
/// through [`load_jsonl`]; the materialised snapshot in
/// `data/scenarios.jsonl` is `serialise_jsonl(&generate_scenarios())`.
#[must_use]
pub fn serialise_jsonl(scenarios: &[Scenario]) -> String {
    let mut out = String::new();
    for s in scenarios {
        let line = serde_json::to_string(s).expect("scenario serialises");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Parse the JSONL snapshot back into scenarios. Used by the runner
/// when `--load-snapshot` is passed (audit path).
pub fn load_jsonl(jsonl: &str) -> anyhow::Result<Vec<Scenario>> {
    let mut out = Vec::new();
    for (i, line) in jsonl.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let s: Scenario = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("scenarios.jsonl line {}: {}", i + 1, e))?;
        out.push(s);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tiny deterministic PRNG (xorshift64*).
//
// Inlined so the bench has zero external rng dep. Same algorithm
// Marsaglia 2003; sufficient for fixture generation (NOT for crypto).
// ---------------------------------------------------------------------------

struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        // Guard against the all-zero state which xorshift cannot
        // escape; the constant matches Marsaglia's original suggestion.
        let state = if seed == 0 {
            0xDEAD_BEEF_CAFE_F00D
        } else {
            seed
        };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn gen_range(&mut self, upper: usize) -> usize {
        // upper > 0 by construction (callers index into non-empty
        // arrays). usize cast is safe — upper ≤ FRAGMENTS.len() = 10.
        (self.next_u64() as usize) % upper
    }
}

// Unit tests live in `tests/longmemeval_reflection_bench.rs` so they
// exercise the public surface and run under the standard `cargo test`
// gate. `cargo bench --bench longmemeval_reflection` has
// `harness = false` and does not run `#[cfg(test)]` blocks in this
// file, so co-locating tests here would silently drop them from CI.
