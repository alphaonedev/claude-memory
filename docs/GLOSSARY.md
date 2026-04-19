# ai-memory Glossary

Every concept in the system, with a single-paragraph definition and a
pointer to authoritative documentation.

## Agent

A caller of ai-memory. Identified by `agent_id`. Can be a human, an
AI model (e.g. `ai:claude-opus-4.7`), or an automated system. Each
stored memory carries `metadata.agent_id` for attribution. See
[Agent Identity (NHI)](#agent-identity-nhi).

## Agent Identity (NHI)

Non-Human Identity marker stamped on every memory. A **claimed**
identity (not attested). Follows the regex
`^[A-Za-z0-9_\-:@./]{1,128}$`. Resolution precedence: explicit flag →
env var → MCP client info → process-stable host fallback →
anonymous. Once stored, immutable across update / upsert / import /
sync / consolidate. See `CLAUDE.md` § "Agent Identity (NHI)".

## Archive

Soft-deletion area. When a memory is deleted or garbage-collected
while `archive_on_gc=true` (the default), it moves here instead of
hard deletion. Can be listed, restored, or purged via `ai-memory
archive`. Preserves operator history; lets you recover from
mis-curation.

## Attested identity (Layer 2b, v0.7)

An `agent_id` that is **proven** — extracted from the peer's mTLS
certificate (CN or SAN URI) rather than self-declared in a request
body. Set via `--attest-mode reject|warn|off` on the HTTP daemon.
Primitives shipped in #285; wiring into `sync_push` is a v0.7.1
follow-up.

## Autonomy hooks (v0.6.0.0)

Synchronous post-store LLM invocations — `auto_tag` +
`detect_contradiction` — that fire on every successful `memory_store`
when `AI_MEMORY_AUTONOMOUS_HOOKS=1`. Results persist into
`metadata.auto_tags` and `metadata.confirmed_contradictions`. Off by
default because they add ~1–5 s of Ollama latency.

## Backup / restore

Hot-backup-safe snapshot via SQLite `VACUUM INTO` plus a sha256
manifest. `ai-memory backup --to <dir> --keep N` for retention;
`ai-memory restore --from <path>` with manifest verification.

## Chaos harness

`packaging/chaos/run-chaos.sh` — runs 200 cycles per fault class
against a 3-node Postgres-backed deployment to measure convergence
bound. Four fault classes: `kill_primary_mid_write`,
`partition_minority`, `drop_random_acks`, `clock_skew_peer`. Outputs
a JSONL convergence-bound report, **not** a loss probability.
Methodology: `docs/ADR-0001-quorum-replication.md` +
`docs/RUNBOOK-chaos-campaign.md`.

## Confidence

`confidence` field on a memory. Float in `[0.0, 1.0]`. Default `1.0`.
Affects recall scoring (higher = ranks higher). The autonomous
forget pass requires a superseder to have **equal or higher**
confidence before archiving the older memory.

## Consolidate

Merge multiple memories into one long-term summary. Sources are
archived (not lost); the consolidated memory carries `derived_from`
links to each source. Available as `ai-memory consolidate`,
`memory_consolidate` (MCP), and `POST /api/v1/consolidate`.

## Curator (v0.6.1)

Autonomous background process (`ai-memory curator`). Runs the full
autonomy loop: auto-tag → detect-contradiction → consolidate →
forget-superseded → priority-feedback → rollback-log → self-report.
Opt-in via CLI or the `ai-memory-curator.service` systemd unit.

## Curator rollback log

`_curator/rollback/<ts>` memories that record every autonomous
action as a reversible snapshot. Reverse with
`ai-memory curator --rollback <id>` or `--rollback-last N`. Once
reversed, entries are tagged `_reversed` — audit trail preserved.

## Feature tier

Controls which AI capabilities are active based on available memory
budget. `keyword` (FTS5 only, 0 MB), `semantic` (MiniLM + HNSW,
~256 MB), `smart` (+ nomic-embed + Gemma 4 E2B, ~1 GB), `autonomous`
(+ Gemma 4 E4B + cross-encoder, ~4 GB). Set per-invocation
(`--tier`) or as daemon default. See `docs/ADMIN_GUIDE.md`.

## Federation

Opt-in multi-agent replication layer (v0.7, PR #282). Configured via
`ai-memory serve --quorum-writes N --quorum-peers URL,URL`. Every HTTP
write fans out to peers; returns 201 only on `W-1` peer acks within
`--quorum-timeout-ms`. Otherwise returns 503 `quorum_not_met`.

## FTS5

SQLite's full-text search extension. Powers exact-phrase keyword
search on `(title, content)`. Always enabled regardless of feature
tier — it's the default recall mechanism for `keyword` and the first
stage of hybrid recall for `semantic`+.

## Governance

The mechanism that enforces approval policies on writes. Set via
`namespace_set_standard` (MCP) or governance config. Actions that
require approval return **202 Accepted** with a `pending_id`;
approvers post to `/api/v1/pending/{id}/approve`. Consensus gates
require N distinct registered agents.

## HNSW

Hierarchical Navigable Small World — the approximate nearest-neighbor
index used for semantic recall. In-process by default
(`instant-distance` crate). Postgres backend (#279) uses pgvector's
native HNSW instead.

## Hybrid recall

The default recall mode. Combines FTS5 keyword scoring, semantic
cosine similarity via HNSW, priority/confidence/recency/tier
boosts. Adaptive blending weights semantic (0.50 for short content
≤500 chars) → keyword (0.15 for long content ≥5000 chars) because
embeddings lose information on long text.

## MCP (Model Context Protocol)

Anthropic's JSON-RPC protocol for AI-tool integration. ai-memory ships
an MCP server via `ai-memory mcp` exposing 23 tools (memory_store,
memory_recall, etc.) + 2 prompts over stdio. Works with Claude Code,
Claude Desktop, Cursor, Codex, Grok, Gemini, Llama Stack. See
`docs/USER_GUIDE.md`.

## Memory

The core data unit. A 15-field record with `id`, `tier`, `namespace`,
`title`, `content`, `tags`, `priority`, `confidence`, `source`,
`access_count`, timestamps, `expires_at`, and `metadata`.
`(title, namespace)` is a unique key — storing a duplicate upserts.
See `docs/DEVELOPER_GUIDE.md` § "Data Model".

## Memory link

A typed relationship between two memories. Kinds: `related_to`,
`supersedes`, `contradicts`, `derived_from`. Used by consolidate (to
track provenance) and the curator (to mark contradictions).

## Namespace

String partition for memories. Typical use: one namespace per project
or per agent. Reserved prefix `_` (e.g. `_messages/<target>`,
`_curator/reports`, `_agents`) is for system use — the curator
ignores `_`-prefixed namespaces.

## Post-store hook

See **Autonomy hooks**.

## Priority

Integer 1–10 on a memory. Default 5. Higher values rank higher on
recall. The curator's priority-feedback pass nudges this: +1 for
memories with `access_count ≥ 10` in the last 7 days; −1 for
memories untouched for 30+ days.

## Quorum (W-of-N)

Federation write contract from ADR-0001. `N` = peer count, `W` = how
many peers must ack before the write returns OK. Default
`W = ceil((N+1)/2)` (majority). Configurable via `--quorum-writes`.

## Recall

Query operation that returns memories matching a natural-language
context. Semantics: semantic + keyword + priority/confidence/recency
blend. **Mutates the DB**: increments `access_count`, extends TTL,
promotes mid→long at 5 accesses, nudges priority every 10 accesses.

## SAL — Storage Abstraction Layer (v0.7)

Trait-based storage boundary (`MemoryStore`). Adapters: `SqliteStore`
(default), `PostgresStore` (pgvector-backed, under `--features
sal-postgres`). Migration via `ai-memory migrate --from … --to …`.
Running `serve` against Postgres needs v0.7.1's handlers refactor —
see `docs/RUNBOOK-adapter-selection.md`.

## Scope (Task 1.5)

Visibility semantics for multi-agent recall. Stored as
`metadata.scope` on a memory. Values: `private` (default), `team`,
`unit`, `org`, `collective`. Combined with `--as-agent` at recall
time: an agent in `ai-memory-mcp` namespace sees private memories
only in that exact namespace; team memories in the parent subtree;
unit in the grandparent; org anywhere in the org's namespace tree;
collective everywhere.

## Source

String tag on a memory indicating where it came from. Values:
`user`, `claude`, `hook`, `api`, `cli`, `sync`, `import`, `mine`,
`consolidate`, `autonomous`. Used by admins for filtering and by the
curator to avoid recursive tagging of its own outputs.

## Sync-daemon

One-way peer-to-peer knowledge mesh (pre-v0.7). Configured via
`ai-memory sync-daemon --peers URL,URL`. Pulls and pushes periodically;
no quorum, no acks. For synchronous federation use `serve
--quorum-writes` (v0.7).

## Tags

Optional string list on a memory. Used for filtering in list /
search / recall. The curator's `auto_tag` pass populates
`metadata.auto_tags` separately from the user-supplied `tags` field.

## Tier

`short` (6 h TTL), `mid` (7 d TTL, default), `long` (permanent). Tier
is **monotonic** — never downgrades. Promotion happens manually
(`ai-memory promote`) or automatically after 5 recalls.

## TOON

Token-efficient JSON alternative format (`toon.rs` module). 40–60%
smaller than JSON for the same payload. Used optionally in MCP tool
responses when `format: "toon"` or `"toon_compact"` is requested.

## TTL (Time-To-Live)

Seconds until a memory expires. Set by tier default or explicitly via
`--ttl-secs` / `--expires-at`. Extended on recall. GC removes
expired memories every 30 minutes by default.

## Upsert

`(title, namespace)` collision behaviour. Storing a memory whose
`(title, namespace)` matches an existing row updates that row
in-place. Tier is never downgraded. Original `metadata.agent_id` is
preserved.

## Vector clock

Lamport-style causal timestamp the sync-daemon exchanges to avoid
double-applying updates. Stored in `sync_state` table.

## See also

- `docs/USER_GUIDE.md` — MCP tool reference (every `memory_*` tool).
- `docs/DEVELOPER_GUIDE.md` — how these concepts map to modules.
- `docs/ADMIN_GUIDE.md` — operational semantics (GC, archive, backup).
