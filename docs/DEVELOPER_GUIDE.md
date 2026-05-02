# Developer Guide

## Architecture Overview

`ai-memory` is an AI-agnostic memory management system built as a single Rust binary that serves three roles:

1. **MCP tool server** -- stdio JSON-RPC server exposing 43 memory tools + 2 MCP prompts for any MCP-compatible AI client (Claude AI, OpenAI ChatGPT, xAI Grok, META Llama, and others)
2. **CLI tool** -- direct SQLite operations for store, recall, search, list, etc. (completely AI-agnostic)
3. **HTTP daemon** -- an Axum web server exposing the same operations as a REST API with 50 endpoints (completely AI-agnostic)

**Key architectural features:** Zero token cost (no context loaded until recall), TOON compact default response format (79% smaller than JSON), MCP prompts capability (`recall-first` behavioral rules + `memory-workflow` reference card), 4 feature tiers with optional local LLMs via Ollama, true dedup on title+namespace, 6-factor recall scoring with score field in responses.

All three interfaces share the same database layer (`db.rs`) and validation layer (`validate.rs`). The daemon adds automatic garbage collection (every 30 minutes) and graceful shutdown with WAL checkpointing.

```
main.rs          -- CLI parsing (clap), daemon setup (axum), command dispatch (40 subcommands)
models.rs        -- Data structures: Memory, MemoryLink, query types, constants
handlers.rs      -- HTTP request handlers (Axum extractors + JSON responses), error sanitization
db.rs            -- All SQLite operations: CRUD, FTS5, recall scoring, GC, migration, FTS query sanitization, transactional touch/consolidate
mcp.rs           -- MCP (Model Context Protocol) server over stdio JSON-RPC, 43 tools, notification handling
validate.rs      -- Input validation for all write paths
errors.rs        -- Structured error types (ApiError, MemoryError), error sanitization for HTTP responses
color.rs         -- ANSI color output for CLI (zero dependencies, auto-detects terminal)
config.rs        -- Tier configuration system (keyword, semantic, smart, autonomous), feature gating, TtlConfig, and archive_on_gc
embeddings.rs    -- Embedding pipeline: HuggingFace model loading, vector generation, cosine similarity
llm.rs           -- LLM integration via Ollama for query expansion, auto-tagging, contradiction detection
mine.rs          -- Retroactive conversation import from Claude, ChatGPT, and Slack exports
reranker.rs      -- Hybrid recall algorithm: blends semantic (embedding) and keyword (FTS5) scores
hnsw.rs          -- In-memory HNSW vector index for approximate nearest-neighbor search
```

### Embedding Pipeline (semantic tier and above)

When running at the `semantic` tier or higher, ai-memory loads a HuggingFace embedding model at startup and generates dense vector embeddings for each memory. The pipeline:

1. **Model loading** (`embeddings.rs`) -- downloads and caches a sentence-transformer model from HuggingFace on first run
2. **Embedding generation** -- new memories are embedded at insert time; existing memories are backfilled on first startup with embeddings enabled
3. **Storage** -- embeddings are stored as BLOB columns in the `memories` table (schema migration v3)
4. **Hybrid recall** (`reranker.rs`) -- at recall time, the query is embedded and compared against stored embeddings via cosine similarity, then blended with FTS5 keyword scores to produce a final ranking

**Embedding models:**
- `all-MiniLM-L6-v2` (384 dimensions, ~90 MB) -- used at the `semantic` tier
- `nomic-embed-text-v1.5` (768 dimensions, ~270 MB) -- used at the `smart` and `autonomous` tiers

## Code Structure

### `src/main.rs`

- `Cli` struct with `clap` derive -- defines all CLI commands and global flags (`--db`, `--json`)
- `Command` enum -- `Serve`, `Mcp`, `Store`, `Update`, `Recall`, `Search`, `Get`, `List`, `Delete`, `Promote`, `Forget`, `Link`, `Consolidate`, `Resolve`, `Shell`, `Sync`, `SyncDaemon`, `AutoConsolidate`, `Gc`, `Stats`, `Namespaces`, `Export`, `Import`, `Completions`, `Man`, `Mine`, `Archive`, `Agents`, `Pending`, `Backup`, `Restore`, `Curator`, `Bench`, `Migrate` (gated `--features sal`), `Doctor`, `Boot`, `Install`, `Wrap`, `Logs`, `Audit` — 40 subcommands total in v0.6.3.1.
- `StoreArgs` includes `--expires-at` and `--ttl-secs` flags for custom expiration
- `UpdateArgs` includes `--expires-at` flag for setting expiration on existing memories
- `ListArgs` includes `--offset` flag for pagination
- `auto_namespace()` -- detects namespace from git remote URL or directory name
- `human_age()` -- formats ISO timestamps as "2h ago", "3d ago" for CLI output
- `serve()` -- starts the Axum server with all routes (50 endpoints including `POST /memories/{id}/promote`, the 4 archive endpoints, the namespace-standard endpoints, and the webhook subscription endpoints), spawns GC task, handles graceful shutdown via SIGINT with WAL checkpoint
- `cmd_*()` functions -- one per CLI command, each opens the DB directly

### `src/models.rs`

- `Tier` enum (`Short`, `Mid`, `Long`) with TTL defaults: 6h, 7d, none
- `Memory` struct -- the core data type with 15 fields (includes extensible `metadata` JSON column)
- `MemoryLink` struct -- typed directional links between memories
- Request types: `CreateMemory`, `UpdateMemory`, `SearchQuery`, `ListQuery`, `RecallQuery`, `RecallBody`, `LinkBody`, `ForgetQuery`, `ConsolidateBody`, `ImportBody`
- Response types: `Stats`, `TierCount`, `NamespaceCount`
- `TtlConfig` struct -- per-tier TTL overrides loaded from `config.toml` (`short_ttl_secs`, `mid_ttl_secs`, `long_ttl_secs`, `short_extend_secs`, `mid_extend_secs`)
- `ResolvedTtl` struct -- resolved TTL values after merging config defaults with per-tier overrides
- Constants: `MAX_CONTENT_SIZE` (65536), `PROMOTION_THRESHOLD` (5), `SHORT_TTL_EXTEND_SECS` (3600), `MID_TTL_EXTEND_SECS` (86400)

### `src/mcp.rs`

The MCP (Model Context Protocol) server implementation. MCP is an open standard -- this server works with any MCP-compatible AI client. Runs over stdio, processing one JSON-RPC message per line. Exposes **43 tools**.

- `RpcRequest` / `RpcResponse` / `RpcError` -- JSON-RPC 2.0 types
- `tool_definitions()` -- returns the 43 tool schemas for `tools/list` (includes `memory_capabilities`, `memory_expand_query`, `memory_auto_tag`, `memory_detect_contradiction`, `memory_archive_list`, `memory_archive_restore`, `memory_archive_purge`, `memory_archive_stats`)
  - `memory_recall` schema includes `until` parameter and `format` parameter (enum: `"json"`, `"toon"`, `"toon_compact"`, default: `"toon_compact"`)
  - `memory_search` schema includes `format` parameter (enum: `"json"`, `"toon"`, `"toon_compact"`, default: `"toon_compact"`) and enforces `maximum: 200` on limit
  - `memory_list` schema includes `format` parameter (enum: `"json"`, `"toon"`, `"toon_compact"`, default: `"toon_compact"`) and enforces `maximum: 200` on limit
  - `memory_consolidate` schema enforces `minItems: 2, maxItems: 100` on IDs
  - `memory_update` schema includes `expires_at` parameter
- `handle_store()`, `handle_recall()`, `handle_search()`, `handle_list()`, `handle_delete()`, `handle_promote()`, `handle_forget()`, `handle_stats()`, `handle_update()`, `handle_get()`, `handle_link()`, `handle_get_links()`, `handle_consolidate()`, `handle_archive_list()`, `handle_archive_restore()`, `handle_archive_purge()`, `handle_archive_stats()` -- one handler per tool
- `handle_request()` -- routes JSON-RPC methods: `initialize`, `notifications/initialized`, `tools/list`, `tools/call`, `ping`
- Notification handling: all JSON-RPC notifications (requests without an `id` field) are correctly skipped without sending a response, per the JSON-RPC 2.0 specification
- `run_mcp_server()` -- main loop: reads lines from stdin, parses JSON-RPC, dispatches, writes responses to stdout

Protocol version: `2024-11-05`. All tool responses are wrapped in MCP content blocks (`{"content": [{"type": "text", "text": "..."}]}`). The protocol is AI-agnostic -- any MCP client can connect.

**MCP Prompts:** The server exposes 2 prompts via `prompts/list`:
- **recall-first** -- System prompt with 8 behavioral rules for proactive memory use. Supports an optional `namespace` argument for scoped recall.
- **memory-workflow** -- Quick reference card for all 43 tool usage patterns.

**MCP Error Codes:** The server uses standard JSON-RPC 2.0 error codes:
- `-32700` -- Parse error (malformed JSON)
- `-32600` -- Invalid request (missing required fields)
- `-32601` -- Method not found (unknown JSON-RPC method)
- `-32602` -- Invalid params (bad tool arguments)
- Application-level errors are returned as text in the MCP content block with `"isError": true`, not as JSON-RPC error codes.

### `src/validate.rs`

Input validation for every write path. Called by CLI, HTTP handlers, and MCP handlers.

| Function | Validates |
|----------|-----------|
| `validate_title()` | Non-empty, max 512 bytes, no null bytes |
| `validate_content()` | Non-empty, max 64KB, no null bytes |
| `validate_namespace()` | Non-empty, max 128 bytes, no slashes/spaces/nulls |
| `validate_source()` | Must be one of: user, claude, hook, api, cli, import, consolidation, system |
| `validate_tags()` | Max 50 tags, each max 128 bytes, no empty strings |
| `validate_id()` | Non-empty, max 128 bytes, no null bytes |
| `validate_expires_at()` | Valid RFC3339, not in the past |
| `validate_ttl_secs()` | Positive, max 1 year |
| `validate_relation()` | Must be one of: related_to, supersedes, contradicts, derived_from |
| `validate_confidence()` | Finite number, 0.0 to 1.0 |
| `validate_priority()` | Integer, 1 to 10 |
| `validate_create()` | Full validation for CreateMemory |
| `validate_memory()` | Full validation for Memory (import) |
| `validate_update()` | Validates only present fields |
| `validate_link()` | Validates both IDs, relation, and rejects self-links |
| `validate_consolidate()` | 2-100 IDs, validates title, summary, namespace |

### `src/color.rs`

ANSI color output for CLI -- zero external dependencies. Auto-detects terminal via `std::io::IsTerminal`.

- `init()` -- sets global color flag based on terminal detection
- `short()`, `mid()`, `long()` -- tier-specific colors (red, yellow, green)
- `dim()`, `bold()`, `cyan()` -- semantic colors
- `tier_color()` -- dispatches to tier color by string name
- `priority_bar()` -- renders a 10-character bar (`█████░░░░░`) colored by priority level (green for 8+, yellow for 5-7, red for 1-4)

Colors are suppressed when stdout is not a terminal (e.g., piping to file). The `--json` flag bypasses color output entirely.

### `src/errors.rs`

Structured error types for the HTTP API:

- `ApiError` -- serializable error with `code` and `message` fields
- `MemoryError` enum -- `NotFound`, `ValidationFailed`, `DatabaseError`, `Conflict`
- Implements `IntoResponse` for Axum, mapping to appropriate HTTP status codes
- Implements `From<anyhow::Error>` and `From<rusqlite::Error>`
- **Error sanitization**: `DatabaseError` responses return a generic `"Internal server error"` message to clients, never leaking internal database error details. Detailed errors are logged server-side.

### `src/handlers.rs`

All HTTP handlers for the 24-endpoint REST API. State is `Arc<Mutex<(Connection, PathBuf)>>`. Each handler acquires the lock, validates input, performs DB operations, returns JSON.

Key handlers:
- `create_memory` / `bulk_create` -- memory creation with deduplication (bulk limited to 1,000 items)
- `get_memory` / `list_memories` / `update_memory` / `delete_memory` -- standard CRUD
- `promote_memory` -- `POST /memories/{id}/promote` endpoint for promoting to long-term
- `search` / `recall` -- FTS-powered search with sanitized queries
- `forget` / `consolidate` -- bulk operations
- `import_memories` -- import with 1,000 item limit
- `archive_list` / `archive_restore` / `archive_purge` / `archive_stats` -- archive management endpoints
- All ID path parameters are validated before database access

> **Note:** HTTP handlers are tested via integration tests (`tests/integration.rs`), not unit tests.

### `src/db.rs`

The database layer. Key functions:

| Function | Description |
|----------|-------------|
| `open()` | Opens DB, sets WAL mode, creates schema, runs migrations |
| `insert()` | Upsert on `(title, namespace)` -- never downgrades tier, keeps max priority |
| `get()` | Fetch by ID |
| `touch()` | Bump access count, extend TTL, auto-promote mid->long at 5 accesses, reinforce priority every 10 accesses. **Uses BEGIN IMMEDIATE/COMMIT transaction** for atomicity. |
| `update()` | Partial update of any fields |
| `delete()` | Delete by ID (links cascade) |
| `forget()` | Bulk delete by namespace + FTS pattern + tier |
| `list()` | List with filters: namespace, tier, priority, date range, tags, offset |
| `search()` | FTS5 AND search with 6-factor composite scoring |
| `recall()` | FTS5 OR search + touch + auto-promote + TTL extension |
| `find_contradictions()` | Find memories in same namespace with similar titles |
| `consolidate()` | Merge multiple memories, delete originals, aggregate tags and max priority. **Uses BEGIN IMMEDIATE/COMMIT transaction** for atomicity. |
| `sanitize_fts_query()` | Strips special characters and quotes tokens to prevent FTS injection |
| `create_link()` / `get_links()` / `delete_link()` | Memory linking (ON DELETE CASCADE) |
| `gc()` | Delete expired memories |
| `stats()` | Aggregate statistics (totals, by tier, by namespace, expiring soon, links, DB size) |
| `list_namespaces()` | List namespaces with memory counts |
| `export_all()` / `export_links()` | Full data export |
| `checkpoint()` | WAL checkpoint (TRUNCATE) for clean shutdown |
| `archive_memory()` | Move a memory to the archive table |
| `list_archived()` | List all archived memories |
| `restore_archived()` | Restore an archived memory to the active table |
| `purge_archive()` | Permanently delete all archived memories |
| `archive_stats()` | Archive statistics (count, size, date range) |
| `health_check()` | Verifies DB accessibility and FTS5 integrity |

**Transaction safety**: `touch()` and `consolidate()` use `BEGIN IMMEDIATE` to acquire a write lock upfront, preventing deadlocks and ensuring the entire read-modify-write cycle is atomic. This is critical for `touch()` because it reads the current access count, computes promotion/reinforcement logic, and writes back -- all of which must be atomic under concurrent access.

**FTS query sanitization**: The `sanitize_fts_query()` function strips all FTS5 special characters (`*`, `"`, `(`, `)`, `:`, `+`, `-`, `~`, `^`, `{`, `}`, `[`, `]`, `|`, `\`) from user input and wraps each remaining token in double quotes. This prevents injection of FTS query syntax that could cause unexpected results or errors.

**Migration error handling**: The migration logic only ignores "duplicate column" errors (indicating the migration already ran). All other errors are propagated, ensuring real failures are caught early.

### `src/hnsw.rs`

In-memory HNSW (Hierarchical Navigable Small World) vector index for approximate nearest-neighbor search. The `VectorIndex` struct provides `insert`, `search`, and `remove` operations on dense embeddings. When the index is small (below the HNSW threshold), it falls back to linear scan. The index has no persistence -- it is rebuilt from the database on startup. This keeps the on-disk format simple (embeddings stored as BLOBs in SQLite) while providing fast in-memory ANN search during runtime.

### `src/toon.rs`

TOON (Token-Oriented Object Notation) serializer. Converts JSON recall/search/list responses into the compact TOON wire format. The format spec is documented in the [TOON Format Specification](#toon-format-specification) section below. Public API: `memories_to_toon()`, `search_to_toon()`. Compact mode emits a 6-field projection (id/tier/title/namespace/score/created_at); full mode emits the complete record.

### `src/config.rs`

Tier configuration system + global runtime config. Parses `~/.config/ai-memory/config.toml`, applies environment-variable overrides (`AI_MEMORY_*`), validates tier capabilities (`keyword`, `semantic`, `smart`, `autonomous`), and emits the immutable `Config` consumed by every other module. Includes `TtlConfig` (per-tier TTL + extension windows), `archive_on_gc`, embedding-model selection, Ollama URL, and feature gating that disables higher-tier code paths when the configured tier doesn't permit them.

### `src/embeddings.rs`

Embedding pipeline for `semantic+` tiers. Loads HuggingFace sentence-transformer models (`all-MiniLM-L6-v2` 384-dim or `nomic-embed-text-v1.5` 768-dim) on first run via `hf-hub`, runs inference via Candle, generates dense vectors at insert time, and backfills missing embeddings on first startup. Vectors are stored as BLOBs in the `memories.embedding` column. Consumed by `reranker.rs` for hybrid recall and `hnsw.rs` for approximate nearest-neighbour indexing.

### `src/llm.rs`

LLM integration via Ollama for query expansion, auto-tagging, and contradiction detection. Implements `OllamaClient` (HTTP via `reqwest`) and supplies the production implementation of the `AutonomyLlm` trait (see `src/autonomy.rs`). Prompts are kept short and structured to minimize token cost; failures are non-fatal — the curator and autonomy passes log and continue.

### `src/mine.rs`

Retroactive conversation import — bulk-imports historical Claude / ChatGPT / Slack export files into ai-memory as backfilled memories. Each conversation becomes a single memory; metadata captures `source`, `agent_id`, and timestamps from the export. Used to seed memory before live capture is available.

### `src/reranker.rs`

Hybrid recall algorithm. Blends the FTS5 keyword score and the embedding cosine similarity into a single ranking, applying configurable weighting and a 6-factor scoring formula (recency, priority, access count, tier weight, content match, namespace match). Returns a score field in every recall response so callers can audit ranking decisions.

### `src/identity.rs`

Non-Human Identity (NHI) resolution for `agent_id`. Centralises the precedence chain across CLI, MCP, and HTTP entry points so `metadata.agent_id` is uniformly populated. Public API: `resolve_agent_id()` (CLI/MCP), `resolve_http_agent_id()` (HTTP body + `X-Agent-Id` header), `preserve_agent_id()` (round-trip), `process_discriminator()` (stable per-process identifier). Default-id formats: `ai:<client>@<hostname>:pid-<pid>` (MCP), `host:<hostname>:pid-<pid>-<uuid8>` (CLI), `anonymous:req-<uuid8>` (HTTP per-request fallback). `agent_id` is a *claimed* identity, not attested.

### `src/curator.rs`

Autonomous curator daemon (v0.6.1). Runs a periodic sweep over stored memories, invoking `auto_tag` and `detect_contradiction` via the configured LLM and persisting results into each memory's metadata. Complements the synchronous post-store hooks (#265). Hard cap on operations per cycle (default 100); skips internal `_`-prefixed namespaces; honours include/exclude lists; dry-run mode emits a report without touching rows; LLM errors are logged but never abort a cycle. Public API: `CuratorConfig`, `CuratorReport`, `run_once()`, `run_daemon()`.

### `src/autonomy.rs`

Full-autonomy loop — stacks on the curator daemon. Four passes beyond auto-tag:

1. **Consolidation** — find near-duplicate memories in the same namespace (Jaccard ≥ 0.55, max cluster size 8), LLM-summarise into a single canonical memory, archive originals.
2. **Forgetting of superseded memories** — when `metadata.confirmed_contradictions` is set, demote/forget the contradicted entry.
3. **Priority feedback** — nudge `priority` up for hot memories, down for cold ones (purely arithmetic, no LLM call).
4. **Rollback log + self-report** — every autonomous action lands in `_curator/rollback/<ts>` (reversible) and every cycle in `_curator/reports/<ts>`.

Defines the `AutonomyLlm` trait so the curator can be unit-tested without a live Ollama instance. Public API: `run_autonomy_passes()`, `persist_self_report()`, `reverse_rollback_entry()`, `RollbackEntry`, `AutonomyPassReport`.

### `src/replication.rs`

W-of-N quorum-write layer for the peer-mesh sync (v0.7 track C). Scaffolds the contract described in [`ADR-0001-quorum-replication.md`](ADR-0001-quorum-replication.md). The `QuorumWriter` sits ABOVE the existing sync-daemon — deployments without `--quorum-writes` keep the v0.6.0 one-way push behaviour byte-for-byte. Public API: `QuorumPolicy`, `QuorumWriter::commit`, `AckTracker`. Emits metrics: `replication_quorum_ack_total{result}`, `replication_quorum_failures_total{reason}`, `replication_clock_skew_seconds`.

### `src/federation.rs`

Federation autonomy — wires the quorum primitives from `replication` into the HTTP write path (v0.7 track C, PR 2 of N). When `ai-memory serve` is started with `--quorum-writes N --quorum-peers <urls>`, every successful HTTP write fans out a 1-memory `/api/v1/sync/push` POST to each peer; the write returns OK only once `W-1` peer acks land within `--quorum-timeout-ms`. Fewer acks → `503 quorum_not_met`. Public API: `FederationConfig`, `broadcast_store_quorum()`.

### `src/subscriptions.rs`

v0.6.0.0 webhook subscriptions. Subscribers register a URL + shared secret + event/namespace/agent filters; matching events POST an HMAC-SHA256-signed JSON payload (header `X-Ai-Memory-Signature: sha256=<hex>`) over a fire-and-forget thread. SSRF hardening: `http://` only to `127.0.0.0/8` or `localhost`; everywhere else requires `https://`; RFC1918 / RFC4193 / link-local hosts rejected unless `allow_private_networks=true`. Stored secret is SHA-256 of the plaintext (plaintext returned once at registration). Public API: `Subscription`, `NewSubscription`, `insert()`, `delete()`, `list()`, `dispatch_event()`, `validate_url()`.

### `src/migrate.rs`

Cross-backend migration tool — streams memories from one SAL backend to another (v0.7 track B, PR 2 of N). Gated behind `--features sal`; extended transparently by `--features sal-postgres`. Supported URLs: `sqlite:///abs/path.db`, `sqlite://./relative.db`, `postgres://user:pass@host:port/db`. CLI: `ai-memory migrate --from <url> --to <url> [--batch 1000] [--dry-run] [--namespace foo]`. Reads via `MemoryStore::list`, writes via `MemoryStore::store` with the source memory's id verbatim — adapter upsert-on-id semantics make repeated migration idempotent.

### `src/metrics.rs`

v0.6.0.0 Prometheus metrics, exposed at `GET /metrics` by the daemon. Minimal, non-invasive instrumentation — single global `Registry`, a handful of `IntCounter` / `IntCounterVec` / `IntGauge` / `HistogramVec` handles. Callers increment via typed helpers (`record_store(tier, ok)`, `record_recall(mode, latency_seconds)`, `record_autonomy_hook(kind, ok)`, `curator_cycle_completed(...)`) rather than poking the registry directly so a future metrics-backend swap stays internal. Public API: `Metrics` (struct), `registry()`, `render()`.

## Database Schema

### `memories` table

```sql
CREATE TABLE memories (
    id               TEXT PRIMARY KEY,
    tier             TEXT NOT NULL,           -- 'short', 'mid', 'long'
    namespace        TEXT NOT NULL DEFAULT 'global',
    title            TEXT NOT NULL,
    content          TEXT NOT NULL,
    tags             TEXT NOT NULL DEFAULT '[]',  -- JSON array
    priority         INTEGER NOT NULL DEFAULT 5,  -- 1-10
    confidence       REAL NOT NULL DEFAULT 1.0,   -- 0.0-1.0
    source           TEXT NOT NULL DEFAULT 'api', -- 'user', 'claude', 'hook', 'api', 'cli', etc.
    access_count     INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL,           -- ISO 8601 / RFC3339
    updated_at       TEXT NOT NULL,
    last_accessed_at TEXT,
    expires_at       TEXT,                    -- NULL for long-term
    embedding        BLOB                     -- dense vector (v3 migration, NULL if keyword tier)
);

-- Indexes
CREATE INDEX idx_memories_tier ON memories(tier);
CREATE INDEX idx_memories_namespace ON memories(namespace);
CREATE INDEX idx_memories_priority ON memories(priority DESC);
CREATE INDEX idx_memories_expires ON memories(expires_at);

-- Unique constraint enables upsert/deduplication behavior
CREATE UNIQUE INDEX idx_memories_title_ns ON memories(title, namespace);
```

### `memories_fts` virtual table

```sql
CREATE VIRTUAL TABLE memories_fts USING fts5(
    title, content, tags,
    content=memories, content_rowid=rowid
);
```

Kept in sync via `AFTER INSERT`, `AFTER DELETE`, and `AFTER UPDATE` triggers on `memories`.

### `memory_links` table

```sql
CREATE TABLE memory_links (
    source_id   TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id   TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation    TEXT NOT NULL DEFAULT 'related_to',
    created_at  TEXT NOT NULL,
    PRIMARY KEY (source_id, target_id, relation)
);
```

Relation types: `related_to`, `supersedes`, `contradicts`, `derived_from`.

### `archived_memories` table

```sql
CREATE TABLE archived_memories (
    id               TEXT PRIMARY KEY,
    tier             TEXT NOT NULL,
    namespace        TEXT NOT NULL DEFAULT 'global',
    title            TEXT NOT NULL,
    content          TEXT NOT NULL,
    tags             TEXT NOT NULL DEFAULT '[]',
    priority         INTEGER NOT NULL DEFAULT 5,
    confidence       REAL NOT NULL DEFAULT 1.0,
    source           TEXT NOT NULL DEFAULT 'api',
    access_count     INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    last_accessed_at TEXT,
    expires_at       TEXT,
    archived_at      TEXT NOT NULL,
    archive_reason   TEXT NOT NULL DEFAULT 'gc'
);

-- Indexes
CREATE INDEX idx_archived_memories_namespace ON archived_memories(namespace);
CREATE INDEX idx_archived_memories_archived_at ON archived_memories(archived_at);
```

Added in schema migration v3 -> v4. Stores memories archived by GC before deletion. The 16 columns mirror the `memories` table with two additions: `archived_at` (timestamp of archival) and `archive_reason` (why the memory was archived, e.g., `'gc'`).

### `schema_version` table

Tracks migration state. Current version: 4.

## Recall Scoring Formula

The recall function uses a 6-factor composite score to rank results:

```
score = (fts_rank * -1)                                              -- FTS5 relevance (negated: lower = better in SQLite)
      + (priority * 0.5)                                             -- Priority weight (1-10 -> 0.5-5.0)
      + (MIN(access_count, 50) * 0.1)                                         -- Frequency bonus
      + (confidence * 2.0)                                           -- Certainty weight (0.0-1.0 -> 0.0-2.0)
      + tier_boost                                                   -- long=3.0, mid=1.0, short=0.0
      + (1.0 / (1.0 + (julianday('now') - julianday(updated_at)) * 0.1))  -- Recency decay
```

The `search` function uses the same formula minus the tier boost.

### Hybrid Recall Algorithm (semantic tier and above)

At the `semantic` tier and above, the `reranker.rs` module blends two scoring signals:

1. **Semantic score** -- cosine similarity between the query embedding and each memory's stored embedding (0.0 to 1.0)
2. **Keyword score** -- the existing 6-factor FTS5 composite score, normalized to 0.0-1.0

The final score is a weighted blend: `final = (semantic_weight * semantic_score) + (keyword_weight * keyword_score)`. The default weights are 0.6 semantic / 0.4 keyword. Results from both pipelines are merged, deduplicated by memory ID, and sorted by the blended score.

### Tier Configuration System

The `config.rs` module defines 4 feature tiers that gate functionality:

| Tier | Embeddings | LLM | Tools Available |
|------|-----------|-----|-----------------|
| `keyword` | No | No | 13 base tools + `memory_capabilities` + 4 archive tools |
| `semantic` | Yes | No | 14 base tools + `memory_capabilities` + 4 archive tools |
| `smart` | Yes | Yes | Full 43-tool surface |
| `autonomous` | Yes | Yes | Full 43-tool surface + autonomous behaviors |

The tier is set at startup via `ai-memory mcp --tier <tier>` and cannot be changed at runtime. The `memory_capabilities` tool reports the active tier and which features are available, allowing AI clients to adapt their behavior.

> **Note:** Configuration is loaded once at process startup. Changes to `config.toml` require restarting the ai-memory process (MCP server, HTTP daemon, or CLI) to take effect.

The recency decay factor ensures that recent memories rank higher when other factors are similar. A memory updated today gets a boost of ~1.0, a memory from 10 days ago gets ~0.5, and a memory from 100 days ago gets ~0.09.

### TOON Format Specification

TOON (Token-Oriented Object Notation) is a token-efficient serialization format designed for LLM communication. It replaces JSON for recall, search, and list responses, reducing output size by 40-60% by declaring field names once as a header and listing values row by row with pipe delimiters.

The implementation is in `src/toon.rs`.

#### Structure Overview

A TOON response consists of three parts in order:

1. **Metadata line** (optional) -- key:value pairs for scalar fields
2. **Header line** -- declares field names once
3. **Data rows** -- one per object, values matching header column order

#### Metadata Line Syntax

Scalar (non-array) response fields are serialized as pipe-delimited `key:value` pairs on the first line:

```
count:3|mode:hybrid
```

If there are no metadata fields, this line is omitted entirely.

#### Header Line Syntax

The header declares the array name followed by field names in square brackets, pipe-delimited, ending with a colon:

```
memories[id|title|tier|namespace|priority|confidence|score|access_count|tags|source|created_at|updated_at]:
```

Field names appear exactly once in the entire output regardless of how many data rows follow. This is the primary source of token savings over JSON.

#### Data Row Syntax

Each data row contains values pipe-delimited in the same order as the header fields:

```
abc-123|PostgreSQL 16 config|long|infra|9|1.0|0.763|2|postgres,database|claude|2026-04-03T15:00:00+00:00|2026-04-03T15:00:00+00:00
```

- **Strings** are output as-is (unless they require escaping)
- **Numbers** (integers and floats) are output as their string representation
- **Booleans** are output as `1` (true) or `0` (false)
- **Arrays** (e.g., tags) are joined with commas: `postgres,database`
- **Objects** are output as the literal `[object]`
- **Null/missing values** are represented as an empty string (zero characters between the delimiters), e.g., `abc||mid` means the second field is null

#### Escaping Rules

Two characters require escaping in TOON values:

| Character | Escaped As | Reason |
|-----------|-----------|--------|
| `\|` (pipe) | `\\|` | Pipe is the field delimiter |
| `\n` (newline) | `\\n` | Newline is the row delimiter |

Escaping is only applied when the value actually contains a pipe or newline character. Values without these characters are output verbatim with no additional escaping.

Example: a title containing a pipe like `A|B` is serialized as `A\|B` in the data row.

#### Compact vs Full Mode

TOON supports two modes that differ only in which fields are included:

**Full mode** (12 fields):
```
memories[id|title|tier|namespace|priority|confidence|score|access_count|tags|source|created_at|updated_at]:
```

**Compact mode** (7 fields) -- omits timestamps, confidence, access_count, and source for tighter output:
```
memories[id|title|tier|namespace|priority|score|tags]:
```

The MCP server defaults to compact mode (`toon_compact`). Clients can request `"toon"` for full mode or `"json"` for standard JSON via the `format` parameter on recall, search, and list tools.

#### Search Response Normalization

Search responses use a `"results"` key instead of `"memories"`. The TOON serializer normalizes this internally -- the output always uses the `memories[...]` header regardless of the source key.

#### Complete Parsing Example

Given this JSON response:

```json
{
  "memories": [
    {"id": "abc-123", "title": "PostgreSQL config", "tier": "long", "namespace": "infra", "priority": 9, "score": 0.763, "tags": ["postgres", "db"]},
    {"id": "def-456", "title": "Redis cache", "tier": "long", "namespace": "infra", "priority": 8, "score": 0.541, "tags": ["redis"]},
    {"id": "ghi-789", "title": "Deploy notes", "tier": "mid", "namespace": "infra", "priority": 5, "score": 0.320, "tags": []}
  ],
  "count": 3,
  "mode": "hybrid"
}
```

TOON compact output:

```
count:3|mode:hybrid
memories[id|title|tier|namespace|priority|score|tags]:
abc-123|PostgreSQL config|long|infra|9|0.763|postgres,db
def-456|Redis cache|long|infra|8|0.541|redis
ghi-789|Deploy notes|mid|infra|5|0.32|
```

To parse TOON:

1. Read the first line. If it does not start with a bracket-containing identifier (e.g., `memories[`), parse it as metadata: split on `|`, then split each segment on `:` to get key-value pairs.
2. Read the header line. Extract the array name and field list: strip the trailing `:`, extract the portion inside `[...]`, and split on `|` to get the ordered field names.
3. Read each subsequent non-empty line as a data row. Split on `|` (respecting `\|` escapes), mapping each positional value to the corresponding header field name.
4. Unescape `\|` to `|` and `\n` to newline in each value. Empty values represent null/missing fields.

## API Reference

Base URL: `http://127.0.0.1:9077/api/v1`

All responses are JSON. Error responses include `{"error": "message"}`. Database errors are sanitized -- clients receive `"Internal server error"` instead of raw SQLite error details.

The HTTP API exposes **50 endpoints** in v0.6.3.1 (canonical count from `src/lib.rs:68-194` Router builder; v0.6.3 baseline of 42 is frozen on the [evidence page](https://alphaonedev.github.io/ai-memory-mcp/evidence.html)).

### Health Check

```
GET /health
```

Deep health check: verifies DB is readable and FTS5 integrity-check passes.

Response (200): `{"status": "ok", "service": "ai-memory"}`
Response (503): `{"status": "error", "service": "ai-memory"}`

### Create Memory

```
POST /memories
Content-Type: application/json

{
  "title": "Project uses Axum",
  "content": "The HTTP server is built with Axum 0.8.",
  "tier": "mid",
  "namespace": "ai-memory",
  "tags": ["rust", "web"],
  "priority": 6,
  "confidence": 1.0,
  "source": "api",
  "expires_at": "2026-04-06T00:00:00Z",
  "ttl_secs": 86400
}
```

Response (201):
```json
{
  "id": "a1b2c3d4-...",
  "tier": "mid",
  "namespace": "ai-memory",
  "title": "Project uses Axum",
  "potential_contradictions": ["id1", "id2"]
}
```

Defaults: `tier=mid`, `namespace=global`, `priority=5`, `confidence=1.0`, `source=api`.

Optional: `expires_at` (RFC3339), `ttl_secs` (overrides tier default). Deduplicates on title+namespace (upsert).

### Bulk Create

```
POST /memories/bulk
Content-Type: application/json

[
  {"title": "Memory 1", "content": "..."},
  {"title": "Memory 2", "content": "..."}
]
```

Response: `{"created": 2, "errors": []}`

Limited to **1,000 items per request**.

### Get Memory

```
GET /memories/{id}
```

Response:
```json
{
  "memory": { ... },
  "links": [ ... ]
}
```

### Update Memory

```
PUT /memories/{id}
Content-Type: application/json

{
  "content": "Updated content",
  "priority": 8,
  "expires_at": "2026-06-01T00:00:00Z"
}
```

All fields are optional. Only provided fields are updated. Validated before write.

### Delete Memory

```
DELETE /memories/{id}
```

Response: `{"deleted": true}`. Links are cascade-deleted.

### Promote Memory

```
POST /memories/{id}/promote
```

Promotes a memory to long-term tier and clears its expiry.

Response: `{"promoted": true}`

### List Memories

```
GET /memories?namespace=my-app&tier=long&limit=20&offset=0&min_priority=5&since=2026-01-01T00:00:00Z&until=2026-12-31T23:59:59Z&tags=rust
```

All query parameters are optional. Max limit is 200.

Response: `{"memories": [...], "count": 5}`

### Search (AND semantics)

```
GET /search?q=database+migration&namespace=my-app&tier=mid&limit=10&since=...&until=...&tags=...
```

Response: `{"results": [...], "count": 3, "query": "database migration"}`

Uses 6-factor scoring (without tier boost). Queries are sanitized to prevent FTS injection.

### Recall (OR semantics + touch)

```
GET /recall?context=auth+flow+jwt&namespace=my-app&limit=10&tags=auth&since=2026-01-01T00:00:00Z&until=2026-12-31T23:59:59Z
```

Or via POST:

```
POST /recall
Content-Type: application/json

{"context": "auth flow jwt", "namespace": "my-app", "limit": 10}
```

Response: `{"memories": [...], "count": 5}`

Recall automatically: bumps `access_count`, extends TTL, and auto-promotes mid-tier memories with 5+ accesses to long-term. The touch operation is transactional.

### Forget (Bulk Delete)

```
POST /forget
Content-Type: application/json

{"namespace": "my-app", "pattern": "deprecated API", "tier": "short"}
```

At least one field is required. Pattern uses FTS matching (sanitized). Response: `{"deleted": 3}`

### Consolidate

```
POST /consolidate
Content-Type: application/json

{
  "ids": ["id1", "id2", "id3"],
  "title": "Auth system summary",
  "summary": "JWT with refresh tokens, RBAC middleware, Redis sessions.",
  "namespace": "my-app",
  "tier": "long"
}
```

Requires 2-100 IDs. Deletes source memories, creates new with aggregated tags and max priority. The entire operation is transactional. Response (201): `{"id": "new-id", "consolidated": 3}`

### Create Link

```
POST /links
Content-Type: application/json

{"source_id": "id1", "target_id": "id2", "relation": "related_to"}
```

Relations: `related_to`, `supersedes`, `contradicts`, `derived_from`. Self-links rejected. Response (201): `{"linked": true}`

### Get Links

```
GET /links/{id}
```

Response: `{"links": [{"source_id": "...", "target_id": "...", "relation": "...", "created_at": "..."}]}`

### Namespaces

```
GET /namespaces
```

Response: `{"namespaces": [{"namespace": "my-app", "count": 42}]}`

### Stats

```
GET /stats
```

Response:
```json
{
  "total": 150,
  "by_tier": [{"tier": "long", "count": 80}, ...],
  "by_namespace": [{"namespace": "my-app", "count": 42}, ...],
  "expiring_soon": 5,
  "links_count": 12,
  "db_size_bytes": 524288
}
```

### Garbage Collection

```
POST /gc
```

Response: `{"expired_deleted": 3}`

### Export

```
GET /export
```

Response: full JSON dump of all memories and links with `exported_at` timestamp.

### Import

```
POST /import
Content-Type: application/json

{"memories": [...], "links": [...]}
```

Validates each memory before import. Limited to **1,000 memories per request**. Response: `{"imported": 50, "errors": []}`

## Error Code Reference

Structured error codes returned by the HTTP API and MCP server:

| Code | HTTP Status | Description |
|------|-------------|-------------|
| `NOT_FOUND` | 404 | Memory or resource not found |
| `VALIDATION_FAILED` | 400 | Invalid input parameters |
| `DATABASE_ERROR` | 500 | SQLite or internal error |
| `CONFLICT` | 409 | Duplicate or conflicting operation |

Error responses are JSON: `{"code": "NOT_FOUND", "message": "Memory not found"}`. `DATABASE_ERROR` responses are sanitized -- clients receive a generic `"Internal server error"` message; detailed errors are logged server-side only.

## CLI Reference

Global flags:
- `--db <path>` -- database path (default: `ai-memory.db`, env: `AI_MEMORY_DB`)
- `--json` -- output as machine-parseable JSON

### `serve`

Start the HTTP daemon (50 endpoints).

```bash
ai-memory serve --host 127.0.0.1 --port 9077
```

### `mcp`

Run as an MCP tool server over stdio. This is the primary integration path for any MCP-compatible AI client. Exposes 43 tools.

```bash
ai-memory mcp
ai-memory mcp --tier semantic   # default
ai-memory mcp --tier smart      # enables LLM-powered tools (requires Ollama)
```

Reads JSON-RPC from stdin, writes responses to stdout. Logs to stderr. Correctly handles notifications (no response sent). Works with any MCP-compatible client (Claude AI, OpenAI ChatGPT, xAI Grok, META Llama, etc.).

### `store`

```bash
ai-memory store \
  -T "Title" \
  -c "Content" \
  --tier mid \
  --namespace my-app \
  --tags "tag1,tag2" \
  --priority 7 \
  --confidence 0.9 \
  --source claude \
  --expires-at "2026-04-15T00:00:00Z" \
  --ttl-secs 86400
```

Use `-c -` to read content from stdin. Validates all fields before writing. `--expires-at` sets an explicit expiration timestamp (RFC3339). `--ttl-secs` sets a TTL in seconds (overrides tier default).

### `update`

```bash
ai-memory update <id> -T "New title" -c "New content" --priority 8 --expires-at "2026-06-01T00:00:00Z"
```

The `--expires-at` flag sets or changes the expiration on an existing memory.

### `recall`

```bash
ai-memory recall "search context" --namespace my-app --limit 10 --tags auth --since 2026-01-01T00:00:00Z
```

### `search`

```bash
ai-memory search "exact terms" --namespace my-app --tier long --limit 20 --since 2026-01-01 --until 2026-12-31 --tags rust
```

### `get`

```bash
ai-memory get <id>
```

Shows the memory plus all its links.

### `list`

```bash
ai-memory list --namespace my-app --tier mid --limit 50 --offset 0 --since 2026-01-01 --until 2026-12-31 --tags devops
```

The `--offset` flag enables pagination. Use with `--limit` to page through results.

### `delete`

```bash
ai-memory delete <id>
```

### `promote`

```bash
ai-memory promote <id>
```

Promotes to long-term and clears expiry.

### `forget`

```bash
ai-memory forget --namespace my-app --pattern "old stuff" --tier short
```

At least one filter is required.

### `link`

```bash
ai-memory link <source-id> <target-id> --relation supersedes
```

Relation types: `related_to` (default), `supersedes`, `contradicts`, `derived_from`. Self-links rejected.

### `consolidate`

```bash
ai-memory consolidate "id1,id2,id3" -T "Summary title" -s "Consolidated content" --namespace my-app
```

### `gc`

```bash
ai-memory gc
```

### `stats`

```bash
ai-memory stats
```

### `namespaces`

```bash
ai-memory namespaces
```

### `export` / `import`

```bash
ai-memory export > backup.json
ai-memory import < backup.json
```

Export includes memories and links. Import validates each memory and skips invalid ones.

### `resolve`

Resolve a contradiction by marking one memory as superseding another.

```bash
ai-memory resolve <winner_id> <loser_id>
```

Creates a "supersedes" link from winner to loser. Demotes the loser (priority=1, confidence=0.1). Touches the winner (bumps access count).

### `shell`

Interactive REPL for browsing and managing memories.

```bash
ai-memory shell
```

REPL commands: `recall <ctx>`, `search <q>`, `list [ns]`, `get <id>`, `stats`, `namespaces`, `delete <id>`, `help`, `quit`. Color output with tier labels and priority bars.

### `sync`

Sync memories between two database files.

```bash
ai-memory sync <remote.db> --direction pull|push|merge
```

- `pull` -- import all memories from remote into local
- `push` -- export all local memories to remote
- `merge` -- bidirectional sync (both databases get all memories)

Uses dedup-safe upsert (title+namespace). Links are synced alongside memories.

### `auto-consolidate`

Automatically group and consolidate memories.

```bash
ai-memory auto-consolidate [--namespace <ns>] [--short-only] [--min-count 3] [--dry-run]
```

Groups memories by namespace+primary tag. Groups with >= min_count members are consolidated into one long-term memory. Use `--dry-run` to preview.

### `mine`

Import memories from historical conversations (Claude, ChatGPT, Slack exports).

```bash
ai-memory mine --format claude <path-to-export>
ai-memory mine --format chatgpt <path-to-export>
ai-memory mine --format slack <path-to-export>
```

Takes `--format` to specify the input file format (`claude`, `chatgpt`, `slack`) and a path to the export file or directory.

### `man`

Generate roff man page to stdout.

```bash
ai-memory man           # print roff to stdout
ai-memory man | man -l -  # view immediately
```

### `completions`

```bash
ai-memory completions bash
ai-memory completions zsh
ai-memory completions fish
```

## Adding New Features

1. **Add the model** in `models.rs` -- new struct or new fields on existing structs
2. **Add validation** in `validate.rs` -- new validation function
3. **Add the DB function** in `db.rs` -- SQL operations
4. **Add the HTTP handler** in `handlers.rs` -- Axum handler function
5. **Add the route** in `main.rs` inside the `Router::new()` chain
6. **Add the CLI command** in `main.rs` -- new variant in `Command` enum, new `Args` struct, new `cmd_*()` function
7. **Add the MCP tool** in `mcp.rs` -- tool definition in `tool_definitions()`, handler function, route in `handle_request()`
8. **Add tests** in `tests/integration.rs`

## Testing

The project has **1,886 lib tests + 49+ integration tests at 93.84% line coverage** as of v0.6.3.1 (was 1,600 lib / 93.08% on v0.6.3). v0.6.3 baseline numbers are frozen on the [evidence page](https://alphaonedev.github.io/ai-memory-mcp/evidence.html); v0.6.3.1 deltas are documented in the release notes. Modules each carry their own unit-test suite; integration tests live under `tests/`.

```bash
# Run all tests
cargo test

# Run with output
cargo test -- --nocapture

# Run a specific test
cargo test test_name

# Check formatting
cargo fmt --check

# Run clippy
cargo clippy -- -D warnings
```

Integration tests run through the CLI binary, creating temporary databases for isolation.

## Benchmarks

### Criterion (microbenchmarks)

Criterion benchmarks are in `benches/recall.rs`. They test insert, recall, and search performance at 1,000 memories scale.

```bash
cargo bench
# recall/short_query, recall/medium_query, recall/long_query
# search/simple_search, search/filtered_search
# insert/store_memory
```

### LongMemEval (end-to-end accuracy)

The `benchmarks/longmemeval/` directory evaluates recall accuracy against the [LongMemEval](https://github.com/xiaowu0162/LongMemEval) dataset (ICLR 2025). Four harnesses are available:

| Harness | Strategy | R@5 | Speed |
|---------|----------|-----|-------|
| `harness_99.py --no-expand` | Parallel FTS5, 10 cores | **97.0%** | 232 q/s (2.2s) |
| `harness_99.py` | LLM expansion + parallel FTS5 | **97.8%** | 142 q/s (3.5s) |
| `harness_fast.py` | Single-process native SQLite | 96.2% | 57 q/s (8.8s) |
| `harness.py` | CLI subprocess per operation | 96.2% | 1.2 q/s (414s) |

Best result: **97.8% R@5 (489/500), 99.0% R@10, 99.8% R@20** -- 499/500 at R@20.

```bash
# Quick run (keyword, ~2s)
python3 benchmarks/longmemeval/harness_99.py \
  --dataset-path /tmp/LongMemEval --variant S --no-expand --workers 10

# Full run with LLM expansion (requires Ollama + gemma3:4b)
python3 benchmarks/longmemeval/harness_99.py \
  --dataset-path /tmp/LongMemEval --variant S --workers 10
```

See `benchmarks/longmemeval/README.md` for full replication instructions.

## CI/CD Pipeline

GitHub Actions CI runs on every push to `main` and every pull request:

1. **Check formatting** -- `cargo fmt --check`
2. **Clippy** -- `cargo clippy -- -D warnings`
3. **Run tests** -- `cargo test`
4. **Build release** -- `cargo build --release`

Runs on both `ubuntu-latest` and `macos-latest`.

### Release Pipeline

On tag push (e.g., `v0.2.0`):

1. Builds release binaries for `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin`
2. Packages as `.tar.gz`
3. Creates a GitHub Release with the artifacts

## Building from Source

```bash
git clone https://github.com/alphaonedev/ai-memory-mcp.git
cd ai-memory

# Debug build
cargo build

# Release build (optimized, stripped)
cargo build --release

# The binary is at target/release/ai-memory
```

### New Dependencies (v0.4.0)

- `candle-core`, `candle-nn`, `candle-transformers` -- HuggingFace Candle for local embedding model inference
- `hf-hub` -- HuggingFace Hub client for downloading embedding models
- `tokenizers` -- HuggingFace tokenizers for text preprocessing
- `reqwest` -- HTTP client for Ollama API communication (LLM inference)

All dependencies are always compiled; tier selection controls which features are activated at runtime.

Release profile settings (from `Cargo.toml`):
- `opt-level = 3`
- `strip = true` (removes debug symbols)
- `lto = "thin"` (link-time optimization)

---

## Working Under an Autonomous Campaign

When this repository is being driven by the `campaign` Python harness
(at `alphaonedev/agentic-mem-labs/tools/campaign/`, Apache 2.0 ©
AlphaOne LLC), the development workflow is the same workflow described
above plus the constraints in
[`ENGINEERING_STANDARDS.md` §7](ENGINEERING_STANDARDS.md#7-autonomous-campaign-workflow).

### Concurrent operation

- A live campaign holds a designated `release/vX.Y.Z` branch as its
  exclusive merge target. Human contributors can still open PRs against
  `develop` (or pre-existing release branches) without conflict.
- The campaign records every decision and PR to its ai-memory namespace
  (named after the campaign, e.g. `campaign-v063`). To see what the
  agent has done in the current iteration window:

      ai-memory --db ~/.claude/ai-memory.db list --namespace campaign-v063

- The append-only audit trail lives on a `campaign-log/vX.Y.Z` branch
  of `agentic-mem-labs`. One markdown report per iteration:

      git -C ~/agentic-mem-labs-log log --oneline campaign-log/v0.6.3

### Memory namespace as the campaign's operating substrate

Every campaign uses an ai-memory namespace named after the campaign.
The namespace contains: the campaign's overview/scope/hard rules,
approvals, code-quality standards, Engineering Standards alignment, a
snapshot of open issues + PRs at campaign start, one summary memory per
iteration, decisions, blockers, and "future"/deferred items.

Treat the namespace as both the agent's working memory and the
historical record. After a campaign ends, the namespace is preserved
indefinitely (`tier = long`).

### What human reviewers should focus on under a campaign

PRs from `campaign/<slug>` branches into `release/vX.Y.Z` get
`gh pr merge --squash --delete-branch` once CI is green. The agent
self-reviews quality (clippy pedantic, fmt, tests). For human
spot-checks: charter alignment, hard-rule compliance, test coverage,
audit consistency on the `campaign-log/vX.Y.Z` branch.

The campaign is a *complement* to human development, not a replacement.
For everything outside the active charter — bug triage, design ADRs,
release cuts, dependency upgrades, security response — humans still
own the work.
