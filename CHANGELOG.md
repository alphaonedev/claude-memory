# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [v0.6.4] — 2026-05-08 — `quiet-tools`

**Headline:** ai-memory v0.6.4 ships 5 tools by default, not 43. Saves ~4,700 input tokens per request on Codex / Grok / Gemini / Claude-Desktop (76.4% reduction, measured against `cl100k_base`). Run `ai-memory mcp --profile full` to keep v0.6.3 behavior 1:1. See `RELEASE_NOTES_v0.6.4.md` and `docs/MIGRATION_v0.6.4.md`.


### Breaking

- **Default tool surface collapses from 43 to 5 (#523).** v0.6.4 ships
  with `--profile core` as the default for `ai-memory mcp`, advertising
  only `memory_store`, `memory_recall`, `memory_list`, `memory_get`,
  and `memory_search` plus the always-on `memory_capabilities`
  bootstrap. Eager-loading harnesses (Codex CLI, Grok CLI, Gemini CLI,
  Claude Desktop) drop ~5,300 input tokens of tool schemas from every
  request — measured against `cl100k_base`, the BPE Claude/GPT use for
  input accounting. **Action required for power users:** to reproduce
  v0.6.3 behavior 1:1, run `ai-memory mcp --profile full` (or set
  `AI_MEMORY_PROFILE=full` / `[mcp].profile = "full"` in config.toml).
  See `docs/MIGRATION_v0.6.4.md`.

### Added

- **`--profile` flag + `[mcp].profile` config + `AI_MEMORY_PROFILE` env
  (#521).** Resolution order: CLI > env > config > `core` default. Six
  named profiles plus comma-list custom syntax. Parse errors exit with
  code 2 and a diagnostic that lists every valid profile/family.
- **Family-scoped tool registration filter (#522).** `tools/list`
  returns only the tools loaded under the active profile;
  `tools/call` rejects unloaded tools with `-32601` plus a
  profile/family hint pointing the agent at the right `--profile` or
  `memory_capabilities --include-schema` invocation. v0.6.4-006 will
  extend `memory_capabilities` for runtime expansion.
- **Static schema-size table (#525).** New `crate::sizes` module
  computes per-tool `cl100k_base` BPE cost via `tiktoken-rs`, cached
  behind a `OnceLock`. CI-gated assertion: no individual tool may
  exceed 1,500 tokens. Truthfulness correction: the v0.6.4 RFC's
  ~25,800-token full-surface claim was measured against MiniLM and
  over-counted JSON by ~4×; the actual cl100k_base measurement is
  ~6,000 tokens.

### Fixed

- **G9 HTTP webhook parity (#526).** v0.6.3.1 P5 wired
  `dispatch_event_with_details` into the four lifecycle event types
  (`memory_delete`, `memory_promote`, `memory_link_created`,
  `memory_consolidated`) on the **MCP path only**. The HTTP handlers
  were silent — `grep "dispatch_event" src/handlers.rs` returned zero
  matches. v0.6.4-017 closes the gap symmetrically: HTTP `DELETE`,
  `POST /memories/{id}/promote`, `POST /links`, and `POST /consolidate`
  now fire the same events the MCP path fires, with the same
  payloads, the same fire-and-forget semantics, and the same
  signing/SSRF protections. New integration tests in
  `tests/webhook_http_parity.rs` pin the contract.


## [0.7.0] — 2026-05-15 — `attested-cortex` (grand-slam, reconciled)

**Headline:** v0.7.0 closes the `attested-cortex` epic in its final reconciled shape — **69/69 attested-cortex tasks across 11 tracks** (A/B/C/D/E/F/G/H/I/J/K), the **grand-slam wave** (L1-5/L1-6/L1-7/L2-1…L2-8 recursive-learning + Agent Skills + substrate-rules), the **WT-1 atomisation primitive** (A through G, issues #748-#752), the **QW Tencent quick wins** (1-4, including QW-2 PR #749), the **Batman 6-form write-time-investment closeout + 7th-form Layer-4 wiring** (issues #754-#760, PRs #761-#766), the **procurement-grade audit deliverable** ([`docs/internal/batman-framework-audit.md`](docs/internal/batman-framework-audit.md), PR #753), and the **release-branch security-hardening sweep** (16 commits reconciled into the feature trunk at merge `64528b1`). Final substrate surface: **71 MCP tools at full profile** (Family::Power: 22), schema **sqlite v39 / postgres v38**, capabilities-v3 with three new application blocks (`atomisation`, `memory_kinds_vocab`, `confidence_calibration`), eight new namespace-policy fields on `GovernancePolicy`, and a programmable 25-event hook pipeline. **postgres + Apache AGE remains a first-class storage backend** with live daemon support (`ai-memory serve --store-url postgres://…`), 6-factor recall scoring parity, link migration, and the `ai-memory schema-init` CLI verb. The substrate is both **more articulate** (capabilities v3 with pre-computed calibration strings, named loaders, the 52% MCP-tool token reduction on the full profile maintained even at 71 tools, three new application blocks) and **cryptographically trustworthy** (per-agent Ed25519 attestation with append-only `signed_events` audit chain — including V-4 cross-row hash chain at sqlite v34, sidechain transcripts with `memory_replay`, programmable hook pipeline, opt-in Apache AGE acceleration, K1/G1 namespace-inheritance enforcement, deny-first permission system, A2A maturity, K10 HMAC method+`pending_id` binding with single-use nonce cache, SSRF v4-mapped + NAT64 rejection, secret-redacting hooks, `BEGIN IMMEDIATE` `invalidate_link` wrap). Canonical scope: [`docs/v0.7/V0.7-EPIC.md`](docs/v0.7/V0.7-EPIC.md). Audit (adversarial, code-evidence-based): [`docs/internal/batman-framework-audit.md`](docs/internal/batman-framework-audit.md). Migration: [`docs/MIGRATION_v0.7.md`](docs/MIGRATION_v0.7.md) + [`docs/migration-v0.7.0-postgres.md`](docs/migration-v0.7.0-postgres.md). Operator how-to: [`docs/postgres-age-guide.md`](docs/postgres-age-guide.md). Release notes: [`docs/v0.7.0/release-notes.md`](docs/v0.7.0/release-notes.md). What's new: [`docs/whats-new-v07.html`](docs/whats-new-v07.html). RFC: [`docs/v0.7/rfc-attested-cortex.md`](docs/v0.7/rfc-attested-cortex.md).

### v0.7.0 WT-1 atomisation primitive (PRs #748-#752, branch `feat/v0.7.0-grand-slam`)

The WT-1 atomisation primitive lets the substrate decompose a long memory into addressable, individually-recallable "atoms" before embedding — a structural prerequisite for Batman Form 2 and the foundation under Form 4 fact-grain provenance. Lands as seven sub-tasks A through G, end-to-end coverage from schema → engine → MCP → namespace policy → recall → CLI → capabilities/cookbook/docs.

- **WT-1-A — schema v36 atomisation foundation** ([commit `6710709`](https://github.com/alphaonedev/ai-memory-mcp/commit/6710709), PR #748). Adds the `atomised_into` / `atom_of` / `derives_from` link relations to the canonical link vocabulary, extends the v23 `memory_links.relation` CHECK constraint covering the three new relations, and ports the migration through postgres (`migrations/postgres/0017_v07_atomisation.sql`). Schema bump **sqlite v34 → v36** (v35 is the V-4 closeout midpoint), **postgres v34 → v35**. Test pin: [`tests/wt_1_a_schema_migration.rs`](tests/wt_1_a_schema_migration.rs).
- **WT-1-B — atomiser engine + `LlmCurator` scaffolding** ([commits `1c3cdab`](https://github.com/alphaonedev/ai-memory-mcp/commit/1c3cdab), [`99419dc`](https://github.com/alphaonedev/ai-memory-mcp/commit/99419dc), [`473ee5f`](https://github.com/alphaonedev/ai-memory-mcp/commit/473ee5f), PR #750). New `src/atomisation/mod.rs` houses the atomisation flow (`AtomConfig`, error enum, `Curator` trait abstraction). The default curator wires Gemma 4 via the configured LLM client; per-atom tokens are measured against `cl100k_base` via `tiktoken-rs` (matches the v0.6.4 `crate::sizes` discipline). 11-test acceptance suite at [`tests/atomisation/core.rs`](tests/atomisation/core.rs).
- **WT-1-C — `memory_atomise` MCP tool** ([commit `aa6365a`](https://github.com/alphaonedev/ai-memory-mcp/commit/aa6365a), PR #751). Registers `memory_atomise` under `Family::Power` (semantic-tier+); the tool refuses with a typed error at the keyword tier so the v0.6.4 `--profile core` 7-tool surface stays minimal. Atomic write of the parent memory + N atom rows + N `atomised_into` link writes inside a single `BEGIN IMMEDIATE` / `COMMIT` transaction; any atom-write or link-write failure ROLLBACKs the entire fan-out. 622-test acceptance suite at [`tests/wt1c_mcp_atomise.rs`](tests/wt1c_mcp_atomise.rs). Tool count bumps **63 → 64**.
- **WT-1-D — `auto_atomise` namespace policy + `pre_store` hook** ([commit `6ad2a21`](https://github.com/alphaonedev/ai-memory-mcp/commit/6ad2a21)). New `GovernancePolicy` fields `auto_atomise: Option<bool>`, `auto_atomise_threshold_cl100k: Option<u32>`, `auto_atomise_max_atom_tokens: Option<u32>`, `auto_atomise_mode: Option<AutoAtomiseMode>` (`Off` / `Deferred` / `Synchronous`); policy resolution leaf-first via the existing `resolve_governance_policy` chain walk. New `pre_store::auto_atomise` hook intercepts substrate writes above the configured token threshold and routes through the WT-1-B engine. Acceptance suite at [`tests/auto_atomise/core.rs`](tests/auto_atomise/core.rs).
- **WT-1-E — recall atom preference + forensic atomisation chain** ([commits `3fbfb9c`](https://github.com/alphaonedev/ai-memory-mcp/commit/3fbfb9c), [`2f840b0`](https://github.com/alphaonedev/ai-memory-mcp/commit/2f840b0)). Recall now applies an atom-preference WHERE clause (recall returns atoms before parents when both score equivalently — atoms are the addressable granularity Batman Form 4 requires). Forensic bundle export gains a per-bundle atomisation chain envelope so an offline verifier can prove the atom → parent lineage independently of the live DB. 13-test acceptance suite spanning recall, search, MCP, HTTP, and forensic surfaces.
- **WT-1-F — `ai-memory atomise` CLI subcommand** ([commit `27f3fe8`](https://github.com/alphaonedev/ai-memory-mcp/commit/27f3fe8)). New `ai-memory atomise <memory-id>` verb shells the WT-1-B path from the CLI; `--dry-run` previews the proposed atom set without writing; `--json` returns the structured envelope for scripting. Composes cleanly with `ai-memory recall` for the recall-atom-preference checkpoint. Acceptance suite at [`tests/cli/atomise.rs`](tests/cli/atomise.rs).
- **WT-1-G — capabilities-v3 + cookbook + docs** ([commit `9c8be0c`](https://github.com/alphaonedev/ai-memory-mcp/commit/9c8be0c), PR #752). Capabilities-v3 gains a new `atomisation` block (`CapabilityAtomisation` in `src/config.rs`) reporting `status` (`stub` / `implemented`), curator backend, token caps, and the `auto_atomise` namespace policy surface. Cookbook entry [`cookbook/atomisation/01-basic-flow.sh`](cookbook/atomisation/01-basic-flow.sh) walks store → atomise → recall round-trip. Docs: [`docs/atomisation.md`](docs/atomisation.md). Example: [`examples/atomise_roundtrip.rs`](examples/atomise_roundtrip.rs). Test pins at [`tests/capabilities_v3_l3_5.rs`](tests/capabilities_v3_l3_5.rs).

### v0.7.0 QW Tencent quick wins (PRs #749 + commits on `feat/v0.7.0-grand-slam`)

Four quick-win primitives surfaced by the Tencent positioning analysis. Each lands as a substrate primitive (not a doc-only patch) so the capability is testable and exposed via MCP / CLI / HTTP.

- **QW-1 — file-backed reflection chain export** ([commit `6d32633`](https://github.com/alphaonedev/ai-memory-mcp/commit/6d32633)). New `ai-memory export-reflections` CLI verb + `memory_export_reflection` MCP tool walks a reflection's `reflects_on` chain and emits a deterministic POSIX-ustar archive (the L2-5 forensic-bundle discipline applied at the per-reflection scope). Namespace policy field `auto_export_reflections_to_filesystem` + new `post_reflect::auto_export` hook automate the export at write time when a namespace opts in. Cookbook: [`cookbook/file-backed-export/01-export-and-inspect.sh`](cookbook/file-backed-export/01-export-and-inspect.sh).
- **QW-2 — persona-as-artifact substrate primitive** ([commit `53b4d39`](https://github.com/alphaonedev/ai-memory-mcp/commit/53b4d39), PR #749). New `MemoryKind::Persona` (Form 6 vocabulary expansion lands the kind; QW-2 ships the substrate plumbing). Per-`(entity_id, namespace)` persona row indexed by `idx_personas_by_entity` (schema sqlite v37 / postgres v36). Two MCP tools: `memory_persona` (read most recent persona) returns the structured envelope `{id, entity_id, namespace, body_md, sources, generated_at, version, attest_level}` and `memory_persona_generate` mints the artefact from a cluster of `MemoryKind::Reflection` memories via the reflection-pass curator (300-500 word Markdown distillation with `[^N]: <reflection-id>` footnoted citations). `post_reflect::auto_persona` hook automates regeneration every N memories per namespace policy (`auto_persona_trigger_every_n_memories`). Docs: [`docs/persona.md`](docs/persona.md). Cookbook: [`cookbook/persona/01-build-persona-from-observations.sh`](cookbook/persona/01-build-persona-from-observations.sh).
- **QW-3 — context-offload substrate primitive** ([commit `2a85db2`](https://github.com/alphaonedev/ai-memory-mcp/commit/2a85db2), follow-up [`20b6be1`](https://github.com/alphaonedev/ai-memory-mcp/commit/20b6be1)). New `offloaded_blobs` substrate table (schema sqlite v35 → carried forward through subsequent bumps) stores verbatim content under a namespace with optional `ttl_seconds`; the caller keeps the short `ref_id` in their context window and dereferences on demand. Two MCP tools under `Family::Power`: `memory_offload(content, ttl_seconds?)` returns `{ref_id, content_sha256, stored_at}`; `memory_deref(ref_id)` verifies the sha256 and returns `{ref_id, content, stored_at, sha256}` (refuses tampered rows). Background TTL sweep at [`src/background/offload_ttl_sweep.rs`](src/background/offload_ttl_sweep.rs). Docs: [`docs/context-offload.md`](docs/context-offload.md). Substrate-only at v0.7.0; the v0.8.0 short-term-context-compression patch wires the pair into the auto-compaction loop.
- **QW-4 — Tencent competitive positioning** ([commit `f34a225`](https://github.com/alphaonedev/ai-memory-mcp/commit/f34a225)). **Docs-only deliverable, no code path** (per [`docs/internal/v070-ship-readiness-adrs.md` ADR-1](docs/internal/v070-ship-readiness-adrs.md#adr-1--qw-4-disposition-docs-only-no-code-feature)). Positioning page update at [`docs/positioning.md`](docs/positioning.md) adds the TencentDB Agent Memory entry alongside the existing landscape comparison. The three code-bearing QW items are QW-1 (file-backed reflection export), QW-2 (persona-as-artifact), and QW-3 (context-offload).

### v0.7.0 Batman 6-form write-time-investment closeout (issues #754-#759, PRs #762-#766)

The 2026-05-15 procurement-grade audit ([`docs/internal/batman-framework-audit.md`](docs/internal/batman-framework-audit.md), PR #753) classified the v0.7.0 grand-slam HEAD's Batman-form coverage as **0 clean / 4 partial (Forms 2, 4, 5, 6) / 2 absent (Forms 1, 3)** based on adversarial code-evidence verification — escalation trigger 1 fired. The five Form PRs below close every gap the audit flagged, lifting the coverage to **6 clean IMPLEMENTED forms + the 7th-form Layer-4 wiring** at the v0.7.0 reconciled HEAD. Each Form PR carries its own acceptance suite pinning the audit's adversarial checks.

- **Form 1 — online dedup-and-synthesis** (closes [#754](https://github.com/alphaonedev/ai-memory-mcp/issues/754), PR #762, [commit `aebe76c`](https://github.com/alphaonedev/ai-memory-mcp/commit/aebe76c)). Single batch action-emitting LLM call evaluated BEFORE the SQL write, with prompt vocabulary `{add, update, delete, no_op}` per existing-candidate. Replaces the v0.6.0.0 post-store per-pair binary yes/no classifier (kept reachable as `legacy_per_pair_classifier: Option<bool>` namespace policy for backwards compatibility). New `src/synthesis/mod.rs` houses the synthesis prompt + parser; the write-path is gated on the verdict (insert / merge / supersede / no-op). 423-test acceptance suite at [`tests/form_1_synthesis.rs`](tests/form_1_synthesis.rs).
- **Form 2 — synchronous atomise-before-embed namespace policy** (closes [#755](https://github.com/alphaonedev/ai-memory-mcp/issues/755), PR #762, [commit `aebe76c`](https://github.com/alphaonedev/ai-memory-mcp/commit/aebe76c)). The WT-1-D `auto_atomise` policy gains `AutoAtomiseMode::Synchronous` — the substrate atomises the parent BEFORE the embed call so each atom's vector lives at the addressable granularity Batman Form 2 requires. `Deferred` (existing WT-1-D default) and `Off` modes retained. 391-test acceptance suite at [`tests/form_2_synchronous_atomise.rs`](tests/form_2_synchronous_atomise.rs).
- **Form 3 — multi-step ingest orchestrator** (closes [#756](https://github.com/alphaonedev/ai-memory-mcp/issues/756), PR #763, [commit `88663d7`](https://github.com/alphaonedev/ai-memory-mcp/commit/88663d7)). New `src/multistep_ingest/` module + new MCP tool `memory_ingest_multistep` (`Family::Power`) orchestrates a two-phase ingest: phase 1 deterministic helpers (`src/multistep_ingest/helpers.rs`) extract structural facts (URIs, timestamps, named entities, key-value pairs) under an explicit-trust contract; phase 2 LLM pass refines / synthesises with **prompt-cache reuse** keyed on the phase-1 fingerprint so re-ingesting near-identical payloads short-circuits the LLM call. Acceptance suite at [`tests/form_3_multistep_ingest.rs`](tests/form_3_multistep_ingest.rs). Example: [`examples/multistep_ingest_roundtrip.rs`](examples/multistep_ingest_roundtrip.rs). Cookbook: [`cookbook/multistep-ingest/01-two-phase.sh`](cookbook/multistep-ingest/01-two-phase.sh). Docs: [`docs/multistep-ingest.md`](docs/multistep-ingest.md). Tool count bumps **65 → 66**.
- **Form 4 — fact-provenance citations + source-as-URI + atom-grain span** (closes [#757](https://github.com/alphaonedev/ai-memory-mcp/issues/757), PR #764, [commit `17bcf0c`](https://github.com/alphaonedev/ai-memory-mcp/commit/17bcf0c)). Memory rows gain per-fact citations (`citations: Vec<Citation>`), source-as-URI (`source_uri: Option<String>` distinct from the legacy `source` text field), and atom-grain span coordinates (`atom_span: Option<{start, end, parent_id}>`) so a downstream consumer can resolve a fact back to the exact byte range in the source artefact. Schema bump **sqlite v37 → v38** (migration `0032_v07_form4_provenance.sql`), **postgres v36 → v37** (migration `0019_v07_form4_provenance.sql`). Recall, search, HTTP, and forensic-bundle surfaces all carry the new fields. Docs: [`docs/provenance.md`](docs/provenance.md).
- **Form 5 — auto-confidence + shadow-mode telemetry + freshness decay + calibration tooling** (closes [#758](https://github.com/alphaonedev/ai-memory-mcp/issues/758), PR #766, [commit `2153898`](https://github.com/alphaonedev/ai-memory-mcp/commit/2153898)). New `src/confidence/` module houses three components: `derive` (per-source-namespace baseline `confidence` value computed from `crate::confidence::calibrate` history, opt-in via `AI_MEMORY_AUTO_CONFIDENCE=1`); `shadow` (records side-channel observations of caller-supplied vs. system-derived confidence for offline calibration, opt-in via `AI_MEMORY_CONFIDENCE_SHADOW=1`, sampled at `AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE`); `decay` (exponential freshness decay model, opt-in via `AI_MEMORY_CONFIDENCE_DECAY=1`). New MCP tool `memory_calibrate_confidence` (`Family::Power`) returns a `CalibrationReport` envelope (`{window_days, total_observations, baselines: [{namespace, source, count, median, mean, buckets}]}`). New CLI verb `ai-memory calibrate-confidence`. Schema bump **sqlite v38 → v39** (migration `0033_v07_form5_confidence_calibration.sql`), **postgres v37 → v38** (migration `0020_v07_form5_confidence_calibration.sql`). Docs: [`docs/confidence-calibration.md`](docs/confidence-calibration.md). Tool count bumps **66 → 67**.
- **Form 6 — `MemoryKind` Batman vocabulary + recall filter + optional auto-classify** (closes [#759](https://github.com/alphaonedev/ai-memory-mcp/issues/759), PR #765, [commit `f9b75e0`](https://github.com/alphaonedev/ai-memory-mcp/commit/f9b75e0)). `MemoryKind` extends from `{Observation, Reflection, Persona, Skill}` to the full Batman vocabulary `{Observation, Reflection, Persona, Skill, Concept, Entity, Claim, Relation, Event, Conversation, Decision}`. Recall and search gain a `--kind` filter (CLI) / `kind` parameter (MCP `memory_recall` + `memory_search`) for tight Batman-grain retrieval. New `pre_store::auto_classify_kind` hook + namespace policy field `auto_classify_kind: Option<MemoryKindAutoClassify>` (`Off` / `RegexOnly` / `RegexThenLlm`) routes uncoded writes through a 400-rule regex classifier + optional LLM fallback. Acceptance suite at [`tests/form_6_memorykind_vocab.rs`](tests/form_6_memorykind_vocab.rs). Docs: [`docs/memory-kind-vocab.md`](docs/memory-kind-vocab.md).

### v0.7.0 Batman 7th-form — agent-EXTERNAL Layer-4 wiring (issue #760, PR #761)

The pre-audit grand-slam HEAD had substrate-INTERNAL governance wired via `GOVERNANCE_PRE_WRITE` at `storage::insert` (issue #691 Deliverable E) but agent-EXTERNAL enforcement (`Bash` / `FilesystemWrite` outside the substrate / `NetworkRequest` / `ProcessSpawn`) was "callable but un-wired" per `src/governance/agent_action.rs:38-42` (audit finding §7th-form). The 7th-form PR closes the gap.

- **7th-form Layer-4 wiring** (closes [#760](https://github.com/alphaonedev/ai-memory-mcp/issues/760), PR #761, [commit `891c639`](https://github.com/alphaonedev/ai-memory-mcp/commit/891c639)). Daemon boot installs `GOVERNANCE_PRE_ACTION` covering the four agent-EXTERNAL `AgentAction` variants. MCP `skill_export`, `federation::sync`, `hooks::executor`, and the LLM client all consult the hook before side-effecting. New operator CLI `ai-memory governance install-defaults` seeds the `governance_rules` table with the audit-recommended starter rule set (`AgentAction::Bash` deny patterns for `rm -rf`, `curl | sh` shape, etc.; `AgentAction::NetworkRequest` SSRF defense-in-depth; `AgentAction::FilesystemWrite` outside `$HOME/.local-runs/` policy; `AgentAction::ProcessSpawn` for unrelated daemon-forks). 307-test acceptance suite at [`tests/form_7_agent_external_wiring.rs`](tests/form_7_agent_external_wiring.rs) pins the bypass-impossibility property across all four surfaces. Cookbook: [`cookbook/agent-external-governance/01-deny-bash.sh`](cookbook/agent-external-governance/01-deny-bash.sh). Docs: [`docs/governance/agent-action-rules.md`](docs/governance/agent-action-rules.md).

### v0.7.0 audit deliverable — adversarial procurement-grade verification (issue #753, PR #753)

- **Batman 6-form framework audit** (PR #753, [commit `fd397f9`](https://github.com/alphaonedev/ai-memory-mcp/commit/fd397f9)). 464-line adversarial code-evidence-based audit at [`docs/internal/batman-framework-audit.md`](docs/internal/batman-framework-audit.md). Methodology: 4-step adversarial protocol; read-only source code; classifications biased lower on uncertainty; no reliance on Strategic Nugget #014 / planning docs. Findings drove issues #754-#760 (Form 1-6 closeout + 7th-form Layer-4 wiring). The audit is the reference document procurement reviewers should consult — it documents what was missing pre-2026-05-15 and exactly which PRs closed which gap, so the v0.7.0 reconciled state is independently verifiable. Audit dated 2026-05-15 against pre-closeout commit `53b4d39`; the closeout PRs #761-#766 land after.

### v0.7.0 expanded scope — postgres+AGE first-class (Wave 1-4)

The original `attested-cortex` epic deferred daemon-level adapter selection to v0.7.1 ([`docs/RUNBOOK-adapter-selection.md`](docs/RUNBOOK-adapter-selection.md), pre-2026-05-09 framing). Per operator directive 2026-05-09, the adapter-selection refactor and the related postgres+AGE surface gaps surfaced by the v0.7.0 A2A campaign (#646, F6) **fold into the v0.7.0 ship** rather than carving out a v0.7.0.1 / v0.7.1 micro-release. The expanded scope splits into four implementation waves:

- **Wave 1 — surgical postgres+AGE fixes** (3 parallel streams, in flight). Stream A: `PostgresStore::link()` + `::register_agent()`, recall 6-factor parity, `migrate.rs` link-walk, SQL view aliases for off-process inspection. Stream B: new `ai-memory schema-init` CLI verb (idempotent bootstrap of postgres + AGE projection). Stream C: AGE 1.5 + PG 16 cypher-binding quirk fixed in `tests/age_cte_equivalence.rs` (test-side only — production code never hit it).
- **Wave 2 — postgres schema parity v15 → v28** (13 migrations ported: governance inheritance, webhook subscriptions, audit chain, transcripts, signed events, agent quotas, link `attest_level`, A2A correlation, smart-load veto, KG temporal-index v2, tier-promotion metadata, subscription DLQ, `consolidated_from_agents` array). Pinned by `tests/postgres_schema_parity.rs` against the SQLite v28 truth fixture.
- **Wave 3 — `ai-memory serve --store-url postgres://`** adapter-selection refactor. New `AppState.store: Arc<dyn MemoryStore>` field; handler call sites route through the SAL trait. `--features sal-postgres` opt-in; default sqlite build is byte-for-byte unchanged.
- **Wave 4 — live A2A on postgres**. The v0.7.0 A2A campaign (`ai-memory-a2a-v0.7.0`) re-runs with both droplets pointed at a shared postgres+AGE backend. S70-S76 flip from "PASS via Path B in-tree validators" to "PASS via live daemon-on-postgres". This is the cert acceptance gate for the expanded scope.

**Tag-cut criterion:** two consecutive 100% GREEN A2A rounds against the binary built from `round-2-fixes` after Wave 1-4 lands, with the Wave 4 live-on-postgres acceptance gate satisfied.

### F-series fixes (NHI campaign findings)

The v0.7.0 A2A campaign and the parallel post-ship NHI Round-2 sweep surfaced 18 findings; all 18 are closed in the v0.7.0 ship.

- **F1** ([#644](https://github.com/alphaonedev/ai-memory-mcp/issues/644), commit `e0d2086`) — `namespace_owner` now walks the parent chain. Deep-child Owner-level writes resolve correctly through inherited governance policies; the prior "no resolvable owner" 403 is fixed.
- **F2** ([#645](https://github.com/alphaonedev/ai-memory-mcp/issues/645), commit `e0d2086`) — `audit::init` seeds the `SEQUENCE` atomic from the trailing `audit.log` record at startup; the per-process counter no longer resets to 1 across daemon restart. `audit verify` is monotonic across restarts.
- **F3 / F4 / F5** — campaign-side fixes: S70 import CLI flag drift (test-side), `Harness.node_db_path()` helper for multi-droplet topology, AGE perf gate documentation.
- **F6** ([#646](https://github.com/alphaonedev/ai-memory-mcp/issues/646), Wave 1) — postgres SQL views + `migrate-links` + `schema-init` CLI surfaces. **In flight as of 2026-05-09**; Wave 1 commits will close the issue.
- **F7** (commit `f9ef40a`) — HTTP `POST /api/v1/memories` now wires through `agent_quotas` counters; quota enforcement is no longer advisory-by-accident.
- **F8** (commits `579afe2`, `63c46ab`) — `permissions.mode` defaults to `enforce` (was `advisory`). One-time migration banner on first start. **Breaking change** — see release notes for opt-back-in.
- **F9** (commit `f9ef40a`) — HTTP missing-required-field returns 400 (was 422 from axum body-extractor).
- **F10** (commit `f9ef40a`) — Embedder timeout on >64KB content surfaces an `EmbedStatus` enum on the response instead of silently producing an un-indexed row at HTTP 201.
- **F11** (commits `579afe2`, `bd01978`) — `ai-memory forget --pattern X` and `forget --tier T` without `--namespace` require `--confirm-global`. **Breaking change** — see release notes.
- **F12** (commits `579afe2`, `63c46ab`) — Ed25519 keypair auto-generated on `serve` startup if absent. Idempotent on rerun.
- **F13** (commit `66f48ae`) — `memory_capabilities` schema/behavior drift fixed; `verbose` and `include_schema` flags actually do what the schema claims.
- **F14** (commits `66f48ae`, `5b36d7c`) — Smart-load router weights underscore tokens correctly (`memory_notify` no longer collapses to `meta`; `memory_expand_query` no longer collapses to `graph`).
- **F15** (commit `66f48ae`) — MCP `memory_store` / `memory_update` `inputSchema` now lists the `metadata` field.
- **F16** (commit `66f48ae`) — `agent_type` MCP enum opened to match daemon's permissive accept-set.
- **F17** (commits `082c999`, `f02d092`) — `find_paths` `max_depth` cap of 7 documented in tool description; directed-vs-undirected semantics clarified inline.
- **F18** (commits `082c999`, `63c46ab`) — `check_duplicate` raw-content sha256 short-circuit for byte-identical strings; the embedding-similarity 0.92 ceiling no longer hides true duplicates.
- **AGE 1.5.0 + PG 16 cypher-binding compat** (Wave 1, Stream C) — fixed in `tests/age_cte_equivalence.rs`. Production code never hit it; the harness did. Unblocks the parity test suite on AGE 1.5.0.

### v0.7.0 recursive-learning add-on (Tasks 1-6 of 8, issue [#655](https://github.com/alphaonedev/ai-memory-mcp/issues/655))

Substrate-native primitive for **recursive refinement**: an agent reads one or more memories, synthesises a higher-order reflection (a lesson, pattern, contradiction-resolution, etc.), and persists it with cryptographic-grade provenance back to each source it reflects on. Bounded by design — a substrate-enforced depth cap rejects runaway recursion before any write opens. No autonomous goal modification, no model fine-tuning loops, no unbounded recursion. Folds into the v0.7.0 ship rather than carving a separate v0.7.1 release. Tasks 1-6 landed on `feat/v0.7.0-recursive-learning`; Tasks 7-8 (ship-gate test suite + docs/release-notes/capabilities honesty pass) land on the same branch and roll up here.

- **Task 1** ([commit `f5d8a9e`](https://github.com/alphaonedev/ai-memory-mcp/commit/f5d8a9e)) — `memories.reflection_depth INTEGER NOT NULL DEFAULT 0` column on SQLite (schema v29) and Postgres (`CURRENT_SCHEMA_VERSION 31`). New migration `migrations/postgres/0013_v0700_reflection_depth.sql`. `Memory` struct gains the `reflection_depth: i32` field (`#[serde(default)]` keeps wire-compat with pre-v0.7.0 federation peers) plus `impl Default for Memory` so future struct-field additions stop fanning out to ~50 test fixtures. UPSERT clauses on both adapters take `MAX(old, new)` so newer-wins federation merges preserve the higher-depth signal.
- **Task 2** ([commit `630a6db`](https://github.com/alphaonedev/ai-memory-mcp/commit/630a6db)) — namespace governance gains `GovernancePolicy.max_reflection_depth: Option<u32>` (pure JSON metadata; no schema bump). Accessor `effective_max_reflection_depth(&self) -> u32` returns the compiled default `3` when unset; `Some(0)` is a documented kill-switch that refuses every reflection (the substrate check is `attempted > cap`, so cap=0 fails at depth ≥ 1). Per-namespace overrides ride the same leaf-first chain walk `resolve_governance_policy` already does.
- **Task 3** ([commit `b51a3f3`](https://github.com/alphaonedev/ai-memory-mcp/commit/b51a3f3)) — new canonical link relation `reflects_on` joins `VALID_RELATIONS` (alongside `related_to`, `supersedes`, `contradicts`, `derived_from`). Directionality matches `derived_from`: the reflection memory is the link's `source_id`, the original being reflected on is `target_id`. The two MCP `memory_link` / `memory_unlink` `inputSchema.relation` enums and the `claude_help` prompt's pipe-list extend in lockstep. No schema migration needed — `memory_links.relation` has no `CHECK` clause on either adapter. `db::find_paths`'s recursive-CTE walks every relation, so `reflects_on` chains surface naturally in chain-walk queries without further work.
- **Task 4** ([commit `3dc76f3`](https://github.com/alphaonedev/ai-memory-mcp/commit/3dc76f3)) — new MCP tool `memory_reflect` (`Family::Power`, tool-count bumps **51 → 52**). Atomic insert of a reflection memory + N `reflects_on` link writes inside a single `BEGIN IMMEDIATE` / `COMMIT` transaction; any link-insert failure ROLLBACKs the entire write so the reflection memory itself never survives a half-written state. Postgres parity via inherent `PostgresStore::reflect` (single `sqlx::Transaction` mirroring the SQLite path). New error variant `MemoryError::ReflectionDepthExceeded { attempted: u32, cap: u32, namespace: String }` (HTTP `409 CONFLICT`, code `REFLECTION_DEPTH_EXCEEDED`). The reflection memory carries a system-generated `metadata.reflection_metadata` block (`reflected_on_source_ids`, `reflection_depth`, `reflection_created_at`); caller-supplied metadata keys win on collision (documented additive contract).
- **Task 5** ([commit `c61a05b`](https://github.com/alphaonedev/ai-memory-mcp/commit/c61a05b)) — H5 audit chain now covers depth-cap refusals on `memory_reflect`. Every `ReflectError::DepthExceeded` appends a `reflection.depth_exceeded` row to the append-only `signed_events` audit table binding `(agent_id, attempted, cap, namespace, source_ids, proposed_title, created_at)` under a canonical-CBOR (RFC 8949 §4.2.1) payload with a SHA-256 `payload_hash` and `attest_level = "unsigned"`. The reflection's content body is deliberately omitted from the audit payload (PII guarantee — only enumerable provenance fields are signed). Audit-write failures are best-effort: logged via `tracing::warn!(target: "signed_events", ...)` but the cap refusal still propagates to the caller. Caller-policy refusals (hook vetoes, see Task 6) carry their own provenance and do NOT emit this row.
- **Task 6** ([commit `fbf093c`](https://github.com/alphaonedev/ai-memory-mcp/commit/fbf093c)) — Track G hook pipeline grows from 21 to 23 events with two new `HookEvent` variants: `pre_reflect` (decision-class, `Write` event class, 5s deadline) fires BEFORE the depth-cap check and may VETO the reflection by returning `Deny { reason, code }`; vetoes propagate as `ReflectError::HookVeto` (`"REFLECTION_HOOK_VETO (code=<N>): <reason>"`) distinct from a cap refusal. `post_reflect` (notify-class, `Write` event class, 5s deadline) fires AFTER the atomic transaction commits, so post-handlers read the fully-durable reflection memory + its `reflects_on` links via the same connection. The G10 hot-path floor had already raised the pipeline count from 20 to 21 (`pre_recall_expand`); Task 6 raises it to 23. Hook vetoes are *not* audited via the Task 5 cap-refusal row — caller-policy refusals carry their own provenance, and conflating them with substrate-cap refusals would dilute the audit signal. The MCP wire-in of `hooks.toml` → `ReflectHooks` is deferred to G7+ (the v0.7.0 handler ships an unreachable `HookVeto` arm pending that bridge).

Tasks 7-8 (ship-gate test suite + docs/release-notes/capabilities honesty pass) land on the same branch and roll up into this v0.7.0 entry. Tracker issue: [#655](https://github.com/alphaonedev/ai-memory-mcp/issues/655).

### v0.7.0 grand-slam wave — substrate-native recursive learning at scale (issues [#666](https://github.com/alphaonedev/ai-memory-mcp/issues/666)–[#673](https://github.com/alphaonedev/ai-memory-mcp/issues/673), [#691](https://github.com/alphaonedev/ai-memory-mcp/issues/691), [#693](https://github.com/alphaonedev/ai-memory-mcp/issues/693))

Extends the recursive-learning substrate primitive into a complete substrate-native learning loop. Folds into the v0.7.0 ship rather than carving a separate v0.7.1 release (operator decision `05e0cb9a`, v0.7.1 ABOLISHED). Lands on `feat/v0.7.0-grand-slam` at commit `c359e89`.

- **L1-5 Agent Skills ingestion substrate.** New typed `skills` table holds agentskills.io-compliant SKILL.md manifests with YAML frontmatter, optional `resources/` sub-directory, content-addressed SHA-256 digest, Ed25519 attestation when an operator keypair is on disk, and version chaining on re-register. **5 MCP tools** in the initial substrate ship: `memory_skill_register`, `memory_skill_list`, `memory_skill_get`, `memory_skill_resource`, `memory_skill_export`. Register → export → re-register produces the IDENTICAL SHA-256 digest (the round-trip guarantee). Federation preserves digest + signing-agent identity across hops. See [`docs/agent-skills.md`](docs/agent-skills.md).
- **L1-6 substrate rules-enforcement engine — Option B foundation.** Operator-keypair-signed seed rules (`R001..R004`) in the `governance_rules` table. `verify_rule_signature` runs on load and refuses to start the daemon on a signed-rule-with-bad-signature. Bypass-impossibility integration test fleet ([commit `6038f85`](https://github.com/alphaonedev/ai-memory-mcp/commit/6038f85)). New `ai-memory rules sign` operator CLI ([commit `4e5b560`](https://github.com/alphaonedev/ai-memory-mcp/commit/4e5b560)). MCP read-only inspection via `memory_rule_list` + `memory_check_agent_action`; mutation is operator-only per design revision 2026-05-13. L1-6 Deliverable E ([commit `1b877ce`](https://github.com/alphaonedev/ai-memory-mcp/commit/1b877ce), [#691](https://github.com/alphaonedev/ai-memory-mcp/issues/691)) wires `check_agent_action` into `storage::insert` as a pre-write hook with the structured `RuleRefused` error variant. **Audit-honest framing:** substrate authority is a foundation in v0.7.0, a complete cover in v0.8.0 ([#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697)).
- **L1-7 compaction pipeline.** New `CompactionPass` trait + cosine clustering pipeline supporting the curator's reflection mode and future consolidation rewrites. 25-event pipeline. ([merge commit `7451143`](https://github.com/alphaonedev/ai-memory-mcp/commit/7451143).)
- **L2-1 reflection-pass curator** ([commit `c3f6e82`](https://github.com/alphaonedev/ai-memory-mcp/commit/c3f6e82), [#666](https://github.com/alphaonedev/ai-memory-mcp/issues/666)) — asynchronous curator clusters `Observation`-kind memories by namespace + temporal proximity + recall co-occurrence proxy and mints reflections through the substrate path. Opt-in per namespace; `MIN_CLUSTER_SIZE = 3`, `MAX_CLUSTER_SIZE = 12`, 7-day temporal window. One level of reflection per pass; multi-level chains form over repeated passes when `max_reflection_depth` permits. Operator-facing CLI: `ai-memory curator --reflect`. Runbook: [`docs/RUNBOOK-curator-soak.md`](docs/RUNBOOK-curator-soak.md).
- **L2-2 federation-aware reflection coordination** ([commit `0b1c9cc`](https://github.com/alphaonedev/ai-memory-mcp/commit/0b1c9cc), [#667](https://github.com/alphaonedev/ai-memory-mcp/issues/667)) — receivers stamp `metadata.reflection_origin = {peer_origin, original_depth, local_depth_at_arrival}` on inbound reflection memories. The local cap is enforced on **derived** writes regardless of source peers' caps — federation cannot launder depth. The new MCP tool `memory_reflection_origin` returns the structured origin envelope.
- **L2-3 reflection invalidation propagation** ([commit `3f419be`](https://github.com/alphaonedev/ai-memory-mcp/commit/3f419be), [#668](https://github.com/alphaonedev/ai-memory-mcp/issues/668)) — a Reflection→Reflection `supersedes` edge fires `propagate_reflection_invalidation` which writes one notification memory per dependent under `<dependent.namespace>/_invalidations` with `metadata.notification_kind = "reflection_invalidation"` and the four-tuple `{dependent_id, invalidated_id, invalidating_id, timestamp}`. **Notification, NOT cascade** — dependents are flagged for operator/curator review, never auto-superseded. Cascade rollback is v0.8.0 Pillar 2.5. The new MCP tool `memory_dependents_of_invalidated` is the read-only inspection surface.
- **L2-4 transcript replay union** ([commit `a50b34c`](https://github.com/alphaonedev/ai-memory-mcp/commit/a50b34c), [#669](https://github.com/alphaonedev/ai-memory-mcp/issues/669)) — `memory_replay` on a reflection memory returns the union of transcripts reachable by walking `reflects_on` to the source observations. Caller-controlled walk depth via `depth=N`; `depth=0` returns the reflection's own transcripts only (matches the pre-L2-4 I4 shape).
- **L2-5 forensic bundle** ([commit `bb870b3`](https://github.com/alphaonedev/ai-memory-mcp/commit/bb870b3), [#670](https://github.com/alphaonedev/ai-memory-mcp/issues/670)) — new CLI verbs `ai-memory export-forensic-bundle` and `ai-memory verify-forensic-bundle`. Deterministic in-process POSIX-ustar tar with per-file SHA-256, optional Ed25519 manifest signature, and **byte-identical mod timestamp** reproducibility. AgenticMem Attest tier integration. Pairs with L1-3 `verify-reflection-chain`. See [`docs/forensic-export.md`](docs/forensic-export.md).
- **L2-6 reflection-as-skill promote** ([commit `505c538`](https://github.com/alphaonedev/ai-memory-mcp/commit/505c538), [#671](https://github.com/alphaonedev/ai-memory-mcp/issues/671)) — new MCP tool `memory_skill_promote_from_reflection` promotes a `Reflection`-kind memory (depth ≥ namespace cap, default floor `1`) into a SKILL.md-format Agent Skill. Each `reflects_on` source becomes a `references/source_{i}.md` resource. Frontmatter carries `derived_from_reflection_id` + `original_reflection_depth`. Promote → export → re-register produces the IDENTICAL SHA-256 digest. **Closes the recursive-learning loop.**
- **L2-7 skill ↔ reflection composition** ([commit `0966b57`](https://github.com/alphaonedev/ai-memory-mcp/commit/0966b57), [#672](https://github.com/alphaonedev/ai-memory-mcp/issues/672)) — SKILL.md frontmatter gains the optional `composes_with_reflections` list, each entry a `{namespace, min_depth}` pair. New MCP tool `memory_skill_compositional_context` returns the skill body + reflection memories from the declared namespaces, filtered by per-entry `min_depth` and bounded by `GovernancePolicy::effective_max_reflection_depth` (the **authoritative ceiling** — composition cannot bypass the substrate cap). Reflections ranked by recency + saturating recall_count; cumulative content bounded by `budget_tokens` (default 4000, max 32000).
- **L2-8 reflection-aware reranker boost** ([commit `90291c0`](https://github.com/alphaonedev/ai-memory-mcp/commit/90291c0), [#673](https://github.com/alphaonedev/ai-memory-mcp/issues/673)) — reranker applies `boost * (1 + per_depth_increment * min(reflection_depth, max_depth_cap))` to `Reflection`-kind memories AFTER the cross-encoder blend. Defaults: `boost=1.2`, `per_depth_increment=0.05`, `max_depth_cap=3` (mirrors the substrate cap). `boost=1.0` is the documented kill-switch — reproduces pre-L2-8 ranking exactly.
- **MCP tool count 60 → 63** across the grand-slam wave:
  - L2-2 adds `memory_reflection_origin` (60 → 61 effective).
  - L2-3 adds `memory_dependents_of_invalidated` (61 → 62 effective, registered after L2-2 in the tool-count audit).
  - L2-6 adds `memory_skill_promote_from_reflection` (62).
  - L2-7 adds `memory_skill_compositional_context` (63).
  - Plus the L1-5 substrate's 5 `memory_skill_*` tools registered earlier on the same branch (`register`, `list`, `get`, `resource`, `export`).
- **Schema v33** ([commit `58877c7`](https://github.com/alphaonedev/ai-memory-mcp/commit/58877c7)) — promotes the `memory_links.relation` validation from a v23 trigger to a SQL-side CHECK constraint covering `related_to | supersedes | contradicts | derived_from | reflects_on`. Postgres parity migration mirrors the same constraint. Lands in v0.7.0 per `05e0cb9a` v0.7.1-fold decision (v0.7.1 ABOLISHED).
- **Schema v34 — V-4 closeout (#698) `signed_events` cross-row hash chain.** Adds `prev_hash BLOB` + `sequence INTEGER` columns plus a UNIQUE INDEX on `signed_events`, mirroring the JSONL property in `src/audit.rs` at the SQL surface. Per-row Ed25519 signatures (existing) prove individual event integrity; the cross-row chain (this closeout) is the LOAD-BEARING tamper-evidence property — a DELETE of row N is detected at row N+1's `prev_hash` mismatch and a tampered `sequence` is detected by the contiguity check in [`verify_chain`](src/signed_events.rs). Postgres parity bumps to v33. Backfill stamps pre-existing rows in [`migrate_v34_backfill_chain`](src/storage/migrations.rs) and is idempotent on replay. New operator surface: `ai-memory verify-signed-events-chain [--since <sequence>] [--format text|json]`. Flips the V-4 validation status from YELLOW (operator directive's `monotonic_sequence == prior + 1` was unsatisfiable without a sequence column) to GREEN. Test pin: [`tests/signed_events_chain_v34.rs`](tests/signed_events_chain_v34.rs) (7 tests covering first-row zero-prev_hash, multi-row chaining, payload tamper detection, sequence tamper detection, concurrent drainer inserts via PE-3 pattern, backfill idempotency, and backfill correctness on pre-existing rows). Drainer-soak integration test ([`tests/deferred_audit_soak.rs`](tests/deferred_audit_soak.rs)) now asserts chain holds after 5K concurrent inserts.

### v0.7.0 substrate authority — Policy Engine (Option B landed, parent meta [#693](https://github.com/alphaonedev/ai-memory-mcp/issues/693))

The v0.7.0 substrate ships the policy engine surface that gates
agent-EXTERNAL actions (Bash, FilesystemWrite outside the substrate,
NetworkRequest, ProcessSpawn, Custom) against an operator-signed
`governance_rules` table, alongside the existing K9 governance
pipeline that gates substrate-INTERNAL ops. Full architectural
documentation lives at
[`docs/policy-engine.md`](docs/policy-engine.md); the audit-trail
coverage matrix at
[`docs/security/audit-trail-coverage.md`](docs/security/audit-trail-coverage.md).

**Shipped at v0.7.0 grand-slam HEAD:**

- **L1-6 substrate-rules engine** ([#691](https://github.com/alphaonedev/ai-memory-mcp/issues/691)).
  `AgentAction` enum + variants (`Bash` / `FilesystemWrite` /
  `NetworkRequest` / `ProcessSpawn` / `Custom`); `RulesStore` typed
  CRUD over the new `governance_rules` table (migration
  `0024_v07_governance_rules.sql`); `check_agent_action` audited path
  (every call emits one `governance.check` row to `signed_events`);
  seed rules R001-R004 land at `enabled = 0` per the cold-start
  contract; operator keypair at `~/.config/ai-memory/operator.key`
  (mode 0600 enforced at load); load-time Ed25519 signature
  verification with the bypass-prevention property
  (`canonical_bytes_for_signing` commits to `enabled`, so a direct
  `UPDATE governance_rules SET enabled = 1` invalidates the recorded
  signature and the rule is skipped). Six L1-6 integration tests
  pin the tampered-signature / direct-enabled-flip / open-permissions
  / sign-seed-idempotent / rotated-key matrices.
- **L1-6 Deliverable E — `storage::insert` governance pre-write hook**
  ([#691](https://github.com/alphaonedev/ai-memory-mcp/issues/691),
  commit `1b877ce`). Process-wide `OnceLock` in
  `src/storage/mod.rs::GOVERNANCE_PRE_WRITE`; installed exactly once
  at daemon `serve` boot (CLI one-shot paths leave it empty by
  design). Every substrate write path (`insert`,
  `insert_with_conflict`, `insert_if_newer`) consults the hook before
  the SQL `INSERT`; refusal short-circuits the write with no row
  touched and propagates `MemoryError::RefusedByGovernance` →
  HTTP `403 GOVERNANCE_REFUSED`. Six integration tests
  (`tests/governance_storage_insert_hook.rs`) pin the bypass-impossibility
  property — including that **all three** insert paths are gated and
  that the CLI one-shot mode does NOT install the hook.

**v0.7.0 Option B work in flight (parent meta [#693](https://github.com/alphaonedev/ai-memory-mcp/issues/693)):**

- **PE-1** ([#694](https://github.com/alphaonedev/ai-memory-mcp/issues/694))
  universal `AgentAction` wire-point coverage. Branch
  `policy-engine/wire-points`.
- **PE-2** ([#695](https://github.com/alphaonedev/ai-memory-mcp/issues/695))
  Claude Code PreToolUse harness hook installer. Branch
  `policy-engine/harness-hook`. Once merged, `ai-memory install
  --harness claude-code --enforce-policy` configures the hook so
  the harness consults `memory_check_agent_action` before every
  Bash / Write / Network / ProcessSpawn the agent proposes.
- **PE-3** ([#696](https://github.com/alphaonedev/ai-memory-mcp/issues/696))
  deferred audit-log queue. Branch
  `policy-engine/deferred-audit-log`. Closes the storage-hook
  audit gap: refusals at the substrate-internal pre-write path are
  typed AND chain-logged via a process-local tokio drain task —
  same canonical bytes / payload hash as the audited path, no
  re-entrancy on the substrate writer.

**Honest framing.** v0.7.0 ships substrate authority for
agent-EXTERNAL actions that are **substrate-visible** (the storage
write path mechanically; the agent-external Bash / Write / Network /
ProcessSpawn surface via opt-in harness coverage once PE-2 merges).
Out-of-band channels (agents that bypass the harness entirely) are
not enforceable by the substrate — see V08-PE-1 (mandatory-hook
profile) and V08-PE-6 (TPM-bound binary integrity) under the v0.8.0
closeout below. Subprocess-chain visibility (a permitted Bash whose
child forks an unrelated process) is also out of scope at v0.7.0 —
see V08-PE-3.

**v0.8.0 closeout epic — 100% Cryptographic Forensic Audit Trail
([#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697)).**
Closes the remaining ~5% gap. Eight sub-tasks (V08-PE-1 …
V08-PE-8): mandatory-hook profile, read-action gating, subprocess-chain
visibility via eBPF/dtrace, persistent audit queue (durable across
daemon restart — closes PE-3's process-local gap), severity-based
human escalation (adds `Decision::Escalate`), TPM-bound binary
integrity, refuse-by-default profile, and the
`ai-memory verify-audit-trail` completeness verifier. Effort:
22-28 sessions · 3-4 weeks wall-clock. Full sub-task detail in
ROADMAP2 §16. Operator directive of 2026-05-14 verbatim — "Every
tool call passes through a policy engine; the engine logs every
refusal cryptographically; severity-classified rules can escalate
to human" — is the property v0.8.0 closes literally.

**v0.7.0 grand-slam fold update.** PE-1 / PE-2 / PE-3 have all
landed on `feat/v0.7.0-grand-slam`:

- **PE-1** wire-points ([#694](https://github.com/alphaonedev/ai-memory-mcp/issues/694))
  installs `GOVERNANCE_PRE_ACTION` at daemon boot covering the four
  agent-EXTERNAL action variants. MCP skill_export, federation::sync,
  hooks::executor, and the LLM client all consult the hook before
  side-effecting.
- **PE-2** harness-hook ([#695](https://github.com/alphaonedev/ai-memory-mcp/issues/695))
  `ai-memory install --harness claude-code --enforce-policy` wires
  the PreToolUse hook into the harness `settings.json` so every
  Bash / Write / Network / ProcessSpawn the agent proposes passes
  through `memory_check_agent_action`.
- **PE-3** deferred-audit-log ([#696](https://github.com/alphaonedev/ai-memory-mcp/issues/696))
  closes the storage-hook chain-log gap. Refusals at the
  substrate-internal pre-write path are now BOTH typed AND chain-logged
  via a process-local tokio drain task (`governance.refusal` rows in
  `signed_events`); the in-flight write transaction releases its lock
  before the audit row writes so deadlock is structurally impossible.

### Track summary (11 tracks, 69 tasks)

- **Track A — Capabilities v3 response shape (5 tasks).** Adds `summary`, `to_describe_to_user`, `callable_now`, `agent_permitted_families` to the `memory_capabilities` response, plus `schema_version="3"` (additive over v2). Pre-computed per-agent calibration strings let LLMs converge on accurate first-answer descriptions instead of improvising. v3 fields are additive — v2 wire shape stays supported through the v0.7.x line. Canonical phrasings pinned in [`docs/v0.7/canonical-phrasings.md`](docs/v0.7/canonical-phrasings.md).
- **Track B — Loader tools (5 tasks).** `memory_load_family` and `memory_smart_load(intent)` are promoted to **always-on first-class tools** (no longer hidden inside an introspection tool's parameter set). Reasoning-class LLMs find them on first ask. Includes harness detection from MCP `clientInfo` (Claude Code, Codex, Grok CLI, Gemini CLI, Continue, Cursor, Cline, Aider, Goose, Claude Desktop, generic JSON-RPC) and family-descriptor embeddings powering `memory_smart_load`'s intent-to-family routing.
- **Track C — Schema compaction (5 tasks).** **52% MCP tool-token reduction** on the full profile. Description / docs split (long form moved to per-tool docs links), optional params hidden from default schema, inline examples stripped, hard CI gate enforces ≤ 3,500 input tokens for `--profile full` `tools/list`. Combined with v0.6.4's 76.4% default-profile reduction, the cortex now ships at < 3.5K tokens even when fully loaded.
- **Track D — Per-harness positioning + tests (4 tasks).** Cross-harness benchmark across the 11 supported harnesses; landing-page compatibility matrix at [`docs/v0.7/compatibility-matrix.html`](docs/v0.7/compatibility-matrix.html); install-time system-prompt snippet emitted by `ai-memory install`; harness integration tests in `tests/harness_*.rs` covering both 5-tool default and full-profile loading paths.
- **Track E — Discovery Gate T0 calibration cells (3 tasks).** Discovery Gate T1-T3 loader cells; T0 orchestration script driving 4 LLMs (Claude, Grok, Gemini, GPT) for ≥ 95% convergence verification on canonical phrasings; post-ship convergence verification scheduled against the released binary. See [`docs/v0.7/T0-ORCHESTRATION.md`](docs/v0.7/T0-ORCHESTRATION.md).
- **Track F — Docs + release (6 tasks).** [`docs/MIGRATION_v0.7.md`](docs/MIGRATION_v0.7.md) v0.6.4 → v0.7.0 guide; [`docs/whats-new-v07.html`](docs/whats-new-v07.html) what's-new page; [`docs/v0.7/rfc-attested-cortex.md`](docs/v0.7/rfc-attested-cortex.md) RFC; `README.md` + `docs/ADMIN_GUIDE.md` updates; top-nav badges; this release-cut PR.
- **Track G — Hook Pipeline (11 tasks, Bucket 0).** The substrate ships: `~/.config/ai-memory/hooks.toml` config file; **25 lifecycle event types** with payloads — the Track G 20 baseline (`pre_store`, `post_store`, `pre_recall`, `post_recall`, `pre_search`, `post_search`, `pre_delete`, `post_delete`, `pre_promote`, `post_promote`, `pre_link`, `post_link`, `pre_consolidate`, `post_consolidate`, `pre_governance_decision`, `post_governance_decision`, `on_index_eviction`, `pre_archive`, `pre_transcript_store`, `post_transcript_store`) plus 5 grand-slam additions (`pre_recall_expand` G10 + `pre_reflect`/`post_reflect` recursive-learning Task 6/8 + `pre_compaction`/`on_compaction_rollback` L1-7), enumerated in `src/hooks/events.rs::HookEvent`; `ExecExecutor` + `DaemonExecutor` JSON-stdio IPC; decision types (`Allow`/`Deny`/`Modify`/`Defer`); chain ordering with priority; per-event timeouts; hot reload on `hooks.toml` mtime change; `on_index_eviction` for HNSW/cache eviction observability; reranker batching for concurrent recall; `pre_recall` daemon-mode hook; **R3 auto-link reference detector** as a reference hook binary.
- **Track H — Ed25519 Attested Identity (6 tasks, Bucket 1).** `ai-memory identity generate` CLI mints per-agent Ed25519 keypairs; outbound link signing fills the v0.6.3 `memory_links.signature` "dead column"; inbound signature verification on every link write; `attest_level` enum (`unsigned` / `signed` / `verified` / `rejected`); `memory_verify` MCP tool surfaces signature state on demand; **append-only `signed_events` audit table** with hash-chained provenance; end-to-end test pinning the full mint → sign → verify → audit cycle.
- **Track I — Sidechain Transcripts (5 tasks, Bucket 1.7).** `memory_transcripts` schema (BLOB + zstd-3); `memory_transcript_links` join table; per-namespace TTL with exact-match → longest `prefix/*` → `*` → default-off precedence; `memory_replay` MCP tool reconstructs full conversation context from a transcript link; **R5 `pre_store` transcript-extraction reference hook** ships as a standalone Rust binary at `tools/transcript-extractor/` (kept out of the published crates.io upload via the parent `Cargo.toml`'s `include` allowlist).
- **Track J — Apache AGE Acceleration (8 tasks, Bucket 2).** AGE detected at Postgres-SAL connect-time via `pg_extension` probe (logged-only fallback to CTE on missing extension or probe error); Cypher implementations of `kg_query`, `kg_timeline`, `kg_invalidate`, and **R2 `find_paths`**; dual-path tests gated on `AI_MEMORY_TEST_AGE_URL`; AGE / CTE per-query performance budgets with bench-time gate; `KgBackend { Cte, Age }` enum exposed via `Capabilities` (`kg_backend` field) for `ai-memory doctor` and `memory_capabilities`.
- **Track K — A2A + Permissions + G1 cutline (11 tasks, Bucket 3).** **K1/G1 namespace-inheritance enforcement** (the mandatory cutline — `resolve_governance_policy` walks the namespace chain; first non-null policy wins); `pending_actions` timeout sweeper (closes the v0.6.3.1 `default_timeout_seconds` honesty disclosure); `permissions.mode` enforcement gate (defaults to `enforce` per F8); approval-event routing; `permissions.rule_summary` re-instated; A2A correlation IDs + ACK retries + TTL + replay protection; subscription DLQ + replay-from-cursor + HMAC; per-agent quotas with daily reset; unified permission pipeline (rules + modes + hooks → decision); approval API on **HTTP + SSE + MCP** with HMAC and `remember=forever`; `ai-memory governance migrate-to-permissions` translator CLI for upgrading v0.6.x governance configs.

### Migration from v0.6.x

- **From v0.6.4 (sqlite, staying on sqlite):** auto-migrates v20 → v34 on first start (the Wave 1-4 narrative checkpoint v20 → v28 was the initial postgres+AGE land; in-flight v0.7.0 work then added v29-v30 for recursive-learning, v33 for L2 wave `memory_links.relation` CHECK, and v34 for V-4 closeout `signed_events` cross-row chain). See `docs/MIGRATION_v0.7.md` for the v0.6.4 → v0.7.0 surface delta.
- **From v0.6.4 (sqlite, switching to postgres+AGE):** see `docs/migration-v0.7.0-postgres.md`. Provision postgres + AGE + pgvector → `ai-memory schema-init` → dry-run migrate → real migrate → verify → cutover.
- **From v0.7-alpha (postgres at schema v15):** `ai-memory schema-init --upgrade` walks v15 → v33 idempotently (Wave 1-4 ported v15 → v28; subsequent L0.7 / L2 / V-4 closeout work added v29 - v33 on the postgres side).

### Breaking changes

- **F8 — `permissions.mode` defaults to `enforce`** (was `advisory`). Operators relying on default-permissive must opt back in via `[permissions] mode = "advisory"` in `config.toml`.
- **F11 — `forget --pattern` / `forget --tier` without `--namespace`** require `--confirm-global`.

### Security-hardening sweep — release/v0.7.0 reconciliation (16 commits, folded at merge `64528b1`)

Sixteen late-cycle security-hardening commits landed on `release/v0.7.0` between the initial release-cut and the reconciled v0.7.0 HEAD. All sixteen are folded into the v0.7.0 ship via the reconciliation merge `64528b1` (parent `fd397f9` audit deliverable + parent `6b6b3c0` release tip). Both audiences (release auditors + feature operators) see the same surface. The eleven late-cycle K10 / K9 / SSRF / hooks / db / permissions / transcripts fixes below are the headline; the remaining five reconciled commits are the prior `release/v0.7.0` C5 budget gate fix (`5711a5d`), C1/C2/H10 governance fix (`42d384d`), H5/H6/I1 identity fix (`4305925`), H1/H3/H4 governance fix (`c02d5ed`), and H9 hooks-stderr-drain fix (`e2b9544`).

- **SSRF — reject IPv4-mapped IPv6 + NAT64 prefix bypasses** ([commit `3ab72dc`](https://github.com/alphaonedev/ai-memory-mcp/commit/3ab72dc)) — `validate_url_with` now refuses `::ffff:10.0.0.1` and `64:ff9b::10.0.0.1` style addresses that would otherwise smuggle private-range traffic past the v6 path. Test pin: `tests/k10_approval_security.rs` SSRF v4-mapped cases (release-branch tightening on `6b6b3c0` updated callers to pass the explicit flag).
- **K9 governance gate parity on `handle_kg_invalidate`** ([commit `a41c08f`](https://github.com/alphaonedev/ai-memory-mcp/commit/a41c08f)) — the KG invalidate path now consults the same governance pre-write gate `handle_link` already used; the prior asymmetry left a substrate-internal write path ungated.
- **K10 SSE — close `host:` prefix privilege-escalation** ([commit `7496a6e`](https://github.com/alphaonedev/ai-memory-mcp/commit/7496a6e)) — SSE subscription auth no longer accepts a `host:`-prefixed agent id as a substitute for the bound agent; the prefix used to short-circuit the namespace-inheritance check. An anonymous subscriber sees nothing.
- **K10 HMAC — bind method + `pending_id` in canonical request** ([commit `99ffacc`](https://github.com/alphaonedev/ai-memory-mcp/commit/99ffacc)) — the approval API HMAC now signs `(method, pending_id, body_hash)` rather than just `body_hash`; the prior shape allowed a captured signature to be replayed against a different verb or a different pending row.
- **`invalidate_link` BEGIN IMMEDIATE wrap** ([commit `2c77537`](https://github.com/alphaonedev/ai-memory-mcp/commit/2c77537)) — the UPDATE + audit-INSERT pair is now wrapped in a single `BEGIN IMMEDIATE` so a concurrent reader cannot observe the invalidation without the audit row, or vice-versa.
- **Hooks executor — redact secret-shaped stderr** ([commit `cbe934c`](https://github.com/alphaonedev/ai-memory-mcp/commit/cbe934c)) — operator-log + caller-`reason` strings now scrub anything matching `password|secret|key|token|cred` patterns before surfacing; closes the side-channel where a hook subprocess could leak credentials by panicking with them in the message body.
- **K10 HMAC nonce cache — single-use signatures within 300s window** ([commit `a69325f`](https://github.com/alphaonedev/ai-memory-mcp/commit/a69325f)) — replay protection now tracks (signature, nonce) tuples in a 300-second sliding window; a captured signature cannot be replayed even before its timestamp expires. Replay-window tightening from earlier release pass retained.
- **H8 — rebound namespace `Ask` must not silently elevate** ([commit `69ad41c`](https://github.com/alphaonedev/ai-memory-mcp/commit/69ad41c)) — when a namespace's `Ask` policy is rebound to a stricter parent, the prior leaf-resolution short-circuit no longer surfaces the parent's permissive grant; the resolver now walks the full chain on rebind.
- **I1 — `transcripts` decompression cap is config-driven** ([commit `26fab06`](https://github.com/alphaonedev/ai-memory-mcp/commit/26fab06)) — the zstd decompression bound now reads `TranscriptsConfig.max_decompressed_bytes` (default 16 MiB) instead of a compile-time constant; operators can tighten the cap on memory-constrained hosts.
- **K10 SSE — strip lagged-event count to close volume side-channel** ([commit `d1f6c9f`](https://github.com/alphaonedev/ai-memory-mcp/commit/d1f6c9f)) — the SSE `Retry-After` and `X-Lagged-Events` headers no longer surface the exact count of dropped events; an attacker can no longer infer the rate of other subscribers' traffic from the lag signal.
- **SSRF v4-mapped tests use `validate_url_with` explicit flag** ([commit `6b6b3c0`](https://github.com/alphaonedev/ai-memory-mcp/commit/6b6b3c0)) — test-side tightening so the SSRF test fleet exercises the explicit-flag path that production callers now take.

All sixteen fixes are no-op for callers operating inside the substrate's expected envelope; each closes a specific bypass / replay / inference vector surfaced during the v0.7.0 cert sequence or the post-cert security pass.

### Fixed — ship-readiness reconciliation (v0.7.0 final cut)

The reconciliation pass that brought the WT-1 / QW / Batman 6+7 feature trunk together with the release-branch security tip surfaced a handful of latent bugs and discipline drift. All are closed at the v0.7.0 reconciled HEAD.

- **`signed_events::append_signed_event_no_tx` variant** — the K9 governance pre-write hook now writes its audit row via a no-tx variant to avoid nested-transaction collision with the `BEGIN IMMEDIATE` wrap that the `2c77537` `invalidate_link` fix introduced. Audit-honest: the V-4 cross-row hash chain (#698) is preserved because the no-tx writer still walks through the same `prev_hash` + `sequence` increment path; the only difference is the absence of an inner `BEGIN`/`COMMIT` pair.
- **`postgres_schema.sql` + migration `0018_v07_persona.sql` — backfill missing `memory_kind` column** — latent QW-2 bug uncovered during the reconciliation: the persona index `idx_personas_by_entity` referenced `memory_kind` but the postgres schema had not yet added the column. The reconciliation backfills the column in `postgres_schema.sql` and ports the migration so a fresh postgres bootstrap matches the SQLite parity.
- **`examples/atomise_roundtrip.rs` Memory{} literal updated** for the Form 4/5 field additions (`citations`, `source_uri`, `atom_span` from Form 4; the per-memory `confidence` source-tracking fields from Form 5). The example continues to build and the round-trip property holds.
- **`memory_calibrate_confidence` MCP tool description trimmed to 38 `cl100k_base` tokens** (was 55, exceeded the c2 per-tool token budget gate). The static schema-size CI assertion (`crate::sizes`) gates the trimmed wire form.
- **14 `sign_approve_body` test call sites updated** for K10 HMAC method+`pending_id` binding lockstep — the canonical-request shape change at `99ffacc` required every caller in the test fleet to pass the verb + pending row id.
- **`executor_error_child_exit_with_signaled_code` assertion updated** for the stderr-redaction discipline introduced at `cbe934c` — the test expected the raw secret-shaped stderr to surface in the panic message; the assertion now expects the redacted form.

### Schema migrations (this release)

- **sqlite: v34 → v35** (signed_events V-4 closeout midpoint, #698) → **v36** (WT-1-A atomisation foundation: `atomised_into` / `atom_of` / `derives_from` link relations + CHECK constraint extension; `migrations/sqlite/0030_v07_atomisation.sql`) → **v37** (QW-2 persona substrate primitive: `personas` table + `idx_personas_by_entity` index; `migrations/sqlite/0031_v07_persona.sql`) → **v38** (Form 4 fact-provenance: per-memory `citations` / `source_uri` / `atom_span` columns; `migrations/sqlite/0032_v07_form4_provenance.sql`) → **v39** (Form 5 confidence calibration: `confidence_observations` shadow-mode table + `confidence_baselines` calibration store; `migrations/sqlite/0033_v07_form5_confidence_calibration.sql`). `CURRENT_SCHEMA_VERSION = 39` in `src/storage/migrations.rs`.
- **postgres: v34 → v35** (WT-1-A; `migrations/postgres/0017_v07_atomisation.sql`) → **v36** (QW-2; `migrations/postgres/0018_v07_persona.sql`) → **v37** (Form 4; `migrations/postgres/0019_v07_form4_provenance.sql`) → **v38** (Form 5; `migrations/postgres/0020_v07_form5_confidence_calibration.sql`). `CURRENT_SCHEMA_VERSION = 38` in `src/store/postgres.rs`. Postgres remains one ahead of SQLite at the v15 → v34 land where the V-4 closeout SQLite midpoint v35 has no postgres counterpart (postgres lands the chain at its own v33 → v34 step). Parity test [`tests/postgres_schema_parity.rs`](tests/postgres_schema_parity.rs) pins the equivalence.

### MCP tool surface

- **Full profile: 71 tools** (up from the 63 advertised in the initial v0.7.0 framing). **Family::Power: 22 tools.**
- **New tools added in this release** (delta vs the v0.7.0 initial framing):
  - `memory_atomise` (Family::Power) — WT-1-C, PR #751
  - `memory_offload` (Family::Power) — QW-3, [`2a85db2`](https://github.com/alphaonedev/ai-memory-mcp/commit/2a85db2) + [`20b6be1`](https://github.com/alphaonedev/ai-memory-mcp/commit/20b6be1)
  - `memory_deref` (Family::Power) — QW-3
  - `memory_persona` — QW-2, PR #749
  - `memory_persona_generate` — QW-2
  - `memory_export_reflection` — QW-1, [`6d32633`](https://github.com/alphaonedev/ai-memory-mcp/commit/6d32633)
  - `memory_ingest_multistep` (Family::Power) — Form 3, PR #763
  - `memory_calibrate_confidence` (Family::Power) — Form 5, PR #766
- **New CLI-only surfaces** (not exposed as MCP tools):
  - `ai-memory atomise <memory-id>` — WT-1-F
  - `ai-memory export-reflections` — QW-1
  - `ai-memory governance install-defaults` — 7th-form, PR #761
  - `ai-memory calibrate-confidence` — Form 5
- The v0.6.4 `--profile core` 7-tool default surface is unchanged; every new tool is registered under `Family::Power` so the keyword-tier `core` profile remains at the minimum.

### Capabilities-v3 — new application blocks

The v3 response shape gains three application blocks (additive over v2 — v2 wire shape remains supported through the v0.7.x line):

- **`atomisation`** ([`CapabilityAtomisation`](src/config.rs)) — WT-1-G. Reports `status` (`stub` / `implemented`), curator backend identifier, per-atom token cap, and the `auto_atomise` namespace-policy surface (the policy fields the substrate honours).
- **`memory_kinds_vocab`** ([`CapabilityMemoryKindVocab`](src/config.rs)) — Form 6. Reports the full Batman vocabulary `{Observation, Reflection, Persona, Skill, Concept, Entity, Claim, Relation, Event, Conversation, Decision}` and the `auto_classify_kind` namespace-policy surface.
- **`confidence_calibration`** ([`CapabilityConfidenceCalibration`](src/config.rs)) — Form 5. Reports the three opt-in feature flags (`auto_confidence`, `confidence_shadow`, `confidence_decay`) and their advertised status (`unimplemented` / `shadow_mode` / `implemented`) so an agent can interrogate whether to trust the substrate's derived confidence value.

The L1-1 `memory_kinds` v2 list (`["observation", "reflection"]`) stays unchanged for wire-compat; the new `memory_kinds_vocab` block is the v3-only surface advertising the Batman extension.

### Env vars — new in this release

- **`AI_MEMORY_AUTO_CONFIDENCE`** (Form 5) — `1` to enable the per-source-namespace baseline `confidence` derivation at write time. Defaults off; advertised status flips to `implemented` when set.
- **`AI_MEMORY_CONFIDENCE_SHADOW`** (Form 5) — `1` to enable side-channel observation recording for offline calibration. Defaults off; advertised status `shadow_mode` when set.
- **`AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE`** (Form 5) — `0.0..=1.0` (default `1.0`) — sampling rate for the shadow recorder.
- **`AI_MEMORY_CONFIDENCE_DECAY`** (Form 5) — `1` to enable the exponential freshness-decay model.

### Namespace policy fields — new on `GovernancePolicy`

Each field is `Option<...>` and inherits leaf-first through the existing `resolve_governance_policy` chain walk:

- **`auto_export_reflections_to_filesystem: Option<bool>`** — QW-1, drives `post_reflect::auto_export`.
- **`auto_atomise: Option<bool>`** — WT-1-D, enables `pre_store::auto_atomise`.
- **`auto_atomise_threshold_cl100k: Option<u32>`** — WT-1-D, content-size gate for the auto-atomise hook.
- **`auto_atomise_max_atom_tokens: Option<u32>`** — WT-1-D, per-atom token cap the engine targets.
- **`auto_atomise_mode: Option<AutoAtomiseMode>`** — Form 2 (`Off` / `Deferred` / `Synchronous`). `Synchronous` atomises before the embed call.
- **`auto_persona_trigger_every_n_memories: Option<u32>`** — QW-2, drives `post_reflect::auto_persona`.
- **`auto_export_personas_to_filesystem: Option<bool>`** — QW-2.
- **`legacy_per_pair_classifier: Option<bool>`** — Form 1, keeps the v0.6.0.0 post-store per-pair classifier reachable for backwards compatibility.
- **`auto_classify_kind: Option<MemoryKindAutoClassify>`** — Form 6 (`Off` / `RegexOnly` / `RegexThenLlm`), drives `pre_store::auto_classify_kind`.

### Docs — new in this release

- [`docs/atomisation.md`](docs/atomisation.md) — WT-1 atomisation primitive overview + WT-1-G capability block reference.
- [`docs/persona.md`](docs/persona.md) — QW-2 persona-as-artifact substrate primitive.
- [`docs/context-offload.md`](docs/context-offload.md) — QW-3 context-offload substrate primitive + `memory_offload` / `memory_deref` reference.
- [`docs/positioning.md`](docs/positioning.md) — QW-4 competitive landscape including TencentDB Agent Memory entry.
- [`docs/v0.7.0/test-config.md`](docs/v0.7.0/test-config.md) — pins grok-4.3 + `reasoning_effort=medium` as the canonical xAI config for the v0.7.0 test fleet ([commit `41229d1`](https://github.com/alphaonedev/ai-memory-mcp/commit/41229d1)).
- [`docs/multistep-ingest.md`](docs/multistep-ingest.md) — Form 3 multi-step ingest orchestrator (two-phase deterministic + LLM with prompt-cache reuse).
- [`docs/provenance.md`](docs/provenance.md) — Form 4 fact-provenance citations + source-as-URI + atom-grain span.
- [`docs/confidence-calibration.md`](docs/confidence-calibration.md) — Form 5 auto-confidence + shadow-mode + freshness decay + calibration tooling.
- [`docs/memory-kind-vocab.md`](docs/memory-kind-vocab.md) — Form 6 `MemoryKind` Batman vocabulary + recall filter + optional auto-classify.
- [`docs/governance/agent-action-rules.md`](docs/governance/agent-action-rules.md) — 7th-form agent-EXTERNAL action rule reference (extended from prior K9 doc).
- [`docs/internal/batman-framework-audit.md`](docs/internal/batman-framework-audit.md) — adversarial procurement-grade audit deliverable (PR #753).

### Cookbook — new in this release

- [`cookbook/atomisation/01-basic-flow.sh`](cookbook/atomisation/01-basic-flow.sh) — WT-1 store → atomise → recall round-trip.
- [`cookbook/persona/01-build-persona-from-observations.sh`](cookbook/persona/01-build-persona-from-observations.sh) — QW-2 build persona from reflection cluster.
- [`cookbook/context-offload/01-offload-large-tool-output.sh`](cookbook/context-offload/01-offload-large-tool-output.sh) — QW-3 offload + deref round-trip.
- [`cookbook/file-backed-export/01-export-and-inspect.sh`](cookbook/file-backed-export/01-export-and-inspect.sh) — QW-1 reflection-chain export + inspect.
- [`cookbook/multistep-ingest/01-two-phase.sh`](cookbook/multistep-ingest/01-two-phase.sh) — Form 3 two-phase ingest with prompt-cache reuse.
- [`cookbook/agent-external-governance/01-deny-bash.sh`](cookbook/agent-external-governance/01-deny-bash.sh) — 7th-form Layer-4 deny-bash rule installation.

### Removed / Deprecated

- The pre-2026-05-15 v0.7.0 headline tag "release pending Wave 1-4 cert" is superseded by this reconciled state. Wave 1-4 has long landed; the active gate is the v0.7.0 reconciled HEAD (`64528b1`) which folds WT-1 + QW + Batman 6+7 + audit + security hardening into a single shippable cut.
- The v0.6.0.0 post-store per-pair binary yes/no contradiction classifier is **superseded** by the Form 1 batch action-emitting synthesis path. The legacy classifier remains reachable via `legacy_per_pair_classifier: Some(true)` on the namespace policy for callers that need the v0.6.x shape — flagged for removal in v0.8.0.

## [0.7.0-release-branch-headline] — 2026-05-06 — `attested-cortex` (initial release-cut narrative, superseded by 2026-05-09 reconciled headline above)

**Headline:** v0.7.0 closes the `attested-cortex` epic — **69/69 tasks across 11 tracks** (A/B/C/D/E/F/G/H/I/J/K). The substrate becomes both **more articulate** (capabilities v3 with pre-computed calibration strings, named loaders, 52% MCP-tool token reduction on the full profile) and **cryptographically trustworthy** (per-agent Ed25519 attestation with append-only `signed_events` audit chain, sidechain transcripts with `memory_replay`, programmable 20-event hook pipeline, opt-in Apache AGE acceleration, K1/G1 namespace-inheritance enforcement, real permission system with deny-first semantics, A2A maturity). Canonical scope: [`docs/v0.7/V0.7-EPIC.md`](docs/v0.7/V0.7-EPIC.md). Migration: [`docs/MIGRATION_v0.7.md`](docs/MIGRATION_v0.7.md). What's new: [`docs/whats-new-v07.html`](docs/whats-new-v07.html). RFC: [`docs/v0.7/rfc-attested-cortex.md`](docs/v0.7/rfc-attested-cortex.md).

> **Backward compatibility.** v3 capabilities are additive over v2; existing v0.6.4 SDKs continue to work against a v0.7.0 server. v0.6.4's `--profile core` 5-tool default surface is unchanged. The hook pipeline is **default off** — a v0.7.0 install with no `hooks.toml` behaves identically to v0.6.4 at the lifecycle layer. Schema migrations v20 → v22 (`audit_log` → `signed_events` → `memory_transcripts`) run automatically on first start and are idempotent.

### Track summary (11 tracks, 69 tasks)

- **Track A — Capabilities v3 response shape (5 tasks).** Adds `summary`, `to_describe_to_user`, `callable_now`, `agent_permitted_families` to the `memory_capabilities` response, plus `schema_version="3"` (additive over v2). Pre-computed per-agent calibration strings let LLMs converge on accurate first-answer descriptions instead of improvising. v3 fields are additive — v2 wire shape stays supported through the v0.7.x line. Canonical phrasings pinned in [`docs/v0.7/canonical-phrasings.md`](docs/v0.7/canonical-phrasings.md).
- **Track B — Loader tools (5 tasks).** `memory_load_family` and `memory_smart_load(intent)` are promoted to **always-on first-class tools** (no longer hidden inside an introspection tool's parameter set). Reasoning-class LLMs find them on first ask. Includes harness detection from MCP `clientInfo` (Claude Code, Codex, Grok CLI, Gemini CLI, Continue, Cursor, Cline, Aider, Goose, Claude Desktop, generic JSON-RPC) and family-descriptor embeddings powering `memory_smart_load`'s intent-to-family routing.
- **Track C — Schema compaction (5 tasks).** **52% MCP tool-token reduction** on the full profile. Description / docs split (long form moved to per-tool docs links), optional params hidden from default schema, inline examples stripped, hard CI gate enforces ≤ 3,500 input tokens for `--profile full` `tools/list`. Combined with v0.6.4's 76.4% default-profile reduction, the cortex now ships at < 3.5K tokens even when fully loaded.
- **Track D — Per-harness positioning + tests (4 tasks).** Cross-harness benchmark across the 11 supported harnesses; landing-page compatibility matrix at [`docs/v0.7/compatibility-matrix.html`](docs/v0.7/compatibility-matrix.html); install-time system-prompt snippet emitted by `ai-memory install`; harness integration tests in `tests/harness_*.rs` covering both 5-tool default and full-profile loading paths.
- **Track E — Discovery Gate T0 calibration cells (3 tasks).** Discovery Gate T1-T3 loader cells; T0 orchestration script driving 4 LLMs (Claude, Grok, Gemini, GPT) for ≥ 95% convergence verification on canonical phrasings; post-ship convergence verification scheduled against the released binary. See [`docs/v0.7/T0-ORCHESTRATION.md`](docs/v0.7/T0-ORCHESTRATION.md).
- **Track F — Docs + release (6 tasks).** [`docs/MIGRATION_v0.7.md`](docs/MIGRATION_v0.7.md) v0.6.4 → v0.7.0 guide; [`docs/whats-new-v07.html`](docs/whats-new-v07.html) what's-new page; [`docs/v0.7/rfc-attested-cortex.md`](docs/v0.7/rfc-attested-cortex.md) RFC; `README.md` + `docs/ADMIN_GUIDE.md` updates; top-nav badges; this release-cut PR.
- **Track G — Hook Pipeline (11 tasks, Bucket 0).** The substrate ships: `~/.config/ai-memory/hooks.toml` config file; **25 lifecycle event types** with payloads — the Track G 20 baseline (`pre_store`, `post_store`, `pre_recall`, `post_recall`, `pre_search`, `post_search`, `pre_delete`, `post_delete`, `pre_promote`, `post_promote`, `pre_link`, `post_link`, `pre_consolidate`, `post_consolidate`, `pre_governance_decision`, `post_governance_decision`, `on_index_eviction`, `pre_archive`, `pre_transcript_store`, `post_transcript_store`) plus 5 grand-slam additions (`pre_recall_expand` G10 + `pre_reflect`/`post_reflect` recursive-learning Task 6/8 + `pre_compaction`/`on_compaction_rollback` L1-7), enumerated in `src/hooks/events.rs::HookEvent`; `ExecExecutor` + `DaemonExecutor` JSON-stdio IPC; decision types (`Allow`/`Deny`/`Modify`/`Defer`); chain ordering with priority; per-event timeouts; hot reload on `hooks.toml` mtime change; `on_index_eviction` for HNSW/cache eviction observability; reranker batching for concurrent recall; `pre_recall` daemon-mode hook; **R3 auto-link reference detector** as a reference hook binary.
- **Track H — Ed25519 Attested Identity (6 tasks, Bucket 1).** `ai-memory identity generate` CLI mints per-agent Ed25519 keypairs; outbound link signing fills the v0.6.3 `memory_links.signature` "dead column"; inbound signature verification on every link write; `attest_level` enum (`unsigned` / `signed` / `verified` / `rejected`); `memory_verify` MCP tool surfaces signature state on demand; **append-only `signed_events` audit table** with hash-chained provenance; end-to-end test pinning the full mint → sign → verify → audit cycle.
- **Track I — Sidechain Transcripts (5 tasks, Bucket 1.7).** `memory_transcripts` schema (BLOB + zstd-3); `memory_transcript_links` join table; per-namespace TTL with exact-match → longest `prefix/*` → `*` → default-off precedence; `memory_replay` MCP tool reconstructs full conversation context from a transcript link; **R5 `pre_store` transcript-extraction reference hook** ships as a standalone Rust binary at `tools/transcript-extractor/` (kept out of the published crates.io upload via the parent `Cargo.toml`'s `include` allowlist).
- **Track J — Apache AGE Acceleration (8 tasks, Bucket 2).** AGE detected at Postgres-SAL connect-time via `pg_extension` probe (logged-only fallback to CTE on missing extension or probe error); Cypher implementations of `kg_query`, `kg_timeline`, `kg_invalidate`, and **R2 `find_paths`**; dual-path tests gated on `AI_MEMORY_TEST_AGE_URL`; AGE / CTE per-query performance budgets with bench-time gate; `KgBackend { Cte, Age }` enum exposed via `Capabilities` (`kg_backend` field) for `ai-memory doctor` and `memory_capabilities`.
- **Track K — A2A + Permissions + G1 cutline (11 tasks, Bucket 3).** **K1/G1 namespace-inheritance enforcement** (the mandatory cutline — `resolve_governance_policy` walks the namespace chain; first non-null policy wins); `pending_actions` timeout sweeper (closes the v0.6.3.1 `default_timeout_seconds` honesty disclosure); `permissions.mode` enforcement gate (`advisory` preserves v0.6.4 first-boot semantics, `enforce` deny-firsts); approval-event routing; `permissions.rule_summary` re-instated; A2A correlation IDs + ACK retries + TTL + replay protection; subscription DLQ + replay-from-cursor + HMAC; per-agent quotas with daily reset; unified permission pipeline (rules + modes + hooks → decision); approval API on **HTTP + SSE + MCP** with HMAC and `remember=forever`; `ai-memory governance migrate-to-permissions` translator CLI for upgrading v0.6.x governance configs.

### Quality

- **Hard coverage gate ≥ 93%.** CI fails any PR below the line floor.
- **Clippy `-D pedantic` clean baseline** restored across nine files (#614).
- **Test race fixes** for the subscription `dispatch_count` race, the snippet env race, the keypair env race, the binary-spawn flake on macOS (OnceLock + PID-scoped target), and the b3 budget race.
- **52% MCP tool token reduction** on the full profile (Track C), measured against `cl100k_base`.
- **CI token budget gate** — hard 3,500-token ceiling on `--profile full` `tools/list` (Track C5).

### Follow-ups (post-v0.7.0)

- **v0.7.0.1 — issue [#625](https://github.com/alphaonedev/ai-memory-mcp/issues/625):** E1/E2 cross-platform Rust binaries for the Discovery Gate T0 / T1-T3 loader cell harnesses (currently shell-only on macOS / Linux).

---

### Granular task notes (folded forward from prior `[Unreleased]` block)

The following per-task entries were authored as v0.7 tracks landed and are preserved here for reviewers tracing PR-level provenance:

- **v0.7.0 I5 — R5 reference `pre_store` transcript-extraction hook.**
  New standalone Rust binary at `tools/transcript-extractor/`
  (`ai-memory-transcript-extractor` crate, kept out of the published
  crates.io upload via the parent `Cargo.toml`'s `include` allowlist).
  The binary reads the same JSON `FireEnvelope` shape
  (`src/hooks/executor.rs::FireEnvelope`) the production executor (G3)
  writes to a hook subprocess, classifies the in-flight memory as a
  transcript via three independent signals
  (`metadata.kind == "transcript"`, namespace prefix
  `transcript/`/`transcripts/`, or speaker tokens like `User:` /
  `Assistant:` / `<|user|>` in the first 512 chars of content),
  splits the content into paragraphs scored by a token-bag density
  heuristic, and surfaces the top-K survivors as
  `delta.metadata.extracted_memories` on a `Modify` decision —
  preserving any existing metadata keys an upstream hook already
  wrote. Each candidate carries a `score`, byte-span `span_start`/
  `span_end` into the source content, and a 80-char-capped `title`
  for the future `post_store` mint companion to fold into a
  `memory_transcript_links` row. Both stdio framings are supported:
  one-shot (default; matches `ExecExecutor`) and `--daemon`
  (newline-delimited JSON; matches `DaemonExecutor`). The substrate
  is the deliverable — the heuristic itself is *deliberately* a
  bag-of-words approximation rather than an LLM call (see the
  binary's README) so the reference impl runs in CI without an
  Ollama daemon and without dragging the full `ai-memory` dep
  graph into the tool. New per-namespace opt-in field
  `TranscriptNamespaceConfig.auto_extract` (defaults `None` → off)
  with matching resolver `TranscriptsConfig::auto_extract_for`
  applying the same exact-match → longest `prefix/*` → `*` →
  default-off precedence the I3 TTL resolver uses; 4 unit tests
  cover the resolver. The reference binary ships 14 unit tests
  (envelope round-trip in both modes, all three classification
  signals, stop-word filtering, paragraph chunking floor,
  `EXTRACTOR_TOP_K` env clipping, metadata-key preservation,
  malformed-input degrade-to-Allow, byte-span correctness).
  New integration test `tests/transcript_extractor.rs` builds the
  sibling binary on the fly and asserts the end-to-end stdio
  contract (extraction fires for a transcript memory, returns
  `Allow` for non-transcript memories, falls through to `Allow` on
  the wrong event class) plus the namespace opt-in resolver. R5
  commitment recovered; production tightening of the heuristic is
  scoped to a follow-up post-G11 task that will register the
  `post_store` mint companion.
- **v0.7.0 G2 — 20 hook lifecycle event types with payloads.** New
  `src/hooks/events.rs` module attaches a JSON-serializable payload
  struct to every variant of `HookEvent` (lifted out of G1's
  `src/hooks/config.rs` stub; re-exported from the G1 path for
  back-compat). The 20 events the hook pipeline supports:
  `pre_store`, `post_store`, `pre_recall`, `post_recall`,
  `pre_search`, `post_search`, `pre_delete`, `post_delete`,
  `pre_promote`, `post_promote`, `pre_link`, `post_link`,
  `pre_consolidate`, `post_consolidate`, `pre_governance_decision`,
  `post_governance_decision`, `on_index_eviction`, `pre_archive`,
  `pre_transcript_store`, `post_transcript_store`. Pre-events carry
  writable deltas (`MemoryDelta`, `RecallQuery`, `SearchQuery`,
  `MemoryRef`, `PromoteDelta`, `LinkDelta`, `ConsolidationDelta`,
  `GovernanceContext`, `TranscriptDelta`); post-events carry
  read-only snapshots (`Memory`, `RecallResult`, `SearchResult`,
  `MemoryRef`, `PromoteResult`, `Link` (= `MemoryLink` re-export),
  `ConsolidationResult`, `GovernanceDecision`, `EvictionEvent`,
  `Transcript`). The `Link` and `Transcript` wire types reuse / project
  from `crate::models::MemoryLink` and `crate::transcripts::Transcript`
  respectively. Every variant carries a doc-comment naming the
  source-code location G3-G11 will hook into. Hooks are not yet fired
  at the memory operation points — that's G3-G11. New round-trip JSON
  tests cover all 20 variants and one representative payload per
  family.
- **v0.7.0 J1 — Apache AGE detection in Postgres SAL.** New
  `KgBackend { Cte, Age }` enum (snake-case serde) lives at
  `src/store/mod.rs`; the Postgres adapter probes
  `SELECT 1 FROM pg_extension WHERE extname='age'` at connect time and
  records the resolved tag on the `PostgresStore` handle. AGE is
  opt-in: a missing extension OR a probe error falls back to
  `KgBackend::Cte` (logged at `debug`, never blocks bootstrap). The
  resolved backend is exposed via `PostgresStore::kg_backend()` so
  Track J's downstream tasks (J2 `kg_query`, J3 `kg_timeline`,
  J4 `kg_invalidate`, J7 `find_paths`) can dispatch on it. Added an
  optional `kg_backend: Option<String>` field on the v2 + v3
  `Capabilities` documents (skipped from the JSON wire when `None`)
  so `ai-memory doctor` and `memory_capabilities` can surface the
  active path once the SAL adapter is threaded through `AppState` in
  J2. Substrate only — no behavioural change to existing
  `memory_kg_*` MCP tools in this PR. New tests: 4 unit
  (snake-case wire shape, default tag pin, accessor wiring) plus 3
  live tests gated on `AI_MEMORY_TEST_AGE_URL` /
  `AI_MEMORY_TEST_POSTGRES_URL`.
- **v0.7.0 K2 — `pending_actions` timeout sweeper.** Closes the
  v0.6.3.1 honest-Capabilities-v2 disclosure that
  `default_timeout_seconds` was advertised in v1 but unused. Schema
  bumped to v21: `pending_actions` gains nullable
  `default_timeout_seconds` (per-row TTL) and `expired_at` (RFC3339
  stamp set when the sweeper fires) plus a composite
  `(status, requested_at)` index. New `db::sweep_pending_action_timeouts`
  helper is driven by a 60-second background tokio task spawned from
  `daemon_runtime::bootstrap_serve`; per-row override beats the
  cluster default (24h, matching `doctor`'s CRIT window). Each
  expired row fires a `pending_action_expired` event through the
  existing subscription dispatcher. A non-positive global default
  disables the sweeper entirely (operator escape hatch). 7 new
  tests cover the unit + integration paths.
- **Boot follow-ups folded from v0.6.4 into v0.6.3.1 (PR-9h, issue #487
  PR #497 reqs #72 + #73)** — version-drift detection adds
  `MIN_SUPPORTED_SCHEMA = 16` / `MAX_SUPPORTED_SCHEMA = 19` constants in
  `src/cli/boot.rs`, a new `WarnSchemaUnsupported { db_schema }`
  manifest variant, and the JSON top-level `schema_supported: bool`
  field for SIEM ingest. Boot privacy controls add a `[boot]` config
  block with `enabled` (default `true`; `false` exits 0 silently with
  empty stdout AND empty stderr — the privacy-sensitive escape hatch
  for hosts where memory titles must not enter CI logs) and
  `redact_titles` (default `false`; `true` keeps the manifest header
  but replaces every body row's `title` with `<redacted>`). Env-var
  `AI_MEMORY_BOOT_ENABLED=0` takes precedence over the config-file
  value. Documented in `docs/integrations/claude-code.md` and
  `docs/integrations/README.md`.
- **`ai-memory doctor` CLI (Phase P7 / R7)** — operator-visible health
  dashboard. New subcommand
  `ai-memory doctor [--db <path>] [--remote <url>] [--json] [--fail-on-warn]`
  produces a 7-section health report (Storage, Index, Recall, Governance,
  Sync, Webhook, Capabilities) with per-section severity tagging
  (`INFO` / `WARN` / `CRIT` / `N/A`). Exits `0` healthy / `1` warning
  with `--fail-on-warn` / `2` critical. `--remote <url>` queries a live
  daemon's `/api/v1/capabilities` + `/api/v1/stats` endpoints to support
  fleet-wide health sweeps at T3+. Read-only — never mutates the DB;
  every query is a single indexed `COUNT(*)` so the lock window stays
  sub-millisecond on a populated store. Consumes Capabilities v2 (P1),
  data integrity (P2 — `embedding_dim`), and recall observability (P3 —
  eviction counter, recall_mode distribution) surfaces with graceful
  fallback when those phases haven't merged yet — pre-P2/P3 schemas
  render the affected fields as `not_observed (pre-PX schema)` instead
  of erroring. New helpers in `src/db.rs`: `doctor_dim_violations`,
  `doctor_oldest_pending_age_secs`, `doctor_governance_coverage`,
  `doctor_governance_depth_distribution`,
  `doctor_webhook_delivery_totals`, `doctor_max_sync_skew_secs`. New
  module `src/cli/doctor.rs` and integration tests in
  `tests/doctor_cli.rs` (4 acceptance tests:
  `doctor_reports_clean_on_fresh_db`, `doctor_warns_on_dim_violations`,
  `doctor_critical_on_pending_actions_older_than_24h`,
  `doctor_remote_queries_capabilities_endpoint`). Documented in
  `docs/operations/doctor.md`.

### Phase P6 (R1) — `budget_tokens` recall recovery

Recovered the prior phased ROADMAP's "killer feature, no competitor has
this." `memory_recall` (MCP / HTTP / CLI) accepts an optional
`budget_tokens` parameter and returns the highest-ranked memories whose
cumulative content tokens fit under the budget, using the deterministic
`tiktoken-rs` `cl100k_base` BPE — the same tokenizer Claude / GPT use
for context-window accounting. The R1 always-return-at-least-one
guarantee surfaces an overflow flag rather than dropping a top-ranked
hit when the caller asks for an unrealistically tight budget.

- `tiktoken-rs` 0.7 added (pure-Rust BPE; ~1.7 MB bundled table; offline
  deterministic).
- New response `meta` block when a budget is supplied:
  `budget_tokens_used`, `budget_tokens_remaining`, `memories_dropped`,
  `budget_overflow`. Legacy top-level `tokens_used` / `budget_tokens`
  fields preserved verbatim — pre-P6 callers continue to work
  byte-for-byte.
- `budget_tokens=0` is now a valid request meaning "give me nothing"
  (returns an empty memories array with `meta.budget_overflow=false`).
  Supersedes the v0.6.3 Ultrareview #348 hard-reject of 0 — the meta
  block now disambiguates "user asked for zero" from "buggy
  uninitialised counter" by always round-tripping the requested budget.
- Budget-unset path is unchanged on the recall hot path: cl100k_base
  is skipped entirely, `tokens_used` falls back to a fast `len/4` byte
  heuristic so the bench harness's `recall_hot` p95 budget (< 50 ms)
  is preserved.
- Documentation: new `docs/recall.md`; `PERFORMANCE.md` gets a new row
  for `memory_recall (budget, budget_tokens=4096)` at < 90 ms p95
  (autonomous tier budget).
- Scoring and fusion are unchanged — budget is a strict post-rank
  filter. Two recalls of the same query with different budgets produce
  a strict prefix-of-prefix relationship.

Acceptance tests in `tests/budget_tokens.rs`.

### Phase P2 — Data-integrity hardening (G4, G5, G6, G13)

Schema **v18** (migration `0011_v0631_data_integrity.sql`) closes four
silent-corruption / silent-mutation paths surfaced by the v0.6.3 audit.
(Schema v17 was claimed by P4 governance-inheritance backfill — see below.)

- **G4 — mixed embedding dims silently tolerated.** New
  `memories.embedding_dim` and `archived_memories.embedding_dim` columns;
  `db::set_embedding` enforces "first write establishes the namespace's
  dim" and returns a typed `EmbeddingDimMismatch` on any subsequent
  write at a different dim. New `Stats::dim_violations` counter (also
  exposed via `db::dim_violations`) surfaces legacy mismatched rows so
  the P7 doctor can flag them. Migration backfills existing rows from
  `length(embedding) / 4`.
- **G5 — archive lossy + restore resets.** `archived_memories` now
  carries `embedding`, `embedding_dim`, `original_tier`, and
  `original_expires_at`. `archive_memory`, `gc(archive=true)`, and
  `forget(archive=true)` populate them; `restore_archived` round-trips
  the original tier and expiry instead of forcing `tier='long'` /
  `expires_at=NULL`. Pre-v17 archive rows are backfilled to
  `original_tier='long'` (the loss is acknowledged — the live row was
  gone before v17 ever shipped).
- **G6 — UNIQUE(title, namespace) silent merge.** `memory_store` MCP
  tool grows an `on_conflict: error | merge | version` parameter.
  Capability negotiation: v2-aware MCP clients default to `error`; v1 /
  unknown clients keep the legacy `merge` upsert. HTTP
  `POST /api/v1/memories` accepts `on_conflict` in the body and
  defaults to `error` (HTTP has no v1 backward-compat to honour). New
  `db::find_by_title_namespace` and `db::next_versioned_title` helpers.
- **G13 — f32 endianness magic byte.** Embedding BLOBs now carry a
  one-byte header (`0x01` = LE-f32). Readers tolerate missing-header as
  legacy LE-f32 and return a typed `EmbeddingFormatError` for any
  unknown header; `0x02` (BE-f32) is reserved and rejected until v0.7
  adds the conversion path. New `embeddings::encode_embedding_blob` /
  `decode_embedding_blob` / `decoded_dim` helpers.

Tests: `tests/data_integrity_v17.rs` (8 cases — every charter-cited
acceptance test passes plus two doctor-stat round-trips).

### Capabilities v2 honesty schema (P1, REMEDIATIONv0631 §"Phase P1")

The capabilities response was promising features that did not exist. v2
keeps the wire envelope but tells the truth about what's wired.

**Schema changes — bumped at the same `schema_version="2"` discriminator.**

- **`features.recall_mode_active`** (new): live runtime tag —
  `"hybrid"` when the embedder is loaded, `"degraded"` when configured
  but failed to materialize, `"disabled"` for the keyword tier.
  Operators can refuse to dispatch semantic-recall scenarios against a
  daemon whose embedder did not load.
- **`features.reranker_active`** (new): derived from the actual
  `CrossEncoder` enum variant — `"neural"` / `"lexical_fallback"` /
  `"off"`. Replaces the previous "trust the tier flag" reporting.
- **`features.memory_reflection`** is now a `{planned, version,
  enabled}` object (was `bool`). The subsystem is roadmap (v0.7+); the
  bool form lied by claiming the feature was wired on the autonomous
  tier.
- **`compaction`** and **`transcripts`** carry the same planned-feature
  shape, so operators can distinguish "feature exists but disabled"
  from "feature not in this build."
- **`permissions.mode = "advisory"`** (was `"ask"`, which implied an
  interactive prompt loop the code does not run). Until P4 ships the
  enforcement gate, governance metadata is recorded but not enforced.
- **Dropped fields** (no backing implementation existed):
  `permissions.rule_summary`, `hooks.by_event`,
  `approval.subscribers`, `approval.default_timeout_seconds`.

**Backward compatibility — v1 clients continue to work.** Pass
`Accept-Capabilities: v1` (HTTP) or the MCP `accept: "v1"` argument to
`memory_capabilities` to receive the legacy pre-v0.6.3.1 shape. v1
projection collapses `memory_reflection` back to a bool and drops all
v2-only blocks. Default response remains v2.

**Files touched:** `src/config.rs`, `src/mcp.rs`, `src/handlers.rs`,
`tests/capabilities_v2.rs` (new). 9 new integration tests pin the honest
contract.


## [v0.6.3] — 2026-04-27 — STRUCTURED MEMORY + PERFORMANCE

The grand-slam release. Hierarchical namespace taxonomy + temporal-validity
knowledge graph + entity registry + duplicate detection + bench tool with
public p95 budgets — six streams (A through F) shipped together. Plus
post-rc1 capabilities schema v2 (additive `schema_version="2"` + 5 new
top-level blocks for hooks/permissions/compaction/approval/transcripts
introspection) and a CI coverage gate locking in 93.05% baseline.

**Validation evidence:**

- 1 600 lib tests pass; line coverage **93.08%** (gate floor 92%)
- Ship-gate campaign run #25007261531 — 4 phases green in 14m wall
  (Phase 1 functional · Phase 2 multi-agent W=2/N=3 · Phase 3 v0.6.2→v0.6.3
  migration · Phase 4 chaos 50 cycles kill_primary_mid_write)
- A2A-gate campaign run #25007946890 — 48 scenarios green in 28m wall
  (35 v0.6.0 baseline + 4 auto-append + 9 new for v0.6.3:
  capabilities_v2_schema, taxonomy_walk, kg_query_temporal, kg_timeline,
  entity_aliases, check_duplicate, lifecycle_end_to_end, sqlcipher_at_rest,
  autonomous_tier_suite). Cell: ironclaw-mtls.

Live evidence:
<https://alphaonedev.github.io/ai-memory-test-hub/releases/v0.6.3/>

### Distribution-channel hardening (folded into v0.6.3 final cut)

- **Dockerfile — `COPY migrations/`** added so cargo build can resolve
  the new Stream A-C `include_str!` references at compile time. Without
  it, the Docker build failed before publish.
- **Dockerfile — pin build stage to `rust:1.94-slim-bookworm`** so the
  produced binary's glibc matches the runtime stage
  (`debian:bookworm-slim`, glibc 2.36). Without the explicit bookworm
  pin, `rust:1.94-slim` resolves to a trixie-based image (glibc 2.41)
  and the binary fails at startup with `version GLIBC_2.39 not found`.
- **`Cargo.toml` `package.include`** restricts the published crate to
  source-only (src, benches, examples, migrations, build.rs,
  Cargo.{toml,lock}, README.md, LICENSE, CHANGELOG.md, PERFORMANCE.md).
  Without it, the crate weighs 22 MiB compressed (140 MiB unpacked,
  thanks to `audits/`) — over crates.io's 10 MiB upload limit; uploads
  hit HTTP 503 from the Fastly WAF. Trimmed crate is 558 KiB compressed
  (73 files), well under the limit.
- **CI silent-failure on `cargo publish`** — replaced
  `cargo publish || echo "warning"` with proper retry-with-backoff
  (3 attempts × 30s sleep). Genuine "version already exists" detected
  via stderr grep (idempotent re-run); everything else (5xx, network
  errors, oversized package) fails the job loudly. This is the masking
  bug that hid the crates.io 503s during initial v0.6.3 publish.
- **New `dockerfile-validate` CI job** runs on every push + PR. Builds
  the Docker image (no GHCR push) and smoke-tests with
  `docker run --rm ai-memory:ci-validate --version` + `--help`. Closes
  the Dockerfile-drift class of bugs (new `include_str!` for missing
  dir, missing system dep, glibc mismatch, etc.) at PR time, not at
  release time.

### Added

- **Capabilities schema v2 — `memory_capabilities` introspection extension
  (arch-enhancement-spec §7)**. The capabilities report (MCP
  `memory_capabilities` + HTTP `GET /api/v1/capabilities`) gains a
  `schema_version: "2"` discriminator and five new top-level blocks:
  `permissions`, `hooks`, `compaction`, `approval`, `transcripts`. Pre-v0.7
  the `permissions.active_rules` field reflects a live count of namespace
  standards carrying `metadata.governance` (transparent passthrough; the
  full permission system is v0.7 work — arch-spec §3); `hooks.registered_count`
  reflects the live `subscriptions` table count (proxy for hook subscribers
  pre-v0.7 Bucket 0); `approval.pending_requests` reflects the live count
  of `pending_actions` rows with `status='pending'`. `compaction.enabled`
  and `transcripts.enabled` report `false` until v0.8 / v0.7-Bucket-1.7 land
  the underlying systems. **All v1 fields preserved at the same top-level
  paths** — older clients reading `tier`, `version`, `features`, `models`
  by name continue to work without modification. New tests:
  `mcp::tests::mcp_capabilities_v2_schema_includes_all_blocks`,
  `mcp::tests::mcp_capabilities_v2_backwards_compatible`,
  `mcp::tests::mcp_capabilities_pending_requests_reflects_db`,
  `handlers::tests::http_capabilities_v2_schema_includes_all_blocks`,
  `config::tests::capabilities_v2_zero_state_round_trip`. New helpers:
  `db::count_active_governance_rules`, `db::count_subscriptions`,
  `db::count_pending_actions_by_status`. Pure additive — no migration,
  no behavior change to any existing tool.

- **Hierarchical namespace taxonomy (Pillar 1 / Stream A)** — new
  `memory_get_taxonomy` MCP tool plus REST mirror at
  `GET /api/v1/taxonomy`. Walks live (non-expired) memories grouped by
  `namespace`, splits on `/`, and folds them into a `TaxonomyNode` tree.
  Each node carries `count` (memories at exactly this namespace) and
  `subtree_count` (count plus every descendant the depth limit allowed
  us to expand); the response envelope adds `total_count` (an
  independent aggregation that stays honest even when `limit` drops
  rows from the walk) and a `truncated` flag. Parameters:
  `namespace_prefix` (optional, accepts trailing `/`),
  `depth` (default 8 = `MAX_NAMESPACE_DEPTH`, clamped),
  `limit` (default 1000, hard ceiling 10000 — densest namespaces win
  when truncated). Closes the "flat blob" perception gap from charter
  §"The Demo That Sells It" (charter lines 218–230) and unblocks the
  taxonomy demo CLI surface deferred to a later iteration. Charter
  §"Stream A — Hierarchy", lines 320–326.

- **Temporal-validity KG schema (Stream B foundation)** — SQLite schema
  bumps to v15 (`src/db.rs::migrate`). `memory_links` gains four nullable
  temporal columns — `valid_from`, `valid_until`, `observed_by` (TEXT),
  and `signature` (BLOB; placeholder for v0.7 attested identity). On
  upgrade, existing links are backfilled: `valid_from` is set to the
  source memory's `created_at` (charter pre-flight default — defensive
  null avoidance). Three temporal indexes are created for the upcoming
  recursive-CTE traversal in `memory_kg_query` / `memory_kg_timeline`:
  `idx_links_temporal_src` (source_id, valid_from, valid_until),
  `idx_links_temporal_tgt` (target_id, valid_from, valid_until), and
  `idx_links_relation` (relation, valid_from). New `entity_aliases`
  side table (entity_id, alias, created_at; PK on entity_id+alias)
  with `idx_entity_aliases_alias` lookup index unblocks the upcoming
  Stream C entity-registry tools. The Postgres declarative schema
  (`src/store/postgres_schema.sql`) is mirrored for fresh-init parity;
  existing PG installs do not auto-gain the new columns since the PG
  store layer is still WIP (an explicit ALTER migration lands when
  `link()` is wired up there). Pure additive — no existing query
  breaks. Charter §"Critical Schema Reference", lines 686–723.

- **Entity registry (Pillar 2 / Stream B)** — `memory_entity_register`
  + `memory_entity_get_by_alias` MCP tools (count 38 → 40) plus the
  matching HTTP surface (`POST /api/v1/entities`,
  `GET /api/v1/entities/by_alias`, with 201 / 200 / 409 status
  discipline and `X-Agent-Id` honoured). Entities are long-tier
  memories tagged `entity` with `metadata.kind = "entity"`; aliases
  live in the v15 `entity_aliases` side table. Registration is
  idempotent on `(canonical_name, namespace)` — re-registering reuses
  the entity_id and merges new aliases via `INSERT OR IGNORE`. A
  non-entity memory occupying the same `(title, namespace)` returns a
  hard error rather than letting the upsert path silently overwrite
  unrelated content. Resolver returns the most-recently-created
  entity when no namespace filter is supplied; ignores stray
  `entity_aliases` rows that point at non-entity memories. Builds on
  the v15 schema (#384). Charter §"Stream B — KG Schema + Entity
  Model", lines 369–375.

- **`memory_kg_timeline` (Pillar 2 / Stream C)** — entity-anchored
  chronological view powering the `ai-memory kg-timeline` headline
  demo. `db::kg_timeline()` queries `memory_links` ordered by
  `valid_from ASC` (tie-break `created_at`) with optional inclusive
  `since` / `until` filters; limit clamps to `[1, 1000]`, default
  200. `db::create_link()` now stamps `valid_from = created_at` on
  every insert so newly created links are visible to the timeline
  without a later sweep, closing the forward gap left by the v15
  backfill of legacy rows. `memory_kg_timeline` MCP tool (count
  40 → 41) plus `GET /api/v1/kg/timeline?source_id=…&since=…
  &until=…&limit=…`. Returns `KgTimelineEvent` carrying `target_id`,
  `relation`, validity window, `observed_by`, and the target's
  `title` / `namespace`. Charter §"Stream C — KG Query Layer",
  lines 377–383.

- **`memory_kg_invalidate` (Pillar 2 / Stream C)** — second tool of
  the KG-traversal triplet. Marks a KG link as superseded by setting
  its `valid_until` column so a contradicting fact can invalidate
  the prior assertion without deleting the row, preserving the
  timeline. The link is identified by its composite key
  `(source_id, target_id, relation)` since `memory_links` has no
  separate id; `valid_until` defaults to wall-clock now when
  omitted. `db::invalidate_link()` returns
  `Option<InvalidateResult>` — `None` when the triple does not
  match, `Some` with the value now stored and `previous_valid_until`
  so callers can distinguish a fresh supersession from an idempotent
  retry. `memory_kg_invalidate` MCP tool (count 41 → 42) plus HTTP.
  Schema does not yet carry an audit column for the supersession
  `reason`; that arrives with v0.7 attestation. Charter §"Stream C —
  KG Query Layer", lines 377–383.

- **`memory_kg_query` depth=1 (Pillar 2 / Stream C)** — outbound
  "expand neighbors" first slice. `memory_kg_query` MCP tool (count
  42 → 43) plus HTTP. `db::kg_query()` ships with constants
  `KG_QUERY_DEFAULT_LIMIT = 200`, `KG_QUERY_MAX_LIMIT = 1000`, and
  `KG_QUERY_MAX_SUPPORTED_DEPTH = 1`; callers passing `max_depth=2`
  get a clean error rather than a silent truncation, so the API
  contract is stable from day one — the recursive-CTE multi-hop
  follow-up just lifts the ceiling without changing the surface.
  Filters per the charter spec: `valid_at` (RFC3339, only links
  valid at that instant); `allowed_agents` (only links observed by
  an agent in the set; **empty list returns zero rows by design** —
  callers signaling "no agents trusted" must get an empty traversal,
  not the unfiltered fallback); `limit` clamped to `[1, 1000]`.
  Charter §"Stream C — KG Query Layer", lines 377–383.

- **`memory_kg_query` depth 2..=5 (Pillar 2 / Stream C)** — lifts
  `KG_QUERY_MAX_SUPPORTED_DEPTH` from 1 to 5, matching the published
  `memory_kg_query (depth ≤ 5)` 250 ms p95 / 500 ms p99 budget in
  `PERFORMANCE.md`. Replaces the depth=1 JOIN with a recursive CTE
  that re-applies the temporal / agent filter on every hop and
  prunes cycles via the accumulated `path`; each row's `depth` +
  `path` now reflect the actual chain (e.g. depth=2 →
  `src->mid->target`). API contract is unchanged — depth=1 collapses
  to the original time-ordered single-hop result, and the
  over-ceiling MCP/HTTP error path (422 with `max_depth=N exceeds
  supported depth=5`) is preserved. Closes the Stream C
  `memory_kg_query` slice; traversals at depth 2..=5 are now correct
  under temporal-validity and observed-by filtering. Charter
  §"Stream C — KG Query Layer", lines 377–383.

- **`memory_check_duplicate` (Pillar 2 / Stream D)** — pre-write
  near-duplicate check across DB / MCP / HTTP. `db::check_duplicate`
  performs a cosine scan over live embedded memories with the
  threshold clamped at `DUPLICATE_THRESHOLD_MIN = 0.5` (so permissive
  callers can't dress unrelated content as a merge candidate) and
  default `DUPLICATE_THRESHOLD_DEFAULT = 0.85` (tuned for the
  MiniLM-L6-v2 embedder — near-paraphrases land ≥ 0.88, loosely
  related content sits well below). `memory_check_duplicate` MCP
  tool (count 37 → 38) returns the nearest-neighbor cosine, the
  above-threshold boolean, and an optional `suggested_merge` target.
  HTTP `POST /api/v1/check_duplicate` mirrors the MCP surface and
  embeds *before* taking the DB lock (issue #219 pattern). Charter
  §"Stream D — Duplicate Check", lines 384–386.

- **`ai-memory bench` scaffold (Pillar 3 / Stream E)** — first slice
  of perf instrumentation. New CLI subcommand + `src/bench.rs`
  runner so operators (and the `bench.yml` CI guard / Stream F) can
  verify the published `PERFORMANCE.md` budgets. Covers the three
  embedding-free hot-path operations: `memory_store` (no embedding)
  / 20 ms p95, `memory_search` (FTS5) / 100 ms p95, and
  `memory_recall` (hot, depth=1) / 50 ms p95. Each invocation seeds
  a disposable `:memory:` SQLite DB so the operator's main DB is
  untouched. Reports p50 / p95 / p99 in either a human table or
  `--json`. Exit code is non-zero when any p95 exceeds its target
  by more than the documented 10% tolerance — so the same binary
  slots into the CI guard once Stream F lands. `PERFORMANCE.md`
  status table now distinguishes "scaffold landed" from "Stream E
  follow-up" so partial coverage isn't silent. Charter §"Stream E —
  Performance Instrumentation", lines 388–393.

- **Performance budgets published** — new `PERFORMANCE.md` at the repo
  root carries the authoritative p95/p99 latency contract for every
  hot-path operation (verbatim from the v0.6.3 grand-slam charter):
  `memory_session_start` hook, `memory_recall` hot/cold,
  `memory_store` with/without embedding, `memory_search`,
  `memory_check_duplicate`, `memory_kg_query` (depth ≤ 3 / ≤ 5),
  `memory_kg_timeline`, `memory_get_taxonomy`, `curator cycle`, and
  `federation ack`. Documents the **>10% p95 breach fails CI**
  threshold (p99 informational until the v0.6.3 soak window closes),
  the Apple M4 / 32 GB / NVMe SSD reference hardware baseline (with a
  note on Linux x86_64 CI parity), and a status table flagging the
  bench tool (Stream E) and `bench.yml` workflow (Stream F) as still
  in-flight. Closes Pillar 3 / Stream F doc deliverable from the
  v0.6.3 charter.

- **`bench.yml` CI guard (Pillar 3 / Stream F)** — new
  `.github/workflows/bench.yml` runs `ai-memory bench` on every pull
  request and trunk push (`main`, `develop`, `release/**`) plus on
  manual `workflow_dispatch`. The job builds the release binary on
  `ubuntu-latest` (the latency reference per `PERFORMANCE.md`),
  streams the bench table into the workflow run summary, and uploads
  a `bench-results` artifact (`bench-results.json` +
  `bench-table.txt`) for downstream tooling. The `ai-memory bench`
  binary already exits non-zero when any operation's measured p95
  exceeds its target by more than the published 10% tolerance, so
  the workflow fails on regression without additional gating logic.
  Closes the last Stream F deliverable from charter §"Stream F —
  Performance Budgets + CI Guard"; budgets are now continuously
  enforced against trunk and PRs.

- **`ai-memory bench` KG depth=3 + depth=5 coverage (Pillar 3 / Stream E)**
  — `memory_kg_query` is now exercised at the deepest hop of both
  documented budget buckets: depth=3 against the "depth ≤ 3" 100 ms
  p95 row and depth=5 against the "depth ≤ 5" 250 ms tail-case row in
  `PERFORMANCE.md`. The runner seeds a second in-process fixture (50
  chains × 5 hops each = 300 memories + 250 links) so the recursive
  CTE actually traverses three / five hops per query rather than
  collapsing to a single hop on the existing fan-out fixture. Local M4
  measurements: depth=3 p95 ~0.6 ms, depth=5 p95 ~0.7 ms — both PASS,
  both well inside the 10% tolerance enforced by `bench.yml`. No new
  dependencies. Completes the KG half of Stream E; embedding-bound
  paths still need a fixture decision and remain tracked separately.

- **`ai-memory bench` KG coverage (Pillar 3 / Stream E)** —
  `memory_kg_query` (depth=1) and `memory_kg_timeline` are now driven
  by the `bench` subcommand against the same in-memory disposable
  SQLite database used by the embedding-free operations. The runner
  seeds an in-process KG fixture (50 source memories × 4 outbound
  links each, every link `valid_from`-stamped so `kg_timeline` sees
  them) and reports p50/p95/p99 against the 100 ms p95 budgets
  published in `PERFORMANCE.md`. Local M4 measurements: `kg_query`
  p95 ~0.7 ms, `kg_timeline` p95 ~0.1 ms — both PASS, both well
  inside the 10% tolerance enforced by the `bench.yml` CI guard.
  No new dependencies. Closes the KG half of the iter-0017 follow-up
  ask; embedding-bound paths still need a fixture decision and are
  tracked separately.

- **Per-tool MCP tracing spans (Pillar 3 / Stream E)** — every
  `tools/call` dispatch now runs inside an `info`-level
  `mcp_tool_call` span carrying the tool name and JSON-RPC id. After
  the handler returns, an `ok` event records `elapsed_ms`; an
  `Err` outcome emits a `warn` event with the error message so
  on-call dashboards can alert on per-tool error rate. The MCP server
  entrypoint (`run_mcp_server`) installs a `tracing_subscriber::fmt`
  subscriber pinned to `stderr` (stdio JSON-RPC owns stdout) honoring
  `RUST_LOG`; `try_init` makes it a no-op when another command in the
  same process already initialised tracing. Foundation for the v0.6.3
  charter §"Stream E — Performance Instrumentation" ask;
  paired with the `ai-memory bench` scaffold to give exporters
  per-tool latency attribution against the published `PERFORMANCE.md`
  budgets.

### Fixed

- **[#358]** mTLS allowlist parser now tolerates inline trailing `#`
  comments after a fingerprint
  (`load_fingerprint_allowlist`, `src/main.rs`). Previously, a line like
  `sha256:abc…def  # node-1` was parsed whole and failed the 64-hex-char
  length check (`got 74`), aborting `ai-memory serve` on startup. Full-line
  `#` comments and the Ultrareview #338 strict character-set check
  (rejects embedded whitespace inside the hex run) are preserved. Doc
  update: `docs/ADMIN_GUIDE.md` now explicitly calls out trailing-comment
  tolerance. Encountered in the a2a-gate mTLS matrix; the gate-side
  generator fix in `ai-memory-ai2ai-gate#35` already worked around it for
  v0.6.2 — this is the parser-side resolution.

### Changed

- **CI coverage gate — fail-under 92%**. The `coverage` job in
  `.github/workflows/ci.yml` now invokes `cargo llvm-cov` with
  `--fail-under-lines 92`, locking in the v0.6.3 baseline of 93.05%
  with a 1% absorb buffer. PRs that drop total line coverage below
  92% will fail the gate. Per-module floors (`handlers.rs`, `db.rs`,
  `federation.rs`, `mcp.rs`, `governance.rs` ≥90%) are tracked in the
  v0.7 assertion table for follow-up enforcement.

### Tests

- **[#401]** RAII `ChildGuard` fixes mTLS test daemon-leak on assert
  panic.
  `tests/integration.rs::test_serve_mtls_fingerprint_allowlist_accepts_only_known_peer`
  was leaking `target/debug/ai-memory … serve` child processes
  whenever any of its 4 asserts panicked between spawn and the
  manual `kill()` at the bottom — `std::process::Child` has no
  kill-on-drop on Unix. Adds a generic `ChildGuard { child:
  Option<Child>, cleanup_paths: Vec<PathBuf> }` alongside the
  existing `DaemonGuard`, with an unwind-safe `Drop` that kills,
  reaps, and unlinks; refactors the mTLS test to wrap both spawned
  children. End-user impact is zero (production `serve` deployments
  via systemd / launchd / Docker reap children correctly), but the
  campaign runner had been accumulating ~28 GB of orphaned daemons
  across 7 reparented PIDs during the v0.6.3 dev sprint.

## [v0.6.2] — 2026-04-24 — A2A-CERTIFIED

First release to carry the a2a-gate **consecutive-green streak 3/3**
certification. Three consecutive full-testbook passes across six
homogeneous cells (ironclaw + hermes × off/tls/mtls on DigitalOcean,
and openclaw × off on a local Docker mesh) validate that A2A
scenarios against ai-memory v0.6.2 are green end-to-end on
`release/v0.6.2 @ 3e018d6`.

**Evidence** — every scenario artifact is committed alongside the
releasing branch of the a2a-gate repo:
<https://alphaonedev.github.io/ai-memory-ai2ai-gate/runs/>

### Fixed — federation fanout correctness (a2a-gate v3r22–r30)

- **[#325]** `create_link` fanout — `POST /api/v1/links` broadcasts
  the new link to every peer via quorum write. Scenario-11 of the
  a2a-gate harness exercised this: charlie couldn't see an M1→M2
  link written on alice's node. `SyncPushBody` grows a
  `links: Vec<MemoryLink>` field applied via `db::create_link` on
  peers; duplicates are idempotent via the existing
  `(source_id, target_id, relation)` unique index. New
  `federation::broadcast_link_quorum`. Delete-link fanout deferred
  to v0.7 CRDT-lite tombstones.
- **[#326]** `consolidate` fanout — `POST /api/v1/consolidate`
  broadcasts the new consolidated memory AND the source-id
  deletions in a single sync_push call. Scenario-5 exposed the
  gap: peer nodes never saw the consolidated memory, so
  `metadata.consolidated_from_agents` read as `"[]"`. New
  `federation::broadcast_consolidate_quorum`.
- **[#327]** Embedder-failure visibility on `ai-memory serve` —
  HuggingFace-Hub fetch failure now logs at `ERROR` with an
  `⚠️ EMBEDDER LOAD FAILED` marker and a remediation pointer.
  `/api/v1/health` grows `embedder_ready: bool` +
  `federation_enabled: bool` fields so harnesses can assert
  semantic-tier readiness before scenarios run.
- **[#363]** List cap 200 → 1000 + pending-action fanout +
  namespace_meta fanout (S34 / S35 / S40). Closed the three
  fanout gaps surfaced by v3r22.
- **[#364]** `clear_namespace_standard` fanout symmetry follow-up
  to #363 — the clear path was missing from `SyncPushBody`;
  scenario-35 on peer-nodes saw stale standards after a clear on
  the leader.
- **[#366]** HTTP `/api/v1/recall` now uses hybrid semantic when
  the embedder is loaded. Scenario-18 previously black-holed
  because the endpoint fell through to FTS-only even with a live
  embedder.
- **[#367]** Relax semantic cosine threshold 0.3 → 0.2 in
  `recall_hybrid`. Scenario-18 caught a miss at 0.25–0.29 cosine
  for legitimately-related content; the lower threshold preserves
  top-K recall without introducing noise (blended score still
  gated by `fts.rank + …` component).
- **[#368]** S40 fanout retry — `post_and_classify` retries once
  on `AckOutcome::Fail` with a 250 ms backoff. `Idempotency-Key`
  already present on `sync_push` makes a partial-apply race
  dedupe to a no-op on the peer via `insert_if_newer`. RCA:
  v3r26 hermes-tls scenario-40 saw `node-2 499/500 bulk rows`
  post-quorum because the detached per-peer POST had transiently
  failed; no retry, no catchup.
- **[#369]** S40 `bulk_create` terminal catchup batch per peer.
  After the per-row quorum drains, the leader sends ONE batched
  `sync_push` per peer with every committed row. Peer-side
  `insert_if_newer` dedupes already-applied rows; rows dropped by
  the detached path land now. O(1) extra POST per peer vs O(N)
  retries per row. Proven to close the gap on v3r28 after retry
  alone was insufficient on v3r27 (ironclaw-off still dropped one
  row despite the retry — sustained SQLite-mutex contention
  during a 500-row burst can drop two consecutive POSTs).

### Evidence & reproducibility

The a2a-gate repository carries the full certification evidence:

- **Runs dashboard** —
  <https://alphaonedev.github.io/ai-memory-ai2ai-gate/runs/>
- **AI NHI insights** (tri-audience analysis) —
  <https://alphaonedev.github.io/ai-memory-ai2ai-gate/insights/>
- **Local Docker mesh reproducibility spec** —
  <https://alphaonedev.github.io/ai-memory-ai2ai-gate/local-docker-mesh/>

Per-campaign evidence pages under `runs/` carry scenario-level
JSON, stderr logs, baseline attestation, F3 peer-replication
canary, and a campaign.meta.json provenance trace. The DO
campaigns (v3r28 / v3r29 / v3r30) used `release/v0.6.2 @ 3e018d6`
with `ai_memory_source_build=true`; the local-docker campaigns
(r1 / r2 / r3) used the same commit via a committed release
binary.

### Certification matrix

| | off | tls | mtls |
|---|---|---|---|
| **ironclaw (DO)** | ✅ v3r30 35/35 | ✅ v3r30 35/35 | ✅ v3r30 37/37 |
| **hermes (DO)** | ✅ v3r30 35/35 | ✅ v3r30 35/35 | ✅ v3r30 37/37 |
| **openclaw (local-docker)** | ✅ r3 35/35 | ⏸ Phase 3 | ⏸ Phase 3 |

Total: **214 passing scenarios** across six cells on the final
certification run (v3r30 DO + local-docker r3).

## [Unreleased] — v0.6.1 + v0.7 tracks

### v0.7.0 round-2-fixes folding (2026-05-11) — no v0.7.0.1, everything ships in v0.7.0

Operator directive: there will be no v0.7.0.1 patch release. Items
originally triaged for v0.7.0.1 fold into v0.7.0 directly.

#### Fixed (closes via round-2-fixes)

- **#318 MCP stdio writes bypass federation fanout** — new opt-in
  `mcp_federation_forward_url` in `AppConfig`. When set, MCP
  `memory_store` calls forward to the local HTTP daemon's
  `POST /api/v1/memories`, which already runs
  `broadcast_store_quorum`. Single-node MCP deployments are
  unchanged when the config is unset. Closes the a2a-gate-r6
  finding "30 MCP stdio writes persisted locally but zero rows
  replicated to peers."
- **#355 rustls-pemfile RUSTSEC-2025-0134 (unmaintained, transitive
  via axum-server)** — bumped `axum-server 0.7 → 0.8`. The 0.8
  release drops the rustls-pemfile dependency. `cargo audit` now
  reports clean; `rustls-pemfile` is gone from `Cargo.lock`.
- **#507 `config.toml` `db = "~/..."` not expanded** — `AppConfig::effective_db`
  now expands leading `~` / `~/` to `$HOME` via a new private
  `expand_tilde` helper. Daemon no longer reports
  `warn db unavailable` against an existing DB at the
  tilde-expanded location. Bare `~` resolves to `$HOME` itself;
  `~user/` not supported.
- **#625 E1/E2 orchestration scripts ported from bash to Rust** —
  new standalone crates `tools/t0-orchestrate/` +
  `tools/post-ship-converge/` producing the `ai-memory-t0` and
  `ai-memory-post-ship-converge` binaries. The old
  `scripts/t0-orchestrate.sh` and `scripts/post-ship-converge.sh`
  are deleted. `tests/e1_orchestration_dry_run.rs` and
  `tests/e2_post_ship_dry_run.rs` drop their `#![cfg(unix)]` gates
  so Windows CI now validates the same dry-run envelope shape.
- **L15 entrypoint wire** — `entrypoint.plan-c.sh` now writes
  `auto_tag_model = "gemma3:4b"` to the daemon's `config.toml`
  (env-overridable as `AI_MEMORY_AUTO_TAG_MODEL`). Closes the Plan
  C R4 finding `H8: LLM call (auto_tag) exceeded 30s timeout`
  caused by Gemma 4 e4b thinking-mode generating 396-564 tokens
  for a 5-tag prompt; gemma3:4b finishes the same prompt in
  ~0.7s.
- **Postgres SAL `consolidate` upsert** — the prior implementation
  was a plain `INSERT INTO memories`, which exploded with
  `duplicate key value violates unique constraint
  "memories_title_ns_uidx"` when an operator re-ran a consolidate
  at the same `(title, namespace)` (common across repeat cert
  runs against the same persistent postgres database). Rewrote as
  `ON CONFLICT (title, namespace) DO UPDATE` matching the rest of
  the adapter's upsert contract; `RETURNING id` returns the
  existing id on conflict. Surfaced by Plan C R4 cert S5 failure;
  reproduced with daemon log
  `ERROR ai_memory::handlers: store backend error: backend
  unavailable: postgres: consolidate insert: error returned from
  database: duplicate key value violates unique constraint
  "memories_title_ns_uidx"`.
- **No-sal build break in `src/federation.rs`** — `spawn_catchup_loop`
  unconditionally called `spawn_catchup_loop_with_store`, which is
  `#[cfg(feature = "sal")]`-gated. Surfaced by the #625 port
  subagent. Fix: cfg-branch the body so the sqlite-only build
  goes through `catchup_once` directly.

#### Documentation

- Closed 12 v0.7.0 ship-tracker issues in one batch with a uniform
  "Closed by v0.7.0 ship sequence" comment — #637 (Round-2 master),
  #638 (F6 LLM-dispatch deadlock), #639 (F7 agent_quotas bypass),
  #640 (F8/F11/F12 secure-by-default), #641 (F13-F16 capabilities
  drift), #642 (F17/F18 find_paths surface), #646 (F6 SQL-view
  deferral), #647 (postgres+AGE scope tracker), #649 (Wave 4 live
  A2A re-validation), #635 (ship-readiness report), #508/#509
  (Grok Prime-Directive assessments).

### Added — v0.7 attested-cortex (Track H, Task H1)

- **Per-agent Ed25519 keypair CLI (`ai-memory identity`).** OSS substrate
  for the v0.7 attested-cortex epic. New `src/identity/keypair.rs`
  exposes the four-verb lifecycle (`generate / save / load / list`) plus
  a `save_public_only` path for importing peer allowlist entries. Keys
  are persisted under `<config>/ai-memory/keys/<agent_id>.{pub,priv}` —
  `~/.config/ai-memory/keys/` on Linux, `~/Library/Application
  Support/ai-memory/keys/` on macOS, `%APPDATA%\ai-memory\keys\` on
  Windows. On Unix the public file is written with mode `0o644` and
  the private file with mode `0o600`; on Windows the files inherit the
  parent ACL. The on-disk format is the raw 32-byte key (no PEM/DER
  wrapper) so the format is byte-identical to the COSE/CBOR shape H2
  will sign with.
- **`ai-memory identity` clap subcommand** wires the lifecycle into
  the CLI: `generate --agent-id <id>` (defaults to the same NHI-hardened
  id the rest of the CLI synthesizes via `identity::resolve_agent_id`),
  `import --agent-id <id> --pub <path> --priv <path>` (private optional;
  cross-checks `.priv` derives `.pub` and refuses mismatches),
  `list` (public-only — never loads private material, safe for
  dashboards), and `export-pub --agent-id <id>` (URL-safe-no-padding
  base64 of the 32-byte public key, pipe-friendly for peer-allowlist
  bootstrapping). `--key-dir <path>` is a global override for the
  default key directory.
- **Hardware-backed key storage is OUT of OSS scope.** TPM 2.0,
  PKCS#11 HSMs, Apple Secure Enclave / TEE, and AWS/GCP/Azure cloud
  KMS adapters are intentionally **not** implemented in this crate. The
  OSS path stops at file-based 0600 storage; certified hardware-backed
  deployments live in the AgenticMem™ commercial layer per
  `ROADMAP2.md`. The OSS code never imports a hardware-token library.
- **New deps (pure-Rust, MIT/Apache):** `ed25519-dalek = "2"` (with
  the `rand_core` feature for `SigningKey::generate`), `rand_core =
  "0.6"` (CSPRNG bound — we use `OsRng`), `base64 = "0.22"` (for the
  `export-pub` wire format).
- **16 new unit tests in `src/identity/keypair`** — generate-save-load
  round-trip with sign+verify, Unix mode 0600 / 0644 enforcement, list
  enumeration + sort + private-skip semantics, list-on-missing-dir
  returns empty, truncated/mismatched key file rejection, base64
  round-trip (URL-safe and padded), and a `save_public_only` happy
  path. **5 new unit tests in `src/cli/identity`** drive the four CLI
  verbs through the standard `CliOutput` capture harness, including
  `generate --no-overwrite` refusal and JSON-mode emission.

### Fixed — v0.6.0 pre-tag SAL blocker punchlist (#293)

Five correctness blockers surfaced by the v0.6.0 code-review (meta
issue [#293](https://github.com/alphaonedev/ai-memory-mcp/issues/293)),
all closed before the tag:

- **[#294]** SAL upsert key mismatch — aligned Postgres adapter to
  `ON CONFLICT (title, namespace)` matching SQLite's documented
  contract. Added `UNIQUE INDEX memories_title_ns_uidx` to
  `postgres_schema.sql`.
- **[#295]** `metadata.agent_id` immutability — Postgres UPSERT and
  UPDATE now preserve the original `agent_id` via `jsonb_set` CASE
  clause, mirroring SQLite's `json_set` SQL-layer guard. Task 1.2
  NHI invariant is now enforced on both adapters.
- **[#296]** Tier-downgrade protection on Postgres UPDATE — added
  `tier_rank()` SQL function and `GREATEST(tier_rank(...))`
  precedence so `Long → *` and `Mid → Short` are refused at the
  SQL layer, matching SQLite.
- **[#297]** Postgres schema parity — added 6 tables + generated
  `scope_idx` column (memory_links, archived_memories,
  namespace_meta, pending_actions, sync_state, subscriptions) so
  cross-backend migration is no longer lossy beyond the memories
  table.
- **[#298]** Migration cursor data loss — the prior
  `created_at`-based pagination silently dropped low-priority
  memories under `priority DESC` list ordering. Replaced with a
  single-call `MAX_ROWS=1M` migrate that refuses loudly when
  saturated. Streaming migrate for corpora >1M rows tracked for
  v0.7 with `MemoryStore::list_all`.

New regression tests (behind `AI_MEMORY_TEST_POSTGRES_URL`):
`upserts_by_title_namespace_not_id`, `upsert_preserves_agent_id`,
`update_refuses_tier_downgrade`. Plus `migrate_sqlite_to_sqlite_roundtrip`
tightened to assert single-call semantics.

### Removed — TurboQuant embedding compression scrapped

TurboQuant (Google Research, arXiv 2504.19874) was evaluated as an
embedding-compression path for ai-memory (PRs #284 and #287). Both
closed unmerged. The `alphaonedev/turboquant` fork was archived.
Decision rationale: the ~2× embedding storage reduction at 4
bit-width is irrelevant at ai-memory's target scale (<100k memories
per deployment); beyond that, Postgres + pgvector (#279) is the right
answer. The fork-maintenance + heavy-transitive-deps burden (ort,
tokenizers, safetensors, burn) was not justified by the marginal
gain. Real compression wins live elsewhere: Ollama KV compression
(#288 runbook) for inference memory, Postgres + pgvector for native
vector storage at scale, SQLCipher at rest (shipped) for data-at-rest
protection.

### Added — world-class documentation sprint

Seven new authoritative docs close the reference-material gaps in
the existing `docs/` tree:

- **`docs/README.md`** — navigation hub grouping every doc by audience
  (end users, admins, developers, design decisions, SDKs).
- **`docs/QUICKSTART.md`** — first memory stored + recalled in under
  5 minutes across three paths (CLI, MCP with Claude Code / Cursor /
  Codex, HTTP daemon).
- **`docs/CLI_REFERENCE.md`** — every subcommand, flag, and
  environment variable the `ai-memory` binary exposes. Auto-synced
  to `src/main.rs` clap definitions.
- **`docs/API_REFERENCE.md`** — every HTTP endpoint the daemon
  exposes, with payload shapes, query params, status codes, and
  `curl` recipes. 24+ endpoints.
- **`docs/GLOSSARY.md`** — every concept (agent, tier, scope,
  curator, quorum, SAL, …) with single-paragraph definitions and
  links to authoritative docs.
- **`docs/TROUBLESHOOTING.md`** — common errors (startup, MCP,
  autonomy, HTTP, sync, performance, governance) with root-cause
  analysis and fixes.
- **`docs/SECURITY.md`** — complete threat model, trust boundaries,
  auth stack (API key + mTLS Layer 1/2/2b), SQLCipher at rest,
  SSRF-hardened webhook dispatch, responsible disclosure process.

Existing docs (`USER_GUIDE.md`, `ADMIN_GUIDE.md`, `DEVELOPER_GUIDE.md`,
`INSTALL.md`, `PHASE-1.md`, `AI_DEVELOPER_*.md`, `ENGINEERING_STANDARDS.md`,
`ARCHITECTURAL_LIMITS.md`, `ADR-0001-quorum-replication.md`,
`RUNBOOK-*.md`) cross-linked from `docs/README.md` for discovery.

### Added — v0.7 Storage Abstraction Layer (Track B PR 1)

- **Storage Abstraction Layer (SAL) — `MemoryStore` trait + `SqliteStore`
  + `PostgresStore`** — preview surface for v0.7. Gated behind
  `--features sal` (trait + sqlite adapter) and `--features sal-postgres`
  (adds the Postgres + pgvector backend). Default builds unchanged.
  Trait design carries over from the red-team-hardened #222 proposal:
  typed `StoreError` with `#[non_exhaustive]`, `CallerContext` on every
  mutator, optional `Transaction` handle, `verify()` contract, advertised
  `Capabilities` bitflags (NATIVE_VECTOR, FULLTEXT, DURABLE, etc.).
- **Postgres adapter ships with**:
  - `src/store/postgres_schema.sql` — idempotent bootstrap creating the
    `memories` table with a `vector(384)` column, pgvector `hnsw` index
    for cosine NN search, `gin` FTS + tags + metadata indexes.
  - `packaging/docker-compose.postgres.yml` — `pgvector/pgvector:pg16`
    fixture for integration tests. Hardened container
    (`cap_drop: [ALL]`, `no-new-privileges`, tmpfs for `/tmp`).
  - Live integration tests in `src/store/postgres.rs` that skip when
    `AI_MEMORY_TEST_POSTGRES_URL` is unset — keeps default `cargo test`
    offline while giving CI a straightforward opt-in path.
  - Unit-level tests: capability bits, RFC3339 parse helpers, schema
    constants.

### Added — v0.7 quorum replication primitives (Track C PR 1)

- **ADR-0001 — Quorum replication + chaos-testing methodology**
  (`docs/ADR-0001-quorum-replication.md`). Full design doc covering the
  W-of-N write-quorum model, failure modes, chaos-fault classes, and
  the implementation phasing. Explicitly states that v0.7 will NOT
  publish a "<0.01% loss" probability — instead it will publish a
  convergence-bound report per chaos campaign.
- **Quorum-write primitives** (`src/replication.rs`) — `QuorumPolicy`
  (N / W / deadlines / clock-skew threshold), `AckTracker` (collects
  local commit + peer acks, surfaces timeouts + id-drift), typed
  `QuorumError`. Pure-logic, I/O-free so unit tests don't need a live
  peer mesh.
- **12 unit tests** covering: single-node degenerate case,
  majority-default, W clamping, peer ack deduplication, deadline
  expiry reporting Unreachable vs Timeout, id-drift handling,
  Error trait participation.

### Added — v0.6.1 curator daemon (Track A)

### Added
- **Autonomous curator daemon** — new `ai-memory curator` subcommand with
  `--once` (single sweep + JSON report) and `--daemon` (continuous loop,
  interval configurable via `--interval-secs`, clamped to `[60, 86400]`).
  Invokes `auto_tag` + `detect_contradiction` on memories that lack an
  `auto_tags` metadata key, persisting results on success. Dry-run mode
  emits the same report without touching any row. Hard operation cap
  per cycle (`--max-ops`, default 100) prevents runaway LLM usage.
  Complements the synchronous post-store hooks shipped in v0.6.0.0
  (#265) — the curator catches memories stored before hooks were enabled,
  or when the LLM was offline, or that become interesting only after
  more context accumulates.
- **Curator systemd unit** — `packaging/systemd/ai-memory-curator.service`
  with the same sandbox posture as the main daemon
  (`ProtectSystem=strict`, empty `CapabilityBoundingSet`,
  `MemoryDenyWriteExecute`, `@system-service` syscall filter).
- **Curator Prometheus metrics** — `ai_memory_curator_cycles_total`,
  `ai_memory_curator_operations_total{kind,result}`,
  `ai_memory_curator_cycle_duration_seconds{dry_run}`.

### Added — full autonomy loop (earning the "100% autonomous" claim)

Builds on Track A's curator with the four passes required to make the
"100% autonomous" claim honest:

- **Autonomous consolidation** — the curator scans each namespace for
  near-duplicate memories (Jaccard keyword overlap ≥ 0.55 on a
  token-length-≥3 bag), clusters up to 8 members per group, calls
  `LLM.summarize_memories`, and commits the consolidated memory via
  the existing `db::consolidate` transaction. Source memories are
  archived, not lost.
- **Autonomous forgetting of superseded memories** — when a memory's
  `metadata.confirmed_contradictions` points at a newer, equal- or
  higher-confidence memory, the curator archives the stale one.
  Confidence + freshness BOTH required — never forgets on detection
  alone.
- **Priority feedback** — memories with `access_count ≥ 10` and a
  recall in the last 7 days get priority +1 (cap 10); memories cold
  for 30+ days drop priority -1 (floor 1). Arithmetic only; no LLM.
- **Rollback log** — every autonomous action (consolidate, forget,
  priority-adjust) writes a `RollbackEntry` memory into
  `_curator/rollback/<ts>` carrying the pre-action snapshot. Reversible
  via `ai-memory curator --rollback <id>` or `--rollback-last N`.
  Once reversed, the log memory is tagged `_reversed` — the history
  itself is preserved as an audit trail.
- **Self-report** — at the end of every cycle the curator writes its
  own `CuratorReport` as a memory in `_curator/reports/<ts>`. Agents
  can recall "what did the curator do yesterday" using the ordinary
  `memory_recall` path.

### Testing — end-to-end autonomy coverage

- `AutonomyLlm` trait introduced as the narrow LLM surface the passes
  need; `OllamaClient` impls it in prod, `StubLlm` stubs it in tests.
- 10 unit tests in `src/autonomy.rs` including a full
  `full_autonomy_cycle_end_to_end` that seeds duplicates + a
  superseded pair, runs `run_autonomy_passes`, and asserts that
  clusters were formed, memories forgotten, rollback entries written,
  and the rollback-log namespace populated.
- `reverse_consolidation_restores_originals` verifies the undo path
  by consolidating two memories, rolling back, and asserting both
  originals are back and the merged memory is gone.

### Honest-claim note

v0.6.1 earns the **"fully-autonomous curator loop"** claim: the
system can tag, consolidate, forget, rebalance priority, report on
itself, and reverse any of its own actions — without human input.
It does **not** yet claim multi-agent autonomy across a federation
(that's Track C) or cross-backend autonomy (that's Track B).
"100% autonomous" without those caveats would still be overclaiming.

### Added — cross-backend migration (Track B PR 2)

- **`ai-memory migrate --from <url> --to <url>`** CLI subcommand,
  gated behind `--features sal`. Supported URL shapes:
  - `sqlite:///absolute/path.db` / `sqlite://./relative.db` → `SqliteStore`
  - `postgres://user:pass@host:port/db` → `PostgresStore`
    (only under `--features sal-postgres`)
- Reads pages via `MemoryStore::list`, writes via `MemoryStore::store`.
  **Idempotent on re-run** — source ids are preserved verbatim and
  both adapters upsert on id.
- `--batch N` (1..10 000, default 1000), `--namespace <ns>` filter,
  `--dry-run`, `--json` for machine-readable reports.
- **6 unit tests**: sqlite URL parsing, unknown-scheme rejection,
  sqlite→sqlite full-roundtrip, dry-run writes nothing, idempotent
  re-run, namespace filter.
- Pagination strategy: slides `until` window backwards with dedup by
  id — handles identical `created_at` timestamps that break naïve
  `since`-cursor paging on SQLite.

### What's still out of scope for v0.7-alpha

Explicitly deferred to v0.7.1 (noted in `src/migrate.rs` docblock):

- **Daemon-level adapter selection** (`ai-memory serve --store-url
  postgres://…`) — requires refactoring `handlers.rs` from
  `crate::db::` free functions to dispatch through
  `Box<dyn MemoryStore>`. That's a big change and belongs in its
  own PR.
- **Live dual-write** — reverse migration (pg → sqlite) works using
  the same command but there is no always-on replication between
  heterogenous backends yet.
- **Schema rewriting** — both adapters currently agree on the
  `Memory` shape so no field mapping is needed.

### Cross-backend-autonomy claim now earned

v0.7-alpha earns: **"one-shot migration between SQLite and
Postgres/pgvector, bidirectional, idempotent"**.

Still honest caveats:
- A production deployment running `ai-memory serve` against Postgres
  as the live store needs v0.7.1's adapter-selection refactor.
- The migration is file-level point-in-time. For zero-downtime cutover
  you still need to stop writes on the source, migrate, and restart
  against the destination — documented in the module docblock.

### Added — federation autonomy (Track C PR 2)

- **Quorum writes wired into the HTTP daemon** (`src/federation.rs`).
  `ai-memory serve --quorum-writes N --quorum-peers <url,url,…>` fans
  out every successful write to each peer's `/api/v1/sync/push` and
  returns OK only after the local commit + `W - 1` peer acks land
  within `--quorum-timeout-ms`. Insufficient acks → `503` with body
  `{"error":"quorum_not_met","got":X,"needed":Y,"reason":…}` and
  `Retry-After: 2`. Local write is **not** rolled back on quorum
  failure — the sync-daemon's eventual-consistency loop catches
  stragglers up (per ADR-0001 § Model).
- **Opt-in + default-off** — daemons without `--quorum-writes`
  behave byte-for-byte identical to v0.6.0. Zero impact on
  non-federated deployments.
- **Optional mTLS for federation traffic** — `--quorum-client-cert`
  + `--quorum-client-key` feed the outbound reqwest client an mTLS
  identity so peer acks can be authenticated end-to-end.
- **Chaos harness** — `packaging/chaos/run-chaos.sh` spawns a
  three-node local fixture, issues a configurable burst of writes,
  and injects one of four fault classes (`kill_primary_mid_write`,
  `partition_minority`, `drop_random_acks`, `clock_skew_peer`).
  Emits a JSONL convergence-bound report per cycle — the data
  shape ADR-0001 commits to publishing instead of a loss probability.

### Testing

- **7 async mock-peer integration tests** in `src/federation.rs`
  using real ephemeral-port axum servers.
- Full suite on default features: 289 unit + 158 integration tests
  still green. fmt + clippy pedantic green.

### Added — LadybugDB roadmap

- **`docs/ROADMAP-ladybug.md`** — authoritative plan for integrating
  LadybugDB (the `lbug` Rust crate) as a new `MemoryStore` SAL
  adapter alongside `SqliteStore` and `PostgresStore`. Deliberately
  **not** a 100% transition — the document explains why (AI-agnostic
  value prop, SAL trait is the right seam, ~4000 LOC rewrite is
  wrong shape). Phased plan: scaffold → migration tool support →
  benchmark matrix → promotion decision gated on 6 hard
  prerequisites. Maintenance posture (pinned SHA, monthly rebase,
  upstream-first policy, scrap criteria) informed by the TurboQuant
  scrap. Not shipping in v0.6.0.0; v0.7.1+ track.

### Added — Ollama KV-cache tuning runbook

- **`docs/RUNBOOK-ollama-kv-tuning.md`** — operator-facing runbook
  for enabling `OLLAMA_KV_CACHE_TYPE=q4_0` + `OLLAMA_FLASH_ATTENTION=1`
  on Ollama. Delivers 2–4× KV-cache memory reduction on every
  ai-memory LLM path with near-lossless quality. Zero ai-memory
  code changes.

### "100% autonomous AI" claim earned

Shipping together in v0.6.0.0:

- Autonomous curator loop (tag / consolidate / forget / priority /
  rollback / self-report) per Track A + A-2.
- Multi-agent federation with W-of-N quorum writes per Track C + C-2.
- Cross-backend portability (SQLite ↔ Postgres+pgvector) per Track
  B + B-2.
- Autonomous hooks firing on every successful `memory_store`.

Remaining caveats (documented in runbooks, not overclaims):

- Real chaos campaigns against a production-shaped deployment:
  `docs/RUNBOOK-chaos-campaign.md`.
- Week-long curator soak against a production corpus:
  `docs/RUNBOOK-curator-soak.md`.
- Daemon-level adapter selection (`serve --store-url postgres://…`):
  `docs/RUNBOOK-adapter-selection.md` — v0.7.1 follow-up.
- Attested `sender_agent_id` from mTLS cert identity — v0.7 Layer
  2b primitives shipped (#285); handler wiring follow-up.

## [0.6.0] — 2026-04-19 — Phase 1 complete + v0.6.0.0 sprint

Phase 1 baseline (Tasks 1.1–1.12 from alpha train) plus the v0.6.0.0 sprint
additions covering opt-in LLM autonomy hooks, decay-aware recall, multi-agent
messaging primitives, at-rest encryption, ops surfaces, and SDK scaffolds.

Defer-outs from this release (not shipped in 0.6.0):

- **Autonomous curator daemon** — continuous background consolidation / GC
  driven by LLM decisions. Deferred to v0.6.1. v0.6.0 ships only the
  opt-in post-store hooks (synchronous, store path only).
- **Multi-node replication + chaos testing** — durability claims beyond
  single-node VACUUM INTO snapshots + optional peer sync are out of scope
  for v0.6.0. No loss-probability target is published.
- **Storage abstraction layer (Postgres / pgvector adapter)** — remains a
  v0.7 track. v0.6.0 is SQLite-only; the SAL preview on `feat/sal-trait-redesign`
  stays private/feature-gated until v0.7 extraction.

### Added — v0.6.0.0 sprint (autonomy hooks + multi-agent + at-rest + ops + SDKs)

**Autonomy / recall**
- **Time-decay half-life on recall scoring** — per-tier exponential decay
  multiplier on the hybrid-recall score blend. Default half-lives: short
  7 d, mid 30 d, long 365 d. Configurable via `[scoring]` in `config.toml`;
  `legacy_scoring = true` disables decay for A/B comparison and regression
  rollback. Half-lives clamped to `[0.1, 36500]` days.
- **Contextual recall (conversation-token bias)** — `memory_recall` accepts
  an optional `context_tokens: array<string>`. When supplied, the primary
  query embedding is fused 70/30 with an embedding of the joined context
  tokens, biasing recall toward memories that match both the explicit
  query AND nearby conversation topics. CLI: `--context-tokens tok1,tok2`.
- **Post-store LLM autonomy hooks** — opt-in synchronous hooks that fire
  `llm::auto_tag` + `llm::detect_contradiction` on every successful
  `memory_store`. Results persist into `metadata.auto_tags` and
  `metadata.confirmed_contradictions`. Enabled via
  `AI_MEMORY_AUTONOMOUS_HOOKS=1` env var or `autonomous_hooks = true` in
  config. Off by default (adds Ollama round-trip latency). Skipped for
  content under 50 bytes, when no LLM is wired, and for `_`-prefixed
  internal namespaces.
**Multi-agent primitives**
- **Agent-to-agent notify + inbox** — `memory_notify(target, title, payload)`
  + `memory_inbox([agent_id, unread_only])` MCP tools. Messages are
  ordinary memories in the reserved `_messages/<target>` namespace;
  sender identity stamped in metadata; `access_count == 0` is the
  conventional unread marker. No new schema.
- **Webhook subscribe / unsubscribe / list** — `memory_subscribe` +
  `memory_unsubscribe` + `memory_list_subscriptions` MCP tools. Events
  fire on `memory_store` (v0.6.1 extends to delete/promote/link) and
  POST an HMAC-SHA256-signed JSON payload to subscriber URLs
  (`X-Ai-Memory-Signature: sha256=<hex>`). SSRF-hardened — private-range
  IPs rejected, https required for non-loopback hosts. Migration v13
  adds the `subscriptions` table.
**At-rest encryption**
- **Optional SQLCipher encryption at rest** — new cargo feature
  `sqlcipher` swaps `rusqlite` to the
  `bundled-sqlcipher-vendored-openssl` feature. Default builds are
  byte-for-byte unchanged. Operators who want encryption build with
  `cargo build --no-default-features --features sqlcipher` and supply
  `--db-passphrase-file <path>` at startup. Passphrase never appears
  in the process list or shell history.

**Ops**
- **Prometheus `/metrics` endpoint** (and `/api/v1/metrics`) exposes
  `ai_memory_store_total`, `ai_memory_recall_total`,
  `ai_memory_recall_latency_seconds`, `ai_memory_autonomy_hook_total`,
  `ai_memory_contradiction_detected_total`,
  `ai_memory_webhook_dispatched_total`,
  `ai_memory_webhook_failed_total`, `ai_memory_memories`,
  `ai_memory_hnsw_size`, `ai_memory_subscriptions_active`. Pure Rust,
  no new transitive C deps.
- **Hardened systemd units** under `packaging/systemd/` —
  `ai-memory.service`, `ai-memory-sync.service`,
  `ai-memory-backup.service`, `ai-memory-backup.timer` with README.
  Full sandbox (`ProtectSystem=strict`, `MemoryDenyWriteExecute=yes`,
  `SystemCallFilter=@system-service`, `CapabilityBoundingSet=` empty,
  `RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`). Target
  `systemd-analyze security` exposure score <5.0.
- **Backup / restore CLI** — `ai-memory backup --to <dir> [--keep N]`
  writes a hot-backup-safe SQLite `VACUUM INTO` snapshot plus a
  sha256 manifest. `ai-memory restore --from <path>` verifies the
  manifest before replacing the current DB; previous DB is moved
  aside to `<db>.pre-restore-<ts>.db` as a safety net. Paired with
  the hourly `ai-memory-backup.timer` systemd unit.

**SDKs**
- **TypeScript SDK scaffold** under `sdk/typescript/` —
  `@alphaone/ai-memory` (v0.6.0-alpha.0), strict TS, undici-based
  fetch, covers all current + v0.6.0.0 target endpoints (18+ methods),
  Jest tests guarded by `AI_MEMORY_TEST_DAEMON` env var. Includes
  HMAC-SHA256 webhook verifier. Not yet published to npm.
- **Python SDK scaffold** under `sdk/python/` — `ai-memory`
  (v0.6.0-alpha.0), sync (`AiMemoryClient`) + async
  (`AsyncAiMemoryClient`) clients via `httpx`, Pydantic v2 models
  (15/15 Memory fields), exception hierarchy, HMAC-SHA256 webhook
  verifier. Not yet published to PyPI.

### v0.6.0 GA disclosures (unchanged from pre-sprint baseline)

The following items are **MANDATORY DISCLOSURES** for the v0.6.0 release.
Operators upgrading from v0.5.4.x MUST read this section before deploying.

The following items are **MANDATORY DISCLOSURES** for the v0.6.0 GA release.
Operators upgrading from v0.5.4.x MUST read this section before deploying.

### Breaking changes

- **Consensus governance now requires agent pre-registration** (issue #234).
  The fix for security issue #216 (one caller satisfying `Consensus(N)` with
  N spoofed agent_ids) added an `is_registered_agent()` gate. Existing
  `consensus:N` policies become **indefinitely-locked** unless approver
  agents are registered first via `ai-memory agents register --agent-id <id>
  --agent-type <type>`.

  Migration: register all consensus approvers before upgrading. Example:

  ```bash
  ai-memory agents register --agent-id alice --agent-type human
  ai-memory agents register --agent-id bob   --agent-type human
  ai-memory agents register --agent-id carol --agent-type human
  ```

### Security disclosures (peer-mesh sync)

- **Sync endpoints are unauthenticated when TLS is not enabled** (issue #231).
  `POST /api/v1/sync/push` and `GET /api/v1/sync/since` accept all callers
  when `serve` runs without `--tls-cert + --tls-key`. Production peer-mesh
  deployments **MUST** set `--tls-cert + --tls-key + --mtls-allowlist`.
  See `docs/ADMIN_GUIDE.md` § Peer-mesh security.

- **sync-daemon does no server-cert verification without --client-cert**
  (issue #232). The daemon uses `danger_accept_invalid_certs(true)` when
  `--client-cert` is not provided — any server cert is accepted. For
  untrusted networks, ALWAYS use mTLS in both directions.

- **Any valid mTLS peer can dump the full database** (issue #239). By design,
  the trust boundary is the mTLS cert. Sync endpoints bypass per-memory
  visibility filtering. **Allowlist only peers you fully trust.** Per-namespace
  / per-scope sync filtering is a Phase 5 feature.

- **Body-claimed `sender_agent_id` is not yet attested to the cert CN/SAN**
  (issue #238). mTLS gates network access but the receiving handler accepts
  `sender_agent_id` from the body without checking the cert identity. A peer
  with a valid cert can claim any agent_id. Tracked as Layer 2b for v0.7.

### Schema migration

- v0.5.4.6 → v0.6.0 runs six additive migrations (v7 through v12). All are
  idempotent, transactional, and default-safe. Worst-case lock on a 10M-row
  database: 1–3 seconds during v10 (scope_idx index build). Schedule a brief
  maintenance window for large databases.

### Surface gaps tracked for v0.6.1

- Namespace standards / governance config is currently **MCP-only** (issue
  #236). HTTP and CLI surfaces will land in v0.6.1.
- `--agent-type` accepts only 6 hardcoded values (issue #235). Workaround:
  use `system` for custom agents, or wait for v0.6.1.

## [0.6.0-alpha.2] — 2026-04-16 — Phase 1 Track A complete + release-plumbing reconciliation

Supersedes **0.6.0-alpha.1** (2026-04-16, same day — partial publish). alpha.1
shipped the Task 1.3 feature to crates.io, Ubuntu PPA, Homebrew, and GitHub
Release binaries, but Docker (GHCR) and Fedora COPR failed due to a pre-existing
divergence between `main` and `release/v0.6.0`:

- Dockerfile pinned to `rust:1.87-slim` while code uses let-chains stabilized in
  1.88 (fixed on main in #187, never back-merged)
- Fedora COPR workflow `sed` blindly injected SemVer pre-release strings into
  RPM `Version:` field, which forbids `-`

alpha.2 back-merges `main` → `release/v0.6.0` (commits from `ce8fd47` through
`36747b2`, including RUSTSEC-2026-0098/0099 fixes), bumps `rust-version` to 1.88
(the honest MSRV), updates `time` 0.3.45 → 0.3.47 (RUSTSEC-2026-0009 DoS), and
patches the COPR workflow to split SemVer pre-release versions into `Version:` +
`Release:` pairs per Fedora packaging guidelines. No feature changes vs alpha.1.

alpha.1 will be **yanked from crates.io** once alpha.2 publishes successfully.

## [0.6.0-alpha.1] — 2026-04-16 — Phase 1 Track A complete (PARTIAL — yanked, superseded by alpha.2)

First cut of the v0.6.0 release train. Integration branch for Phase 1 tasks 1.3–1.12
plus the already-landed foundation work (1.1, 1.2). Pre-release; API is not yet stable.
Successive alphas will be tagged at each track completion (A/B/C/D per
[docs/PHASE-1.md](docs/PHASE-1.md) §Dependency Graph).

### Added — Task 1.1 (schema metadata foundation)

- **`metadata` JSON column** on `memories` and `archived_memories` tables, default `'{}'`.
  Schema migration to v7. All CRUD paths preserve metadata.
- **`Memory.metadata: serde_json::Value`** field with serde defaults.
- **`CreateMemory.metadata`**, **`UpdateMemory.metadata`** — MCP, HTTP, and CLI all accept
  arbitrary JSON metadata on store/update.
- **TOON format** renders `metadata` column inline.

### Added — Task 1.2 (Agent Identity in Metadata, NHI-hardened) — [#193]

- **`metadata.agent_id`** on every stored memory, resolved via a defense-in-depth
  precedence chain (explicit flag / body / MCP param → `AI_MEMORY_AGENT_ID` env →
  MCP `initialize.clientInfo.name` → `host:<host>:pid-<pid>-<uuid8>` →
  `anonymous:pid-<pid>-<uuid8>`).
- **HTTP `X-Agent-Id` request header** honored when no body `agent_id` is supplied;
  per-request `anonymous:req-<uuid8>` synthesized otherwise, with `WARN` log line.
- **`--agent-id` global CLI flag** (also reads `AI_MEMORY_AGENT_ID` env var).
- **`--agent-id` filter** on `list` and `search` (CLI, MCP tool param, HTTP query param).
- **Immutability**: `metadata.agent_id` is preserved across UPDATE, UPSERT dedup,
  import, sync, consolidate, and MCP `memory_update`. Enforced at both SQL level
  (`json_set` CASE clauses in `db::insert` and `db::insert_if_newer`) and caller
  level (`identity::preserve_agent_id` in every path that writes metadata).
- **Validation**: `^[A-Za-z0-9_\-:@./]{1,128}$` — permits prefixed / scoped / SPIFFE
  forms, rejects whitespace, null bytes, control chars, shell metacharacters.
- **New module** `src/identity.rs` (17 unit tests): precedence chain, process
  discriminator (`OnceLock<pid-<pid>-<uuid8>>`), component sanitization, HTTP
  resolution, provenance preservation.
- **`gethostname = "0.5"`** added as dependency (minimal, no transitive deps).
- **28 new tests** (20+ beyond spec minimum of 4): 17 unit + 2 validator + 9 integration.

### Security — red-team findings fixed during Task 1.2 review

- **T-3 (HIGH)**: MCP `memory_update` could rewrite `metadata.agent_id` on an existing
  memory, bypassing the documented immutability invariant. Fixed in commit `b228dcc`
  by wiring `identity::preserve_agent_id` into `handle_update`. Regression test
  `test_mcp_update_preserves_agent_id`.
- **GAP 1 (HIGH)**: `cmd_import` blindly trusted `metadata.agent_id` in input JSON,
  allowing an attacker-crafted file to forge any agent identity. Fixed in `356b448`:
  restamps with caller's id by default; `--trust-source` flag opts into legitimate
  backup-restore; original claim preserved as `imported_from_agent_id`. `cmd_sync`
  gets the same treatment on `pull` and `merge` paths.
- **GAP 2 (MEDIUM)**: `db::consolidate` merged source metadata with last-write-wins
  semantics on `agent_id`, nondeterministically dropping attribution and giving the
  consolidator no record. Fixed in `356b448`: consolidator's id is authoritative;
  all source authors preserved in `metadata.consolidated_from_agents` array.
  HTTP `ConsolidateBody` gains optional `agent_id` field plus `X-Agent-Id` header.
- **GAP 3 (LOW)**: `cmd_mine` produced memories with empty metadata, orphaning them
  from every agent_id filter. Fixed in `356b448`: caller's `agent_id` +
  `mined_from` source tag injected into every mined memory.
- **Defense-in-depth**: `db::insert_if_newer` (sync `merge` path) gains the same
  SQL-level `json_set` preservation clause as `db::insert`.

### Documentation — Phase 1.5 governance — [#194]

- **Governance §2.1 + §2.1.1**: new `Supervised off-host agents` approved class with
  7 binding pre-conditions (heartbeat, dead-man's switch, rate limit, lock-aware
  operation, instance-disambiguating attribution, etc.).
- **Governance §3.4.3.1**: concurrency lock primitive (short-tier `ai-memory` entry
  as lock, 15-min TTL, race-loser-yields semantics, stale-lock human escalation).
- **Governance §3.4.4.1 / §3.4.4.2**: audit-memory retention policy (immutable,
  non-consolidatable, append-only) + volume control at scale.
- **Governance new §3.5** (7 sub-sections): multi-agent coordination — branch
  ownership, handoff procedure, stale-branch GC, inter-agent conflict resolution,
  §3.4 SOP serialization, humans-in-CLI vs supervised off-host coordination,
  single-agent operation default.
- **Governance §5.4**: sole-approver policy applies uniformly to every approved
  agent class.
- **Workflow §8.5.1**: multi-agent operation cross-reference + lock acquisition
  discipline.

### Added — Task 1.3 (Agent Registration)

- **`_agents` reserved namespace** holding one long-tier memory per registered
  agent (`title = "agent:<agent_id>"`, `metadata.agent_type` +
  `metadata.capabilities` + `metadata.registered_at` + `metadata.last_seen_at`).
- **MCP tools**: `memory_agent_register`, `memory_agent_list` (brings tool count
  to **28**).
- **HTTP endpoints**: `POST /api/v1/agents`, `GET /api/v1/agents` (brings
  endpoint count to **26**).
- **CLI**: `ai-memory agents register --agent-id … --agent-type … [--capabilities …]`
  and `ai-memory agents list` (default sub-command).
- **`VALID_AGENT_TYPES`** closed set: `ai:claude-opus-4.6`, `ai:claude-opus-4.7`,
  `ai:codex-5.4`, `ai:grok-4.2`, `human`, `system`. Enforced by
  `validate_agent_type`.
- **Re-registration semantics**: upsert refreshes `agent_type`, `capabilities`,
  `last_seen_at`; preserves `registered_at` and `metadata.agent_id`
  (rides existing immutability SQL clause).
- **Trust model unchanged**: `agent_id` is still *claimed, not attested*. Future
  work will pair registration with provable attestation.
- **6 new integration tests**: register+list, duplicate-preserves-registered-at,
  invalid-type-rejected, invalid-id-rejected, namespace-isolation (no leak into
  `global`), and raw MCP JSON-RPC register/list roundtrip.

### Pending — remaining Phase 1 tasks to land in this release train

- Task 1.4 — Hierarchical Namespace Paths — depends on 1.1 ✓
- Task 1.5 — Visibility Rules — depends on 1.4
- Task 1.6 — N-Level Rule Inheritance — depends on 1.4
- Task 1.7 — Vertical Promotion — depends on 1.4
- Task 1.8 — Governance Metadata — depends on 1.1 ✓
- Task 1.9 — Governance Roles — depends on 1.8
- Task 1.10 — Approval Workflow — depends on 1.9
- Task 1.11 — Budget-Aware Recall — depends on 1.1 ✓
- Task 1.12 — Hierarchy-Aware Recall — depends on 1.4 + 1.11

### Release engineering

- Branched from `develop` @ `ee6cf9a` on 2026-04-16; all Phase 1 work now lands on `release/v0.6.0`.
- Successive alphas (`v0.6.0-alpha.N`) tagged at each track completion; `v0.6.0-rc.1`
  at feature-complete; `v0.6.0` GA when Phase 1 is done and external review window
  closes.
- `main` remains frozen at v0.5.4-patch.6 until v0.6.0 GA — no more 0.5.4 patches.

## [0.5.4-patch.4] — 2026-04-13

### Added

- **Three-level rule layering**: global (`*`) + parent + namespace standards, auto-prepended to recall and session_start. Max depth 5, cycle-safe.
- **Cross-namespace standards**: A standard memory from any namespace can be set as the standard for any other namespace. One policy, many projects.
- **Auto-detect parent by `-` prefix**: `set_standard("ai-memory-tests", id)` auto-discovers `ai-memory` as parent if it has a standard set. No explicit `parent` parameter needed.
- **Filesystem path awareness**: On `session_start`, walks from cwd up to home directory, checks if parent directory names have namespace standards, auto-registers parent chain. OS-agnostic via `PathBuf` and `dirs` crate.
- **`parent` parameter on `memory_namespace_set_standard`**: Explicit parent declaration for rule layering.
- Schema migration v6: `parent_namespace` column on `namespace_meta`

### Changed

- `inject_namespace_standard` resolves full parent chain: global → grandparent → parent → namespace
- Response returns `"standard"` (1 level) or `"standards"` array (multiple levels)
- TOON format: `standards[id|title|content]:` section renders all levels

## [0.5.4-patch.3] — 2026-04-12

### Added

- **Namespace standards**: 3 new MCP tools (`memory_namespace_set_standard`, `memory_namespace_get_standard`, `memory_namespace_clear_standard`) — 26 MCP tools total. Set a memory as the enforced standard/policy for a namespace; auto-prepended to recall and session_start results when scoped to that namespace.
- **Auto-prepend**: `handle_recall` and `handle_session_start` automatically prepend the namespace standard as a separate `"standard"` field when namespace is specified. Deduplicated from results. Count excludes standard.
- **TOON standard section**: TOON format renders namespace standard as a separate `standard[id|title|content]` section before memories.
- Schema migration v5: `namespace_meta` table
- 2 new integration tests: `test_mcp_namespace_standard_auto_prepend`, `test_namespace_standard_cascade_on_delete`

### Fixed

- **Shell `validate_id()` gap**: Interactive REPL `get` and `delete` commands now call `validate_id()`.
- **HNSW stale entry on dedup update**: `handle_store` dedup path now calls `idx.remove()` before `idx.insert()`.
- **Cascade cleanup**: `db::delete` removes `namespace_meta` rows referencing the deleted memory. `db::gc` cleans orphaned `namespace_meta` rows after expiring memories.
- **Consolidate warning**: `handle_consolidate` warns if any source memory is a namespace standard, prompting re-set to the new consolidated memory ID.

## [0.5.4-patch.2] — 2026-04-12

### Fixed

- **Tier downgrade protection**: `update()` now rejects tier downgrades (long→mid, long→short, mid→short) with a clear error message; prevents accidental data loss from TTL being added to permanent memories
- **Embedding regeneration on content update**: MCP `memory_update` now regenerates embedding vector and updates HNSW index when title or content changes, preventing stale semantic recall results
- **Consolidated memory embedding**: MCP `memory_consolidate` now generates embedding for the new consolidated memory at creation time and removes old entries from HNSW index, instead of relying on backfill
- **Self-contradiction exclusion**: CLI and MCP store now exclude the actual memory ID from `potential_contradictions` on upsert, fixing cosmetic self-referencing bug
- **Atomic CLI promote**: Removed non-atomic raw SQL `UPDATE` in `cmd_promote`; `db::update()` with `Some("")` already clears `expires_at` correctly
- **MCP `validate_id()` defense-in-depth**: Added `validate_id()` to `handle_get`, `handle_update`, `handle_delete`, `handle_promote`, `handle_get_links`, `handle_archive_restore`, `handle_auto_tag`, `handle_detect_contradiction`
- **CLI `validate_id()` defense-in-depth**: Added `validate_id()` to `cmd_get`, `cmd_update`, `cmd_delete`, `cmd_promote`

### Added

- `Tier::rank()` method for numeric tier comparison (Short=0, Mid=1, Long=2)
- 5 new unit tests: `tier_rank_ordering`, `update_rejects_tier_downgrade_long_to_short`, `update_rejects_tier_downgrade_long_to_mid`, `update_allows_tier_upgrade_short_to_long`, `update_allows_same_tier`
- 6 new integration tests: `test_cli_validate_id_rejects_invalid`, `test_tier_downgrade_rejected`, `test_tier_upgrade_allowed`, `test_duplicate_title_no_self_contradiction`, `test_promote_clears_expires_at`, `test_version_flag_patch2`

### Test Coverage

| Metric | Count |
|--------|-------|
| Unit tests | 139 |
| Integration tests | 49 |
| **Total** | **188** |
| Modules with tests | 15/15 |

## [0.5.4-patch.1] — 2026-04-12

### Fixed

- `--version` / `-V` flag missing — added `version` to `#[command]` attribute
- CLI `update` rejected past `expires_at` — changed to format-only validation, matching MCP behavior
- `archive_restore` tier promotion — release binary now includes `'long'` hardcoded in INSERT SQL

## [0.5.4] — 2026-04-12

### Added

- **Configurable TTL per tier**: `[ttl]` section in config.toml with 5 overrides: `short_ttl_secs`, `mid_ttl_secs`, `long_ttl_secs`, `short_extend_secs`, `mid_extend_secs`. Set to 0 to disable expiry.
- **Archive before GC deletion**: Expired memories archived to `archived_memories` table before deletion (default: `true`). Configurable via `archive_on_gc` in config.toml.
- 4 new MCP tools: `memory_archive_list`, `memory_archive_restore`, `memory_archive_purge`, `memory_archive_stats` (21 total)
- 4 new HTTP endpoints: `GET/DELETE /api/v1/archive`, `POST /api/v1/archive/{id}/restore`, `GET /api/v1/archive/stats` (24 total)
- `archive` CLI subcommand with `list`, `restore`, `purge`, `stats` actions (26 total commands)
- Schema migration v4: `archived_memories` table with indexes
- `TtlConfig` and `ResolvedTtl` types in config.rs for type-safe TTL resolution
- TTL values clamped to 10-year maximum to prevent integer overflow
- Negative `older_than_days` rejected in archive purge
- Archive restore checks for active ID collision (prevents silent overwrite)
- `validate_id()` on all archive restore endpoints (HTTP, MCP, CLI)

### Changed

- `db::update()` returns `(bool, bool)` — `(found, content_changed)` — for embedding regeneration
- `db::touch()` accepts configurable `short_extend` / `mid_extend` parameters
- `db::gc()` accepts `archive: bool` parameter
- `db::recall()` and `db::recall_hybrid()` accept configurable extend values
- All `gc_if_needed` callers respect `archive_on_gc` config setting
- Update facility: tier downgrade protection, title collision detection, embedding regeneration on content change

### Fixed

- Embeddings not regenerated on content update via `memory_update` (MCP + dedup store path)
- Tier downgrade not protected in update path (long never downgrades, mid never to short)
- Title+namespace collision on update returned opaque error (now returns 409 CONFLICT)
- MCP and CLI update handlers missing `validate_id()` call
- Negative TTL extension values now clamped to 0

## [0.5.2] — 2026-04-08

### Added

- Fedora COPR: `sudo dnf copr enable alpha-one-ai/ai-memory && sudo dnf install ai-memory`
- CI workflow for automated COPR upload on tag push
- debian/ packaging directory (control, rules, changelog, copyright)
- RPM spec file (ai-memory.spec) for COPR builds
- OpenClaw as 9th supported AI platform across all docs
- Animated architecture SVG and benchmark SVG in README
- Fedora/RHEL COPR and Ubuntu PPA install cards on GitHub Pages (8 install methods)

### Changed

- GitHub Pages professionalized: condensed hero, 13→7 nav links, 7→4 stats
- Install method count updated to 8 across all docs

## [0.5.1] — 2026-04-08

### Added

- Docker image auto-published to GitHub Container Registry (ghcr.io) on tag push
- `server.json` manifest for Official MCP Registry (modelcontextprotocol/registry)
- CONTRIBUTING.md, CHANGELOG.md, CODE_OF_CONDUCT.md
- Open Graph and Twitter Card meta tags on GitHub Pages
- Scope tables for all 9 AI platform tabs on GitHub Pages
- `mine` command documented across all docs (USER_GUIDE, ADMIN_GUIDE, DEVELOPER_GUIDE, index.html)
- Error code reference in DEVELOPER_GUIDE (NOT_FOUND, VALIDATION_FAILED, DATABASE_ERROR, CONFLICT)
- config.toml reference section in ADMIN_GUIDE
- Store command flags (`--source`, `--expires-at`, `--ttl-secs`) documented in README

### Changed

- Dockerfile: Rust 1.82 → 1.86, added build-essential, added benches/ copy
- Dockerfile: version label 0.4.0 → 0.5.0
- CI workflow: added Docker (GHCR) job triggered on tag push
- Claude Code MCP config: corrected from `~/.claude/.mcp.json` to three-scope model (`~/.claude.json`, `.mcp.json`, project-local)
- All 8 AI platform configs: added Windows paths, env var syntax, scope tables
- Hybrid recall blend weights: corrected docs from 50/50 & 85/15 to 60/40 (matches code)
- Default tier: corrected docs from "keyword" to "semantic" (matches code)
- Test count: corrected from 167 to 161 (118 unit + 43 integration)
- Module count: corrected from 14 to 15 (added mine.rs)
- CLI command count: corrected from 24 to 25 (added mine)

### Fixed

- Dockerfile build failure: missing benches/ directory, outdated Rust version, missing C++ compiler

## [0.5.0] — 2026-04-08

### Added

- MCP server with 17 tools for AI-native memory management
- HTTP API with 20 endpoints for external integration
- CLI with 25 commands for local operation and scripting
- 4 feature tiers (Core, Standard, Advanced, Enterprise) for flexible deployment
- TOON format for structured, topology-aware memory representation
- Hybrid recall engine combining semantic search, keyword matching, and graph traversal
- Multi-node sync for distributed memory across instances
- Auto-consolidation to merge and deduplicate related memories
- `mine` command for importing memories from conversation history
- LongMemEval benchmark support achieving 97.8% Recall@5

### Changed

- Upgraded memory storage layer for improved write throughput
- Refined relevance scoring in hybrid recall for better precision
- Improved CLI output formatting and error messages

### Fixed

- Resolved race condition during concurrent memory writes
- Fixed encoding issue with non-ASCII content in TOON format
- Corrected sync conflict resolution when timestamps are identical

## [0.4.0]

### Added

- Initial MCP server implementation with core tool set
- Basic memory storage and retrieval
- CLI foundation with essential commands
- Semantic search over stored memories
- SQLite-backed persistent storage

### Changed

- Migrated internal data model to support richer metadata

### Fixed

- Fixed crash on empty query input
- Resolved file descriptor leak in long-running server mode

## [0.3.0]

### Added

- Embedding-based semantic search
- Memory tagging and filtering
- Configuration file support

### Changed

- Switched to async I/O for server operations

### Fixed

- Fixed memory leak during large batch imports

## [0.2.0]

### Added

- Persistent storage backend
- Basic CLI for memory CRUD operations
- JSON export and import

### Fixed

- Fixed incorrect timestamp handling across time zones

## [0.1.0]

### Added

- Initial prototype with in-memory storage
- Core data model for memory entries
- Basic search functionality

[0.5.2]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/alphaonedev/ai-memory-mcp/releases/tag/v0.1.0
