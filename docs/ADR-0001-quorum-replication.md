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

A real durability claim needs measurement. The v0.6.0 chaos harness
(`packaging/chaos/run-chaos.sh`) injects four fault classes. Two are
true injections; two are **documented simulations** that approximate a
related fault class without the kernel-capability requirements of the
real thing. The ADR is honest about which is which so campaign
reports never overclaim.

#### Injected (real) — carry the phase's evidence

1. **`kill_primary_mid_write`** — SIGKILL the originating node between
   the local-commit and the quorum-ack step. Reconciliation on restart
   must converge. Exercised: abrupt writer loss, recovery, idempotent
   replay.
2. **`partition_minority`** — `iptables -I INPUT -s <peer> -j DROP`
   for 500 ms on the originating node, severing its ability to reach
   both peers, then restoring. Exercised: quorum contract under
   transient partition. Writes during the partition MUST fail with
   `quorum_not_met`; writes after restoration MUST converge.

#### Simulated — exercise the code path, not the fault class

3. **`drop_random_acks`** — approximated by `SIGSTOP` on the ack-peer
   process for 500 ms (no kernel-level packet manipulation). This
   exercises the writer's ack-timeout and retry logic as if acks were
   dropped, but does NOT exercise real packet-drop scenarios such as
   partial-frame corruption or TCP retransmit storms. A real
   implementation would need `iptables` with the `STATISTIC` module
   or `tc netem loss 33%` — tracked as follow-up (see Open questions).
4. **`clock_skew_peer`** — RECORDED only. The harness logs the intent
   and moves on without actually manipulating the peer's clock;
   `date --set` / NTP override requires `CAP_SYS_TIME` on the peer
   container, which the ship-gate infrastructure does not grant. A
   real implementation can either (a) bind-mount a read-only `faketime`
   LD_PRELOAD into the peer, or (b) run the peer in a privileged
   container with its NTP daemon masked. Tracked as follow-up.

#### What a passing campaign demonstrates

Each campaign reports a **convergence bound per fault class** —
`(sum ok across cycles) / (sum writes across cycles)`. The pass
criterion is **≥ 0.995 per class for all four classes**. A campaign
that meets this demonstrates:

- The two real classes (`kill_primary`, `partition_minority`) directly
  validate quorum-writer behaviour under abrupt writer loss and
  transient network isolation.
- The two simulated classes validate that the writer's retry and
  ack-timeout code paths do not regress, even though they do not
  validate the underlying fault's semantics end-to-end.

#### What the claim is — and isn't

The public claim derived from a passing campaign is **"convergence
fraction ≥ 0.995 under the four-fault-class campaign described in
ADR-0001, with two fault classes simulated as documented."** It is
**NOT** "<0.01% loss probability" (not measurable at campaign scales
without thousands of hours of runtime) and it is **NOT** a guarantee
against real-world packet drops or clock skew until the two simulated
classes are promoted to real injections. Marketing copy MUST reflect
both of those qualifications — a campaign report with only
`kill_primary` + `partition_minority` as true faults cannot carry a
"chaos-proof" tagline on its own.

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
