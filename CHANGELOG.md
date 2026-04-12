# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.4] — 2026-04-12

### Added

- `memory_gc` MCP tool: on-demand garbage collection with `dry_run` support and archive-before-delete (#45)
- `memory_session_start` MCP tool: auto-recall recent memories on session start, LLM summary at smart/autonomous tiers (#46)
- `memory_archive_list` MCP tool: browse archived (GC'd) memories
- `memory_archive_purge` MCP tool: permanently delete archived memories with optional age filter
- `memory_archive_restore` MCP tool: restore archived memory back to active as long-term
- `memory_archive_stats` MCP tool: archive statistics (count, namespaces)
- Archive system: GC now archives expired memories before deletion, allowing recovery
- `gc_with_count()` atomic transactional GC (archive + delete in single transaction)
- Past `expires_at` in `memory_update`: allows programmatic TTL management and E2E GC testing
- 10 new unit tests for archive pipeline, GC, link validation, TOON escaping, UTF-8 validation

### Changed

- MCP tool count: 17 → 23 (6 new tools)
- Title/namespace validation: byte-length → character-count (CJK-safe) (#21, #43)
- FTS5 sanitizer: hyphens now allowed inside words for hyphenated term search (#23)
- HNSW mutex: poison recovery instead of panic on corrupted state (#6)
- `memory_promote`: atomic single-UPDATE instead of two-step (#4)
- MCP priority: validated before i64→i32 cast, prevents silent truncation (#32)
- CLI write commands: WAL checkpoint after all writes to prevent unbounded WAL growth (#28, #38)
- Auto-consolidate: safe UTF-8 truncation using `.chars().take()` instead of byte slicing (#41)
- `export_all`: now excludes expired memories (#19)
- `export_links`: now filters dangling refs to expired memories (#31)
- `create_link`: verifies both source and target IDs exist before creating (#5)
- FTS search: strips zero-width Unicode characters before querying (#13)
- TOON escaping: added `\r` (carriage return) and `\\` (backslash) handling (#44)
- Schema version: 3 → 4 (adds `memory_archive` table)

### Fixed

- RT-01 (#4): Promote non-atomic — crash between steps could cause data loss
- RT-02 (#5): create_link gave opaque error for nonexistent IDs
- RT-03 (#6): HNSW mutex poisoning caused panic instead of recovery
- RT-10 (#13): Zero-width Unicode characters bypassed FTS search
- RT-16 (#19): export_all included expired memories, import resurrected dead data
- RT-14 (#31): export_links exported dangling refs to expired memories
- RT-18 (#21): Title validation used byte length, rejecting CJK titles early
- RT-20 (#23): FTS5 sanitizer stripped hyphens, breaking hyphenated term search
- RT-25 (#28): CLI commands didn't checkpoint WAL, file grew between GC cycles
- RT-30 (#32): MCP priority `as i32` silent truncation on large values
- RT-43 (#38): Missing WAL checkpoint on multiple CLI write paths
- RT-51 (#41): Auto-consolidate byte truncation panicked on multi-byte UTF-8
- RT-69 (#43): Namespace validation used bytes while title used chars
- RT-73/74 (#44): TOON escaping missing `\r` and backslash handling

### Test Coverage

| Metric | Count |
|--------|-------|
| Unit tests | 134 |
| Integration tests | 43 |
| **Total** | **177** |
| Modules with tests | 15/15 |

## [0.5.2] — 2026-04-08

### Added

- Ubuntu PPA: `sudo add-apt-repository ppa:jbridger2021/ai-memory && sudo apt install ai-memory`
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

[0.5.4]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.5.2...v0.5.4
[0.5.2]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/alphaonedev/ai-memory-mcp/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/alphaonedev/ai-memory-mcp/releases/tag/v0.1.0
