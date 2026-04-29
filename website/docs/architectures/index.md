---
slug: /architectures/
title: ai-memory architectures
description: Five reference architectures for ai-memory — from a single agent on a laptop to a globally federated hive of millions. With honest "today vs roadmap" markers per v0.6.3.
hide_table_of_contents: false
---

import Link from '@docusaurus/Link';

<div className="arch-hero">
  <h1>ai-memory architectures</h1>
  <p>
    A primitive that scales from <strong>one agent</strong> on a laptop to a <strong>global hive</strong> of millions.
    Every tier below ships either today, or sits behind a documented gap with a known roadmap.
    Nothing on this page is marketing fiction — every "today" claim cites code in the v0.6.3 source tree.
  </p>
</div>

## The five tiers

<div className="arch-tier-grid">

  <Link to="/docs/architectures/t1-single-node-single-agent" className="arch-tier-card">
    <span className="tier-id">TIER 1</span>
    <h3>Single node, single agent</h3>
    <p>One ai-memory instance, one consumer. SQLite, no network, zero ops. The bedrock primitive.</p>
    <span className="tier-scale">SCALE: 1 node · 1 agent · ~10⁶ memories</span>
    <span className="cap-badge cap-today">SHIPS TODAY</span>
  </Link>

  <Link to="/docs/architectures/t2-single-node-many-agents" className="arch-tier-card">
    <span className="tier-id">TIER 2</span>
    <h3>Single node, many agents</h3>
    <p>One instance fanned across ~10 concurrent agents, each isolated by namespace + scope visibility, gated by per-namespace governance.</p>
    <span className="tier-scale">SCALE: 1 node · 10 agents · namespace-isolated</span>
    <span className="cap-badge cap-today">SHIPS TODAY</span>
  </Link>

  <Link to="/docs/architectures/t3-multi-node-cluster" className="arch-tier-card">
    <span className="tier-id">TIER 3</span>
    <h3>Multi-node cluster</h3>
    <p>4 nodes × 5 agents with <strong>W-of-N quorum writes</strong>, mTLS fingerprint allowlist, federated governance, vector-clock catch-up — all shipping today.</p>
    <span className="tier-scale">SCALE: 4 nodes · 20 agents · quorum-bounded</span>
    <span className="cap-badge cap-today">SHIPS TODAY</span>
  </Link>

  <Link to="/docs/architectures/t4-data-center-swarm" className="arch-tier-card">
    <span className="tier-id">TIER 4</span>
    <h3>Data-center swarm</h3>
    <p>Multi-rack deployment with quorum writes shipping today; <strong>Postgres+pgvector backbone</strong> behind <code>sal-postgres</code> feature flag, GA targeted v0.7.</p>
    <span className="tier-scale">SCALE: 100s nodes · 1000s agents · racked & zoned</span>
    <span className="cap-badge cap-today">CORE TODAY</span>
    <span className="cap-badge cap-roadmap">PG GA · v0.7</span>
  </Link>

  <Link to="/docs/architectures/t5-global-hive" className="arch-tier-card">
    <span className="tier-id">TIER 5</span>
    <h3>Global hive</h3>
    <p>Multi-region cloud, attested agent identity, federated governance, hundreds of thousands to millions of agents acting as a unified collective.</p>
    <span className="tier-scale">SCALE: multi-region · 10⁵–10⁶ agents · attested</span>
    <span className="cap-badge cap-future">VISION v1.0+</span>
  </Link>

</div>

## What ships today vs. what's on the road

The honest matrix. Cell colors match the badges above.

<table className="tier-matrix">
  <thead>
    <tr>
      <th>Capability</th>
      <th>T1</th>
      <th>T2</th>
      <th>T3</th>
      <th>T4</th>
      <th>T5</th>
    </tr>
  </thead>
  <tbody>
    <tr>
      <td>SQLite-backed store, FTS5 keyword recall</td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-roadmap">→ pgvector</span></td>
      <td><span className="cap-badge cap-roadmap">→ pgvector</span></td>
    </tr>
    <tr>
      <td>Semantic recall (HNSW, MiniLM 384-dim)</td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">per-node</span></td>
      <td><span className="cap-badge cap-roadmap">shared idx</span></td>
      <td><span className="cap-badge cap-roadmap">shared idx</span></td>
    </tr>
    <tr>
      <td>Namespace isolation + scope visibility (<code>as_agent</code>)</td>
      <td>n/a</td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
    </tr>
    <tr>
      <td>Per-namespace governance policy</td>
      <td>n/a</td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
    </tr>
    <tr>
      <td>Pending-approval gates (write/promote/delete)</td>
      <td>n/a</td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
    </tr>
    <tr>
      <td>Knowledge-graph w/ temporal validity (v0.6.3)</td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
    </tr>
    <tr>
      <td>Hierarchical taxonomy (<code>memory_get_taxonomy</code>)</td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
    </tr>
    <tr>
      <td>Capabilities introspection v2 (v0.6.3)</td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
    </tr>
    <tr>
      <td>One-way <code>sync_push</code> fanout (memories, links, governance, pending)</td>
      <td>n/a</td>
      <td>n/a</td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
    </tr>
    <tr>
      <td>Vector-clock causality (<code>sync/since</code>)</td>
      <td>n/a</td>
      <td>n/a</td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
    </tr>
    <tr>
      <td>Quorum-write contract (W-of-N peer ack)</td>
      <td>n/a</td>
      <td>n/a</td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
    </tr>
    <tr>
      <td>mTLS peer mesh + fingerprint allowlist</td>
      <td>n/a</td>
      <td>n/a</td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
      <td><span className="cap-badge cap-today">YES</span></td>
    </tr>
    <tr>
      <td>Postgres + pgvector backend</td>
      <td><span className="cap-badge cap-partial">feature flag</span></td>
      <td><span className="cap-badge cap-partial">feature flag</span></td>
      <td><span className="cap-badge cap-partial">feature flag</span></td>
      <td><span className="cap-badge cap-roadmap">GA · v0.7</span></td>
      <td><span className="cap-badge cap-roadmap">GA · v0.7</span></td>
    </tr>
    <tr>
      <td>Cryptographic agent attestation (<code>signature</code> field)</td>
      <td>—</td>
      <td>—</td>
      <td><span className="cap-badge cap-roadmap">field reserved</span></td>
      <td><span className="cap-badge cap-roadmap">v0.7</span></td>
      <td><span className="cap-badge cap-future">v1.0+</span></td>
    </tr>
    <tr>
      <td>Distributed consensus (Raft / Paxos)</td>
      <td>—</td>
      <td>—</td>
      <td>—</td>
      <td>—</td>
      <td><span className="cap-badge cap-future">v1.0+</span></td>
    </tr>
    <tr>
      <td>Gossip / DHT for many-node discovery</td>
      <td>—</td>
      <td>—</td>
      <td>—</td>
      <td><span className="cap-badge cap-roadmap">scoped</span></td>
      <td><span className="cap-badge cap-future">required</span></td>
    </tr>
  </tbody>
</table>

## Why a layered architecture story

ai-memory is a **primitive, not a platform**. The same Rust binary, the same data model, the same MCP protocol surface scales from a developer's `~/.claude/ai-memory.db` to a fleet running across racks. What changes between tiers:

- **Topology** — one process, then many, then many on many machines.
- **Consistency model** — single-writer atomic → eventually consistent peer mesh → quorum → consensus.
- **Trust boundary** — local trust → namespace + scope → governance policy → attested identity.

What stays the same:

- The 23 MCP tools and 24 HTTP endpoints.
- The recall pipeline (FTS5 + HNSW + adaptive blending).
- The tier model (`short` / `mid` / `long`).
- The namespace hierarchy and scope visibility filter.
- The governance contract (`Allow` / `Deny` / `Pending`).

Every tier inherits everything below it.

## Federation primitives that live in the codebase today

These are the building blocks every multi-node tier composes. All shipped, all in `v0.6.3`:

- `src/main.rs:405-447` — quorum-write CLI surface: `--quorum-writes N`, `--quorum-peers <comma-list>`, `--quorum-timeout-ms`, `--quorum-client-cert/-key/-ca-cert`, `--catchup-interval-secs`.
- `src/main.rs:380-393` — TLS / mTLS allowlist: `--tls-cert`, `--tls-key`, `--mtls-allowlist <SHA-256 fingerprint file>`. Allowlist presence enforces mTLS.
- `src/handlers.rs:442-454` — `broadcast_store_quorum` + `finalise_quorum` wired into the write path; returns `200 OK` with `quorum_acks` on success, `503 quorum_not_met` on timeout.
- `src/federation.rs` — **10 broadcast functions**: `broadcast_store_quorum`, `broadcast_delete_quorum`, `broadcast_archive_quorum`, `broadcast_restore_quorum`, `broadcast_link_quorum`, `broadcast_consolidate_quorum`, `broadcast_pending_quorum`, `broadcast_pending_decision_quorum`, `broadcast_namespace_meta_quorum`, `broadcast_namespace_meta_clear_quorum`.
- `src/handlers.rs` — `POST /api/v1/sync/push` accepts memories, deletions, archives, links, pending actions, pending decisions, namespace metadata, and (v0.6.3) entity registrations. `GET /api/v1/sync/since?peer=<id>&clock=<n>` returns causally-correct deltas for a rejoining peer.
- `src/db.rs` — `compute_visibility_prefixes()` + `visibility_clause()` enforce scope-based recall filtering at the SQL level using the indexed `scope_idx` generated column.
- `src/models.rs` — `GovernancePolicy` per-namespace, `PendingAction` queue, `namespace_ancestors()` for visibility hierarchy.
- `src/replication.rs` (422 lines) — `QuorumWriter` + `AckTracker`, **functional**, wired into the write path.
- `Cargo.toml` — `sal-postgres` feature flag for the v0.7 GA Postgres+pgvector backbone (`sqlx` + `pgvector` deps, schema parity fixes shipped #294-#297).

## How to read each tier page

Each tier page contains:

1. An **animated SVG diagram** showing memory data flow — writes, recalls, peer sync, governance gates, and (where relevant) attestations.
2. A **"what's actually happening"** narrative walking the reader through a recall and a write.
3. **Capability badges** on every primitive — green ✅ ships today, amber 🟡 partial, indigo 🔷 roadmap, pink 💖 future vision.
4. A **deployment recipe** with real commands.
5. **Governance, skills, and attestations** wiring for the tier — what enforces the rules at this scale.
6. **Honest limits** — what would break, and at what scale.

Start at [Tier 1](/docs/architectures/t1-single-node-single-agent) and walk forward, or jump straight to the tier that matches your fleet.

<div className="honesty-notice">
  <strong>Engineering honesty.</strong> Every "ships today" claim cites a file path and behavior in the v0.6.3 source tree.
  Roadmap items reference ADRs and tracked work. The vision tier (T5) is the north star — we'll get there, but it's not shipping in v0.7.
  If a diagram shows something the code doesn't do, that's a documentation bug — please file an issue.
</div>
