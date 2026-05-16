# Federation hardening (mTLS + X-API-Key + fingerprint allowlist)

v0.7.0 hardens the v0.6.x federation surface with three concurrent
authentication layers and three new `AI_MEMORY_FED_*` env vars. Peers
that don't satisfy every configured layer cannot push or fan-out into
the local store.

- **Code paths:** [`src/federation/mod.rs`](../src/federation/mod.rs),
  [`src/federation/peer.rs`](../src/federation/peer.rs),
  [`src/federation/peer_attestation.rs`](../src/federation/peer_attestation.rs),
  [`src/federation/quorum.rs`](../src/federation/quorum.rs),
  [`src/federation/receive.rs`](../src/federation/receive.rs),
  [`src/federation/sync.rs`](../src/federation/sync.rs),
  [`src/federation/vector_clock.rs`](../src/federation/vector_clock.rs),
  [`src/federation/reflection_bookkeeping.rs`](../src/federation/reflection_bookkeeping.rs).
- **Issue trail:** [#238](https://github.com/alphaonedev/ai-memory-mcp/issues/238),
  [#239](https://github.com/alphaonedev/ai-memory-mcp/issues/239),
  [#318](https://github.com/alphaonedev/ai-memory-mcp/issues/318),
  v0.7.0 security-hardening sweep.

## The three auth layers

### Layer 1 — mTLS allowlist

```bash
ai-memory serve --tls-cert /etc/ai-memory/server.crt \
                --tls-key  /etc/ai-memory/server.key \
                --mtls-allowlist /etc/ai-memory/peer-fingerprints.allow
```

The allowlist file is a newline-delimited set of SHA-256
fingerprints in hex with optional `:` separators and `#` comments.
Peers without a listed cert **cannot open the TCP connection** — the
TLS handshake fails before any HTTP layer code runs.

### Layer 2 — X-API-Key

```bash
ai-memory serve --api-key "$(cat /etc/ai-memory/api.key)"
```

When set, every endpoint except `/api/v1/health` requires either the
`X-API-Key` header or the `?api_key=` query parameter. Required in
combination with mTLS for federated peers.

Pinned by [`tests/federation_x_api_key.rs`](../tests/federation_x_api_key.rs).

### Layer 3 — Peer Ed25519 attestation

```bash
export AI_MEMORY_FED_PEER_ATTESTATION=1
```

When set, inbound federation writes (`POST /api/v1/sync/push`,
`POST /api/v1/inbox`) MUST carry a per-batch Ed25519 signature in the
`X-Peer-Signature` header that verifies against the peer's enrolled
public key. Peers without an enrolled key are rejected at the
authentication layer.

Pinned by [`tests/federation_b2_hardening.rs`](../tests/federation_b2_hardening.rs),
[`tests/g_issue_238_sender_attestation.rs`](../tests/g_issue_238_sender_attestation.rs).

## Three new env vars

| Var | Default | Effect |
|---|---|---|
| `AI_MEMORY_FED_PEER_ATTESTATION` | unset | When set, inbound peer writes must carry `X-Peer-Signature` and verify against the enrolled peer key. |
| `AI_MEMORY_FED_SYNC_TRUST_PEER` | unset (deny) | When set, the substrate trusts the peer's claim about origin agent on inbound sync. Default behavior is to re-stamp inbound rows with `imported_from_agent_id = <peer.claim>` and `agent_id = <local.sync-id>`. |
| `AI_MEMORY_FED_TRUST_BODY_AGENT_ID` | unset (deny) | When set, the substrate trusts the wire body's `agent_id` claim instead of the authenticated peer cert. Default: the peer cert wins. |

The default posture is **strict**: an inbound write from an authenticated
peer is treated as the peer's write, not as the underlying agent's
write, unless the operator explicitly opts in to the peer's claim via
the two `TRUST_*` flags. See
[`tests/g_issue_239_sync_scope.rs`](../tests/g_issue_239_sync_scope.rs)
for the bypass-attempt fleet.

## Quorum + vector clocks

v0.6.x quorum semantics are unchanged: W-of-N writes (default majority),
vector-clock CRDT-lite merge, mTLS allowlist between peers. v0.7.0
adds reflection-aware bookkeeping
([`src/federation/reflection_bookkeeping.rs`](../src/federation/reflection_bookkeeping.rs))
so federated reflection writes carry origin metadata that prevents
depth-cap laundering.

## Operator checklist

1. **Generate peer certs.** Use your CA of choice; export the
   SHA-256 fingerprint via `openssl x509 -in peer.crt -noout -fingerprint -sha256`.
2. **Populate `peer-fingerprints.allow`.** One fingerprint per line.
3. **Generate per-peer Ed25519 keys** (`ai-memory identity generate`)
   and exchange public keys out-of-band.
4. **Set `AI_MEMORY_FED_PEER_ATTESTATION=1`** on the receiving daemon.
5. **Leave the two `TRUST_*` flags unset** unless your peer mesh is
   under operator-level control.
6. **Verify** with `curl --cert peer.crt --key peer.key
   https://memory.prod/api/v1/health` — a 200 with `{"status":"ok"}`
   means TLS + mTLS + API key all aligned.
7. **Watch** the daemon log for `peer_attestation_failed` /
   `peer_fingerprint_unknown` lines — those are real rejections.

## Hardening lineage

- The v0.6.x default was "any TCP peer can push if they speak the
  protocol". The v0.7.0 default is "authenticated peer cert OR
  refusal". Three concurrent layers, all enforced.
- The original Ed25519 `signature` column shipped in v0.6.3 was a
  dead column — v0.7.0 fills it via the Ed25519 attestation
  track (H1-H6). Inbound federation re-verifies link signatures via
  `POST /api/v1/links/verify`; see
  [`docs/signed-events-v4.md`](signed-events-v4.md) for the V-4
  audit chain that records each verification outcome.

See also: [`docs/MIGRATION_v0.7.md` §"Federation hardening"](MIGRATION_v0.7.md#federation-hardening),
the canonical inventory in
[`docs/internal/v070-feature-inventory.md` §"Feature: Federation hardening"](internal/v070-feature-inventory.md).
