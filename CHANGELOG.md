# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
