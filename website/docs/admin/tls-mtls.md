---
sidebar_position: 4
title: TLS & mTLS
description: Secure ai-memory's HTTP API and peer-mesh sync.
---

# TLS & mTLS

ai-memory's HTTP daemon and sync-daemon use **rustls** under the hood — no OpenSSL dependency. Two layers:

| Layer | What | Flag |
|---|---|---|
| **1. TLS** | HTTPS on `serve` | `--tls-cert` + `--tls-key` |
| **2. mTLS** | Pin client certs by SHA-256 fingerprint | `--mtls-allowlist` (server) + `--client-cert`/`--client-key` (daemon) |

## Layer 1: HTTPS on serve

```bash
ai-memory serve --tls-cert server.pem --tls-key server.key
```

- Cert: PEM-encoded X.509 (may include full chain)
- Key: PKCS#8 or RSA (PEM-encoded)
- Both flags required together — clap rejects half-config

The sync-daemon already speaks HTTPS outbound. Layer 1 lets two peers run a mesh across the open internet from a single binary each — no nginx, no Caddy.

## Layer 2: mTLS with fingerprint pinning

SSH `known_hosts`-style trust: pin SHA-256 fingerprints of trusted client certs. No CA / PKI required.

```bash
# Generate the fingerprint
openssl x509 -in client.pem -outform DER | sha256sum

# Allowlist file (one fingerprint per line)
cat > peers.allow <<EOF_INNER
# allowed peers
sha256:25ab790783dbe969f994063db0412f1930e187e5e1e6c7d79bb76224a76b7bb7
EOF_INNER

# Start serve with mTLS
ai-memory serve \
  --tls-cert server.pem --tls-key server.key \
  --mtls-allowlist ./peers.allow

# Daemon presents its client cert
ai-memory sync-daemon --peers https://peer-b:9077 \
  --client-cert client.pem --client-key client.key
```

Allowlist accepts:
- One fingerprint per line
- Optional `sha256:` prefix
- Optional `:` separators (SSH-style)
- Case-insensitive hex
- `#` comments

A peer **without** a valid cert is rejected at the **TLS handshake** — well before any HTTP request reaches the application.

## Important security notes (v0.6.0)

> **MUST READ** before deploying for sync.

- **Sync endpoints are unauthenticated when TLS is not enabled** ([#231](https://github.com/alphaonedev/ai-memory-mcp/issues/231)). Production peer-mesh deployments **MUST** set `--mtls-allowlist`.
- **`sync-daemon` without `--client-cert` does no server-cert verification** ([#232](https://github.com/alphaonedev/ai-memory-mcp/issues/232)) — uses `danger_accept_invalid_certs(true)`. For untrusted networks, **always** use mTLS in both directions.
- **Body-claimed `sender_agent_id` is not yet attested** to the mTLS cert CN/SAN ([#238](https://github.com/alphaonedev/ai-memory-mcp/issues/238)) — Layer 2b feature, post-v0.6.0.

## Generating self-signed certs (dev / lab)

```bash
openssl req -x509 -newkey rsa:2048 -keyout server.key -out server.pem \
  -days 365 -nodes -subj "/CN=peer.local"
```

For production, use a real CA or a fingerprint-pinning topology.
