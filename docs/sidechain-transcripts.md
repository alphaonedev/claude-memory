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
    — base `memory_transcripts` table (schema v21).
  - [`migrations/sqlite/0018_v07_transcript_links.sql`](../migrations/sqlite/0018_v07_transcript_links.sql)
    — link table from memory → transcript span (schema v24).
  - [`migrations/sqlite/0019_v07_transcript_lifecycle.sql`](../migrations/sqlite/0019_v07_transcript_lifecycle.sql)
    — lifecycle index (schema v25; `archived_at` column added via
    Rust-emitted `ALTER TABLE`).
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
[transcripts]
default_ttl_secs       = 2592000      # 30 days (compiled default)
archive_grace_secs     = 604800       # 7 days (compiled default)
max_decompressed_bytes = 16777216     # 16 MiB cap (compiled default; see I1)

[transcripts.namespaces."team/audit"]
default_ttl_secs    = 7776000         # 90 days (regulated namespace)
archive_grace_secs  = 2592000         # 30 days
auto_extract        = true            # opt this namespace into R5

[transcripts.namespaces."ephemeral/*"]
default_ttl_secs    = 3600            # 1 hour
archive_grace_secs  = 300             # 5 minutes
```

Schema: [`TranscriptsConfig`](../src/config.rs) at
[`src/config.rs:1917`](../src/config.rs);
[`TranscriptNamespaceConfig`](../src/config.rs) at
[`src/config.rs:1946`](../src/config.rs).

**Precedence** (resolved by `TranscriptsConfig::resolve` at
[`src/config.rs:2004`](../src/config.rs)):

1. Exact match in `namespaces` (e.g. `"team/audit"`).
2. Longest matching prefix pattern ending in `/*` (e.g. `"team/*"`
   matches `"team/eng"` and `"team/eng/inner"`).
3. Bare `"*"` wildcard.
4. Struct-level `default_ttl_secs` / `archive_grace_secs`.
5. Compiled defaults (30 days TTL, 7 days archive grace).

Each field resolves independently — a per-namespace override that
only sets `default_ttl_secs` inherits the global `archive_grace_secs`.
Non-positive values fall through to the next precedence level.

## Wire shape

```sql
CREATE TABLE memory_transcripts (
  id               TEXT PRIMARY KEY,           -- UUID
  namespace        TEXT NOT NULL,
  created_at       TEXT NOT NULL,              -- RFC3339 UTC
  expires_at       TEXT,                       -- RFC3339 UTC; NULL = no TTL
  compressed_size  INTEGER NOT NULL,
  original_size    INTEGER NOT NULL,
  zstd_level       INTEGER NOT NULL DEFAULT 3,
  content_blob     BLOB NOT NULL,              -- zstd-3 compressed
  archived_at      TEXT                        -- added by I3 ALTER
);

CREATE TABLE memory_transcript_links (
  memory_id     TEXT NOT NULL,
  transcript_id TEXT NOT NULL,
  span_start    INTEGER,
  span_end      INTEGER,
  PRIMARY KEY (memory_id, transcript_id),
  FOREIGN KEY (memory_id)     REFERENCES memories(id)           ON DELETE CASCADE,
  FOREIGN KEY (transcript_id) REFERENCES memory_transcripts(id) ON DELETE CASCADE
);
```

`content_blob` is the only token-bearing column; everything else is
metadata. The compression ratio on representative LLM-turn payloads
is ~4-6× — the substrate makes the trade explicit (raw bytes via
`original_size` vs stored bytes via `compressed_size`) so operators
can audit the trade per-row without decompressing.

`ON DELETE CASCADE` on both foreign keys means deleting a memory
wipes its provenance edges, and pruning a transcript (I3) wipes the
dangling links so `transcripts_for_memory` never returns ids that
can no longer be fetched.

## Lifecycle sweep

A background worker runs the two-phase sweep (cadence
operator-tunable via the daemon config):

1. **Archive pass.** Transcripts whose age exceeds the resolved
   `default_ttl_secs` AND whose linked memories are all expired (or
   absent) get `archived_at` stamped to the current RFC3339
   timestamp. The blob stays put so a late-binding `memory_replay`
   (I4) call can still reach it during the grace window.
2. **Prune pass.** Archived transcripts whose
   `archived_at + archive_grace_secs` has passed are deleted. The
   join-table rows are cleaned up automatically by `ON DELETE
   CASCADE`.

Implementation: `sweep_transcript_lifecycle` at
[`src/transcripts/storage.rs:359`](../src/transcripts/storage.rs).
The supporting partial index
`idx_memory_transcripts_archived_at WHERE archived_at IS NOT NULL`
keeps the prune-phase scan O(archived rows) rather than O(total
transcripts).

## `memory_replay` union (L2-4)

`memory_replay(memory_id, depth=N)` returns the **union** of
transcripts reachable by walking `reflects_on` edges from the target
memory up to `depth` levels (`replay_transcript_union` at
[`src/transcripts/replay.rs:95`](../src/transcripts/replay.rs)).
`depth=0` reproduces the pre-L2-4 shape (direct link only). The walk
respects the per-namespace `max_reflection_depth` cap —
composition cannot bypass.

Each returned entry is a `ReplayEntry`
([`src/transcripts/replay.rs:67`](../src/transcripts/replay.rs))
carrying transcript id, namespace, decompressed content (or the
relevant span if `span_start`/`span_end` were set on the link),
created_at, and the originating memory id.

## Security hardening (I1)

The v0.7.0 release/v0.7.0 branch landed
**`TranscriptsConfig.max_decompressed_bytes`** as a config-driven
cap (commit `26fab06`) with a default of `MAX_DECOMPRESSED_BYTES =
16 * 1024 * 1024` (16 MiB, [`src/transcripts/storage.rs:29`](../src/transcripts/storage.rs)).
Prior to I1, a malicious peer could push a 1 KiB zstd payload that
decompressed to hundreds of MiB and exhaust the daemon's memory. The
cap is checked on every `fetch`; payloads above the cap are refused
with a `TranscriptDecompressionExceedsCap` error.

The cap is **per-call**: concurrent fetches consume up to
N × `max_decompressed_bytes` of transient memory. Operators with
legitimately larger transcripts raise the cap explicitly via the
`[transcripts] max_decompressed_bytes` config field.

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
- [`tests/i1_zstd_bomb.rs`](../tests/i1_zstd_bomb.rs) — decompression-cap
  enforcement.

## Operator workflow

1. **Pick a namespace** that benefits from transcript storage (high
   information density per turn — engineering namespaces, postmortems,
   RCA tickets).
2. **Add the `[transcripts.namespaces."<name>"]` block** to
   config.toml, setting per-namespace TTL / archive-grace overrides
   if the global defaults don't fit. Set `auto_extract = true` to opt
   the namespace into the R5 pre_store extraction hook.
3. **Wire the R5 hook** in `hooks.toml` if you want automatic
   transcript extraction on store:
   ```toml
   [[hook]]
   event = "pre_store"
   command = "/usr/local/bin/transcript-extractor"
   priority = 500
   timeout_ms = 2000
   mode = "exec"
   enabled = true
   namespace = "team/*"
   ```
4. **Restart** the daemon (or `kill -HUP` for hooks; transcripts
   config is loaded at startup).
5. **Verify** with `ai-memory mcp call memory_capabilities '{"schema_version":"3"}' | jq '.transcripts'` —
   the response surfaces the enabled namespaces.
6. **Audit disk usage** periodically — the `content_blob` column is
   the only large surface. `du -sh ~/.local/share/ai-memory/memory.db`
   tells the story.

## Production ingestion patterns

**Pattern A: explicit per-turn store via the R5 hook.** The
recommended pattern. The transcript-extractor hook fires on every
`pre_store`, captures the (operator-supplied) `transcript` field
from the inbound payload, compresses it, inserts the
`memory_transcripts` row, and writes the `memory_transcript_links`
entry. The agent sees no extra latency beyond the hook overhead
(<5ms for typical turns).

**Pattern B: batched extraction via post-hoc job.** When the agent
isn't transcript-aware (legacy integrations, A2A peers without R5
support), run a periodic job that scans recent memories, fetches the
upstream conversation log, compresses + inserts, and back-fills the
links. Less timely but doesn't require agent cooperation.

**Pattern C: opt-in per-memory via metadata flag.** The R5 hook can
inspect `metadata.attach_transcript = true` and only fire on
explicitly-flagged memories. Lower volume; lower disk cost; lower
recall fidelity. Suitable for high-volume namespaces where most
turns are not worth the storage.

**Avoid Pattern D: bulk re-ingest.** Re-ingesting a long historical
conversation as a single transcript is legal (the `original_size`
column accepts arbitrary integers) but slow to fetch and likely to
hit `max_decompressed_bytes` on read. Better to split historical
conversations into per-turn rows during ingest, even if it costs
more INSERT statements.

## Redaction policy authoring

The substrate stores transcripts as raw bytes — redaction must
happen **before** `store`. The two recommended patterns:

1. **Inline at the hook.** The transcript-extractor reference
   implementation accepts a `--redact-pattern <regex>` flag. Operators
   author the regex once and the hook scrubs every incoming turn
   before compression. The redacted text is what lands in
   `content_blob`; the raw text never touches disk.
2. **Pre-extractor pipeline.** A higher-volume pattern: a dedicated
   redaction service sits between the agent and the substrate. The
   agent's transcript field is post-redaction by the time the K10
   approve happens. Tracks better against a regulatory audit because
   the redaction layer is independently testable.

**Recommended baseline redaction patterns** (sentinel regexes; tune
to your tenant's data):

```
# Email addresses
[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}

# US SSN
\b\d{3}-\d{2}-\d{4}\b

# 16-digit card-like sequences
\b(?:\d[ -]*?){13,16}\b

# JWT tokens
ey[A-Za-z0-9_-]+\.ey[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+
```

**What the substrate will NOT redact for you.** Inline secrets in
prose (API keys mentioned in passing, credentials pasted into a
turn) require operator-authored patterns. The transcript-extractor
ships a baseline set but is intentionally conservative — false-
positive redaction breaks legitimate conversations.

## Retention tuning

**TTL choices by namespace shape:**

| Namespace shape | `default_ttl_secs` | `archive_grace_secs` | Rationale |
|---|---|---|---|
| Ephemeral chat / scratch | 3600 (1h) | 300 (5m) | Storage cost dominates value; lifecycle sweep aggressive. |
| Engineering work | 2592000 (30d, default) | 604800 (7d, default) | Replay value lasts through a quarter; archive grace covers post-launch RCA. |
| Postmortems / RCA | 31536000 (1y) | 7776000 (90d) | High replay value; long retention for compliance. |
| Regulated tenant | per contract | per contract | Pin to the contractual retention SLA; alert on sweep-time drift. |

**Disk-cost estimation.** Sustained 1 KiB raw / 200 B compressed per
turn at 100 turns/agent/day across 50 agents and 30-day TTL =
50 × 100 × 30 × 200 = ~30 MB of stable hot storage. The lifecycle
sweep keeps the ceiling bounded; without it, transcripts grow
linearly with time.

**`max_decompressed_bytes` tuning.** The 16 MiB default is generous
for chat-shape turns. Operators ingesting code review transcripts
(diffs + comments) regularly hit 4-8 MiB and should leave headroom.
Operators ingesting full audit-log dumps need to raise the cap —
but should also reconsider whether `memory_transcripts` is the right
substrate for the data (a dedicated log store may be cheaper).

## Troubleshooting

| Symptom | Likely cause | Diagnostic recipe |
|---|---|---|
| `memory_replay` returns empty | No transcript stored for that memory_id, or links missing | `SELECT * FROM memory_transcript_links WHERE memory_id = '<id>';` — if empty, the R5 hook didn't fire or wasn't wired for that namespace. |
| `TranscriptDecompressionExceedsCap` on fetch | Single transcript above `max_decompressed_bytes` | Raise the cap explicitly OR shard the transcript into per-turn rows on next ingest. |
| Disk growing unboundedly | Lifecycle sweep not running, OR TTL too generous | Inspect `SELECT COUNT(*), MIN(created_at), MAX(archived_at) FROM memory_transcripts;` to confirm the sweep is making progress. |
| Reflections refused via I3 cascade | Transcript was pruned but memory still exists | `ON DELETE CASCADE` removes the link row when transcript is deleted; memory persists. Re-ingest if recall fidelity matters. |
| R5 hook fires but no transcript stored | Hook returned `Modify` with empty transcript field, or namespace not opted-in | Check `auto_extract` is `true` for the namespace; check the hook's stdout for the response payload. |
| Compression ratio worse than expected (<2x) | Transcript content is already compressed/binary | zstd-3 doesn't help on binary blobs. Either skip transcript storage for these or accept the cost. |
| `memory_replay` slow for deep walks | Reflection-edge fan-out high; depth >2 hitting many transcripts | Reduce `depth` or shard the inquiry across smaller memory subsets. |

## Operator runbook (3am procedures)

**Daemon OOM suspected zstd bomb.** Check daemon log for
`TranscriptDecompressionExceedsCap` warnings just before the OOM. If
present, an attacker (or a misbehaving R5 hook) pushed an oversized
transcript. Immediate mitigation: set
`[transcripts] max_decompressed_bytes = 4194304` (4 MiB) in
config.toml and restart. Then audit the substrate:
```sql
SELECT id, namespace, original_size, compressed_size
FROM memory_transcripts
WHERE original_size > 16777216
ORDER BY original_size DESC LIMIT 20;
```
The rows are the candidates for incident response.

**Disk pressure.** Force an aggressive sweep by transiently lowering
TTL via SQL:
```sql
-- Mark ancient transcripts as archived NOW
UPDATE memory_transcripts
SET archived_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
WHERE archived_at IS NULL
  AND created_at < datetime('now', '-7 days');
```
The next prune pass will collect them. Reset the TTL config after
the disk pressure clears.

**Suspected redaction failure.** Pause R5 by disabling the
transcript-extractor hook (`enabled = false` in `hooks.toml`, then
`SIGHUP`). Audit existing transcripts via the redaction regex:
```bash
ai-memory mcp call memory_replay '{"memory_id":"…"}' \
  | jq -r '.entries[].content' \
  | grep -E '<your-secret-pattern>'
```
For confirmed leaks, the substrate has no in-place redaction primitive
today — the recovery is DELETE on the offending rows (the K10 +
governance trail records the deletion as an auditable event).

**Replay returns stale content after archive.** Expected during the
`archive_grace_secs` window — the blob is still present, the
`archived_at` stamp is informational. After the grace window the
prune phase deletes the row and `memory_replay` returns no entry for
that source.

See also: [`docs/MIGRATION_v0.7.md` §"Sidechain transcripts"](MIGRATION_v0.7.md),
the canonical inventory in
[`docs/internal/v070-feature-inventory.md` §"Feature: Sidechain transcripts"](internal/v070-feature-inventory.md),
the hook pipeline that drives the R5 pre_store extraction at
[`docs/hook-pipeline.md`](hook-pipeline.md), the signed-events
chain that records transcript-store events at
[`docs/signed-events-v4.md`](signed-events-v4.md), the federation
hardening that prevents zstd-bomb decompression DOS over the peer
mesh at [`docs/federation.md`](federation.md), the K10 approvals
path that gates transcript-write rules at
[`docs/k10-sse-approvals.md`](k10-sse-approvals.md), and the K8
quotas substrate that bounds per-agent transcript byte volume at
[`docs/k8-quotas.md`](k8-quotas.md).
