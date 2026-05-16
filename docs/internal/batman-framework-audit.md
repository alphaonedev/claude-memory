# ai-memory vs Batman's 6-Form Write-Time-Investment Framework — Honest Audit

> ## Post-audit status (2026-05-15, HEAD `c9472c1`)
>
> **This document reflects state at commit `53b4d39` (audit baseline).**
> The 0-of-6-IMPLEMENTED + 4-partial + 2-absent findings (and the
> PARTIAL grade for the 7th-form claim) **drove the Forms 1-6 + 7th-form
> closeout wave** — PRs [#761](https://github.com/alphaonedev/ai-memory-mcp/pull/761),
> [#762](https://github.com/alphaonedev/ai-memory-mcp/pull/762),
> [#763](https://github.com/alphaonedev/ai-memory-mcp/pull/763),
> [#764](https://github.com/alphaonedev/ai-memory-mcp/pull/764),
> [#765](https://github.com/alphaonedev/ai-memory-mcp/pull/765),
> [#766](https://github.com/alphaonedev/ai-memory-mcp/pull/766) — all
> merged 2026-05-15.
>
> **Post-closeout state at HEAD `c9472c1`: all 7 forms IMPLEMENTED.**
>
> **Post-closeout state at HEAD `d17725c` (2026-05-15, ship-readiness
> sweep): all 7 forms remain IMPLEMENTED with hardened acceptance
> suites.** See [§"POST-CLOSEOUT STATE (2026-05-15)"](#post-closeout-state-2026-05-15)
> at the end of this document for the per-form ship-readiness audit
> trail (Cluster A-K fix PRs) and the v0.7.0 ship-readiness initiative
> ([#767](https://github.com/alphaonedev/ai-memory-mcp/issues/767))
> rollup.
>
> - Form 1 — `src/synthesis/mod.rs` + `tests/form_1_synthesis.rs` (issue [#754](https://github.com/alphaonedev/ai-memory-mcp/issues/754)).
> - Form 2 — `src/atomisation/mod.rs` + `Synchronous` mode branch in `src/hooks/pre_store/auto_atomise.rs` + `tests/form_2_synchronous_atomise.rs` (issue [#755](https://github.com/alphaonedev/ai-memory-mcp/issues/755)).
> - Form 3 — `src/multistep_ingest/{mod,executor,helpers,pipeline,cache}.rs` + `tests/form_3_multistep_ingest.rs` (issue [#756](https://github.com/alphaonedev/ai-memory-mcp/issues/756)).
> - Form 4 — `migrations/sqlite/0032_v07_form4_provenance.sql` + `migrations/postgres/0019_v07_form4_provenance.sql` + `tests/form_4_provenance.rs` (issue [#757](https://github.com/alphaonedev/ai-memory-mcp/issues/757)).
> - Form 5 — `src/confidence/{mod,calibrate,shadow,decay}.rs` + `tests/form_5_confidence_calibration.rs` (issue [#758](https://github.com/alphaonedev/ai-memory-mcp/issues/758)).
> - Form 6 — `src/models/memory.rs:38-200` (10-variant `MemoryKind` enum) + `tests/form_6_memorykind_vocab.rs` (issue [#759](https://github.com/alphaonedev/ai-memory-mcp/issues/759)).
> - 7th-form (Option-B foundation) — `src/governance/{mod,agent_action,deferred_audit,rules_store,wire_point}.rs` + `tests/form_7_agent_external_wiring.rs` (issue [#760](https://github.com/alphaonedev/ai-memory-mcp/issues/760)). The agent-EXTERNAL Layer-4 surface (Bash / FilesystemWrite / NetworkRequest / ProcessSpawn) is `callable_now=false` at the substrate boundary; v0.8.0 wires it 100% per [#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697).
>
> The audit body below is preserved verbatim as the historical record
> that drove the wave. For the canonical post-wave feature inventory,
> see [`docs/internal/v070-feature-inventory.md`](v070-feature-inventory.md).

---

> Audit date: 2026-05-15
> ai-memory commit hash audited: `53b4d399db7bf8c08700ba9ea860f784ef3d2baa`
> Branches inspected: `audit/batman-6-form` (off `feat/v0.7.0-grand-slam` which subsumes `wt-1-integration` after the 2026-05-15 cascade); cross-referenced against `main`.
> Batman article reference: "Pay at write time, read for free" — six write-time-investment forms (online dedup-and-synthesis, atomisation, multi-step ingest, provenance metadata, confidence scoring, type tagging).
> Auditor: AI NHI dev team session (Claude Opus 4.7 1M context)
> Methodology: adversarial code-evidence verification per the 4-step protocol. Read-only on source code. Classifications biased LOWER on uncertainty (Rule 6). No reliance on Strategic Nugget #014 / planning docs (Rule 3).

## Executive summary

> **Post-closeout (2026-05-15, HEAD `d17725c`):** all 7 forms are
> IMPLEMENTED at substrate-evidence level. See
> [§"POST-CLOSEOUT STATE (2026-05-15)"](#post-closeout-state-2026-05-15)
> at the end of this document for the per-form ship-readiness audit
> trail (Cluster A-K fix PRs against the original audit findings) and
> the v0.7.0 ship-readiness initiative ([#767](https://github.com/alphaonedev/ai-memory-mcp/issues/767))
> rollup. The audit body below is preserved verbatim as the historical
> record that drove the wave.

ai-memory v0.7.0 (commit `53b4d39`) implements **0 of Batman's 6 forms cleanly (IMPLEMENTED)**, **4 partially (Forms 2, 4, 5, 6)**, and **2 absent (Forms 1, 3)**. The 7th-form claim (substrate-authority enforcement at write time via K9/Layer-4) is **PARTIAL** — substrate-INTERNAL write-path enforcement (`memory_store`/`memory_link`/`memory_delete`/`memory_archive`/`memory_consolidate`/`memory_replay`) is wired and substrate-authoritative; agent-EXTERNAL enforcement (Bash/FilesystemWrite/NetworkRequest/ProcessSpawn) is `callable but un-wired` (`src/governance/agent_action.rs:38-42`). The X-post claim "ships 5 of Batman's 6 forms plus a 7th" materially overcounts: code-evidence supports zero clean implementations and partial coverage at best on the four forms ai-memory shares vocabulary with.

Escalation trigger 1 fires (≤2 forms IMPLEMENTED). Recommend correcting the public claim before v0.7.0 launch and either shipping Forms 2, 4, 6 to clean IMPLEMENTED grade in v0.7.x or rephrasing the X post around what is actually present (substrate-authoritative governance pipeline + structural atomisation engine + memory-grain provenance).

## Form-by-form findings

### FORM 1 — Online dedup-and-synthesis

Files inspected:
  - `/Users/fate/v07/audit-batman/src/mcp/tools/store.rs`
  - `/Users/fate/v07/audit-batman/src/handlers/http.rs`
  - `/Users/fate/v07/audit-batman/src/llm.rs`
  - `/Users/fate/v07/audit-batman/src/autonomy.rs`
  - `/Users/fate/v07/audit-batman/src/storage/mod.rs` (`find_contradictions`)
  - `/Users/fate/v07/audit-batman/src/mcp/tools/check_duplicate.rs`
  - `/Users/fate/v07/audit-batman/src/mcp/tools/detect_contradiction.rs`

Code evidence (the MCP store post-store hook, `src/mcp/tools/store.rs:524-605`):
```
// v0.6.0.0 post-store autonomy hooks. When enabled via
// `AI_MEMORY_AUTONOMOUS_HOOKS=1` or `autonomous_hooks = true` in
// config.toml AND an LLM is wired AND the content is long enough
// to be meaningfully taggable, fire `auto_tag` + `detect_contradiction`
// synchronously and persist the results into the memory's metadata.
...
for cand in &existing {
    ...
    match llm_client.detect_contradiction(&mem.content, &cand.content) {
        Ok(true) => confirmed_contradictions.push(cand.id.clone()),
        ...
    }
}
```
And the contradiction prompt itself (`src/llm.rs:40-43`):
```
const CONTRADICTION_PROMPT: &str = r#"Do these two statements contradict each other? Answer ONLY "yes" or "no".

Statement A: {a}
Statement B: {b}"#;
```
HTTP-path equivalent `maybe_detect_conflicts` (`src/handlers/http.rs:197-266`) is `#[allow(dead_code)]` and has no call site (round-2 comment at line 192: "the function is staged for the next round").

Behavioral assessment:
On a store with `autonomous_hooks=true`, ai-memory does an FTS-based same-namespace fuzzy-title prefilter (`db::find_contradictions`, `src/storage/mod.rs:1663-1678`), then ALWAYS inserts the new row, then ISSUES PER-PAIR BINARY YES/NO LLM CALLS against each candidate, then writes a `confirmed_contradictions` metadata array as a separate `db::update`. The new row is committed BEFORE the LLM round-trips run. Nothing is updated/deleted/merged based on a contradiction verdict — the field is metadata only. There is no single batch LLM call emitting per-fact actions (add/update/delete/no-op); the per-pair binary classifier is structurally different from SimpleMem-style action-emitting prompts. `autonomy.rs` runs a background consolidation pass (Jaccard + cosine clustering + LLM summarise) on a scheduled curator cycle — that is the Rule 5 adjacent-functionality case, not online write-time dedup. The HTTP path's `maybe_detect_conflicts` is staged dead code.

Divergence from Batman's criterion:
1. No single batch LLM call; ai-memory does N per-pair yes/no calls.
2. No action verbs (add/update/delete/no-op); the LLM produces a boolean per pair.
3. The write is not gated by the verdict — insert lands first; metadata records the verdict after.
4. The "merge" decision is left to the operator via a follow-up `memory_consolidate` MCP tool.

Adversarial check:
1. Would Batman recognise this as the form he described? **No.** Per-pair binary yes/no is not action-emitting; the row is already committed when the verdict lands.
2. Would SimpleMem / mem9 recognise the architectural pattern? **No.** Those issue one batch LLM call per write with structured per-fact actions; ai-memory's prompt is yes/no.
3. Is the divergence structural? **Yes.** The shape of the LLM contract is different; the placement relative to the SQL write is different; the action vocabulary is absent.

Classification: **ABSENT**
Confidence: **HIGH**

Gap analysis:
- Missing: single batch action-emitting LLM call evaluated BEFORE the SQL write, with prompt vocabulary `{add, update, delete, no_op}` per existing-candidate, and write-path gating on the verdict.
- LOE: 2-3 NHI sessions to design the action-emitting prompt + parser, refactor `handle_store` to call into a pre-write synthesis path, refactor the dedup branch to honour the LLM verdict (not just the `find_contradictions` FTS match), and ship telemetry that compares the new vs old code path under shadow mode.
- Recommended release: v0.8.0. Too large a write-path refactor for a v0.7.x patch.
- Procurement-grade implication: the most quotable Batman form is the one ai-memory is structurally furthest from. Public claim should not say "ships Form 1"; honest framing is "ships per-pair LLM contradiction detection + scheduled background consolidation; v0.8.0 unifies into the SimpleMem-style action-emitting pattern".

---

### FORM 2 — Atomisation (decompose-before-embedding)

Files inspected:
  - `/Users/fate/v07/audit-batman/src/atomisation/mod.rs`
  - `/Users/fate/v07/audit-batman/src/atomisation/curator.rs`
  - `/Users/fate/v07/audit-batman/src/hooks/pre_store/auto_atomise.rs`
  - `/Users/fate/v07/audit-batman/src/mcp/tools/store.rs` (lines 505-642)
  - `/Users/fate/v07/audit-batman/src/mcp/tools/atomise.rs`

Code evidence (the atomiser entry point, `src/atomisation/mod.rs:248-307`):
```
pub async fn atomise(
    &self, conn: &Connection, source_id: &str, max_atom_tokens: u32, force: bool, calling_agent_id: &str,
) -> Result<AtomiseResult, AtomiseError> {
    self.atomise_sync(conn, source_id, max_atom_tokens, force, calling_agent_id)
}
...
// Step 1 — load source memory.
let source = db::get(conn, source_id)... .ok_or(AtomiseError::NotFound)?;
```
And the actual sequence in the store path (`src/mcp/tools/store.rs:505-642`):
```
// (line 505) Generate and store embedding if embedder is available
if let Some(emb) = embedder {
    let text = format!("{} {}", mem.title, mem.content);
    match emb.embed(&text) {
        Ok(embedding) => {
            if let Err(e) = db::set_embedding(conn, &actual_id, &embedding) ...
...
// (line 619) v0.7.0 WT-1-D — auto-atomisation pre_store substrate hook.
{
    let _outcome = crate::hooks::pre_store::maybe_enqueue_auto_atomise(&post_mem, &agent_id);
}
```
And the auto-atomise hook itself (`src/hooks/pre_store/auto_atomise.rs:14-30`):
```
//! # Hard guarantees
//!
//! 1. **Non-blocking.** The hook returns synchronously after at most
//!    a token-count + policy resolution. The curator round-trip runs
//!    on a detached `std::thread::spawn`. The `memory_store` latency
```

Behavioral assessment:
The atomisation engine exists, is well-factored, and produces structurally-correct atomic propositions via a curator LLM call. However, the substrate's auto-atomise hook fires AFTER the source memory has been inserted AND embedded (`mem.title + mem.content` is embedded as one blob at line 505-522), AND the atomiser itself loads `db::get(conn, source_id)` and writes atoms as NEW rows. Atoms are first-class memories with their own embedding pass (triggered by the per-atom `db::insert` going through the same path). The decomposition is real, but it happens POST-embed-of-source, not before. The retrieval surface (WT-1-E atom-preference WHERE) does surface atoms in place of the source, so the FUNCTIONAL benefit Batman describes (atomic propositions are the indexed unit) IS present — but the ORDER is different.

Divergence from Batman's criterion:
1. Source memory is embedded as one document before decomposition runs.
2. Atomisation is best-effort deferred (worker thread, fire-and-forget) — not part of the write transaction.
3. Atoms may not exist for hours after the source row is searchable (or never if the curator is down). The `atom_of` index and `atomised_into` column let the retrieval path prefer atoms when present, falling back to the source.
4. Manual atomisation via `memory_atomise` MCP tool DOES run before the atom's embeddings, but the source is already embedded by that point.

Adversarial check:
1. Would Batman recognise this as Form 2? **Partially.** The decomposition exists and produces atoms; but Batman's exemplar (LLM-Wiki two-step ingest) decomposes BEFORE the index entry exists, and ai-memory always embeds the source first.
2. Would LLM-Wiki recognise the architectural pattern? **Partially.** The atom retrieval path is recognisable; the order of writes is not.
3. Structural? **Yes.** The source-embed-first / atomise-later flow is a deliberate non-blocking design choice that diverges from "decompose THEN embed".

Classification: **PARTIAL**
Confidence: **HIGH**

Gap analysis:
- Missing: synchronous decompose-before-embed mode on the store path. The non-blocking hook is intentional design; closing the gap means adding an OPT-IN synchronous mode (`auto_atomise = "synchronous"` namespace policy) that defers source-embedding until after atom-emission, then either skips the source embed entirely or down-weights it.
- LOE: 1-2 NHI sessions for the synchronous mode + policy plumbing; the atomiser core is already in place.
- Recommended release: v0.7.x patch (low blast radius, opt-in via policy).
- Procurement-grade implication: this is the closest-to-clean form ai-memory ships. Honest framing: "structural atomiser landed; synchronous-mode opt-in scheduled for v0.7.x to match Batman's decompose-before-embed exactly".

---

### FORM 3 — Multi-step ingest (deterministic + LLM with prompt-cache reuse)

Files inspected:
  - `/Users/fate/v07/audit-batman/src/llm.rs` (every prompt constant)
  - `/Users/fate/v07/audit-batman/src/atomisation/curator.rs`
  - `/Users/fate/v07/audit-batman/src/mcp/tools/store.rs`
  - `/Users/fate/v07/audit-batman/src/handlers/http.rs`
  - searched the entire `src/` tree for `prompt_cache|cache_prompt|cached_tokens|TRUST|two-phase|two_phase|multi-step|multi_step|deterministic.*helper` — no matches outside test/doc comments

Code evidence: no code matching criterion found. ai-memory's LLM-using paths (`auto_tag`, `detect_contradiction`, `summarize_memories`, `query_expansion`, atomisation `decompose`) each issue independent prompts with no shared cache token, no "trust pre-computed deterministic helper" instruction, and no orchestrated multi-step pipeline. Each is single-shot.

Behavioral assessment:
ai-memory's LLM usage is one-shot-per-purpose. The closest thing to "multi-step" is the autonomy curator pass (`autonomy.rs`) which does deterministic Jaccard pre-filtering and then a cosine-similarity numeric check before calling the LLM `summarize_memories` — but the LLM call is not told to TRUST the pre-computed signals; it simply takes the cluster's contents and returns a summary. No prompt-cache reuse anywhere; the OllamaClient uses a fresh prompt per call. No shared prefix is exploited.

Divergence from Batman's criterion:
1. No multi-stage LLM pipeline where stage-N is told what stage-N-1 produced and instructed to trust it.
2. No prompt-cache reuse / shared prefix caching.
3. Deterministic helpers exist (Jaccard, cosine, FTS, validators) but they gate or filter — they don't feed structured context into a downstream LLM call as TRUSTED input.

Adversarial check:
1. Would Batman recognise this? **No.** None of ai-memory's LLM call sites match the Understand-Anything / OpenKB multi-step deterministic-then-LLM pattern.
2. Would OpenKB recognise it? **No.**
3. Structural? **Yes.** ai-memory has no architectural notion of multi-stage LLM with explicit-trust instructions.

Classification: **ABSENT**
Confidence: **HIGH**

Gap analysis:
- Missing: multi-step ingest orchestrator, prompt-cache reuse strategy, explicit-trust instructions in downstream prompts.
- LOE: 3-4 NHI sessions — this is a new subsystem; it needs design before code.
- Recommended release: v0.9.0. Strategic primitive, not a v0.7/v0.8 spit-and-polish item.
- Procurement-grade implication: the X post's "5 of 6" claim is plausible-on-paper here only if a reader confuses "uses LLM in multiple places across the codebase" with "multi-step ingest". They are different. Honest framing: ABSENT in v0.7.0; under design for v0.9.0.

---

### FORM 4 — Provenance metadata (fact-level: source + capture_timestamp + confidence + citations)

Files inspected:
  - `/Users/fate/v07/audit-batman/src/models/memory.rs` (the `Memory` struct, lines 129-185)
  - `/Users/fate/v07/audit-batman/src/signed_events.rs`
  - `/Users/fate/v07/audit-batman/src/audit.rs`

Code evidence (the Memory model, `src/models/memory.rs:129-185`):
```
pub struct Memory {
    pub id: String,
    pub tier: Tier,
    pub namespace: String,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub priority: i32,
    pub confidence: f64,                   // 0.0-1.0 — how certain is this memory
    pub source: String,                    // "user", "claude", "hook", "api", "import"
    pub access_count: i64,
    pub created_at: String,
    pub updated_at: String,
    ...
    pub metadata: Value,                   // freeform; agent_id, scope, governance, etc.
    pub reflection_depth: i32,
    pub memory_kind: MemoryKind,           // Observation | Reflection | Persona
    pub entity_id: Option<String>,         // QW-2 persona only
    pub persona_version: Option<i32>,
}
```
And the action-provenance distinction in `src/signed_events.rs:5-15`:
```
//! Each identity-bearing write (today: every `memory_link` insert
//! through `db::create_link` / `db::create_link_signed`) appends one
//! row to `signed_events` so a downstream auditor can replay the
//! exact sequence of attestation events the daemon emitted...
```

Behavioral assessment:
ai-memory captures `source` (origin role), `created_at` (capture timestamp), and `confidence` (operator-provided 0.0-1.0) on every memory. It does NOT capture `citations` (no field, no metadata convention). `signed_events` is action-provenance (who did this write, when, what was the canonical hash, signature) — not fact-provenance (where did this fact come from). Batman's Form 4 calls for fact-provenance; ai-memory has 3 of 4 fact-grain fields (source, capture_timestamp, confidence) plus deep action-provenance via signed_events. These are complementary but not the same; the audit must distinguish them per the brief.

Divergence from Batman's criterion:
1. No structured `citations` field. Operators can stuff citations into `metadata` as freeform JSON, but there is no first-class shape, no validator, no recall surface filter.
2. `source` is a low-cardinality role label ("user", "claude", "hook", "api", "import"), not a URL/source-id pointing to the canonical document the fact came from. Hindsight-style "this fact came from URL X at timestamp T" is not directly representable.
3. Atoms inherit source-level provenance, not finer-grained "this atom came from sentence 47 of source memory M". The `atom_source_id` metadata key surfaces the parent memory id, which is closer to Batman's intent than the source role alone — but the atom-grain richer provenance (offset within source, original speaker, etc.) is not there.

Adversarial check:
1. Would Hindsight recognise this? **Partially.** 3-of-4 fact-grain fields cover source+timestamp+confidence; citations and source-as-URL are missing.
2. Would Supermemory recognise this? **Partially.** The 3-tier provenance idea is recognisable, but ai-memory's `source` is role-label, not the per-fact lineage Supermemory exposes.
3. Structural? **Partial divergence.** The fields exist but the semantics differ.

Classification: **PARTIAL**
Confidence: **HIGH**

Gap analysis:
- Missing: first-class `citations: Vec<Citation>` field (Citation = {url, accessed_at, hash, span?}); `source` semantic upgrade to allow URL/document-id values; per-atom span offset into source body.
- LOE: 1 NHI session for the citations field + validator + recall filter; 1 more for source-as-URL semantic upgrade + migration; 1 more for atom-grain span.
- Recommended release: citations in v0.7.x (low-risk additive metadata + validator); source-as-URL in v0.8.0 (semantic change, needs migration); atom-grain span in v0.8.0.
- Procurement-grade implication: ai-memory is closer to Form 4 than to any other. Honest framing: "memory-grain provenance fields (source, capture_timestamp, confidence) ship; citations field + atom-grain span scheduled for v0.7.x and v0.8.0".

---

### FORM 5 — Confidence scoring (with shadow-mode calibration)

Files inspected:
  - `/Users/fate/v07/audit-batman/src/models/memory.rs` (`confidence` field, lines 139, 229-230, 278-280)
  - `/Users/fate/v07/audit-batman/src/storage/mod.rs` (insert path, conf preservation)
  - searched the entire `src/` tree for `shadow|calibration|shadow_mode|shadow-mode` — only matches are unrelated (`unknown tier ... does NOT shadow it` in config.rs, `pre-computed calibration fields` in capabilities.rs referring to recall-mode telemetry, not confidence)

Code evidence (default value, `src/models/memory.rs:278-280`):
```
fn default_confidence() -> f64 {
    1.0
}
```
And the field on the struct (`src/models/memory.rs:138-139`):
```
/// 0.0-1.0 — how certain is this memory
pub confidence: f64,
```

Behavioral assessment:
ai-memory has a `confidence: f64` field on every memory, settable at write time via `CreateMemory.confidence` (defaulting to `1.0` when the caller omits it). The recall pipeline uses confidence as one of the FTS rank components (`db.rs` recall scoring: `fts.rank + priority*0.5 + access_count*0.1 + confidence*2.0 + tier_bonus + recency_factor` per `CLAUDE.md`). There is NO automatic confidence assignment — every caller-supplied confidence is taken at face value. There is NO shadow-mode (`AI_MEMORY_CONFIDENCE_SHADOW=1` does not exist; no telemetry channel ships confidence distributions for later threshold-calibration). There is NO freshness-decay model (Hindsight-style); confidence is set once at capture and never recomputed.

Divergence from Batman's criterion:
1. No automatic trust signal — the confidence number is whatever the caller put on the request.
2. No shadow-mode telemetry capture (ship dark, collect, decide threshold from data).
3. No calibration mechanism (no per-source baselines, no decay function).
4. The field exists and is honoured in recall ranking, but is not "calibrated via shadow-mode" as Batman's criterion requires.

Adversarial check:
1. Would Hindsight (freshness lifecycle) recognise this? **No.** Hindsight derives freshness from age + access patterns; ai-memory's confidence is a static caller-supplied number.
2. Would Supermemory (relative version distance) recognise this? **No.**
3. Structural? **Yes.** The shadow-mode dimension is entirely missing.

Classification: **PARTIAL** (the field exists and is consumed by recall; no calibration / shadow-mode)
Confidence: **HIGH**

Gap analysis:
- Missing: automatic confidence assignment from source/atom-derivation/age signals; shadow-mode telemetry to collect distributions; calibration tooling.
- LOE: 2-3 NHI sessions — needs a design pass on what the auto-confidence formula should be, then telemetry + sampling, then a calibration CLI.
- Recommended release: v0.8.0. Calibration is a non-trivial primitive.
- Procurement-grade implication: ai-memory's confidence field is a slot Batman would recognise — but the calibration story is the load-bearing half of Form 5 and that half is absent. Honest framing: "confidence field ships and is consumed by recall ranking; calibration / shadow-mode scheduled for v0.8.0".

---

### FORM 6 — Type tagging (semantic kind per atom)

Files inspected:
  - `/Users/fate/v07/audit-batman/src/models/memory.rs` (`MemoryKind` enum, lines 24-69)
  - `/Users/fate/v07/audit-batman/src/models/link.rs` (`MemoryLinkRelation`, lines 88-110)
  - `/Users/fate/v07/audit-batman/src/models/namespace.rs` (namespace policy — distinct from type tagging)

Code evidence (the `MemoryKind` enum, `src/models/memory.rs:24-38`):
```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// Default — a direct observation or note from the caller.
    Observation,
    /// A memory synthesised by the reflection pass over lower-depth peers...
    Reflection,
    /// v0.7.0 QW-2 — Persona-as-artifact...
    Persona,
}
```

Behavioral assessment:
Every memory carries a `memory_kind` discriminator stored in the `memories.memory_kind` SQL column (schema v30). The enum has three variants: `Observation` (default; user notes), `Reflection` (synthesised by the recursive-learning pass), `Persona` (QW-2 entity profiles). Readers can filter on `memory_kind` (it has a column index). Distinct from namespace policy (which is a structural constraint at write time) and distinct from schema validation (which is write-time content validation). Batman's exemplar (Tolaria frontmatter-as-type) categorises atoms into broader vocabulary (`concept` / `entity` / `claim` / `relation` / `event` / `conversation` or similar); ai-memory's 3-variant enum is much narrower and is keyed to ai-memory's own lifecycle (observation → reflection → persona), not to Batman's downstream-reader-filtering use case.

`MemoryLinkRelation` carries a richer vocabulary (`related_to` / `supersedes` / `contradicts` / `derived_from` / `reflects_on` / `derives_from`) but these tag RELATIONSHIPS between memories, not the memory itself.

Divergence from Batman's criterion:
1. Vocabulary is narrower (3 variants vs Batman's 6+ exemplar categories).
2. The taxonomy is ai-memory-lifecycle-shaped (where in the curation flow the memory was created), not downstream-reader-filter-shaped (what kind of fact does this represent).
3. No `entity_type` or `claim_kind` or `event_kind` enrichment on the variant — a Persona-kind memory carries `entity_id` and `persona_version`, but a Reflection-kind memory has no further type structure.

Adversarial check:
1. Would Tolaria recognise this? **Partially.** The slot is there; the vocabulary is much narrower.
2. Would a downstream reader use `memory_kind` to filter by Batman's intent? **Partially.** A reader can filter `Observation` vs `Reflection`, but cannot ask for "all claims" or "all events" — those vocabulary tags don't exist.
3. Structural? **Yes.** The vocabulary mismatch is fundamental, not surface.

Classification: **PARTIAL**
Confidence: **HIGH**

Gap analysis:
- Missing: broader vocabulary (claim, relation, event, conversation, decision) on `MemoryKind`; per-variant enrichment fields; recall surface that filters by these tags.
- LOE: 1 NHI session to extend the enum (additive, backward-compat via `#[serde(default)]`); 1 more for the recall filter surface; the per-variant enrichment is per-tag scope and harder to size in aggregate.
- Recommended release: v0.7.x or v0.8.0. Additive enum extension is low-risk; the L1-6 roadmap already reserves Goal/Plan/Step/Decision variants for v0.8.0.
- Procurement-grade implication: ai-memory has the slot but not the vocabulary. Honest framing: "MemoryKind enum exists with 3 variants (Observation/Reflection/Persona); Batman-shaped vocabulary (claim/relation/event/decision) scheduled for v0.8.0 L1-6".

---

## Forms ai-memory implements that Batman doesn't describe

### 7th-form claim — substrate-authority enforcement at write time

Files inspected:
  - `/Users/fate/v07/audit-batman/src/governance/mod.rs` (K9 unified pipeline, lines 1-100)
  - `/Users/fate/v07/audit-batman/src/governance/agent_action.rs` (Layer-4 agent-EXTERNAL engine, lines 1-100)
  - `/Users/fate/v07/audit-batman/src/storage/mod.rs` (`GOVERNANCE_PRE_WRITE` hook, lines 72-123)
  - `/Users/fate/v07/audit-batman/src/mcp/tools/store.rs` (K9 + `enforce_governance` call sites, lines 297-358)
  - `/Users/fate/v07/audit-batman/src/daemon_runtime.rs:2062` (hook installation)

Code evidence (substrate-internal K9 wiring in handle_store, `src/mcp/tools/store.rs:297-358`):
```
{
    use crate::permissions::{Op, PermissionContext, Permissions};
    ...
    match Permissions::evaluate(&ctx, &[]) {
        crate::permissions::Decision::Allow | crate::permissions::Decision::Modify(_) => {}
        crate::permissions::Decision::Deny(reason) => {
            return Err(format!("store denied by permission rule: {reason}"));
        }
        crate::permissions::Decision::Ask(prompt) => { ... }
    }
}
// Task 1.9: governance enforcement (store-side).
{
    ...
    match db::enforce_governance(...) {
        GovernanceDecision::Allow => {}
        GovernanceDecision::Deny(reason) => {
            return Err(format!("store denied by governance: {reason}"));
        }
        GovernanceDecision::Pending(pending_id) => { ... }
    }
}
```
And the EXPLICIT, honest layering admission in `src/governance/agent_action.rs:23-42`:
```
//! # Enforcement language (honest)
//!
//! - **Substrate-INTERNAL ops** (`memory_store`, `memory_link`, etc.):
//!   the K9 pipeline is **substrate-authoritative** — mechanically
//!   applied at the write path. The agent cannot bypass.
//! - **Agent-EXTERNAL ops** (Bash / FilesystemWrite outside the
//!   substrate / NetworkRequest / ProcessSpawn): this engine is
//!   **substrate-rule-bound, harness-mediated**. The rule lives in
//!   the substrate's `governance_rules` table; the harness ... consults
//!   the substrate via `crate::mcp::tools::check_agent_action` and
//!   honors the decision. That is mechanical at the **harness hook
//!   boundary** (operator-configured), not at the **agent attention**
//!   boundary (probabilistic).
//!
//! This module ships **callable but un-wired** in the substrate
//! write path. Storage::insert and `create_link_signed` do NOT
//! consult `check_agent_action` in this commit...
```

Behavioral assessment:
The substrate-internal half of the 7th-form claim is real — K9 + `enforce_governance` are wired BEFORE every substrate write (Op::MemoryStore/Link/Delete/Archive/Consolidate/Replay) and substrate-authoritatively short-circuit on Deny. The `GOVERNANCE_PRE_WRITE` storage-level hook is installed by `daemon_runtime.rs:2062` and consulted by `storage::insert` before the SQL row touches disk. Seven attestation columns (`signed_events.prev_hash`, `sequence`, signature, etc.) preserve a tamper-evident chain. The agent-EXTERNAL half (Bash/FilesystemWrite/NetworkRequest/ProcessSpawn) is callable as a query (`check_agent_action`) but is NOT wired into the storage write path itself — it relies on the harness honouring a PreToolUse hook consultation, which the module docs candidly describe as "mechanical at the harness hook boundary, not at the agent attention boundary".

Adversarial check:
The honesty of the module docs is itself notable — `agent_action.rs:38-42` does NOT claim mechanical agent-attention enforcement; it explicitly downgrades the claim. If the 7th form is "substrate-authority enforcement at write time" narrowly construed (substrate-internal ops only), ai-memory ships it. If it's "Layer-4 enforcement of all agent-external actions", that is `callable but un-wired`.

Classification: **PARTIAL**
Confidence: **HIGH**

Gap analysis:
- Substrate-INTERNAL enforcement: shipped and wired. Genuinely substrate-authoritative.
- Agent-EXTERNAL enforcement (the more procurement-attractive half): query surface exists, write-path wiring does not, seed rules R001-R004 ship `enabled=0`.
- LOE to wire agent-EXTERNAL: 2-3 NHI sessions per follow-up issue #691 — wire `wire_check::check` into Bash/FilesystemWrite/NetworkRequest/ProcessSpawn entry points across `skill_export`, `federation::sync`, `hooks::executor`, `llm` (already enumerated in `governance/mod.rs:65-69`).
- Recommended release: agent-EXTERNAL wiring in v0.7.x or v0.8.0 depending on the operator-side test-fleet audit timeline noted in the source comment.
- Procurement-grade implication: the 7th-form claim is genuinely defensible for substrate-internal ops. Public framing should narrow accordingly: "substrate-authoritative enforcement at the write path for substrate-internal ops (memory_store/link/delete/archive/consolidate/replay); agent-EXTERNAL Layer-4 enforcement query-callable, write-path wiring scheduled for v0.7.x".

## Honest gaps requiring action

Prioritised by procurement-grade salience.

1. **Form 1 — ABSENT.** Most quoted form in the X post; structurally furthest from what ai-memory ships. v0.8.0 is the soonest realistic close target. Public claim must NOT say "ships Form 1".
2. **Form 3 — ABSENT.** v0.9.0 strategic primitive; honest to defer.
3. **Form 2 — PARTIAL.** Closest-to-clean. Add synchronous-mode opt-in via namespace policy in v0.7.x to make this IMPLEMENTED.
4. **Form 4 — PARTIAL.** Add first-class `citations` field in v0.7.x; source-as-URL semantic upgrade in v0.8.0.
5. **Form 5 — PARTIAL.** Calibration/shadow-mode in v0.8.0. The field exists; the missing half is the calibration story.
6. **Form 6 — PARTIAL.** Extend `MemoryKind` enum with Batman-shaped vocabulary in v0.7.x or v0.8.0 (L1-6 path already reserved).
7. **7th form — PARTIAL.** Substrate-internal half ships; agent-EXTERNAL wiring follow-up issue #691 in v0.7.x.

## Procurement-grade implications

The X post claim "ai-memory v0.7.0 ships 5 of Batman's 6 forms plus a 7th form" does not survive adversarial code-evidence verification. Code-evidence supports:

- **0 forms cleanly IMPLEMENTED**
- **4 forms PARTIAL** (Forms 2, 4, 5, 6)
- **2 forms ABSENT** (Forms 1, 3)
- **7th-form claim PARTIAL** — defensible for substrate-internal ops; agent-EXTERNAL wiring is `callable but un-wired`

The honest procurement-grade public framing:

> "ai-memory v0.7.0 ships substrate-native structural primitives covering 4 of Batman's 6 write-time-investment forms at PARTIAL-implementation grade — atomisation (decompose-after-embed via deferred curator hook; synchronous mode in v0.7.x), provenance metadata (memory-grain source/timestamp/confidence; citations in v0.7.x), confidence scoring (write-time field with recall consumption; shadow-mode calibration in v0.8.0), and type tagging (3-variant MemoryKind with vocabulary extension in v0.7.x/v0.8.0). Forms 1 (online dedup-and-synthesis) and 3 (multi-step ingest) are scheduled for v0.8.0 and v0.9.0 respectively. The 7th-form claim (substrate-authority enforcement at write time) is shipped for substrate-internal ops and scheduled for agent-EXTERNAL ops in v0.7.x per issue #691."

The shorter form for the X-post correction:

> "Correction to 2026-05-15 post: code-evidence audit finds ai-memory v0.7.0 ships 4 of 6 Batman forms at PARTIAL grade (atomisation/provenance/confidence/type-tagging), with Forms 1 and 3 scheduled for v0.8.0 and v0.9.0. The 7th-form (substrate-authority at write time) ships for substrate-internal ops; agent-EXTERNAL wiring follows in v0.7.x. The honest audit is at docs/internal/batman-framework-audit.md."

## Recommended scope adjustments

| Form | Current | v0.7.x ship-target | v0.8.0 ship-target | v0.9.0 ship-target |
|------|---------|---------------------|---------------------|---------------------|
| 1 — online dedup-and-synthesis | ABSENT | — | IMPLEMENTED | — |
| 2 — atomisation | PARTIAL | synchronous-mode policy (→ IMPLEMENTED) | — | — |
| 3 — multi-step ingest | ABSENT | — | — | IMPLEMENTED |
| 4 — fact-provenance | PARTIAL | citations field (→ stronger PARTIAL) | source-as-URL semantic upgrade + atom-grain span (→ IMPLEMENTED) | — |
| 5 — confidence scoring | PARTIAL | — | shadow-mode calibration (→ IMPLEMENTED) | — |
| 6 — type tagging | PARTIAL | enum vocabulary extension (→ closer to IMPLEMENTED) | L1-6 enrichment (→ IMPLEMENTED) | — |
| 7th — substrate-authority at write | PARTIAL (internal) | agent-EXTERNAL wiring (→ IMPLEMENTED) | — | — |

Defensible defer: Form 3 (it's a strategic primitive; rushing it would produce a v0.7.x version that fails Batman's exemplar match anyway). Form 5 calibration (shadow-mode needs real-workload data which doesn't exist in v0.7.0 deployments yet).

Ship-now candidates: Form 2 synchronous-mode policy (low blast radius, opt-in via namespace standard), Form 4 citations (additive metadata field with validator), Form 6 enum vocabulary extension (additive enum + serde-default backward compat), 7th-form agent-EXTERNAL wiring (issue #691 follow-up already enumerated in source comments).

## ESCALATION

Triggers fired:

- **Trigger 1 (ai-memory implements ≤2 of Batman's 6 forms).** Code-evidence count: 0 IMPLEMENTED, 4 PARTIAL, 2 ABSENT. Material procurement finding. The X post overcounts.
- **Trigger 3 (evidence contradicts specific Strategic Nugget #014 / WT framework doc claims).** The X-post's "5 of 6 + 7th" claim is contradicted by the code-evidence count. Specifically: Form 1 is ABSENT not IMPLEMENTED, and three of the four PARTIAL forms diverge structurally from Batman's exemplars in ways that would not survive a careful reader's check.

Triggers NOT fired:

- **Trigger 2 (Layer 4 has no implementation code).** Substrate-INTERNAL Layer-4 is wired (K9 + `enforce_governance` + `GOVERNANCE_PRE_WRITE` hook). The 7th-form claim does NOT collapse.
- **Trigger 4 (WT-1 atomisation materially less complete than the WT-1 audit screenshot suggested).** The atomisation engine landed and is structurally sound; the divergence from Batman's Form 2 is the embed-before-decompose order, not implementation incompleteness.
- **Trigger 5 (ai-memory implements something genuinely novel that should arguably be 7th/8th/Nth form).** The substrate-internal authoritative governance pipeline + signed_events chain is novel and procurement-defensible; it's effectively what the 7th-form claim is pointing at, so it doesn't qualify as an "Nth form Batman missed". The signed_events cross-row hash chain (schema v34, #698 V-4 closeout) IS an arguable candidate for a procurement story Batman's framework doesn't cover — append-only tamper-evident SQL-side chain mirroring the JSONL audit log — but classifying that as an 8th-form would be a marketing choice, not a code-evidence finding.

---

## POST-CLOSEOUT STATE (2026-05-15)

**Date of post-closeout:** 2026-05-15.
**Final commit hash (audit-doc post-closeout snapshot):** `d17725c` (HEAD
of `fix/v0.7.0-cluster-k`, the v0.7.0 ship-readiness omnibus PR base).
**Original audit baseline commit:** `53b4d39` — found 0 of 6 Batman
forms IMPLEMENTED, 4 PARTIAL (Forms 2, 4, 5, 6), 2 ABSENT (Forms 1, 3),
and the 7th-form claim PARTIAL (substrate-INTERNAL only).
**Initiative tracking issue:** [#767](https://github.com/alphaonedev/ai-memory-mcp/issues/767)
— v0.7.0 ship-readiness, maximum-rigor per-feature requirements
coverage + 6-agent review + 100% fix-all.

The original audit drove the Forms 1-6 + 7th-form closeout wave, and
the subsequent 6-reviewer ship-readiness wave drove a 12-cluster
fix-all (A-L) campaign. As of `d17725c`, all 7 forms are
**IMPLEMENTED** at the substrate-evidence level, each with acceptance
suites that pin both the original Batman criterion AND the
ship-readiness review's adversarial check on the implementation
itself.

### Per-form post-closeout state

| Form | State at `53b4d39` | State at `d17725c` | Closing PRs |
|---|---|---|---|
| Form 1 — online dedup-and-synthesis | ABSENT | **IMPLEMENTED** | [#762](https://github.com/alphaonedev/ai-memory-mcp/pull/762) (Form 1 substrate land), [#777](https://github.com/alphaonedev/ai-memory-mcp/pull/777) (Cluster B — synthesis security + verdict-application + prompt-injection guard) |
| Form 2 — atomisation (decompose-before-embed) | PARTIAL | **IMPLEMENTED** | [#762](https://github.com/alphaonedev/ai-memory-mcp/pull/762) (Form 2 synchronous-mode policy) |
| Form 3 — multi-step ingest with prompt-cache reuse | ABSENT | **IMPLEMENTED** | [#763](https://github.com/alphaonedev/ai-memory-mcp/pull/763) (Form 3 orchestrator + deterministic helpers + prompt-cache) |
| Form 4 — fact-provenance (citations + source-as-URI + atom-span) | PARTIAL | **IMPLEMENTED** | [#764](https://github.com/alphaonedev/ai-memory-mcp/pull/764) (Form 4 fields + recall surface), [#771](https://github.com/alphaonedev/ai-memory-mcp/pull/771) (Cluster A — UTF-8 panic + corrupt-JSON tracing + atomisation idempotency) |
| Form 5 — confidence (auto + shadow + decay + calibration) | PARTIAL | **IMPLEMENTED** | [#766](https://github.com/alphaonedev/ai-memory-mcp/pull/766) (Form 5 substrate + `memory_calibrate_confidence`), [#774](https://github.com/alphaonedev/ai-memory-mcp/pull/774) (Cluster G — shadow-mode unboundedness + sampling cache + streaming calibration) |
| Form 6 — type tagging (Batman vocabulary) | PARTIAL | **IMPLEMENTED** | [#765](https://github.com/alphaonedev/ai-memory-mcp/pull/765) (Form 6 vocabulary expansion + auto-classify hook), [#772](https://github.com/alphaonedev/ai-memory-mcp/pull/772) (Cluster E — kind-filter inversion fix + Skills CLI/HTTP parity) |
| 7th-form — substrate-authority at write (agent-EXTERNAL Layer-4) | PARTIAL (substrate-INTERNAL only) | **IMPLEMENTED** (substrate-INTERNAL + agent-EXTERNAL via [#760](https://github.com/alphaonedev/ai-memory-mcp/issues/760)) | [#761](https://github.com/alphaonedev/ai-memory-mcp/pull/761) (7th-form Layer-4 wiring), [#775](https://github.com/alphaonedev/ai-memory-mcp/pull/775) (Cluster D — L1-6 fail-closed knob + handle_deref IDOR + matcher correctness) |

### Cross-cutting infrastructure landed in the ship-readiness wave

| Concern | Closing PR | Cluster |
|---|---|---|
| Signed-events chain integrity (BEGIN IMMEDIATE + drainer DLQ + HMAC negative tests) | [#770](https://github.com/alphaonedev/ai-memory-mcp/pull/770) | C |
| Docs + cookbook + CLI-help accuracy sweep + 6 new MVP docs | [#768](https://github.com/alphaonedev/ai-memory-mcp/pull/768) | H |
| CI postgres integration tests + memory_kind backfill pinning | [#773](https://github.com/alphaonedev/ai-memory-mcp/pull/773) | I |
| Migration filename collision cleanup + uniqueness test | [#769](https://github.com/alphaonedev/ai-memory-mcp/pull/769) | J |
| QW-4 disposition + operator-decision ADRs + accepted-debt sweep + audit post-closeout + issue cleanup | this PR | K |

### Audit-honest framing post-closeout

The X-post claim "ai-memory v0.7.0 ships 5 of Batman's 6 forms plus a
7th" — which the original audit found materially overcounted — is now
**defensibly true at HEAD `d17725c`**: all 6 Batman forms are
IMPLEMENTED at substrate-evidence level, AND the 7th form
(substrate-authority at write time, covering BOTH substrate-INTERNAL
and agent-EXTERNAL Layer-4) is IMPLEMENTED. The honest-framing
amendment vs. the original audit's recommended caveat is that the
ship-readiness wave actually closed every gap the audit flagged
rather than rephrasing the public claim.

The canonical post-wave feature inventory remains
[`docs/internal/v070-feature-inventory.md`](v070-feature-inventory.md).
The full review-and-fix campaign rollup lives in
[`docs/internal/v070-review-synthesis.md`](v070-review-synthesis.md)
and [`docs/internal/v070-ship-readiness-final.md`](v070-ship-readiness-final.md).

— Cold mountain.

