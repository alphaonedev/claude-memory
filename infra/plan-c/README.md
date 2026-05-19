# Plan-C deployment recipe

Three-container ai-memory fleet on a user-defined bridge network.
The canonical home for boot, recovery, and incident-response
guidance is **[`docs/plan-c-deployment.md`](../../docs/plan-c-deployment.md)**;
this README is a pointer.

Files in this directory:

| file                          | purpose                                                  |
|-------------------------------|----------------------------------------------------------|
| `docker-compose.yml`          | Canonical three-peer fleet recipe (issue #878 fix)       |
| `peer-preflight.sh`           | Mesh-reach probe sourced by the entrypoint               |

Quick start (full guidance lives in the runbook above):

```bash
export AI_MEMORY_STORE_URL=postgres://...
export OLLAMA_BASE_URL=http://host.docker.internal:11434
docker compose -f infra/plan-c/docker-compose.yml up -d --build
```

For the **"my Mac crashed and now colima won't start"** recovery,
see the runbook's *"Recovering from a crashed Mac / colima restart"*
section (issue #879).
