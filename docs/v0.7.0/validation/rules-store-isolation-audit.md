# V-2 `RulesStore` handle isolation audit

**Branch:** `validation/policy-engine-commercial-claim` @ `22101a7`
**Date:** 2026-05-14
**Claim being validated:** "the agent's reasoning context can't influence which rules get loaded"

## Architectural shape

`rules_store` is NOT a constructor-built type — it is a free-function namespace over a `rusqlite::Connection`. The "handle" is the `Connection` itself, and there are exactly two production paths that operate on it:

1. **Daemon-side rules consultation closure** (`src/daemon_runtime.rs:2031-2068`) — installed exactly once at daemon boot inside `bootstrap_serve`. The closure CAPTURES `db_path: PathBuf` at boot time and on every wire_check invocation it opens a FRESH `Connection` via `db::open(&rules_db_path)` and consults `check_agent_action_no_audit`. The captured `rules_db_path` is the same path the daemon was launched with — an agent cannot redirect it.

2. **Operator CLI mutation surface** (`src/cli/rules.rs`) — `ai-memory rules keygen / add / enable / disable / sign-seed / remove / check`. All take the operator's signing key from disk (default `~/.config/ai-memory/keys/operator.priv`, mode 0600).

## Construction-site / call-site audit

`grep -rn 'rules_store::insert|rules_store::remove|rules_store::set_enabled|rules_store::update_signature' src/`:

| file:line | classification | rationale |
|---|---|---|
| `src/cli/rules.rs:264, 296, 297, 314, 315, 326, 672` | **Authorized CLI** | `ai-memory rules add / enable / disable / remove / sign-seed`. Operator key on disk required. |
| `src/mcp/tools/rule_list.rs:122` | **TEST-ONLY** | inside `#[cfg(test)] mod tests` helper `insert()` (line 95+). |
| `src/mcp/tools/check_agent_action.rs:206` | **TEST-ONLY** | inside `#[cfg(test)] mod tests`. |
| `src/governance/agent_action.rs:710` | **TEST-ONLY** | inside `#[cfg(test)] mod tests`. |
| `src/cli/rules.rs:1187, 1236` | **TEST-ONLY** | inside `#[cfg(test)] mod tests`. |

## MCP exposure check

`src/mcp/mod.rs:1000-1001`:

```rust
"memory_check_agent_action" => handle_check_agent_action(conn, arguments),  // READ
"memory_rule_list" => handle_rule_list(conn, arguments),                     // READ
```

No `memory_rule_add` / `memory_rule_remove` / `memory_rule_enable` / `memory_rule_disable` registered. The reserved error vocabulary is documented at `src/mcp/tools/check_agent_action.rs:151`:

```rust
pub const MCP_MUTATION_DISABLED_ERROR: &str =
    "governance.not_available_over_mcp: rule mutation is operator-only \
     (CLI `ai-memory rules` or HTTP `POST /api/v1/governance/rules`)";
```

Comment at line 14-21: `MUTATION over MCP stdio is explicitly disabled — rule_add / rule_remove / rule_enable / rule_disable are NOT registered as MCP tools`.

## HTTP exposure check

The HTTP admin surface (`POST /api/v1/governance/rules`) requires the `X-AI-Memory-Operator-Signature` header (operator-signed) per the design note at the top of `0024_v07_governance_rules.sql`. The agent cannot forge that signature without the operator's private key.

## Handle-propagation check

`AppState` (in `src/handlers.rs`) does NOT hold an `Arc<RulesStore>` — it holds `Db = Arc<Mutex<(Connection, PathBuf, ResolvedTtl, bool)>>`. The wire-point closure in `bootstrap_serve` does NOT consume `AppState`'s `Db`; it opens its OWN connection from a captured `PathBuf` (`rules_db_path`) at line 2030. The agent's request never carries a connection handle.

The `OnceLock` `GOVERNANCE_PRE_ACTION` (`src/governance/wire_check.rs:77`) is a process-wide singleton. After the first `.set()` succeeds at daemon boot, all subsequent `.set()` attempts return `Err` (line 2069 logs `wire_check pre-action hook already installed`). The hook closure is therefore IMMUTABLE for the daemon's lifetime — no re-install path exists in production code.

## GAPs

**None identified.** Every mutation entry point requires either:
- the operator's Ed25519 private key on disk (CLI `--sign`), or
- the operator's signature on an HTTP admin header.

The agent's natural-language reasoning context cannot reach either credential.

## Limitations / honest scope

1. **Memory-side route to the rules table.** The `governance_rules` table lives in the same SQLite database as `memories`. A future MCP tool that allowed raw SQL execution would breach isolation — none exists today. The MCP surface is the curated tool set in `src/mcp/mod.rs::dispatch()`; arbitrary SQL is NOT a tool. The regression test in `tests/rules_store_isolation_pin.rs` pins this.

2. **`db_path` env-var redirection.** A poisoned environment at daemon launch time (e.g. `AI_MEMORY_DB` pointed elsewhere) could swap the rules DB. This is an OPERATOR concern (process startup hygiene), not an agent-request-time concern — the env is read at `bootstrap_serve` time, before any agent request is served.

3. **R001..R004 land at `enabled=0`.** Out-of-the-box, NO rule is active. An operator MUST run `ai-memory rules enable Rxxx --sign` to activate. This is by design (per the migration's doc note about macOS `/private/tmp` realpath) — but it means a vanilla daemon out of the box does NOT refuse the seed actions. This is an operator-procurement concern, not a substrate gap.

## Verdict

**V-2: GREEN.** Rules-engine state is daemon-controlled. The agent's request payload cannot mutate or redirect the rules store via any registered MCP tool or HTTP endpoint reachable without the operator's signing key.

## Notes for OpenAI Codex auditor

1. `rules_store` is intentionally a free-function namespace, not an owned-Type/handle. The "isolation" claim reduces to: (a) closures capture path-at-boot, (b) MCP exposes only read tools, (c) HTTP mutation requires operator signature. All three are mechanical, not policy-based.
2. The `OnceLock`-once-set invariant is structurally enforced by `std::sync::OnceLock::set` — no `.take()` or `.replace()` exists on `OnceLock` in std.
