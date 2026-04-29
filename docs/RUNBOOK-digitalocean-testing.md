# Runbook — DigitalOcean multi-agent / multi-node ship-gate for v0.6.0.0

Status: **ship-gate procedure** — v0.6.0.0 does **not** tag until
this passes.
Date: 2026-04-19
Depends on: the trident content merged into `release/v0.6.0` (curator,
autonomy, SAL, quorum, federation, migration).

This runbook is **the** gate before cutting `v0.6.0` on the release
branch. No tag without a green DigitalOcean campaign report.

## Source of truth — the ship-gate repo

The reproducible, versioned, CI-driven implementation of everything in
this runbook lives at
[`alphaonedev/ai-memory-ship-gate`](https://github.com/alphaonedev/ai-memory-ship-gate).
That repo's `terraform/`, `scripts/phase{1,2,3,4}_*.sh`, and
`.github/workflows/campaign.yml` are the **executable** forms of the
commands below. This document describes the methodology; the other
repo runs it.

Reproduce a campaign without hand-pasting commands:

```sh
gh repo fork alphaonedev/ai-memory-ship-gate --clone
gh secret set DIGITALOCEAN_TOKEN -R <your-fork>
gh secret set DIGITALOCEAN_SSH_KEY_FINGERPRINT -R <your-fork>
gh secret set DIGITALOCEAN_SSH_PRIVATE_KEY -R <your-fork>
gh workflow run campaign.yml -R <your-fork> \
  -f ai_memory_git_ref=release/v0.6.0 \
  -f campaign_id=my-validation-run
```

The commands in the rest of this runbook are the manual-operator path
for debugging individual phases — they are not the production ship
procedure.

## Scope

Validates the v0.6.0.0 release candidate across three axes on real
cloud infrastructure:

1. **Single-node functional** — install, CRUD, MCP, curator autonomy,
   backup/restore.
2. **Multi-agent / multi-node federation** — three droplets running
   `ai-memory serve --quorum-writes 2`, multiple agents writing
   concurrently, convergence check.
3. **Cross-backend migration** — SQLite droplet → Postgres droplet
   via `ai-memory migrate`.

## Fixture

Three DigitalOcean droplets plus one shared Postgres database:

| Role | Host | Size | Purpose |
|---|---|---|---|
| `aim-node-a` | droplet | s-2vcpu-4gb | Federation peer A + sync-daemon |
| `aim-node-b` | droplet | s-2vcpu-4gb | Federation peer B + sync-daemon |
| `aim-node-c` | droplet | s-2vcpu-4gb | Federation peer C + sync-daemon |
| `aim-postgres` | managed DB | 2vcpu-4gb-pgvector | Shared Postgres for SAL migration test |
| `aim-chaos` | droplet | s-1vcpu-2gb | Chaos-client runs `run-chaos.sh` |

Region: `nyc3` (or wherever latency to the operator is lowest).

## Provisioning

The production Terraform + cloud-init live in the ship-gate repo at
<https://github.com/alphaonedev/ai-memory-ship-gate/tree/main/terraform>.
That module creates three peer droplets + one chaos client + a
campaign-scoped VPC + a firewall mesh + an in-droplet dead-man
switch. It is the authoritative implementation — the snippet below is
illustrative, not a copy to paste.

```hcl
# Illustrative shape — the real module lives in the ship-gate repo.
resource "digitalocean_droplet" "aim_node" {
  for_each = toset(["a", "b", "c"])
  image    = "ubuntu-24-04-x64"
  name     = "aim-${var.campaign_id}-node-${each.key}"
  region   = var.region
  size     = var.peer_size          # s-4vcpu-8gb (8GB needed for rustc)
  ssh_keys = [var.ssh_key_fingerprint]
  vpc_uuid = digitalocean_vpc.campaign.id
  tags     = local.tags
  user_data = local.cloud_init      # pinned to var.ai_memory_git_ref
}
```

Postgres is **not** provisioned as a DO managed database — the
Phase 3 migration test brings Postgres up locally on `node-a` via
`packaging/docker-compose.postgres.yml` for ~$0 marginal cost. A
managed `pgvector` tier would add ~$60/mo to each campaign for
identical test semantics.

## Deployment

On each node, the ship-gate cloud-init clones
`github.com/alphaonedev/ai-memory-mcp` at the pinned ref and builds
with `cargo build --release`:

```bash
# Shape of what cloud-init runs on each droplet — real script at
# https://github.com/alphaonedev/ai-memory-ship-gate/blob/main/terraform/cloud-init.yaml
git clone https://github.com/alphaonedev/ai-memory-mcp.git /opt/ai-memory-mcp
cd /opt/ai-memory-mcp && git checkout "$AI_MEMORY_GIT_REF"
CARGO_BUILD_JOBS=2 cargo build --release
install -m 0755 target/release/ai-memory /usr/local/bin/ai-memory
```

Once a tag is cut, a prebuilt-binary path will replace the cold
`cargo build` step. The channel-aware `install.sh` is tracked in
`#TBD` — for v0.6.0 the build-from-ref path is canonical.

Verify:

```bash
ai-memory --version                          # 0.6.0 on every node
systemctl status ai-memory                   # running
curl -sk https://127.0.0.1:9077/api/v1/health  # {"status":"ok"}
```

## Phase 1 — Single-node functional (per node)

Runtime: ~5 min per node.

```bash
# CLI roundtrip
ai-memory store --title "phase-1-${HOSTNAME}" --content "functional check" --tier mid
ai-memory recall "functional check" --limit 5
ai-memory stats --json > /tmp/phase1-stats-${HOSTNAME}.json

# Autonomy: one curator sweep with dry-run
ai-memory curator --once --dry-run --json > /tmp/phase1-curator-${HOSTNAME}.json

# Backup + verify
ai-memory backup --to /var/backups/ai-memory
test -f /var/backups/ai-memory/ai-memory-*.db
test -f /var/backups/ai-memory/ai-memory-*.manifest.json
```

**Pass**: every command exits 0, `stats.total >= 1`,
`curator.errors == []` (or only `no LLM client` if Ollama isn't on
the node), backup files exist.

## Phase 2 — Multi-agent / multi-node federation

Runtime: ~15 min.

### Setup

On each node, restart `ai-memory serve` with federation:

```bash
# node-a
ai-memory serve --host 0.0.0.0 --port 9077 \
  --tls-cert /etc/ai-memory/cert.pem \
  --tls-key  /etc/ai-memory/key.pem \
  --mtls-allowlist /etc/ai-memory/peer-fingerprints.txt \
  --quorum-writes 2 \
  --quorum-peers https://aim-node-b:9077,https://aim-node-c:9077 \
  --quorum-client-cert /etc/ai-memory/cert.pem \
  --quorum-client-key  /etc/ai-memory/key.pem

# node-b and node-c analogous, each pointing at the other two.
```

### Multi-agent write campaign

From `aim-chaos`, simulate **four concurrent agents** writing from
four different nodes simultaneously. Each agent identifies itself
via `--agent-id` + writes 50 unique memories. Total: 200 memories
across the mesh.

```bash
for AGENT in ai:agent-alice ai:agent-bob ai:agent-charlie ai:agent-dana; do
  for i in $(seq 1 50); do
    curl -sS --cacert /etc/ai-memory/ca.pem \
      -H "X-Agent-Id: $AGENT" \
      -H "Content-Type: application/json" \
      -X POST "https://aim-node-a:9077/api/v1/memories" \
      -d "{\"tier\":\"mid\",\"namespace\":\"do-soak\",\"title\":\"$AGENT-w$i\",\"content\":\"multi-agent write $AGENT seq $i from node-a\",\"priority\":5,\"confidence\":1.0,\"source\":\"do-soak\",\"metadata\":{}}" &
  done
done
wait
```

### Convergence check

After all writes complete, wait 60 s for sync-daemon cycles, then
verify convergence:

```bash
for node in aim-node-a aim-node-b aim-node-c; do
  ct=$(curl -sS --cacert /etc/ai-memory/ca.pem \
        "https://${node}:9077/api/v1/memories?namespace=do-soak&limit=1000" \
        | jq '.memories | length')
  echo "${node}: ${ct}"
done
```

**Pass criteria**:

- Every node reports `>= 190` memories (allow 5% loss for
  quorum_not_met on concurrent writes — this is expected under
  multi-writer contention).
- `ai-memory stats --json` on each node shows identical
  `total` within 2% of each other after a 5-minute settle period.
- Prometheus on each node:
  `ai_memory_store_total{result="err"} / ai_memory_store_total` < 5%.

### Quorum-not-met probe

Explicit kill of node-b, write 10 memories to node-a with
`--quorum-writes 2` against peers b+c. Expected: each write returns
503 `{"error":"quorum_not_met","got":2,"needed":2,"reason":"timeout"}`
because only c is reachable (1 ack vs needed 1). Actually with N=3
and W=2, local + c should meet quorum. Revise:

With **node-b AND node-c down** (both peers killed), node-a writes
should **always** return 503. With **node-b down only**, writes
should succeed via node-c. Both branches must be observed in the
report.

## Phase 3 — Cross-backend migration

Runtime: ~10 min for a 1000-memory corpus.

```bash
# On node-a: build with sal-postgres
cargo build --release --features sal-postgres

# Seed 1000 memories locally
for i in $(seq 1 1000); do
  ai-memory store --title "migrate-test-$i" --content "row $i for migration test" &
done
wait

# Migrate to Postgres
ai-memory migrate \
  --from sqlite:///var/lib/ai-memory/ai-memory.db \
  --to "postgres://ai_memory:${DB_PASS}@${DB_HOST}:25060/defaultdb?sslmode=require" \
  --batch 500 --json > /tmp/migrate-report.json

# Verify
jq '{memories_read, memories_written, errors}' /tmp/migrate-report.json
```

**Pass**: `memories_read == 1000`, `memories_written == 1000`,
`errors == []`. Re-run the same migrate command (idempotency test):
second run must also report 1000/1000/[].

## Phase 4 — Chaos campaign

Runtime: ~40 min.

Run the harness from `aim-chaos` against the three-node fixture:

```bash
AI_MEMORY_BIN=./target/release/ai-memory \
  ./packaging/chaos/run-chaos.sh \
    --cycles 200 --writes 100 --verbose \
    --fault kill_primary_mid_write
```

Repeat with `--fault partition_minority`, `--fault drop_random_acks`,
`--fault clock_skew_peer`.

**Pass criteria** per fault class (from ADR-0001):

- `convergence_bound >= 0.995`.
- `total_fail == 0` (no non-201/non-503 responses).
- Post-campaign node counts agree within 1 memory across all three
  peers.

## Aggregate ship-gate

v0.6.0.0 tags **only** when:

- [ ] Phase 1 passes on all three nodes.
- [ ] Phase 2 (multi-agent / multi-node) passes.
- [ ] Phase 3 (migration) passes round-trip + idempotency.
- [ ] Phase 4 (chaos) passes all four fault classes.
- [ ] All per-phase JSON reports are attached to the release PR.

On pass: cut the tag, archive the droplets, publish the aggregated
report as `docs/CAMPAIGN-v0.6.0.0.md` on the release.

## Not included (deliberate)

- **LLM / Ollama installation** on the droplets. Gemma 4 via Ollama
  is compute-heavy and the DO test focuses on the memory system
  itself. The curator phase runs with `--dry-run`; operators who
  want the full LLM-mediated autonomy test should size up to
  `s-4vcpu-16gb` and follow `RUNBOOK-curator-soak.md`.
- **TurboQuant compression** — scrapped (#284/#287). See CHANGELOG
  scrap note.
- **Week-long soak** — not a ship-gate. Post-release validation; see
  `RUNBOOK-curator-soak.md`.

## Cleanup

After reporting:

```bash
terraform destroy -auto-approve
doctl databases delete aim-postgres --force
```

Audit that the droplets + DB are actually gone before closing the
ship-gate ticket.
