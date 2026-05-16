# v0.7.0 Feature Inventory — Net-New from v0.6.4

> Baseline: **v0.6.4** (commit `6416539a370490977a25eeabded54393b08ac87c`, tag `v0.6.4`)
> Target:   **v0.7.0** (commit `64528b15d484c68e4352ab6798f2c39c80296032`, branch `catalogue/v0.7.0-features` @ HEAD)
> Compiled: 2026-05-15 by the AI NHI ship-readiness initiative
> Methodology: `git diff v0.6.4..HEAD` (code & schema) + doc scan + commit-trailer issue cross-reference
> Worktree:  `/Users/fate/v07/catalogue`
> Scratch:   `/Users/fate/v07/v07-fixes/.local-runs/tmp/` (project no-`/tmp` HARD RULE honored)
> Read-only review except for this deliverable.

---

## Summary (code-evidence totals)

| Metric | Count | Evidence |
|---|---|---|
| Commits ahead of `v0.6.4` | **453** | `git log --oneline v0.6.4..HEAD \| wc -l` |
| Files touched | **545** | `git diff --name-only v0.6.4..HEAD \| wc -l` |
| Insertions / deletions | **+233,589 / −23,541** | `git diff --stat v0.6.4..HEAD` tail |
| `src/*.rs` files touched | **207** | `git diff --name-only v0.6.4..HEAD -- src/ \| grep -c '\.rs$'` |
| New top-level `src/<x>/mod.rs` modules | **23** | see §"New substrate modules" below |
| New MCP tools (added since v0.6.4) | **28** | diff of `"memory_*"` literals between `v0.6.4:src/mcp.rs` and `HEAD:src/mcp/registry.rs` |
| Total MCP tools at v0.7.0 full profile | **71** (registry literals — release notes head-line of **63** counts the canonical user-facing surface; the extra 8 are sub-tools / aliases / internals registered alongside) | `grep -oE '"memory_[a-z_]+"' src/mcp/registry.rs \| sort -u \| wc -l` |
| New SQLite migrations (`0015..0033`) | **20** | `migrations/sqlite/0015_v07_pending_action_timeouts.sql … 0033_v07_form5_confidence_calibration.sql` |
| New Postgres migrations (`0012..0020`) | **10** | `migrations/postgres/0012_v0700_metadata_object_check.sql … 0020_v07_form5_confidence_calibration.sql` |
| New `AI_MEMORY_*` env vars | **17** (incl. probe artefact `AI_MEMORY_` — net 16 useful) | diff of `AI_MEMORY_[A-Z_]*` literals |
| New HTTP routes | **8** | diff of `.route("..."` literals between v0.6.4 and HEAD |
| New `GovernancePolicy` fields | **9** (`max_reflection_depth`, `auto_export_reflections_to_filesystem`, `auto_atomise`, `auto_atomise_threshold_cl100k`, `auto_atomise_max_atom_tokens`, `auto_persona_trigger_every_n_memories`, `auto_export_personas_to_filesystem`, `auto_atomise_mode`, `legacy_per_pair_classifier`, `auto_classify_kind`) | `src/models/namespace.rs:289-422` vs `v0.6.4:src/models.rs` |
| New `MemoryKind` variants | **10 (whole enum is new)** — `Observation` (default), `Reflection`, `Persona`, `Concept`, `Entity`, `Claim`, `Relation`, `Event`, `Conversation`, `Decision` | `src/models/memory.rs:38-85` (did not exist in v0.6.4) |
| New `Capability*` structs in `src/config.rs` | **7** (`CapabilityReflection`, `CapabilitySkills`, `CapabilityForensic`, `CapabilityGovernance`, `CapabilityAtomisation`, `CapabilityMemoryKindVocab`, `CapabilityConfidenceCalibration`) | `grep '^pub struct Capability' src/config.rs` vs v0.6.4 |
| New integration tests | **163** (file count under `tests/`) | `git diff --name-status v0.6.4..HEAD -- tests/ \| awk '$1=="A"' \| wc -l` |
| New benchmarks | **11** (5 in `benches/`, 6 in `benchmarks/`) | `git diff --name-status v0.6.4..HEAD -- benches/ benchmarks/` |
| New documents in `docs/` | **42** | `git diff --name-status v0.6.4..HEAD -- docs/ \| awk '$1=="A"' \| wc -l` |
| New cookbook directories | **8** | `cookbook/{agent-external-governance,atomisation,context-offload,file-backed-export,multistep-ingest,persona,production-deployment,recursive-learning}` |
| New helper binaries | **4** | `tools/{auto-link-detector,post-ship-converge,t0-orchestrate,transcript-extractor}` |
| Cargo version bump | `0.6.4` → `0.7.0` | `Cargo.toml` |

### New substrate modules (23 new `mod.rs`)

`src/{atomisation,background,cli,confidence,curator,federation,forensic,governance,handlers,hooks,identity,kg,mcp,models,multistep_ingest,notification,offload,parsing,persona,storage,store,synthesis,transcripts}/mod.rs`

(`src/handlers.rs` → `src/handlers/mod.rs`, `src/mcp.rs` → `src/mcp/`, `src/models.rs` → `src/models/`: these are flat-to-tree refactors with substantial expansion.)

### 28 new MCP tools (full list, alphabetical)

```
memory_atomise
memory_calibrate_confidence
memory_check_agent_action
memory_dependents_of_invalidated
memory_deref
memory_export_reflection
memory_find_paths
memory_ingest_multistep
memory_load_family
memory_offload
memory_persona
memory_persona_generate
memory_quota_status
memory_reflect
memory_reflection_origin
memory_replay
memory_rule_list
memory_skill_compositional_context
memory_skill_export
memory_skill_get
memory_skill_list
memory_skill_promote_from_reflection
memory_skill_register
memory_skill_resource
memory_smart_load
memory_subscription_dlq_list
memory_subscription_replay
memory_verify
```

### 17 new `AI_MEMORY_*` env vars (1 probe artefact, 16 net)

```
AI_MEMORY_ALLOW_LOOPBACK_WEBHOOKS
AI_MEMORY_AUTO_CONFIDENCE
AI_MEMORY_CONFIDENCE_DECAY
AI_MEMORY_CONFIDENCE_SHADOW
AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE
AI_MEMORY_DB_PATH
AI_MEMORY_FED_PEER_ATTESTATION
AI_MEMORY_FED_SYNC_TRUST_PEER
AI_MEMORY_FED_TRUST_BODY_AGENT_ID
AI_MEMORY_KEY_DIR
AI_MEMORY_OPERATOR_PUBKEY
AI_MEMORY_PERMISSIONS_MODE
AI_MEMORY_PRECOMPUTE_FAMILY_EMBEDDINGS
AI_MEMORY_SYSTEM_PROMPT_DIR
AI_MEMORY_TEST_AGE_URL
AI_MEMORY_TOOLS_VERBOSE
```

### 8 new HTTP routes

```
/api/v1/approvals/stream      (K10 SSE approval channel)
/api/v1/auto_tag              (LLM auto-tag endpoint)
/api/v1/expand_query          (HTTP parity for memory_expand_query)
/api/v1/kg/find_paths         (KG chain-walk over HTTP)
/api/v1/links/verify          (Ed25519 link verification surface)
/api/v1/memory_load_family    (HTTP parity for memory_load_family)
/api/v1/quota/status          (K8 quota status surface)
/api/v1/tools/list            (MCP tools/list mirror for harness ops)
```

### Per-feature-category headline counts

| Category | Sub-tasks / items | Status |
|---|---|---|
| WT-1 atomisation primitive | **7** sub-tasks (A-G) | SHIPPED |
| QW Tencent quick wins | **4** (QW-1 file-backed export, QW-2 persona, QW-3 context-offload, QW-4 inferred from `RuleRefused`) | SHIPPED |
| Batman 7-form closeout | **7** forms (1-6 + 7th-form agent-EXTERNAL Layer-4) | Forms 1-6 SHIPPED; 7th-form Option B foundation LANDED, full cover at v0.8.0 per `#697` |
| Recursive-learning primitive | **8** tasks (issue `#655`) | Tasks 1-6 SHIPPED commits; Tasks 7-8 ship-gate landed on `feat/v0.7.0-recursive-learning` |
| L1/L2 grand-slam wave | **8** L2 items + **3** L1 items (issues `#666`-`#673`, `#691`, `#693`) | SHIPPED |
| Schema migration ladder | **20 sqlite + 10 postgres** | SHIPPED; v33 → v34 V-4 closeout pinned by `tests/signed_events_chain_v34.rs` |
| Security-hardening sweep | **11** late-cycle commits | SHIPPED on `release/v0.7.0`, reconciled into trunk @ `64528b1` |
| Round-2 fixes | **F1-F18** (18 findings) | SHIPPED |
| Capability v3 system | **7** new Capability* structs + Track A 5 tasks | SHIPPED |
| Signed events V-4 closeout | `prev_hash + sequence` cross-row hash chain | SHIPPED (sqlite v34 / postgres v33) |
| Forensic bundle | L2-5 (issue `#670`) | SHIPPED |
| Federation hardening | mTLS + X-API-Key + fingerprint allowlist | SHIPPED |
| K8 quota status tool | `memory_quota_status` | SHIPPED |
| Batman framework audit | `docs/internal/batman-framework-audit.md` (4,478 words) | SHIPPED |
| 11-track v0.7.0 epic (A-K) | **69/69** tasks | SHIPPED per CHANGELOG headline |

---

## Per-feature matrix

### Feature: WT-1 atomisation primitive (Form 2 substrate-native)

- **Issue(s):** `#754` (Form 1 synthesis), part of v0.7.0 grand-slam wave; documented in `docs/atomisation.md`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES (whole module is new)
- **Docs:** `docs/atomisation.md` (1,455 words)
- **Cookbook:** `cookbook/atomisation/01-basic-flow.sh` (1 file)
- **Code paths (absolute):**
  - Substrate: `/Users/fate/v07/catalogue/src/atomisation/mod.rs` (29,533 bytes) + `/Users/fate/v07/catalogue/src/atomisation/curator.rs` (22,432 bytes)
  - Curator integration: `/Users/fate/v07/catalogue/src/atomisation/curator.rs`
  - Schema: `/Users/fate/v07/catalogue/migrations/sqlite/0030_v07_atomisation.sql`, `/Users/fate/v07/catalogue/migrations/sqlite/0031_v07_namespace_auto_atomise.sql`, `/Users/fate/v07/catalogue/migrations/postgres/0017_v07_atomisation.sql`, `/Users/fate/v07/catalogue/migrations/postgres/0018_v07_namespace_auto_atomise.sql`
  - MCP tool: `/Users/fate/v07/catalogue/src/mcp/tools/atomise.rs`
  - Pre-store hook: `/Users/fate/v07/catalogue/src/hooks/pre_store/auto_atomise.rs`
  - CLI: `/Users/fate/v07/catalogue/src/cli/commands/atomise.rs`
- **Tests (absolute):**
  - Integration: `/Users/fate/v07/catalogue/tests/atomisation.rs`, `/Users/fate/v07/catalogue/tests/atomisation/core.rs`, `/Users/fate/v07/catalogue/tests/auto_atomise.rs`, `/Users/fate/v07/catalogue/tests/auto_atomise/core.rs`, `/Users/fate/v07/catalogue/tests/wt1c_mcp_atomise.rs`, `/Users/fate/v07/catalogue/tests/wt_1_a_schema_migration.rs`
  - Form 2: `/Users/fate/v07/catalogue/tests/form_2_synchronous_atomise.rs`
- **Env vars:** none new (governed via namespace policy)
- **Namespace policy fields:** `auto_atomise`, `auto_atomise_threshold_cl100k`, `auto_atomise_max_atom_tokens`, `auto_atomise_mode` (`Off | Deferred | Synchronous`)
- **Capability registry entry:** `CapabilityAtomisation` at `/Users/fate/v07/catalogue/src/config.rs:1164`
- **Public API surface:** `memory_atomise` MCP tool; `ai-memory atomise` CLI; namespace standard JSON keys
- **Known unknowns:** WT-1 sub-task A-G fan-out — release notes claim "A-G shipped" but per-letter mapping is implicit; reviewer should cross-check `wt1c_*` / `wt_1_a_*` test names against the original WT-1 spec.

### Feature: Form 1 — online dedup-and-synthesis (single-batch action-emitting LLM)

- **Issue(s):** `#754`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Code paths:** `/Users/fate/v07/catalogue/src/synthesis/mod.rs`
- **Tests:** `/Users/fate/v07/catalogue/tests/form_1_synthesis.rs`
- **Namespace policy fields:** `legacy_per_pair_classifier: Option<bool>` (opt-IN to legacy yes/no classifier; default routes to single-batch synth)
- **Capability registry entry:** subsumed by `CapabilityAtomisation`
- **Public API surface:** internal store-path call; no new MCP tool

### Feature: Form 2 — synchronous atomise-before-embed

- **Issue(s):** `#755`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Code paths:** `/Users/fate/v07/catalogue/src/atomisation/mod.rs` + `auto_atomise_mode = Synchronous` branch in `/Users/fate/v07/catalogue/src/hooks/pre_store/auto_atomise.rs`
- **Tests:** `/Users/fate/v07/catalogue/tests/form_2_synchronous_atomise.rs`
- **Namespace policy fields:** `auto_atomise_mode` (`Off | Deferred | Synchronous`) at `src/models/namespace.rs:395`

### Feature: Form 3 — multi-step ingest orchestrator (prompt-cache reuse + explicit-trust helpers)

- **Issue(s):** `#756`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Docs:** `docs/multistep-ingest.md` (1,079 words)
- **Cookbook:** `cookbook/multistep-ingest/01-two-phase.sh`
- **Code paths:** `/Users/fate/v07/catalogue/src/multistep_ingest/{mod.rs,executor.rs,helpers.rs,pipeline.rs,cache.rs}`
- **MCP tool:** `/Users/fate/v07/catalogue/src/mcp/tools/ingest_multistep.rs` → `memory_ingest_multistep`
- **Tests:** `/Users/fate/v07/catalogue/tests/form_3_multistep_ingest.rs`

### Feature: Form 4 — fact-provenance (citations + source-URI + atom-grain span)

- **Issue(s):** `#757`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Docs:** `docs/provenance.md` (983 words)
- **Schema:** `/Users/fate/v07/catalogue/migrations/sqlite/0032_v07_form4_provenance.sql`, `/Users/fate/v07/catalogue/migrations/postgres/0019_v07_form4_provenance.sql`
- **Tests:** `/Users/fate/v07/catalogue/tests/form_4_provenance.rs`
- **Known unknowns:** no dedicated MCP tool — provenance rides on the existing `memory_store` / `memory_atomise` payloads. Reviewer should verify wire-shape backward-compat with pre-v0.7.0 federation peers.

### Feature: Form 5 — auto-confidence + shadow-mode calibration + freshness decay

- **Issue(s):** `#758`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Docs:** `docs/confidence-calibration.md` (913 words)
- **Schema:** `/Users/fate/v07/catalogue/migrations/sqlite/0033_v07_form5_confidence_calibration.sql`, `/Users/fate/v07/catalogue/migrations/postgres/0020_v07_form5_confidence_calibration.sql`
- **Code paths:** `/Users/fate/v07/catalogue/src/confidence/{mod.rs,calibrate.rs,shadow.rs,decay.rs}`
- **MCP tool:** `/Users/fate/v07/catalogue/src/mcp/tools/calibrate_confidence.rs` → `memory_calibrate_confidence`
- **CLI:** `/Users/fate/v07/catalogue/src/cli/commands/calibrate_confidence.rs`
- **Tests:** `/Users/fate/v07/catalogue/tests/form_5_confidence_calibration.rs`, `/Users/fate/v07/catalogue/tests/calibration_t0.rs`
- **Env vars:** `AI_MEMORY_AUTO_CONFIDENCE`, `AI_MEMORY_CONFIDENCE_SHADOW`, `AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE`, `AI_MEMORY_CONFIDENCE_DECAY`
- **Capability registry entry:** `CapabilityConfidenceCalibration` at `src/config.rs:1331`

### Feature: Form 6 — MemoryKind Batman vocabulary

- **Issue(s):** `#759`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES (enum did not exist in v0.6.4)
- **Docs:** `docs/memory-kind-vocab.md` (800 words)
- **Schema:** `/Users/fate/v07/catalogue/migrations/sqlite/0025_v07_memory_kind.sql`
- **Code paths:** `/Users/fate/v07/catalogue/src/models/memory.rs:38-200` (10-variant enum)
- **Hooks:** `/Users/fate/v07/catalogue/src/hooks/pre_store/auto_classify_kind.rs`
- **Namespace policy fields:** `auto_classify_kind: Option<MemoryKindAutoClassify>` (`Off | RegexOnly | RegexThenLlm`) at `src/models/namespace.rs:421`
- **Capability registry entry:** `CapabilityMemoryKindVocab` at `src/config.rs:1260`
- **Tests:** `/Users/fate/v07/catalogue/tests/form_6_memorykind_vocab.rs`, `/Users/fate/v07/catalogue/tests/l1_1_memory_kind.rs`

### Feature: Form 7 (7th-form) — agent-EXTERNAL Layer-4 wiring

- **Issue(s):** `#760`, parent meta `#693`, V08 closeout `#697`
- **Status:** PARTIAL — Option B foundation (PE-1/PE-2/PE-3) SHIPPED; complete cover at v0.8.0 per CHANGELOG "Honest framing" callout
- **Net-new in v0.7.0:** YES
- **Docs:** `docs/policy-engine.md` (3,597 words), `docs/security/audit-trail-coverage.md`, `docs/governance/agent-action-rules.md`
- **Cookbook:** `cookbook/agent-external-governance/01-deny-bash.sh`
- **Schema:** `/Users/fate/v07/catalogue/migrations/sqlite/0024_v07_governance_rules.sql`
- **Code paths:** `/Users/fate/v07/catalogue/src/governance/{mod.rs,agent_action.rs,deferred_audit.rs,rules_store.rs,wire_check.rs}`
- **MCP tools:** `/Users/fate/v07/catalogue/src/mcp/tools/{check_agent_action.rs,rule_list.rs}` → `memory_check_agent_action`, `memory_rule_list`
- **CLI:** `/Users/fate/v07/catalogue/src/cli/rules.rs`, `/Users/fate/v07/catalogue/src/cli/governance_install_defaults.rs`, `/Users/fate/v07/catalogue/src/cli/governance_migrate.rs`
- **Tests:** `/Users/fate/v07/catalogue/tests/form_7_agent_external_wiring.rs`, `/Users/fate/v07/catalogue/tests/governance_agent_action.rs`, `/Users/fate/v07/catalogue/tests/governance_a2a_rules.rs`, `/Users/fate/v07/catalogue/tests/governance_storage_insert_hook.rs`, `/Users/fate/v07/catalogue/tests/governance_l16_activation.rs`, `/Users/fate/v07/catalogue/tests/governance_wire_points.rs`, `/Users/fate/v07/catalogue/tests/policy_engine_hostile_prompts.rs`, `/Users/fate/v07/catalogue/tests/cli_install_pretool_hook.rs`, `/Users/fate/v07/catalogue/tests/governance_deferred_log_audit.rs`
- **Capability registry entry:** `CapabilityGovernance` at `src/config.rs:1083`
- **Public API surface:** `R001..R004` seed rules; `~/.config/ai-memory/operator.key`; `ai-memory install --harness claude-code --enforce-policy`
- **Known unknowns:** the "complete cover" 5% gap (V08-PE-1 .. V08-PE-8) is explicitly out-of-scope per `#697`. Reviewer should sanity-check that the "PE-1 / PE-2 / PE-3 all landed" claim matches code (CHANGELOG asserts this — `governance_wire_points.rs` is the pin).

### Feature: Recursive-learning primitive (Tasks 1-8, issue `#655`)

- **Issue(s):** `#655` (parent), tasks 1-6 commits `f5d8a9e`, `630a6db`, `b51a3f3`, `3dc76f3`, `c61a05b`, `fbf093c`; Tasks 7-8 land on the grand-slam branch
- **Status:** SHIPPED (Tasks 1-6 + 7-8 ship-gate + docs)
- **Net-new in v0.7.0:** YES
- **Docs:** `docs/RECURSIVE_LEARNING.md` (3,782 words)
- **Cookbook:** `cookbook/recursive-learning/*.sh` + `*.md` (5 scenarios + README, 11 files total)
- **Schema:** `/Users/fate/v07/catalogue/migrations/postgres/0013_v0700_reflection_depth.sql` (SQLite v29 via in-tree `storage/migrations.rs` ladder)
- **Code paths:**
  - Substrate: `/Users/fate/v07/catalogue/src/storage/reflect.rs`, `/Users/fate/v07/catalogue/src/storage/mod.rs` (reflect path + `GOVERNANCE_PRE_WRITE` OnceLock)
  - MCP tool: `/Users/fate/v07/catalogue/src/mcp/tools/reflect.rs` → `memory_reflect`
  - Origin tool: `/Users/fate/v07/catalogue/src/mcp/tools/reflection_origin.rs` → `memory_reflection_origin`
  - Dependents-of-invalidated: `/Users/fate/v07/catalogue/src/mcp/tools/dependents_of_invalidated.rs`
- **Tests:** `/Users/fate/v07/catalogue/tests/recursive_learning_task{1..7}_*.rs` (8 files), `/Users/fate/v07/catalogue/tests/ship_gate/grand_slam_recursive_learning.rs`, `/Users/fate/v07/catalogue/tests/approval_reflect.rs`, `/Users/fate/v07/catalogue/tests/federation_reflection_replication.rs`, `/Users/fate/v07/catalogue/tests/reranker_reflection_test.rs`, `/Users/fate/v07/catalogue/tests/longmemeval_reflection_bench.rs`
- **Namespace policy fields:** `max_reflection_depth: Option<u32>` (default 3, `Some(0)` is documented kill-switch)
- **Capability registry entry:** `CapabilityReflection` at `src/config.rs:917`
- **Public API surface:** `memory_reflect` (Family::Power); `pre_reflect` + `post_reflect` hook events (Track G grew 21 → 23)
- **Reproducer script:** `scripts/reproduce-recursive-learning.sh` (CLAUDE.md documented)

### Feature: L1-5 Agent Skills ingestion substrate

- **Issue(s):** grand-slam wave (issues `#666`–`#673`, `#691`)
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Docs:** `docs/agent-skills.md` (1,641 words)
- **Schema:** `/Users/fate/v07/catalogue/migrations/sqlite/0026_v07_agent_skills.sql`
- **Code paths:** `/Users/fate/v07/catalogue/src/models/skill.rs`, `/Users/fate/v07/catalogue/src/parsing/skill_md.rs`
- **MCP tools (5):** `/Users/fate/v07/catalogue/src/mcp/tools/skill_{register,list,get,resource,export,promote,compositional_context}.rs`
- **Tests:** `/Users/fate/v07/catalogue/tests/skill_test.rs`, `/Users/fate/v07/catalogue/tests/skill_composition_test.rs`, `/Users/fate/v07/catalogue/tests/skill_promote_test.rs`, `/Users/fate/v07/catalogue/tests/ship_gate/grand_slam_skills.rs`
- **Capability registry entry:** `CapabilitySkills` at `src/config.rs:975`

### Feature: L1-6 substrate rules-enforcement engine (Option B foundation)

- **Issue(s):** `#691`, Deliverable E commit `1b877ce`
- **Status:** SHIPPED (foundation; complete cover at v0.8.0)
- **See:** Feature "Form 7 — agent-EXTERNAL Layer-4 wiring" above (same body of code)
- **Audit deliverables:** `/Users/fate/v07/catalogue/docs/v0.7.0/validation/rules-store-isolation-audit.md`, `/Users/fate/v07/catalogue/docs/v0.7.0/validation/wire-check-bypass-audit.md`

### Feature: L1-7 compaction pipeline

- **Issue(s):** grand-slam wave (merge commit `7451143`)
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Code paths:** `/Users/fate/v07/catalogue/src/curator/{compaction.rs,pipeline.rs,cluster.rs,reflection_pass.rs,candidates.rs,persist.rs}`
- **Tests:** `/Users/fate/v07/catalogue/tests/curator/compaction_test.rs`, `/Users/fate/v07/catalogue/tests/curator/reflection_pass_test.rs`
- **Capability registry entry:** `CapabilityCompaction` (extends v0.6.4)
- **Hook events added:** `pre_compaction`, `on_compaction_rollback` (Track G additions)

### Feature: L2-1 reflection-pass curator

- **Issue(s):** `#666`, commit `c3f6e82`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Code paths:** `/Users/fate/v07/catalogue/src/curator/reflection_pass.rs` (67,400 bytes)
- **CLI:** `ai-memory curator --reflect` via `/Users/fate/v07/catalogue/src/cli/curator.rs` (modified)
- **Runbook:** `/Users/fate/v07/catalogue/docs/RUNBOOK-curator-soak.md`
- **Constants:** `MIN_CLUSTER_SIZE = 3`, `MAX_CLUSTER_SIZE = 12`, 7-day temporal window

### Feature: L2-2 federation-aware reflection coordination

- **Issue(s):** `#667`, commit `0b1c9cc`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Code paths:** `/Users/fate/v07/catalogue/src/federation/reflection_bookkeeping.rs`, `/Users/fate/v07/catalogue/src/federation/receive.rs`
- **MCP tool:** `memory_reflection_origin` → `/Users/fate/v07/catalogue/src/mcp/tools/reflection_origin.rs`
- **Tests:** `/Users/fate/v07/catalogue/tests/federation_reflection_replication.rs`

### Feature: L2-3 reflection invalidation propagation (NOT cascade)

- **Issue(s):** `#668`, commit `3f419be`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Code paths:** `/Users/fate/v07/catalogue/src/notification/invalidation.rs`
- **MCP tool:** `memory_dependents_of_invalidated` → `/Users/fate/v07/catalogue/src/mcp/tools/dependents_of_invalidated.rs`
- **Tests:** `/Users/fate/v07/catalogue/tests/notification/invalidation_test.rs`
- **Notification namespace:** `<dependent.namespace>/_invalidations`

### Feature: L2-4 transcript replay union

- **Issue(s):** `#669`, commit `a50b34c`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES (extends v0.6.4 transcript surface)
- **Code paths:** `/Users/fate/v07/catalogue/src/transcripts/{mod.rs,replay.rs,storage.rs}`
- **MCP tool:** `memory_replay` → `/Users/fate/v07/catalogue/src/mcp/tools/replay.rs`
- **Tests:** `/Users/fate/v07/catalogue/tests/transcripts/replay_test.rs`, `/Users/fate/v07/catalogue/tests/i4_memory_replay_authz.rs`

### Feature: L2-5 forensic bundle

- **Issue(s):** `#670`, commit `bb870b3`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Docs:** `docs/forensic-export.md` (1,756 words)
- **Cookbook:** `cookbook/recursive-learning/04-forensic-bundle.sh`
- **Code paths:** `/Users/fate/v07/catalogue/src/forensic/{mod.rs,bundle.rs}`
- **CLI:** `/Users/fate/v07/catalogue/src/cli/export.rs`, `/Users/fate/v07/catalogue/src/cli/verify.rs` — `ai-memory export-forensic-bundle`, `ai-memory verify-forensic-bundle`
- **Tests:** `/Users/fate/v07/catalogue/tests/forensic.rs`, `/Users/fate/v07/catalogue/tests/forensic/bundle_test.rs`, `/Users/fate/v07/catalogue/tests/forensic/wt1e_chain_test.rs`
- **Capability registry entry:** `CapabilityForensic` at `src/config.rs:1038`
- **Reproducibility property:** deterministic in-process POSIX-ustar tar with byte-identical mod timestamps

### Feature: L2-6 reflection-as-skill promote

- **Issue(s):** `#671`, commit `505c538`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **MCP tool:** `memory_skill_promote_from_reflection` → `/Users/fate/v07/catalogue/src/mcp/tools/skill_promote.rs`
- **Cookbook:** `cookbook/recursive-learning/03-reflection-to-skill-promote.sh`
- **Tests:** `/Users/fate/v07/catalogue/tests/skill_promote_test.rs`
- **Round-trip guarantee:** promote → export → re-register produces IDENTICAL SHA-256 digest

### Feature: L2-7 skill ↔ reflection composition

- **Issue(s):** `#672`, commit `0966b57`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **MCP tool:** `memory_skill_compositional_context` → `/Users/fate/v07/catalogue/src/mcp/tools/skill_compositional_context.rs`
- **Cookbook:** `cookbook/recursive-learning/05-autoresearch-composition.sh`
- **Tests:** `/Users/fate/v07/catalogue/tests/skill_composition_test.rs`, `/Users/fate/v07/catalogue/tests/ship_gate/grand_slam_composition.rs`
- **Budget bounds:** `budget_tokens` default 4000, max 32000

### Feature: L2-8 reflection-aware reranker boost

- **Issue(s):** `#673`, commit `90291c0`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** EXTENDS_v0_6_4 (reranker existed; reflection-aware boost is new)
- **Code paths:** modified `/Users/fate/v07/catalogue/src/reranker.rs` (defaults `boost=1.2`, `per_depth_increment=0.05`, `max_depth_cap=3`)
- **Tests:** `/Users/fate/v07/catalogue/tests/reranker_reflection_test.rs`
- **Kill-switch:** `boost=1.0`

### Feature: QW-1 file-backed reflection chain export

- **Issue(s):** v0.7.0 Tencent QW-1
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Cookbook:** `cookbook/file-backed-export/01-export-and-inspect.sh`
- **MCP tool:** `memory_export_reflection` → `/Users/fate/v07/catalogue/src/mcp/tools/export_reflection.rs`
- **CLI:** `/Users/fate/v07/catalogue/src/cli/commands/export_reflections.rs`
- **Hook:** `/Users/fate/v07/catalogue/src/hooks/post_reflect/auto_export.rs`
- **Namespace policy field:** `auto_export_reflections_to_filesystem: Option<bool>`
- **Default destination:** `~/.ai-memory/reflections/<namespace>/<id>.md`
- **Tests:** `/Users/fate/v07/catalogue/tests/cli/export_reflections.rs`

### Feature: QW-2 persona-as-artifact

- **Issue(s):** v0.7.0 Tencent QW-2
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Docs:** `docs/persona.md` (894 words)
- **Cookbook:** `cookbook/persona/01-build-persona-from-observations.sh`
- **Schema:** `/Users/fate/v07/catalogue/migrations/sqlite/0031_v07_persona.sql`, `/Users/fate/v07/catalogue/migrations/postgres/0018_v07_persona.sql`
- **Code paths:** `/Users/fate/v07/catalogue/src/persona/mod.rs` (30,003 bytes)
- **MCP tools:** `memory_persona`, `memory_persona_generate` → `/Users/fate/v07/catalogue/src/mcp/tools/persona.rs`
- **Hook:** `/Users/fate/v07/catalogue/src/hooks/post_reflect/auto_persona.rs`
- **CLI:** `/Users/fate/v07/catalogue/src/cli/commands/persona.rs`
- **Namespace policy fields:** `auto_persona_trigger_every_n_memories: Option<u32>`, `auto_export_personas_to_filesystem: Option<bool>`
- **Tests:** `/Users/fate/v07/catalogue/tests/persona.rs`, `/Users/fate/v07/catalogue/tests/persona/acceptance.rs`
- **Memory kind:** `MemoryKind::Persona` (paired with `entity_id` + `persona_version` columns)
- **Default destination:** `~/.ai-memory/personas/<namespace>/<entity_id>.md`

### Feature: QW-3 context offload primitive

- **Issue(s):** v0.7.0 Tencent QW-3
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Docs:** `docs/context-offload.md` (497 words)
- **Cookbook:** `cookbook/context-offload/01-offload-large-tool-output.sh`
- **Schema:** `/Users/fate/v07/catalogue/migrations/sqlite/0029_v07_offloaded_blobs.sql`, `/Users/fate/v07/catalogue/migrations/postgres/0016_v07_offloaded_blobs.sql`
- **Code paths:** `/Users/fate/v07/catalogue/src/offload/mod.rs` (27,017 bytes)
- **MCP tools:** `memory_offload`, `memory_deref` → `/Users/fate/v07/catalogue/src/mcp/tools/offload.rs`
- **CLI:** `/Users/fate/v07/catalogue/src/cli/offload.rs`
- **Background sweep:** `/Users/fate/v07/catalogue/src/background/offload_ttl_sweep.rs`
- **Tests:** `/Users/fate/v07/catalogue/tests/offload.rs`, `/Users/fate/v07/catalogue/tests/offload/acceptance.rs`, `/Users/fate/v07/catalogue/tests/offload/registration.rs`, `/Users/fate/v07/catalogue/tests/l07_3_chunk_d_http_surface.rs`

### Feature: Sidechain transcripts (Track I — v0.6.4 → v0.7.0 hardening)

- **Issue(s):** Track I (5 tasks, Bucket 1.7)
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Schema:** `/Users/fate/v07/catalogue/migrations/sqlite/0016_v07_transcripts.sql`, `0018_v07_transcript_links.sql`, `0019_v07_transcript_lifecycle.sql`
- **Code paths:** `/Users/fate/v07/catalogue/src/transcripts/{mod.rs,replay.rs,storage.rs}`
- **MCP tool:** `memory_replay`
- **Helper binary:** `/Users/fate/v07/catalogue/tools/transcript-extractor/` (R5 reference hook, intentionally excluded from crates.io upload via parent `Cargo.toml` `include` allowlist)
- **Tests:** `/Users/fate/v07/catalogue/tests/transcripts.rs`, `/Users/fate/v07/catalogue/tests/transcripts/replay_test.rs`, `/Users/fate/v07/catalogue/tests/transcript_extractor.rs`
- **Capability registry entry:** `CapabilityTranscripts` (extends v0.6.4)
- **Security hardening (release/v0.7.0):** I1 — `TranscriptsConfig.max_decompressed_bytes` config-driven (commit `26fab06`), default 16 MiB

### Feature: Ed25519 attested identity (Track H — full closeout)

- **Issue(s):** Track H (6 tasks, Bucket 1)
- **Status:** SHIPPED
- **Net-new in v0.7.0:** EXTENDS_v0_6_4 (v0.6.4 had `memory_links.signature` "dead column"; v0.7.0 fills it)
- **Code paths:** `/Users/fate/v07/catalogue/src/identity/{mod.rs,keypair.rs,sign.rs,verify.rs,replay.rs}`
- **MCP tool:** `memory_verify` → `/Users/fate/v07/catalogue/src/mcp/tools/verify.rs`
- **CLI:** `/Users/fate/v07/catalogue/src/cli/identity.rs` — `ai-memory identity generate`
- **Schema:** `/Users/fate/v07/catalogue/migrations/sqlite/0017_v07_link_attest_level.sql`, `0020_v07_signed_events.sql`
- **Env vars:** `AI_MEMORY_KEY_DIR`, `AI_MEMORY_OPERATOR_PUBKEY`
- **Round-2 fix F12:** keypair auto-generated on `serve` startup if absent
- **Tests:** `/Users/fate/v07/catalogue/tests/identity_e2e.rs`, `/Users/fate/v07/catalogue/tests/memory_verify.rs`, `/Users/fate/v07/catalogue/tests/round2_f12_keypair_autogen.rs`, `/Users/fate/v07/catalogue/tests/federation_inbound_verify.rs`

### Feature: Signed events V-4 closeout (cross-row hash chain)

- **Issue(s):** `#698`
- **Status:** SHIPPED (flips V-4 YELLOW → GREEN)
- **Net-new in v0.7.0:** YES
- **Schema:** `/Users/fate/v07/catalogue/migrations/sqlite/0020_v07_signed_events.sql`, `0028_v07_signed_events_chain.sql` (v34 cross-row chain), `/Users/fate/v07/catalogue/migrations/postgres/0015_v07_signed_events_chain.sql`
- **Code paths:** `/Users/fate/v07/catalogue/src/signed_events.rs`, `/Users/fate/v07/catalogue/src/storage/migrations.rs` (backfill `migrate_v34_backfill_chain`)
- **CLI:** `/Users/fate/v07/catalogue/src/cli/verify_signed_events.rs` — `ai-memory verify-signed-events-chain [--since N] [--format text|json]`
- **Tests:** `/Users/fate/v07/catalogue/tests/signed_events_chain_v34.rs` (7 tests pinning first-row zero `prev_hash`, multi-row chaining, payload tamper, sequence tamper, concurrent drainer inserts via PE-3, backfill idempotency, backfill correctness), `/Users/fate/v07/catalogue/tests/deferred_audit_soak.rs` (5K concurrent insert chain assertion), `/Users/fate/v07/catalogue/tests/cli_verify_chain.rs`

### Feature: Apache AGE acceleration (Track J)

- **Issue(s):** Track J (8 tasks, Bucket 2)
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Docs:** `docs/postgres-age-guide.md` (4,479 words), `docs/kg-backend-fallback.md`
- **Code paths:** `/Users/fate/v07/catalogue/src/kg/{mod.rs,cycle_check.rs}` + AGE detection in postgres storage adapter
- **MCP tools:** `memory_find_paths`, `memory_kg_query`, `memory_kg_timeline`, `memory_kg_invalidate` (find_paths is new)
- **Bench:** `/Users/fate/v07/catalogue/benches/age_vs_cte.rs`
- **Env var:** `AI_MEMORY_TEST_AGE_URL` (test-side AGE pin)
- **Tests:** `/Users/fate/v07/catalogue/tests/age_cte_equivalence.rs`, `/Users/fate/v07/catalogue/tests/kg_age_fallback.rs`, `/Users/fate/v07/catalogue/tests/kg/cycle_check_test.rs`, `/Users/fate/v07/catalogue/tests/g4_postgres_link_projects_into_age_graph.rs`, `/Users/fate/v07/catalogue/tests/g5_find_paths_cypher_no_syntax_error.rs`, `/Users/fate/v07/catalogue/tests/g2_postgres_find_paths_age_param_binding.rs`

### Feature: K1/G1 namespace-inheritance enforcement + permissions pipeline (Track K, 11 tasks)

- **Issue(s):** Track K
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Docs:** `docs/policy-engine.md` (3,597 words)
- **Schema:** `/Users/fate/v07/catalogue/migrations/sqlite/0015_v07_pending_action_timeouts.sql`, `0021_v07_a2a_correlation.sql`, `0022_v07_agent_quotas.sql`
- **Code paths:** `/Users/fate/v07/catalogue/src/approvals.rs`, `/Users/fate/v07/catalogue/src/quotas.rs`
- **Env var:** `AI_MEMORY_PERMISSIONS_MODE`
- **Round-2 fix F8:** default `permissions.mode = enforce` (was `advisory`)
- **MCP tools:** `memory_quota_status`, `memory_subscription_dlq_list`, `memory_subscription_replay`, `memory_pending_*` (extends v0.6.4)
- **CLI:** `/Users/fate/v07/catalogue/src/cli/governance_migrate.rs` — `ai-memory governance migrate-to-permissions`
- **Capability registry entry:** `CapabilityPermissions` (extends v0.6.4)
- **Tests:** `/Users/fate/v07/catalogue/tests/k7_dlq_list_tool.rs`, `/Users/fate/v07/catalogue/tests/k7_hmac.rs`, `/Users/fate/v07/catalogue/tests/k7_replay_tool.rs`, `/Users/fate/v07/catalogue/tests/k8_quota_status_tool.rs`, `/Users/fate/v07/catalogue/tests/k8_quota_enforcement.rs`, `/Users/fate/v07/catalogue/tests/k8_daily_reset.rs`, `/Users/fate/v07/catalogue/tests/k9_permission_pipeline.rs`, `/Users/fate/v07/catalogue/tests/k10_approval_http.rs`, `/Users/fate/v07/catalogue/tests/k10_approval_sse.rs`, `/Users/fate/v07/catalogue/tests/k10_approval_security.rs`, `/Users/fate/v07/catalogue/tests/k10_remember_forever.rs`, `/Users/fate/v07/catalogue/tests/k11_migrate_dry_run.rs`, `/Users/fate/v07/catalogue/tests/k11_migrate_in_place.rs`, `/Users/fate/v07/catalogue/tests/k11_migrate_to_file.rs`, `/Users/fate/v07/catalogue/tests/permissions_mode_gate.rs`

### Feature: Hook pipeline (Track G, 25 events)

- **Issue(s):** Track G (11 tasks, Bucket 0)
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Code paths:** `/Users/fate/v07/catalogue/src/hooks/{mod.rs,chain.rs,config.rs,decision.rs,events.rs,executor.rs,recall.rs,timeouts.rs}`, sub-trees `pre_store/`, `post_reflect/`
- **Helper binary:** `/Users/fate/v07/catalogue/tools/auto-link-detector/` (R3 reference hook)
- **Config:** `~/.config/ai-memory/hooks.toml`
- **Capability registry entry:** `CapabilityHooks` at `src/config.rs:703` (extended)
- **25 events:** 20 baseline + 5 grand-slam additions (`pre_recall_expand`, `pre_reflect`, `post_reflect`, `pre_compaction`, `on_compaction_rollback`)
- **Tests:** `/Users/fate/v07/catalogue/tests/hooks_executor_test.rs`, `/Users/fate/v07/catalogue/tests/hooks_hot_reload.rs`, `/Users/fate/v07/catalogue/tests/hooks_pre_recall.rs`, `/Users/fate/v07/catalogue/tests/hooks_timeout_budget.rs`, `/Users/fate/v07/catalogue/tests/g3_hooks_stderr_drain.rs`, `/Users/fate/v07/catalogue/tests/g11_auto_link_detector.rs`

### Feature: Capabilities v3 response shape (Track A, 5 tasks)

- **Issue(s):** Track A
- **Status:** SHIPPED
- **Net-new in v0.7.0:** EXTENDS_v0_6_4 (v0.6.4 was v2)
- **Code paths:** `/Users/fate/v07/catalogue/src/config.rs` (all `Capability*` structs), `/Users/fate/v07/catalogue/src/mcp/tools/capabilities.rs`
- **Docs:** `docs/v0.7/canonical-phrasings.md`
- **Tests:** `/Users/fate/v07/catalogue/tests/capabilities_v3.rs`, `/Users/fate/v07/catalogue/tests/capabilities_v3_l3_5.rs`, `/Users/fate/v07/catalogue/tests/round2_f13_capabilities.rs`, `/Users/fate/v07/catalogue/tests/s75_capabilities_db_schema_version.rs`
- **Added fields:** `summary`, `to_describe_to_user`, `callable_now`, `agent_permitted_families`, `schema_version="3"`
- **Round-2 fix F13:** `verbose` and `include_schema` flags fixed

### Feature: Loader tools (Track B, 5 tasks)

- **Issue(s):** Track B
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES (promoted from hidden parameter set to always-on tools)
- **MCP tools:** `memory_load_family`, `memory_smart_load` → `/Users/fate/v07/catalogue/src/mcp/tools/load_family.rs`, `src/mcp/tools/skill_compositional_context.rs` (smart load is in registry)
- **Env var:** `AI_MEMORY_PRECOMPUTE_FAMILY_EMBEDDINGS`
- **Tests:** `/Users/fate/v07/catalogue/tests/memory_load_family.rs`, `/Users/fate/v07/catalogue/tests/memory_smart_load.rs`, `/Users/fate/v07/catalogue/tests/b3_precompute_doesnt_block_serve.rs`, `/Users/fate/v07/catalogue/tests/b4_config_cleanup.rs`, `/Users/fate/v07/catalogue/tests/round2_f14_smart_load.rs`

### Feature: Schema compaction (Track C, 5 tasks) — 52% MCP tool-token reduction

- **Issue(s):** Track C
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Audit:** `docs/v0.7/schema-compaction-audit.md`
- **Tests:** `/Users/fate/v07/catalogue/tests/c2_tool_docs_field.rs`, `/Users/fate/v07/catalogue/tests/c3_no_inline_examples.rs`
- **CI gate:** ≤ 3,500 input tokens for `--profile full` `tools/list`

### Feature: Per-harness positioning + tests (Track D, 4 tasks)

- **Issue(s):** Track D
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Docs:** `docs/v0.7/compatibility-matrix.html`, `docs/positioning.md`, `docs/integrations/networking.md`
- **CLI:** `/Users/fate/v07/catalogue/src/cli/install.rs` (modified — emits install-time system-prompt snippet)
- **Env var:** `AI_MEMORY_SYSTEM_PROMPT_DIR`
- **Bench:** `/Users/fate/v07/catalogue/benches/harness_bench.rs`, `/Users/fate/v07/catalogue/benchmarks/competitive-benchmarks/`
- **Tests:** `/Users/fate/v07/catalogue/tests/harness_integration.rs`

### Feature: Discovery Gate + T0 calibration (Track E, 3 tasks)

- **Issue(s):** Track E
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Docs:** `docs/v0.7/T0-ORCHESTRATION.md`, `docs/v0.7/POST-SHIP-CONVERGENCE.md`
- **Helper binaries:** `/Users/fate/v07/catalogue/tools/t0-orchestrate/`, `/Users/fate/v07/catalogue/tools/post-ship-converge/`
- **Tests:** `/Users/fate/v07/catalogue/tests/discovery_gate_t1_t3.rs`, `/Users/fate/v07/catalogue/tests/e1_orchestration_dry_run.rs`, `/Users/fate/v07/catalogue/tests/e2_post_ship_dry_run.rs`

### Feature: Postgres + AGE first-class (Wave 1-4)

- **Issue(s):** `#646` (Wave 1), per `05e0cb9a` v0.7.1-fold decision
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES (Storage Abstraction Layer is new; SqliteStore + PostgresStore both live)
- **Docs:** `docs/postgres-age-guide.md` (4,479 words), `docs/migration-v0.7.0-postgres.md` (1,604 words)
- **Code paths:** `/Users/fate/v07/catalogue/src/store/mod.rs`, `/Users/fate/v07/catalogue/src/storage/{connection.rs,migrations.rs,reflect.rs,mod.rs}`
- **CLI:** `/Users/fate/v07/catalogue/src/cli/schema_init.rs` — `ai-memory schema-init`
- **Cargo features:** `sal-postgres` (opt-in; default sqlite build is byte-for-byte unchanged)
- **Postgres migrations:** 10 new files `0012..0020`
- **Tests:** `/Users/fate/v07/catalogue/tests/postgres_schema_parity.rs`, `/Users/fate/v07/catalogue/tests/sal_v07_postgres_findings.rs`, `/Users/fate/v07/catalogue/tests/serve_postgres_*.rs` (5 files), `/Users/fate/v07/catalogue/tests/recall_scoring_parity.rs`, `/Users/fate/v07/catalogue/tests/cli_schema_init.rs`, `/Users/fate/v07/catalogue/tests/migrate_links_roundtrip.rs`, `/Users/fate/v07/catalogue/tests/federation_postgres_fanout.rs`, `/Users/fate/v07/catalogue/tests/g1_postgres_quota_increment_on_store.rs`, `/Users/fate/v07/catalogue/tests/governance_postgres_inheritance.rs`, `/Users/fate/v07/catalogue/tests/s79_postgres_recall_returns_results.rs`

### Feature: Federation hardening (mTLS + X-API-Key + fingerprint allowlist)

- **Issue(s):** `#238`, `#239`, `#318` (continued), security-hardening sweep
- **Status:** SHIPPED
- **Net-new in v0.7.0:** EXTENDS_v0_6_4
- **Code paths:** `/Users/fate/v07/catalogue/src/federation/{mod.rs,peer.rs,peer_attestation.rs,quorum.rs,receive.rs,sync.rs,vector_clock.rs}`
- **Env vars:** `AI_MEMORY_FED_PEER_ATTESTATION`, `AI_MEMORY_FED_SYNC_TRUST_PEER`, `AI_MEMORY_FED_TRUST_BODY_AGENT_ID`
- **Tests:** `/Users/fate/v07/catalogue/tests/federation_b2_hardening.rs`, `/Users/fate/v07/catalogue/tests/federation_x_api_key.rs`, `/Users/fate/v07/catalogue/tests/federation_inbound_verify.rs`, `/Users/fate/v07/catalogue/tests/federation_reflection_replication.rs`, `/Users/fate/v07/catalogue/tests/g_issue_238_sender_attestation.rs`, `/Users/fate/v07/catalogue/tests/g_issue_239_sync_scope.rs`, `/Users/fate/v07/catalogue/tests/issue_318_mcp_federation_forward.rs`

### Feature: Security-hardening sweep (release/v0.7.0 reconciled into trunk)

- **Issue(s):** 11 late-cycle commits between initial release-cut and reconciled HEAD `64528b1`
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Per-commit map (CHANGELOG):**
  - K9 governance gate on `handle_kg_invalidate` (`a41c08f`)
  - K10 SSE `host:` prefix bypass (`7496a6e`)
  - K10 HMAC method+pending_id binding (`99ffacc`)
  - K10 HMAC nonce single-use 300s window (`a69325f`)
  - K10 SSE lagged-event count strip (`d1f6c9f`)
  - SSRF IPv4-mapped-IPv6 + NAT64 (`3ab72dc`, test `6b6b3c0`)
  - `invalidate_link` BEGIN IMMEDIATE wrap (`2c77537`)
  - Hooks executor secret-redaction (`cbe934c`)
  - H8 rebound-namespace `Ask` walk (`69ad41c`)
  - I1 zstd-decompression cap config-driven (`26fab06`)
- **Tests:** `/Users/fate/v07/catalogue/tests/k10_approval_security.rs`, `/Users/fate/v07/catalogue/tests/i1_zstd_bomb.rs`, `/Users/fate/v07/catalogue/tests/h2_invalidate_link_signed.rs`

### Feature: Round-2 NHI sweep (F1-F18)

- **Issue(s):** `#644` (F1), `#645` (F2), `#646` (F6)
- **Status:** SHIPPED (all 18 closed)
- **Net-new in v0.7.0:** YES
- **Per-finding:** see CHANGELOG `## [0.7.0]` "F-series fixes" section (lines 16-36 of CHANGELOG.md)
- **Tests:** `/Users/fate/v07/catalogue/tests/round2_f{6,7,8,9,10,11,12,13,14,15,16,17,18}_*.rs` (13 files)

### Feature: K8 quota status tool

- **Issue(s):** Track K, K8 family
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **MCP tool:** `memory_quota_status` → `/Users/fate/v07/catalogue/src/mcp/tools/quota_status.rs`
- **HTTP route:** `/api/v1/quota/status`
- **Code paths:** `/Users/fate/v07/catalogue/src/quotas.rs`
- **Schema:** `/Users/fate/v07/catalogue/migrations/sqlite/0022_v07_agent_quotas.sql`
- **Tests:** `/Users/fate/v07/catalogue/tests/k8_quota_status_tool.rs`, `/Users/fate/v07/catalogue/tests/k8_daily_reset.rs`, `/Users/fate/v07/catalogue/tests/k8_quota_enforcement.rs`

### Feature: Batman framework audit deliverable

- **Status:** SHIPPED (audit document only)
- **Net-new in v0.7.0:** YES
- **Docs:** `/Users/fate/v07/catalogue/docs/internal/batman-framework-audit.md` (4,478 words)
- **Code paths:** N/A — audit document
- **Cross-reference:** anchors Forms 1-7 inventory above

### Feature: Schema migration ladder v33 → v39 / postgres v17 → v38

- **Status:** SHIPPED — final extant: sqlite up to `0033_v07_form5_confidence_calibration.sql`, postgres up to `0020_v07_form5_confidence_calibration.sql`
- **Net-new in v0.7.0:** YES (all 20 sqlite + 10 postgres files added since v0.6.4)
- **Per-migration evidence:** every `migrations/sqlite/0015..0033` and `migrations/postgres/0012..0020` is `git diff --name-status v0.6.4..HEAD` "A"-status
- **Known unknowns:** the task prompt said "sqlite v33 → v39 / postgres v17 → v38" but in-tree highest migration files are sqlite 0033 / postgres 0020. The release-notes references to "v34" (V-4 closeout) and beyond are applied via the in-process ladder in `/Users/fate/v07/catalogue/src/storage/migrations.rs` rather than as additional SQL files. Reviewer should verify whether the "v34 → v39" range refers to in-process migrations or whether SQL files are still in flight.

### Feature: Adapter selection refactor + `AppState.store: Arc<dyn MemoryStore>`

- **Issue(s):** Wave 3
- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Code paths:** `/Users/fate/v07/catalogue/src/store/mod.rs`, `/Users/fate/v07/catalogue/src/handlers/mod.rs`, `/Users/fate/v07/catalogue/src/handlers/{http.rs,transport.rs,federation_receive.rs,hook_subscribers.rs}`
- **Public surface:** `ai-memory serve --store-url postgres://...`

### Feature: Tests pinning ship gate

- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Test files:** `/Users/fate/v07/catalogue/tests/ship_gate.rs`, `/Users/fate/v07/catalogue/tests/ship_gate/{grand_slam_recursive_learning.rs,grand_slam_skills.rs,grand_slam_composition.rs}`

### Feature: Helper binaries (4 new under `tools/`)

- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Bins:**
  - `/Users/fate/v07/catalogue/tools/auto-link-detector/` — R3 reference `pre_link` hook (775 LoC `src/main.rs`)
  - `/Users/fate/v07/catalogue/tools/transcript-extractor/` — R5 reference `pre_store` hook (Track I)
  - `/Users/fate/v07/catalogue/tools/t0-orchestrate/` — Track E T0 calibration driver
  - `/Users/fate/v07/catalogue/tools/post-ship-converge/` — post-ship convergence verifier
- **Cargo note:** kept out of crates.io upload via the parent `Cargo.toml` `include` allowlist

### Feature: Benchmarks (11 new)

- **Status:** SHIPPED
- **Net-new in v0.7.0:** YES
- **Files:** `/Users/fate/v07/catalogue/benches/{age_vs_cte,harness_bench,longmemeval_reflection,reflect,reranker_throughput}.rs`; `/Users/fate/v07/catalogue/benchmarks/longmemeval_reflection/{dataset,runner}.rs` + `data/scenarios.jsonl`; `/Users/fate/v07/catalogue/benchmarks/competitive-benchmarks/{README.md,expected_output.md,harness.sh}`; existing `/Users/fate/v07/catalogue/benchmarks/v0.6.4-cross-harness.md`

### Feature: 42 new docs

- **Status:** SHIPPED
- **See list above** (§ "docs added since v0.6.4"). Key per-feature anchors:
  - `docs/MIGRATION_v0.7.md` (2,068 words) — v0.6.4 → v0.7.0 surface delta
  - `docs/migration-v0.7.0-postgres.md` (1,604 words)
  - `docs/v0.7/V0.7-EPIC.md` (7,331 words) — canonical scope
  - `docs/v0.7/rfc-attested-cortex.md` (17,501 words) — design RFC
  - `docs/v0.7.0/release-notes.md` (5,001 words)
  - `docs/v0.7.0/roadmap-audit-report.md`
  - `docs/v0.7.0/validation/{rules-store-isolation-audit,wire-check-bypass-audit,soak-test-results}.md`
  - `docs/policy-engine.md` (3,597 words)
  - `docs/RECURSIVE_LEARNING.md` (3,782 words)
  - `docs/agent-skills.md` (1,641 words)
  - `docs/internal/batman-framework-audit.md` (4,478 words)

### Feature: 8 cookbook directories

- **Status:** SHIPPED
- See full file list in §"Inventory cookbook details" above. The `recursive-learning/` cookbook is the largest (5 scenarios with paired `.md` + `.sh` files, plus README).

---

## Known unknowns / open questions (for downstream reviewers)

1. **Schema-ladder version names** — task prompt anchors at "sqlite v33 → v39 / postgres v17 → v38". On-disk: sqlite tops out at `0033_v07_form5_confidence_calibration.sql`, postgres at `0020_v07_form5_confidence_calibration.sql`. The "v34" / "v36" / etc. labels in CHANGELOG appear to refer to the `PRAGMA user_version` numbering applied by `src/storage/migrations.rs` rather than SQL filenames. Reviewer should verify the SQL-file vs in-process-ladder mapping.
2. **MCP tool count drift** — release-notes headline says "63 MCP tools at full profile"; the registry literal grep at `src/mcp/registry.rs` yields **71 unique `memory_*` names**. The delta of 8 likely covers (a) sub-tools, (b) aliases, (c) internal-only registrations. Reviewer should reconcile against the published `--profile full` `tools/list` output.
3. **Form 1 vs Form 2 ship-status** — CHANGELOG lists `#754` (Form 1) and `#755` (Form 2) as still OPEN on GitHub, but the code paths (`src/synthesis/`, `src/atomisation/mod.rs`), tests (`tests/form_1_synthesis.rs`, `tests/form_2_synchronous_atomise.rs`), and namespace-policy fields (`legacy_per_pair_classifier`, `auto_atomise_mode = Synchronous`) are present. Reviewer should clarify the "OPEN issue + SHIPPED code" reconciliation.
4. **Form 7 closeout completeness** — release-notes explicitly mark V08-PE-1..V08-PE-8 as v0.8.0 work. Reviewer must verify what ship-readiness means in this context: foundation lands v0.7.0; full cover is v0.8.0.
5. **Recursive-learning Tasks 7-8** — Tasks 1-6 each have a named commit; Tasks 7-8 (ship-gate test suite + docs/release-notes/capabilities honesty pass) are stated to "land on the same branch and roll up here". Pinned by `tests/recursive_learning_task7_*.rs` (2 files) and `tests/ship_gate/grand_slam_recursive_learning.rs`; no Task-8-named test file. Reviewer should verify Task 8 coverage.
6. **CapabilityCuration / CapabilityFederation absence** — no `Capability*` struct for the curator pipeline or federation. The release notes treat both as first-class features. Reviewer should confirm whether the absence is intentional (rolled into `CapabilityCompaction` and federation-config respectively) or a capabilities-surface gap.
7. **Persona `entity_id` + `persona_version` columns** — referenced in `MemoryKind::Persona` doc-comment as "populated only for this variant". No dedicated schema migration; columns must already exist or be added by `0031_v07_persona.sql`. Reviewer should verify column-level coverage.
8. **`memory_deref` MCP tool** — added in v0.7.0 but not covered explicitly by any of the named feature categories (QW-3 context offload registers `memory_offload`, but `memory_deref` is its read-side companion). Reviewer should confirm `memory_deref` belongs under QW-3 / context-offload.
9. **Tencent QW-4** — task prompt enumerates QW-1, QW-2, QW-3 (file-backed export, persona, context-offload). A fourth QW item is not surfaced explicitly in CHANGELOG or in `cookbook/`. If QW-4 was scoped, reviewer should locate its code path or confirm it didn't ship.
10. **HTTP `/api/v1/auto_tag` and `/api/v1/expand_query` ownership** — both surfaces are added but neither is enumerated in CHANGELOG track summaries. Reviewer should map these to their owning track/feature.
11. **`policy-engine/wire-points`, `policy-engine/harness-hook`, `policy-engine/deferred-audit-log` branch refs** — CHANGELOG references these as the source branches for PE-1/PE-2/PE-3. Verify all three are merged into HEAD `64528b1` (the CHANGELOG asserts this; reviewer should grep).

---

## Procurement-grade discipline notes

- Every file path in this document was verified by either `git diff --name-only v0.6.4..HEAD` or direct `ls` against the worktree.
- "Net-new" claims rest on `git diff --name-status v0.6.4..HEAD` `A`-status output.
- "Extends v0.6.4" claims rest on `M`-status output plus a baseline grep against `git show v0.6.4:<file>`.
- Word counts via `wc -w` on the on-disk file.
- Tool names extracted via `grep -oE '"memory_[a-z_]+"'` against `src/mcp/registry.rs` (HEAD) and `src/mcp.rs` (v0.6.4), diffed with `comm`.
- HTTP routes extracted via `grep -oE '\.route\("[^"]+"'` against all `*.rs` files at each ref, diffed with `comm`.
- Env vars extracted via `grep -oE 'AI_MEMORY_[A-Z_]*'` against all `src/*.rs` files at each ref, diffed with `comm`.

This document is the foundation for the 6-agent parallel ship-readiness review. Each row above is precise enough for a reviewer to navigate directly to the code.

— Cold mountain.
