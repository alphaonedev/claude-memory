# Context-offload substrate primitive

v0.7.0 QW-3 ships the substrate plumbing for an offload+deref store
that v0.8.0 short-term-context-compression will build on. The **full
pattern** (Mermaid canvas, auto-cadence trigger from the recall
pipeline, `node_id` cross-link into `memories`) targets v0.8.0; this
release lands the SQLite/Postgres substrate, the engine in
`src/offload/mod.rs`, and the audit-event wiring so v0.8.0 has the
plumbing to call.

## Why it exists

Long-running agent sessions accumulate large tool-call outputs that
crowd the working window. The Tencent comparison work (absorbed
2026-05-15) showed that promoting these to a substrate-side blob
store, keyed by a short `ref_id` the agent keeps inline, recovers
context budget without losing the verbatim content. This module is
the substrate side of that pattern.

## Surface

`ContextOffloader::offload(content, namespace, ttl_seconds, agent_id)`
returns `{ ref_id, content_sha256, stored_at }`. The `ref_id`
shape is `ofl_<base32-of-sha256-first-8-bytes>` — 13 character
payload, deterministic per content, with a `ofl_` prefix that keeps
the offload class identifiable in audit logs.

`ContextOffloader::deref(ref_id)` returns the original content. The
SHA-256 of the freshly-decompressed bytes is recomputed and compared
to the stored hash before content is surfaced; a tampered row fails
with `OffloadError::IntegrityFailed`. When the offloader is built
with a signer, the Ed25519 signature over the canonical bundle
`{ ref_id, content_sha256, stored_at, namespace }` is verified
before decompression — same encoder family as `identity::sign::
canonical_cbor` (the H2 link signer).

## Storage

`offloaded_blobs` carries the zstd-compressed content (level 3 —
matches `memory_transcripts`), the integrity hash, the Unix-seconds
`stored_at`, an optional `ttl_seconds`, the storing `agent_id`, and
the URL-safe-base64 Ed25519 signature. The partial index
`idx_offloaded_blobs_ttl` covers `(stored_at, ttl_seconds)` only on
rows that have a TTL so the daily sweep is O(expired) rather than
O(total).

## Audit chain

Every `offload` and `deref` call appends a sibling row to
`signed_events` (`event_type = context_offloaded` /
`context_dereferenced`) so the H5 cross-row hash chain captures the
event. The payload-hash input is the canonical CBOR bundle — same
bytes on every host so a downstream auditor re-derives the digest
without diff'ing the mutable `offloaded_blobs` table.

## TTL sweep

`offload::sweep_expired(conn, now, max_per_run, sleep_between_deletes)`
is the daily background task entry point. It removes rows where
`stored_at + ttl_seconds < now`, bounded at `max_per_run` per call
(1000 in the daemon defaults) with a configurable sleep between
deletes (10 ms by default) so the connection lock window stays
short under contended writes.

## Size limits

Per-blob size is bounded by `OffloadConfig::max_offload_blob_bytes`
(default 1 MiB) on the write side and by `MAX_DECOMPRESSED_BYTES`
(16 MiB — same as `memory_transcripts`) on the read side. The read
ceiling defends against a zstd bomb landed by a hostile writer with
direct SQL access.

## What's NOT here (v0.8.0 work)

- Mermaid-canvas integration (the visual short-term-context layout
  v0.8.0 will project onto offloaded blobs).
- Auto-cadence trigger from the recall pipeline (the heuristic that
  decides "this tool output is offload-worthy" without operator
  prompting).
- `node_id` cross-link into the `memories` table (the typed
  reference that lets `memory_replay` walk into offloaded content).

The substrate ships now so the v0.8.0 patches are pure caller
additions — no schema or signing-pipeline churn needed.
