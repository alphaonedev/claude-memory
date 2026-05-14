# Forensic export (v0.7.0)

> **Status (2026-05-14):** Procurement-grade forensic export ships in
> v0.7.0 across two layers: the L1-3 `verify-reflection-chain` CLI
> (in-DB chain verification with structured JSON output) and the
> L2-5 `export-forensic-bundle` / `verify-forensic-bundle` CLI pair
> (a single deterministic tar an external auditor can re-verify with
> no daemon state and no network). Both surfaces are wired into the
> `AgenticMem Attest` evidence tier. Every claim on this page maps
> to shipped code at commit `c359e89`.

This page documents the operator-facing surface. The substrate
contract for reflection chains sits in
[`RECURSIVE_LEARNING.md`](RECURSIVE_LEARNING.md); the cryptographic
identity model sits in
[`v0.7/rfc-attested-cortex.md`](v0.7/rfc-attested-cortex.md) §Track-H.

## Three CLI verbs

| Verb | Substrate | Wave | Purpose |
|---|---|---|---|
| `ai-memory verify-reflection-chain <memory_id>` | In-DB chain walk | L1-3 | Walk `reflects_on` backward from the supplied memory, verify each Ed25519 signature against the enrolled signing-agent keys, check the `bounded_status` against namespace caps, optionally include `signed_events` provenance, and emit a structured chain-integrity report. |
| `ai-memory export-forensic-bundle --memory-id <id> [--include-reflections] [--include-transcripts] [--output <path>]` | Tar bundle | L2-5 | Build a deterministic, optionally-signed POSIX-ustar tarball containing the target memory, every reflection ancestor reachable via `reflects_on`, the signed edges, the `signed_events` provenance, and (when requested) the transcript-union payloads. |
| `ai-memory verify-forensic-bundle <bundle.tar>` | Out-of-band tar | L2-5 | Re-hash every file in the tar, compare against the manifest's `files[*].sha256`, and (when a signature is present) re-derive the canonical concat and Ed25519-verify against the operator's public key. |

The three verbs compose. A typical compliance flow:

1. Run `verify-reflection-chain` periodically on production data to
   confirm the in-DB chains are still signature-clean (CI-style
   green-light gate).
2. On evidence demand, run `export-forensic-bundle` against the
   memories under review to produce a tar artefact.
3. Hand the tar plus the operator's public key to the external
   auditor.
4. Auditor runs `verify-forensic-bundle <bundle.tar>` on an air-gapped
   workstation. No daemon, no DB, no network — the tar is
   self-contained.

## `verify-reflection-chain` (L1-3)

**Landed in v0.7.0 (L1-3, see [`src/cli/verify.rs`](../src/cli/verify.rs)).**

Walks the `reflects_on` edges backward from `<memory_id>` to depth
`0`, verifies each Ed25519 signature when present, and emits a
structured chain-integrity report. The walker is breadth-first over
the substrate's edge index — no recursive CTE in the user-facing
path, no traversal-order surprises.

### Output formats

- `--format text` (default) — human-readable report to stdout. One
  block per memory id, one line per edge, with verification verdict
  per edge.
- `--format json` — structured `ChainReport` evidence packet
  (`AgenticMem Attest` tier). The wire shape:

```jsonc
{
  "root_id": "<uuid>",
  "n_memories": 7,
  "chain_depth": 3,
  "edges_verified": 6,
  "edges_failed": 0,
  "edges": [
    {
      "source_id": "...",
      "target_id": "...",
      "signature_hex": "<hex>",   // or null when unsigned
      "attest_level": "signed",
      "verified": true
      // "failure_reason": "..."  // present when verified == false
    }
    // ...
  ],
  "max_reflection_depth_per_namespace": {
    "ns/observations": 3
  },
  "bounded_status": "within_cap",  // | "exceeded_cap" | "no_cap_configured"
  "signed_events": [],             // populated when --include-signed-events
  "generated_at": "2026-05-14T12:34:56Z"
}
```

### Exit codes

- `0` — chain fully verified (or no signatures present *and*
  `bounded_status != "exceeded_cap"`).
- `1` — at least one edge failed signature verification, OR the
  chain exceeds its namespace `max_reflection_depth` cap.

### `--include-signed-events`

When passed, the report includes one `SignedEventSummary` per
`signed_events` row covering a memory in the chain — useful for
correlating chain edges against the append-only audit table
(`reflection.created`, `reflection.depth_exceeded`, etc.). Off by
default to keep the report payload small for the common
green-light case.

### What it does NOT do

- Does not mutate any substrate state. The walker is read-only.
- Does not call the LLM. Purely cryptographic.
- Does not call out to the network. Works against a local SQLite
  DB or a postgres URL (when `--features sal-postgres` is
  compiled in).
- Does not exercise the federation receive path. A chain that
  imported from a remote peer is verified using the local
  enrolled keys — the L2-2 `metadata.reflection_origin` stamp
  gives an auditor the upstream-attribution they need to ask the
  origin peer for additional context.

## `export-forensic-bundle` (L2-5)

**Landed in v0.7.0 (L2-5, [commit `bb870b3`](https://github.com/alphaonedev/ai-memory-mcp/commit/bb870b3), [issue #670](https://github.com/alphaonedev/ai-memory-mcp/issues/670)).**

Builds a **deterministic, optionally-signed POSIX-ustar tarball**
the OSS surface for the `AgenticMem Attest` evidence tier. The
acceptance criterion is **byte-identical mod timestamp**: two
builds over the same DB produce identical bytes except for the
RFC3339 `generated_at` field in the manifest.

### Flags

| Flag | Default | Effect |
|---|---|---|
| `--memory-id <ID>` | required | Memory id whose reflection chain to bundle. |
| `--include-reflections` | `false` | Include the target memory + every reachable ancestor via `reflects_on`. When omitted, only the target memory is emitted. |
| `--include-transcripts` | `false` | Include the transcript-union payloads (per L2-4 `replay_transcript_union`). Adds `transcripts/<id>.json` + `transcripts/<id>.content` to the archive. |
| `--output <PATH>` | `forensic-bundle-<short-id>-<rfc3339>.tar` | Output path. Defaults to the working directory. |

### Evidence packet structure

The tarball is a **POSIX ustar** archive (no `tar` crate
dependency — written in-process per the repo's dep-flatness
convention). Layout:

```text
<bundle>.tar
├── manifest.json                       — bundle metadata + per-file SHA-256 + optional Ed25519 sig
├── verification.json                   — L1-3 `verify-reflection-chain` JSON report
├── memories/<id>.json                  — MemoryEnvelope per in-scope memory
├── edges/<src>__<rel>__<dst>.json      — EdgeEnvelope per signed link
│                                         (reflects_on / supersedes / derived_from)
├── signed_events/<event_id>.json       — SignedEventEnvelope per audit row in scope
├── transcripts/<id>.json               — TranscriptEnvelope metadata (when --include-transcripts)
└── transcripts/<id>.content            — raw decompressed UTF-8 body  (when --include-transcripts)
```

Each envelope is a **stable subset** of the substrate row shape — we
deliberately re-emit a pinned schema rather than serialising the
internal `Memory` / `SignedEvent` / `MemoryLink` structs verbatim so
a future struct-field refactor cannot silently break the on-disk
format. Schema bumps are signalled by `manifest.schema_version`
(currently `1`).

### `manifest.json` shape

```jsonc
{
  "schema_version": 1,
  "memory_id": "<uuid>",
  "generated_at": "2026-05-14T12:34:56Z",  // the only field that varies across rebuilds
  "include_reflections": true,
  "include_transcripts": false,
  "files": [
    {
      "path": "memories/abc.json",
      "size": 1234,
      "sha256": "<hex>"
    }
    // ... sorted lexicographically by path
  ],
  "signer_agent_id": "operator@example.com",    // omitted when unsigned
  "signature": "<base64>"                       // omitted when unsigned
}
```

### Byte-identical-mod-timestamp reproducibility

The acceptance criterion from #670 is **byte-identical mod
timestamp**: two builds over the same DB produce identical bytes
except for the `manifest.generated_at` RFC3339 stamp. Mechanisms:

1. **POSIX ustar in-process.** No `tar` crate; the archive writer
   is a small in-tree module that emits a canonical ustar header
   per entry. Every header field (uid, gid, mtime, mode, uname,
   gname) is pinned to a constant — there is no caller-supplied
   filesystem metadata in the archive.
2. **Lex-sorted file emission.** Every file is emitted in
   lexicographic-by-path order via a `BTreeMap` so SQLite row
   order does not bleed into the bytes.
3. **Stable envelope serialisation.** Every envelope struct
   derives `Serialize` with a fixed field order;
   `serde_json::to_vec_pretty` emits in struct-order rather than
   alphabetical.
4. **Per-file SHA-256 in the manifest** so any byte drift surfaces
   immediately on re-verify.

The legitimate non-determinism is `manifest.generated_at`. That
field is explicitly documented as *expected to vary across rebuilds*
and is positioned in the manifest so a downstream diff tool can
ignore it exactly (see the integration test fleet at
[`tests/forensic/`](../tests/forensic/) and
[`tests/forensic.rs`](../tests/forensic.rs)).

### Signature semantics

When the daemon has an AlphaOne operator keypair on disk (default
location: `~/.local/share/ai-memory/keypair.ed25519`), the bundle
gets signed. The signature is over a **canonical concatenation** of
the per-file SHA-256 digests in manifest order. The signer agent id
is written into `manifest.signer_agent_id`; the Ed25519 signature
goes into `manifest.signature` as base64.

When no operator keypair is present, both fields are absent
(`#[serde(skip_serializing_if = "Option::is_none")]`). The bundle is
still useful — the per-file SHA-256s are intact and the chain-level
edge signatures are still verifiable — but the bundle envelope itself
carries no operator attestation. Auditors should treat an unsigned
bundle as "integrity-checkable but not operator-attested."

## `verify-forensic-bundle`

**Landed in v0.7.0 (L2-5, see [`src/cli/export.rs`](../src/cli/export.rs)).**

Re-verifies a tar bundle end-to-end without any daemon state. The
flow:

1. Open the tar, walk every file entry.
2. For each entry whose path appears in `manifest.files`, re-hash
   the body and compare against `manifest.files[path].sha256`.
   Mismatch is an error with a structured reason.
3. Refuse if any path in `manifest.files` is absent from the tar
   ("missing required file") or if any path in the tar is absent
   from `manifest.files` ("unexpected file").
4. When `manifest.signature` is present, re-derive the canonical
   concat of the per-file SHA-256s in manifest order and Ed25519-
   verify against `manifest.signer_agent_id`'s public key
   (out-of-band distribution).

Exit codes:

- `0` — bundle verified (all hashes match; signature verified when
  present; or no signature and the tar is internally consistent).
- `1` — at least one hash mismatch, missing file, unexpected file,
  or signature verification failure.

## AgenticMem Attest tier integration

The `AgenticMem Attest` evidence tier is the procurement-facing
audit story that pairs the v0.7.0 forensic-bundle surface with the
operator-keypair attestation chain. The bundle is the **OSS-side
artefact**; the Attest tier wraps it with:

- An operator-signed bundle (the `manifest.signature` block).
- A `verification.json` block carrying the L1-3
  `verify-reflection-chain` JSON output at the time of bundle
  assembly, so the auditor sees both the in-DB integrity verdict
  *and* the offline-reverifiable artefact in the same envelope.
- A signed-events trace covering the chain so an auditor can
  reconstruct the substrate's tamper-evident audit log for every
  memory in scope.

The substrate side of Attest is the substrate side of v0.7.0
itself — the schema column shapes, the Ed25519 signing pipeline,
the canonical-CBOR `signed_events` payload format, and the
forensic-bundle tar. No additional binary or daemon mode is needed
on the substrate side: any v0.7.0 ai-memory deploy can produce
Attest-tier evidence on demand.

## Cookbook

The reproducibility script for the recursive-learning primitive is
[`scripts/reproduce-recursive-learning.sh`](../scripts/reproduce-recursive-learning.sh).
A typical operator workflow for forensic export is:

```bash
# 1. Verify an in-DB chain.
ai-memory verify-reflection-chain <memory_id> --format json \
  --include-signed-events > chain-report.json

# 2. Build a deterministic evidence bundle (includes ancestors + transcripts).
ai-memory export-forensic-bundle \
  --memory-id <memory_id> \
  --include-reflections \
  --include-transcripts \
  --output bundle.tar

# 3. Off-host verification (no daemon, no DB):
ai-memory verify-forensic-bundle bundle.tar
```

Operators can also exercise byte-identical reproducibility locally:

```bash
# Build twice over the same DB.
ai-memory export-forensic-bundle --memory-id <id> --output a.tar
ai-memory export-forensic-bundle --memory-id <id> --output b.tar

# Tars differ ONLY in manifest.generated_at.
tar -xOf a.tar manifest.json > a.json
tar -xOf b.tar manifest.json > b.json
diff <(jq 'del(.generated_at)' a.json) <(jq 'del(.generated_at)' b.json)
# (no output — manifests are byte-identical except for generated_at)
```

## Operator references

- **CLI implementations:**
  - [`src/cli/verify.rs`](../src/cli/verify.rs) — `verify-reflection-chain`
  - [`src/cli/export.rs`](../src/cli/export.rs) — `export-forensic-bundle` + `verify-forensic-bundle`
- **Bundle builder:** [`src/forensic/bundle.rs`](../src/forensic/bundle.rs)
- **Integration tests:** [`tests/forensic.rs`](../tests/forensic.rs), [`tests/forensic/`](../tests/forensic/)
- **Substrate cross-references:**
  - Reflection chain contract: [`RECURSIVE_LEARNING.md`](RECURSIVE_LEARNING.md)
  - Skill bundle integration: [`agent-skills.md` §Federation behavior](agent-skills.md#federation-behavior)
  - Transcript replay union (L2-4): [`src/transcripts/`](../src/transcripts/)
- **Issue tracker:** [#670](https://github.com/alphaonedev/ai-memory-mcp/issues/670)
- **Commits:** L2-5 merge [`bb870b3`](https://github.com/alphaonedev/ai-memory-mcp/commit/bb870b3); L2-4 transcript-union merge [`a50b34c`](https://github.com/alphaonedev/ai-memory-mcp/commit/a50b34c)
