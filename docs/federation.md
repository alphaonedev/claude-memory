# Federation hardening (mTLS + X-API-Key + peer attestation)

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

### Layer 1 — mTLS allowlist (transport)

```bash
ai-memory serve --tls-cert /etc/ai-memory/server.crt \
                --tls-key  /etc/ai-memory/server.key \
                --mtls-allowlist /etc/ai-memory/peer-fingerprints.allow
```

The allowlist file is a newline-delimited set of SHA-256
fingerprints in hex with optional `:` separators, `#` line comments,
and trailing inline comments after the fingerprint
([`src/main.rs:128-160`](../src/main.rs)). Peers without a listed cert
**cannot open the TCP connection** — the TLS handshake fails before
any HTTP layer code runs.

### Layer 2 — X-API-Key (application)

```bash
ai-memory serve --api-key "$(cat /etc/ai-memory/api.key)"
```

When set, every endpoint except `/api/v1/health` requires either the
`X-API-Key` header or the `?api_key=` query parameter. Required in
combination with mTLS for federated peers.

Pinned by [`tests/federation_x_api_key.rs`](../tests/federation_x_api_key.rs).

### Layer 3 — Peer attestation (identity)

```bash
export AI_MEMORY_FED_PEER_ATTESTATION='{
  "peer-node-1": {
    "allowed_sender_agent_ids": ["ai:peer-node-1@host", "alice"],
    "allowed_namespaces": ["public/*", "shared/team-x/**"]
  },
  "peer-node-2": {
    "allowed_namespaces": ["public/*"]
  }
}'
```

The env var is a JSON object mapping a claimed peer-id (delivered on
the `x-peer-id` HTTP header) to a `PeerScope`
([`src/federation/peer_attestation.rs:107-118`](../src/federation/peer_attestation.rs)).

- **`allowed_sender_agent_ids`** — exact strings (no glob) the peer
  may claim as `body.sender_agent_id` on `/sync/push`. Empty = peer
  may only author as itself (`body.sender_agent_id == peer-id`).
- **`allowed_namespaces`** — glob patterns matched against
  `Memory::namespace` on `/sync/since`. `*` = single segment, `**` =
  any suffix. Empty = peer may not pull any rows (default-deny).

The attestation core is `attest_sender`
([`src/federation/peer_attestation.rs:247`](../src/federation/peer_attestation.rs))
on the inbound `/sync/push` path and `namespace_allowed`
([`src/federation/peer_attestation.rs:338`](../src/federation/peer_attestation.rs))
on the outbound `/sync/since` path. Both are pure functions over
operator-configured allowlist rows; both default-deny.

A peer without an `x-peer-id` header is rejected with
`peer_id_header_missing` unless one of the bypass envs is set. A peer
that claims a `body.sender_agent_id` not in its allowlist is rejected
with `sender_agent_id_mismatch`.

Pinned by [`tests/federation_b2_hardening.rs`](../tests/federation_b2_hardening.rs),
[`tests/g_issue_238_sender_attestation.rs`](../tests/g_issue_238_sender_attestation.rs),
[`tests/g_issue_239_sync_scope.rs`](../tests/g_issue_239_sync_scope.rs).

## Three new env vars

| Var | Default | Effect |
|---|---|---|
| `AI_MEMORY_FED_PEER_ATTESTATION` | unset → empty allowlist | When set to JSON, populates the per-peer `PeerScope` allowlist. Unset = empty config (default-deny on `/sync/since`, header-must-equal-body on `/sync/push`). |
| `AI_MEMORY_FED_SYNC_TRUST_PEER` | unset (deny) | When set to `"1"`, widens "no scope row" cases on `/sync/since` to legacy full-dump behavior. Once a scope row exists for a peer, its namespace list is the authoritative gate and the bypass is ignored. |
| `AI_MEMORY_FED_TRUST_BODY_AGENT_ID` | unset (deny) | When set to `"1"`, the substrate trusts the wire body's `agent_id` claim instead of the authenticated peer-id. Default: header wins. |

Constants: [`src/federation/peer_attestation.rs:75-88`](../src/federation/peer_attestation.rs).

The default posture is **strict**: an inbound write from an authenticated
peer is treated as the peer's write, not as the underlying agent's
write, unless the operator explicitly opts in to the peer's claim via
the two `TRUST_*` flags. Bypass detection:
`trust_body_agent_id_bypass()` /
`sync_trust_peer_bypass()` at
[`src/federation/peer_attestation.rs:211-220`](../src/federation/peer_attestation.rs).

A malformed `AI_MEMORY_FED_PEER_ATTESTATION` JSON value is treated as
an empty allowlist (default-deny) plus a `tracing::warn!` so the
operator sees the typo immediately
([`src/federation/peer_attestation.rs:171-198`](../src/federation/peer_attestation.rs)).
Refusing to start on a malformed allowlist would be a self-DOS hazard
during config rollouts.

## Quorum + vector clocks

v0.6.x quorum semantics are unchanged: W-of-N writes (default majority),
vector-clock CRDT-lite merge, mTLS allowlist between peers
([`src/federation/quorum.rs`](../src/federation/quorum.rs),
[`src/federation/vector_clock.rs`](../src/federation/vector_clock.rs)).
v0.7.0 adds reflection-aware bookkeeping
([`src/federation/reflection_bookkeeping.rs`](../src/federation/reflection_bookkeeping.rs))
so federated reflection writes carry origin metadata that prevents
depth-cap laundering. `enforce_local_cap_on_derived`
([`src/federation/reflection_bookkeeping.rs:200`](../src/federation/reflection_bookkeeping.rs))
refuses an inbound reflection memory whose derived depth exceeds the
local namespace cap, even if the sending peer's local cap is higher.

## Operator checklist

1. **Generate peer certs.** Use your CA of choice; export the
   SHA-256 fingerprint via
   `openssl x509 -in peer.crt -noout -fingerprint -sha256`.
2. **Populate `peer-fingerprints.allow`.** One fingerprint per line.
   Inline comments (`# label`) and `:` separators tolerated.
3. **Author the peer attestation JSON** and stage it in your secrets
   manager. Treat the file like a config blob, not a credential — the
   contents are operator-configured authorization, not authentication
   material.
4. **Set `AI_MEMORY_FED_PEER_ATTESTATION`** on the receiving daemon's
   environment.
5. **Leave the two `TRUST_*` flags unset** unless your peer mesh is
   under operator-level control (e.g., the in-tree integration tests
   set both — see
   [`src/handlers/mod.rs:822-838`](../src/handlers/mod.rs) for the
   legacy-test bypass installation pattern).
6. **Verify** with
   `curl --cert peer.crt --key peer.key -H "x-peer-id: peer-node-1" \
   https://memory.prod/api/v1/health` — a 200 with `{"status":"ok"}`
   means TLS + mTLS + API key all aligned.
7. **Watch** the daemon log for `peer_id_header_missing` /
   `sender_agent_id_mismatch` lines — those are real rejections.

## Tuning guidance (production deployment runbook)

**Per-peer connection limits.** mTLS allowlist size is bounded only
by the operator's discipline; in practice the substrate has been
exercised at 50-peer cells without measurable handshake overhead.
For >100 peers, consider front-ending with a TLS-terminating proxy
that itself enforces the allowlist (sidecar pattern) so the
ai-memory daemon doesn't carry the X.509 verification cost on every
fresh connection.

**Sync interval.** `spawn_catchup_loop`
([`src/federation/receive.rs:35`](../src/federation/receive.rs))
drives the periodic pull from peers; default cadence is operator-set
via the `FederationConfig` ([`src/federation/peer.rs:30`](../src/federation/peer.rs)).
For small meshes (2-5 peers, modest write volume), 30s is fine. For
large meshes, increase to 60-300s to spread the pull traffic.

**Quorum width.** v0.6.x defaults to majority (`W = ceil(N/2 + 1)`)
which is the correct default for partition-tolerance. For a regulated
deployment where every write must be witnessed by every peer (W = N),
configure explicitly — but be aware that any single-peer outage
becomes a write outage.

**Reflection-depth interop.** When peers run different
`max_reflection_depth` settings, the `enforce_local_cap_on_derived`
function refuses incoming reflections that exceed the **local** cap.
The sending peer's cap is irrelevant. Operators with heterogeneous
mesh configs should pin a mesh-wide depth ceiling in their runbook
to avoid surprise refusals.

## mTLS rotation playbook

1. **Generate new server keypair + cert** on the receiving daemon
   (your CA's standard issuance).
2. **Stage the new cert/key alongside the old**:
   `/etc/ai-memory/server.crt.new` + `/etc/ai-memory/server.key.new`.
3. **For each peer**, issue the new SHA-256 fingerprint and stage it
   alongside the old in `peer-fingerprints.allow` (both fingerprints
   present during the rotation window).
4. **Reload peers' allowlist** (each peer's runbook). Until every
   peer's allowlist accepts both fingerprints, do NOT swap the daemon
   cert — half the mesh will reject the new fingerprint.
5. **Restart the daemon** with the new `--tls-cert` / `--tls-key`.
   The first handshake against the new cert proves the rotation
   landed.
6. **Watch peer-side logs** for handshake failures over the next 24h.
7. **Remove the old fingerprint** from every peer's allowlist after
   the soak period. The deprecated keypair material can now be
   destroyed.

The whole sequence is reversible until step 5; after step 5 the only
rollback is to re-deploy the previous cert (which the old
fingerprint allowlist will still accept on the peer side during the
soak window).

## Cert-revocation procedure

The mTLS allowlist is fingerprint-pinned, not CA-trust-anchored —
**revocation is removal from the allowlist file**, not OCSP/CRL.
Operator procedure:

1. **Identify the compromised peer's fingerprint** (your inventory
   plus `openssl x509 -in <peer.crt> -noout -fingerprint -sha256`).
2. **Remove the line** from `/etc/ai-memory/peer-fingerprints.allow`.
   Leave a `# revoked YYYY-MM-DD by <operator>` comment in the file
   for the audit trail.
3. **Force daemon reload** of the allowlist (today this requires a
   daemon restart — there is no allowlist hot-reload surface yet).
4. **Confirm rejection**: from any host using the revoked cert,
   `curl --cert revoked.crt --key revoked.key https://memory.prod/api/v1/health`
   must fail at the TLS layer.
5. **Remove the peer's row from `AI_MEMORY_FED_PEER_ATTESTATION`** in
   the same change. A future re-issuance under a fresh cert requires
   re-adding both the fingerprint AND the attestation row.
6. **Audit the signed_events chain** with
   `ai-memory verify-signed-events-chain` (see
   [`docs/signed-events-v4.md`](signed-events-v4.md)) over the window
   the revoked peer had access. Tamper detection on the chain bounds
   the blast radius.

## Multi-peer scaling guidance

| Mesh size | Quorum default | Sync cadence | Notes |
|---|---|---|---|
| 2-3 peers | W = 2 (majority) | 30s | Default; small CRDT merge load. |
| 4-10 peers | W = ceil(N/2 + 1) | 30-60s | Catchup loop dominates network use. |
| 11-50 peers | W = ceil(N/2 + 1) | 60-120s | Consider sharding by namespace prefix. |
| 50+ peers | App-level coordinator | 120-300s | At this scale the substrate's
peer-to-peer mesh model is the wrong shape — use a gossip layer or
a proper consensus coordinator and treat each ai-memory daemon as a
leaf. |

Vector-clock storage scales linearly with peer count. The CRDT-lite
merge cost is bounded by row count, not peer count — adding peers
does not asymptotically hurt merge throughput. The blast radius of a
single compromised peer scales with what the operator wired into its
`PeerScope`; default-deny on both `allowed_namespaces` and
`allowed_sender_agent_ids` keeps a compromised peer from authoring as
other agents or pulling unrelated namespaces.

## Troubleshooting

| Symptom | Likely cause | Diagnostic recipe |
|---|---|---|
| Inbound `/sync/push` returns 403 `peer_id_header_missing` | Peer's HTTP client isn't setting `x-peer-id` | Fix the peer's outbound config; the header is mandatory under default-deny. |
| Inbound `/sync/push` returns 403 `sender_agent_id_mismatch` | Body's `sender_agent_id` is not in the peer's allowlist | Either remove the field (peer authors as itself) OR add the claimed value to `allowed_sender_agent_ids` for this peer-id in the JSON. |
| Outbound `/sync/since` returns empty payload | No matching `allowed_namespaces` entry for the requesting peer | Add a glob pattern that matches the namespace the peer is trying to pull. Verify with `namespace_allowed_test_glob`. |
| TLS handshake fails | Peer cert not in `peer-fingerprints.allow` | Recompute the SHA-256 fingerprint and add it (or fix the typo). |
| `AI_MEMORY_FED_PEER_ATTESTATION` parse warning at startup | JSON syntax error in the env var | `echo $AI_MEMORY_FED_PEER_ATTESTATION | jq .` — fix the syntax. Substrate is running in default-deny until you do. |
| Reflections refused as `local_cap_refusal` | Sending peer's depth exceeds local namespace cap | Verify with `enforce_local_cap_on_derived` test. Either bump the local cap or raise the issue with the sending peer's operator. |
| Quorum write hangs | One peer is unreachable; W > available peers | Inspect `tests/federation_b2_hardening.rs` for the timeout shape. Drop the unreachable peer from `FederationConfig` until the outage is resolved. |

## Operator runbook (3am procedures)

**Suspected compromised peer cert.** Follow the cert-revocation
procedure above. Total time-to-revoke from operator confirmation:
~2 minutes (allowlist edit + daemon restart). Audit the
`signed_events` chain afterwards — V-4 detects tamper but does not
remediate; the operator decides whether to roll back affected rows.

**Mesh-wide write failure after env change.** Most likely cause is
`AI_MEMORY_FED_PEER_ATTESTATION` JSON breakage. Look for the
`failed to parse peer-attestation env var as JSON` warning. The
daemon does not refuse to boot on parse failure — it runs in
default-deny, so writes don't error out, they get refused. Restore
the previous env var value, restart, validate with `jq` before
re-rolling.

**One peer's writes are landing under the wrong `agent_id`.** Check
whether `AI_MEMORY_FED_TRUST_BODY_AGENT_ID=1` is set. Default
behavior re-stamps inbound rows with the peer's identity; the bypass
trusts the body's claim. If the env is set unintentionally, unset
it and restart — but expect peer pushes to start failing if the
peers are claiming non-self identities and don't have allowlist rows.

**Hardening sanity check.** Run the federation hardening test suite
locally against a fresh build before any production cert/env
rotation: `AI_MEMORY_NO_CONFIG=1 cargo test --test federation_b2_hardening`
+ `cargo test --test g_issue_238_sender_attestation` +
`cargo test --test g_issue_239_sync_scope`. All green = the
production daemon will refuse the same attack shapes.

## Hardening lineage

- The v0.6.x default was "any TCP peer can push if they speak the
  protocol". The v0.7.0 default is "authenticated peer cert AND
  operator-allowed peer-id AND in-scope namespace OR refusal". Three
  concurrent layers, all enforced.
- The original Ed25519 `signature` column shipped in v0.6.3 was a
  dead column — v0.7.0 fills it via the Ed25519 attestation
  track (H1-H6). Inbound federation re-verifies link signatures via
  `POST /api/v1/links/verify`; see
  [`docs/signed-events-v4.md`](signed-events-v4.md) for the V-4
  audit chain that records each verification outcome.
- The peer attestation work (#238) added body-vs-header claim
  cross-checking. The scope-filter work (#239) added per-peer
  namespace allowlists. Both default-deny; both pinned by their own
  test binaries.

See also: [`docs/MIGRATION_v0.7.md` §"Federation hardening"](MIGRATION_v0.7.md#federation-hardening),
the canonical inventory in
[`docs/internal/v070-feature-inventory.md` §"Feature: Federation hardening"](internal/v070-feature-inventory.md),
the V-4 audit chain that records peer-write events at
[`docs/signed-events-v4.md`](signed-events-v4.md), the governance
pipeline that consumes federated rule writes at
[`docs/governance.md`](governance.md), the hook pipeline that fires
on every inbound peer write at
[`docs/hook-pipeline.md`](hook-pipeline.md), the K8 quotas substrate
that gates inbound peer writes per claimed agent_id at
[`docs/k8-quotas.md`](k8-quotas.md), the K10 SSE approvals path that
streams federated approval requests at
[`docs/k10-sse-approvals.md`](k10-sse-approvals.md), and the sidechain
transcripts whose decompression cap protects against peer zstd-bomb
DOS at [`docs/sidechain-transcripts.md`](sidechain-transcripts.md).
