# ai-memory v0.7.0 — Full-spectrum AI NHI Test Campaign (2026-05-17)

## What this is

A reproducible, peer-reviewable test campaign for **ai-memory v0.7.0** running against:

- **Node A** (`192.168.50.100`, FROSTYi.local) — IronClaw AI agents (alice / bob / charlie / dave) backed by the local v0.7.0 binary with grok-4.3 as the LLM.
- **Node B** (`192.168.50.1`) — PostgreSQL + Apache AGE backend serving as the remote SAL store under test.

Four substantive test tracks are exercised end-to-end:

| Track | Scope | Where it runs |
|-------|-------|---------------|
| **A** | NHI test playbook (12 phases, P0 → P11 + verdict) | Node A, local sqlite |
| **B** | A2A 4-domain campaign (alice / bob / charlie / dave) | Node A, IronClaw + grok-4.3 |
| **C** | Postgres + Apache AGE backend tests | Node A binary → Node B postgres |
| **D** | Cross-node integration (.100 ↔ .1) | Both nodes |

## How this directory is organised

| File | Audience | Purpose |
|------|----------|---------|
| `README.md` | All | Campaign index (this file) |
| `setup-reproducible-env.md` | Engineering | Step-by-step environment reproduction recipe |
| `track-a-nhi-results.md` | Engineering | Track A raw results (per-phase) |
| `track-b-a2a-results.md` | Engineering | Track B raw results (per-domain) |
| `track-c-postgres-age-results.md` | Engineering | Track C raw results (per-test) |
| `track-d-cross-node-results.md` | Engineering | Track D raw results (per-scenario) |
| `audience-non-technical.md` | End users | Plain-English summary of what works + what doesn't |
| `audience-c-level.md` | Executives | Business framing: risk, cost, time-to-ship, ROI |
| `audience-engineering.md` | Engineers | Detailed findings + reproduction + recommendations |
| `final-verdict.md` | All | Ship / fix-first / hold verdict with evidence pointers |

## Reproducibility contract

Every test result in this campaign meets these criteria:

1. **Pinned binary** — exact git SHA + build profile recorded per phase.
2. **Pinned dependencies** — Cargo.lock + Cargo.toml SHAs committed with the test artifacts.
3. **Pinned external services** — PostgreSQL version + AGE extension version + ollama / xAI model strings recorded.
4. **Pinned data** — fixture SHAs or generation seeds recorded; nothing depends on random untracked state.
5. **Pinned environment** — env vars, hostnames, IPs, OS versions captured at test time.
6. **Re-runnable shell recipes** — every test produces a `repro.sh` that re-runs the same scenario from a fresh shell.
7. **Per-result commits** — each finalized phase/track lands as its own git commit so the GitHub log shows the campaign progression.

## How to peer-review

A reviewer should be able to:

1. Clone `alphaonedev/ai-memory-mcp` at the recorded SHA.
2. Read `setup-reproducible-env.md` end-to-end and have a working Node A + Node B.
3. Run any `repro.sh` and observe matching results.
4. Read the audience-appropriate writeup for their role.
5. Disagree with any finding and follow the evidence pointer to the raw artifact.

## Hard rules during the campaign

Inherited from `ai-memory/v0.7.0-nhi-testing` playbook hard rules and CLAUDE.md:

- **No code fixes mid-phase.** Findings are filed as GitHub issues with the `auto-filed-by-agent` label and queued. The track-and-fix global rule (memory id `71ecce23-611b-4984-962d-d37c4309f261`) requires every finding to reach a tracker entry.
- **No memory modifications outside the `ai-memory/v0.7.0-nhi-testing` namespace** (or the campaign-specific namespaces below).
- **No PR #820 merge, no release/v0.7.0 merge, no tag cut, no Homebrew/COPR/GHCR/crates.io publish.** All shipping decisions remain human-gated.

## Memory namespace convention

| Track | Namespace | Title pattern |
|-------|-----------|---------------|
| A | `ai-memory/v0.7.0-nhi-testing` | `NHI-P{N}-{name}-2026-05-17` |
| B | `_v070_grand_slam/a2a_campaign/wave5-2026-05-17` | `A2A-W5-{domain}-{scenario}-2026-05-17` |
| C | `_v070_grand_slam/postgres_age_campaign/2026-05-17` | `PGAGE-{phase}-2026-05-17` |
| D | `_v070_grand_slam/cross_node_campaign/2026-05-17` | `XNODE-{scenario}-2026-05-17` |
| Verdict | All four above + `ai-memory/v0.7.0-nhi-testing` | `v0.7.0 — Full-spectrum verdict (2026-05-17)` |

## Provenance

| Item | Value |
|------|-------|
| Campaign start | 2026-05-17 |
| Operator | binary2029@gmail.com (justin@alpha-one.mobi) |
| Authoring agent | Claude (Opus 4.7 1M context) |
| Authority | Autonomous execution authorized by operator while traveling |
| Source handoff | `.local-runs/handoff-prompt-next-session-2026-05-17.md` (in this repo) |
| Global track-and-fix rule | memory `71ecce23-611b-4984-962d-d37c4309f261` (namespace `global/policies`) |
| PR under test | #820 (`local/install-815-816` → `release/v0.7.0`) with 4 fix commits landed 2026-05-17 (#822, #823, #824, #825) |

🤖 Drafted by Claude (Opus 4.7 1M context).
