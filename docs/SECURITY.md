# ai-memory Security Overview

Threat model, trust boundaries, and hardening options for operators.

For responsible disclosure: **security@alphaone.dev**. Please encrypt
against the maintainer key listed in `SECURITY.md.sig` (if present)
or via the fingerprint on our releases page.

## Threat model

ai-memory is designed to be safe under the following attacker
capabilities:

1. **Local untrusted user** on the same machine as the CLI or HTTP
   daemon. They should NOT be able to read memories outside their
   own database, alter governance state, or escalate to the daemon's
   effective UID.
2. **Network attacker** reaching the HTTP daemon. They should NOT be
   able to bypass API-key / mTLS, inject memories with a forged
   `agent_id`, or enumerate memories without authorization.
3. **Compromised peer** holding valid mTLS cert. They CAN push
   memories under any `agent_id` they claim in the request body —
   this is the **Layer 2b gap**, tracked as issue #238 and addressed
   in v0.7 (see `src/attestation.rs`).
4. **Compromised LLM** (Ollama returning malicious content). Autonomy
   hooks never `exec` or write to disk outside the database. Worst
   case: bad tags, spurious contradiction flags. Reversible via the
   rollback log.

Out of scope (non-goals):

- **Byzantine peer tolerance**. Peers are assumed to be honest at the
  sync protocol level (mTLS + future Layer 2b attestation gate that).
- **Side-channel attacks** (timing, cache, etc.) on the SQLCipher
  passphrase. We expose the passphrase only via a root-readable file.
- **Denial of service at the database layer**. SQLite uses a
  process-global mutex; malicious writers can queue. Rate-limit
  upstream of the daemon.

## Trust boundaries

```
┌──────────────┐   mTLS + API key    ┌──────────────┐
│ MCP client   │───────────────────▶│ ai-memory    │
│ (Claude Code)│  stdio JSON-RPC     │ daemon /     │
├──────────────┤                     │ MCP server / │
│ HTTP client  │────────────────────▶│ CLI          │
│ (SDK, curl)  │                     │              │
├──────────────┤   mTLS + sync       └──────┬───────┘
│ peer daemon  │◀────────────────────┐      │
└──────────────┘                     │      │ SQLite mutex
                                     │      ▼
                              ┌──────┴──────┐
                              │ ai-memory.db│
                              │ (optionally │
                              │  SQLCipher) │
                              └─────────────┘
```

- **MCP (stdio)**: trusts the parent process. Run as the same user
  as the MCP client. No authentication needed.
- **HTTP daemon**: trusts no one by default. API key + mTLS gate
  inbound.
- **Peer sync**: trusts peers on the mTLS allowlist.
- **Governance**: trusts registered agents for approvals. Adding an
  approver is the only way in.

## Authentication

### API key (HTTP)

Set at daemon startup:

```bash
ai-memory serve --api-key "$(pwgen -s 48 1)"
```

Or via config file:

```toml
api_key = "long-random-string"
```

Every HTTP endpoint except `/api/v1/health` enforces the key. Accepts
either:

- Header: `X-API-Key: <key>`
- Query parameter: `?api_key=<key>`

Rotation: generate new key, update config, restart the daemon.
Clients have a grace period determined by their connection
lifetime — there's no in-flight rotation today.

### mTLS (Layer 1 + Layer 2)

Layer 1 enables HTTPS:

```bash
ai-memory serve \
  --tls-cert /etc/ai-memory/cert.pem \
  --tls-key  /etc/ai-memory/key.pem
```

`rustls` under the hood, no OpenSSL dep. PKCS#8 and RSA keys both
supported. Certificate expiry is the operator's responsibility; the
daemon does not notify on impending expiry.

Layer 2 adds a client-cert fingerprint allowlist:

```bash
ai-memory serve \
  --tls-cert /etc/ai-memory/cert.pem \
  --tls-key  /etc/ai-memory/key.pem \
  --mtls-allowlist /etc/ai-memory/peer-fingerprints.txt
```

Allowlist format: one SHA-256 hex fingerprint per line, optional
`:` separators, `#` comments. Any peer not on the allowlist cannot
complete the TLS handshake.

```
# peer-a.example.com
2F:79:84:AB:…:CD
# peer-b.example.com
7E:1B:FE:22:…:AA
```

### Attested `agent_id` (Layer 2b, v0.7)

The current trust model authenticates the **connection** (mTLS
allowlist) but not the **identity claim**. A peer with a valid cert
can POST `sync_push` claiming any `sender_agent_id`. v0.7 closes
this:

- `AttestationMode::Off` (default) — preserves v0.6.0 behaviour.
- `AttestationMode::Warn` — log the mismatch, accept the request.
- `AttestationMode::Reject` — return 403 on mismatch.

Primitives shipped in #285; handler wiring is a v0.7.1 follow-up.

## Data at rest

### SQLCipher encryption

Opt-in cargo feature. Replaces the bundled SQLite with SQLCipher
(AES-256 page encryption).

```bash
cargo build --release --no-default-features --features sqlcipher
```

Supply the passphrase via a root-readable file:

```bash
echo -n 'strong-passphrase' > /etc/ai-memory/db.key
chmod 0400 /etc/ai-memory/db.key
ai-memory --db-passphrase-file /etc/ai-memory/db.key <cmd>
```

The CLI reads the file, exports `AI_MEMORY_DB_PASSPHRASE` for the
process lifetime, and clears it on exit. Passphrase never appears in
`ps`/`/proc/<pid>/environ`.

Defaults (page size, cipher, KDF iterations) match SQLCipher 4.x. To
open the DB manually: `sqlcipher ai-memory.db` + `PRAGMA key='…';`.

### File permissions

The daemon expects the DB file + WAL/SHM companions to be writable
only by the `ai-memory` user:

```bash
chown ai-memory:ai-memory /var/lib/ai-memory/*.db*
chmod 0600 /var/lib/ai-memory/*.db*
```

The bundled systemd unit enforces `ReadWritePaths=/var/lib/ai-memory`
and drops all capabilities.

### Backups

`ai-memory backup` writes SQLCipher-encrypted snapshots too when the
daemon is built with `--features sqlcipher`. The sha256 manifest
commits to the ciphertext, not the plaintext — verification works
without the passphrase.

## Input validation

Every write path validates:

- `agent_id` — regex `^[A-Za-z0-9_\-:@./]{1,128}$`. Rejects shell
  metacharacters, whitespace, control chars.
- `namespace` — rejects `..` segments (path traversal), caps length.
- `title` / `content` — length caps, HTML-safe (not stripped).
- `tags` — each tag validated against the same regex as above.
- `tier` / `scope` — whitelisted values only.
- `metadata` — JSON object, size capped.

Body-size limit: 16 MB per request (`DefaultBodyLimit::max(16 * 1024 * 1024)`).

## Network hardening

### Bind address

```bash
ai-memory serve --host 127.0.0.1   # loopback-only (default)
ai-memory serve --host 0.0.0.0     # public (requires TLS + auth)
ai-memory serve --host 10.0.0.5    # specific interface
```

Never bind `0.0.0.0` without TLS + API key + mTLS.

### Webhooks (SSRF-hardened)

The webhook dispatch path validates URLs before POSTing:

- `https://` required unless the host is a loopback address.
- Private-range IPv4 (10/8, 172.16/12, 192.168/16), IPv6
  unique-local, and link-local are rejected.
- DNS is resolved once per send; we do NOT follow redirects.
- HMAC-SHA256 signs every payload when a secret is supplied.

Subscribers can still receive malicious webhook-URL registrations —
review subscription inserts through governance if that's a concern.

### Sync peer push

`POST /api/v1/sync/push` is gated by mTLS fingerprint allowlist.
Without TLS + mTLS, it accepts any caller — the startup log warns
about this. Never run the sync endpoint on a public network without
mTLS.

## Governance

Policies are set per-namespace via `memory_namespace_set_standard`
(MCP) or directly in SQL. Supported policy levels:

- `allow` — no approval needed.
- `single:<agent-type>` — one registered agent of this type.
- `consensus:N` — N distinct registered agents must approve.

An action that hits a policy returns `202 Accepted` with a
`pending_id`; approvers POST to `/api/v1/pending/{id}/approve`.

**Critical**: `consensus:N` requires **pre-registered agents**
(issue #216 / #234). Unregistered approvers cannot satisfy the
quorum. See `ai-memory agents register`.

## Audit trail

Every memory carries `metadata.agent_id` (immutable once written).
Governance actions log `decided_by`. The curator's rollback log
preserves every autonomous action as a reversible snapshot memory
in `_curator/rollback/<ts>`.

For compliance-grade audit, also:

- Enable daemon structured logs (`RUST_LOG=ai_memory=info`) and ship
  to syslog.
- Enable Prometheus `/metrics` and scrape the full counter set.
- Retain `archive` memories (don't `archive purge`).

## Responsible disclosure

If you find a security vulnerability, please:

1. **Do not** open a public issue.
2. Email **security@alphaone.dev** with details. Encrypt to the key
   listed on our releases page if the impact is severe.
3. We aim to acknowledge within 72 hours and ship a fix within 14
   days for critical issues. We'll credit reporters who wish to be
   credited.

Our bug bounty program is documented at <https://alphaonedev.github.io/ai-memory-mcp/security>.
