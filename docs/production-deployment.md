# Production Deployment Guide

**Audience:** operators standing up `ai-memory` for real workloads — single-instance, hub-spoke teams, or W-of-N federations. **Reading time:** 10 minutes.

This guide collects the must-do steps for a hardened deployment. It assumes you have the binary on disk (`brew install ai-memory`, `cargo install ai-memory`, `apt install ai-memory`, or `docker pull ghcr.io/alphaonedev/ai-memory:latest`) and a host with persistent storage. For the threat model and disclosure policy see [`SECURITY.md`](../SECURITY.md). For telemetry and observability see [`telemetry.md`](telemetry.md).

---

## 1. Operator responsibilities

`ai-memory` is operator-controlled substrate. The binary does not phone home, does not auto-update, and does not register your deployment with any central registry. Five things only you can decide:

1. **Identity material.** Generate Ed25519 keypairs per agent (Section 2). Private keys never leave the host; you decide the rotation cadence.
2. **mTLS allowlist.** Federation refuses any peer whose Ed25519 public key is not on your allowlist (Section 3). Allowlists are mutual.
3. **Storage backend.** SQLite (default, single-instance, WAL mode) or PostgreSQL with Apache AGE (multi-writer hub-spoke). The substrate is the same; the operational profile differs.
4. **Topology.** Single-instance, hub-spoke, or W-of-N federation (Section 7).
5. **Backup discipline.** No data leaves the host without your action — that includes losing it. Schedule snapshots (Section 4).

The remaining sections are mechanical: keypairs, allowlist, backups, migrations, observability, topology, upgrades.

---

## 2. Keypair provisioning

Every agent in a deployment needs its own Ed25519 keypair. The CLI never auto-generates one for you — generation is explicit so a typo cannot silently rotate a long-lived peer.

```bash
ai-memory identity generate --agent-id alice@team-finance
ai-memory identity generate --agent-id bob@team-finance
ai-memory identity list
ai-memory identity export-pub --agent-id alice@team-finance
```

Default storage paths (overridable with `--key-dir` or `AI_MEMORY_KEY_DIR`):

- **Linux:** `~/.config/ai-memory/keys/`
- **macOS:** `~/Library/Application Support/ai-memory/keys/`
- **Windows:** `%APPDATA%\ai-memory\keys\`

Files land with `0600` permissions on Unix. `generate` refuses an existing `--agent-id` unless you pass `--force` — rotation is opt-in. Two agents sharing a keypair is a configuration error; the substrate cannot detect it but every audit chain you produce afterwards will be ambiguous about provenance.

Hardware-backed key storage (TPM 2.0, PKCS#11 HSMs, Apple Secure Enclave, cloud KMS adapters) is intentionally out of OSS scope and ships in the commercial tier.

---

## 3. mTLS allowlist bootstrap

Federation peers exchange signed messages over mTLS. The allowlist is the operator's source of truth for which peers may speak with this node.

```bash
# Export local public key
ai-memory identity export-pub --agent-id alice@team-finance > alice.pub

# Out-of-band: send alice.pub to bob, receive bob.pub
ai-memory identity import --agent-id bob@team-finance --pub bob.pub
```

After import, the allowlist is mutual: alice's node only accepts inbound federation messages signed by bob's key, and vice versa. A peer presenting a key not on the allowlist is rejected at the handshake — no log record of the message contents is created, only a metric increment.

Allowlist format: a directory of public-key files keyed by agent id. There is no central allowlist file to corrupt; adding or removing a peer is `ai-memory identity import`/`rm`. Audit your allowlist with `ai-memory identity list` whenever you suspect drift.

---

## 4. Backup and restore

SQLite deployments use `ai-memory backup` (a `VACUUM INTO` wrapper that emits a defragmented snapshot plus a sha256 manifest):

```bash
ai-memory backup --to /var/backups/ai-memory --keep 48
ai-memory restore --from /var/backups/ai-memory   # uses newest snapshot
```

`--keep` rotates oldest-first. The manifest pins the snapshot's sha256, byte size, source-DB path, and binary version that produced it. `restore` verifies the sha256 before swapping the target file in. Pass `--skip-verify` only if you have already verified out-of-band — the flag exists for restoring from cold storage that has been re-hashed by a separate tool, not as a routine bypass.

PostgreSQL deployments use the standard tooling:

```bash
pg_dump --format=custom ai_memory > ai-memory-$(date -u +%Y%m%dT%H%M%SZ).pgdump
pg_restore --clean --create --dbname=postgres ai-memory-<timestamp>.pgdump
```

**Post-restore verification.** v0.7.0 substrate verifies the reflection chain (L1-L3 recursive learning) automatically on next daemon start; corruption surfaces as a startup refusal with the offending row id. The dedicated `ai-memory verify-reflection-chain` admin CLI for ad-hoc verification of a quiescent database lands in v0.8.0; until then the on-start check is load-bearing.

Backup cadence target: hourly snapshots, 48-hour rotation, weekly off-host transfer to a separate failure domain. Sizing: a 1 GB SQLite file produces a ~700-900 MB snapshot after `VACUUM INTO`.

---

## 5. Schema migrations

Migrations are forward-only and run automatically on the first daemon start after an upgrade. There is no offline migration step. The substrate refuses to start against a database newer than the binary expects (downgrade refusal) and progresses through every intermediate version on upgrade — never skips.

A dry-run flag for offline previewing of a pending migration ships in v0.8.0 (`ai-memory migrate --dry-run`). Until then, the recommended workflow on a major-version upgrade is:

1. Take a snapshot (`ai-memory backup --to <path>`).
2. Start the new binary against a copy of the snapshot in a scratch directory.
3. Observe the migration log; the binary writes one INFO line per schema-version step.
4. Promote the new binary against the live database only after the scratch migration completes cleanly.

Migration failures roll back; the database is never left in a half-migrated state. If a migration aborts mid-way the binary refuses to serve and prints the offending schema-version transition.

---

## 6. Observability

Out-of-the-box observability lands in three places:

- **Tracing spans on stderr.** Every MCP tool call, every governance decision, every federation event emits a `tracing::info!` span. `RUST_LOG=ai_memory=info` is the default; `RUST_LOG=ai_memory=debug` for deep traces.
- **File logging.** Opt-in via `[logging]` in `config.toml` (path, rotation size, retention days, `structured = true` for JSON). Routes to a rotating appender; off by default.
- **`ai-memory doctor`.** A 7-section health dashboard run locally: database integrity, schema version, retention drift, embedder availability, hook pipeline status, federation peer reachability, recent audit summary. Nothing leaves the host.

Hooks (`pre_store`, `post_store`, `post_recall`, `pre_federation_send`, etc.) are the supported extension surface for routing events to a SIEM, paging an operator, or short-circuiting writes. See [`docs/integrations/`](integrations/) and [`telemetry.md`](telemetry.md).

---

## 7. Deployment topologies

**Single-instance.** One host, SQLite, WAL mode. Defaults are correct. This is the recommended starting topology for any deployment under ~5 agents or under ~10 GB of stored memories.

**Hub-spoke (team).** One PostgreSQL+AGE hub, N spoke agents pushing federated memories on a schedule. The hub is the source of truth for cross-agent recall; spokes hold their own local SQLite for offline work. mTLS allowlist on the hub names every spoke; spokes have an allowlist of one entry (the hub).

**W-of-N federation.** Three or more peers, each holding its own SQLite, mesh-federating writes with an attested commit requiring W signatures out of N peers before a write is accepted as canonical. Resolves the "any single operator can rewrite history" problem. CRDT-based eventual consistency by default; opt-in MVCC strict-consistency mode ships in v1.0.

Sizing guide (Apple M2, 16 GB, SQLite reference):

| Topology | Agents | Stored memories | Notes |
|---|---|---|---|
| Single | 1-5 | up to 1M | WAL mode, BLOB content paged on demand |
| Hub-spoke | 5-50 | up to 10M | Postgres+AGE hub, SQLite spokes |
| W-of-N | 3-9 peers | up to 1M per peer | Federation broadcasts dominate at high write rates |

---

## 8. Upgrades

The canonical upgrade sequence:

```bash
# 1. Snapshot the live database
ai-memory backup --to /var/backups/ai-memory

# 2. Stop the daemon
systemctl stop ai-memory   # or pkill, brew services stop, etc.

# 3. Install the new binary (channel-appropriate command)
brew upgrade ai-memory     # or apt, dnf, cargo install --force, docker pull

# 4. Start the daemon; migrations run automatically
systemctl start ai-memory

# 5. Verify
ai-memory doctor
```

Rollback: stop the daemon, restore the pre-upgrade snapshot, downgrade the binary. The substrate refuses to start against a database newer than the binary expects, so a partial rollback fails loudly rather than silently corrupting data.

---

## See also

- [`SECURITY.md`](../SECURITY.md) — threat model, disclosure policy
- [`telemetry.md`](telemetry.md) — what the binary emits, where it goes, what it does not do
- [`migration-v0.7.0-postgres.md`](migration-v0.7.0-postgres.md) — SQLite-to-Postgres migration
- [`RUNBOOK-chaos-campaign.md`](RUNBOOK-chaos-campaign.md) — operator drill for partition + power-loss recovery
- [`../cookbook/production-deployment/01-secure-bootstrap.sh`](../cookbook/production-deployment/01-secure-bootstrap.sh) — runnable end-to-end demo of Sections 2-4 + 7
