# cookbook/production-deployment

Runnable companions to [`docs/production-deployment.md`](../../docs/production-deployment.md).

| Recipe | What it demonstrates |
|---|---|
| [`01-secure-bootstrap.sh`](01-secure-bootstrap.sh) | Provisions two Ed25519 keypairs, builds mutual mTLS allowlists, bootstraps `ai-memory` on both nodes with a shared namespace, performs a federated-style write, snapshots and restores from corruption, and verifies chain integrity end-to-end. |

## Running

```bash
# Default: scratch root under the current directory's .local-runs/
./01-secure-bootstrap.sh

# Override scratch root (must NOT be under /tmp /var/tmp /private/tmp)
AI_MEMORY_DEMO_ROOT=$HOME/ai-memory-demo ./01-secure-bootstrap.sh

# Override binary (default: `ai-memory` on PATH)
AI_MEMORY_BIN=/opt/ai-memory/bin/ai-memory ./01-secure-bootstrap.sh
```

Each run carves a fresh timestamped subdirectory and writes a `run.log`. Idempotent: re-running never overwrites prior runs.

## Hard rules

- **No /tmp.** The script refuses to run if `AI_MEMORY_DEMO_ROOT` resolves under `/tmp`, `/var/tmp`, or `/private/tmp`. Project-wide convention; see [`CLAUDE.md`](../../CLAUDE.md).
- **Operator installs the binary.** The script never installs or auto-updates `ai-memory`; it expects the binary already on `PATH`.
- **Forward-looking CLI surfaces are called out.** Any reference to a CLI that ships in v0.8.0 or later is explicitly labeled in the script comments; the demo does not depend on those surfaces.

## See also

- [`docs/production-deployment.md`](../../docs/production-deployment.md) — the canonical operator guide
- [`docs/SECURITY.md`](../../SECURITY.md) — threat model and disclosure policy
- [`docs/telemetry.md`](../../docs/telemetry.md) — what the binary emits and where
