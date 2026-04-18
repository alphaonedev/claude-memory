---
sidebar_position: 1
title: Deployment
description: Deploy ai-memory for a single user, a team, or an organization.
---

# Deployment

ai-memory supports three deployment shapes — pick the smallest one that fits.

## Single-user (laptop / workstation)

Just `ai-memory mcp` per the [Quickstart](/docs/user/quickstart). One SQLite file, no infrastructure, no admin needed.

## Team (peer-to-peer mesh)

Each agent runs its own ai-memory instance. They sync directly via the **sync-daemon** with HTTPS / mTLS:

```bash
# Each peer
ai-memory serve --tls-cert server.pem --tls-key server.key \
  --mtls-allowlist ./peers.allow

# Each peer also runs:
ai-memory sync-daemon \
  --peers https://peer-b:9077,https://peer-c:9077 \
  --client-cert client.pem --client-key client.key
```

See [Peer mesh](./peer-mesh) for the full topology.

## Organization (HTTP daemon, multi-tenant)

Run `ai-memory serve` on a shared host. Clients use the HTTP API or MCP-over-HTTP. Per-request `X-Agent-Id` header attributes writes.

```bash
ai-memory serve \
  --host 0.0.0.0 --port 9077 \
  --tls-cert server.pem --tls-key server.key \
  --db /var/lib/ai-memory/memories.db
```

Production deployments **MUST** use TLS — see [TLS / mTLS](./tls-mtls) and the security disclosures in the [v0.6.0 release notes](https://github.com/alphaonedev/ai-memory-mcp/releases).

## systemd unit (Linux)

```ini
# /etc/systemd/system/ai-memory.service
[Unit]
Description=ai-memory daemon
After=network.target

[Service]
ExecStart=/usr/bin/ai-memory serve --tls-cert /etc/ai-memory/cert.pem --tls-key /etc/ai-memory/key.pem --db /var/lib/ai-memory/memories.db
Restart=on-failure
User=ai-memory

[Install]
WantedBy=multi-user.target
```

## Docker

```bash
docker run -d --name ai-memory \
  -p 9077:9077 \
  -v /etc/ai-memory:/certs:ro \
  -v ai-memory-data:/data \
  ghcr.io/alphaonedev/ai-memory-mcp:latest \
  ai-memory --db /data/memories.db serve \
    --host 0.0.0.0 --tls-cert /certs/cert.pem --tls-key /certs/key.pem
```
