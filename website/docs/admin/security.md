---
sidebar_position: 6
title: Security
description: Security model, hardening, and disclosed gaps.
---

# Security

## Model

ai-memory's security is built around three trust anchors:

1. **Filesystem permissions** — the SQLite file. Anyone who can read the file can read all memories.
2. **mTLS fingerprint allowlist** — peer-mesh trust anchor (`known_hosts`-style).
3. **Agent identity** — claimed via `metadata.agent_id`, immutable once stored, **not yet attested** at the cryptographic layer (Layer 2b, post-v0.6.0).

## Hardening checklist (production)

- [ ] Run as non-root user
- [ ] Restrict DB file permissions: `chmod 600 memories.db`
- [ ] HTTP daemon only listens on TLS (never plain HTTP in production)
- [ ] mTLS allowlist for sync endpoints
- [ ] Set `--agent-id` per process to avoid leaking hostname/PID in `host:` defaults
- [ ] Backup SQLite file regularly
- [ ] Subscribe to release announcements for security patches

## Disclosed gaps in v0.6.0 (tracked + scheduled)

| Issue | Severity | Workaround |
|---|---|---|
| [#231](https://github.com/alphaonedev/ai-memory-mcp/issues/231) Sync endpoints unauthenticated when TLS disabled | High | Always set `--tls-cert` + `--tls-key` for production sync |
| [#232](https://github.com/alphaonedev/ai-memory-mcp/issues/232) Daemon uses `danger_accept_invalid_certs(true)` without mTLS | High | Always pass `--client-cert` |
| [#234](https://github.com/alphaonedev/ai-memory-mcp/issues/234) Consensus voting requires agent pre-registration | High | Register approver agents first |
| [#237](https://github.com/alphaonedev/ai-memory-mcp/issues/237) mTLS allowlist edge cases | High | Verify allowlist file at startup |
| [#238](https://github.com/alphaonedev/ai-memory-mcp/issues/238) Body-claimed `sender_agent_id` not attested | Medium | Trust mTLS cert as identity gate; Layer 2b coming |
| [#239](https://github.com/alphaonedev/ai-memory-mcp/issues/239) `since=0` allows full DB dump for any valid mTLS peer | Medium | Restrict allowlist to fully trusted peers |

See [issue #230](https://github.com/alphaonedev/ai-memory-mcp/issues/230) for the full v0.6.0 red-team report.

## Audit history

- v0.5.4 — 3 rounds of red-team audits, ~100+ findings, all resolved
- v0.5.4-patch.6 — 12 security fixes (CORS, FTS injection, HNSW cap, removed all `unsafe`, CVE patches)
- v0.6.0 — full-spectrum red-team, 0 P0 blockers, 7 P1s with documentation workarounds

## Reporting vulnerabilities

Please **do not** open public issues for security vulnerabilities. Email security@alphaone.dev (or use GitHub's private vulnerability reporting).
