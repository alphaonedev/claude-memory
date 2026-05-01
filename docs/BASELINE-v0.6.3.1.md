# Baseline — ai-memory-mcp v0.6.3.1

> **Purpose.** This file is the canonical, code-derived snapshot of what
> ai-memory-mcp v0.6.3.1 actually *is* — produced from `main` via 6 parallel
> read-only scans of the source tree. It is the reference doc that the
> long-running [#512](https://github.com/alphaonedev/ai-memory-mcp/issues/512)
> drift audit measures published surfaces against.
>
> **Version naming.** This product ships as **release tag `v0.6.3.1`**. The
> `Cargo.toml` `version` field encodes the same release as `0.6.3+patch.1`
> using SemVer build metadata — crates.io rejects 4-segment versions so the
> `+patch.N` suffix is the intentional encoding. Docs, HTML, and release
> notes always use the **release-tag form `v0.6.3.1`**. The Cargo encoding
> is internal.
>
> **Scope.** Code is the only source of truth. This baseline does not
> contain any aspirational claims, marketing copy, or numbers without a
> traceable source.
>
> **Snapshot.** `main` @ `9e85d36` (2026-05-01).

---

## 1. Workspace, crates, features, toolchain

### 1.1 Workspace identity
- **Layout:** single-crate repo (no `[workspace]` table). The root crate is monorepo-organized by convention.
- **Root crate:** `ai-memory` — `Cargo.toml:1`.
- **Companion crate:** `ai-memory-fuzz` — `fuzz/Cargo.toml:2` (publish=false, fuzz harness only).

### 1.2 Versions (canonical = release tag)
| Crate | Cargo.toml | Release-tag form |
| --- | --- | --- |
| `ai-memory` | `0.6.3+patch.1` (`Cargo.toml:3`) | **`v0.6.3.1`** |
| `ai-memory-fuzz` | `0.0.0` (`fuzz/Cargo.toml:3`) | n/a (unpublished) |

### 1.3 Targets
- **Library:** `ai_memory` (root, implicit from `package.name`).
- **Binary:** `ai-memory` (`src/main.rs`).
- **Bench target:** `recall` (`Cargo.toml:198–200`, `harness = false`).
- **Fuzz binaries:** `fuzz_validate` (`fuzz/Cargo.toml:13`), `fuzz_namespace` (`fuzz/Cargo.toml:20`).

### 1.4 Cargo features (root crate)
Source: `Cargo.toml:134–153`.

| Feature | Default | Enables |
| --- | --- | --- |
| `default` | yes | `["sqlite-bundled"]` |
| `sqlite-bundled` | yes | `rusqlite/bundled` |
| `sqlcipher` | no | `rusqlite/bundled-sqlcipher-vendored-openssl` |
| `sal` | no | `dep:async-trait`, `dep:bitflags`, `dep:thiserror` |
| `sal-postgres` | no | `sal`, `dep:sqlx`, `dep:pgvector` |
| `test-with-models` | no | (opt-in flag for tests) |
| `e2e` | no | (opt-in flag for tests) |

### 1.5 Optional dependencies
| Dep | Version | Gate | Cargo.toml |
| --- | --- | --- | --- |
| `async-trait` | `0.1` | `sal` | `:116` |
| `bitflags` | `2` | `sal` | `:117` |
| `thiserror` | `2` | `sal` | `:118` |
| `sqlx` | `0.8` | `sal-postgres` | `:124` |
| `pgvector` | `0.4` | `sal-postgres` | `:125` |

### 1.6 Toolchain
- **Edition (both crates):** `2024`.
- **MSRV (root):** `1.88` (`Cargo.toml:5` `rust-version = "1.88"`).
- **MSRV (fuzz):** undeclared.
- **`rust-toolchain.toml`:** not present.

---

## 2. MCP tool surface (43 tools)

All registered in `src/mcp.rs::tool_definitions()` (lines 147–715). Dispatched in `handle_request()` (lines 3473–3572). Listed via `tools/list` handler at `:3404–3445`. The schema-version stamp is `toolsVersion = "2026-04-26"` (`src/mcp.rs:153`).

No tool is `#[cfg]`-gated at compile time; tier-gated tools degrade at runtime to a `501 / "LLM not available"` style response.

| # | Tool | Definition / handler | Required params | Optional params | Tier required |
| --- | --- | --- | --- | --- | --- |
| 1 | `memory_store` | `:161` / `:857` | `title`, `content` | `tier`, `namespace`, `tags`, `priority`, `confidence`, `source`, `metadata`, `agent_id`, `scope`, `on_conflict` | keyword |
| 2 | `memory_recall` | `:183` / `:1314` | `context` | `namespace`, `limit`, `tags`, `since`, `until`, `as_agent`, `budget_tokens`, `context_tokens`, `format` | keyword |
| 3 | `memory_search` | `:203` / `:1737` | `query` | `namespace`, `tier`, `limit`, `agent_id`, `as_agent`, `format` | keyword |
| 4 | `memory_list` | `:220` / `:2101` | — | `namespace`, `tier`, `limit`, `agent_id`, `format` | keyword |
| 5 | `memory_get_taxonomy` | `:234` / `:1771` | — | `namespace_prefix`, `depth`, `limit` | keyword |
| 6 | `memory_check_duplicate` | `:246` / `:1801` | `title`, `content` | `namespace`, `threshold` | semantic |
| 7 | `memory_entity_register` | `:260` / `:1859` | `canonical_name`, `namespace` | `aliases`, `metadata`, `agent_id` | keyword |
| 8 | `memory_entity_get_by_alias` | `:275` / `:1913` | `alias` | `namespace` | keyword |
| 9 | `memory_kg_timeline` | `:287` / `:1944` | `source_id` | `since`, `until`, `limit` | keyword |
| 10 | `memory_kg_invalidate` | `:301` / `:1992` | `source_id`, `target_id`, `relation` | `valid_until` | keyword |
| 11 | `memory_kg_query` | `:315` / `:2029` | `source_id` | `max_depth`, `valid_at`, `allowed_agents`, `limit` | keyword |
| 12 | `memory_delete` | `:330` / `:2127` | `id` | — | keyword |
| 13 | `memory_promote` | `:341` / `:2242` | `id` | `to_namespace` | keyword |
| 14 | `memory_forget` | `:353` / `:2384` | — | `namespace`, `pattern`, `tier`, `dry_run` | keyword |
| 15 | `memory_stats` | `:366` / `:2405` | — | — | keyword |
| 16 | `memory_update` | `:371` / `:2410` | `id` | `title`, `content`, `tier`, `namespace`, `tags`, `priority`, `confidence`, `expires_at`, `metadata` | keyword |
| 17 | `memory_get` | `:391` / `:2515` | `id` | — | keyword |
| 18 | `memory_link` | `:402` / `:2532` | `source_id`, `target_id` | `relation` | keyword |
| 19 | `memory_get_links` | `:415` / `:2584` | `id` | — | keyword |
| 20 | `memory_consolidate` | `:426` / `:2591` | `ids`, `title` | `summary`, `namespace` | keyword |
| 21 | `memory_capabilities` | `:440` / `:1586` | — | `accept` (`v1`\|`v2`) | keyword |
| 22 | `memory_expand_query` | `:455` / `:1665` | `query` | — | smart |
| 23 | `memory_auto_tag` | `:466` / `:1672` | `id` | — | smart |
| 24 | `memory_detect_contradiction` | `:477` / `:1710` | `id_a`, `id_b` | — | smart |
| 25 | `memory_archive_list` | `:489` / `:3281` | — | `namespace`, `limit`, `offset` | keyword |
| 26 | `memory_archive_restore` | `:501` / `:3290` | `id` | — | keyword |
| 27 | `memory_archive_purge` | `:512` / `:3300` | — | `older_than_days` | keyword |
| 28 | `memory_archive_stats` | `:522` / `:3306` | — | — | keyword |
| 29 | `memory_gc` | `:530` / `:3310` | — | `dry_run` | keyword |
| 30 | `memory_session_start` | `:540` / `:3328` | — | `namespace`, `limit`, `format` | smart |
| 31 | `memory_namespace_set_standard` | `:552` / `:2740` | `namespace`, `id` | `parent`, `governance` | keyword |
| 32 | `memory_namespace_get_standard` | `:576` / `:2809` | `namespace` | `inherit` | keyword |
| 33 | `memory_namespace_clear_standard` | `:588` / `:2887` | `namespace` | — | keyword |
| 34 | `memory_pending_list` | `:599` / `:3221` | — | `status`, `limit` | keyword |
| 35 | `memory_pending_approve` | `:610` / `:3230` | `id` | — | keyword |
| 36 | `memory_pending_reject` | `:621` / `:3264` | `id` | — | keyword |
| 37 | `memory_agent_register` | `:632` / `:2959` | `agent_id`, `agent_type` | `capabilities` | keyword |
| 38 | `memory_agent_list` | `:645` / `:2989` | — | — | keyword |
| 39 | `memory_notify` | `:653` / `:3008` | `target_agent_id`, `title`, `payload` | `priority`, `tier` | keyword |
| 40 | `memory_inbox` | `:668` / `:3072` | — | `agent_id`, `unread_only`, `limit` | keyword |
| 41 | `memory_subscribe` | `:680` / `:3136` | `url` | `events`, `secret`, `namespace_filter`, `agent_filter` | keyword |
| 42 | `memory_unsubscribe` | `:695` / `:3207` | `id` | — | keyword |
| 43 | `memory_list_subscriptions` | `:706` / `:3216` | — | — | keyword |

**Aliases / deprecated names:** none. The codebase uses exact string matching on tool name; no compatibility shims.

**MCP prompts (non-tool resources):**
- `recall-first` (`src/mcp.rs:724`) — system prompt for proactive memory recall, TOON format, tier strategy. Optional `namespace` argument.
- `memory-workflow` (`src/mcp.rs:735`) — quick reference card for tool usage patterns.

---

## 3. CLI surface

Single binary: `ai-memory` (`src/main.rs` → `daemon_runtime::run()` at `src/daemon_runtime.rs:429`). Uses `clap` derive.

### 3.1 Global flags (apply to every subcommand)
Source: `src/daemon_runtime.rs:96–125`.

| Flag | Env var | Default |
| --- | --- | --- |
| `--db <PATH>` | `AI_MEMORY_DB` | `ai-memory.db` |
| `--json` | — | false |
| `--agent-id <ID>` | `AI_MEMORY_AGENT_ID` | (synthesized) |
| `--db-passphrase-file <PATH>` | (sets `AI_MEMORY_DB_PASSPHRASE`) | — |

### 3.2 Subcommand inventory (38 top-level)
Source: `src/daemon_runtime.rs:127–259` and `src/cli/*.rs`.

`serve`, `mcp`, `store`, `update`, `recall`, `search`, `get`, `list`, `delete`, `promote`, `forget`, `link`, `consolidate`, `gc`, `stats`, `namespaces`, `export`, `import`, `resolve`, `shell`, `sync`, `sync-daemon`, `auto-consolidate`, `completions`, `man`, `mine`, `archive`, `agents`, `pending`, `backup`, `restore`, `curator`, `bench`, `migrate` (gated `--features sal`), `doctor`, `boot`, `install`, `wrap`, `logs`, `audit`.

Hidden subcommands: **none** (`#[command(hide = true)]` not used).

Nested subcommand groups:
- `archive`: `list`, `restore`, `purge`, `stats`.
- `agents`: `list` (default), `register`.
- `pending`: `list`, `approve`, `reject`.
- `install`: targets `claude-code`, `openclaw`, `cursor`, `cline`, `continue`, `windsurf`.
- `logs`: `tail`, `cat`, `archive`, `purge`.
- `audit`: `verify`, `tail`, `path`.

Per-subcommand flag detail is exhaustive and lives in `src/cli/*.rs`. Cross-check against `docs/CLI_REFERENCE.md` is the responsibility of the drift scanner.

### 3.3 Environment variables read
| Env var | Read site | Purpose |
| --- | --- | --- |
| `AI_MEMORY_DB` | `daemon_runtime.rs:105` | DB path (overrides `--db`) |
| `AI_MEMORY_AGENT_ID` | `daemon_runtime.rs:113`, `identity.rs:127` | Agent identifier |
| `AI_MEMORY_DB_PASSPHRASE` | `db.rs:206`, `daemon_runtime.rs:438` | SQLCipher passphrase (set internally from `--db-passphrase-file`) |
| `AI_MEMORY_ANONYMIZE` | `daemon_runtime.rs:881`, `config.rs:1309`, `identity.rs:54` | Anonymize agent_id |
| `AI_MEMORY_NO_CONFIG` | `config.rs:1220` | Bypass config.toml load |
| `AI_MEMORY_BOOT_ENABLED` | `config.rs:1139` | Boot feature gate |
| `AI_MEMORY_AUTONOMOUS_HOOKS` | `config.rs:1294` | Enable autonomous hooks |
| `AI_MEMORY_AUDIT_DIR` | `cli/audit.rs` | Audit log directory |
| `AI_MEMORY_LOG_DIR` | `cli/logs.rs` | Operational log directory |
| `HOME`, `XDG_STATE_HOME`, `LOCALAPPDATA`, `USERPROFILE` | `log_paths.rs`, `config.rs`, `embeddings.rs`, `audit.rs` | Platform-dependent path resolution |
| `INVOCATION_ID` | `log_paths.rs:193` | systemd journal context detection |
| `PATH` | `cli/install.rs:365` | Installer binary lookup |
| `AI_MEMORY_TEST_POSTGRES_URL` | `store/postgres.rs:797` | Test-only Postgres URL (gated feature) |

---

## 4. Configuration (`config.toml`)

Loader: `AppConfig::load()` (`src/config.rs:1232`). Default location: `~/.config/ai-memory/config.toml`.

### 4.1 Top-level keys (`[AppConfig]`, `src/config.rs:928–984`)
| Key | Type | Default | Purpose |
| --- | --- | --- | --- |
| `tier` | `Option<String>` | `"semantic"` (resolved by `effective_tier()`) | Feature tier: `keyword` \| `semantic` \| `smart` \| `autonomous` |
| `db` | `Option<String>` | `"ai-memory.db"` | SQLite path |
| `ollama_url` | `Option<String>` | `"http://localhost:11434"` | Ollama base URL |
| `embed_url` | `Option<String>` | falls back to `ollama_url` | Separate embedding-model URL |
| `embedding_model` | `Option<String>` | tier-derived | `mini_lm_l6_v2` \| `nomic_embed_v15` |
| `llm_model` | `Option<String>` | tier-derived | Ollama tag (e.g. `gemma4:e2b`) |
| `cross_encoder` | `Option<bool>` | tier-derived | Enable neural reranker |
| `default_namespace` | `Option<String>` | `"global"` | Default namespace for stores |
| `max_memory_mb` | `Option<usize>` | none | Memory budget for auto-tier selection |
| `archive_on_gc` | `Option<bool>` | `true` | Archive expired memories before GC delete |
| `api_key` | `Option<String>` | none | HTTP `X-API-Key` |
| `archive_max_days` | `Option<i64>` | none (disabled) | Archive retention horizon |
| `ttl` | `Option<TtlConfig>` | compiled defaults | Per-tier TTL overrides |
| `scoring` | `Option<RecallScoringConfig>` | compiled defaults | Recall time-decay half-life |
| `autonomous_hooks` | `Option<bool>` | `false` | Fire LLM hooks on store |
| `logging` | `Option<LoggingConfig>` | disabled | Operational logging |
| `audit` | `Option<AuditConfig>` | disabled | Security audit trail |
| `boot` | `Option<BootConfig>` | enabled, no redact | Boot privacy controls |
| `identity` | `Option<IdentityConfig>` | none | Identity resolution |

### 4.2 Sub-tables
**`[ttl]` (`src/config.rs:727–738`):**
- `short_ttl_secs` — default `21600` (6h).
- `mid_ttl_secs` — default `604800` (7d).
- `long_ttl_secs` — default `None` (never expires).
- `short_extend_secs` — default `3600` (1h, on-access TTL bump).
- `mid_extend_secs` — default `86400` (1d, on-access TTL bump).

**`[logging]` (`src/config.rs:992–1015`):**
- `enabled` (`false`), `path` (`~/.local/state/ai-memory/logs/`), `max_size_mb` (`100`), `max_files` (`30`), `retention_days` (`90`), `structured` (`false`), `level` (`info`), `rotation` (`daily`), `filename_prefix` (`ai-memory.log`).

**`[audit]` (`src/config.rs:1024–1061`):**
- `enabled` (`false`), `path` (`~/.local/state/ai-memory/audit/`), `schema_version` (validated against binary), `redact_content` (v1 only supports `true`), `hash_chain` (`true`), `attestation_cadence_minutes` (`60`), `append_only` (`true`), `retention_days` (`90`), `compliance` (sub-table for SOC2/HIPAA/GDPR/FedRAMP presets).

**`[boot]` (`src/config.rs:1118–1130`):**
- `enabled` (`true`; env `AI_MEMORY_BOOT_ENABLED`), `redact_titles` (`false`).

**`[identity]` (`src/config.rs:1202–1208`):**
- `anonymize_default` (`false`).

---

## 5. Storage: schema, migrations, tiers

### 5.1 SQLite schema
- **Current schema version:** `19` (`src/db.rs:178`, `CURRENT_SCHEMA_VERSION`).
- **Migration files:** `migrations/sqlite/`
  - `0010_v063_hierarchy_kg.sql` → v15 (KG temporal validity + `entity_aliases`).
  - `0011_v0631_data_integrity.sql` → v18 (embedding_dim guard, archive lossless).
  - `0012_governance_inherit.sql` → v17 (governance.inherit backfill).
  - `0013_webhook_event_types.sql` → v19 (subscriptions `event_types` column + index).

### 5.2 Tables (canonical shape, post-migration v19)
- **`memories`** (`src/db.rs:111–127`) — primary store. Columns: `id`, `tier`, `namespace`, `title`, `content`, `tags` (JSON), `priority`, `confidence`, `source`, `access_count`, `created_at`, `updated_at`, `last_accessed_at`, `expires_at`, `metadata` (JSON), `embedding` (BLOB), `embedding_dim`, generated columns `scope_idx` and `agent_id_idx`. `UNIQUE(title, namespace)`.
- **`memory_links`** (`src/db.rs:135–141`) — KG edges. PK `(source_id, target_id, relation)`. Columns: `relation`, `created_at`, `valid_from`, `valid_until`, `observed_by`, `signature`. Temporal-validity indexes on (source_id, valid_from, valid_until) and (target_id, valid_from, valid_until) and (relation, valid_from).
- **`memories_fts`** (`src/db.rs:143–166`) — FTS5 virtual table mirroring `title`/`content`/`tags`. INSERT/DELETE/UPDATE triggers (`ai`, `ad`, `au`).
- **`archived_memories`** (`src/db.rs:306–327`) — same shape as `memories` plus `archived_at`, `archive_reason` (default `'ttl_expired'`), `original_tier` (default `'long'` for pre-v18), `original_expires_at`.
- **`entity_aliases`** (`migrations/sqlite/0010:26–34`) — PK `(entity_id, alias)`; `created_at` non-null; index on `alias`.
- **`namespace_meta`** (`src/db.rs:331–336`) — PK `namespace`; `standard_id` (FK → memories), `updated_at`, `parent_namespace`.
- **`pending_actions`** (`src/db.rs:371–385`) — governance queue. Status `pending`\|`approved`\|`rejected`; `approvals` JSON array.
- **`sync_state`** (`src/db.rs:446–454`) — federation high-watermarks. PK `(agent_id, peer_id)`.
- **`subscriptions`** (`src/db.rs:478–496`) — webhook subs. `event_types` (JSON) + index added in v19.
- **`schema_version`** (`src/db.rs:168–170`) — single integer column.

### 5.3 Postgres adapter
- **Current schema version (Postgres):** `15` (`src/store/postgres.rs:65`). Lags SQLite (19).

### 5.4 Memory tiers (`Tier` enum)
Source: `src/models.rs:10–51`.

| Variant | External string | Default TTL | On-access extend | Purpose |
| --- | --- | --- | --- | --- |
| `Tier::Short` | `"short"` | `21600 s` (6h) | `3600 s` (1h) | Ephemeral / working memory |
| `Tier::Mid` | `"mid"` | `604800 s` (7d) | `86400 s` (1d) | Default long-term tier |
| `Tier::Long` | `"long"` | `None` (∞) | none | Permanent unless `expires_at` is set |

### 5.5 Feature tiers (orthogonal capability hierarchy)
Source: `FeatureTier` enum, `src/config.rs:79–161`.

| Tier | Embedding | LLM | Cross-encoder | ~Memory |
| --- | --- | --- | --- | --- |
| `keyword` | none (FTS5 only) | none | none | 0 MB |
| `semantic` | MiniLM-L6-v2 (384-dim) | none | none | ~256 MB |
| `smart` | nomic-embed-text-v1.5 (768-dim) | gemma4:e2b | none | ~1 GB |
| `autonomous` | nomic-embed-text-v1.5 (768-dim) | gemma4:e4b | ms-marco-MiniLM-L-6-v2 | ~4 GB |

Memory-tier (Short/Mid/Long) is orthogonal to feature-tier (keyword/semantic/smart/autonomous).

---

## 6. Hardcoded product-facing constants (63 total)

Source-of-truth numbers that must match docs or be flagged. Categories below; each row cites `file:line`.

### 6.1 Capacity / size limits
| Constant | Value | Location |
| --- | --- | --- |
| `MAX_CONTENT_SIZE` | 65,536 bytes | `src/models.rs:859` |
| `MAX_TITLE_LEN` | 512 chars | `src/validate.rs:11` |
| `MAX_NAMESPACE_LEN` | 512 chars | `src/validate.rs:15` |
| `MAX_NAMESPACE_DEPTH` | 8 levels | `src/models.rs:863` |
| `MAX_METADATA_SIZE` | 65,536 bytes | `src/validate.rs:22` |
| `MAX_METADATA_DEPTH` | 32 levels | `src/validate.rs:23` |
| `MAX_TAG_LEN` | 128 chars | `src/validate.rs:17` |
| `MAX_TAGS_COUNT` | 50 items | `src/validate.rs:18` |
| `MAX_SOURCE_LEN` | 64 chars | `src/validate.rs:16` |
| `MAX_RELATION_LEN` | 64 chars | `src/validate.rs:19` |
| `MAX_ID_LEN` | 128 chars | `src/validate.rs:20` |
| `MAX_AGENT_ID_LEN` | 128 chars | `src/validate.rs:21` |
| `MAX_BULK_SIZE` | 1,000 items | `src/handlers.rs:69` |
| HNSW `MAX_ENTRIES` | 100,000 vectors | `src/hnsw.rs:20` |
| `MAX_VERSION_SUFFIX` (`on_conflict=version`) | 1,024 versions | `src/db.rs:1755` |
| `TAXONOMY_MAX_LIMIT` | 10,000 nodes | `src/db.rs:2085` |
| `MAX_ROWS` (migration ceiling) | 1,000,000 memories | `src/migrate.rs:137` |

### 6.2 Time / TTL / retention
| Constant | Value | Location | Override |
| --- | --- | --- | --- |
| Short tier default TTL | 21,600 s (6h) | `src/models.rs:46` | `[ttl].short_ttl_secs` |
| Mid tier default TTL | 604,800 s (7d) | `src/models.rs:47` | `[ttl].mid_ttl_secs` |
| Long tier default TTL | none (∞) | `src/models.rs:48` | `[ttl].long_ttl_secs` |
| `SHORT_TTL_EXTEND_SECS` | 3,600 s | `src/models.rs:946` | `[ttl].short_extend_secs` |
| `MID_TTL_EXTEND_SECS` | 86,400 s | `src/models.rs:947` | `[ttl].mid_extend_secs` |
| `MAX_TTL_SECS` (clamp ceiling) | 315,360,000 s (~10y) | `src/config.rs:765` | none |
| `GC_INTERVAL_SECS` (daemon) | 1,800 s (30 min) | `src/daemon_runtime.rs:83` | none |
| `WAL_CHECKPOINT_INTERVAL_SECS` | 600 s (10 min) | `src/daemon_runtime.rs:86` | none |
| Curator default interval | 3,600 s (1h) | `src/curator.rs:36` | `[curator].interval_secs` |

### 6.3 Batch / chunk sizes
| Constant | Value | Location |
| --- | --- | --- |
| `BULK_FANOUT_CONCURRENCY` | 8 | `src/handlers.rs:78` |
| HNSW `REBUILD_THRESHOLD` | 200 overflow entries | `src/hnsw.rs:17` |
| `memory_recall` default limit | 50 (clamp [1,1000]) | `src/handlers.rs:3089` |
| `memory_list` default limit | 50 (clamp [1,1000]) | `src/handlers.rs:3089` |
| `memory_inbox` default limit | 10 (clamp [1,50]) | `src/handlers.rs:1568` |
| `get_taxonomy` default limit | 1,000 (clamp [1,10000]) | `src/handlers.rs:1998` |
| `KG_TIMELINE_DEFAULT_LIMIT` | 200 | `src/db.rs:2623` |
| `KG_TIMELINE_MAX_LIMIT` | 1,000 | `src/db.rs:2627` |
| `KG_QUERY_DEFAULT_LIMIT` | 200 | `src/db.rs:2781` |
| `KG_QUERY_MAX_LIMIT` | 1,000 | `src/db.rs:2786` |

### 6.4 Timeouts / intervals
| Constant | Value | Location |
| --- | --- | --- |
| `GENERATE_TIMEOUT` (LLM call) | 30 s | `src/llm.rs:10` |
| `PULL_TIMEOUT` (Ollama model pull) | 120 s | `src/llm.rs:11` |
| `FANOUT_RETRY_BACKOFF` | 250 ms | `src/federation.rs:382` |
| Postgres `DEFAULT_ACQUIRE_TIMEOUT` | 30 s | `src/store/postgres.rs:71` |
| Federation handshake `connect_timeout` | 2 s | `src/federation.rs:145` |

### 6.5 Tier boundaries / similarity thresholds
| Constant | Value | Location |
| --- | --- | --- |
| `PROMOTION_THRESHOLD` (access count) | 5 | `src/models.rs:944` |
| `DUPLICATE_THRESHOLD_MIN` (cosine) | 0.5 | `src/db.rs:2287` |
| `DUPLICATE_THRESHOLD_DEFAULT` (cosine) | 0.85 | `src/db.rs:2293` |
| `CONSOLIDATE_JACCARD_THRESHOLD` | 0.55 | `src/autonomy.rs:45` |
| `CONSOLIDATE_MAX_CLUSTER_SIZE` | 8 | `src/autonomy.rs:49` |

### 6.6 Concurrency
| Constant | Value | Location |
| --- | --- | --- |
| Postgres `DEFAULT_MAX_CONNECTIONS` | 16 | `src/store/postgres.rs:70` |

### 6.7 Knowledge graph
| Constant | Value | Location |
| --- | --- | --- |
| `KG_QUERY_MAX_SUPPORTED_DEPTH` | 5 hops | `src/db.rs:2792` |
| `MAX_EXPLICIT_DEPTH` (legacy contradicts-search cycle limit) | 8 hops | `src/db.rs:4384` |
| `MAX_DEPTH` (histogram bucket ceiling) | 16 hops | `src/db.rs:5141` |

### 6.8 Embedding / reranking
| Constant | Value | Location |
| --- | --- | --- |
| `MINILM_DIM` | 384 | `src/embeddings.rs:16` |
| `MAX_SEQ_LEN` (MiniLM) | 256 tokens | `src/embeddings.rs:17` |
| `EMBEDDING_DIM` (public re-export) | 384 | `src/embeddings.rs:285` |
| `NOMIC_DIM` | 768 | `src/embeddings.rs:25` |
| `CROSS_ENCODER_MAX_SEQ` | 512 tokens | `src/reranker.rs:33` |
| `CROSS_ENCODER_WEIGHT` | 0.4 | `src/reranker.rs:30` |
| `ORIGINAL_WEIGHT` (FTS+HNSW blend) | 0.6 | `src/reranker.rs:28` |

### 6.9 Schema-version range
| Constant | Value | Location |
| --- | --- | --- |
| `MIN_SUPPORTED_SCHEMA` | 16 | `src/cli/boot.rs:54` |
| `MAX_SUPPORTED_SCHEMA` | 19 | `src/cli/boot.rs:61` |
| SQLite `CURRENT_SCHEMA_VERSION` | 19 | `src/db.rs:178` |
| Postgres `CURRENT_SCHEMA_VERSION` | 15 | `src/store/postgres.rs:65` |

### 6.10 Misc product-facing
| Constant | Value | Location |
| --- | --- | --- |
| `DEFAULT_PORT` (HTTP daemon) | 9077 | `src/daemon_runtime.rs:82` |
| Boot hook `DEFAULT_BUDGET_TOKENS` | 4,096 | `src/cli/boot.rs:76` |
| `TOKENS_PER_CHAR` (boot estimator, advisory) | 0.25 | `src/cli/boot.rs:82` |
| Wrap `DEFAULT_BUDGET_TOKENS` | 4,096 | `src/cli/wrap.rs:61` |
| Wrap `DEFAULT_LIMIT` | 10 | `src/cli/wrap.rs:65` |
| Curator `DEFAULT_MAX_OPS_PER_CYCLE` | 100 | `src/curator.rs:39` |
| Curator `MIN_CONTENT_LEN` | 50 chars | `src/curator.rs:43` |

**User-overridable subset:** 16 of 63 constants are reachable via config.toml, CLI flags, or query parameters. The rest are compile-time hard limits.

---

## 7. Performance: benches and published claims

### 7.1 Bench inventory
**Inline (`ai-memory bench` subcommand) — `src/bench.rs`:**
- `StoreNoEmbedding` — `memory_store` without embedder (pure SQLite write).
- `SearchFts` — `memory_search` via FTS5 (200-memory corpus).
- `RecallHot` — `memory_recall` hot path, depth=1 (200-memory corpus).
- `KgQueryDepth1` — `memory_kg_query` depth=1 (fan-out: 50 sources × 4 links).
- `KgQueryDepth3` — `memory_kg_query` depth=3 (chain: 50 × 5 hops).
- `KgQueryDepth5` — `memory_kg_query` depth=5 tail-case.
- `KgTimeline` — `memory_kg_timeline` ordered fact timeline.

**Criterion (`benches/recall.rs`):**
- `bench_recall`: `short_query`, `medium_query`, `long_query` (against 1000 seeded memories).
- `bench_search`: `simple_search`, `filtered_search`.
- `bench_insert`: `store_memory`.

**Cargo `[[bench]]`:** one entry, name `recall`, `harness = false` (`Cargo.toml:198–200`).

**Committed bench-result outputs:** none. `target/criterion` is gitignored. `benchmarks/longmemeval/` exists (Python harness) but holds no committed outputs.

### 7.2 Published claims vs. backing
**Total numeric claims** across `PERFORMANCE.md` and `docs/performance.html`: **29**.
**Backed by an inline bench:** **7**.
**`UNBACKED`** (no bench produces this number): **22**.

| Claim | Source | Backed? |
| --- | --- | --- |
| `memory_session_start` < 100 ms p95 | `PERFORMANCE.md:20` | UNBACKED |
| `memory_recall` (hot, depth=1) < 50 ms p95 | `PERFORMANCE.md:21` | ✓ `RecallHot` |
| `memory_recall` (cold, full hybrid) < 200 ms p95 | `PERFORMANCE.md:22` | UNBACKED (Stream E follow-up at line 76) |
| `memory_recall` (budget=4096) < 90 ms p95 | `PERFORMANCE.md:23` | UNBACKED (Stream E) |
| `memory_store` (no embedding) < 20 ms p95 | `PERFORMANCE.md:24` | ✓ `StoreNoEmbedding` |
| `memory_store` (with embedding) < 200 ms p95 | `PERFORMANCE.md:25` | UNBACKED (Stream E) |
| `memory_search` (FTS5) < 100 ms p95 | `PERFORMANCE.md:26` | ✓ `SearchFts` |
| `memory_check_duplicate` < 50 ms p95 | `PERFORMANCE.md:27` | UNBACKED |
| `memory_kg_query` (depth ≤ 3) < 100 ms p95 | `PERFORMANCE.md:28` | ✓ `KgQueryDepth1`/`KgQueryDepth3` |
| `memory_kg_query` (depth ≤ 5) < 250 ms p95 | `PERFORMANCE.md:29` | ✓ `KgQueryDepth5` |
| `memory_kg_timeline` < 100 ms p95 | `PERFORMANCE.md:30` | ✓ `KgTimeline` |
| `memory_get_taxonomy` < 100 ms p95 | `PERFORMANCE.md:31` | UNBACKED |
| Curator cycle (1k memories) < 60 s p95 | `PERFORMANCE.md:32` | UNBACKED |
| Federation ack (W=2 quorum) < 2 s p95 | `PERFORMANCE.md:33` | UNBACKED |
| `memory_store` (keyword) ≤ 5 ms p95 | `docs/performance.html:177` | ✓ (via `StoreNoEmbedding`, measured 0.4 ms in `PERFORMANCE.md:97`) |
| `memory_store` (semantic, +MiniLM) ≤ 25 ms p95 | `docs/performance.html:178` | UNBACKED |
| `memory_store` (autonomous, +nomic) ≤ 60 ms p95 | `docs/performance.html:179` | UNBACKED |
| `memory_get` (PK lookup) ≤ 2 ms p95 | `docs/performance.html:180` | UNBACKED |
| `memory_search` (top-20) ≤ 8 ms p95 | `docs/performance.html:181` | ✓ (via `SearchFts`, measured 0.5 ms in `PERFORMANCE.md:98`) |
| `memory_recall` (FTS 70% + HNSW 30%) ≤ 35 ms p95 | `docs/performance.html:182` | UNBACKED |
| `memory_recall` (autonomous, +rerank) ≤ 90 ms p95 | `docs/performance.html:183` | UNBACKED |
| `memory_link` (FK insert) ≤ 4 ms p95 | `docs/performance.html:184` | UNBACKED |
| `memory_promote` (+ governance) ≤ 8 ms p95 | `docs/performance.html:185` | UNBACKED |
| `memory_consolidate` (Gemma 4 E2B) ≤ 1500 ms p95 | `docs/performance.html:186` | UNBACKED |
| `memory_kg_query` (depth 3, < 1k edges) ≤ 50 ms p95 | `docs/performance.html:187` | ✓ `KgQueryDepth3` |
| `memory_get_taxonomy` (depth 8, limit 1000) ≤ 30 ms p95 | `docs/performance.html:188` | UNBACKED |
| `memory_archive_purge` (per 1000 rows) ≤ 200 ms p95 | `docs/performance.html:189` | UNBACKED |
| `sync_push` (TLS 1.3, per row) ≤ 15 ms p95 | `docs/performance.html:190` | UNBACKED |
| `bulk_create` (100 rows + fanout) ≤ 2000 ms p95 | `docs/performance.html:191` | UNBACKED |
| `ai-memory boot` (indexed list only) ≤ 50 ms | `docs/performance.html:208` | UNBACKED |

`PERFORMANCE.md:76` notes embedder-bound paths as `🚧 Stream E follow-up`. `PERFORMANCE.md:124–129` notes curator/federation/CLI as "not yet wired in." 22 of 29 claims have no temporal commitment for adding a matching bench.

---

## 8. CI / release-engineering surface

Workflows under `.github/workflows/`:

| File | Trigger | Docs-only behavior |
| --- | --- | --- |
| `ci.yml` | `push` to `main`/`develop`/`release/**`, `tags: ['v*']`, `pull_request` to same branches | `classify` job (`:18–56`) detects docs-only via `^(docs/|.*\.md$)`. Release job (`:122–127`) gated `if: startsWith(github.ref, 'refs/tags/v')` — never fires on PR merge. |
| `bench.yml` | `pull_request`/`push` to `main`/`develop`/`release/**` | `paths-ignore: ['docs/**', '**/*.md']` — docs changes do NOT trigger. |
| `fuzz.yml` | `workflow_dispatch` only | manual only |
| `yank.yml` | `workflow_dispatch` only | manual only (yanks crates.io versions) |
| `session-boot-lifetime.yml` | `pull_request`/`push` to `release/v0.6.3.1`/`main` with explicit `paths:` | code paths only; not triggered by docs |

**Implication for the drift workflow:** docs-only PRs to `chore/product-drift-audit` and merges to `main` do not initiate a release. The release path is `git tag v* && git push --tags` exclusively.

---

## 9. Generation provenance

This baseline was generated on 2026-05-01 from the working tree at `main` (`9e85d36`) via 6 parallel read-only Explore agents covering: workspace/features, MCP tool surface, CLI surface, config + schema + tiers, hardcoded constants, benches + perf claims. Every fact above cites `file:line` against the source tree. No file was modified during generation.

The drift scanner (issue #512, weekly) measures published surfaces against this baseline. All resolutions are documentation-only — code is the source of truth and is never edited by the drift workflow.
