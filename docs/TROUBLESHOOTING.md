# ai-memory Troubleshooting

Common errors, causes, and fixes. If your scenario isn't here, check
`journalctl -u ai-memory --since "1 hour ago"` first, then open an
issue at <https://github.com/alphaonedev/ai-memory-mcp/issues>.

## Startup

### "database is locked"

**Symptom**: `ai-memory <cmd>` reports `Error: database is locked`.

**Cause**: Another ai-memory process (CLI, daemon, curator, or sync)
holds the SQLite write lock. SQLite uses a process-global lock; two
writers can't coexist.

**Fix**:

1. List any running ai-memory processes: `ps -ef | grep ai-memory`.
2. If a daemon is running, route your operation through it (HTTP API
   or MCP) instead of the CLI.
3. If you suspect a stale lock file, stop every process and check
   `~/.ai-memory-wal` / `~/.ai-memory-shm` companion files.
4. For long-running imports, the 5 s default `busy_timeout` may be
   too short. Increase via `AI_MEMORY_BUSY_TIMEOUT_MS=30000`.

### "could not find embedding model"

**Symptom**: First `recall` or `search` hangs then fails. Log shows
`hf-hub` download errors or `candle` model-load failure.

**Cause**: ai-memory downloads the embedding model lazily on first
semantic recall. First run needs ~90 MB for `all-MiniLM-L6-v2` (or
~270 MB for `nomic-embed-text-v1.5` on `smart`/`autonomous` tiers).
Network or disk issues interrupt the download.

**Fix**:

1. Confirm outbound access to `huggingface.co`.
2. Check `~/.cache/huggingface/hub/` for a partial download. Delete
   the model directory and retry.
3. For air-gapped environments, pre-stage the model via
   `huggingface-cli download sentence-transformers/all-MiniLM-L6-v2`.
4. If you don't need semantic recall, run with `--tier keyword` —
   FTS5-only, zero model load.

### "port 9077 already in use"

**Symptom**: `ai-memory serve` fails immediately with `Address
already in use`.

**Cause**: Another `ai-memory serve`, a development tool, or an old
process from a previous shutdown.

**Fix**:

```bash
# Find the offender
lsof -i :9077
# or
ss -tlpn | grep 9077

# Bind to a different port
ai-memory serve --port 19077
```

## MCP integration

### Claude Code / Desktop / Cursor don't see ai-memory tools

**Symptom**: Restarted the IDE after adding the MCP config; no
`memory_*` tools appear in the tool list.

**Causes + fixes**:

1. **Wrong config path**. Verify:
   - Claude Code: `~/.claude/mcp_servers.json` or the project-local
     `.claude/mcp_servers.json`.
   - Claude Desktop: `~/Library/Application Support/Claude/claude_desktop_config.json` (macOS).
   - Cursor: Settings → Features → MCP.

2. **JSON syntax error**. Paste the config into `jq '.' file.json`
   to validate.

3. **`ai-memory` not on PATH**. MCP servers inherit the IDE's PATH.
   Absolute path the command: `"command": "/usr/local/bin/ai-memory"`.

4. **Old IDE version**. MCP support landed in Claude Desktop 0.7+,
   Cursor 0.45+, Claude Code 1.0+.

5. **Server crashed on stdio**. Run `ai-memory mcp` manually in a
   terminal; you should see it waiting on stdin. If it exits
   immediately, check stderr for errors.

### "tools/list returned 31 tools, expected 34"

**Symptom**: Integration test fails on MCP tool count.

**Cause**: A new tool landed in `src/mcp.rs` without updating the
count assertion. Harmless — it's a test that locks the tool count to
prevent accidental removal. Update the assertion to match the new
count and ensure the new tool is in the `assert!(tool_names.contains())`
block.

### MCP tool returns "no memories found" but `ai-memory list` shows them

**Cause**: The MCP server and the CLI point at different databases.

**Fix**: Every entry point reads `AI_MEMORY_DB`. Set it consistently:

```jsonc
// Claude Code mcp_servers.json
{
  "mcpServers": {
    "ai-memory": {
      "command": "ai-memory",
      "args": ["mcp"],
      "env": { "AI_MEMORY_DB": "/Users/you/ai-memory.db" }
    }
  }
}
```

## Autonomy / curator

### "no LLM client configured" in curator report

**Symptom**: `ai-memory curator --once --json` report shows
`"errors": ["no LLM client configured"]` and zero operations.

**Cause**: The feature tier doesn't wire an LLM, or Ollama is
unreachable.

**Fix**:

1. Check feature tier: `ai-memory curator --tier smart` or
   `autonomous` (CLI flag reads the tier from config if unset).
2. Verify Ollama is running: `curl http://localhost:11434/api/tags`.
3. Pull the model: `ollama pull gemma4:e2b` (for `smart` tier).

### Curator cycle times are long (> 10 min)

**Cause**: Each eligible memory triggers an Ollama round-trip (~1–5 s).
With a large corpus and `--max-ops 100`, a cycle can take 5–10 min.

**Fix**:

- Lower `--max-ops` to fit your cycle budget.
- Enable Ollama KV compression (`OLLAMA_KV_CACHE_TYPE=q4_0`) to
  speed up each call. See `docs/RUNBOOK-ollama-kv-tuning.md`.
- Run `--daemon --interval-secs 3600` and let it catch up slowly.

### Curator made a bad call — how to undo it

```bash
# See the last 20 actions
ai-memory list --namespace _curator/rollback --limit 20

# Reverse a specific one
ai-memory curator --rollback <id>

# Reverse the last 5
ai-memory curator --rollback-last 5
```

Reversed entries are **tagged** `_reversed`, not deleted — the audit
trail is preserved.

## HTTP API

### "401 missing or invalid API key"

**Cause**: Daemon started with `--api-key` set. Pass the key:

```bash
curl -H "X-API-Key: YOUR_KEY" http://127.0.0.1:9077/api/v1/stats
# or
curl 'http://127.0.0.1:9077/api/v1/stats?api_key=YOUR_KEY'
```

`/api/v1/health` is always exempt — use it as a reachability probe.

### "500 Internal Server Error" with no body

**Cause**: Error-sanitisation strips stack traces from production
responses to avoid leaking internals.

**Fix**: Check the daemon log (`journalctl -u ai-memory`) for the
full error. If running in foreground, look at stderr. Raise verbosity
with `RUST_LOG=ai_memory=debug`.

### "503 quorum_not_met" on every write

**Cause**: Federation is configured (`--quorum-writes N --quorum-peers …`)
but peers are unreachable or slow.

**Diagnosis**:

1. Body carries `{"got":X,"needed":Y,"reason":"…"}`. `reason`:
   - `unreachable` — no peers responded at all (network / DNS).
   - `timeout` — some peers acked but not enough before
     `--quorum-timeout-ms`.
   - `id_drift` — peers returned different memory ids (replication
     divergence).
2. Curl each peer directly: `curl https://peer-a:9077/api/v1/health`.
3. Check peer mTLS allowlist — your fingerprint may not be listed.

**Fix**: lower `--quorum-writes` temporarily, restore peer
connectivity, restart with the original setting.

## Sync / federation

### Memories stop syncing between peers

**Cause**: Multiple possibilities.

**Diagnosis**:

1. On each peer: `ai-memory sync-daemon` must be running.
   `systemctl status ai-memory-sync` or check the log.
2. Vector-clock skew: `ai-memory stats` on each peer, compare
   `last_synced_at`.
3. mTLS fingerprint drift: if you rotated certs, the allowlist must
   be regenerated on every receiver.
4. `--batch-size 500` default may be too small for a backlog. Bump to
   `5000` temporarily.

### Split-brain: two peers diverged

**Cause**: Network partition. Both halves accepted writes. Now they
disagree on `(title, namespace)` content.

**Fix**: Decide which side is authoritative. On that side, run
`ai-memory export > snapshot.json`. On the other side,
`ai-memory import --trust-source < snapshot.json`. The upsert on
`(title, namespace)` will overwrite the divergent copies with the
authoritative ones.

Per-namespace conflict resolution is an open work item (sync-phase
Layer 2b).

## Performance

### `recall` is slow (> 2 s)

**Common causes**:

1. **First semantic recall after startup** — model load is ~500 ms
   cold. Warm up with a throwaway recall call.
2. **Large corpus + HNSW not yet built** — HNSW is built lazily on
   first semantic query against a given DB. 100k memories takes
   ~15 s to index once; subsequent queries are ms.
3. **Disk I/O bottleneck** — `iostat 1` to confirm. Move DB to SSD.
4. **SQLite contention under concurrent writes** — use `stats`
   output to see WAL size. If the daemon is doing a lot of writes,
   recall waits.

### Memory usage grows unbounded

**Cause**: HNSW index size grows with the number of memories. At
~100k memories × 384-dim vectors × 4 bytes = ~150 MB just for the
index.

**Fix**:

- Aggressive `gc` + reduce retention on `short` tier.
- Move to Postgres + pgvector for out-of-process index
  (`--features sal-postgres`, v0.7) — the canonical answer at
  100k+ memory scale.

## Governance

### My action returned "202 Accepted" but nothing happened

**Cause**: Governance requires an approval. Your action is in the
pending queue.

**Fix**:

```bash
# List pending
ai-memory pending list --status pending

# Approve (requires registered approver)
ai-memory pending approve <pending-id>

# Or reject
ai-memory pending reject <pending-id>
```

Consensus rules require multiple distinct registered agents — see
`docs/ADMIN_GUIDE.md` § "Governance".

## Still stuck?

1. Run `ai-memory stats --json` and attach to the issue.
2. Attach the last 50 lines of `journalctl -u ai-memory`.
3. State your tier (`ai-memory curator --once --dry-run --json` shows
   effective tier + errors).
4. Open <https://github.com/alphaonedev/ai-memory-mcp/issues>.
