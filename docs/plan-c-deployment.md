# Plan-C deployment runbook

This runbook covers operator-facing concerns for the Plan-C
ai-memory container deployment (`Dockerfile.plan-c`, `infra/plan-c/`).
It is the canonical home for boot, recovery, and incident-response
guidance specific to the Plan-C fleet.

## Topology

The canonical recipe at `infra/plan-c/docker-compose.yml` brings up a
three-container fleet on a user-defined bridge network `ic-mesh`:

| container  | published port | role                         |
|------------|---------------|------------------------------|
| `ic-alice` | `19077`       | quorum peer, primary boot    |
| `ic-bob`   | `19078`       | quorum peer, depends_on alice |
| `ic-carol` | `19079`       | quorum peer, depends_on both  |

Each container's daemon listens internally on `19077`; the host-side
publish maps to a unique external port to avoid host-side conflicts.
**Peer URLs are container-DNS form** (e.g. `http://ic-bob:19077`) so
the mesh routes entirely through the bridge — see issue #878 for the
history.

Required environment for `docker compose up`:

```bash
export AI_MEMORY_STORE_URL=postgres://...
export OLLAMA_BASE_URL=http://host.docker.internal:11434
# Optional but recommended for any internet-reachable host:
export AI_MEMORY_API_KEY=$(openssl rand -hex 32)
```

## Boot

```bash
docker compose -f infra/plan-c/docker-compose.yml up -d --build
```

The three containers start in order (`ic-alice` → `ic-bob` →
`ic-carol`) per the `depends_on` directive. Each container runs the
issue #878 peer-mesh reach preflight before exec'ing `ai-memory
serve`; a peer that hasn't booted yet causes EX_CONFIG (78) and the
`restart: on-failure` policy brings the container back in ~10s, by
which time the predecessor is up.

Verify all three healthy:

```bash
docker compose -f infra/plan-c/docker-compose.yml ps
# All three should show "(healthy)" within ~60s.
```

Smoke each daemon's HTTP surface:

```bash
for port in 19077 19078 19079; do
  curl -fsS "http://127.0.0.1:${port}/api/v1/capabilities" | jq -r '.tier'
done
# Expected: autonomous / autonomous / autonomous
```

## Routine recreate

After a binary rebuild or schema migration:

```bash
docker compose -f infra/plan-c/docker-compose.yml down -v
docker compose -f infra/plan-c/docker-compose.yml up -d --build
```

`down -v` wipes the named volumes (`ic-{alice,bob,carol}-{keys,audit}`),
so daemon keypairs are regenerated on the next boot. For zero-downtime
re-deploys that preserve identity, omit `-v` and let the
`entrypoint.plan-c.sh` first-start guard skip key generation.

## Recovering from a crashed Mac / colima restart

> Applies when the host macOS rebooted while colima was running,
> when the colima VM was forcibly stopped (`kill -9`, low-battery
> halt, kernel panic, etc.), or when `colima delete -f` was issued
> without first `colima stop`-ing.

Symptom on `colima start`:

```
FATA[0000] error starting vm: ... 
error: in_use_by exists: '/Users/<you>/.colima/_lima/_disks/colima/in_use_by'
```

Colima writes a `_disks/colima/in_use_by` symlink at VM start (pointing
at the instance currently holding the disk image) and unlinks it on
clean shutdown. A crash leaves the symlink behind, and the next `colima
start` refuses to attach the disk because something *might* still be
using it. macOS does not auto-clean the stale symlink — manual recovery
is required.

**Recovery procedure** (safe when you are certain no other colima VM is
running against the same disk image — usually true on a single-user
laptop):

```bash
# 1) Confirm colima is fully stopped.
colima status                # should print "colima is not running"

# 2) Confirm the stale lock and inspect its target (the instance name).
ls -la ~/.colima/_lima/_disks/colima/in_use_by
# lrwxr-xr-x ... in_use_by -> /Users/<you>/.colima/_lima/colima

# 3) Remove the stale symlink. This is the load-bearing step.
rm ~/.colima/_lima/_disks/colima/in_use_by

# 4) Start colima.
colima start

# 5) Bring the Plan-C fleet back up.
docker compose -f infra/plan-c/docker-compose.yml up -d
```

If `colima status` reports the VM is running but `docker ps` errors
with `Cannot connect to the Docker daemon`, the colima socket is
stale — `colima stop && colima start` cycles it. If `colima stop`
hangs, `colima delete -f` followed by re-`colima start` is the
last-resort recovery; you'll lose the colima VM (not your docker
data, which lives on a separate qcow2 attached at start time —
**unless** you also `rm` the qcow2 file, which you should NOT do).

**Prevention**: install the colima auto-stop launchd job documented at
<https://github.com/abiosoft/colima#colima-vs-docker-desktop> so the
VM is gracefully halted on logout / shutdown. The launchd job
prevents 100% of the `in_use_by` stale-lock incidents we've seen in
the v0.7.0 cert sequence (issue #879).

## Recovering from an ENOSPC during heavy in-flight work

Symptom: cargo builds, docker pulls, or container logs error with
`No space left on device`.

This is usually accumulated agent scratch under `/private/tmp/` (the
macOS realpath of `/tmp`). The project's
**no-/tmp** hard rule (see `CLAUDE.md`) routes all agent scratch to
`.local-runs/`; if `/private/tmp/` has grown anyway, an older agent
violated the rule.

```bash
# Survey the offenders.
du -sh /private/tmp/* | sort -rh | head -20

# Reclaim space (review the list above first; clean carefully).
sudo rm -rf /private/tmp/claude-* /private/tmp/ai-memory-* \
            /private/tmp/cargo-* /private/tmp/rustc-*
```

If colima itself is out of disk, `colima start --disk 100` resizes
the qcow2 (DESTRUCTIVE — wipes the VM state; export volumes first).

## See also

- [`infra/plan-c/docker-compose.yml`](../infra/plan-c/docker-compose.yml) — canonical fleet recipe
- [`infra/plan-c/peer-preflight.sh`](../infra/plan-c/peer-preflight.sh) — issue #878 mesh-reach check
- [`Dockerfile.plan-c`](../Dockerfile.plan-c) — container build
- [`entrypoint.plan-c.sh`](../entrypoint.plan-c.sh) — daemon boot wrapper
- `CLAUDE.md` `## No agent-created files under /tmp …` — project hard rule on scratch locations
