# <Campaign name> — for SME engineers + architects (<DATE>)

> SKELETON. Target length: 1,500–2,000 words. Deep-dive, every claim traceable. Pin SHAs and versions at write time — they will move.

---

## Reproducibility

**Pinned binary at write time:** git SHA `<full-sha>` on branch `<branch>`. Worktree: `<absolute path>`.

**Schema version at HEAD:** v<N> (`CURRENT_SCHEMA_VERSION` in `src/storage/migrations.rs`).

**MCP tool count at `--profile full`:** <X> advertised = <Y> callable + 1 always-on.

**Models (autonomous tier):** <embedder>, <reranker>, <LLM via Ollama>.

**Databases:** <sqlite path>; <postgres+AGE if applicable>.

---

## Test methodology

<One paragraph: what playbook was exercised, how the tests are scoped (NHI vs A2A vs chaos vs ...), what client drove them, where the canonical playbook memory lives.>

| Phase | Name | Tests | Pass | Fail | Memory id |
|---|---|---|---|---|---|
| P0 | <name> | <n> | <n> | <n> | `<id>` |
| ... | | | | | |
| **Total** | | **<sum>** | **<sum>** | **<sum>** | verdict `<id>` |

---

## Token budget data

| Metric | Pre | Post | Ceiling | Status |
|---|---|---|---|---|
| `full_profile_total_tokens` | <n> | <n> | <n> | PASS / FAIL |
| `trimmed_full_profile_total_tokens` | <n> | <n> | <n> | PASS / FAIL |
| Per-tool max | n/a | <n> | <n> | PASS / FAIL |

CI guards pinning these forward: `tests/<file>.rs`, ...

---

## Schema migration ladder (if any landed this campaign)

| Migration | Issue | What changed |
|---|---|---|
| v<n-1> → v<n> | [#<num>](url) | <one-line> |

---

## Coverage data

| Module | Coverage | Floor | Notes |
|---|---|---|---|
| `<path>` | <%> | <%> | <notes> |

---

## Architecture observations

<If refactor work landed this campaign, give the before/after LOC table and cite issue numbers.>

| File / function | Before | After | Issue |
|---|---|---|---|
| `<path>` | <n> LOC | <n> LOC + split | [#<num>](url) |

---

## Security review verdict

**Status: GREEN / YELLOW / RED with <specific items>.**

<List closed security issues with #. Reference the code surfaces (`src/...`) where the fix landed.>

---

## Code review verdict

**Status: GREEN / YELLOW / RED with <specific items>.**

<Same shape as security.>

---

## Forensic audit log

<If touched: what the `signed_events.rs` chain captured during this campaign, what `ai-memory verify` reports.>

---

## Federation signing

<If touched: state of `X-Memory-Sig`, `AI_MEMORY_FED_REQUIRE_SIG`, peer attestation allowlist.>

---

## Performance

<Cite `PERFORMANCE.md` budgets, the CI bench gate, the actual measured numbers from this campaign.>

---

## <Optional: Plan C / deployment-specific verification>

<Container fleet state, postgres+AGE state, embedding-dim migrate state, network blockers if any.>

---

## Provenance maturity — six levels (if applicable)

| Level | Name | Status |
|---|---|---|
| 1 | Identity | <Carried / Partial / Missing — evidence> |
| 2 | Source | <...> |
| 3 | Causal | <...> |
| 4 | Capture confidence | <...> |
| 5 | Versioned | <...> |
| 6 | Reciprocal | <...> |

---

## Open items + dispositions

| Item | Type | Gate |
|---|---|---|
| #<num> | <type> | <operator-approval / $-gated / engineering> |

**<N> engineering-blocked issues.** <If non-zero: list each with disposition.>

---

## Discipline artifacts

- **Prime directive:** memory `<id>`, namespace `<ns>`
- **Orchestrator safeguards:** memory `<id>`, namespace `<ns>`
- **Violations log:** memory `<id>`, namespace `<ns>`
- **Lane index:** memory `<id>`, namespace `<ns>`

---

## What's still TBD after this writeup

<Honest disclosure of what's queued but not done. Each item: what's blocking, who owns it, where it'll land.>

1. <item>
2. <item>

---

*Drafted by <agent> on <date>, against binary SHA `<sha>`. Every claim on this page traces to a commit SHA, file path, memory id, GH issue URL, or canonical CLAUDE.md section. If a number on this page disagrees with what you measure on the binary, the binary wins — file an issue.*
