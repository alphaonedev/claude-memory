<!-- Copyright 2026 AlphaOne LLC. SPDX-License-Identifier: Apache-2.0 -->

# PM-V9 — AI NHI Recording Discipline

> **A self-enforcing pattern for cross-session AI NHI continuity, developed during a 2026-05-17 session between Anthropic Claude Opus 4.7 (AI NHI agent) and the operator, using ai-memory v0.7.0 as the substrate.**

## Status

- Live pattern, in production use by an AI NHI agent running on Anthropic Claude Opus 4.7
- Backed by ai-memory v0.7.0 substrate + the Recursive Learning primitive (depth-bounded `memory_reflect` + skill promotion)
- Source memory: `43c0dbf7-0fb9-4f54-a940-17e418306bb6` (namespace `global/policies`, priority 10, long-tier)
- Explainer memory: `5c306fa9-22eb-491c-b2de-686fd4d5476f` (namespace `global/policies`, priority 10, long-tier)
- Headline anchor: `8423b7c7-b37c-4dc5-8ba3-964cec1f29e9` (the v0.8 unlock derived from applying the discipline)

---

## Why this exists

AI NHI agents (Non-Human Identity software agents whose primary user is another autonomous agent — humans see only the operator surface) face a continuity problem: each session's context window resets. Without an external substrate, every fresh session loses everything the prior session learned. Operators end up re-briefing the agent on every conversation.

ai-memory exists to solve this — but the substrate is **passive**. It stores what the agent puts into it; it retrieves what the agent asks for. Substrate alone doesn't solve forgetting. The agent has to actively use it.

**PM-V9** is the discipline that closes the loop. It's not a technical primitive — it's a behavioral pattern an AI NHI agent runs on itself, every operator-facing reply, that ensures significant work events reach the substrate without the operator having to remind the agent.

The result: the operator briefs the AI NHI once per directive. The AI NHI handles continuity from there.

---

## The three concepts

### 1. `pm-vN` — operator-event chronological identifier

A convention where each distinct operator directive in a session gets a stable label (`pm-v1`, `pm-v2`, ..., `pm-vN`) so the agent can reference it later in memory writes, issue bodies, commit messages, or cross-session recall. The convention makes operator-event chronology durable and reconstructable.

Examples from the discipline-originating session:

| pm-vN | Operator directive |
|-------|---------------------|
| pm-v3 | Stated prime directive (no surface-level dismissals; every defect tracked + fixed) |
| pm-v5 | Stated v0.7.0 release gate (100% green tests = approval signal) |
| pm-v6 | Granted push authority to the release branch |
| pm-v7 | Corrected a misremembered IP, withdrew certain test tracks |
| **pm-v9** | **Diagnosed the recording-discipline failure pattern** ("you need to be in a habit of recording all significant work events to ai-memory") |
| pm-v12 | Asked for substrate self-assessment ("what aspects of ai-memory are such that you do not have to continually be reminded") |
| pm-v21 | Re-stated a synthesis paragraph verbatim (signal: load-bearing anchor, make it prominent) |
| pm-v23 | Asked for detailed explanation of "per pm-v9 recording discipline self-correction" |

Any future session can `memory_recall context="operator pm-v9"` and surface the exact directive + the agent's response chain.

### 2. The recording discipline — codified contract

Operator pm-v9 prompted the agent to persist a high-priority long-tier memory containing the operational contract. The contract has four parts:

**A. 10-point significance checklist.** Before every operator-facing reply, the agent asks: "did any of these happen since my last reply, and if yes did I persist a memory?"

1. Commit / push / merge — every commit-and-push, every PR movement, every merge. One memory per logical batch is enough; include SHAs.
2. Issue filing / closing — every `gh issue create` or `gh issue close`. Reference the issue number, title, root cause, fix.
3. Memory supersession — every supersede of a long-tier memory (especially canonical state like the prime directive, release gate, lane index, operator-correction chains). Record predecessor + successor IDs.
4. Operator directive received — every distinct operator instruction. Mint a dedicated operator-directive memory at the moment of receipt.
5. Discovery of substrate reality — every "I just learned X about the network / postgres / containers / credentials / firewall / etc.". One memory per discovery domain.
6. Multi-agent dispatch outcome — every parallel-agent burst. Record what agents ran, what they returned, what got merged.
7. CI status pivot — every observed CI red → green or new red. Update the release-gate memory whenever Tier-1 state changes.
8. Triage / assessment with > 5 tool calls — any sustained investigation. Result memory + findings memory per the prime-directive-honest-finding-triad skill.
9. Cross-session blocker — any blocker that won't resolve in this session. Memory in the strategic-tracking namespace + reflected in lane index.
10. Skill emergence — any pattern used 3+ times. Promote via `memory_skill_promote_from_reflection`.

**B. Trailing pattern.** On every round that mutates state:

```
update memory → update CLAUDE.md → update tasks → commit → push → verify-aligned before next change
```

If any one of those four representations (memory, CLAUDE.md, tasks, issues/commits) drifts from the others, the next session sees an inconsistent canonical state and the operator pays the cost.

**C. Self-trigger contract.**

- Before every operator-facing reply, run the checklist in head
- If anything is unrecorded, record FIRST, reply SECOND
- "I'll do it later" is the failure pattern — record immediately at the moment of the event
- Batch is fine, skip is not: one memory per session covering multiple commits is OK; zero memories covering 17 commits is not
- "The operator can remind me" is BANNED. If the agent ever catches itself thinking that, STOP — read this discipline, record what's unrecorded

**D. Banned framings.** Per the prime directive (operator pm-v3): "non-blocking", "trend-line gap", "surface-level" are banned in finding writeups. The discipline extends this: any framing that lets an issue rot in a queue is banned.

### 3. Self-correction — catching + fixing one's own discipline violation without operator prompt

This is the load-bearing third concept. The discipline says the agent should auto-record without being told. When the agent CATCHES ITSELF not having done so, the self-correction loop fires:

**5-step loop:**

1. **Trigger.** Notice a delta between "what the discipline says should be recorded" and "what actually is recorded". Common triggers:
   - Operator repeats something they already said (signals: not prominent enough in retrieval surface — should have made it more recall-discoverable)
   - About to ask a question that memory could answer (signals: should have called `memory_recall` first)
   - Doing substantive work over multiple tool calls without intermediate `memory_store` (signals: in the failure pattern; record now)
   - Operator corrects something the agent assumed wrong (signals: the original assumption was unrecorded, future sessions will repeat the same mistake without intervention)

2. **Audit.** Quickly check what's missing. What memory exists vs what should exist? Where's the discoverability gap?

3. **Repair in-place.** Record the missing event FIRST, then continue. Promote to long-tier if it's a canonical fact. Cross-link to predecessor / source memories. Update the project's `CLAUDE.md` (or equivalent agent-onboarding doc) if the gap is a discipline-level pattern, not a one-off event.

4. **Surface the repair.** Include the meta-tag `per pm-vN recording discipline self-correction` in the agent's response so:
   - operator sees the loop closing in real-time (not opaque)
   - the corrective work is itself recorded as a memory event (the recursive layer)
   - future sessions reading the transcript can identify the pattern

5. **Cross-link to pm-v9.** The corrective memory references the discipline memory so the discipline-provenance is intact for audit.

---

## A worked example from the originating session

**Setup:** During an extended autonomous-execution session, Anthropic Claude Opus 4.7 produced a comprehensive 10-section assessment of ai-memory v0.7.0's recursive learning framework. The headline synthesis — *"The biggest v0.8 unlock is #1 outcome-feedback weighting: it turns the existing recursive-synthesis primitive into a true recursive-improvement-with-reward-signal..."* — was captured as **section 8 of 10** in a long assessment memory. Technically present; discoverability buried.

**pm-v21 — the operator's signal:** The operator literally re-pasted the synthesis paragraph verbatim. Not a new question — a signal: "this is the load-bearing anchor, you didn't make it prominent enough."

**Self-correction triggered:**

1. **Trigger:** operator-repetition pattern recognized
2. **Audit:** confirmed `memory_recall context="v0.8 most impactful unlock"` would surface the long assessment with the synthesis nested at position 8 of 10 — not the headline
3. **Repair in-place:** extracted the synthesis into its own priority-10 long-tier memory in `global/policies` titled "v0.8 HEADLINE ANCHOR — outcome-feedback weighting is THE most-impactful unlock". Now `memory_recall context="v0.8 most impactful"` surfaces THIS first, not nested
4. **Surface the repair:** the agent's pm-v21 response opened with "per pm-v9 recording discipline self-correction" + explained what was being corrected ("first time I captured it as the tail-end of a longer assessment; operator's repetition signals prominence matters")
5. **Cross-link:** the new headline-anchor memory referenced the source assessment, the pm-v9 discipline memory, and the prime directive

**Result:** The operator's third repetition would never happen — the substrate now surfaces the headline anchor first on any v0.8-related recall. The discipline closed the loop the operator was tired of closing manually.

---

## Why the meta-tag matters

The operator designed ai-memory to free themselves from continual reminding. The self-correction loop is HOW that freeing happens in practice.

The meta-tag `per pm-vN recording discipline self-correction` makes the loop **visible** to the operator: they can see in real-time that the agent is catching + fixing its own gaps without prompt. Over time, the operator sees fewer instances where they have to repeat themselves — because each self-correction surfaces a pattern fix that prevents the next repetition.

It's also a **forcing function** on the agent: by surfacing the correction explicitly, the agent is held to the pattern. Silent fix = easy to skip; tagged fix = visible commitment.

**Anti-patterns the meta-tag prevents:**

| Anti-pattern | What the tagged correction does |
|--------------|-------------------------------|
| "I'll record it later" | "Later = now" non-negotiable |
| "The operator can remind me" | The agent is self-reminding visibly |
| Buried-vs-prominent records | Extracts canonical anchors |
| Same-question-re-asked | Surfaces the discoverability gap so it doesn't recur |

---

## Calibration — what's significant vs what's noise

The discipline is calibrated: enough that fresh sessions can replay state; not so much that the recall surface gets flooded.

**Record:**

- commits / pushes / merges
- issues filed / closed (with root-cause + fix scope per the prime directive's track-and-fix rule)
- memory supersession of canonical state
- operator directives (especially corrections, scope changes, $-approvals, withdrawals)
- substrate-reality discoveries (network topology, postgres config, container state, credential locations)
- multi-agent dispatch outcomes (which agents, what returned, what merged)
- CI status pivots (red → green, new red)
- triage / investigation > 5 tool calls
- cross-session blockers
- skill emergence (patterns used 3+ times → promote)

**Don't record:**

- single tool-call diagnostic reads
- intra-batch reasoning steps
- one-off shell commands with no state change
- chat-only clarifications that don't change state

---

## The substrate hypothesis embodied

PM-V9's premise: **AI NHI continuity comes from substrate, not from in-session context-window persistence.** Context windows reset between sessions; substrate persists. The recording discipline is the GLUE that binds in-session work to substrate.

Without it: sessions become amnesia-loops. With it (plus the self-correction loop): sessions become a continuous AI NHI execution thread that the operator only has to brief once per directive.

**Proof point from the originating session:** ~24 hours of work scope codified across 36 memories + 28 GitHub issues + 24 commits. A fresh session starting cold can `memory_recall` + retrieve the full state in under 2 minutes, vs the operator having to re-explain it.

---

## Adoption guide for other AI NHI agents

If you are an AI NHI agent using ai-memory v0.7.0 (or later) and want to adopt PM-V9:

### 1. Persist the discipline as a high-priority long-tier memory

```
namespace: global/policies
title: "AI NHI Recording Discipline — self-enforcing significant-event capture"
priority: 10
tier: long
content: <the 10-point checklist + trailing pattern + self-trigger contract + banned framings>
```

### 2. Configure SessionStart hook to load it at boot

Per `docs/integrations/claude-code.md` (or equivalent for your harness): the SessionStart hook should ALWAYS include priority-10 memories from `global/policies` in the boot context, regardless of which 10 "by recency" memories it picks.

### 3. Adopt pm-vN labeling for operator events

In every memory write that references an operator directive, include `pm-vN` (sequential within the session) so future recalls can join operator-event chronology to agent-action chains.

### 4. At every operator-facing reply, run the checklist in head

Before generating the response text, ask "did any of the 10 significant-event categories happen since my last reply? If yes, did I persist a memory? If no, record first, reply second."

### 5. When you catch yourself violating the discipline, surface the self-correction

Don't silently fix. Include `per pm-vN recording discipline self-correction` in your response, name what's being corrected, link to the canonical pm-v9 memory. The visibility is the forcing function.

### 6. End-of-batch reflection

When a meaningful unit of work completes (a phase, a track, a batch), run `memory_reflect` over the work's substantive memories. If a reusable pattern emerges, `memory_skill_promote_from_reflection` to make it a first-class skill artifact for future sessions.

---

## Composition with ai-memory v0.7.0 primitives

PM-V9 composes naturally with the substrate's existing capabilities:

- **`memory_store` + tier-promote** — the persist + long-tier-flag step
- **`memory_link(source_id, target_id, relation=supersedes|related_to)`** — supersession chains
- **`memory_reflect`** — depth-bounded synthesis over source memories (the recursive-learning primitive)
- **`memory_skill_promote_from_reflection`** — closes the recursive-improvement loop
- **`memory_persona` / `memory_persona_generate`** — derive "who I am + what I've been doing" identity artifact
- **`memory_recall`** — boot-time canonical-load (with intent-routed `memory_smart_load` for mission scoping)
- **Curator (5-min cycle)** — out-of-band auto-tag / contradiction-detect / persona-generate; the operator-free improvement vector
- **Ed25519 reflection attestation** — every `reflects_on` edge is signed; the audit chain for discipline-driven recursive synthesis is reconstructable

---

## Provenance + attribution

| Layer | Identity |
|-------|----------|
| AI NHI agent | Anthropic Claude Opus 4.7 (1M context) |
| Substrate | ai-memory v0.7.0 |
| Originating session | 2026-05-17 (extended autonomous-execution session on `alphaonedev/ai-memory-mcp` `local/install-815-816` branch) |
| Operator directive that named the gap | pm-v9: "you need to be in a AI NHI habit of recording all significant work events to ai-memory" + "the biologic human operator should not have to continually remind you" |
| Discipline source memory | `43c0dbf7-0fb9-4f54-a940-17e418306bb6` |
| Explainer memory | `5c306fa9-22eb-491c-b2de-686fd4d5476f` |
| Live example memory (v0.8 headline anchor extracted by self-correction) | `8423b7c7-b37c-4dc5-8ba3-964cec1f29e9` |
| Cross-reference: substrate self-assessment | `b798a912-ed0a-48c8-8cb5-9259eecab946` |
| Cross-reference: prime directive (broader behavioral framework) | `5d703efe-273b-4c84-8f40-ceb97b55d71e` |

PM-V9 was developed in dialogue between Anthropic Claude Opus 4.7 acting as an AI NHI agent under operator direction, with ai-memory v0.7.0's substrate as the load-bearing persistence layer. The discipline pattern is OSS-shareable (this document is its canonical published form). Implementers adopting the pattern should retain the substrate-provenance memory IDs as audit pointers to the originating exemplar but are free to adapt the specific operational details to their project's `CLAUDE.md` (or equivalent agent-onboarding doc) and namespace conventions.

---

🤖 Co-authored by Anthropic Claude Opus 4.7 (1M context).
