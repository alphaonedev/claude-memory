# Migrating from v0.6.3.x to v0.6.4

**v0.6.4 — `quiet-tools`** ships with a **collapsed default tool surface**. This document is the operator's guide to the change and the opt-out path for power users.

---

## What changed

`ai-memory mcp` now defaults to `--profile core` instead of advertising all 43 tools. Eager-loading harnesses (Claude Desktop, OpenAI Codex CLI, xAI Grok CLI, Google Gemini CLI) save ~4,700 input tokens of tool-schema prefix per request — a **76.4% reduction** measured against `cl100k_base`, the BPE Claude / GPT use for input accounting.

The other 38 tools remain reachable. Three opt-up paths:

1. **Static profile** — `ai-memory mcp --profile graph|admin|power|full` selects a wider family set at startup.
2. **Comma-list custom** — `ai-memory mcp --profile core,graph,archive` registers exactly those families.
3. **Runtime expansion** — call `memory_capabilities --include-schema family=<name>` from inside the agent loop. The host (e.g., Claude Code's deferred-tools path) can register the returned schemas without restarting the MCP server.

`memory_capabilities` is always loaded regardless of profile (it's the bootstrap surface for runtime expansion).

---

## Action required for power users

If you depend on tools outside the 5 core (`memory_store`, `memory_recall`, `memory_list`, `memory_get`, `memory_search`), pick **one** of:

### Option A — Reproduce v0.6.3 surface 1:1

```bash
ai-memory mcp --profile full
```

Or via env / config:

```bash
# bash / zsh
export AI_MEMORY_PROFILE=full

# config.toml
[mcp]
profile = "full"
```

Resolution order: CLI flag > `AI_MEMORY_PROFILE` env > `[mcp].profile` config > `core` default.

### Option B — Pick a narrower profile that includes your tools

| If you use | Use profile |
|---|---|
| `memory_kg_*`, `memory_link`, `memory_entity_*` | `graph` |
| `memory_update`, `memory_delete`, `memory_promote`, `memory_pending_*` | `admin` |
| `memory_consolidate`, `memory_auto_tag`, `memory_check_duplicate`, `memory_expand_query` | `power` |
| `memory_archive_*` | `core,archive` (custom) |

### Option C — Recommended: keep `core`, opt up via `memory_capabilities`

```typescript
import { AiMemoryClient, requireProfile } from "@alphaone/ai-memory";

const client = new AiMemoryClient({ baseUrl: "http://localhost:9077" });
await requireProfile(client, "graph");  // throws ProfileNotLoaded if missing
```

```python
from ai_memory import AiMemoryClient, require_profile, ProfileNotLoaded

with AiMemoryClient(base_url="http://localhost:9077") as c:
    require_profile(c, "graph")  # raises ProfileNotLoaded if missing
```

---

## Per-harness recommendations

Run `ai-memory install --harness <name>` after upgrading to write the v0.6.4 default config:

| Harness | Loading mode | Recommended profile | Reason |
|---|---|---|---|
| Claude Code | Deferred (ToolSearch) | `core` | Already lazy; profile barely matters |
| Claude Desktop | Eager | `core` (default) | Save ~4,700 prefix tokens/request |
| OpenAI Codex CLI | Eager | `core` (default) | Same |
| xAI Grok CLI | Eager | `core` (default) | Same |
| Google Gemini CLI | Eager + cache penalty | `core` (default) | Save tokens AND avoid cache-bust |

---

## Diagnostics

`ai-memory doctor --tokens` reports per-family + per-profile token cost using the static schema-size table compiled into the binary. Useful for:

- Auditing the surface a daemon will advertise (active vs. full)
- Comparing savings across hypothetical profiles before committing
- Catching schema-bloat regressions

```bash
ai-memory doctor --tokens                       # human-readable
ai-memory doctor --tokens --json                # structured
ai-memory doctor --tokens --raw-table           # full per-tool dump
ai-memory doctor --tokens --profile graph       # hypothetical profile
```

`ai-memory audit show --capability-expansions` reads the new `audit_log` SQLite table (schema v20) populated by `memory_capabilities --include-schema` calls. Useful for fleet operators verifying which agents are expanding which families.

---

## NHI guardrails phase 1

v0.6.4 adds an opt-in per-agent capability allowlist. Default: gate disabled (Tier-1 single-process semantics, every caller may expand any family). Operators opt in by writing the table:

```toml
[mcp.allowlist]
"alice"          = ["core", "graph"]
"ai:claude-code" = ["full"]
"*"              = ["core"]
```

Pattern resolution: exact match wins; otherwise longest-prefix; otherwise the `"*"` wildcard. No-agent-id callers fall through to the wildcard rule.

Every `memory_capabilities --include-schema` call (grant or deny) is recorded in `audit_log` for compliance review.

---

## What did **not** change

- Database schema is **fully backward-compatible** — existing v0.6.3.x DBs migrate cleanly to v20 (audit_log table added; everything else unchanged).
- HTTP API endpoints — every v0.6.3 route stays at the same path with the same shape. The new `--profile` flag controls only the MCP `tools/list` surface.
- Memory data — no migration required for stored memories; embeddings, archives, links, governance policies all carry forward 1:1.
- Boot manifest cost — `ai-memory boot` output is independent of profile; only `tools/list` is affected.
- The CLI surface (`ai-memory store`, `recall`, etc.) — every v0.6.3 subcommand continues to work unchanged.

---

## Related

- [`docs/v0.6.4/V0.6.4-EPIC.md`](v0.6.4/V0.6.4-EPIC.md) — single-doc framework for the sprint
- [`docs/v0.6.4/rfc-default-tool-surface-collapse.md`](v0.6.4/rfc-default-tool-surface-collapse.md) — design RFC
- [`benchmarks/v0.6.4-cross-harness.md`](../benchmarks/v0.6.4-cross-harness.md) — token-cost measurement
- [`CHANGELOG.md`](../CHANGELOG.md) — full v0.6.4 entry
- v0.6.4 cert campaign in [`alphaonedev/ai-memory-test-hub`](https://github.com/alphaonedev/ai-memory-test-hub/blob/main/campaigns/v0.6.4.md)
