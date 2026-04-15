# Phase 1 — Memory Schema, Hierarchy & Governance

**Version target:** v0.6.0
**Status:** Planning
**Estimated sessions:** 8-10
**Collaborators:** 3 (see task assignments below)

---

## Prerequisites

- Rust 1.87+ with `cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic` passing
- All work branches from `develop`, PRs target `develop`
- AI coding agents: Claude Code Opus 4.6, OpenAI Codex 5.4, or xAI Grok 4.2 (or via IDE plugin in Cursor/Windsurf)
- All code is Rust. No Python, no TypeScript, no shell scripts in core.
- Follow [ENGINEERING_STANDARDS.md](ENGINEERING_STANDARDS.md) and [CONTRIBUTING.md](../CONTRIBUTING.md)

---

## Current Codebase Reference

| Module | Lines | Touches Required |
|--------|------:|------------------|
| `db.rs` | 2,224 | Schema migration, query filters, promotion |
| `main.rs` | 2,053 | CLI flags, new commands |
| `mcp.rs` | 1,951 | New MCP tools, metadata propagation |
| `handlers.rs` | 908 | HTTP API updates |
| `config.rs` | 703 | Governance config loading |
| `models.rs` | 323 | Struct changes, metadata types |
| `validate.rs` | 388 | Namespace path validation, governance validation |
| `toon.rs` | 261 | TOON format for new fields |
| Tests | 192 | Unit: 140, Integration: 52 |

---

## Dependency Graph

Tasks must be completed in this order where arrows exist. Tasks without dependencies can run in parallel.

```
1.1 Schema Migration (metadata column)
 │
 ├──→ 1.2 Agent Identity (needs metadata)
 │     │
 │     └──→ 1.3 Agent Registration (needs agent_id)
 │
 ├──→ 1.4 Hierarchical Namespaces (needs metadata for scope)
 │     │
 │     ├──→ 1.5 Visibility Rules (needs hierarchy)
 │     │
 │     ├──→ 1.6 N-Level Rule Inheritance (needs hierarchy)
 │     │
 │     └──→ 1.7 Vertical Promotion (needs hierarchy)
 │
 ├──→ 1.8 Governance Metadata (needs metadata)
 │     │
 │     ├──→ 1.9 Governance Roles (needs governance metadata)
 │     │
 │     └──→ 1.10 Approval Workflow (needs governance roles)
 │
 └──→ 1.11 Budget-Aware Recall (independent of hierarchy, needs metadata for scope filtering)
       │
       └──→ 1.12 Hierarchy-Aware Recall (needs hierarchy + budget recall)
```

**Critical path:** 1.1 → 1.4 → 1.5 → 1.12

---

## Task Breakdown

### TRACK A — Schema & Agent Identity

**Assigned to:** Collaborator 1
**Files:** `models.rs`, `db.rs`, `mcp.rs`, `handlers.rs`, `validate.rs`, `main.rs`
**Dependencies:** None — this is the foundation. Must land first.

#### Task 1.1 — Schema Migration: Add `metadata` JSON Column

**Branch:** `feature/schema-metadata`
**Estimated:** 1 session

**What to do:**
1. Add `metadata TEXT NOT NULL DEFAULT '{}'` column to `memories` table in `db.rs`
2. Add schema version migration: detect current schema, `ALTER TABLE` if needed
3. Add `metadata` field to `Memory` struct in `models.rs` as `serde_json::Value`
4. Ensure `metadata` is preserved through all CRUD operations (store, update, get, list, recall, export, import)
5. Migrate existing `source` field INTO metadata on schema upgrade: `{"source": "cli"}` — keep `source` column for backward compat during transition
6. Add `metadata` to TOON output in `toon.rs`
7. Add `metadata` to archive table schema

**Tests required:**
- Schema migration runs clean on existing database
- Store with metadata, get returns metadata
- Update preserves unknown metadata fields
- Export/import roundtrip preserves metadata
- Empty metadata `{}` is the default

**Acceptance criteria:**
- `cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic` clean
- All 192 existing tests pass
- Minimum 8 new unit tests for metadata CRUD

---

#### Task 1.2 — Agent Identity in Metadata

**Branch:** `feature/agent-identity`
**Depends on:** 1.1
**Estimated:** 0.5 session

**What to do:**
1. Add `--agent-id` CLI flag to `main.rs` (optional, defaults to hostname or "anonymous")
2. Add `agent_id` parameter to MCP `memory_store` tool in `mcp.rs`
3. On store: populate `metadata.agent_id` automatically from flag/parameter
4. On recall/list/get: include `agent_id` in response (extracted from metadata)
5. Add `--agent-id` filter to `list` and `search` commands: show only memories from a specific agent
6. Add `agent_id` to HTTP API `POST /api/v1/memories` body in `handlers.rs`

**Tests required:**
- Store with agent_id, get returns it in metadata
- Filter by agent_id on list
- Default agent_id when not specified
- Agent_id preserved through update

**Acceptance criteria:**
- 4+ new tests
- Pedantic clippy clean

---

#### Task 1.3 — Agent Registration

**Branch:** `feature/agent-register`
**Depends on:** 1.2
**Estimated:** 0.5 session

**What to do:**
1. New MCP tool: `memory_agent_register` — params: `agent_id` (required), `agent_type` (required: `"ai:claude-opus-4.6"`, `"ai:codex-5.4"`, `"ai:grok-4.2"`, `"human"`, `"system"`), `capabilities` (optional JSON array)
2. Stores agent registration as a special memory with tier=long, namespace=`_agents`, title=`agent:<agent_id>`
3. New MCP tool: `memory_agent_list` — returns all registered agents
4. New CLI command: `ai-memory agents` — list registered agents
5. Add to HTTP API: `GET /api/v1/agents`, `POST /api/v1/agents`

**Tests required:**
- Register agent, list agents, verify fields
- Duplicate registration updates existing
- Agent types validated

**Acceptance criteria:**
- 4+ new tests
- Pedantic clippy clean

---

### TRACK B — Memory Hierarchy & Visibility

**Assigned to:** Collaborator 2
**Files:** `validate.rs`, `db.rs`, `mcp.rs`, `main.rs`, `models.rs`
**Dependencies:** Task 1.1 must be merged before starting 1.5+

#### Task 1.4 — Hierarchical Namespace Paths

**Branch:** `feature/hierarchical-namespaces`
**Depends on:** 1.1
**Estimated:** 1 session

**What to do:**
1. Update `validate_namespace()` in `validate.rs` to accept `/`-delimited paths: `alphaone/engineering/platform`
2. Maximum depth: 8 levels. Maximum path length: 512 chars.
3. Namespace path normalization: strip leading/trailing `/`, collapse `//`, lowercase
4. Existing flat namespaces (`ai-memory`, `global`) remain valid — hierarchy is opt-in
5. Add `namespace_depth()` helper function in `models.rs`
6. Add `namespace_parent()` helper: `alphaone/engineering/platform` → `alphaone/engineering`
7. Add `namespace_ancestors()` helper: returns `["alphaone/engineering/platform", "alphaone/engineering", "alphaone"]`
8. Update `validate_namespace` tests

**Tests required:**
- Valid hierarchical paths accepted
- Invalid paths rejected (too deep, too long, invalid chars)
- Parent extraction correct at all levels
- Ancestors list correct
- Flat namespaces still work
- Edge cases: single-level, root-level, max depth

**Acceptance criteria:**
- 10+ new tests
- Pedantic clippy clean
- Zero breaking changes to existing namespace behavior

---

#### Task 1.5 — Visibility Rules

**Branch:** `feature/visibility-rules`
**Depends on:** 1.4
**Estimated:** 1 session

**What to do:**
1. Add `scope` to memory metadata: `"private"` (default), `"team"`, `"unit"`, `"org"`, `"collective"`
2. Add `--scope` flag to CLI `store` command and MCP `memory_store`
3. Modify `recall()` and `search()` in `db.rs` to apply visibility filtering:
   - Agent at `alphaone/engineering/platform/agent-1` with scope filtering:
     - Sees `private` memories only in its own namespace
     - Sees `team` memories in `alphaone/engineering/platform/*`
     - Sees `unit` memories in `alphaone/engineering/*`
     - Sees `org` memories in `alphaone/*`
     - Sees `collective` memories in `*`
4. Add `--as-agent` flag to recall/search for specifying the querying agent's namespace position
5. When no hierarchy is configured (flat namespaces), visibility defaults to current behavior (namespace-exact match)

**Tests required:**
- Private memories invisible to other agents
- Team memories visible to same-team agents
- Org memories visible to all org agents
- Collective visible to everyone
- Flat namespace backward compatibility
- Cross-scope recall returns correct union

**Acceptance criteria:**
- 10+ new tests
- Pedantic clippy clean

---

#### Task 1.6 — N-Level Rule Inheritance

**Branch:** `feature/n-level-rules`
**Depends on:** 1.4
**Estimated:** 1 session

**What to do:**
1. Extend `namespace_set_standard` in `mcp.rs` to support N-level parents (currently supports `*` + 1 parent)
2. On `session_start` or `recall`, collect standards from all ancestor namespaces:
   - `*` (global) → `alphaone` → `alphaone/engineering` → `alphaone/engineering/platform`
3. Standards are concatenated in order: most general first, most specific last (specific overrides general)
4. Add `--inherit` flag to `namespace_get_standard` to show the full inherited chain

**Tests required:**
- 4-level inheritance chain works
- Most specific standard overrides general
- Missing intermediate levels are skipped cleanly
- Existing 3-level behavior unchanged

**Acceptance criteria:**
- 6+ new tests
- Pedantic clippy clean

---

#### Task 1.7 — Vertical Memory Promotion

**Branch:** `feature/vertical-promotion`
**Depends on:** 1.4
**Estimated:** 0.5 session

**What to do:**
1. Extend `memory_promote` MCP tool with optional `to_namespace` parameter
2. When `to_namespace` is specified: clone the memory to the target namespace (parent level), link with `derived_from` relation
3. Add `--to-namespace` flag to CLI `promote` command
4. Validate that `to_namespace` is an ancestor of the memory's current namespace
5. Original memory remains at its level. Promoted copy exists at the higher level.

**Tests required:**
- Promote from agent to team namespace
- Promoted memory linked to original
- Cannot promote to non-ancestor namespace
- Original memory unchanged

**Acceptance criteria:**
- 4+ new tests
- Pedantic clippy clean

---

### TRACK C — Governance & Smart Recall

**Assigned to:** Collaborator 3
**Files:** `config.rs`, `db.rs`, `mcp.rs`, `handlers.rs`, `main.rs`, `models.rs`
**Dependencies:** Task 1.1 must be merged before starting 1.8+. Task 1.5 should be merged before 1.12.

#### Task 1.8 — Governance Metadata

**Branch:** `feature/governance-metadata`
**Depends on:** 1.1
**Estimated:** 1 session

**What to do:**
1. Define governance schema in `models.rs`:
   ```rust
   pub struct GovernancePolicy {
       pub write: GovernanceLevel,    // any, registered, owner
       pub promote: GovernanceLevel,  // any, approve, owner
       pub delete: GovernanceLevel,   // any, approve, owner
       pub approver: ApproverType,    // human, agent:<id>, consensus:<n>
   }
   ```
2. Governance is stored as JSON in the namespace standard's metadata (not a separate table)
3. Extend `namespace_set_standard` to accept a `governance` JSON parameter
4. Extend `namespace_get_standard` to return governance policy
5. Default governance when not set: `{ "write": "any", "promote": "any", "delete": "owner", "approver": "human" }`
6. Add governance validation in `validate.rs`

**Tests required:**
- Set governance on namespace, retrieve it
- Default governance when not configured
- Invalid governance rejected
- Governance serialization/deserialization roundtrip

**Acceptance criteria:**
- 6+ new tests
- Pedantic clippy clean

---

#### Task 1.9 — Governance Enforcement

**Branch:** `feature/governance-enforcement`
**Depends on:** 1.8
**Estimated:** 1 session

**What to do:**
1. Before `store` in `db.rs`: check governance `write` policy for the target namespace
   - `any` — allow (current behavior)
   - `registered` — agent must be registered (Task 1.3)
   - `owner` — only the namespace owner agent can write
2. Before `delete` in `db.rs`: check governance `delete` policy
3. Before `promote` (vertical, Task 1.7): check governance `promote` policy
4. When policy is `approve`: queue the action instead of executing it
5. New table: `pending_actions` — `id, action_type, memory_id, namespace, requested_by, requested_at, status`
6. New MCP tools: `memory_pending_list`, `memory_pending_approve`, `memory_pending_reject`
7. New CLI commands: `ai-memory pending list`, `ai-memory pending approve <id>`, `ai-memory pending reject <id>`

**Tests required:**
- Write blocked when policy = owner and agent != owner
- Delete blocked when policy = approve
- Pending action created on blocked operation
- Approve executes the pending action
- Reject removes the pending action
- Default governance allows all (backward compat)

**Acceptance criteria:**
- 10+ new tests
- Pedantic clippy clean

---

#### Task 1.10 — Governance Approver Types

**Branch:** `feature/governance-approvers`
**Depends on:** 1.9
**Estimated:** 0.5 session

**What to do:**
1. Implement approver type logic in pending action approval:
   - `"human"` — any human can approve (no automated approval)
   - `"agent:<agent-id>"` — only the specified agent can approve
   - `"consensus:<n>"` — N different agents must approve before the action executes
2. Track approvals on pending actions: `approvals` JSON array in pending_actions table
3. Consensus auto-executes when threshold is met

**Tests required:**
- Human approver blocks automated approval
- Agent approver accepts only from designated agent
- Consensus requires N approvals
- Consensus auto-executes at threshold

**Acceptance criteria:**
- 6+ new tests
- Pedantic clippy clean

---

#### Task 1.11 — Context-Budget-Aware Recall

**Branch:** `feature/budget-recall`
**Depends on:** 1.1 (metadata for scope filtering)
**Estimated:** 1-2 sessions

**What to do:**
1. Add `budget_tokens` parameter to `recall()` in `db.rs` (optional, default: unlimited)
2. Add `--budget` flag to CLI `recall` command
3. Add `budget_tokens` to MCP `memory_recall` tool parameters
4. Implementation:
   - Run existing recall (scored, ranked)
   - Estimate token count per memory: `(title.len() + content.len()) / 4` (rough approximation)
   - Accumulate memories until budget is exceeded
   - Return as many memories as fit within the budget
5. Add `tokens_used` and `budget_tokens` to recall response metadata
6. Add to HTTP API recall endpoints

**Tests required:**
- Budget of 100 tokens returns fewer memories than unlimited
- Budget of 0 returns no memories
- Budget larger than all memories returns everything
- Token estimation is reasonable
- TOON format respects budget

**Acceptance criteria:**
- 6+ new tests
- Pedantic clippy clean
- **This is the #1 differentiator feature — no competitor has it**

---

#### Task 1.12 — Hierarchy-Aware Recall

**Branch:** `feature/hierarchy-recall`
**Depends on:** 1.5 (visibility rules) + 1.11 (budget recall)
**Estimated:** 0.5 session

**What to do:**
1. When an agent recalls with a hierarchical namespace, automatically include memories from all ancestor namespaces (filtered by visibility/scope)
2. Ancestor memories are scored and ranked alongside the agent's own memories
3. Namespace level is a factor in scoring: closer namespace = higher boost
4. Example: agent at `alphaone/engineering/platform/agent-1` recalls "PostgreSQL" → gets results from agent-1 (highest boost), platform team, engineering unit, and alphaone org (lowest boost)

**Tests required:**
- Recall includes ancestor namespace memories
- Closer namespace gets higher score boost
- Flat namespace recall unchanged (backward compat)

**Acceptance criteria:**
- 4+ new tests
- Pedantic clippy clean

---

## Collaborator Assignments

### @alphaonedev — Schema & Agent Identity (Track A)

| Task | Branch | Sessions | Dependencies |
|------|--------|:--------:|:------------:|
| 1.1 Schema Migration | `feature/schema-metadata` | 1 | None — **start immediately** |
| 1.2 Agent Identity | `feature/agent-identity` | 0.5 | 1.1 |
| 1.3 Agent Registration | `feature/agent-register` | 0.5 | 1.2 |
| **Total** | | **2** | |

**Start first.** Everything else depends on 1.1. Merge 1.1 ASAP so @qtfkwk and @bentompkins can begin. Lightest task load allows bandwidth for PR reviews as gatekeeper.

<details>
<summary><strong>Starter Prompt for AI Coder — Task 1.1 (Schema Migration)</strong></summary>

```
I am working on the ai-memory project (https://github.com/alphaonedev/ai-memory-mcp), a Rust-based persistent memory system for AI agents using SQLite.

Read the following files to understand the current codebase:
- src/models.rs (Memory struct definition)
- src/db.rs (all database operations, schema creation)
- src/mcp.rs (MCP tool handlers)
- src/handlers.rs (HTTP API handlers)
- src/main.rs (CLI commands)
- docs/PHASE-1.md (full task specification)
- docs/ENGINEERING_STANDARDS.md (code standards)

TASK: Add a `metadata` JSON column to the memories table (Task 1.1 in PHASE-1.md).

Requirements:
1. Add `metadata TEXT NOT NULL DEFAULT '{}'` column to memories table schema in db.rs
2. Add schema version detection and ALTER TABLE migration for existing databases
3. Add `metadata` field to Memory struct in models.rs as `serde_json::Value`
4. Preserve metadata through ALL CRUD operations: store, update, get, list, recall, search, export, import, archive
5. Add metadata to TOON output in toon.rs
6. Add metadata to memory_archive table schema
7. Write minimum 8 new unit tests covering metadata CRUD, migration, roundtrip, and default values

Constraints:
- Branch from `develop`: git checkout develop && git checkout -b feature/schema-metadata
- Must pass: cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic
- Must pass: AI_MEMORY_NO_CONFIG=1 cargo test
- All existing 192 tests must continue to pass
- SPDX header on any new files: // Copyright 2026 AlphaOne LLC // SPDX-License-Identifier: Apache-2.0
- No new unwrap() calls in production code
- All SQL must use parameterized queries (params![])
```

</details>

<details>
<summary><strong>Starter Prompt for AI Coder — Task 1.2 (Agent Identity)</strong></summary>

```
I am working on the ai-memory project (https://github.com/alphaonedev/ai-memory-mcp), a Rust-based persistent memory system for AI agents using SQLite.

Read: src/models.rs, src/db.rs, src/mcp.rs, src/handlers.rs, src/main.rs, docs/PHASE-1.md

PREREQUISITE: Task 1.1 (metadata column) must be merged. The Memory struct now has a `metadata: serde_json::Value` field.

TASK: Add agent_id support (Task 1.2 in PHASE-1.md).

Requirements:
1. Add --agent-id CLI flag to main.rs (optional, defaults to hostname or "anonymous")
2. Add agent_id parameter to MCP memory_store tool in mcp.rs
3. On store: populate metadata.agent_id automatically from flag/parameter
4. On recall/list/get: include agent_id in response (extracted from metadata)
5. Add --agent-id filter to list and search commands
6. Add agent_id to HTTP API POST /api/v1/memories body in handlers.rs
7. Write minimum 4 new unit tests

Constraints:
- Branch: feature/agent-identity from develop
- Pedantic clippy clean, all tests pass, no new unwrap()
```

</details>

<details>
<summary><strong>Starter Prompt for AI Coder — Task 1.3 (Agent Registration)</strong></summary>

```
I am working on the ai-memory project (https://github.com/alphaonedev/ai-memory-mcp), a Rust-based persistent memory system for AI agents using SQLite.

Read: src/models.rs, src/db.rs, src/mcp.rs, src/handlers.rs, src/main.rs, docs/PHASE-1.md

PREREQUISITE: Tasks 1.1 and 1.2 must be merged. agent_id is now in metadata.

TASK: Add agent registration (Task 1.3 in PHASE-1.md).

Requirements:
1. New MCP tool: memory_agent_register — params: agent_id (required), agent_type (required: "ai:claude-opus-4.6", "ai:codex-5.4", "ai:grok-4.2", "human", "system"), capabilities (optional JSON array)
2. Store agent as a special memory: tier=long, namespace="_agents", title="agent:<agent_id>"
3. New MCP tool: memory_agent_list — returns all registered agents
4. New CLI command: ai-memory agents — list registered agents
5. Add HTTP API: GET /api/v1/agents, POST /api/v1/agents
6. Write minimum 4 new unit tests

Constraints:
- Branch: feature/agent-register from develop
- Pedantic clippy clean, all tests pass, no new unwrap()
```

</details>

---

### @qtfkwk — Memory Hierarchy & Visibility (Track B)

| Task | Branch | Sessions | Dependencies |
|------|--------|:--------:|:------------:|
| 1.4 Hierarchical Namespaces | `feature/hierarchical-namespaces` | 1 | 1.1 |
| 1.5 Visibility Rules | `feature/visibility-rules` | 1 | 1.4 |
| 1.6 N-Level Rule Inheritance | `feature/n-level-rules` | 1 | 1.4 |
| 1.7 Vertical Promotion | `feature/vertical-promotion` | 0.5 | 1.4 |
| **Total** | | **3.5** | |

**Start after 1.1 merges.** Tasks 1.5, 1.6, 1.7 can run in parallel after 1.4 merges.

<details>
<summary><strong>Starter Prompt for AI Coder — Task 1.4 (Hierarchical Namespaces)</strong></summary>

```
I am working on the ai-memory project (https://github.com/alphaonedev/ai-memory-mcp), a Rust-based persistent memory system for AI agents using SQLite.

Read: src/validate.rs, src/models.rs, src/db.rs, docs/PHASE-1.md, docs/ENGINEERING_STANDARDS.md

PREREQUISITE: Task 1.1 (metadata column) must be merged.

TASK: Add hierarchical namespace paths (Task 1.4 in PHASE-1.md).

Requirements:
1. Update validate_namespace() in validate.rs to accept /-delimited paths: "alphaone/engineering/platform"
2. Maximum depth: 8 levels. Maximum path length: 512 chars.
3. Namespace path normalization: strip leading/trailing /, collapse //, lowercase
4. Existing flat namespaces ("ai-memory", "global") remain valid — hierarchy is opt-in
5. Add namespace_depth() helper function in models.rs — returns number of levels
6. Add namespace_parent() helper: "alphaone/engineering/platform" → "alphaone/engineering"
7. Add namespace_ancestors() helper: returns vec of all ancestors from most specific to least
8. Write minimum 10 new unit tests covering valid paths, invalid paths, parent extraction, ancestors, edge cases

Key design decision: The / character is currently rejected by validate_namespace(). You need to allow / as a delimiter while still rejecting \ and null bytes. Keep all other existing validation rules.

Constraints:
- Branch: feature/hierarchical-namespaces from develop
- Pedantic clippy clean, all tests pass
- ZERO breaking changes to existing flat namespace behavior
- No new unwrap() in production code
```

</details>

<details>
<summary><strong>Starter Prompt for AI Coder — Task 1.5 (Visibility Rules)</strong></summary>

```
I am working on the ai-memory project (https://github.com/alphaonedev/ai-memory-mcp), a Rust-based persistent memory system for AI agents using SQLite.

Read: src/db.rs (recall and search functions), src/mcp.rs, src/main.rs, src/models.rs, docs/PHASE-1.md

PREREQUISITE: Task 1.4 (hierarchical namespaces) must be merged. namespace_parent() and namespace_ancestors() are now available.

TASK: Add scope-based visibility rules (Task 1.5 in PHASE-1.md).

Requirements:
1. Add "scope" field to memory metadata: "private" (default), "team", "unit", "org", "collective"
2. Add --scope flag to CLI store command and MCP memory_store
3. Modify recall() and search() in db.rs to apply visibility filtering:
   - An agent at "alphaone/engineering/platform/agent-1":
     - Sees "private" memories only in its exact namespace
     - Sees "team" memories in "alphaone/engineering/platform/*"
     - Sees "unit" memories in "alphaone/engineering/*"
     - Sees "org" memories in "alphaone/*"
     - Sees "collective" memories everywhere
4. Add --as-agent flag to recall/search for specifying the querying agent's namespace position
5. When no hierarchy exists (flat namespaces), default to current behavior (namespace-exact match)
6. Write minimum 10 new unit tests

Implementation hint: Scope visibility can be implemented with SQL WHERE clauses using namespace prefix matching:
WHERE (json_extract(metadata, '$.scope') = 'collective')
   OR (json_extract(metadata, '$.scope') = 'org' AND namespace LIKE 'alphaone/%')
   OR (json_extract(metadata, '$.scope') = 'unit' AND namespace LIKE 'alphaone/engineering/%')
   ... etc

Constraints:
- Branch: feature/visibility-rules from develop
- Pedantic clippy clean, all tests pass, no new unwrap()
```

</details>

<details>
<summary><strong>Starter Prompt for AI Coder — Task 1.6 (N-Level Rule Inheritance)</strong></summary>

```
I am working on the ai-memory project (https://github.com/alphaonedev/ai-memory-mcp), a Rust-based persistent memory system for AI agents using SQLite.

Read: src/mcp.rs (search for namespace_set_standard, namespace_get_standard, session_start), src/db.rs, docs/PHASE-1.md

PREREQUISITE: Task 1.4 (hierarchical namespaces) must be merged.

TASK: Extend the rule inheritance system from 3 levels to N levels (Task 1.6 in PHASE-1.md).

Currently the system supports: global (*) → parent namespace → namespace. This needs to support arbitrary depth: * → org → unit → team → agent.

Requirements:
1. Extend namespace_set_standard in mcp.rs to store standards at any hierarchy level
2. On session_start or recall, collect standards from ALL ancestor namespaces using namespace_ancestors()
3. Standards are concatenated in order: most general first (global *), most specific last
4. Add --inherit flag to namespace_get_standard to show the full inherited chain
5. Write minimum 6 new unit tests: 4-level chain, missing intermediates skipped, existing 3-level behavior unchanged

Constraints:
- Branch: feature/n-level-rules from develop
- Pedantic clippy clean, all tests pass, no new unwrap()
```

</details>

<details>
<summary><strong>Starter Prompt for AI Coder — Task 1.7 (Vertical Promotion)</strong></summary>

```
I am working on the ai-memory project (https://github.com/alphaonedev/ai-memory-mcp), a Rust-based persistent memory system for AI agents using SQLite.

Read: src/mcp.rs (search for memory_promote), src/db.rs (promote function), src/main.rs, docs/PHASE-1.md

PREREQUISITE: Task 1.4 (hierarchical namespaces) must be merged.

TASK: Add vertical memory promotion across the namespace hierarchy (Task 1.7 in PHASE-1.md).

Currently memory_promote changes tier (short→mid→long). This adds a new dimension: promoting a memory UP the namespace hierarchy (agent→team→unit→org).

Requirements:
1. Extend memory_promote MCP tool with optional to_namespace parameter
2. When to_namespace is specified: CLONE the memory to the target namespace, create a "derived_from" link between clone and original
3. Add --to-namespace flag to CLI promote command
4. Validate that to_namespace is an ancestor of the memory's current namespace (use namespace_ancestors())
5. Original memory remains at its level unchanged. Promoted copy exists at the higher level.
6. Write minimum 4 new unit tests

Constraints:
- Branch: feature/vertical-promotion from develop
- Pedantic clippy clean, all tests pass, no new unwrap()
```

</details>

---

### @bentompkins — Governance & Smart Recall (Track C)

| Task | Branch | Sessions | Dependencies |
|------|--------|:--------:|:------------:|
| 1.8 Governance Metadata | `feature/governance-metadata` | 1 | 1.1 |
| 1.9 Governance Enforcement | `feature/governance-enforcement` | 1 | 1.8 |
| 1.10 Governance Approvers | `feature/governance-approvers` | 0.5 | 1.9 |
| 1.11 Budget-Aware Recall | `feature/budget-recall` | 1.5 | 1.1 |
| 1.12 Hierarchy-Aware Recall | `feature/hierarchy-recall` | 0.5 | 1.5 + 1.11 |
| **Total** | | **4.5** | |

**Start 1.8 and 1.11 in parallel after 1.1 merges.** Budget recall (1.11) has no dependency on governance — can start immediately. 1.12 waits for visibility rules (from @qtfkwk) and budget recall.

<details>
<summary><strong>Starter Prompt for AI Coder — Task 1.8 (Governance Metadata)</strong></summary>

```
I am working on the ai-memory project (https://github.com/alphaonedev/ai-memory-mcp), a Rust-based persistent memory system for AI agents using SQLite.

Read: src/models.rs, src/config.rs, src/mcp.rs (search for namespace_set_standard), src/validate.rs, docs/PHASE-1.md

PREREQUISITE: Task 1.1 (metadata column) must be merged.

TASK: Add governance metadata to namespace standards (Task 1.8 in PHASE-1.md).

Requirements:
1. Define governance types in models.rs:
   - GovernanceLevel enum: Any, Registered, Owner, Approve
   - ApproverType enum: Human, Agent(String), Consensus(u32)
   - GovernancePolicy struct: write, promote, delete (GovernanceLevel), approver (ApproverType)
2. GovernancePolicy must implement Serialize/Deserialize for JSON storage
3. Governance is stored as JSON within the namespace standard's metadata field
4. Extend namespace_set_standard MCP tool to accept an optional governance JSON parameter
5. Extend namespace_get_standard MCP tool to return governance policy in response
6. Default governance when not set: { write: Any, promote: Any, delete: Owner, approver: Human }
7. Add governance validation in validate.rs
8. Write minimum 6 new unit tests

Constraints:
- Branch: feature/governance-metadata from develop
- Pedantic clippy clean, all tests pass, no new unwrap()
```

</details>

<details>
<summary><strong>Starter Prompt for AI Coder — Task 1.9 (Governance Enforcement)</strong></summary>

```
I am working on the ai-memory project (https://github.com/alphaonedev/ai-memory-mcp), a Rust-based persistent memory system for AI agents using SQLite.

Read: src/db.rs (store, delete, update functions), src/mcp.rs, src/handlers.rs, src/models.rs, docs/PHASE-1.md

PREREQUISITE: Task 1.8 (governance metadata) must be merged. GovernancePolicy is now available.

TASK: Enforce governance policies on memory operations (Task 1.9 in PHASE-1.md).

Requirements:
1. Before store in db.rs: check governance write policy for the target namespace
   - Any: allow (current behavior)
   - Registered: agent must be registered (agent_id exists in _agents namespace)
   - Owner: only the namespace owner can write
   - Approve: queue the action instead of executing
2. Same enforcement for delete and promote operations
3. New table: pending_actions (id TEXT PK, action_type TEXT, memory_id TEXT, namespace TEXT, payload TEXT, requested_by TEXT, requested_at TEXT, status TEXT DEFAULT 'pending')
4. New MCP tools: memory_pending_list, memory_pending_approve, memory_pending_reject
5. New CLI commands: ai-memory pending list, ai-memory pending approve <id>, ai-memory pending reject <id>
6. Add pending actions to HTTP API: GET /api/v1/pending, POST /api/v1/pending/:id/approve, POST /api/v1/pending/:id/reject
7. Write minimum 10 new unit tests

Key design: When governance blocks an operation, return a response like {"status": "pending", "pending_id": "...", "reason": "governance requires approval"} instead of an error. The caller knows the action was received but not yet executed.

Constraints:
- Branch: feature/governance-enforcement from develop
- Pedantic clippy clean, all tests pass, no new unwrap()
```

</details>

<details>
<summary><strong>Starter Prompt for AI Coder — Task 1.10 (Governance Approvers)</strong></summary>

```
I am working on the ai-memory project (https://github.com/alphaonedev/ai-memory-mcp), a Rust-based persistent memory system for AI agents using SQLite.

Read: src/db.rs, src/mcp.rs (pending_approve handler), src/models.rs (ApproverType), docs/PHASE-1.md

PREREQUISITE: Task 1.9 (governance enforcement) must be merged. pending_actions table and approve/reject tools exist.

TASK: Implement approver type logic (Task 1.10 in PHASE-1.md).

Requirements:
1. On memory_pending_approve, check who is approving against the namespace's governance approver type:
   - Human: accept approval (no automated check — any approval is valid)
   - Agent(agent_id): only accept if the approving agent matches the designated agent_id
   - Consensus(n): track approvals in a JSON array on the pending_action. Auto-execute when n different agents have approved.
2. Add approvals column to pending_actions: TEXT DEFAULT '[]' (JSON array of {agent_id, approved_at})
3. Consensus auto-executes the pending action when threshold is met
4. Write minimum 6 new unit tests

Constraints:
- Branch: feature/governance-approvers from develop
- Pedantic clippy clean, all tests pass, no new unwrap()
```

</details>

<details>
<summary><strong>Starter Prompt for AI Coder — Task 1.11 (Budget-Aware Recall)</strong></summary>

```
I am working on the ai-memory project (https://github.com/alphaonedev/ai-memory-mcp), a Rust-based persistent memory system for AI agents using SQLite.

Read: src/db.rs (recall function — this is the main function to modify), src/mcp.rs (memory_recall handler), src/main.rs (recall CLI command), src/toon.rs, docs/PHASE-1.md

PREREQUISITE: Task 1.1 (metadata column) must be merged.

TASK: Add context-budget-aware recall (Task 1.11 in PHASE-1.md). THIS IS THE #1 DIFFERENTIATOR FEATURE — no competitor has this.

Requirements:
1. Add budget_tokens parameter to recall() in db.rs (optional, type Option<usize>, default None = unlimited)
2. Add --budget flag to CLI recall command in main.rs
3. Add budget_tokens to MCP memory_recall tool parameters in mcp.rs
4. Implementation:
   - Run existing recall (scored, ranked) — do NOT change the scoring logic
   - After scoring and ranking, estimate token count per memory: (title.len() + content.len()) / 4
   - Accumulate memories in ranked order until budget would be exceeded
   - Return as many memories as fit within the budget
5. Add tokens_used and budget_tokens to recall response metadata
6. Add budget_tokens to HTTP API recall endpoints in handlers.rs
7. TOON output should include budget info in the meta line
8. Write minimum 6 new unit tests

The value: LLMs have finite context windows. Being able to say "give me the most relevant memories that fit in 4K tokens" means agents never waste context on low-relevance memories. This is a capability no other memory system offers.

Constraints:
- Branch: feature/budget-recall from develop
- Pedantic clippy clean, all tests pass, no new unwrap()
```

</details>

<details>
<summary><strong>Starter Prompt for AI Coder — Task 1.12 (Hierarchy-Aware Recall)</strong></summary>

```
I am working on the ai-memory project (https://github.com/alphaonedev/ai-memory-mcp), a Rust-based persistent memory system for AI agents using SQLite.

Read: src/db.rs (recall function), src/models.rs (namespace_ancestors), docs/PHASE-1.md

PREREQUISITES: Task 1.5 (visibility rules from @qtfkwk) AND Task 1.11 (budget recall) must be merged.

TASK: Make recall hierarchy-aware (Task 1.12 in PHASE-1.md).

Requirements:
1. When an agent recalls with a hierarchical namespace, automatically query memories from ALL ancestor namespaces (filtered by scope/visibility from Task 1.5)
2. Ancestor memories are scored and ranked alongside the agent's own memories
3. Add a namespace proximity boost to scoring: memories from the agent's own namespace get the highest boost, parent namespace slightly less, grandparent less, etc. Suggested formula: boost = 1.0 / (1.0 + depth_distance * 0.3)
4. Example: agent at "alphaone/engineering/platform/agent-1" recalls "PostgreSQL" → gets results from agent-1 namespace (boost 1.0), platform team (boost ~0.77), engineering unit (boost ~0.63), alphaone org (boost ~0.53)
5. Flat namespace recall must remain unchanged (backward compat) — if no / in namespace, skip hierarchy logic
6. Budget limiting (from Task 1.11) applies AFTER hierarchy-aware scoring
7. Write minimum 4 new unit tests

Constraints:
- Branch: feature/hierarchy-recall from develop
- Pedantic clippy clean, all tests pass, no new unwrap()
```

</details>

---

## Execution Timeline

```
Week 1:
  Collab 1: [1.1 Schema Migration] → [1.2 Agent Identity] → [1.3 Agent Reg]
  Collab 2: (waiting for 1.1) ───────→ [1.4 Hierarchical NS]
  Collab 3: (waiting for 1.1) ───────→ [1.8 Governance Meta] + [1.11 Budget Recall]

Week 2:
  Collab 1: Code review + integration testing + bug fixes
  Collab 2: [1.5 Visibility] + [1.6 N-Level Rules] + [1.7 Vertical Promotion]
  Collab 3: [1.9 Governance Enforcement] → [1.10 Approvers] → [1.12 Hierarchy Recall]

Week 3:
  All:      Integration testing, merge to develop, red team review
            Tag v0.6.0, release
```

---

## Integration Test Plan (Post-Merge)

After all 12 tasks merge to `develop`, run the full integration scenario:

1. **Register 3 agents** with different types (AI Claude, AI Codex, human)
2. **Create namespace hierarchy:** `testorg/engineering/platform/agent-1`
3. **Set governance** on `testorg/engineering`: `{ "promote": "approve", "approver": "human" }`
4. **Store memories** at each level with different scopes
5. **Recall as agent-1** — verify visibility includes team + unit + org
6. **Recall with budget** — verify token limiting works
7. **Promote memory** from agent to team — verify governance blocks without approval
8. **Approve promotion** — verify memory appears at team level
9. **Set standards** at each level — verify N-level inheritance
10. **Full recall** — verify hierarchy-aware scoring (closer namespace = higher score)

**Expected new test count:** 192 existing + ~68 new = ~260 total

---

## Tooling Requirements

| Tool | Required For | Notes |
|------|-------------|-------|
| Claude Code Opus 4.6 | AI coding agent | Via CLI or Cursor plugin |
| OpenAI Codex 5.4 | AI coding agent | Via CLI or Cursor plugin |
| xAI Grok 4.2 | AI coding agent | Via CLI or Cursor plugin |
| Cursor / Windsurf | IDE | With one of the above AI plugins |
| Rust 1.87+ | Compilation | `rustup update stable` |
| SQLite 3.x | Runtime | Bundled via `rusqlite` |

**All code is Rust.** No Python, TypeScript, or shell in core. Test harnesses and benchmarks may use Python.

---

## PR Checklist (Every Task)

- [ ] Branch from `develop`
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic` zero warnings
- [ ] `AI_MEMORY_NO_CONFIG=1 cargo test` all passing
- [ ] `cargo audit` clean
- [ ] New tests cover all new functionality (minimum counts listed per task)
- [ ] SPDX header on any new files
- [ ] PR targets `develop`
- [ ] PR description states what changed and why
- [ ] CLA signed (first PR only)
