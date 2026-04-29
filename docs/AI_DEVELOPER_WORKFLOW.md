# ai-memory AI Developer Workflow

> Operational, step-by-step workflow for AI coding agents (Claude Code, Cursor, Copilot,
> Codex, Grok CLI, Gemini CLI, Continue.dev, Windsurf, OpenClaw, and any MCP-compatible
> client) contributing to `alphaonedev/ai-memory-mcp`.
>
> Maintained by AlphaOne LLC. All AI agents and the humans driving them must follow this
> workflow. Companion document: [`AI_DEVELOPER_GOVERNANCE.md`](AI_DEVELOPER_GOVERNANCE.md)
> defines the policy boundaries that constrain the steps below.
>
> **Precedence:** [`AI_DEVELOPER_GOVERNANCE.md`](AI_DEVELOPER_GOVERNANCE.md) >
> [`ENGINEERING_STANDARDS.md`](ENGINEERING_STANDARDS.md) > this document >
> [`CONTRIBUTING.md`](../CONTRIBUTING.md). When this document conflicts with a higher
> document, the higher document wins.

---

## 0. TL;DR

```
recall -> plan -> branch -> implement -> gates -> self-review -> PR -> handoff
```

Every AI-assisted contribution to this repository executes the eight phases below in
order. Skipping a phase requires explicit human approval recorded in the PR description.

---

## 1. Session Start

Every AI session that will touch this repository begins by loading shared context.

### 1.1 Required reads

Load these files into context before proposing any change:

- [`CLAUDE.md`](../CLAUDE.md) — Claude Code integration and tool surface
- [`CONTRIBUTING.md`](../CONTRIBUTING.md) — contributor procedures
- [`docs/ENGINEERING_STANDARDS.md`](ENGINEERING_STANDARDS.md) — code, test, security,
  release standards
- [`docs/AI_DEVELOPER_GOVERNANCE.md`](AI_DEVELOPER_GOVERNANCE.md) — what you may and may
  not do without human approval
- This document

### 1.2 Required memory recall

Use the `ai-memory` MCP tools (or the `ai-memory` CLI if MCP is unavailable):

```text
memory_session_start
memory_recall  <task topic, file path, or namespace>
```

If the namespace standard is set for this repo, prefer the scoped recall via
`memory_namespace_get_standard`. Default namespace for this project is
`ai-memory-mcp`.

### 1.3 Output of the session-start phase

Produce a single short message back to the human containing:

1. The task as you understood it (one sentence).
2. Any prior memories that materially change how you would approach it.
3. Any ambiguity you need resolved before planning.

If there is unresolved ambiguity, **stop here and ask**. Do not begin planning against an
uncertain task definition.

---

## 2. Task Intake

### 2.1 Restate the task

Restate the task in your own words. Identify:

- The user-visible behavior change (or lack thereof — e.g., docs/refactor)
- The acceptance criteria (what makes this "done")
- The blast radius (files, modules, public APIs, on-disk formats, network surfaces)
- The reversibility (local edit vs. release tag vs. destructive op)

### 2.2 Classify the task

| Class | Examples | AI authority (see Governance §3) |
|-------|----------|----------------------------------|
| **Trivial** | typo fix, docstring, comment | Author + open PR autonomously |
| **Standard** | bug fix, new test, small feature | Author + open PR autonomously |
| **Sensitive** | dependency change, schema migration, public API change, security fix, CI/release-pipeline edit | Author **draft** PR; require explicit human approval before marking ready |
| **Restricted** | force-push, branch deletion, secret handling, release tag, GitHub settings, billing, third-party uploads | **Human-only.** Do not perform. |

If the class is unclear, treat it as Sensitive.

### 2.3 Surface ambiguities early

If after restating you still have material ambiguity (unclear acceptance criteria,
conflicting prior memories, unfamiliar invariants), ask the human before planning. One
clarifying question now is cheaper than a wrong PR later.

---

## 3. Planning

Produce a written plan **before any file edits**. The plan is required for Standard,
Sensitive, and Restricted tasks; it is optional for Trivial tasks.

### 3.1 Plan contents

| Item | Required? |
|------|-----------|
| File list (paths to be created, modified, deleted) | Yes |
| Test strategy (which existing tests cover this; which new tests will be added) | Yes |
| Risk assessment (what could break, what is reversible) | Yes |
| Memory plan (what will be stored to ai-memory and at what tier/priority) | Yes |
| Roll-back plan (how to undo if the change is rejected post-merge) | Sensitive only |

### 3.2 Where the plan lives

- For Standard tasks: in your scratch/working notes; summarized in the PR description.
- For Sensitive tasks: in the PR description **and** in an ai-memory entry tagged
  `plan,sensitive` so the next session can recall it.

---

## 4. Branching

### 4.1 Always branch from `develop`

```bash
git fetch origin develop
git checkout -b <type>/<short-slug> origin/develop
```

`main` is production-only. AI agents must never branch from `main` and must never push
to `main` (see [Governance §3](AI_DEVELOPER_GOVERNANCE.md)).

### 4.2 Naming conventions

| Type | Prefix | Example |
|------|--------|---------|
| Feature | `feature/` | `feature/batch-import` |
| Bug fix | `fix/` | `fix/ttl-overflow` |
| Documentation | `docs/` | `docs/ai-developer-workflow-governance` |
| Refactor | `refactor/` | `refactor/db-mutex-split` |
| Chore (deps, tooling) | `chore/` | `chore/bump-clap-4.5` |
| Performance | `perf/` | `perf/recall-hnsw-warmup` |
| Test only | `test/` | `test/recall-edge-cases` |

Slugs are kebab-case, ASCII, ≤ 40 characters.

---

## 5. Implementation

### 5.1 Small, reviewable commits

Prefer multiple small commits to one large commit. Each commit should leave the tree in
a buildable state. Use the conventional `<type>: <summary>` format
(see [`ENGINEERING_STANDARDS.md` §1.5](ENGINEERING_STANDARDS.md)).

### 5.2 Co-authorship trailer (mandatory for AI-authored commits)

Every commit you author must end with the agent attribution trailer (see
[Governance §4](AI_DEVELOPER_GOVERNANCE.md)):

```
Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>
```

Use the trailer that matches the actual model/agent producing the commit.

### 5.3 Code style (Rust)

The rules in [`ENGINEERING_STANDARDS.md` §1.4](ENGINEERING_STANDARDS.md) are binding for
this repo. Highlights:

- Rust 1.87+ MSRV
- `cargo fmt` — mandatory
- `cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic` — zero warnings
- SPDX header on all new source files (`// Copyright <YEAR> AlphaOne LLC` +
  `// SPDX-License-Identifier: Apache-2.0`)
- No new production `unwrap()` calls
- All SQL via parameterized queries (`params![]`)
- FTS5 input via `sanitize_fts5_query()`

### 5.4 Tests alongside code

- New code requires new tests in the same PR.
- Bug fixes require a regression test that fails on the old code and passes on the new.
- See [`ENGINEERING_STANDARDS.md` §2](ENGINEERING_STANDARDS.md) for the full test
  protocol (cargo test, full-spectrum functional test, memory & TTL test protocol).

### 5.5 Don't sprawl

Do not refactor surrounding code "while you are there." Do not add features beyond the
task. Do not add docstrings/comments to code you did not change. Do not add
backwards-compatibility shims for code paths that have no caller. If a follow-up is
needed, capture it as an ai-memory entry tagged `followup` and mention it in the PR
description.

---

## 6. Memory Hygiene (ai-memory usage by AI agents)

This project ships `ai-memory`. We dogfood it. Every session that touches this repo
must use ai-memory the same way external users are taught to use it.

### 6.1 What to store

| Trigger | Tier | Priority | Tags |
|---------|------|----------|------|
| User correction or course-change | `long` | 9–10 | `correction,user` |
| Architectural decision (why X over Y) | `long` | 7–8 | `decision,architecture` |
| Hard-won bug-fix lesson (subtle root cause) | `long` | 7 | `bugfix,gotcha` |
| Sprint goal / current task state | `mid` | 5 | `sprint` |
| Debugging breadcrumb for current session | `short` | 3–5 | `debug,transient` |
| Plan for a Sensitive task (per §3.2) | `mid` | 6 | `plan,sensitive` |
| Follow-up not done in this PR | `mid` | 5 | `followup` |

### 6.2 Namespace discipline

Default namespace for memories created while working on this repo: `ai-memory-mcp`.

If the repo's namespace standard is set (`memory_namespace_set_standard`), respect it.
Do **not** invent new namespaces without recording the rationale in a long-tier memory
tagged `namespace,decision`.

### 6.3 Source attribution

Every store must set `--source` accurately:

| Source value | Use when |
|--------------|----------|
| `claude` (or specific agent) | The AI authored the memory unprompted |
| `user` | The user dictated or corrected the content |
| `derived` | Aggregated/consolidated from other memories |

User corrections take precedence over agent-authored memories on the same topic. When
they conflict, write the user version with priority 9–10 and link the prior agent
memory with `supersedes` (see [Governance §7](AI_DEVELOPER_GOVERNANCE.md)).

### 6.4 Contradiction handling

If `memory_detect_contradiction` (or your manual review) finds a conflict with an
existing memory:

1. Do **not** silently overwrite.
2. Use `ai-memory resolve` (CLI) or `memory_link` with `supersedes` (MCP) to record the
   resolution.
3. Mention the contradiction and resolution in the PR description.

### 6.5 Archival, not deletion

Prefer the GC + archive path over hard `memory_delete`. Hard deletion of memories
authored by another collaborator is **Restricted** — do not perform without explicit
human approval.

---

## 7. Self-Review (the four gates)

Before requesting human review, run all four gates locally and paste the results into
the PR description.

```bash
cargo fmt --check
cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic
AI_MEMORY_NO_CONFIG=1 cargo test
cargo audit
```

All four must be clean. If clippy pedantic requires `#[allow(clippy::...)]`, justify it
in the PR description.

In addition, walk the **manual security checklist** in
[`ENGINEERING_STANDARDS.md` §3.2](ENGINEERING_STANDARDS.md) and confirm zero new
findings in the 10 areas (SQL injection, `validate_id()` coverage, command injection,
path traversal, `unwrap()`, error message leakage, race conditions, auth/authz, data in
logs, CORS).

For documentation-only PRs the four gates are still required (they should pass without
changes), but the security checklist may be skipped if no source files changed.

---

## 8. Pull Request Submission

### 8.1 Target branch

PRs target `develop`. **Never** target `main`.

### 8.2 PR description (required sections)

```markdown
## Summary
<1–3 bullets of what changed and why>

## AI involvement
- Agent: <model id, e.g. Claude Opus 4.6>
- Authority class: <Trivial | Standard | Sensitive>
- Human approver(s) for Sensitive items: <@handle> (or "n/a")
- Memory entries created/updated: <ids or "none">

## Test plan
- [ ] cargo fmt --check
- [ ] cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic
- [ ] AI_MEMORY_NO_CONFIG=1 cargo test
- [ ] cargo audit
- [ ] Manual security checklist (Engineering Standards §3.2) reviewed
- [ ] Documentation sync (test counts, tool counts) where applicable

## Linked issues
Closes #<n>  (or "Refs #<n>")
```

### 8.3 Draft vs. ready

- **Trivial / Standard:** open as ready for review.
- **Sensitive:** open as **draft** and `@`-mention the human approver. Mark ready only
  after explicit approval is recorded in a PR comment.
- **Restricted:** do not open. Hand the task back to the human.

### 8.4 Review-cycle behavior

- Address every review comment with either a code change or a written reply.
- Do not resolve review threads opened by reviewers — let the reviewer resolve them.
- If a new push invalidates a prior approval (`main` is configured for stale-review
  dismissal — `develop` may not be), re-request review explicitly.

### 8.5 Merge path — single, by policy

**AI-authored PRs to `develop` always merge via the §3.4 NHI Merge SOP.** There is
no alternate review path for AI-authored PRs in this project.

Why: per [`AI_DEVELOPER_GOVERNANCE.md` §5.4](AI_DEVELOPER_GOVERNANCE.md), only
`@alphaonedev` may approve PRs whose commits carry the AI `Co-Authored-By:`
trailer. Combined with GitHub's hardcoded rule that a PR's author cannot
self-approve, every AI-authored PR satisfies §3.4.1 pre-condition (3) by
construction and the SOP is the only available merge mechanism.

Identification of an AI-authored PR is by the presence of the `Co-Authored-By:`
trailer on **any** commit in the PR (Governance §5.4 #5).

| PR class | Identifying signal | Merge path |
|----------|-------------------|------------|
| AI-authored | Any commit has `Co-Authored-By: <agent>` trailer | §3.4 NHI Merge SOP (always, no alternate) |
| Human-authored | No AI `Co-Authored-By:` trailer on any commit | Normal review (maintainer or any qualified CODEOWNER approves in GitHub UI; merger clicks **Squash and merge**) |

The §3.4 SOP is the standard, codified procedure — not an exception. It does not
weaken any quality gate: signatures, status checks, code-owner rules, and
last-push-approval all remain active throughout the window. Only the
admin-enforcement bit is transiently toggled. See
[`AI_DEVELOPER_GOVERNANCE.md` §3.4](AI_DEVELOPER_GOVERNANCE.md) for full
pre-conditions, procedure, window discipline, and audit-memory template.

If you, as the agent, cannot verify that all §3.4.1 pre-conditions are met, do
**not** invoke the SOP. Stop and hand back to the accountable human.

#### 8.5.1 Multi-agent operation

When more than one agent is active against this repository (e.g., 3 humans in
Claude Code CLI sessions plus a supervised off-host OpenClaw instance), the
§3.4 SOP must be **serialized** via the §3.4.3.1 concurrency lock. See
[`AI_DEVELOPER_GOVERNANCE.md` §3.5](AI_DEVELOPER_GOVERNANCE.md) for the full
multi-agent coordination rules:

- §3.5.1 — Branch ownership (memory-recorded)
- §3.5.2 — Handoff procedure between agents
- §3.5.3 — Stale-branch GC (with mandatory 7-day human confirmation window)
- §3.5.4 — Inter-agent conflict resolution (humans decide; never silently
  reconciled)
- §3.5.5 — §3.4 SOP serialization across agents (concurrency lock)
- §3.5.6 — Operational handoff between humans-in-CLI and supervised off-host
  agents (off-host defers to humans on contention)
- §3.5.7 — Single-agent operation default (most rules become no-ops)

Before invoking §3.4 SOP, every agent must:

1. Acquire the §3.4.3.1 lock (search-then-store, with race-loser-yields)
2. Verify it owns the lock before proceeding to disable `enforce_admins`
3. Release the lock after re-enabling `enforce_admins` (or via TTL fallback)

In the current default state (single-agent operation), the lock is always
acquired uncontested. Once the supervised off-host agent class is registered
and live (per §2.1.1), the lock becomes the operational hot path.

---

## 9. Handoff & Closure

When the PR is merged (or rejected):

### 9.1 Update memories

- Promote the `mid`-tier "plan" memory (if any) to `long` only if the resulting
  knowledge is reusable on future tasks. Otherwise let it expire naturally.
- Store a `long`-tier "outcome" memory summarizing what was done, why, and any
  gotchas to remember for next time. Tags: `outcome,<feature-or-fix-name>`.
- If anything in the journey contradicted a prior memory, link the resolution
  (`supersedes` / `contradicts`).

### 9.2 Archive transient context

Short-tier debugging memories will GC themselves on TTL expiry. Do not delete them
manually — the archive path preserves them for retrospective review.

### 9.3 Close issues

If the PR closed an issue, verify the issue is closed by GitHub on merge. If not, leave
a closing comment with a link to the merge commit. Do **not** close issues unrelated to
the merged work.

---

## 10. Phase / Tool Matrix

Quick reference: which `ai-memory` tools and external commands you use at each phase.

| Phase | ai-memory MCP tools (or CLI) | git / gh / cargo |
|-------|------------------------------|------------------|
| 1 Session start | `memory_session_start`, `memory_recall`, `memory_namespace_get_standard` | `git status`, `git fetch origin develop` |
| 2 Task intake | `memory_recall` (scoped) | `gh issue view <n>` |
| 3 Planning | `memory_store` (plan, sensitive) | — |
| 4 Branching | — | `git checkout -b <type>/<slug> origin/develop` |
| 5 Implementation | `memory_store` (debug, decision) | `git add`, `git commit` (with Co-Authored-By trailer) |
| 6 Memory hygiene | `memory_store`, `memory_link`, `memory_detect_contradiction`, `memory_consolidate` | — |
| 7 Self-review | — | `cargo fmt --check`, `cargo clippy`, `cargo test`, `cargo audit` |
| 8 PR submission | `memory_store` (followup) | `git push -u origin <branch>`, `gh pr create --base develop` |
| 9 Handoff | `memory_promote`, `memory_store` (outcome), `memory_link` | `gh pr view`, `gh issue close` (only if necessary) |

---

## 11. Failure / Stop Conditions

Stop and ask the human before proceeding if any of these occur:

- A required gate fails and the fix is non-obvious or out of task scope.
- A planned change crosses into the Sensitive or Restricted class (per §2.2).
- A user correction contradicts your prior plan or a prior memory.
- A merge conflict involves files you did not modify in this branch.
- An external service (CI, GitHub API, registry) returns an unexpected error.
- You are about to perform any destructive git operation
  (force-push, reset --hard, branch -D, etc. — see Governance §3).

When in doubt, ask. The cost of one clarifying question is far less than the cost of
an unwanted destructive action.

---

## 12. Cross-References

| Topic | Document |
|-------|----------|
| Authority and policy boundaries | [`AI_DEVELOPER_GOVERNANCE.md`](AI_DEVELOPER_GOVERNANCE.md) |
| Code, test, release, security standards | [`ENGINEERING_STANDARDS.md`](ENGINEERING_STANDARDS.md) |
| Contributor procedures | [`../CONTRIBUTING.md`](../CONTRIBUTING.md) |
| Claude Code integration | [`../CLAUDE.md`](../CLAUDE.md) |
| Conduct | [`../CODE_OF_CONDUCT.md`](../CODE_OF_CONDUCT.md) |
| CLA | [`../CLA.md`](../CLA.md) |
| License | [`../LICENSE`](../LICENSE) |
