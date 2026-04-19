# ADR-0001 — Quorum replication + chaos-testing methodology (v0.7 track C)

Status: **Proposed** — design ratified in this PR; implementation lands in
follow-up PRs as the QuorumWriter layer is instrumented and real chaos
campaigns run against a multi-node fixture.

Date: 2026-04-19
Author: Claude Opus 4.7 (1M context) on behalf of @binary2029
Related: PR #277 (v0.6.0 GA), PR #279 (Track B — SAL + Postgres)

---

## Context

v0.6.0 shipped the **sync-daemon** (PR #226) — a one-way, fire-and-forget
push of local memories to one or more peers. It satisfied "knowledge
mesh" use cases but it is deliberately *not* a replication protocol:

- No **acknowledgement** from peers — a push is considered successful
  as soon as the outbound HTTP request returns 2xx from *any* peer, and
  failures silently retry on the next cycle.
- No **quorum** — a single peer's success is enough.
- No **divergence detection** — if two nodes write concurrently with
  the same `(title, namespace)`, both versions propagate independently.
- No **chaos-tested loss-probability guarantee**. The v0.6.0 CHANGELOG
  explicitly refuses to publish a loss number.

The post-v0.6.0 capability trident asks us to earn a *defensible* durability
claim — not "zero-loss" (which is provably impossible at finite
replication factor) but a **W-of-N quorum** that the operator can reason
about and that an external reader can verify.

## Decision

Ship a **W-of-N quorum-write** layer over the existing sync-daemon's
HTTP peer mesh, and ship a **chaos-test harness** that exercises it
against controllable failure modes. Explicitly **do not** adopt a full
consensus protocol (Raft / Paxos) in v0.7 — the complexity budget is
better spent on observability and testing than on replacing the sync
mesh.

### Model: W-of-N quorum writes

- **N** — total number of configured peers (local node + remotes).
- **W** — write-quorum size. An operator-configurable setting; default
  `W = ceil((N + 1) / 2)` (majority).
- `memory_store` returns **OK to the caller** only after the local
  write commits AND at least `W - 1` remote peers have acknowledged
  with a 2xx that carries their post-commit memory id.
- Peers that fail to ack within a deadline (`--quorum-timeout-ms`,
  default 2000 ms) are marked **lagging** and tracked by the reconciliation
  loop.
- `memory_recall` is served from the local replica only — strong
  consistency is NOT promised. Reads are **eventually consistent**
  within one sync-daemon cycle (default 30 s) across peers, plus one
  RTT worst case for quorum-committed writes to propagate.

### Failure modes covered

| Failure | Visible to caller | Visible to metrics |
|---|---|---|
| Zero peers reachable | `StoreError::BackendUnavailable{quorum}` | `replication_quorum_failures_total{reason="unreachable"}` |
| Fewer than W-1 peers ack within deadline | `StoreError::BackendUnavailable{quorum}` | `replication_quorum_failures_total{reason="timeout"}` |
| Local write fails | `StoreError::Backend` (unchanged) | `ai_memory_store_total{result="err"}` |
| Peer returns 2xx but body disagrees on id | Warning log, memory is treated as committed locally, ID drift recorded | `replication_id_drift_total` |
| Peer clock skew detected at >30s | Warning log, no request failure | `replication_clock_skew_seconds` |

### Chaos-testing methodology

A real durability claim needs measurement. The chaos harness supports
four classes of injected fault:

1. **`kill_primary_mid_write`** — SIGKILL the originating node between
   the local-commit and the quorum-ack step. Reconciliation on restart
   must converge.
2. **`partition_minority`** — iptables-drop traffic from the originating
   node to `N - W + 1` peers. Writes MUST fail with
   `BackendUnavailable{quorum}` and the caller MUST NOT see partial
   commits.
3. **`drop_random_acks`** — randomly drop 1/3 of inbound ack packets
   for 60 s. Retry behaviour MUST eventually converge; no memory MUST
   silently go missing.
4. **`clock_skew_peer`** — run one peer with its NTP frozen 5 min
   behind. Writes MUST still succeed; skew MUST appear in
   `replication_clock_skew_seconds`.

Each campaign reports a **durability bound**: the empirical fraction of
writes that converged to every quorum-member within `--quorum-timeout-ms
* 10` under N chaos cycles. A number below 1.0 does not immediately
fail the run — it surfaces for tracking. The claim we eventually
defend is **not** "<0.01% loss" (not measurable at chaos-campaign
scales without thousands of hours of runtime) but rather "**100% of
committed writes converged to every reachable quorum-member under 200
chaos cycles of each failure class**".

### Non-goals

- **Strong-consistency reads.** Would require a read quorum + leader
  election. Not worth the complexity for a memory store whose reads
  are inherently approximate (semantic recall).
- **Byzantine fault tolerance.** Peers are assumed to be honest
  (mTLS + signed memories gate that at the transport layer).
- **Split-brain healing.** When `N < W` on both halves of a partition,
  both halves stop accepting writes. Healing on reconnect follows the
  same reconciliation the sync-daemon already does.
- **Loss-probability as a public metric.** Chaos campaigns
  report a convergence fraction; marketing copy MUST NOT
  translate that to a probability without an explicit methodology note.

## Consequences

### Positive

- Operators get a *knob* (`--quorum-writes N`) with a clear contract
  instead of the current implicit at-least-one fire-and-forget push.
- Chaos campaigns give us a replicable convergence bound and surface
  regressions when new sync paths are added.
- The QuorumWriter sits *above* the existing sync-daemon — no
  disruption to the v0.6.0 code paths. Deployments that don't set
  `--quorum-writes` keep the existing behaviour byte-for-byte.
- We avoid a full Raft integration, which would require a persistent
  log, term numbers, leader election, and a new protocol version.
  Those are appropriate for a future v1.0 but are premature here.

### Negative

- Write latency rises by one RTT to the slowest peer in the W quorum.
  Operators who don't want that cost keep `--quorum-writes 1` (current
  behaviour).
- Adds a new failure mode (`BackendUnavailable{quorum}`) that callers
  need to handle. MCP and HTTP endpoints map this to 503 with
  `Retry-After`.
- Does not improve read consistency; reads stay eventual. Operators
  who need read-your-writes must hit the originating node.

### Neutral

- This is the foundation for a future Raft / Paxos swap but does NOT
  commit us to one. The QuorumWriter API is the stable seam; the
  protocol behind it can change.

## Implementation plan

| Phase | Scope | PR |
|---|---|---|
| 1 | ADR + `src/replication.rs` scaffold with `QuorumWriter` + unit tests | This PR (#280) |
| 2 | Wire `QuorumWriter` into the `memory_store` path behind `--quorum-writes N` flag | follow-up |
| 3 | Chaos harness as a `cargo test --features chaos` integration suite, runs three nodes via `assert_cmd` + random-port bind | follow-up |
| 4 | CI job that runs the chaos suite on PRs touching `replication` / `sync-daemon` code | follow-up |
| 5 | Publish the first convergence-bound report, update CHANGELOG with the methodology-note | v0.7.0 release notes |

## Open questions

- **Quorum policy per-namespace?** A "critical ops" namespace might
  want `W = N`, while a "chat scratch" namespace might want `W = 1`.
  Tracked for v0.7.1 — not gating v0.7.0.
- **Asymmetric quorums** (different W for reads vs writes)? Punted —
  reads are eventual anyway.
- **Clock-skew tolerance knob?** The default 30 s warning threshold
  is arbitrary; will tune when chaos campaigns report real skews.
