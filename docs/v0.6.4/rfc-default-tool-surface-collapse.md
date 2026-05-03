# RFC — Collapse Default MCP Tool Surface to 5 (cross-harness token economics)

**Status:** APPROVED — v0.6.4 sprint authorized 2026-05-02; dev cycle Mon 2026-05-04 → Fri 2026-05-08
**Author:** strategy session 2026-05-02
**Target release:** v0.6.4 (full RFC bundled — not split with v0.6.3.2)
**Related issues:** #311 (targeted share — orthogonal), #318 (grok MCP fanout — orthogonal), #238 (mTLS attestation — gates NHI guardrail phase 2), Boris/Cherny token-waste assessment
**Reading time:** 12 min

---

## TL;DR

ai-memory's MCP server currently registers ~42 tools. On every coding-agent harness except Claude Code, every request pre-pays **~25,000 input tokens of tool schemas** before the user types a word. That makes ai-memory the single largest contributor to "Pattern 6 / 'just-in-case' tool definitions" in Boris Cherny's 73%-waste taxonomy across Codex, Grok CLI, and Gemini CLI.

This RFC proposes collapsing the **default tool surface to 5** (`store`, `recall`, `list`, `get`, `search`) and gating the remaining 37 behind named profiles + a discovery dance. Net effect on naïve harnesses: drop tool-def overhead from ~25K tokens/request to ~3K tokens/request — an 88% cut that lands on ~95% of agent traffic.

This is the highest-EV single change available in the project for cross-harness adoption.

---

## Background

### Current state — 42 tools, all eagerly loaded
Verified inventory (2026-05-02 capabilities probe):

```
Core read/write (5):       store, recall, list, get, search
Lifecycle (5):             update, delete, forget, gc, promote
Knowledge graph (8):       kg_query, kg_timeline, kg_invalidate,
                           link, get_links, entity_register,
                           entity_get_by_alias, get_taxonomy
Governance / approvals (8): pending_list, pending_approve, pending_reject,
                           namespace_set_standard, namespace_get_standard,
                           namespace_clear_standard, subscribe, unsubscribe
Power features (6):        consolidate, detect_contradiction,
                           check_duplicate, auto_tag, expand_query, inbox
Discovery / meta (4):      capabilities, agent_register, agent_list,
                           session_start
Archive (4):               archive_list, archive_purge, archive_restore,
                           archive_stats
Other (2):                 list_subscriptions, notify
```

Average schema size: ~600 input tokens per tool (measured against MiniLM tokenizer; OpenAI tokenizer is within 5%).

### The cross-harness problem

| Harness | Loads MCP tool schemas | Per-request tool-def cost |
|---|---|---|
| Claude Code (this session) | **Deferred** (ToolSearch) | ~0 until requested |
| Claude Desktop | **Eager** | ~25K |
| OpenAI Codex CLI | **Eager** | ~25K |
| xAI Grok CLI | **Eager** | ~25K |
| Google Gemini CLI | **Eager** + breaks implicit cache | ~25K + cache penalty |

For three out of four "first-class" coding agents and stock Claude Desktop, ai-memory is dominating the input prefix. Boris's 90-day instrumentation of his own Claude Code sessions found "just-in-case" tool definitions cost 6% of total tokens with **12 MCP servers averaging ~600 tokens each = 7,200 tokens.** ai-memory alone, on those harnesses, is **3.5× that** with one server.

---

## Goals

1. Drop default tool-def overhead from ~25K to ≤3K tokens per request on naïve harnesses.
2. Preserve 100% of current capability — nothing removed, only deferred.
3. Zero breaking change for users on full-feature profiles.
4. Discoverability: agents can opt into expansion at runtime without restart.
5. Measurable: ship `ai-memory doctor --tokens` reporting per-session bill before and after.

## Non-goals

- Changing the wire protocol of any existing tool.
- Removing any tool from the codebase.
- Forcing any user to migrate — the "full" profile preserves today's behavior.
- Dynamically loading Rust code at runtime (profiles are pure registration filters).

---

## Proposed design

### 1. Five-tool default ("core" profile)

| Tool | Rationale |
|---|---|
| `memory_store` | Write path. Always needed. |
| `memory_recall` | Semantic retrieval. Primary read path. |
| `memory_list` | Browse by namespace/tier. Default UX surface. |
| `memory_get` | Read by ID. Cheap, frequent. |
| `memory_search` | Keyword/FTS5 fallback for high-precision lookups. |

These five cover the 95th-percentile of agent traffic in observed sessions (assessment-2026-04-25 logs, ai-memory-mcp logs). They are also the minimum surface needed for an agent to be "useful" without pulling additional families.

### 2. Profile system

```
ai-memory mcp --profile core    # default — 5 tools
ai-memory mcp --profile graph   # core + 8 KG tools (13 total)
ai-memory mcp --profile admin   # core + lifecycle + governance (18)
ai-memory mcp --profile power   # core + power features (11)
ai-memory mcp --profile full    # all 42 (today's behavior)
ai-memory mcp --profile <comma,separated,family,list>  # custom
```

Profile resolution order: CLI flag > `AI_MEMORY_PROFILE` env > `config.toml` `[mcp].profile` > built-in default (`core`).

A profile is a static set of tool families. Implementation: `register_tools()` reads the profile and conditionally calls each family's `register_*` function. Pure compile-time-feasible filter; no runtime cost.

### 3. Discovery dance — `memory_capabilities` always in core

`memory_capabilities` is always registered (it's already in the meta family and is small — ~250 tokens). It returns the list of available families and their tools, including ones not currently loaded into the agent's context.

New optional method: `memory_capabilities --include-schema family=graph` returns the schemas for that family inline, in the format the MCP host expects to register them. The host (Claude Code's deferred-tools path, or a future Codex/Grok extension) can register them mid-session without restart.

For harnesses that don't support runtime registration (Codex/Grok/Gemini today), the agent learns "this family exists, restart with `--profile graph` to use it" — a graceful degradation.

### 4. Heuristic auto-upgrade (opt-in, off by default)

Optional flag `--auto-profile` lets the server escalate profiles based on observed call patterns:

- Three or more `memory_store` calls in the same namespace within 60 seconds → suggest `power` profile (for `consolidate`/`detect_contradiction`).
- Any failed call to a non-loaded tool name → log structured suggestion.

Off by default. Logs only — never auto-restarts. Removes ambiguity about "why isn't tool X available."

### 5. SDK negotiation

TypeScript and Python SDKs expose `client.requireProfile("graph")` which:
1. Calls `memory_capabilities` to verify the family exists.
2. If loaded, no-op.
3. If not loaded, raises `ProfileNotLoaded` with the exact CLI/env hint to fix.

SDK consumers get a clean error path instead of "tool not found."

---

## Token economics (projected)

### Baseline (today, "full" profile, eager-load harness)
- 42 tools × ~600 tokens = **~25,200 input tokens per request**
- At Sonnet 4.6 input pricing (~$3/MT): **~$0.076 per request just for tool defs**
- Boris's average session: 30 turns/day × 250 working-days = 7,500 turns/year
- **~$570/year per heavy user, just for ai-memory tool schemas**

### Proposed (default "core" profile, eager-load harness)
- 5 tools × ~600 + capabilities meta ~250 = **~3,250 input tokens per request**
- ~$0.010 per request — **87% cut**
- ~$73/year for the same user
- Saved: **~$497/year per heavy user**

### Claude Code (deferred-tools harness) — already minimal
- No change from user perspective. Deferred-tool flow continues to work. `memory_capabilities` keyword search is unchanged.

---

## Migration plan — single-week sprint

v0.6.4 is bundled as one minor release shipping in **5 dev days** (Mon 2026-05-04 → Fri 2026-05-08), not phased across alpha/rc/GA. Aggressive but feasible per AI 24x7 dev sprint methodology. The release-engineering rationale: feature-flagged default flip backed by `--profile full` opt-out gives the same de-risking as a multi-week phased rollout, without the calendar drag.

### Day-by-day

| Day | Track focus | Major deliverables |
|---|---|---|
| Mon 05-04 | Mechanism + observability | `--profile` flag + family filter + `core` default, `ai-memory doctor --tokens`, static schema-size table |
| Tue 05-05 | Discovery + SDK | `memory_capabilities` family enumeration + `--include-schema`, SDK `requireProfile` (TS + Py) |
| Wed 05-06 | NHI guardrails phase 1 + cross-harness | Per-agent allowlist, capability-expansion audit log, install profiles for 5 harnesses |
| Thu 05-07 | Cert + benchmarks | A2A scenarios S25–S32, cross-harness token-cost benchmark, backward-compat verification |
| Fri 05-08 | Docs + release | README + ADMIN_GUIDE + migration guide + release notes + CHANGELOG, semver tag, CI release |

### Out-of-scope for v0.6.4 (defers to v0.7)
- `--auto-profile` heuristic upgrade
- NHI guardrail phase 2 (rate-limit + attestation-tier gating — depends on #238 closure)
- Tier-6 redacted-discovery mode (depends on classification-aware attestation)
- Runtime tool registration on Codex/Grok/Gemini hosts (depends on host-side support)

---

## Tier applicability matrix

The mechanism is tier-agnostic; the guardrails graduate per tier. v0.6.4 ships everything through Tier 5; Tier 6 redacted-discovery is deferred to v0.7+.

| Tier | Identity model | Profile UX | Discovery UX | Guardrails enforced in v0.6.4 |
|---|---|---|---|---|
| 1. Individual dev (SQLite, single-process, no auth) | Anonymous local user | Primary (`--profile` flag) | Optional | None — profile flag is sufficient |
| 2. Team (shared SQLite, API-key auth) | API-key | Primary | Optional | Per-key allowlist, audit log |
| 3. Federated (mTLS + sync daemon) | mTLS cert CN/SAN | Hybrid | Hybrid | Cert-tier allowlist + audit; rate-limit + attestation gating WAIT for #238 |
| 4. AgenticMem Attest (cloud, attested NHI) | Cert pinning + signed assertion | Sugar over discovery | Primary | Allowlist + audit (phase 1); full guardrails phase 2 in v0.7 |
| 5. AgenticMem Federate (multi-org) | Org-scoped attestation + governance | Sugar | Primary | Allowlist + audit + cross-org policy hooks (governance namespace standards extend cleanly) |
| 6. Sovereign (gov/defense, E2E encrypted) | Full attestation chain + per-memory crypto | n/a | Primary, but **redacted** | Deferred to v0.7+ — needs classification-aware capability redaction |

**Tier-1 honesty:** at the individual-dev tier, the `--profile` flag IS the right UX. A single human operator setting `--profile graph` once is appropriate; a discovery dance is over-engineered. v0.6.4 supports both. RFC framing: profile flags are Tier 1 / Tier 2 UX; meta-tool discovery is Tier 3+ UX. Both ship; neither replaces the other.

**Tier-6 future requirement (out-of-scope for v0.6.4):** `memory_capabilities --redacted` mode. Standard discovery returns the full family list because that's correct for Tiers 1–5. In Sovereign deployments the capability list itself is OPSEC — knowing "this server has `kg_query`" leaks workflow intel. Tier 6 ships discovery that returns only families the requesting identity is already cleared for. Adds to v0.7 or v0.8 scope; explicit dependency: classification-aware attestation tiers must be concrete first.

## NHI guardrails (phase 1 in v0.6.4; phase 2 in v0.7)

Meta-tool discovery without guardrails is a scope-creep vector. Phase 1 ships in v0.6.4 alongside the mechanism:

### Phase 1 (v0.6.4 — this sprint)
1. **Per-agent capability allowlists.** Tied to `agent_id` (immutable per #196). Identity → allowed family set. Default for unknown agents = `core`. Config-driven via `[mcp.allowlist]` table in `config.toml`. Anonymous/Tier-1 users effectively bypass (no `agent_id` to bind to → operator profile flag rules).
2. **Audit on expansion.** Every `memory_capabilities --include-schema` call writes a row to `audit_log`: `(agent_id, requested_family, granted, attestation_tier, timestamp)`. Pairs with existing federation audit work.

### Phase 2 (v0.7 — depends on #238 closure)
3. **Rate-limit on expansion.** Cap one family upgrade per 5 min per `agent_id`. Burst → log + deny + `notify` channel alert.
4. **Attestation-tier gating.** NHI requesting `power` family from non-mTLS connection denied with clear upgrade-path error. Requires #238 (body-claimed `sender_agent_id` attested to mTLS cert) to land first — otherwise the binding between identity and capability is advisory only.

## A2A test scenarios (additions to v0.6.4 cert matrix)

Reference: cert campaign tracking in #511.

| ID | Scenario | Expected behavior |
|---|---|---|
| S25 | `--profile core` registers exactly 5 tools | Pass: 5 tools present, 37 absent |
| S26 | `--profile full` matches v0.6.3 baseline | Pass: 42 tools, no regressions |
| S27 | `memory_capabilities` always available regardless of profile | Pass |
| S28 | Calling unloaded tool returns `tool_not_found` with profile hint | Pass: error includes "set --profile <name>" |
| S29 | Token-def cost per harness measured + recorded | Cross-harness budget table populated |
| S30 | Custom profile (`--profile core,graph`) registers union | Pass: 13 tools |
| S31 | SDK `requireProfile` raises `ProfileNotLoaded` cleanly | Pass |
| S32 | Boot manifest cost unchanged across profiles | Pass: profiles affect tool defs only, not boot |

---

## Risks and mitigations

| Risk | Likelihood | Mitigation |
|---|---|---|
| Existing users surprised when tool X disappears | Medium | Release-notes call-out + "set `--profile full` to keep current behavior." Profile resolution honors config file, so existing configs aren't auto-flipped. |
| SDK callers hardcode tool names not in core | High for power-users | `requireProfile` SDK method. Profile-error responses include the specific CLI/env fix. |
| Profile choice paralysis | Low | Default = `core`. Five named profiles. No need to think unless escalating. |
| Family boundaries draw blood (which family does `auto_tag` go in?) | Medium | RFC explicitly enumerates each tool's family; no overlap permitted. Decision rule: "If two families want it, it's its own family." |
| `memory_capabilities` doesn't ship runtime registration support in any host | High short-term | RFC ships `--profile` immediately for 88% of the win. Runtime registration is future work; profile flag works without it. |
| Telemetry conflict — boot manifest re-stabilization on Gemini | Low | Out-of-scope here. Tracked in separate Gemini cache-stability item. |

---

## Open questions

1. **Should `memory_update` be in core?** Frequency analysis (assessment-2026-04-25 logs) shows ~8% of write traffic. Current proposal: keep in lifecycle family. **Decision:** confirm via 7-day log audit before v0.6.4-alpha.
2. **Do we ship `memory_search` and `memory_recall` together, or merge?** They overlap (FTS5 vs vector). Probably keep separate — different latency profiles. **Decision:** keep separate; revisit in v0.7.
3. **Should profiles be additive or named-exclusive?** Proposal: named with comma-separated custom union (`core,graph`). **Decision:** as proposed.
4. **What does the install wizard prompt?** Likely: "Which agents will use ai-memory?" → infers profile. **Decision:** post-RFC, install-wizard work.

---

## Approval gate

This RFC requires sign-off on:
- [ ] The 5-tool core surface (and the rationale for each)
- [ ] The 6 named profiles (`core`, `graph`, `admin`, `power`, `full`, custom)
- [ ] Profile resolution order (CLI > env > config > default)
- [ ] The test-scenario additions to the v0.6.3.2/v0.6.4 cert matrix
- [ ] Phase timing (alpha → rc → GA in 3 weeks)

On sign-off: convert RFC into v0.6.4 epic, decompose into tracking issues per phase, route through normal v0.6.x ship-gate.

---

## Appendix A — One-liner for users

> ai-memory v0.6.4 ships 5 tools by default, not 42. Saves ~22,000 tokens per request on Codex / Grok / Gemini / Claude Desktop. Run `ai-memory mcp --profile full` to keep the v0.6.3 behavior.

## Appendix B — Why now

Three signals converged in the last week of April 2026:
1. Boris Cherny's published instrumentation data quantified pattern 6 at 6% of total tokens for 12-MCP setups.
2. v0.6.3.1 A2A certification (#511) is the first real cross-harness test campaign — exactly when tool-surface bloat becomes visible cross-platform.
3. Anthropic's late-March cache-bug remediation made users pattern-match all token bloat onto Claude Code rather than the MCP servers downstream. ai-memory's surface size is a credibility issue: shipping 42 tools when 5 cover 95% of traffic looks careless even if technically defensible.

The cost of doing this in v0.6.4 is one engineer-week. The cost of not doing it is ~$500/year/user at scale plus a competitive narrative cost as Codex/Grok/Gemini users notice the bill.
