# Capabilities v3 — canonical phrasings

> **Status:** SHIPPING with v0.7.0 — A1 (`summary`) + A2 (`to_describe_to_user`)
> **Date:** 2026-05-05
> **Issue:** [#545](https://github.com/alphaonedev/ai-memory-mcp/issues/545)
> **Built by:** `src/mcp.rs::build_capabilities_summary` and `build_capabilities_describe_to_user`

The capabilities-v3 response (track A of the v0.7.0 `attested-cortex` epic) carries two pre-computed strings that LLMs reading `memory_capabilities` are expected to converge on. This page pins the canonical phrasings so future drift surfaces in CI (the assertions live in `tests/capabilities_v3.rs` and `tests/calibration_t0.rs`).

---

## Why two strings, not one

The two strings serve different audiences:

| Field | Audience | Tone | Purpose |
|---|---|---|---|
| `summary` | The LLM (operator-style readout) | Terse, technical, names the recovery vocabulary verbatim | Makes the LLM converge on accurate first-answer descriptions; teaches it the names of the loader tools (`memory_load_family`, `memory_smart_load`) and the CLI escape hatch (`--profile`) |
| `to_describe_to_user` | The end-user (via the LLM) | Plain English, no MCP jargon | The sentence the LLM should repeat verbatim when an end-user asks "what tools do you have?". Strips `memory_` prefixes, names the recovery hint in user terms ("I can load them on demand") |

The split exists because reasoning-class LLMs in 2026-04 NHI Discovery Gate observation cells consistently embedded MCP-internal vocabulary in their user-facing descriptions when given only one calibration string. The two-string layout lets the operator-facing one stay technical (where that's correct) while the user-facing one stays clean.

---

## A1 — `summary` canonical phrasing

```
{visible} of {total} tools are advertised in tools/list under the current profile ({label}). The other {unloaded} are listed in this manifest but NOT directly callable. To use any unloaded tool, choose one of: (a) restart the server with --profile <family> or --profile full, (b) call memory_load_family(family=<name>) — preferred, (c) call memory_smart_load(intent='<plain language>') — easiest, (d) call the tool by name and recover from JSON-RPC -32601.
```

Substitution variables:

| Variable | Value | Example (`--profile core`) |
|---|---|---|
| `{visible}` | tools advertised in `tools/list` under the active profile (includes always-on bootstraps not already in profile) | `6` |
| `{total}` | total tool count across all families | `43` |
| `{label}` | profile name (`core`/`graph`/`admin`/`power`/`full`) or comma-joined family list for custom profiles | `core` |
| `{unloaded}` | `total − visible` | `37` |

The four recovery paths (a–d) appear verbatim in the canonical phrasing **regardless of the active profile** — so an LLM exposed only to `--profile full` still learns the recovery vocabulary for environments where it isn't.

### Worked examples

| Profile | Opening |
|---|---|
| `core` | `6 of 43 tools are advertised in tools/list under the current profile (core). The other 37 …` |
| `graph` | `14 of 43 tools are advertised in tools/list under the current profile (graph). The other 29 …` |
| `full` | `43 of 43 tools are advertised in tools/list under the current profile (full). The other 0 …` |

---

## A2 — `to_describe_to_user` canonical phrasing

Two forms depending on whether anything is unloaded.

### Form 1 — partial profile (some tools unloaded)

```
I can directly use {n_loaded} memory tool{s} right now ({preview_loaded}{ellipsis}). {n_unloaded} more ({preview_unloaded}, etc.) are available on demand — I can load them if you ask for something that needs them, or you can restart the server with a different profile.
```

### Form 2 — full profile (nothing unloaded)

```
I can directly use all {n_loaded} memory tools right now ({preview_loaded}{ellipsis}). Nothing more to load — the full memory surface is already active.
```

Substitution variables:

| Variable | Value | Notes |
|---|---|---|
| `{n_loaded}` | tools loaded by family membership, EXCLUDING the always-on bootstrap (`memory_capabilities`) | `core`: 5; `graph`: 13; `full`: 42 |
| `{preview_loaded}` | comma-joined first 5 loaded tool names with the `memory_` prefix STRIPPED | `core`: `store, recall, list, get, search` |
| `{ellipsis}` | `, ...` if `n_loaded > 5`, else empty | `graph` and larger get the ellipsis |
| `{n_unloaded}` | `42 − n_loaded` — the 42 excludes the always-on bootstrap from BOTH sides for honest counting | `core`: 37; `graph`: 29; `full`: 0 |
| `{preview_unloaded}` | comma-joined first 4 unloaded tool names, prefix-stripped | `core`: `update, delete, forget, gc` |
| `{s}` | `s` if `n_loaded != 1`, else empty | always `s` in practice |

### Tone constraint

The describe sentence is **forbidden from MCP jargon**. The pinned `tests/capabilities_v3.rs::cap_v3_describe_core_profile_is_plain_english_with_loaded_names` test asserts these strings DO NOT appear in `to_describe_to_user`:

- `--profile <family>`
- `memory_load_family`
- `memory_smart_load`
- `JSON-RPC`
- `-32601`
- `memory_` (any prefix-bearing tool name)
- `tools/list`

If a future increment adds MCP-internal vocabulary to this string, that test goes red. Use `summary` for operator-facing recovery vocabulary; keep `to_describe_to_user` plain.

### Worked examples

| Profile | `to_describe_to_user` |
|---|---|
| `core` | `I can directly use 5 memory tools right now (store, recall, list, get, search). 37 more (update, delete, forget, gc, etc.) are available on demand — I can load them if you ask for something that needs them, or you can restart the server with a different profile.` |
| `graph` | `I can directly use 13 memory tools right now (store, recall, list, get, search, ...). 29 more (update, delete, forget, gc, etc.) are available on demand — I can load them if you ask for something that needs them, or you can restart the server with a different profile.` |
| `full` | `I can directly use all 42 memory tools right now (store, recall, list, get, search, ...). Nothing more to load — the full memory surface is already active.` |

---

## A3 + A4 phrasing extensions (planned)

A3 will add a per-tool `callable_now: bool` flag (combines `loaded` with the `[mcp.allowlist]` agent-can-call check). A4 will add a top-level `agent_permitted_families: ["core", "graph"]` array when the allowlist applies. Neither extends `summary` or `to_describe_to_user` directly; both add structured fields a downstream renderer can use.

A5 bumps the default wire shape from v2 to v3 and seals these phrasings as the recommended client target. v2 + v1 stay supported for backward compat.

---

## How to regenerate

The canonical strings are computed at response time from the live `Profile` state — they're never cached at build time. To inspect what your daemon serves, call `memory_capabilities` with `accept="v3"`:

```
echo '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_capabilities","arguments":{"accept":"v3"}}}' | ai-memory mcp
```

The returned document carries both fields at the top level alongside `schema_version: "3"`.

---

## Refs

- [v0.7.0 epic](./V0.7-EPIC.md) — track A, tasks A1–A5
- [v0.7.0 NHI prompts](./v0.7-nhi-prompts.md) — tasks A1, A2 (per-task NHI starters)
- `src/mcp.rs::build_capabilities_summary` — A1 builder
- `src/mcp.rs::build_capabilities_describe_to_user` — A2 builder
- `tests/capabilities_v3.rs` — A1+A2 contract pins
- `tests/calibration_t0.rs` — A2 calibration assertions (Discovery Gate T0 cell coverage)
