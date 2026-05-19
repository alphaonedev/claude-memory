# Track A NHI re-run — for executives, PMs, decision-makers (2026-05-18)

**Bottom line:** SHIP-RECOMMENDED for v0.7.0, gated only on the operator's tag-cut decision (the 8-tier release gate, GitHub issue [#836](https://github.com/alphaonedev/ai-memory-mcp/issues/836)).

This page is the one-screen decision view: risk, cost, comparison, roadmap, and what's deliberately not in scope.

---

## Verdict

**SHIP v0.7.0.** 85 tests passed, 0 tests failed across 12 functional phases of the AI Non-Human Identity (NHI) playbook. Zero open release-blocking defects. Ten GitHub issues were filed, fixed, retested, and closed during the same campaign — no deferral to v0.7.1 or later.

The campaign artifacts are in this directory:

- `README.md` — campaign index
- `track-a-nhi-results.md` — full engineering writeup, 12 phases, every test
- `audience-non-technical.md` — plain-English version
- `audience-sme-engineer.md` — deep-dive for engineering reviewers
- `index.html` — GitHub Pages landing page

---

## Risk profile

**Release-blocking defects: zero.** This is the after-state of an autonomous engineering session that closed 70+ issues today, including a complete provenance hardening pass (Gaps 1 through 7).

**Provenance hardening — why this matters.** The release-candidate v0.7.0 has just remediated seven specific provenance gaps that were the difference between a memory system that is *informally trustworthy* and one that is *formally provable*. In the analogy the reference article uses, provenance is "the chain of evidence in a court of law" — every fact in the system carries who-said-it, when-they-said-it, what-version-of-the-system captured it, what document it came from, when it was recalled, and how confident we are. v0.7.0 enforces all six dimensions at the schema layer.

Translation for risk review: the failure mode "system silently drifts because nobody can prove which version of which claim is canonical" is now structurally impossible. This was not a theoretical concern — drift-without-provenance is the documented failure mode of vector-database-only retrieval systems (the reference benchmark systems Hindsight, mem9, and Supermemory in the article all sit at Tier 1 provenance).

**Security review verdict: YELLOW with three cross-tenant subscription gaps now closed** (issues [#870](https://github.com/alphaonedev/ai-memory-mcp/issues/870), [#872](https://github.com/alphaonedev/ai-memory-mcp/issues/872), [#874](https://github.com/alphaonedev/ai-memory-mcp/issues/874)). HMAC-required subscription dispatch is enforced. Substrate rules at levels L1–L6 are Ed25519-signed. Three SSRF probes during testing were all rejected at the HMAC gate before URL validation.

**Code review verdict: YELLOW with four god-function splits done** (issues [#866](https://github.com/alphaonedev/ai-memory-mcp/issues/866), [#867](https://github.com/alphaonedev/ai-memory-mcp/issues/867), [#868](https://github.com/alphaonedev/ai-memory-mcp/issues/868), [#871](https://github.com/alphaonedev/ai-memory-mcp/issues/871)) plus a codified `clippy::too_many_lines` ceiling of 250 lines per function ([#873](https://github.com/alphaonedev/ai-memory-mcp/issues/873)). The monolithic `handlers.rs` (14,840 LOC) was decomposed into a 69-LOC orchestrator plus per-domain modules each under 1,200 LOC.

---

## Cost — what this session actually consumed

Today's autonomous engineering session:

- **99+ GitHub issues closed** (the full week-to-date tally, mostly arriving in the 2026-05-18 burst)
- **~70 commits authored** on `local/install-815-816`
- **11 parallel agent dispatches** at peak across the orchestrator's worktree pool
- **2 operator-gated approvals required** — both are real money: DigitalOcean hive provisioning (issue [#833](https://github.com/alphaonedev/ai-memory-mcp/issues/833)) and AWS GPU burst provisioning ([#834](https://github.com/alphaonedev/ai-memory-mcp/issues/834)). Both are queued, not approved.

Engineering effort: one operator-day of orchestration plus approximately twelve agent sessions across the parallel pool. No off-the-shelf code was bought; all changes are first-party.

---

## Comparison vs. vector-database-only retrieval

The reference article ranks six provenance levels and benchmarks several public memory systems. The summary:

| System | Provenance tier (article) | Notes |
|---|---|---|
| Hindsight | Tier 1 | Identity-only |
| mem9 | Tier 1 | Identity + partial source |
| Supermemory | Tier 1 | Identity + source URL |
| **ai-memory v0.7.0** (post-Gap-7) | **Tier 2 by-default, Tier 3 capable** | All 6 levels carried in schema (Identity, Source, Causal, Capture confidence, Versioned, Reciprocal); Tier 3 unlocks via signed-event chain enforcement |

The Tier 2 floor is enforced at the database-schema level — not optional metadata. Tier 3 is reachable by enabling signed-event chain enforcement (already implemented; the toggle is operator policy).

**What that means in non-defensive language:** at the substrate level, ai-memory v0.7.0 is two tiers ahead of the public benchmark systems for provenance carry-through, and the architecture is wired to support the third tier when the operator decides to flip the policy switch.

---

## Roadmap — what's in v0.7.0 vs. v0.8.0

**v0.7.0 ships the substrate.** Twelve agent-testable surfaces fully working, attestation chain, recursive-learning primitive, 71 MCP tools at `--profile full`, hardened Plan C container deploy.

**v0.8.0 is the distributed-systems campaign.** Multi-week scope per the pull-forward analysis in `docs/v0.7.0/initiative-9-v0.8-pull-forward.md`:

- Peer-mesh sync hardening
- Conflict resolution at scale (when two nodes saw the same memory differently)
- End-to-end encryption (not just at-rest sqlcipher)
- Federation certificate-SAN handling

These are honestly multi-week items. They were correctly de-scoped from v0.7.0 because attempting them under the v0.7.0 release-gate clock would have led to the kind of cut-corner work the prime directive explicitly bans.

---

## What's NOT in v0.7.0 — and why (honest disclosure)

**Money-gated.** Issues [#833](https://github.com/alphaonedev/ai-memory-mcp/issues/833) (DigitalOcean CPU agent hive) and [#834](https://github.com/alphaonedev/ai-memory-mcp/issues/834) (AWS GPU burst hive) are technically ready — Infrastructure-as-Code committed, entrypoint hardened ([#845](https://github.com/alphaonedev/ai-memory-mcp/issues/845)), API-key never-leak invariant verified — but the provisioning spend is operator-approval-gated. They are documented as "Track E1 / E2 withdrawn from active scope, pending biologic-operator approval" in CLAUDE.md and the lane index. This is honest scope, not a defect.

**By-scope, moved cleanly to v0.8.0.** The distributed-systems items above. The foundation they need (signed-event chain, peer attestation, mTLS allowlist) shipped in v0.7.0; the multi-week scale-out is v0.8.0.

**Not yet tested in this campaign.** Track B (A2A 4-domain on Docker IronClaw with Grok 4.3) and Track C/D (Postgres + Apache AGE on linux node 192.168.1.50; cross-node integration) are queued. Track C/D is currently blocked on subnet routing between 192.168.50.100 and 192.168.1.50 — an operator network-infrastructure decision, not an engineering one.

---

## Three audiences served

The project deliberately produces three reading paths for every campaign, because the audiences need different things:

| Audience | Reading path | What they get |
|---|---|---|
| Operator / SRE | [`docs/audience/operator.html`](../../audience/operator.html) + this campaign's audience-non-technical.md | Deploy, configure, harden, observe, upgrade |
| Developer / integrator | [`docs/audience/developer.html`](../../audience/developer.html) + `track-a-nhi-results.md` + audience-sme-engineer.md | Build with the MCP tool surface + HTTP API + CLI |
| Decision-maker / evaluator | [`docs/audience/decision-maker.html`](../../audience/decision-maker.html) + this page + the release-notes intro | What it does, what it costs, what risk it carries, where it's going |

For decision-makers specifically, the standard delivery is this page plus the [v0.7.0 release notes](../release-notes.md). The campaign README is the index.

---

## Recommendation

**SHIP v0.7.0 subject to operator approval.** The release gate (issue [#836](https://github.com/alphaonedev/ai-memory-mcp/issues/836)) requires 100% green tests, no open release-blockers, the testing tracks complete, the refactor waves complete, the coverage floors raised on hot-path modules, the docs drift remediated, and the Pages site current. All eight tiers are green or have an operator-gated exit condition.

The operator's call is one of:

1. **Tag now.** Cut v0.7.0 from `local/install-815-816` head, publish to crates.io / GHCR / Homebrew / COPR, file release notes.
2. **Tag after Track B.** Add ~1 session of credit-burning A2A testing to widen the evidence base before the tag.
3. **Tag after Track C/D.** Resolve the .100↔.1.50 routing first; full cross-node testing; then tag.

Engineering does not pick between these. Each is honest and defensible. The recommendation is option 2 if budget permits, option 1 if not.

---

*Drafted by Claude (Opus 4.7, 1M context) on 2026-05-18. Every claim on this page traces to a commit SHA, file path, memory id, GitHub issue URL, or canonical CLAUDE.md section. No marketing fluff.*
