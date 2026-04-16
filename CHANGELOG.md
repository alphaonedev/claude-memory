# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.0-alpha.1] — 2026-04-16 — Phase 1 Track A complete

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

- Ubuntu PPA: `sudo add-apt-repository ppa:jbridger2021/ppa && sudo apt install ai-memory`
- Fedora COPR: `sudo dnf copr enable alpha-one-ai/ai-memory && sudo dnf install ai-memory`
- CI workflows for automated PPA and COPR uploads on tag push
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
