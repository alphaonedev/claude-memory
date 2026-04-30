# ai-memory audit schema v1

PR-5 of issue [#487](https://github.com/alphaonedev/ai-memory-mcp/issues/487).
This document is the **stable, versioned contract** for what
`ai-memory` emits to its security audit trail. SIEMs (Splunk, Datadog,
Elastic, Loki, Sentinel, Chronicle) ingest the lines as plain JSON.
The schema is deliberately **not** bound to any external framework —
not OCSF, not CEF, not LEEF — because a SIEM that can ingest a JSON
line can ingest this one. Mapping into a vendor schema is a one-shot
filter at ingest time.

## Wire format

NDJSON: one JSON object per line. UTF-8. No trailing whitespace.

## Field reference

| Field | Type | Required | Semantics |
|---|---|---|---|
| `schema_version` | `u32` | yes | Always `1` in v1. Bumped only when an existing field's semantics change. Adding optional fields does NOT bump the version. |
| `timestamp` | RFC3339 string (UTC) | yes | When the event was emitted by the binary. |
| `sequence` | `u64` | yes | Per-process monotonic counter starting at 1. Lets a SIEM detect dropped lines independently of the chain check. |
| `actor.agent_id` | string | yes | Resolved NHI agent_id (`ai:<client>@<host>:pid-<n>`, `host:<host>:pid-<n>-<uuid>`, etc.). |
| `actor.scope` | string | optional | Visibility scope: `private \| team \| unit \| org \| collective`. |
| `actor.synthesis_source` | string | yes | How `agent_id` was synthesized: `explicit \| env \| mcp_client_info \| host_fallback \| anonymous_fallback \| http_header \| http_body \| per_request \| default_fallback`. |
| `action` | enum | yes | One of `recall \| store \| update \| delete \| link \| promote \| forget \| consolidate \| export \| import \| approve \| reject \| session_boot`. Adding a variant is non-breaking; renaming or removing one IS breaking. |
| `target.memory_id` | string | yes | Memory id, or `"*"` for sweep operations. Capped at 128 chars. |
| `target.namespace` | string | yes | Memory namespace at action time. Capped at 128 chars. |
| `target.title` | string | optional | Memory title (advisory label, **not content**). Capped at 200 chars; control chars stripped. |
| `target.tier` | string | optional | `short \| mid \| long`. |
| `target.scope` | string | optional | Memory `metadata.scope`. |
| `outcome` | enum | yes | `allow \| deny \| error \| pending`. |
| `auth` | object | optional | HTTP-only auth context. Stdio (CLI / MCP) emissions omit this entirely. |
| `auth.source_ip` | string | optional | Peer IP from the HTTP request. |
| `auth.mtls_fp` | string | optional | SHA-256 fingerprint of the verified client cert. |
| `auth.api_key_id_hash` | string | optional | Hex sha256 (truncated 16 bytes) of the API key id. **Never the raw key.** |
| `session_id` | string | optional | Caller-supplied session correlator. |
| `request_id` | string | optional | Per-request correlator. |
| `error` | string | optional | Sanitized error message. Present only when `outcome = error`. Capped at 256 chars. |
| `prev_hash` | hex string (64 chars) | yes | sha256 of the prior line's `self_hash`, or 64 zeros for the chain head. |
| `self_hash` | hex string (64 chars) | yes | sha256 of every other field in serialization order (with `self_hash` itself zeroed). |

## What is NEVER captured

- `memory.content` — the secret payload of every memory. The schema
  has no `content` field at all. The `redact_content` knob in
  `config.toml` is reserved for a future per-namespace exception API;
  v1 always omits content.
- Raw API keys, raw mTLS private keys, raw passwords. The auth block
  carries hashes only.
- Free-form caller-supplied strings outside the documented fields.

## Version policy

- **Adding** an optional field (`#[serde(skip_serializing_if = ...)]`):
  non-breaking. SIEM parsers tolerate unknown fields.
- **Adding** a variant to `action` or `outcome`: non-breaking. Parsers
  treat unknown enum values as opaque strings.
- **Renaming, removing, or repurposing** any field or variant:
  breaking. `schema_version` increments. Old SIEM parsers SHOULD
  reject events whose `schema_version` they don't recognise rather
  than silently misinterpret.

## Hash chain

```
prev_hash[0]    = "0000…00"          (32 bytes of zeros, hex-encoded)
self_hash[i]    = sha256(canonical_json(event[i]))
prev_hash[i+1]  = self_hash[i]
```

`canonical_json` is `serde_json::to_string` over the event with
`self_hash` cleared (so the field is "self-blinded" — it can be
recomputed without circular dependence). Field order matches the
struct definition, which serde preserves.

`ai-memory audit verify` recomputes each line's `self_hash` and
asserts each `prev_hash` matches the prior line's `self_hash`. Any
mismatch surfaces a precise line number + failure kind and exits 2.

## Append-only OS hint

Best-effort defense in depth, **not** load-bearing:

- **Linux:** `FS_IOC_SETFLAGS` ioctl with `FS_APPEND_FL`. Requires
  `CAP_LINUX_IMMUTABLE`. Filesystem support varies (ext4, xfs, btrfs:
  yes; tmpfs, NFS: no).
- **macOS / FreeBSD / OpenBSD:** `chflags(2)` with `UF_APPEND`.
- **Windows / other:** silently skipped.

If the OS hint cannot be applied, the binary logs a warning via
`tracing::warn!` and continues. The hash chain remains the
authoritative tamper-evidence.

## Threat model

The audit trail defends against:

1. **Silent edit of a past event.** Recomputing `self_hash` over the
   tampered event will not match the stored hash; `audit verify`
   surfaces the offending line number.
2. **Silent insertion of a fake event.** The inserted line's
   `prev_hash` will not match the prior line's `self_hash`.
3. **Silent deletion of a single line.** The next line's `prev_hash`
   will not match the line that's now physically prior.

It does NOT defend against:

- **An attacker with root + write access who rewrites the entire
  file from scratch and re-chains.** This is fundamentally
  impossible to prevent without an append-only WORM-style log
  store; for that compliance level, ship the lines off-host to an
  immutable SIEM in real time and rely on the SIEM's tamper
  evidence. The chain is still useful here because the SIEM can
  cross-check its own ingest record against the on-host file.
- **Truncation of the most recent N lines** (the truncation point
  becomes the new tail and the chain is consistent up to that
  point). Periodic `CHECKPOINT.sig` markers (cadence
  `attestation_cadence_minutes`) bound how much history can be
  silently discarded — a verifier with the prior checkpoint
  signature can detect any rollback past it. v1 emits the marker
  shape; full off-host attestation is reserved for v0.7+.

## Compliance presets

The compliance presets in `[audit.compliance.*]` are pure
configuration. Setting `applied = true` propagates the documented
retention / cadence values to the effective config. When multiple
presets are active simultaneously, the **most-conservative** value
wins (longest retention, shortest attestation cadence).

| Preset | Retention | Attestation | Notes |
|---|---|---|---|
| `soc2` | 730 days | 60 min | Trust Service Criteria CC7.2. |
| `hipaa` | 2190 days (6 yrs) | — | 45 CFR §164.316(b)(2). Pair with `--features sqlcipher` for at-rest crypto. |
| `gdpr` | 1095 days (3 yrs) | — | Reserved `pseudonymize_actors` for v0.7+. |
| `fedramp` | 1095 days | 30 min | NIST SP 800-53 AU-11. |

Note: the binary surfaces the **resolved** retention via
`AuditConfig::effective_retention_days`; operators can verify the
applied policy with `ai-memory doctor` (P7) or by inspecting the
config block at runtime.
