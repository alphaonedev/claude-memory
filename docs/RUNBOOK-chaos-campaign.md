# Runbook — Chaos campaign against a 3-node Postgres-backed deployment

Status: **runbook (executable pending infrastructure)**.
Date: 2026-04-19
Depends on: #278, #279, #280, #281, #282, #283 (all merged). ADR-0001.

This runbook is the concrete, step-by-step procedure for the
"200-cycle chaos campaign, per fault class" commitment in
ADR-0001 § Chaos-testing methodology. It turns the "$5 — real chaos
campaigns" caveat from a subjective claim into an executable script
with a published report format.

## What the campaign proves (if run to completion)

> **"100% of committed writes converged to every reachable quorum
> member within `quorum_timeout_ms * 10` under 200 cycles of each
> failure class."**

That's the defensible-claim shape. It replaces the overclaim
"<0.01% loss probability" with a measured convergence fraction + a
methodology note. The report MUST NOT be translated to a probability
without a statistically-rigorous model; it is a campaign summary.

## Prerequisites

1. Three hosts (physical or VM), each with:
   - 4 vCPU, 8 GB RAM minimum
   - Docker 20+ with compose v2
   - Outbound network to pull `pgvector/pgvector:pg16` and
     `ghcr.io/alphaonedev/ai-memory:v0.7.0-alpha`
   - Port 5433/tcp open inbound from the other two
   - Port 9077/tcp open inbound from the chaos-client host
2. One **chaos-client** host (separate from the three peers). Needs
   `bash`, `curl`, `jq`, optionally `iptables` with sudo for
   `partition_minority` + `clock_skew_peer` fault classes.
3. `cargo build --release --features sal-postgres` binary on each
   peer — or the pre-built container from the release pipeline.

## Deployment

On each peer host:

```sh
# postgres fixture
docker compose -f packaging/docker-compose.postgres.yml up -d

# ai-memory daemon with federation
export AI_MEMORY_DB=postgres://ai_memory:ai_memory_test@localhost:5433/ai_memory_test
ai-memory serve \
    --host 0.0.0.0 --port 9077 \
    --tls-cert /etc/ai-memory/cert.pem \
    --tls-key /etc/ai-memory/key.pem \
    --mtls-allowlist /etc/ai-memory/peer-fingerprints.txt \
    --quorum-writes 2 \
    --quorum-peers https://peer-b:9077,https://peer-c:9077 \
    --quorum-timeout-ms 2000 \
    --quorum-client-cert /etc/ai-memory/cert.pem \
    --quorum-client-key /etc/ai-memory/key.pem
```

Each peer points `--quorum-peers` at the **other two**.
`--quorum-writes 2` = majority quorum on N=3.

## Running the campaign

From the chaos-client host (substitute the real hostnames for
`peer-a`/`peer-b`/`peer-c`; the script assumes loopback for local
testing):

```sh
# 200 cycles per fault class × 4 classes = 800 cycles total
for fault in kill_primary_mid_write partition_minority drop_random_acks clock_skew_peer; do
    ./packaging/chaos/run-chaos.sh \
        --cycles 200 \
        --writes 100 \
        --fault "$fault" \
        --verbose \
        2>&1 | tee "reports/${fault}.log"
done
```

Runtime estimate: 200 cycles × ~3 s/cycle × 4 fault classes = ~40
minutes on modest hardware. Add ~10 minutes for fixture setup.

## Report format

Each campaign produces `reports/<fault>.log` containing one JSONL
line per cycle plus a summary. Final convergence-bound:

```json
{
  "campaign": "kill_primary_mid_write",
  "total_cycles": 200,
  "total_writes": 20000,
  "total_ok": 19920,
  "total_quorum_not_met": 80,
  "convergence_bound": 0.996
}
```

## Pass / fail criteria

**Pass criterion** (what we commit to publishing on v0.7.0 GA):

- `convergence_bound >= 0.995` per fault class.
- `total_fail == 0` (no non-503 non-201 responses — i.e., the daemon
  never crashed or returned a 5xx that wasn't `quorum_not_met`).
- Post-campaign reconciliation: after running `ai-memory sync` on
  each node with `--verbose`, `ai_memory_memories{namespace="chaos"}`
  is identical across all three peers.

**Soft-fail — document but don't block release**:

- `convergence_bound` in `[0.98, 0.995)` → publish with caveat,
  open a follow-up issue to investigate the uncovered loss.

**Hard-fail — block release**:

- `convergence_bound < 0.98`
- Any fault class shows a non-zero `count_node_N` divergence after
  reconciliation (split-brain that didn't heal).

## Publication

On pass, the report lands as `docs/CHAOS-REPORT-v0.7.0.md` with:

- Date, commit SHA, hardware specs.
- Per-fault-class summary table.
- Attached `reports/*.log` JSONL artifacts.
- Explicit methodology note: "convergence bound over N cycles of
  injected failures, not a loss-probability claim".

## Why this is a runbook, not a test

- Runtime is 40+ minutes; inappropriate for per-PR CI.
- Requires iptables/sudo (root privileges) for two fault classes.
- Requires real multi-host networking, not `127.0.0.1`.
- Results are meaningful only on the release candidate commit.

The in-repo `packaging/chaos/run-chaos.sh` supports local three-
process testing against `127.0.0.1` as a smoke test; the published
v0.7.0 campaign uses three physically separate hosts.
