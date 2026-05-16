# Sidechain transcripts (Track I)

v0.7.0 ships raw conversation / reasoning trail storage in zstd-3
compressed BLOBs, linked to derived memories via the
`memory_transcript_links` table. The substrate underlies R5
auto-extraction and the L2-4 `memory_replay` union — operators get
full per-memory reconstruction without paying token costs on every
recall.

- **Code paths:** [`src/transcripts/mod.rs`](../src/transcripts/mod.rs),
  [`src/transcripts/replay.rs`](../src/transcripts/replay.rs),
  [`src/transcripts/storage.rs`](../src/transcripts/storage.rs).
- **Schema:**
  - [`migrations/sqlite/0016_v07_transcripts.sql`](../migrations/sqlite/0016_v07_transcripts.sql)
    — base `memory_transcripts` table.
  - [`migrations/sqlite/0018_v07_transcript_links.sql`](../migrations/sqlite/0018_v07_transcript_links.sql)
    — link table from memory → transcript span.
  - [`migrations/sqlite/0019_v07_transcript_lifecycle.sql`](../migrations/sqlite/0019_v07_transcript_lifecycle.sql)
    — lifecycle columns (`archived_at`, `archive_reason`).
- **Helper binary:** [`tools/transcript-extractor/`](../tools/transcript-extractor/)
  is the R5 reference `pre_store` hook. (Excluded from the crates.io
  upload via the parent `Cargo.toml` `include` allowlist.)
- **Capability registry entry:** `CapabilityTranscripts` extends the
  v0.6.4 entry with the lifecycle columns.
- **MCP tool:** `memory_replay(memory_id, depth?)` in the Graph
  family.

## Opt-in shape

```toml
# ~/.config/ai-memory/config.toml
[transcripts."team/*"]
enabled = true
ttl_days = 30
archive_after_days = 7
max_decompressed_bytes = 16777216   # 16 MiB cap (default; see hardening note)
```

Per-namespace glob match. A namespace with no matching block is
**not** transcript-enabled — writes to it never land in
`memory_transcripts`.

## Wire shape

```
CREATE TABLE memory_transcripts (
  id              TEXT PRIMARY KEY,           -- UUID
  namespace       TEXT NOT NULL,
  agent_id        TEXT NOT NULL,
  source_kind     TEXT NOT NULL,              -- 'llm_turn' | 'tool_output' | 'session_log' | …
  body_zstd       BLOB NOT NULL,              -- zstd-3 compressed
  body_sha256     BLOB NOT NULL,              -- of decompressed body
  created_at      TEXT NOT NULL,
  archived_at     TEXT,
  archive_reason  TEXT
);

CREATE TABLE memory_transcript_links (
  memory_id       TEXT NOT NULL,
  transcript_id   TEXT NOT NULL,
  span_start      INTEGER,
  span_end        INTEGER,
  PRIMARY KEY (memory_id, transcript_id)
);
```

`body_zstd` is the only token-bearing column; everything else is
metadata. The compression ratio on representative LLM-turn payloads
is ~4-6× — the substrate makes the trade explicit so operators can
choose between TTL aggression and recall fidelity.

## Lifecycle sweep

A background worker runs every 60 minutes by default:

1. **Archive pass.** Transcripts whose linked memories are all expired
   or archived get `archived_at` stamped and `archive_reason` set to
   `dependents_gone`.
2. **Prune pass.** Transcripts archived more than
   `archive_after_days` days ago are deleted; `body_zstd` is cleared
   first to free disk before the row deletion.

## `memory_replay` union (L2-4)

`memory_replay(memory_id, depth=N)` returns the **union** of
transcripts reachable by walking `reflects_on` edges from the target
memory up to `depth` levels. `depth=0` reproduces the pre-L2-4 shape
(direct link only). The walk respects the per-namespace
`max_reflection_depth` cap — composition cannot bypass.

## Security hardening (I1)

The v0.7.0 release/v0.7.0 branch landed
**`TranscriptsConfig.max_decompressed_bytes`** as a config-driven
cap (commit `26fab06`) with a default of 16 MiB. Prior to I1, a
malicious peer could push a 1 KiB zstd payload that decompressed to
hundreds of MiB and exhaust the daemon's memory. The cap is checked
on every read; payloads above the cap are refused with
`TranscriptDecompressionExceedsCap`.

Pinned by [`tests/i1_zstd_bomb.rs`](../tests/i1_zstd_bomb.rs).

## Test coverage

- [`tests/transcripts.rs`](../tests/transcripts.rs) — base
  read/write round-trip.
- [`tests/transcripts/replay_test.rs`](../tests/transcripts/replay_test.rs)
  — `memory_replay` walk.
- [`tests/transcript_extractor.rs`](../tests/transcript_extractor.rs)
  — R5 reference hook end-to-end.
- [`tests/i4_memory_replay_authz.rs`](../tests/i4_memory_replay_authz.rs)
  — auth path for `memory_replay`.

## Operator workflow

1. **Pick a namespace** that benefits from transcript storage (high
   information density per turn — engineering namespaces, postmortems,
   RCA tickets).
2. **Add the `[transcripts."<glob>"]` block** to config.toml.
3. **Restart** the daemon.
4. **Verify** with `ai-memory mcp call memory_capabilities
   '{"schema_version":"3"}' | jq '.transcripts'` — the response
   surfaces the enabled namespaces.
5. **Audit disk usage** periodically — the `body_zstd` column is the
   only large surface. `du -sh ~/.local/share/ai-memory/memory.db`
   tells the story.

See also: [`docs/MIGRATION_v0.7.md` §"Sidechain transcripts"](MIGRATION_v0.7.md),
the canonical inventory in
[`docs/internal/v070-feature-inventory.md` §"Feature: Sidechain transcripts"](internal/v070-feature-inventory.md).
