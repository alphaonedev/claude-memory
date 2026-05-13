# Substrate-level agent-action rules engine

**Status:** v0.7.0 (issue [#691](https://github.com/alphaonedev/ai-memory-mcp/issues/691)) — ships callable but **un-wired**; seed rules R001-R004 land at `enabled=0` awaiting operator activation.

## Why this exists — RCA

The K9 governance pipeline (`src/governance/mod.rs`) gates exactly six substrate-INTERNAL operations:

- `memory_store`
- `memory_link`
- `memory_delete`
- `memory_archive`
- `memory_consolidate`
- `memory_replay`

Every operator hard rule that was violated during the v0.7.0 campaign (5-6 occurrences of `/tmp` writes, low-disk `cargo` runs) targeted actions **OUTSIDE** that surface — Bash command execution, filesystem writes outside the substrate, process spawns, network requests. The K9 pipeline had no insertion point to see them.

This module adds a parallel engine — `check_agent_action` — that evaluates a declarative table of typed rules at every external-action entry point.

## Honest enforcement language

There are two boundaries, with distinct guarantees:

| Boundary                            | Guarantee                                                                                                          | Wording                                       |
|-------------------------------------|--------------------------------------------------------------------------------------------------------------------|-----------------------------------------------|
| Substrate-internal ops              | K9 mechanically applied at the write path. Agent cannot bypass.                                                    | **substrate-authoritative**                   |
| Agent-external ops (Bash / FS / …)  | Rule lives in substrate. Harness PreToolUse hook calls `memory_check_agent_action` and honors the returned decision. | **substrate-rule-bound, harness-mediated**    |

The harness boundary is **mechanical config** (operator-audited `settings.json`); the agent-attention boundary is **probabilistic recall**. Strong directional improvement; not literal "100%."

Capabilities v3 stamps:

```json
{
  "governance": {
    "agent_action_check": "substrate-authoritative-for-internal-ops",
    "rules_immutable_seed": true
  }
}
```

## Schema

Migration `0024_v07_governance_rules.sql`:

```sql
CREATE TABLE governance_rules (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    matcher       TEXT NOT NULL,             -- per-kind JSON
    severity      TEXT NOT NULL CHECK (severity IN ('refuse','warn','log')),
    reason        TEXT NOT NULL,
    namespace     TEXT NOT NULL DEFAULT '_global',
    created_by    TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    enabled       INTEGER NOT NULL DEFAULT 1,
    signature     BLOB,
    attest_level  TEXT NOT NULL DEFAULT 'unsigned'
);
```

## Per-kind matcher shapes

| `kind`             | Matcher JSON                                       | Notes                                                                 |
|--------------------|----------------------------------------------------|-----------------------------------------------------------------------|
| `bash`             | `{"command_regex":"..."}`                          | Substring match on the command line.                                  |
| `filesystem_write` | `{"glob":"/tmp/**"}`                               | Reuses the substrate glob vocabulary (`*` per-segment, `**` cross-`/`). |
| `network_request`  | `{"host":"evil.example.com"}`                      | Exact host match.                                                     |
| `process_spawn`    | `{"binary":"cargo","disk_free_min_gib":20}`        | Binary name match plus optional disk-threshold refusal.               |
| `custom`           | `{"kind":"<your_kind>"}`                           | Extension point for caller-specific actions.                          |

## Seed rules (land at `enabled=0`)

| ID   | Kind               | Matcher                                            | Why                                                              |
|------|--------------------|----------------------------------------------------|------------------------------------------------------------------|
| R001 | `filesystem_write` | `{"glob":"/tmp/**"}`                               | Operator hard rule — no `/tmp` writes.                            |
| R002 | `filesystem_write` | `{"glob":"/var/tmp/**"}`                           | Operator hard rule — no `/var/tmp` writes.                        |
| R003 | `filesystem_write` | `{"glob":"/private/tmp/**"}`                       | macOS realpath of `/tmp` — closes the realpath escape hatch.      |
| R004 | `process_spawn`    | `{"binary":"cargo","disk_free_min_gib":20}`        | Refuse `cargo` on low-disk systems (<20 GiB free).                |

**These ship disabled.** macOS treats `/tmp` as a symlink to `/private/tmp`, and many existing scripts/tests assume `/tmp/foo` works. Enabling R001-R003 without a test-fleet audit would break the build.

### Activating seed rules

After running:

```bash
grep -rn "/tmp/" tests/ scripts/ benches/
grep -rn "/private/tmp/" tests/
```

…and resolving every match, activate each rule:

```bash
ai-memory rules enable R001 --sign
ai-memory rules enable R002 --sign
ai-memory rules enable R003 --sign
ai-memory rules enable R004 --sign
```

`--sign` requires the operator's Ed25519 keypair at `${AI_MEMORY_KEY_DIR:-~/.config/ai-memory/keys}/operator.priv` (mode 0600). Without it the CLI refuses with `governance.no_operator_key`.

## Operator identity

The operator is identified by a keypair on disk:

| File                         | Mode (Unix) | Contents                            |
|------------------------------|-------------|-------------------------------------|
| `<key-dir>/operator.pub`     | `0644`      | 32 raw bytes — `VerifyingKey::to_bytes()` |
| `<key-dir>/operator.priv`    | `0600`      | 32 raw bytes — `SigningKey::to_bytes()`   |

Default `<key-dir>` is `~/.config/ai-memory/keys/` (override with `AI_MEMORY_KEY_DIR` or `--key-dir`). Generation:

```bash
ai-memory identity generate --agent-id operator
```

## Mutation gate by surface

| Surface | Read (`list` / `check`)          | Mutation (`add` / `enable` / `disable` / `remove`)                                                   |
|---------|----------------------------------|------------------------------------------------------------------------------------------------------|
| CLI     | Unprivileged                     | Requires `--sign`; loads `operator.priv` from disk.                                                  |
| HTTP    | Unprivileged                     | Requires `X-AI-Memory-Operator-Signature` header; verified against `operator.pub` on disk.            |
| MCP     | Tools `memory_check_agent_action` + `memory_rule_list` | **Explicitly disabled** — mutation tools are NOT registered. Returns `governance.not_available_over_mcp` if a future caller invokes one. |

This split is per design revision 2026-05-13 (issue #691 comment). The rationale: MCP stdio is the agent-facing channel; rule mutation must remain operator-only because a compromised agent must not be able to weaken its own constraints.

## What is *not* in this commit (deliberate)

- **`storage::insert` does NOT consult `check_agent_action`.** The wiring lands in a follow-up PR after the operator runs the test-fleet audit. This commit ships the engine and the audit chain; the substrate write paths still flow through K9 only.
- **`~/.claude/settings.json` PreToolUse hook is NOT installed.** The operator installs the hook (`{"type":"mcp_tool","tool":"memory_check_agent_action"}`) after reviewing the new MCP tools — that's a Restricted action.
- **No GitHub issue closure, no priority-10 verdict memory.** Operator does these after diff review.

## Audit chain

Every `check_agent_action` call appends one row to `signed_events` with:

- `event_type = "governance.check"`
- `agent_id` = the caller (NHI vocabulary)
- `payload_hash` = SHA-256 over the canonical `{action, decision}` JSON
- `timestamp` = RFC3339 UTC

Auditors filter on `event_type = 'governance.check'` to replay every external-action decision the daemon ever made.

## Test surface

- `tests/governance_agent_action.rs` — matcher types, decision routing, audit-event emission
- `tests/governance_singleton.rs` — 100-concurrent-check consistency
- `tests/governance_immutability.rs` — non-operator mutation refused; MCP wire-string stability
- `tests/governance_sandbox_boundary.rs` — every variant has a working refusal path
- `tests/governance_a2a_rules.rs` — peer A authors → replicated to peer B → peer B enforces
- `src/governance/agent_action.rs` (unit) — 21 tests covering matchers + audit + decision combinators
- `src/governance/rules_store.rs` (unit) — 13 tests covering CRUD + canonical encoding

## See also

- [`migrations/sqlite/0024_v07_governance_rules.sql`](../../migrations/sqlite/0024_v07_governance_rules.sql) — schema + seed
- [`src/governance/agent_action.rs`](../../src/governance/agent_action.rs) — engine
- [`src/governance/rules_store.rs`](../../src/governance/rules_store.rs) — CRUD store
- [`src/cli/rules.rs`](../../src/cli/rules.rs) — operator CLI
- Issue [#691](https://github.com/alphaonedev/ai-memory-mcp/issues/691) — original RCA + design
