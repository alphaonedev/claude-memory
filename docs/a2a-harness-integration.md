# A2A Campaign Harness Integration Contract — v0.7.0

**GH issue:** #718 (cross-repo).
**Harness repo:** `alphaonedev/ai-memory-a2a-v0.7.0`.
**This repo (substrate):** `alphaonedev/ai-memory-mcp`.

The A2A harness is maintained in a separate repository so that node
provisioning, droplet wiring, multi-agent orchestration, and result
aggregation can iterate independently of the substrate release cadence.
This document pins the **v0.7.0 contract** the harness must honor when
exercising this repo's binary.

## Cross-repo dependency

| Direction | From | To | Contract |
|---|---|---|---|
| binary | `ai-memory-mcp` v0.7.0 | `ai-memory-a2a-v0.7.0` setup script | scp-built `target/release/ai-memory` MUST be used; harness scripts MUST NOT overwrite via `AI_MEMORY_VERSION=0.6.0` tarball download. See #718 root cause §1. |
| config | `ai-memory-mcp` `config.toml` schema v3 | harness rendered config | harness MUST set `schema_version = "3"` and respect every `[agents.defaults]` / `[permissions]` / `[federation]` block. See `src/config.rs`. |
| federation wire | `ai-memory-mcp` substrate | harness 2-of-N or 4-of-N nodes | `x-peer-id` header REQUIRED on every `/api/v1/sync/push` (#716 substrate cure). v0.7.0 also adds `X-Memory-Sig: ed25519=<base64>` on every outbound POST (#791); receivers MAY require it via `AI_MEMORY_FED_REQUIRE_SIG=1` (default in v0.7.0). |
| TLS | optional | harness | `TLS_MODE=mtls` requires `/etc/ai-memory-a2a/tls/server.pem`; `TLS_MODE=off` is acceptable for harness-only smoke runs (no real-world peer exposure). |
| identity | `AI_MEMORY_AGENT_ID` env | harness boot | every harness-spawned `ai-memory` process MUST set a unique `AI_MEMORY_AGENT_ID` (see CLAUDE.md §Agent Identity for the resolution ladder). |

## v0.7.0 substrate guarantees the harness can rely on

- 71 MCP tools at `--profile full`; 7 at `--profile core` + always-on
  `memory_capabilities`.
- 72 HTTP routes registered.
- Schema v43 (sqlite) / 41 (postgres parity ladder).
- Per-message Ed25519 federation signing (`X-Memory-Sig` header).
- Per-peer attestation via `x-peer-id` header on every push.
- 25-field `Memory` model with `reflection_depth`, `memory_kind`,
  `entity_id`, `persona_version`, `citations`, `source_uri`,
  `source_span`, `confidence_source`, `confidence_signals`,
  `confidence_decayed_at`.

## v0.7.0 substrate features the harness MUST NOT depend on

- E2E `encrypted_envelope` field — deferred to v0.7.1-blocker (#228 in
  this repo).
- `memory_mark_outcome` outcome-feedback tool — deferred to v0.7.1-blocker.
- `memory_skill_apply` — deferred to v0.7.1-blocker.

## What "closes" #718

Per the issue body: when the harness repo lands a workflow that
exercises the locally-built v0.7.0 binary end-to-end (NOT a tarball
download of v0.6.0), AND the boot-script env contract is documented,
AND TLS provisioning is either fixed or explicitly opted out, then
#718 closes. None of those changes live in **this** repo; the present
file is the substrate-side handshake.

This file is the canonical cross-repo reference. The substrate-side
ticket (#718) can be closed with a cross-ref to:

- `alphaonedev/ai-memory-a2a-v0.7.0` (harness repo)
- this file
- the v0.7.0 contract pin above

## Provenance

- Operator directive: `28860423-d12c-4959-bc8b-8fa9a94a33d9`
- Triage: `.local-runs/issue-triage-2026-05-18.md`
- Substrate cure for #238 (parent of #717/#791 attestation gap): PR #716
