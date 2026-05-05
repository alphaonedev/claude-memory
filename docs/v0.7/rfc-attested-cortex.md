---
title: "RFC — `attested-cortex` design rationale (v0.7.0)"
status: DRAFT — finalizes at v0.7.0 release
date: 2026-05-05
codename: attested-cortex
target_release: v0.7.0
predecessor_rfc: ../v0.6.4/rfc-default-tool-surface-collapse.md
predecessor_release: v0.6.4 — quiet-tools (shipped 2026-05-05)
supersedes: ../v0.6.5/V0.6.5-EPIC.md (cortex-fluent rolled into this release)
companion_docs:
  - V0.7-EPIC.md
  - v0.7-nhi-prompts.md
  - ../MIGRATION_v0.7.md
  - ../../ROADMAP2.md (§7.3)
authors: AlphaOne (synthesis)
reading_time: ~25 min
---

# RFC — `attested-cortex` design rationale (v0.7.0)

> **One sentence:** v0.7.0 makes the substrate both *more articulate* (loaders that say "load," capabilities that pre-compute their own description, schemas at half the token cost) and *cryptographically trustworthy* (Ed25519 attestation, programmable hooks, AGE-accelerated graph traversal, namespace-inheritance enforcement, A2A maturity). This RFC records **why** each of those decisions was made in the shape it shipped, what was rejected, and what was deferred to v0.8 / v0.9 / v1.0.

**Status:** DRAFT — final pass when all tracks land. Sections that name TODOs reference work-in-progress.
**Date:** 2026-05-05 (kickoff)
**Codename:** `attested-cortex`
**Reading time:** ~25 min

---

## TL;DR

v0.7.0 ships **five interlocking substrates** under one release banner:

1. **Hook pipeline** (Track G) — 20 lifecycle events with subprocess JSON-stdio + daemon-mode IPC; `Allow` / `Modify` / `Deny` / `AskUser` decision contract.
2. **Ed25519 attestation** (Track H) — fills the dead `signature` column shipped in v0.6.3 with real per-agent signatures + an append-only `signed_events` audit chain.
3. **Sidechain transcripts** (Track I) — zstd-3 BLOB store + `memory_replay`, the substrate for R5 auto-extraction.
4. **Apache AGE acceleration** (Track J) — Cypher backend on Postgres-with-AGE; recursive-CTE fallback on SQLite. Bench-gated: AGE p95 must beat CTE p95 by ≥30% at depth=5 to justify shipping.
5. **Permissions + G1 cutline** (Track K) — namespace-inheritance enforcement (the cutline-protected fix), plus the rules+modes+hooks→Decision refactor that replaces the v0.6.x governance subsystem.

Plus the **legibility** rollup absorbed from the canceled v0.6.5 epic: capabilities v3, named loader tools, schema compaction, per-harness positioning.

This RFC records the **four explicit architectural decisions** the V0.7-EPIC calls out, plus the design principles, the substrate dependency graph, the attestation threat model, the performance-budget rationale, the v0.6.x → v0.7 compatibility matrix, the explicit out-of-scope set, and the pointers back to V0.7-EPIC.md / ROADMAP2.md / MIGRATION_v0.7.md / the GitHub issues for the major tracks.

---

## Why this RFC exists

V0.7-EPIC.md is the **operational** doc — what's being built, by whom, in which week, with which definition-of-done. This RFC is the **why** doc — the design rationale that justifies the operational shape. The two are deliberately separate so that:

- New contributors can read the RFC to understand the v0.7 decision space without reading the 1237-line epic first.
- The epic can change freely (task IDs renamed, branches reshuffled, owners reassigned) without invalidating the design rationale.
- Reviewers can challenge the design at a single document instead of chasing decisions across 11 tracks of starter prompts.
- After v0.7.0 ships, this RFC becomes the **historical record** of why `attested-cortex` was shaped the way it was — `v1.0` security-audit reviewers in Q2 2027 should be able to read this and understand the v0.7 threat model without spelunking through a year of commits.

V0.6.4 had a similar split: [`docs/v0.6.4/rfc-default-tool-surface-collapse.md`](../v0.6.4/rfc-default-tool-surface-collapse.md) recorded the **why** of collapsing the default tool surface to 5; `V0.6.4-EPIC.md` recorded the **what** and **when**. That split worked. v0.7 reuses it.

---

## Table of contents

1. [TL;DR](#tldr)
2. [Why this RFC exists](#why-this-rfc-exists)
3. [Design principles carried forward from v0.6.x](#design-principles-carried-forward-from-v06x)
4. [The four architectural decisions](#the-four-architectural-decisions)
   1. [Decision 1 — Why Ed25519 over X25519 + ChaCha20](#decision-1--why-ed25519-over-x25519--chacha20)
   2. [Decision 2 — Why subprocess-stdio + daemon-mode for hooks (vs. dynamic library plugins)](#decision-2--why-subprocess-stdio--daemon-mode-for-hooks-vs-dynamic-library-plugins)
   3. [Decision 3 — Why AGE behind a feature flag (vs. hard dependency)](#decision-3--why-age-behind-a-feature-flag-vs-hard-dependency)
   4. [Decision 4 — Why permissions replace governance instead of augmenting](#decision-4--why-permissions-replace-governance-instead-of-augmenting)
5. [Substrate dependency graph](#substrate-dependency-graph)
6. [Threat model for the attestation layer](#threat-model-for-the-attestation-layer)
7. [Performance budget rationale](#performance-budget-rationale)
8. [Compatibility matrix](#compatibility-matrix)
9. [Out of scope / explicitly deferred](#out-of-scope--explicitly-deferred)
10. [Approval gate](#approval-gate)
11. [References](#references)

---

## Why this release exists — the longer view

Two narratives converge in v0.7.0; both predate this RFC and both shaped the four architectural decisions below.

### Narrative 1 — the legibility gap (`cortex-fluent`, from the canceled v0.6.5 epic)

The 2026-05-05 NHI Discovery Gate verdict on v0.6.4 came back **6/6 PASS, GATE GREEN**. The substrate was doing its job. But three real-world LLM observation cells captured the same day showed a **legibility gap** — reasoning-class LLMs (Grok 4.2 reasoning) didn't find the runtime loader because it lived inside an introspection tool's parameter set instead of being a top-level tool with a name that says "load."

The gap wasn't a bug; it was a labeling failure. The substrate was capable; the language hadn't quite caught up. The `cortex-fluent` epic was scoped as a v0.6.5 release to close the gap with three changes:
1. Promote loaders to first-class tools (`memory_load_family`, `memory_smart_load`).
2. Pre-compute calibration in `memory_capabilities` v3.
3. Compact tool schemas to halve their token cost.

When v0.7.0 absorbed the trust narrative below, the v0.6.5 epic got rolled into v0.7.0 as Tracks A / B / C / D / E. The legibility work became **half the v0.7.0 release**; the trust work is the other half.

### Narrative 2 — the trust gap (`attested`, from ROADMAP2 §7.3)

The v0.6.3 audit cataloged a set of credibility-shaped findings that all pointed at the same theme: the substrate **advertised trust capabilities it did not deliver**.

| Finding | What v0.6.3 advertised | What v0.6.3 actually did | v0.7 closure |
|---|---|---|---|
| **G1** | "N-level rule inheritance" in capabilities | `resolve_governance_policy` checked the leaf only | K1 (cutline-protected fix) |
| **G12** | `memory_links.signature` column in schema | Nothing populated it ("dead column") | Track H (Ed25519 ships) |
| `permissions.mode` | Capabilities reported `"ask"` | Hard-coded constant; gate never read it | K3 |
| `approval.subscribers` | Capabilities reported a count | Hard-zero; no API to subscribe | K4 |
| `hooks.by_event` | Capabilities reported `{}` | Always empty; no event registry | G1 (config landing) |
| `default_timeout_seconds` | Capabilities reported `30` | Reported, never enforced (no sweeper) | K2 |
| `rule_summary` | Capabilities reported `[]` | Always empty | K5 |
| `transcripts.enabled` | Capabilities reported `false` | No capture path | Track I |

The pattern: every advertised-but-missing field had a real product implication. A procurement team reading the capabilities response saw functionality the substrate didn't deliver. Each missing field individually was small; collectively they were the v0.6.3 audit's biggest finding.

v0.6.3.1 closed the **honesty** half of the gap (Capabilities v2 reported honest live state instead of advertised intent). v0.7 closes the **substance** half: every advertised field now has real backing code, and the new headline features (Ed25519, hooks, transcripts, AGE) extend the surface in directions the v0.6.x roadmap had committed to but not delivered.

### Why the two narratives are the same story

A substrate that **says what it does** and a substrate that **does what it says** are two perspectives on the same property: **honest legibility**. The legibility gap (Grok 4.2 not finding the loader) and the trust gap (the dead signature column) are both failures of the substrate to be **what it described itself as**. v0.7.0 closes both at once because they're not two separate problems.

This framing is why the V0.7-EPIC titles the release `attested-cortex` rather than something narrower like "v0.7-trust" or "v0.7-hooks." The codename captures the unified narrative: **the cortex becomes both more articulate and more attested in one release.**

### Why these specific four decisions shape the release

The four architectural decisions documented in this RFC each carry a specific load-bearing role in the unified narrative:

1. **Ed25519 (Decision 1)** — fills the dead column. Without this, the trust narrative is rhetoric.
2. **Subprocess hooks (Decision 2)** — makes the lifecycle programmable. Without this, the substrate cannot ship R3 / R5 / R4 / R6 recoveries.
3. **AGE feature flag (Decision 3)** — accelerates graph traversal where it matters without sacrificing the SQLite default. Without this, KG operations stay slow on the workloads they matter most on.
4. **Permissions replace governance (Decision 4)** — unifies the three decision-shaped surfaces (gate, hooks, approval API) under one shape. Without this, every track ships its own decision contract and they drift apart.

Drop any one of these and the release narrative fragments. Keep all four and the release ships as a single coherent statement: **the substrate is now what it described itself as.**

---

## Design principles carried forward from v0.6.x

v0.7.0 inherits five principles that have governed every shipped release since v0.6.0. These are **not** new policies; they're the framing inside which the four architectural decisions below were made.

### Principle 1 — **Opt-in for new behavior**

Every new capability v0.7.0 introduces ships **off by default**. A v0.6.4 install that upgrades to v0.7.0 with no config changes behaves identically to v0.6.4 at every behavioral boundary the user can observe:

- Hook pipeline → no `hooks.toml` → no hooks fire.
- Ed25519 attestation → no keypair generated → all writes carry `attest_level = "unsigned"` (the v0.6.4 default).
- Sidechain transcripts → no `[transcripts]` config block → no transcripts written.
- Apache AGE → AGE extension not installed → recursive-CTE path (the v0.6.x default) stays in place.
- Permissions → first boot defaults `mode = "advisory"` which preserves v0.6.4 semantics.

**The single exception** is the G1 inheritance fix (K1): for users still on **pre-v0.6.3.1** v0.6.x jumping straight to v0.7.0, parent `Approve` policies will now block child writes. This is a behavior change. It is documented as such in `MIGRATION_v0.7.md`. The mitigation is per-policy `inherit: bool` (default `true`); operators can preserve pre-v0.6.3.1 leaf-only resolution by setting `inherit = false` on specific child policies.

This principle is non-negotiable. Every track lead validates "did I ship this opt-in?" against their definition-of-done.

### Principle 2 — **Never break v0.6.4 SDK clients**

A v0.6.4 SDK (TypeScript or Python) talking to a v0.7.0 server **must continue to work**. The wire formats, MCP tool surfaces, HTTP routes, CLI subcommands, and error shapes that v0.6.4 SDKs depend on stay at the same paths with the same shapes. New fields are **additive**:

- Capabilities v3 ships a **new** `schema_version: 3` top-level field. v2 fields stay where they are. SDKs that pin `schema_version: 2` continue to receive the v2 shape unchanged through all of v0.7.x.
- New tools are registered alongside existing tools. Nothing is removed. Profile membership is additive.
- New columns on existing tables (`signature` filled, `attest_level` populated, `inherit` added) carry safe defaults for legacy callers. SDKs reading those rows see fields they can ignore.

The release-readiness checklist (V0.7-EPIC.md §"Definition of release-ready") includes an explicit **"No SDK regression — existing 0.6.4 SDKs still work against v0.7.0 server."** This is verified in the cross-harness benchmark (D1) and in the Discovery Gate T0 cells (E1-E3) which run against both client versions.

### Principle 3 — **Advisory-first then enforce-on-flag**

When a new constraint is added (a hook chain that can `Deny`, a permission rule, a quota), the **first** wire-up of the constraint defaults to **advisory** mode: the constraint logs but does not block. Operators flip the mode to `enforce` only after observing the advisory logs and confirming the constraint behaves as expected.

This protects against:
- New rules that fire unexpectedly broadly (a poorly-scoped `Deny` that accidentally blocks all writes).
- Hook subprocess implementations that mis-handle the contract (returning `Deny` when they meant `Modify`).
- Migration anomalies where a converted v0.6.x governance rule maps to a permissions-shape that has slightly different semantics.

The v0.6.3.1 honest-Capabilities-v2 disclosure left `permissions.mode = "advisory"` as the literal value the gate would honor when permissions were finally enforced. v0.7.0's K3 closes that loop: the gate now consults `permissions.mode` and changes behavior accordingly. **Default on first migration: `advisory`.** Operators flip to `enforce` after their own validation cycle.

The hook chain follows the same pattern: hook crashes default to **fail-open** (`Allow`); operators opt into `fail_mode = "closed"` per-hook only after observing the hook is stable.

### Principle 4 — **Schema migrations idempotent**

Every schema migration v0.7.0 ships (v20 → v21 for `signed_events`; v21 → v22 for `memory_transcripts` + `memory_transcript_links`; the `inherit` column backfill on `governance_policies`) is **idempotent**. Re-running a migration produces no error and no observable state change. Migrations are tested against:

- A fresh empty DB (the new-install case)
- A real v0.6.4 production-shaped DB snapshot (the upgrade case)
- A DB at the migration's own target version (the re-run case — must no-op)
- A DB with partial migration state (the crash-during-migration case — must complete cleanly)

The `MAX_SUPPORTED_SCHEMA` constant in `cli::boot` is bumped from 20 → 22 in this release. Boot refuses to start on DBs at higher schema versions to prevent older binaries from corrupting newer data. This pattern is unchanged from v0.6.x.

The permissions migration tool (`ai-memory governance migrate-to-permissions`) follows the same discipline at the data layer: dry-run by default; `--apply` commits; re-running after `--apply` is a no-op because already-migrated rows are skipped by content-hash comparison.

### Principle 5 — **Honest disclosure over advertised potential**

v0.6.3 advertised features in `memory_capabilities` v1 that did not exist in code (`memory_reflection: true`, `permissions.mode: "ask"`, `approval.subscribers: 0`, `hooks.by_event: {}`, `rule_summary: []`, `transcripts.enabled: false`, `compaction.enabled: false`). The audit cataloged these as "advertised potential" — fields the substrate told LLMs about but could not deliver.

v0.6.3.1's Capabilities v2 closed that gap: it reported **honest live state** instead of advertised intent. Fields the substrate could not back were either dropped or marked planned-not-implemented.

v0.7.0 inherits this discipline. Every capabilities-v3 field carries real backing code. **In particular**:
- `permissions.mode` is now actually consulted by the gate (K3).
- `default_timeout_seconds` on `pending_actions` is now actually enforced by a 60-second sweeper (K2).
- `approval.subscribers` events are now actually published through the subscription system (K4).
- `rule_summary` is now populated with the ordered list of active governance rules with a one-line summary each (K5).
- `hooks.by_event` is populated when hooks are configured (G1).
- `transcripts.enabled` flips true on the namespace boundary it actually applies to (I1-I3).

The honest-disclosure principle is the reason v0.7 ships so many capability-cleanup tasks alongside the headline features: every advertised-but-missing field gets backed by real code or is removed from the wire.

---

## The four architectural decisions

The V0.7-EPIC calls out four explicit architectural decisions that this RFC must justify. Each is treated as a sub-section with: the decision, the alternatives considered, the threat / fitness criteria, the chosen rationale, and the migration story (where applicable).

---

### Decision 1 — Why Ed25519 over X25519 + ChaCha20

> **The decision:** v0.7.0 ships **Ed25519 signatures** for per-agent identity, link provenance, and the `signed_events` audit chain. v0.7.0 does **not** ship X25519 + ChaCha20-Poly1305 end-to-end memory encryption. The latter is a v0.8 commitment (see ROADMAP2.md §7.6 / existing issue #228).

#### What each primitive does

| Primitive | Purpose | Threat surface it covers |
|---|---|---|
| **Ed25519 (v0.7)** | Digital signatures over canonical-CBOR-encoded link payloads. | Authenticity (who said this), integrity (was this tampered with), non-repudiation (the signer can't credibly deny they wrote this). |
| **X25519 + ChaCha20-Poly1305 (v0.8)** | Authenticated symmetric encryption with ephemeral key exchange. | Confidentiality (third parties can't read this), forward secrecy (compromise of long-term keys doesn't decrypt past traffic). |

These are **different concerns** with **different threat models**. Ed25519 answers "did agent A really write this link, and was it modified after they wrote it?" X25519 + ChaCha20 answers "if a peer in the federation mesh is compromised, can the attacker read memories that flow through it?"

The v0.6.3 audit's "dead column" finding (G12) was about the **first** concern: `memory_links.signature` existed in the schema but nothing populated it. The right fix is signing, not encryption. v0.7.0 ships the signing layer.

The federation-encryption gap is real and it's the **second** concern. It is **not** what v0.7.0 is about. It belongs to v0.8 alongside the CRDT pillar (Pillar 3) where the federation push/pull semantics are also being formalized — encryption design is much easier when the merge semantics are pinned.

#### Alternatives considered

1. **Ship both layers in v0.7.0.** Rejected — doubles the cryptographic surface in a single release, doubles the audit burden, and forces design-by-coincidence on the federation merge semantics that aren't pinned until v0.8 Pillar 3.

2. **Defer Ed25519 to v0.8 too, ship them together.** Rejected — the v0.6.3 dead column is a credibility-shaped finding that procurement teams notice. "We have a signature column that nothing fills" reads as architectural debt. Closing it in v0.7 is the highest-leverage single change for the trust narrative.

3. **Use a different signature primitive (ECDSA P-256, RSA-PSS).** Rejected. Ed25519 is:
   - **Smaller** signatures (64 bytes vs 71 bytes for ECDSA-P256, ~256 bytes for RSA-2048). The `signature` column in `memory_links` already exists at 64 bytes — Ed25519 is the natural fit.
   - **Faster** signing and verification (orders of magnitude faster than RSA, ~2× faster than ECDSA-P256 in the hot path).
   - **Deterministic** — the same input + same key produce the same signature, simplifying the canonical-CBOR + signature-test pattern.
   - **Library-mature** — `ed25519-dalek` is well-audited, no-std-friendly, and ships in the Rust ecosystem at MSRV-compatible versions.

4. **Use a hash-chain instead of per-link signatures (Merkle-tree style).** Considered for the `signed_events` audit table specifically. Rejected for v0.7: the per-link signature model gives **per-record** non-repudiation that a hash-chain doesn't (a hash-chain lets an attacker append to the head; only signatures bind a specific record to a specific signer). The `signed_events` table is append-only at the application layer, which gives chain-style audit semantics on top of per-record signatures — best of both.

5. **Use HMAC instead of asymmetric signatures.** Rejected. HMAC requires a shared secret; in a federation mesh with N agents, that's N×(N-1)/2 secrets to manage. Asymmetric signatures let any agent verify any other agent's writes from a single public key — the natural shape for a federated NHI mesh.

#### What Ed25519 ships in v0.7.0 (Track H detail)

- **H1** — Per-agent keypair management. Operator-supplied; **not** derived from `agent_id`. Stored at `~/.config/ai-memory/keys/<agent_id>.{pub,priv}` with mode 0600 / 0644. CLI: `ai-memory identity generate / import / list / export-pub`.
- **H2** — Outbound signing on every `memory_links` write. Canonical CBOR encoding (RFC 8949 §4.2.1) of `{src_id, dst_id, relation, observed_by, valid_from, valid_until}`; signature stored in the existing column; `attest_level = "self_signed"`.
- **H3** — Inbound verification against `observed_by` claim. Federated link with valid signature → `peer_attested`. Federated link without known public key → accept-and-flag as `unsigned`. Federated link with known key but signature mismatch → reject + log warning.
- **H4** — `attest_level` enum (`unsigned` | `self_signed` | `peer_attested`) + `memory_verify(link_id)` MCP tool returning `{signature_verified, attest_level, signed_by, signed_at}`.
- **H5** — Append-only `signed_events` audit table (schema v21). No UPDATE / DELETE through the application layer. Every signed write also appends to this chain.
- **H6** — End-to-end verification test. Closes the v0.6.3 G12 audit finding (signature column was dead).

#### Why the v0.8 deferred work is the right deferral target

The v0.8 release has **CRDT Pillar 3** as a headline. CRDT merge semantics dictate the encryption design: an `OR-Set` of tags has different forward-secrecy requirements than a `LWW-Register` with attested-identity tiebreak. Designing the encryption layer **after** the CRDT primitives are pinned avoids ratcheting the encryption design twice. (One representative concrete: an LWW-Register's tiebreak field reveals the signer identity in plaintext if you encrypt-then-sign; sign-then-encrypt hides it but requires the tiebreak field to be re-derived after decryption. This is exactly the kind of design-coupling that should be done **once**, after the CRDT primitives are concrete.)

Issue #228 holds the v0.8 scope. The MIGRATION_v0.7.md says explicitly: "End-to-end memory encryption (X25519 + ChaCha20-Poly1305 layer 3 peer-meshed) → v0.8 per existing #228."

#### What this decision does NOT do

This decision does **not** make ai-memory confidential at rest or in flight against a federation peer. **Memories are still readable** by any peer that holds them. Operators who require confidentiality today should:
- Use the existing mTLS layer for federation transport (v0.6.x, shipped).
- Use the OS-level filesystem encryption for at-rest protection.
- Wait for v0.8 #228 for end-to-end peer-meshed encryption.

This is called out explicitly in the threat model section below.

#### What about hardware-backed key storage?

**Out of OSS scope** — TPM / HSM / Secure Enclave key storage is the AgenticMem commercial layer. The OSS provides the *abstraction* (a `KeyStore` trait that can be backed by file, by env var, or by a hardware module); the *certified-managed deployment* is commercial. This is consistent with ROADMAP2.md §7.3 ("Out of OSS scope") and is documented as a comment on `src/identity/keypair.rs`.

The reason for this split: hardware-backed key storage requires per-platform PKCS#11 / TPM2-TSS / WebAuthn integrations that change frequently, require platform-specific cert chains, and have an attached compliance overhead (FIPS, Common Criteria) that the OSS project cannot maintain alongside its monthly release cadence. The OSS provides the trait surface so that commercial overlays can plug in cleanly; the OSS itself ships file-based keys with strict permissions.

---

### Decision 2 — Why subprocess-stdio + daemon-mode for hooks (vs. dynamic library plugins)

> **The decision:** v0.7.0's hook pipeline uses **subprocess execution with JSON-over-stdio framing**, with two execution modes — `exec` (subprocess per fire) and `daemon` (long-lived child with JSON-RPC). It does **not** use dynamic library plugins (`dlopen`/`LoadLibrary`), Wasm modules, or in-process scripting (Lua / Python embedded).

#### Why two modes?

| Mode | When fired | Cost | Hot-path-safe? |
|---|---|---|---|
| `exec` | Subprocess spawned per hook fire; clean shutdown on stdin close | ~5-50 ms spawn overhead per fire | No — only for low-frequency events |
| `daemon` | Long-lived child process; JSON-RPC framed; reconnect on crash; backpressure | ~0.1-1 ms IPC round-trip per fire | Yes — required for `post_recall`, `post_search` |

The hot-path constraint is the **whole point** of having two modes: `post_recall` and `post_search` default to `daemon` mode because spawning a subprocess on every recall would blow the v0.6.3 50ms recall p95 budget by an order of magnitude. Operators can override with `mode = "exec"` for low-volume events (`pre_archive`, `post_promote`, etc.) where simplicity wins over hot-path performance.

#### Alternatives considered

1. **Dynamic library plugins (`dlopen` Rust `cdylib` or C ABI).** Rejected on multiple grounds:
   - **Crash blast radius.** A panic in a `dlopen`'d library takes down the whole `ai-memory` daemon. With subprocess isolation, a hook that segfaults takes down only the hook process; the executor logs a warning, treats the crash as fail-open `Allow` (per the documented contract), and continues. This is critical for the auto-link-detector reference hook (G11) which runs an LLM scoring step — LLM calls are notoriously failure-prone at the process boundary.
   - **ABI fragility.** Rust `cdylib` ABI is not stable across compiler versions; users would have to recompile every hook every time `ai-memory` is rebuilt. C ABI works but forces all hook authors to write FFI shims.
   - **Cross-language.** Subprocess + JSON works **trivially** for hook authors writing in Python, Node, Bash, Go, anything. dlopen forces a C-ABI shim or a language-specific wrapper. The auto-link-detector reference hook (G11) is in Rust today but the `[transcripts]` extraction hook (I5, R5) will likely be in Python (it shells out to an Ollama embedding model that has the cleanest Python bindings).
   - **Concurrency model coupling.** A `cdylib` hook must use the same async runtime as `ai-memory` (`tokio`) or it deadlocks. A subprocess hook is free to use any runtime, threading model, or no concurrency at all.

2. **Wasm modules (Wasmtime / Wasmer).** Considered seriously. Rejected for v0.7:
   - **Sandboxing is good but the cost is wrong shape.** Wasm gives you in-process isolation; subprocess gives you OS-level isolation. For a hook surface that may shell out to LLMs, network endpoints, file system, or other system resources, the OS-level isolation of subprocess is **stronger** than Wasm's component-model isolation. A Wasm hook that wants to call an HTTP API needs WASI-HTTP wired up; a subprocess hook just makes the call.
   - **Tooling gap.** Hook authors can write a subprocess hook in any language with `read stdin / write stdout` and JSON support. Wasm requires the AOT-compile toolchain plus a WASI-compatible runtime in the host build. ai-memory doesn't ship Wasmtime today; adding it for hooks alone would be a ~2 MB binary bloat and a new MSRV burden.
   - **Re-evaluate in v0.9 or v1.0.** The Wasm component model is maturing rapidly; by v0.9 it may be the right fit for the **R8** TOON v2 schema-inference work or for embedded ML inference. Hooks can migrate to Wasm later — the subprocess contract is compatible with a Wasm-host shim that bridges WASI stdin/stdout to the same JSON contract. **No bridge burned by shipping subprocess in v0.7.**

3. **Embedded scripting (Lua, Python via PyO3).** Rejected:
   - **Lua** — small and fast, but the hook surface needs to do non-trivial work (LLM calls, network, embedding lookups). Lua is the wrong tool for that.
   - **Python via PyO3** — would force a Python interpreter into the `ai-memory` binary, blowing the binary size past 30 MB and adding a Python ABI dependency on every install. The MCP server runs on machines that may not have Python at all.

4. **Kafka-style pluggable event log (Redpanda / NATS JetStream / Kafka).** Rejected:
   - Adds a heavyweight infrastructure dependency (broker + zookeeper-equivalent + topic management) for a feature that 90% of users will never enable.
   - The hook contract requires **synchronous** pre-event modification (`Modify(MemoryDelta)` lets a hook rewrite the memory before persist). Event-bus architectures are asynchronous-by-design; making them synchronous re-creates the subprocess problem at higher complexity.
   - Operators who want event-bus integration can write a daemon-mode hook that publishes to their existing broker. The hook contract stays simple; the integration is operator-controlled.

5. **WebHooks / HTTP callbacks.** Rejected for the canonical hook surface (subprocess wins on latency and on the `Modify` contract semantics), but HTTP-callback subscriptions **already exist** as the v0.6.x `subscriptions` system and **continue to work** in v0.7. The two systems compose: hooks fire **before** subscriptions for pre- events, **after** for post- events (per G5). Operators choose hooks for **synchronous** decision-shaped semantics, subscriptions for **asynchronous** notification-shaped semantics.

#### What the `daemon` mode design actually looks like

The daemon mode runs the hook command as a long-lived child process. Communication is JSON-RPC over the child's stdin/stdout, framed with newline-delimited JSON (NDJSON). The framing choice is documented per G3; alternatives considered were length-prefixed JSON (more efficient on large payloads but tooling-hostile) and gRPC over Unix domain sockets (heavier setup; bigger payload overhead). NDJSON wins on developer ergonomics; throughput is dominated by the JSON serialization itself, not the framing.

Backpressure semantics:
- The executor maintains a bounded queue per daemon child (default 100 events).
- If the child can't keep up, the queue drains to deadline; oldest events are dropped first with a structured warning log.
- Drop counts surface in `ai-memory doctor --hooks` (a new subcommand introduced in G3).
- A child crash triggers reconnection with exponential backoff (50 ms, 100 ms, 200 ms, 500 ms, 1 s, then steady at 2 s).

Per-event-class deadlines (G6) bound the chain runtime regardless of mode:
- **Write events** (`pre_store`, `pre_link`, etc.) — 5000 ms class deadline.
- **Read events** (`pre_recall`, `pre_search`) — 2000 ms class deadline.
- **Index events** (`on_index_eviction`) — 1000 ms class deadline.
- **Transcript events** (`pre_transcript_store`, `post_transcript_store`) — 5000 ms class deadline.

Per-hook `timeout_ms` cannot exceed its event class's deadline. Total chain runtime is bounded by the class deadline; hooks that exceed their slice are killed (subprocess SIGKILL on `exec`; daemon connection closed on `daemon`) and treated as fail-open `Allow` per the default fail mode.

#### Crash semantics

By default, hook crashes are **fail-open**: the executor logs a warning, treats the crash as `Allow`, and continues the chain. Operators can flip per-hook to `fail_mode = "closed"` for hooks where a crash should block the operation (e.g., a security-critical signing hook).

The crash semantics are documented per G5 ("Crash fallback tested (default fail-open)"). The rationale: a hook crash is much more likely to be a hook bug than an attempted bypass; treating it as fail-closed by default would create a noisy availability problem with the wrong remediation (rolling back the hook, not investigating an attack). Operators who run security-critical hooks know to flip the flag.

---

### Decision 3 — Why AGE behind a feature flag (vs. hard dependency)

> **The decision:** Apache AGE is detected at Postgres SAL initialization; if present, KG operations route through Cypher; if absent, the recursive-CTE path stays in place. **Both paths ship in v0.7.0.** SQLite operators get full functionality; Postgres-with-AGE operators get the speed boost on graph-heavy workloads. AGE is **not** a hard dependency.

#### What AGE is (briefly)

[Apache AGE](https://age.apache.org/) is a Postgres extension that adds Cypher (the openCypher graph query language) on top of Postgres. It projects relational tables as a property graph and exposes Cypher queries through a `cypher()` function in regular SQL. For depth>2 path traversals on large graphs, Cypher on AGE is typically 2-10× faster than the equivalent recursive CTE on plain Postgres (per the AGE benchmarks; ai-memory will validate this on its own corpus in J8).

#### Why a feature flag, not a hard dependency

| Constraint | Implication |
|---|---|
| The default ai-memory deployment is **SQLite, single-process, no auth**. | A hard Postgres dependency would force every individual-developer install to set up Postgres. That's not acceptable for the v0.6.x → v0.7 backward-compat principle. |
| AGE is not in the default Postgres distribution. | Even Postgres-using operators have to install the AGE extension separately. Forcing them to do so as a v0.7 upgrade requirement is a friction wall. |
| AGE benchmarks are workload-shaped. | On corpora dominated by depth-1 lookups, the AGE win is marginal or negative (AGE setup overhead dominates). On corpora dominated by depth-3+ traversals, AGE wins big. The right default depends on the workload. |
| The v0.6.x recursive-CTE path **already works**. | It's been in production since v0.6.x. Removing it would break existing deployments for the same speed boost AGE gives — bad trade. |

The feature-flag design lets each operator make the deployment-shaped choice that's right for them. The auto-detection (`SELECT * FROM pg_extension WHERE extname='age'`) means there's no configuration to manage; install AGE and restart, the substrate picks up the new path automatically.

#### Alternatives considered

1. **Hard AGE dependency, drop CTE path.** Rejected for the reasons above. SQLite-default users would have no upgrade path that didn't require Postgres setup.

2. **AGE-only on a separate `ai-memory-graph` binary.** Rejected — splits the binary surface, doubles the test matrix, doubles the release-engineering burden, and requires operators to choose a binary at install time before they know their workload shape.

3. **Build our own graph engine in Rust.** Rejected. The amount of work to build a production-quality graph engine that does what AGE does is months of work. AGE is mature, Apache-2.0 licensed, has a real community, and works well. "Don't build what you can adopt" is the right call.

4. **Use `petgraph` for in-memory graph traversal, separate from the SAL.** Rejected for v0.7 — would require all KG state to fit in memory at once. AGE/CTE both query against the persistent store, which means the graph can be larger than RAM. v0.9 considers `petgraph` for **routing decisions** (path-cost heuristics) but not for storage.

5. **`sqlite-vec` plus a graph-shaped extension on SQLite side.** Rejected for v0.7 — `sqlite-vec` is the v0.9 vector-store migration path; piling a graph-shaped extension on top of an unstabilized vector path is too much risk in a single release. The SQLite path stays at recursive CTE for v0.7; v0.9 considers reshaping it.

#### The bench gate (J8) — the quantitative justification

> **AGE-mode p95 must be ≥30% faster than CTE-mode p95 at depth=5 to ship.** If AGE doesn't earn its complexity on the bench, the AGE path is **dropped** from v0.7.

This is non-negotiable. It's a **kill switch** on the AGE work. The reason: shipping a feature flag with a second backend doubles the ongoing test burden (J5 dual-path tests for every KG operation). The doubled burden is only worth carrying if the second backend gives a meaningful performance win.

The 30% threshold was chosen as the smallest improvement that justifies ongoing dual-path maintenance. Below 30%, the ergonomic benefit of "you can use AGE if you want" doesn't pay for the test-matrix burden. At 50%+ (which we expect on graph-heavy corpora), the calculus is obvious. 30% is the floor.

If AGE fails the gate, the response is **not** to fix AGE — the AGE upstream is what it is. The response is to drop the feature flag, ship v0.7 with CTE-only, and revisit AGE in v0.9 alongside the `sqlite-vec` migration.

#### Dual-path test discipline (J5)

For every KG operation that has both an AGE and a CTE implementation (`memory_kg_query`, `memory_kg_timeline`, `memory_kg_invalidate`, `memory_find_paths`), the J5 test harness runs the same query against an AGE-enabled Postgres test DB **and** against a CTE SQLite test DB, and asserts the result sets are **set-equivalent** (order may differ between paths because the two backends have different traversal orders).

The fixture corpus is 200 memories + 800 links covering enough topology to exercise depth-1 through depth-5 traversals. Cyclic and non-cyclic graphs both tested. The test runs only when `AI_MEMORY_TEST_AGE_URL` env var is set; CI's AGE-postgres job sets it.

This discipline is the **safety net** against Cypher-vs-CTE divergence. Without it, an AGE path could silently return different results from a CTE path, and operators on one path would see different memory recall behavior than operators on the other. The dual-path test makes that a CI failure, not a production surprise.

---

### Decision 4 — Why permissions replace governance instead of augmenting

> **The decision:** v0.7.0 refactors the existing `governance` subsystem into `rules + modes + hooks → Decision` with explicit deny-first semantics. The v0.6.x `governance` shape is **superseded** by the v0.7 `permissions` shape; existing rows convert losslessly via `ai-memory governance migrate-to-permissions`. The migration is idempotent, dry-run by default, `--apply` commits.

#### Why a refactor, not an extension

The v0.6.x `governance` system was designed when the substrate had only one decision-shaped surface: the gate that runs before a write. v0.7 introduces three:

1. The **gate** — same as v0.6.x, now consulted with the new shape.
2. The **hook chain** — programmable from Track G; can `Allow` / `Modify` / `Deny` / `AskUser` per-event.
3. The **approval API** — HTTP + SSE + MCP surface for `pending_actions`, with `remember=forever` progressive trust.

These three surfaces were drifting apart in the v0.6.x governance shape:
- The gate consulted leaf-only policies (the G1 cutline issue — fixed in v0.6.3.1 / re-fixed structurally in K1).
- The subscription system advertised `approval.subscribers` but never published to it.
- `permissions.mode = "advisory"` was a hard-coded constant the gate didn't read.

A **refactor** unifies the three surfaces under a single `Decision` shape:

```rust
enum Decision {
    Allow,
    Modify(MemoryDelta),
    Deny { reason, code },
    AskUser { prompt, options, default },
}
```

— which is **the same shape** the hook chain returns (G4). The unification means:
- A hook can produce a `Decision`; the gate consumes it directly.
- The approval API surfaces `AskUser` decisions through HTTP/SSE/MCP.
- The mode flag (`enforce` / `advisory` / `off`) is consulted **once** at the surface where the `Decision` is interpreted, not threaded through three separate code paths.

A non-refactor "extension" approach would have left the v0.6.x governance code path intact and bolted hooks + approval-API on top. The result would have been three diverging definitions of `Decision`-like shapes, three separate inheritance walks, three separate audit-log formats. The refactor pays an upfront cost (the migration tool) for a cleaner ongoing maintenance shape.

#### Deny-first semantics — what changed

v0.6.x governance was **allow-first**: in the absence of a matching policy, the operation was allowed. v0.7 permissions are **deny-first**: in the absence of an explicit allow, the operation defaults to `AskUser` (which surfaces in the approval API) for ambiguous cases, or `Deny` for explicitly-flagged-restricted operations.

This is a behavior change, but it is **mode-gated**: the `permissions.mode` field controls how the gate interprets the `Decision`:
- `mode = "enforce"` — `Deny` blocks; `AskUser` queues an approval; `Allow` allows.
- `mode = "advisory"` — all decisions log; nothing blocks. (Preserves v0.6.4 behavior.)
- `mode = "off"` — gate is bypassed entirely; everything allows. (Preserves pre-governance behavior.)

The default after a permissions migration is `advisory`. Operators flip to `enforce` after observing the advisory logs. This is Principle 3 (advisory-first then enforce-on-flag) in action.

#### The migration story (K11)

`ai-memory governance migrate-to-permissions`:

```bash
ai-memory governance migrate-to-permissions               # dry-run (default)
ai-memory governance migrate-to-permissions --apply       # commit
```

- **Dry-run** prints the proposed `permissions` rows alongside the source `governance` rows. Each pair shows: source-row → target-row, plus a "no-change" annotation if the target already exists at the right hash.
- **--apply** writes the new rows into the `permissions` table. Source rows in `governance_policies` are not deleted in v0.7 (they're orphaned but harmless; the gate stops consulting them once the migration completes). v0.8 may add `governance migrate-to-permissions --cleanup` to drop orphaned rows; v0.7 leaves them in place to support rollback.
- **Idempotent.** Re-running after `--apply` is a no-op. The tool computes a content-hash over each source row and skips rows whose target already exists at that hash. Partial-migration crash recovery: re-run the tool; it picks up where it left off.
- **Lossless.** Every v0.6.x `GovernancePolicy` row maps to exactly one `PermissionRule` row. The mapping is documented in `docs/governance-to-permissions-mapping.md` (TODO until K11 ships).

The release-readiness gate requires the migration tool to round-trip successfully against a real production-shaped DB (a sanitized v0.6.4 prod snapshot). This is verified in CI per K11.

#### Why now (v0.7), not v0.8 alongside CRDTs?

The v0.7 hook pipeline (Track G) **needs** the unified `Decision` shape. Hooks return decisions; the gate consumes decisions; if the gate's decision shape is different from the hook's decision shape, every hook chain has to convert. The refactor has to land in v0.7 or the hook pipeline is built against a deprecated shape.

The CRDT pillar (v0.8 Pillar 3) introduces *merge* semantics that interact with policy decisions (a `Decision` may need to consider both the local and the federated version of a memory). v0.8 will likely **extend** the `Decision` shape (add a `MergeConflict` variant), not refactor it again. Doing the v0.7 refactor first means the v0.8 extension is additive.

#### What this decision does NOT do

It does not introduce a fundamentally new policy language. The rules-DSL stays roughly the same shape (declarative match/action pairs); what changes is **how decisions flow through the system**. Operators who write custom governance rules today will find their rules mostly translate 1:1; the migration tool handles the mechanical conversion.

It does not break the audit log. Every gate decision continues to write an audit-log row; the row gets new fields (the `Decision` variant, the source surface — gate / hook / approval-API) but the existing fields are preserved.

---

## Substrate dependency graph

The five v0.7 substrates (hooks, attestation, transcripts, AGE, permissions) have explicit dependencies on each other and on the v0.6.x foundation. The dependency graph is the operational guide for sequencing — each track's tasks can only start once their predecessors are merged.

### High-level track dependencies

```
v0.6.x foundation
   │
   ├──► A — Capabilities v3 ──┐
   │                          │
   ├──► B — Loader tools ─────┤
   │                          │
   ├──► C — Schema compaction ┤
   │                          │
   ├──► K1 (G1 cutline) ──────┤
   │                          ▼
   │                       Phase 1 done
   │                          │
   │                          ▼
   ├──► G — Hook pipeline ────────────┐
   │     (G1→G2→G3→G4→G5→G6→G7)       │
   │                                  │
   ├──► H — Ed25519 attestation ──────┤
   │     (H1→H2→H3→H4→H5→H6)          │
   │                                  ▼
   │                              Phase 2 done
   │                                  │
   │                                  ▼
   ├──► I — Transcripts ──────────────┐
   │     (I1→I2→I3→I4→I5)             │
   │                                  │
   ├──► J — AGE acceleration ─────────┤
   │     (J1→J2→J3→J4→J5→J6→J7→J8)    │
   │                                  │
   ├──► K — Permissions overhaul ─────┤
   │     (K2→K3→K4→K5→K6→K7→K8→K9    │
   │      →K10→K11)                   │
   │                                  ▼
   │                              Phase 3 done
   │                                  │
   ├──► D — Per-harness positioning   │
   ├──► E — Discovery Gate T0 cells   │
   └──► F — Docs + release ───────────┘
                                      │
                                      ▼
                                  v0.7.0 ship
```

### Within-track dependencies (selected)

#### Track G — Hook pipeline

```
G1 (config schema, hot reload)
  │
  ├──► G2 (20 event types + payloads)
  │      │
  │      ├──► G3 (executor: exec + daemon modes)
  │      │      │
  │      │      └──► G5 (chain ordering + first-deny-wins)
  │      │             │
  │      │             ├──► G6 (per-event-class timeouts)
  │      │             │
  │      │             ├──► G8 (on_index_eviction event wired)
  │      │             │
  │      │             ├──► G10 (pre_recall daemon-mode hook for query expansion)
  │      │             │
  │      │             └──► G11 (R3 — auto-link detector reference hook)
  │      │
  │      └──► G4 (decision types: Allow / Modify / Deny / AskUser)
  │
  ├──► G7 (hot reload integration test)
  │
  └──► G9 (reranker batching — closes G7 audit finding; semi-independent)
```

#### Track H — Ed25519 attestation

```
H1 (per-agent keypair management — CLI)
  │
  ├──► H2 (outbound link signing)
  │      │
  │      ├──► H3 (inbound verification against observed_by)
  │      │
  │      └──► H5 (signed_events audit table — schema v21)
  │             │
  │             └──► H6 (verification end-to-end test)
  │                    │
  │                    └──► closes G12 audit finding
  │
  └──► H4 (attest_level enum + memory_verify MCP tool)
```

#### Track K — Permissions + G1 cutline

```
K1 (G1 inheritance fix — CUTLINE)
  │
  └──► (this fix ships even if everything else slips)

K2 (pending_actions timeout sweeper)
K3 (permissions.mode actually consulted)
K4 (approval-event routing through subscriptions)  ──► depends on G3 subscription integration
K5 (rule_summary populated)

K6 (A2A correlation IDs + ACK + retry + replay)
K7 (subscription reliability — DLQ + replay + HMAC)
K8 (per-agent quotas — RPS + storage caps)

K9 (permission system: rules+modes+hooks→Decision)
  │
  ├──► depends on G4 (decision types)
  ├──► depends on K1 (inheritance walk)
  │
  └──► K11 (ai-memory governance migrate-to-permissions CLI)

K10 (Approval API: HTTP + SSE + MCP, HMAC mandatory)
  │
  ├──► depends on K4 (event routing)
  ├──► depends on K9 (permission decisions to surface)
  │
  └──► HMAC-mandatory wire contract
```

### Substrate-level cross-track dependencies

| From | To | Why |
|---|---|---|
| **G3** (hook executor) | **G4** (decision types) | Executor needs decision contract to know what to deserialize |
| **G3** (hook executor) | **K9** (permissions Decision) | Both consume `Decision`; shared type |
| **G2** (event types) | **G11** (R3 auto-link detector) | Hook reads `PostStore` event payload |
| **G2** (event types) | **I5** (R5 transcript extraction) | Hook reads `PreStore` event payload |
| **H1** (keypair management) | **H2** (outbound signing) | Signing reads from active keypair |
| **H1** (keypair management) | **K10** (Approval API HMAC) | HMAC keys can share the keypair management infrastructure |
| **H5** (signed_events table) | **K10** (Approval API audit) | Approval decisions append to the same audit chain |
| **I1** (transcripts schema) | **I2** (transcript_links join) | Join needs the FK |
| **I1** (transcripts schema) | **I5** (R5 extraction hook) | Hook writes into transcripts |
| **J1** (AGE detection) | **J2-J4** (Cypher implementations) | Implementations gated on detection result |
| **J5** (dual-path tests) | **J2-J4** (each implementation) | Tests assert AGE ≡ CTE per operation |
| **K1** (G1 inheritance) | **K9** (permissions) | Permissions inherit through the same chain walk |
| **K3** (permissions.mode read) | **K9** (permissions decision) | Mode controls how decision is interpreted |
| **K9** (permissions decision) | **K10** (Approval API) | Approval consumes AskUser decisions |
| **K11** (migration CLI) | **K9** (permissions schema) | Migrate-to-permissions writes into the new schema |

### v0.6.x → v0.7 schema migration sequence

```
v20 (v0.6.4 baseline)
  │
  ▼
v21 (H5: + signed_events table, append-only)
  │
  ▼
v22 (I1: + memory_transcripts table + memory_transcript_links join)
  │
  ▼
(K1 + K9 schema additions — backfill existing rows; no new schema version)
  │
  ▼
v0.7.0 deployed
```

`MAX_SUPPORTED_SCHEMA = 22` in `cli::boot`. Boot refuses to start on DBs at higher schema versions (the standard v0.6.x guardrail).

### Mandatory cutline if scope slips

Per V0.7-EPIC.md "What's deferred (out of v0.7.0 scope, per agreement)" and the cutline framing in ROADMAP2.md §7.3, the **mandatory cutline** for v0.7.0 ship is:

```
K1 (G1 inheritance fix — CUTLINE)
+ Track A (capabilities v3)
+ Track B (loader tools)
+ Track G (hook pipeline)
+ Track H (Ed25519 attestation)
+ F1 (migration guide)
+ F5 (release engineering)
```

— roughly 6-8 weeks with one engineer. Tracks I, J, C, D, E can defer to v0.7.1 if scope pressure forces it; the v0.7.0 narrative (`attested-cortex`) still holds without them.

---

## Threat model for the attestation layer

This section enumerates **what Ed25519 signing protects against** and — equally importantly — **what it does not**. The threat model is the contract between v0.7.0 and operators who build on it.

### What Ed25519 signing protects against

#### T1 — Forgery of link provenance

**Threat:** An attacker (a compromised peer in the federation mesh, a malicious local process, or a corrupted backup file) writes a `memory_link` with `observed_by = agent-A` when they are not agent A.

**Mitigation:** Every outbound link is signed with the active agent's private key (H2). Inbound verification (H3) checks the signature against the public key for the claimed `observed_by`. Forged links fail verification; the verifier rejects them and logs a structured warning.

**Coverage:** Strong, **provided** the verifier knows agent A's public key. Public-key distribution is operator-managed in v0.7 (operators share `.pub` files via their existing channel — the AgenticMem commercial layer adds key discovery / rotation; the OSS ships file-based keys with explicit operator distribution).

#### T2 — Tampering with stored links

**Threat:** An attacker modifies the content of a stored `memory_link` row directly in the database (bypassing the application layer).

**Mitigation:** The signature is computed over the canonical-CBOR-encoded link payload (H2). Any modification of the payload invalidates the signature. The `memory_verify(link_id)` MCP tool surfaces the tamper detection at query time; operators can periodically sweep the link store for invalid signatures.

**Coverage:** Strong against **content tampering**. Does not protect against **deletion** of links (an attacker who deletes a link leaves no trace; the audit chain in `signed_events` records that the link was created but not that it was deleted post-hoc — see T6 below).

#### T3 — Replay of a previously-signed link

**Threat:** An attacker captures a valid signed link from the network and replays it to a different recipient or at a different time.

**Mitigation:** The canonical-CBOR encoding includes `valid_from` and `valid_until` timestamps, so replays outside the validity window are caught. Within the window, recipients track seen `link_id`s in the `signed_events` chain; the same `link_id` cannot be inserted twice.

**Coverage:** Strong within the validity window. Recipients **must** check the chain on insert (this is the default code path; bypassing it requires going around the application layer).

#### T4 — Repudiation of a signed write

**Threat:** Agent A wrote a contentious link; agent A later claims they never wrote it.

**Mitigation:** Asymmetric signatures provide non-repudiation: only the holder of agent A's private key could have produced the signature. Provided agent A is the sole holder (file mode 0600 protects against accidental other-process reads; HSM-backed storage in the AgenticMem commercial layer protects against root-level theft), the signature stands as evidence.

**Coverage:** Strong for the OSS file-based key store within the operator's threat model. **NOT** strong against root-level OS compromise of the agent's host (the attacker can read the private key file and sign anything they want as agent A). This is a documented limitation; HSM-backed storage is the AgenticMem commercial mitigation.

#### T5 — Audit chain tampering

**Threat:** An attacker modifies the `signed_events` table to remove evidence of a write or insert fake evidence.

**Mitigation:** The application layer does not expose UPDATE or DELETE on `signed_events`; the table is append-only at the application boundary (H5). An attacker with direct DB access can still modify the table at the SQL layer; the OS-level filesystem permissions on the DB file (and the OS-level audit log on access) are the operator's responsibility.

**Coverage:** Strong at the application layer. Operators who require stronger guarantees should configure DB-level row-level security or use the AgenticMem commercial layer's tamper-evident storage.

### What Ed25519 signing does NOT protect against

This section is **as important** as the protection list above. The contract with operators is honest: signing addresses certain threats and not others. Operators who require properties not in the protection list should plan their deployment accordingly.

#### NT1 — Confidentiality of memories at rest

**NOT covered.** Memories are stored unencrypted in the SQLite / Postgres backend. Anyone with read access to the DB file (or the Postgres connection) can read every memory. Filesystem encryption (FileVault, LUKS, dm-crypt) is the operator's mitigation today; v0.8 #228 adds end-to-end memory encryption (X25519 + ChaCha20-Poly1305).

**This is the single most important non-coverage to communicate.** Operators frequently confuse "signed" with "encrypted." Signing addresses authenticity and integrity; encryption addresses confidentiality. v0.7 ships the signing layer; the encryption layer is v0.8.

#### NT2 — Confidentiality of memories in flight

**NOT covered by Ed25519.** Federation transport encryption is provided by the **mTLS layer** (v0.6.x, shipped). Ed25519 signing protects against an attacker on the path **modifying** the link; it does **not** prevent an attacker on the path from **reading** the link. mTLS handles the read protection.

Operators who run federation without mTLS (the v0.6.x `tls = "off"` configuration) should be aware that link content traverses the network in plaintext. This is unchanged from v0.6.x; v0.7's signing layer is orthogonal.

#### NT3 — Side-channel attacks on the signing key

**NOT explicitly mitigated by the OSS.** Ed25519 implementations vary in their side-channel resistance; the `ed25519-dalek` crate ai-memory uses includes constant-time signing primitives, but the **storage** of the private key in the file system is plaintext (mode 0600). An attacker with the ability to read process memory or the file system can extract the key.

Operators with side-channel concerns should use HSM-backed storage (AgenticMem commercial) which keeps the private key inside the secure module and exposes only sign / verify operations.

#### NT4 — Compromise of an agent's full key set

**NOT mitigated** at the protocol level. If an attacker steals agent A's private key, they can sign arbitrary new links as agent A. The mitigation is **operator-side key rotation**: revoke agent A's public key from the federation, generate a new keypair, redistribute the new public key. Operators who anticipate compromise should document a rotation runbook.

The `signed_events` chain includes a timestamp on every signed event; a key-compromise post-mortem can use the chain to identify the time-window of suspicious signed activity.

#### NT5 — Denial of service via signature verification cost

**Partially mitigated.** Signature verification is computationally cheap (~50 µs per Ed25519 verify on modern hardware), but a flood of invalid signatures could still consume CPU. v0.7 does not include explicit rate-limiting on verification; the K8 per-agent quotas (RPS limits) provide indirect protection.

Operators concerned about verification-DoS should apply per-peer rate limits at the network layer (firewall / API gateway) before traffic reaches the verifier.

#### NT6 — Trust establishment across federation

**Out of v0.7 scope.** Ed25519 signatures verify *authenticity* given a known public key. Establishing the *trust* relationship (how does agent A learn agent B's public key in the first place; how does agent A know agent B's key hasn't been compromised) is a federation-mesh problem orthogonal to signing.

v0.7 ships signature verification; operators distribute public keys through their existing channel. v1.0 considers a federated key-discovery protocol (mDNS for local-network peers; published key-server endpoint for remote peers; cryptographic key-pinning to prevent silent rotation). Until then, operators manage the trust model out-of-band.

#### NT7 — Hardware-level attacks (Spectre, Meltdown, RowHammer, etc.)

**Not mitigated.** These are platform-level threats; ai-memory inherits whatever protection the host OS / CPU vendor provides. Operators with hardware-level concerns should run on platforms with documented mitigations and assume the substrate runs in a hostile execution environment.

### Threat model summary table

| Threat | Vector | Mitigation | Coverage |
|---|---|---|---|
| T1 | Forged link provenance | Ed25519 signing + verification (H2/H3) | **Strong** w/ operator key distribution |
| T2 | Stored link tampering | Canonical-CBOR signature; `memory_verify` (H4) | **Strong** for content; not for deletion |
| T3 | Link replay | `valid_from`/`valid_until` + `link_id` chain check | **Strong** within validity window |
| T4 | Repudiation | Asymmetric sig — only key-holder could sign | **Strong** w/o root compromise of host |
| T5 | Audit chain tampering | App-layer append-only on `signed_events` (H5) | **Strong** at app layer; not at SQL layer |
| NT1 | Confidentiality at rest | **Not covered** — use FS encryption; v0.8 ships E2E | Out of scope |
| NT2 | Confidentiality in flight | mTLS (v0.6.x); not Ed25519 | mTLS-dependent |
| NT3 | Key-extraction side-channels | OSS uses constant-time primitives but plaintext key file | Partial; HSM in commercial layer |
| NT4 | Full key compromise | Operator-side rotation; `signed_events` chain helps post-mortem | Operator responsibility |
| NT5 | DoS via verification cost | Indirect via K8 RPS quotas | Partial; network-layer rate-limit recommended |
| NT6 | Trust establishment | Out-of-band operator key distribution | Out of v0.7 scope; v1.0 considers protocol |
| NT7 | Hardware attacks | Inherited from platform | Out of scope |

This table is the **honest contract** between v0.7.0 and operators. The `MIGRATION_v0.7.md` cross-links to it; the `docs/SECURITY.md` will reproduce it once the security-disclosure policy lands (TODO).

---

## Performance budget rationale

v0.6.3 established a **50 ms recall p95** budget that has been the lighthouse metric for every release since. v0.7 inherits this budget, adds two new budgets (hook chain class deadlines; AGE bench gate), and documents why each budget is set where it is.

### The 50 ms recall p95 budget — why it stays mandatory

The 50 ms recall p95 budget was set in v0.6.3 with three sources of justification:

1. **Human-perceptual latency.** 50 ms is the threshold below which sequential operations feel "instant." Recall is the most-frequent read path in any agent loop; if it crosses 50 ms, the agent loop feels laggy.
2. **Token-economics interaction.** A recall that takes 200 ms is 4× the budget, but the *agent* that's waiting on recall is also stalled — burning model-side tokens on the prompt-cache idle. The compounding cost of a slow recall is much worse than the recall itself.
3. **Cross-harness comparability.** Other MCP servers in the ecosystem (Letta, mem0) advertise sub-100 ms recall on smaller corpora. ai-memory's claim of 50 ms is a competitive marker; losing it weakens the cross-harness positioning.

The budget is enforced by a CI bench gate that runs the recall path against a 10k-memory fixture and asserts p95 ≤ 50 ms. Every PR runs it; regressions block merge.

#### Why daemon-mode is required for hot-path hooks (G3)

`post_recall` and `post_search` are **hot path** events: they fire on every recall / search call. If a `post_recall` hook used `exec` mode, every recall would spawn a subprocess (~5-50 ms overhead) — the recall budget would be blown by the hook alone before any actual work happened.

`daemon` mode amortizes the spawn cost: the child process is started once at hook-config-load time and stays alive across many fires. Per-fire IPC cost is ~0.1-1 ms (JSON serialize + write to pipe + read response + JSON deserialize). At that cost, the hook fits comfortably inside the 50 ms budget even under load.

The G3 NHI starter prompt explicitly mandates: "50ms recall budget preserved when post_recall is daemon-mode." The G6 bench test verifies this on every PR.

#### Why per-event-class deadlines (G6)

Even within `daemon` mode, an unbounded hook chain could accumulate latency. A chain of 10 hooks each taking 5 ms is 50 ms — exactly the recall budget, leaving no headroom for the actual recall work.

The per-event-class deadlines bound the **total** chain runtime:
- 5000 ms write events (operator tolerance for write latency is higher; writes are less frequent)
- 2000 ms read events (writes can wait; reads cannot)
- 1000 ms index events (eviction is a background concern; should not block)
- 5000 ms transcript events (transcript I/O is the slow path; budget reflects it)

Per-hook `timeout_ms` cannot exceed the class deadline; this is a config-validation rule (parse-time error if violated). At runtime, each hook gets `min(class_remaining, hook_timeout_ms)` of the budget; deadline-exceeded hooks are killed and treated as fail-open `Allow`.

This design makes the chain runtime **bounded**: the worst case is the class deadline. Operators can configure aggressive hook sets without risking unbounded recall latency. The CI bench test in G6 deliberately includes a slow hook chain on `post_recall` and verifies recall p95 stays under 50 ms.

### The AGE bench gate (J8) — why 30%

| Threshold | Implication |
|---|---|
| <0% (AGE slower than CTE) | Drop AGE entirely. The complexity of dual-path maintenance is not justified. |
| 0-30% (AGE marginally faster) | Drop AGE for v0.7. The ergonomic benefit of "AGE is supported" doesn't pay for the dual-path test burden. |
| 30-100% (AGE meaningfully faster) | Ship AGE behind the feature flag. The win pays for the maintenance cost. |
| >100% (AGE significantly faster) | Ship AGE; consider making it the default for Postgres in v0.7.1 if operator feedback confirms. |

The 30% floor was chosen as the smallest improvement that **observably** changes the user experience on graph-heavy workloads. Below 30%, the recall-loop latency improvement is hidden inside other variability (network, OS scheduling, cache warmth); above 30%, operators on graph-heavy workloads notice the improvement without instrumentation.

The gate is **kill-switch**, not warning-only. If AGE fails the gate, the AGE path is removed from v0.7 — the dual-path test infrastructure stays (it's reusable for v0.9 `sqlite-vec` work) but the AGE detection and Cypher implementations are stripped. CTE-only v0.7 still ships; AGE revisits in v0.9 alongside the broader storage-layer work.

### The hook overhead budget — why post_recall must be daemon-mode

Recall p95 budget: 50 ms.
Recall p95 actual (v0.6.4 measured): ~23-28 ms depending on corpus size.
Headroom: ~22-27 ms.

If a `post_recall` hook chain consumes >22 ms, it eats the headroom and the recall p95 starts crossing 50 ms. The G6 class deadline of 2000 ms for read events is **way** above this — it's the upper bound, not the operating point. In practice, hook authors are told (in `docs/hooks/`) that `post_recall` hooks should target ≤5 ms; daemon-mode IPC at 1 ms leaves 4 ms for hook logic.

A hook that needs >5 ms on `post_recall` should be redesigned as an **asynchronous** subscription instead — fire-and-forget through the v0.6.x subscriptions system, not a blocking decision through the hook chain. The hook contract is for synchronous decision-shaped semantics; subscriptions are for asynchronous notification-shaped semantics. The two systems compose.

### A2A / federation budget (K6)

A2A messaging (federation push/pull) has its own budget surface, recorded in PERFORMANCE.md per K6:

- 3-node mesh: p95 ACK round-trip ≤ 100 ms
- 5-node mesh: p95 ACK round-trip ≤ 250 ms
- 10-node mesh: p95 ACK round-trip ≤ 500 ms

The numbers reflect typical wide-area network conditions; LAN-mesh deployments will see much better. The bench in K6 records p50 / p95 / p99 across all three mesh sizes; the release-readiness gate requires the bench to run cleanly on a representative test bed.

Replay-protection LRU sizing (10,000 correlation IDs) is chosen so that the LRU never ages out a still-pending message: 10 k IDs at 100 RPS = 100 seconds of replay protection, which exceeds the 30-second TTL on outbound messages (default).

### Quota / rate-limit budget (K8)

Per-agent quotas have two dimensions:
- **RPS limit** (default 100 RPS per agent_id) — enforced at MCP entry. 429 returned on overage.
- **Storage cap** (default 100 MB per agent_id) — enforced on `memory_store` insert. Insert blocked on cap.

These defaults are **conservative**; operators are expected to tune them per their workload. The defaults are chosen to prevent a runaway agent from saturating the substrate while leaving plenty of headroom for normal operation.

### Summary — performance budget table

| Budget | Scope | Limit | Enforcement |
|---|---|---|---|
| Recall p95 | Hot read path | 50 ms | CI bench gate (every PR) |
| Hook chain — write events | Pre/post store/link/etc. | 5000 ms class deadline | G6 timeout enforcement |
| Hook chain — read events | Pre/post recall/search | 2000 ms class deadline | G6 + recall bench |
| Hook chain — index events | on_index_eviction | 1000 ms class deadline | G6 |
| AGE vs CTE p95 | Graph traversal at depth=5 | AGE ≥ 30% faster | J8 bench gate (kill-switch) |
| A2A 3-node ACK p95 | Federation | 100 ms | K6 bench in PERFORMANCE.md |
| A2A 5-node ACK p95 | Federation | 250 ms | K6 bench |
| A2A 10-node ACK p95 | Federation | 500 ms | K6 bench |
| Per-agent RPS | MCP entry | 100 RPS default | K8 enforcement (429 on overage) |
| Per-agent storage | memory_store | 100 MB default | K8 enforcement (insert blocked) |

---

## Compatibility matrix

The compatibility matrix is the operator's contract: which combinations of server version + SDK version + feature opt-ins behave correctly together.

### Server / SDK compatibility

| Server version | SDK 0.6.3 | SDK 0.6.4 | SDK 0.7.0 | Notes |
|---|---|---|---|---|
| **v0.6.3** | ✅ | ✅ (additive only) | ⚠️ (v0.7 SDK may call missing tools) | v0.6.3 lacks loaders, AGE, attestation |
| **v0.6.4** | ✅ | ✅ | ⚠️ (v0.7 SDK may call missing tools) | v0.6.4 lacks all v0.7 features |
| **v0.7.0** | ✅ (v2 fields preserved) | ✅ (v2 fields preserved; v3 ignored) | ✅ (full feature set) | Wire compat preserved through v0.7.x |

**Forward compat explanation:** the v0.7.0 server preserves v0.6.4 wire formats (v2 capabilities response, all existing tool surfaces, all existing HTTP routes). A v0.6.4 SDK calling a v0.7.0 server sees the same shapes it saw before; new fields are present in responses but the SDK ignores them. New tools (loaders, `memory_find_paths`, `memory_replay`, `memory_verify`, `memory_approval_*`) are present in `tools/list` but the v0.6.4 SDK doesn't call them; no harm.

**Backward compat caveat:** a v0.7.0 SDK calling a v0.6.4 server may attempt to call tools that don't exist (`memory_find_paths` etc.) — the SDK handles this with a clean `ToolNotFound` error, not a hard failure. SDK consumers on v0.7.0 should feature-detect via `memory_capabilities` before calling new tools.

### Feature interaction matrix

This matrix shows which v0.7 features **interact** with each other and how. Operators enabling multiple features should consult this table.

| Feature ↓ / Interacts with → | Hooks | Attestation | Transcripts | AGE | Permissions |
|---|---|---|---|---|---|
| **Hooks** | — | Hooks can fire on `post_link` carrying signed link payload (attest_level visible to hook) | `pre_transcript_store`, `post_transcript_store` events fire | No interaction | Hook decisions feed into permissions Decision contract (G4 ↔ K9) |
| **Attestation** | — | — | Transcript writes can be signed (uses same keypair) | No interaction | Approval API HMAC can share keypair management infra |
| **Transcripts** | — | — | — | No interaction | Transcript namespaces inherit permissions through K1 chain walk |
| **AGE** | — | — | — | — | KG queries respect permissions on result-set filtering |
| **Permissions** | — | — | — | — | — |

The diagonals are intentionally blank; the table is symmetric, so only the upper triangle is filled.

### v0.6.x → v0.7 migration paths

#### Path A: Direct upgrade (v0.6.4 → v0.7.0)

Most users. Steps documented in MIGRATION_v0.7.md §"Upgrade steps". Schema migration v20 → v22 runs automatically. No data loss; existing memories, links, governance policies all carry forward.

```
v0.6.4 binary → v0.7.0 binary
        │
        ▼
schema v20 → v21 → v22 (idempotent; auto-runs on first start)
        │
        ▼
memory_capabilities v3 served alongside v2 (backward compat preserved)
        │
        ▼
ai-memory governance migrate-to-permissions (dry-run by default)
        │
        ▼
ai-memory governance migrate-to-permissions --apply (when ready)
```

#### Path B: Pre-v0.6.3.1 v0.6.x → v0.7.0 (skip v0.6.3.1 / v0.6.4)

Behavior change: G1 inheritance fix means parent `Approve` policies now block child writes. Mitigation: per-policy `inherit: bool` (default `true`); operators who relied on leaf-only resolution can set `inherit = false` on specific child policies before upgrade.

#### Path C: v0.6.3 → v0.7.0 (skip v0.6.4)

Capabilities v3 lands without v2 having shipped on this install. The server returns v2 + v3 alongside on first read; SDKs receive both. No special action required.

#### Path D: Fresh v0.7.0 install (new operator)

No migration. Schema starts at v22. `memory_capabilities` returns v3 by default. Operator generates Ed25519 keypair as part of `ai-memory init` if they want signed writes.

### Distribution channels

v0.7.0 ships through 5 distribution channels (per V0.7-EPIC.md §"Definition of release-ready"):

| Channel | Audience | Verification |
|---|---|---|
| GitHub release | Direct downloads, CI consumers | SHA256SUMS published as release asset |
| Homebrew | macOS / Linux developers | Formula in `homebrew-ai-memory` tap |
| GHCR (container) | Container deployments | Multi-arch (amd64, arm64) |
| COPR (Fedora) | Fedora / RHEL / CentOS users | Per-distro RPM |
| crates.io | Rust ecosystem; `cargo install` | Same SHA as GH release |

OIDC SDK publish via `publish-sdks.yml` workflow handles npm + PyPI for the TypeScript and Python SDKs respectively.

---

## Out of scope / explicitly deferred

This section restates the V0.7-EPIC's "Non-goals" / "What's deferred" with the v0.8 / v0.9 / v1.0 deferral targets. Each deferred item links to its target release and the reason for deferral.

### Deferred to v0.7.1 (post-ship patch release)

| Item | Why deferred from v0.7.0 |
|---|---|
| **A2A test scenarios full sweep** | K6 ships the ACK + retry + replay layer; the full scenario sweep (S25-S40 equivalents for federation) is operationally large and can land post-ship. |
| **Per-agent quotas full enforcement** | K8 ships the basic RPS + storage caps; per-tool quotas (different limits per MCP tool) defer. |
| **Full governance-to-permissions migration polish** | K11 ships the migration tool; field-by-field migration of every edge-case governance policy shape lands in v0.7.1 once operator feedback identifies the edges. |
| **Per-agent profile pre-warm (NHI guardrails phase 2)** | Depends on #238 (mTLS body-claimed `sender_agent_id` attestation) which is itself a v0.7+ commitment. |

### Deferred to v0.8 (`coordination-primitives`)

| Item | Why v0.8 |
|---|---|
| **R4 — Curator CLI surface** | Pillar 2.5 (compaction pipeline) is the natural home; the curator daemon wraps Pillar 2.5 + Bucket 0 hooks into a single operator-visible surface. |
| **R6 — Consensus memory truth-determination** | Pillar 3 (CRDTs) is the natural home; consensus rules use the LWW-Register tiebreak primitive that Pillar 3 introduces. |
| **End-to-end memory encryption (X25519 + ChaCha20-Poly1305)** | See Decision 1. CRDT merge semantics dictate the encryption design; doing both at once is the right sequencing. Tracked in #228. |
| **Distributed task queue** | Pillar 1 of v0.8; not relevant to attestation. |
| **Typed cognition (Goal / Plan / Step / Observation / Decision)** | Pillar 2 of v0.8; promote-as-state-machine is part of it. |
| **CRDT four-primitive set** | Pillar 3 of v0.8. |
| **Compaction pipeline (six-stage)** | Pillar 2.5 of v0.8. |
| **R3 / R5 absorbed into reference hook implementations** | The substrate (hook pipeline) ships in v0.7; the **R4 curator CLI** that wraps R3 + R5 ships in v0.8. |

### Deferred to v0.9 (`skill-memories`)

| Item | Why v0.9 |
|---|---|
| **R8 — TOON v2 schema inference** | Has a v0.9 slot or a formal-cut decision; needs the v0.8 typed-cognition pillar before the schema-inference layer is grounded. |
| **Pool-of-N reranker batching** | G9 ships single-pass batching in v0.7; pool-of-N is the v0.9 optimization alongside default-on rerank. |
| **Long-term per-namespace HNSW shard** | `sqlite-vec` migration is the v0.9 storage-layer work; per-namespace sharding lands with it. |
| **HNSW persistence to disk** | G3 audit finding; v0.9 work alongside `sqlite-vec`. |
| **BertModel pool sized to physical CPU count** | Prerequisite for default-on reranker; v0.9 work. |
| **Fail-loud reranker fallback** | G8 audit finding; v0.9 alongside default-on. |
| **Default-on cross-encoder reranker** | The headline of v0.9 reranker work. |
| **Skill memories as first-class type** | Headline of v0.9. |
| **Function calling in `llm.rs`** | Curator pass uses tool-calling protocol; v0.9 work. |
| **Streaming tool responses** | Long-running MCP tools; v0.9. |

### Deferred to v1.0 (`federation-maturity`)

| Item | Why v1.0 |
|---|---|
| **API stability guarantee** | v0.x is explicit-instability; v1.0 is the stability commitment line. |
| **Public security audit** | Audit needs the full attestation + permissions + federation surface stable; v1.0 is the right time. |
| **mDNS auto-discovery for federation** | Requires the federation maturity work that v0.8 and v0.9 contribute to. |
| **MVCC strict-consistency mode** | CRDTs from v0.8 remain default; MVCC opt-in arrives in v1.0. |
| **OpenTelemetry standardization** | Internal tracing converts to OTel spans; v1.0 is the discipline line. |
| **Memory Portability Spec v2** | Multi-implementation interop tests; v1.0 work. |
| **Strict semver discipline** | Breaking changes require major-version bumps from v1.0. |
| **Federated key-discovery protocol** | NT6 in the threat model; v1.0 considers a protocol; v0.7 leaves it operator-managed. |

### Permanently out of OSS scope

| Item | Rationale |
|---|---|
| **Hardware-backed key storage (TPM / HSM / Secure Enclave)** | AgenticMem commercial layer. The OSS provides the abstraction (the `KeyStore` trait); the certified-managed deployment is commercial. |
| **Compliance certification (FIPS-140, Common Criteria, etc.)** | AgenticMem commercial layer. The OSS is too release-cadence-fast for the compliance review cycle. |
| **24x7 SLA support** | AgenticMem commercial layer. |
| **Curated skill marketplace operations** | AgenticMem commercial layer. The skill marketplace **protocol** is OSS (v1.x scope); the curated marketplace operations are commercial. |
| **Hosted multi-tenant federation hub** | AgenticMem commercial layer. The federation **protocol** is OSS; the hosted hub operations are commercial. |

### Removing v0.6.4 capabilities v2

**Not deferred — explicitly preserved.** v0.6.4's v2 capabilities surface stays at its current path with its current shape through all of v0.7.x. v3 is **additive**. SDKs that pin v2 continue to work. v3 ships in additive layers; v2 fields are not removed.

This is documented in MIGRATION_v0.7.md and re-stated in Compatibility matrix §"Server / SDK compatibility" above. It is the most-frequently-asked compat question; the answer is unambiguous: **v2 stays.**

---

## Reference design — canonical-CBOR encoding for link signing

The v0.7 link-signing payload is a **canonical** CBOR encoding (per RFC 8949 §4.2.1) of the link's identity-bearing fields. This section documents the encoding choice in detail because it is the single most security-sensitive design surface in Track H and operators implementing alternate clients (e.g., Python or TypeScript SDK signing) need a precise specification.

### Why canonical CBOR (not JSON, not Protobuf, not raw bytes)

| Format | Pro | Con | Verdict |
|---|---|---|---|
| **Canonical CBOR** | Deterministic encoding (same input → same bytes); compact; cross-language libraries; explicit canonicalization rules in RFC 8949 §4.2.1 | Less familiar to web devs; hex-debug only | **Chosen** |
| JSON | Universal familiarity | Non-deterministic key ordering; whitespace ambiguity; integer-vs-float ambiguity; multiple canonical forms in the wild | Rejected — canonicalization is a tarpit |
| Protobuf | Typed schema | Field-tag changes break signature validity; proto3 has no canonical encoding spec | Rejected — wire format too fragile for signing |
| Raw bytes (length-prefixed concat) | Minimal overhead | Custom canonicalization; hard to extend; format is not self-describing | Rejected — extensibility cost |
| MessagePack | Compact, deterministic-ish | No canonicalization spec; inherits same ordering ambiguity as JSON | Rejected — RFC-grade canonicalization spec is the differentiator |

CBOR's RFC 8949 §4.2.1 canonicalization rules are explicit:
1. Map keys sorted by their CBOR-encoded byte form (lexicographic on bytes).
2. Integer encoding uses the smallest representation that fits.
3. No indefinite-length items.
4. No tags (signing payload is plain data).
5. No floating-point unless explicitly required (the link payload is all-integer / all-string).

These rules eliminate canonicalization ambiguity at the format level — the same input always produces the same byte sequence regardless of implementation language.

### Field set and ordering

The canonical CBOR encoding is a map with the following keys (encoded in lexicographic byte-order, which for these short ASCII keys is alphabetical):

```
{
  "dst_id":      "<memory_id of dst, UTF-8 string>",
  "observed_by": "<agent_id, UTF-8 string>",
  "relation":    "<relation type, UTF-8 string>",
  "src_id":      "<memory_id of src, UTF-8 string>",
  "valid_from":  <unix-millis, integer>,
  "valid_until": <unix-millis or null, integer-or-null>
}
```

**Why this field set:**
- `src_id` + `dst_id` + `relation` define the link's semantic identity.
- `observed_by` ties the signature to the claimed signer (verifier checks it matches the public key the verifier holds for that agent).
- `valid_from` + `valid_until` prevent replay outside the validity window.

**Why NOT this field set:**
- `confidence` is excluded — it can be re-derived by consumers (open question 1 in the open-questions section).
- `created_at` is excluded — `valid_from` is the semantic equivalent.
- `updated_at` is excluded — links are immutable in v0.7; updates produce new links with `derived_from` chains.
- `link_id` is excluded — it's a content-derived identifier (hash of the canonical payload); including it in the signed payload would create a circular dependency.

### Worked example

Given a link with:
- `src_id = "mem_abc123"`
- `dst_id = "mem_def456"`
- `relation = "related_to"`
- `observed_by = "ai:claude-code@host:pid-12345"`
- `valid_from = 1746468000000` (2026-05-05 12:00:00 UTC, ms)
- `valid_until = null`

The canonical CBOR encoding is (hex):

```
A6                                              # map of 6 pairs
  66 64 73 74 5F 69 64                         #   "dst_id"
  6A 6D 65 6D 5F 64 65 66 34 35 36             #   "mem_def456"
  6B 6F 62 73 65 72 76 65 64 5F 62 79          #   "observed_by"
  78 1F 61 69 3A 63 6C 61 75 64 65 2D ...      #   "ai:claude-code@host:pid-12345"
  68 72 65 6C 61 74 69 6F 6E                   #   "relation"
  6A 72 65 6C 61 74 65 64 5F 74 6F             #   "related_to"
  66 73 72 63 5F 69 64                         #   "src_id"
  6A 6D 65 6D 5F 61 62 63 31 32 33             #   "mem_abc123"
  6A 76 61 6C 69 64 5F 66 72 6F 6D             #   "valid_from"
  1B 00 00 01 8B 0D EA D2 80                   #   1746468000000
  6B 76 61 6C 69 64 5F 75 6E 74 69 6C          #   "valid_until"
  F6                                            #   null
```

The Ed25519 signature (64 bytes) is computed over this byte sequence; verification recomputes the canonical CBOR from the link's database row and verifies the signature against the stored bytes.

### SDK implementation notes

TypeScript SDK uses [`cbor-x`](https://www.npmjs.com/package/cbor-x) with `useRecords: false` and explicit key sorting. Python SDK uses [`cbor2`](https://pypi.org/project/cbor2/) with `canonical=True`. Both SDKs ship a unit test that compares their canonical encoding to a known-good byte sequence generated by the Rust reference implementation, ensuring cross-language signature compatibility.

---

## Reference design — auto-link detector hook (G11 / R3)

The auto-link detector is the **canonical reference** for what a daemon-mode `post_store` hook looks like in v0.7. Other R-series recoveries (R5 transcript extraction in I5; future R-series in v0.8) follow the same pattern.

### What the hook does

On every successful `memory_store`:
1. The hook receives the just-stored `Memory` payload via daemon-mode JSON-RPC over stdin.
2. It queries `ai-memory` for neighbors via `memory_recall` (using a thin Rust SDK client).
3. For each neighbor, it computes cosine similarity between the new memory's embedding and the neighbor's embedding.
4. It applies two heuristics:
   - **`related_to`** — if cosine > 0.85 and neighbor is in the same namespace.
   - **`contradicts`** — if cosine in (0.6, 0.85) and content has obvious negation aligned with neighbor positive form.
5. For each proposal, it emits `memory_link(...)` calls back through `HookDecision::Modify`, so the chain persists the links transactionally with the original store.
6. It emits metrics: `proposals_emitted`, `links_persisted`, `conflicts_detected`.

### Why this is a daemon-mode hook

Spawning a subprocess on every `post_store` would blow the write budget (5000 ms class deadline is the upper bound, but typical write latency is ~10-30 ms; subprocess spawn would multiply that). Daemon mode means the auto-link-detector child process stays alive across many fires, amortizing the spawn cost.

The auto-link-detector additionally needs to maintain:
- A long-lived embedding cache (don't re-embed neighbors on every fire).
- A reusable HTTP client to the `ai-memory` server for `memory_recall` queries.
- Connection pooling for the LLM scoring step (if the operator wires one in).

These are exactly the resources that benefit from process-level persistence; daemon mode is the right execution shape.

### Why the hook is in `tools/auto-link-detector/`, not in the main `ai-memory` binary

Three reasons:
1. **Decoupled release cycle** — the hook can iterate independently of the main `ai-memory` release cadence. A heuristic tweak doesn't require an `ai-memory` minor-version bump.
2. **Operator opt-in clarity** — `tools/auto-link-detector/` makes it visible that this is a separate component the operator chose to enable, not a default behavior.
3. **Reference for downstream hook authors** — operators who want to write their own hooks can copy the auto-link-detector layout (Cargo.toml, daemon-mode framing, `HookDecision::Modify` emission) as a starting point.

The hook ships in the `ai-memory-mcp` repo under `tools/auto-link-detector/` per V0.7-EPIC G11. The docs entry at `docs/hooks/auto-link.md` covers operator opt-in instructions.

### The R3 / R5 recovery pattern

R3 (auto-link inference) and R5 (auto-extraction from conversations) are both v0.6.x charter commitments that vanished in earlier roadmap revisions. v0.7 recovers them by **shipping the substrate** (the hook pipeline) and **shipping the reference implementations** (G11 for R3; I5 for R5). This recovery pattern is documented in ROADMAP2.md §"Recoveries":

| Recovery | Substrate | Reference impl |
|---|---|---|
| R3 — auto-link inference | G — hook pipeline | G11 — `post_store` daemon hook |
| R5 — auto-extraction | I — transcript pipeline | I5 — `pre_store` transcript hook |
| R4 — curator CLI | (deferred to v0.8 Pillar 2.5) | (wraps R3 + R5 + Pillar 2.5 compaction) |
| R6 — consensus memory | (deferred to v0.8 Pillar 3) | (CRDT four-primitive set) |

The R3 / R5 reference implementations are **opt-in per namespace** — the substrate ships in v0.7, but operators must explicitly enable the hooks via `hooks.toml`. This is consistent with Principle 1 (opt-in for new behavior).

---

## Reference design — A2A correlation IDs and replay protection

The A2A maturity work in K6 introduces correlation IDs, ACK semantics, and replay protection. This section documents the design in enough detail that operators planning federation deployments can reason about the wire shape.

### Wire shape

```rust
struct A2AMessage {
    correlation_id: Uuid,           // UUIDv4, generated by sender
    sender: String,                  // sender agent_id
    recipients: Vec<String>,         // 1 or more recipient agent_ids
    payload: serde_json::Value,      // arbitrary JSON
    expires_at: i64,                 // unix-millis; default sender_now + 30_000
}

struct A2AAck {
    correlation_id: Uuid,            // matches the message it ACKs
    recipient: String,               // ACKing agent_id
    accepted: bool,                  // false on duplicate or expired
    reason: Option<String>,          // "duplicate" | "expired" | None
}
```

### Sender state machine

```
Send(message) ──► Wait for ACK (TTL = expires_at - now)
                   │
                   ├── ACK received: done
                   │
                   ├── No ACK before TTL: retry (up to 3×, exponential backoff)
                   │
                   └── 3 retries exhausted: log warning, give up
```

Exponential backoff schedule: 1 s, 5 s, 25 s. Total time-to-give-up: ~31 seconds + initial TTL.

### Receiver state machine

```
Receive(message) ──► Check correlation_id in seen-LRU
                      │
                      ├── Already seen: ACK accepted=false reason="duplicate"
                      │
                      └── Not seen: Check expires_at vs now
                                    │
                                    ├── Expired: drop, log, no ACK sent
                                    │
                                    └── Fresh: process; ACK accepted=true; insert into seen-LRU
```

Seen-LRU sized at 10 k entries (configurable). At default 100 RPS this gives ~100 seconds of replay protection — well above the default 30 s TTL.

### Why UUIDv4

UUIDv4 (random) over UUIDv1 (timestamp + MAC):
- Privacy: doesn't leak sender MAC or generation time.
- No clock-skew concerns: random UUIDv4 doesn't depend on synchronized clocks.
- Collision resistance: 2^122 random space; collision probability is negligible at any operational scale.

UUIDv4 over a counter:
- Counters require coordinated state across reconnects; UUIDv4 is stateless.
- Counters leak message volume; UUIDv4 doesn't.

UUIDv4 over content-hash:
- Content-hash makes equivalent messages have the same ID, defeating replay protection (a sender re-sending the same payload intentionally would be flagged as a replay attack).
- UUIDv4 makes every send a fresh ID by construction.

### HMAC mandatory on the approval API (K10)

The v0.7 design makes HMAC signing **non-optional** on the Approval API surface (HTTP + SSE + MCP). The rationale:

1. **The approval API is the single most security-sensitive surface in v0.7.** Approval decisions can release queued operations that the gate flagged as ambiguous; an unauthenticated approval is functionally a privilege escalation.
2. **HMAC is cheap.** The verification cost is negligible compared to the cost of the operation being approved.
3. **Operator confusion is the main risk.** Making HMAC opt-in creates a deployment shape where operators forget to enable it; making it mandatory means there's no insecure default.

Operators who want to disable the Approval API entirely can do so via config (`[approval] enabled = false`); operators who want it enabled get HMAC. There's no third path.

The HMAC key shares the keypair-management infrastructure from H1 (per open question 7's resolution; final design decision in K10 PR). This minimizes the secrets-management burden for operators.

---

## Risks and mitigations

Every architectural decision in this RFC carries operational risk. The risks below are the ones the V0.7-EPIC owners are tracking; mitigations are documented per risk so that when the risk materializes the response is already known.

### Track G — Hook pipeline

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Hook author writes a `post_recall` hook in `exec` mode and blows the 50 ms recall budget | Medium | High (hot-path regression) | Default `mode = "daemon"` for `post_recall`/`post_search`; `mode = "exec"` requires explicit override; CI bench gate (G6) catches the regression on PR |
| Hook subprocess hangs indefinitely | Medium | High (blocks the operation) | Per-event-class deadlines (G6) kill hooks at the class boundary; per-hook `timeout_ms` cannot exceed the class deadline; SIGKILL on `exec`, connection close on `daemon` |
| Hook chain accumulates latency from N hooks each within budget | Low | High (chain p95 crosses recall budget) | Class deadline applies to **total** chain runtime, not per-hook; per-hook gets `min(class_remaining, hook_timeout_ms)` |
| Hook crash takes down `ai-memory` daemon | Low | Critical | Subprocess isolation — hook crash takes down only the hook process. Default `fail_mode = "open"`: crash treated as `Allow`, chain continues. |
| Hook author returns malformed JSON decision | Medium | Medium (chain continues incorrectly) | Strict deserialization in G4; malformed payload surfaces as "hook returned malformed decision" warning, treated as `Allow` |
| Hook author writes a `Modify` decision on a post- event (which doesn't allow modification) | Medium | Medium (silently ignored) | Compile-time guard via separate types (G4 design); runtime validation in dispatcher rejects `Modify` on post- events with logged warning |
| Hot-reload race condition: in-flight hook execution against old config when new config loads | Low | Medium (inconsistent decisions) | In-flight hook executions complete on old config (G1 design); new fires use new config |
| Daemon-mode child OOM-kills under sustained load | Low | High (chain stops firing) | Reconnection with exponential backoff (G3 design); doctor surfaces drop counts; operator notified via log |
| Operator forgets `hooks.toml` exists and gets surprised by behavior | Medium | Low (hooks are opt-in; no surprise without explicit config) | `ai-memory doctor --hooks` lists registered hooks per event |
| Auto-link detector (G11) creates spurious links on borderline cosine | Medium | Low (operator can `memory_unlink` or restrict heuristic) | Default off; opt-in per namespace; documented heuristic threshold (cosine > 0.85 for `related_to`) |

### Track H — Ed25519 attestation

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Operator confuses signing with encryption | High | Medium (false sense of confidentiality) | RFC threat model is explicit (NT1); MIGRATION_v0.7.md repeats the call-out; release notes reinforce |
| Operator's private key file gets accidentally checked into git | Medium | Critical | File mode 0600 reduces accidental scope; `.gitignore` template recommended; commercial layer offers HSM-backed storage |
| Forged links from a peer with no known public key | Low | Medium | Inbound verification (H3) accepts-and-flags as `unsigned`; `attest_level` makes the trust boundary visible to consumers |
| Public key distribution is operator-managed and someone gets it wrong | High | Medium | v0.7 documents operator responsibility; v1.0 considers federated key-discovery protocol |
| Signature verification cost under DoS attack | Low | Medium | Indirect mitigation via K8 RPS quotas; recommend network-layer rate-limit at firewall |
| Schema migration v20 → v21 (adding `signed_events` table) fails partway | Low | High (broken DB state) | Idempotent migration; transactional commit; tested against real production-shaped DB snapshot |
| Append-only `signed_events` table grows unbounded | Medium | Medium (storage exhaustion over years) | Documented retention policy in `docs/SECURITY.md` (TODO); operator-tunable; v0.8 considers archive-to-cold-storage |
| Compromise of agent A's key allows back-dated signature insertion | Low | High | Mitigation is operator-side rotation; `signed_events` chain timestamp helps post-mortem identify suspicious window |

### Track I — Sidechain transcripts

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Transcript storage explodes disk usage | High (without TTL) | High | Per-namespace TTL with archive → prune lifecycle (I3); zstd-3 compression; default off |
| Transcript content includes sensitive info that operator didn't want stored | High | Critical | Default off; opt-in per namespace; operator must explicitly configure `[transcripts]` block; NT1 in threat model still applies (transcripts are unencrypted) |
| `memory_replay` returns transcript content to a caller without sufficient permissions | Medium | High | Permissions (K9) gate `memory_replay` calls per namespace; default deny on namespaces without explicit allow |
| Transcript-to-memory join (`memory_transcript_links`) corrupts under partial failure | Low | Medium | Foreign-key constraints; cleanup sweep removes orphaned links |
| zstd compression CPU cost on large transcripts | Low | Low | Compression at write time only; reads decompress lazily; default level 3 balances CPU vs ratio |

### Track J — AGE acceleration

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| AGE returns different results from CTE for same query | Medium | High | Dual-path tests (J5) gate every KG operation; result-set equivalence asserted |
| AGE not 30% faster than CTE on operator's workload | Medium | Medium (drop AGE if so) | J8 bench gate is kill-switch; if AGE doesn't earn complexity, drop and revisit in v0.9 |
| Operator installs AGE but a bug in detection misses it | Low | Low | Doctor reports `kg_backend = "age"` or `"cte"`; operator can verify; manual override env var as escape hatch |
| AGE upstream introduces breaking change in a future Postgres version | Medium (long-tail) | High | Pin to AGE version in installation docs; monitor AGE upstream; v0.7.x patch release if needed |
| Operator confuses AGE-mode with default-mode and is surprised by performance differences | Low | Low | Doctor surfaces backend; PERFORMANCE.md has separate AGE/CTE budgets (J6) |

### Track K — Permissions + G1

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| G1 inheritance fix breaks an operator's existing workflow | Medium (for pre-v0.6.3.1 users) | High | Per-policy `inherit: bool` (default true); operators set `inherit = false` to preserve leaf-only resolution; documented in MIGRATION_v0.7.md |
| Permissions migration tool corrupts existing governance state | Low | Critical | Idempotent; dry-run by default; --apply commits; v0.6.x `governance_policies` rows preserved (not deleted) |
| Operator runs --apply and discovers they wanted dry-run | Medium | Low (rollback is straightforward) | Migration is additive; v0.6.x rows still present; rollback = revert to v0.6.4 binary or run inverse migration (TODO in v0.7.1 if requested) |
| Default `mode = "advisory"` after migration silently allows operations operator wanted blocked | Medium | Medium | Honest disclosure: MIGRATION_v0.7.md says explicitly "Default after migration is advisory; flip to enforce after observation"; doctor surfaces current mode |
| Approval API HMAC misconfiguration locks operator out | Low | High | HMAC keys share keypair management infra; operator workflow documented; emergency bypass via direct DB write (operator responsibility) |
| A2A correlation-ID LRU evicts a still-pending message | Low | Medium | LRU sized at 10 k entries (100 RPS × 100 s ≫ 30 s default TTL); operator can tune size if mesh load is higher |

### Cross-track risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| 11 tracks land in parallel and cause merge conflicts in shared files (`src/mcp.rs`, `src/storage/links.rs`) | High | Medium | Critical-path sequencing (V0.7-EPIC.md); G/H/I tracks separated by file boundary; Track K migrations run last |
| v0.7.0 release ships with one track incomplete | Low (cutline-protected) | Variable | Mandatory cutline (K1 + A + B + G + H + F1 + F5) — anything else can defer to v0.7.1 if scope pressure forces it |
| 50 ms recall budget regresses across the combined surface | Medium | High | CI bench gate runs the full surface on every PR; G6 hook overhead test specifically combines hooks + recall |
| SDK regression sneaks in during Track A capabilities-v3 work | Low | Critical | Release-readiness item: "No SDK regression — existing 0.6.4 SDKs still work against v0.7.0 server"; v2 fields preserved |
| Discovery Gate T0 cells fail post-ship | Low | Medium | Re-run E2 after ship; ≥95% convergence required across 4 LLMs; if regressed, hotfix in v0.7.0.1 |

---

## Open questions

This RFC documents the **answered** design decisions. The open questions below are tracked for resolution **during** v0.7 development, not before this RFC ships. Each carries an owning track and a deadline for resolution.

1. **Should the canonical-CBOR encoding for link signing include the `confidence` field?**
   The current H2 design includes `{src_id, dst_id, relation, observed_by, valid_from, valid_until}` but not `confidence`. Argument for inclusion: `confidence` is part of the link's semantic identity. Argument against: `confidence` may be re-derived by consumers (e.g., a CRDT merge in v0.8 might recompute it from votes). **Decision deadline:** before H2 PR opens. **Owner:** Track H lead.

2. **Should `memory_replay` return the full transcript chain or just the transcript-segments cited by the memory?**
   Current I4 design returns the full transcript content + span metadata. Operator concern: transcripts can be very long; returning the full text on every replay is wasteful. Alternative: return only the span(s) referenced by `memory_transcript_links.span_start..span_end`. **Decision deadline:** I4 PR. **Owner:** Track I lead.

3. **What is the right default `inherit: bool` value on a brand-new policy created in v0.7?**
   Migration backfills existing rows to `inherit = true` for backward compat. New policies in v0.7 default to `inherit = true` for consistency, but the **inherit-from-parent** semantics are stronger by default and may surprise new operators. Alternative: default new policies to `inherit = false`, document the inheritance flag explicitly. **Decision deadline:** K1 PR. **Owner:** Track K lead.

4. **Should the hook config file path be configurable, or hardcoded at `~/.config/ai-memory/hooks.toml`?**
   Current G1 design hardcodes the path. Operators with non-standard layouts (XDG_CONFIG_HOME variations, containers with read-only home directories) may need a different path. Proposal: support `AI_MEMORY_HOOKS_CONFIG=/path/to/hooks.toml` env var as override. **Decision deadline:** G1 PR. **Owner:** Track G lead.

5. **How does `attest_level` propagate through CRDT merges in v0.8?**
   This RFC doesn't answer it because CRDT merge semantics aren't pinned until v0.8 Pillar 3. The v0.7 commitment: signed write retains `self_signed` until merged with a peer's signed write, at which point the merge result is `peer_attested` (because both signatures are verifiable). v0.8 may need to introduce a `co_signed` variant for tracked-as-multi-party writes. **Decision deferred to v0.8.** **Owner:** v0.8 Pillar 3 lead.

6. **Should the J8 bench gate be 30%, 25%, or 50%?**
   The 30% threshold is the current best estimate. Pre-v0.7-ship benchmark cycle will validate; if AGE p95 is 28% faster, do we ship? The decision is principled (kill-switch, not warning) but the threshold needs operational data. **Decision deadline:** J8 PR. **Owner:** Track J lead.

7. **Does the approval API require operator-supplied secrets, or can it use the agent's signing keypair for HMAC?**
   K10 design is currently ambiguous. Sharing the signing key has the benefit of fewer secrets to manage; using a separate HMAC secret has the benefit of cryptographic separation. **Decision deadline:** K10 PR. **Owner:** Track K lead.

8. **What is the v0.7.1 patch-release cadence?**
   v0.7.0 explicitly defers items to v0.7.1 (A2A scenarios, per-agent quotas, governance migration polish). When does v0.7.1 ship? Proposal: 4-6 weeks after v0.7.0, based on operator feedback velocity. **Decision deadline:** v0.7.0 release week. **Owner:** Release engineering.

These open questions are not blockers on this RFC; they are **acknowledged uncertainties** that will be resolved during track execution. Decisions should be recorded back into this document as a `## Decisions log` section before the RFC moves from `DRAFT` to `APPROVED`.

---

## NHI guardrails extension (phase 2 in v0.7+)

The v0.6.4 RFC introduced **phase 1 NHI guardrails** (per-agent capability allowlists, audit on expansion). v0.7 was originally scoped to extend these to **phase 2** (rate-limit on expansion, attestation-tier gating). Per V0.7-EPIC.md "Non-goals", phase 2 has been **deferred to v0.7.1 or v0.8**.

The deferral rationale:

- **Phase 2 depends on #238** (mTLS body-claimed `sender_agent_id` attestation). Until #238 lands, the binding between identity and capability is advisory only. v0.7 lays the substrate (Ed25519 attestation gives per-agent identity that #238 can lean on); v0.7.1 / v0.8 closes the loop.
- **Phase 2 risks NHI lockout.** If a permission rule is mis-scoped and an NHI loses access to a family it was using, the recovery path involves operator intervention. Phase 2 deserves more design time than the v0.7 schedule allows.
- **Phase 1 is sufficient for v0.7's headline narrative.** The `attested-cortex` narrative is about **integrity of writes** (Ed25519) and **enforcement of policy** (G1 + permissions). Capability-expansion guardrails are an orthogonal concern that can land out-of-band.

The phase 2 design will appear in a **separate RFC** (`docs/v0.7.1/rfc-nhi-guardrails-phase-2.md`) when the work is scheduled. This RFC's threat model section explicitly **does not** cover capability-expansion attacks; that's phase 2's scope.

---

## Decisions log (placeholder)

Once the open questions above are resolved during track execution, the answers should be recorded here. Each entry: question, resolution, owner, date, link to PR.

> _This section will populate during v0.7.0 development. Final pass when all tracks land moves this RFC from `DRAFT` to `APPROVED` and locks the decisions log._

---

## Approval gate

This RFC requires sign-off on the following architectural commitments before v0.7.0 release. Each item links to its owning track:

- [ ] **Decision 1** — Ed25519 in v0.7; X25519+ChaCha20 deferred to v0.8 #228 (Track H scope)
- [ ] **Decision 2** — Subprocess-stdio + daemon-mode for hooks (Track G scope; G3 daemon design)
- [ ] **Decision 3** — AGE behind feature flag; J8 bench gate as kill-switch (Track J scope)
- [ ] **Decision 4** — Permissions replace governance; K11 migration tool (Track K scope; K1/K9/K11)
- [ ] **Threat model** — T1-T5 protections + NT1-NT7 non-coverages documented and accepted
- [ ] **Performance budget** — 50ms recall p95 stays mandatory; G6 class deadlines; J8 30% threshold
- [ ] **Compatibility matrix** — v0.6.4 SDK ↔ v0.7 server; v0.6.x → v0.7 migration paths
- [ ] **Out-of-scope list** — v0.7.1 / v0.8 / v0.9 / v1.0 deferral targets locked
- [ ] **Schema migration v20 → v22 idempotent + tested** (release-readiness checklist item)
- [ ] **No SDK regression** — existing 0.6.4 SDKs work against v0.7.0 server (release-readiness checklist item)

On sign-off, this RFC moves from `DRAFT` to `APPROVED` and becomes the historical record of the v0.7.0 design rationale. The status changes from `DRAFT — finalizes at v0.7.0 release` to `APPROVED — v0.7.0 reference design`. Subsequent design changes (in v0.7.1, v0.8, etc.) reference this document as their predecessor.

---

## References

### Primary docs

- [`docs/v0.7/V0.7-EPIC.md`](V0.7-EPIC.md) — The canonical operational epic for v0.7.0. 1237 lines. Source of truth for what's shipping, by which task ID, in which week, with which definition-of-done.
- [`docs/v0.7/v0.7-nhi-prompts.md`](v0.7-nhi-prompts.md) — Per-task NHI starter prompts. Useful for technical detail per track.
- [`docs/MIGRATION_v0.7.md`](../MIGRATION_v0.7.md) — Migration guide for users coming from v0.6.4. Cross-links to this RFC for design rationale.
- [`docs/v0.7/schema-compaction-audit.md`](schema-compaction-audit.md) — Track C (schema compaction) audit data backing the ≤3500-token target.

### Predecessors

- [`docs/v0.6.4/rfc-default-tool-surface-collapse.md`](../v0.6.4/rfc-default-tool-surface-collapse.md) — The v0.6.4 RFC; style precedent for this document.
- [v0.6.5 epic (superseded)](../v0.6.5/V0.6.5-EPIC.md) — `cortex-fluent` epic, rolled into v0.7.0 per the V0.7-EPIC.
- [`ROADMAP2.md §7.3`](../../ROADMAP2.md) — The original v0.7 spec (Q2 2026 target; now consolidating into `attested-cortex`).
- [`docs/MIGRATION_v0.6.4.md`](../MIGRATION_v0.6.4.md) — The predecessor migration guide.
- [`docs/MIGRATION-v0.6.2-to-v0.6.3.md`](../MIGRATION-v0.6.2-to-v0.6.3.md) — Earlier migration.
- [`docs/BASELINE-v0.6.3.1.md`](../BASELINE-v0.6.3.1.md) — Honest-Capabilities-v2 disclosure baseline; the document this RFC builds on.

### ADRs (architectural decision records — historical)

- [`docs/ADR-0001-quorum-replication.md`](../ADR-0001-quorum-replication.md) — Quorum replication semantics.
- [`docs/ADR-0002-kg-schema-v15-backward-incompat.md`](../ADR-0002-kg-schema-v15-backward-incompat.md) — KG schema v15 backward-incompat decision.
- [`docs/ADR-0003-kg-invalidation-eventual-consistency.md`](../ADR-0003-kg-invalidation-eventual-consistency.md) — KG invalidation eventual-consistency model.

### GitHub issues (major tracks)

- **Headline** — [#545](https://github.com/alphaonedev/ai-memory-mcp/issues/545) · [#546](https://github.com/alphaonedev/ai-memory-mcp/issues/546) · [#512](https://github.com/alphaonedev/ai-memory-mcp/issues/512)
- **G1 cutline** — namespace inheritance fix (already in v0.6.3.1; restructured in v0.7 K1)
- **G12** — Ed25519 dead column finding from v0.6.3 audit; closed by Track H
- **#228** — End-to-end memory encryption (X25519 + ChaCha20-Poly1305); v0.8 commitment
- **#238** — mTLS body-claimed `sender_agent_id` attestation; v0.7+ NHI guardrail phase 2 dependency
- **#311** — targeted share (orthogonal)
- **#318** — grok MCP fanout (orthogonal)
- **#511** — v0.6.3.1 A2A certification campaign
- **Discovery Gate** — [alphaonedev/ai-memory-discovery-gate#1](https://github.com/alphaonedev/ai-memory-discovery-gate/pull/1)

### External references

- [Apache AGE](https://age.apache.org/) — The Postgres extension for Cypher graph queries.
- [Ed25519 — RFC 8032](https://datatracker.ietf.org/doc/html/rfc8032) — Edwards-curve Digital Signature Algorithm.
- [Canonical CBOR — RFC 8949 §4.2.1](https://datatracker.ietf.org/doc/html/rfc8949#section-4.2.1) — Canonical encoding rules used for the link signing payload.
- [`ed25519-dalek`](https://docs.rs/ed25519-dalek/) — The Rust Ed25519 implementation used by Track H.
- [Boris Cherny — token-waste taxonomy](https://www.anthropic.com/engineering/) — Pattern 6 / "just-in-case" tool definitions; backing data for the v0.6.4 default tool surface collapse and v0.7 schema compaction (Track C).

### Discovery Gate evidence

- v0.6.4 NHI Witness transcripts (Grok 4.2 reasoning before/after): observation cells under [Discovery Gate `runs/2026-05-05/`](https://alphaonedev.github.io/ai-memory-discovery-gate/).
- v0.7.0 cert campaign in [`alphaonedev/ai-memory-test-hub`](https://github.com/alphaonedev/ai-memory-test-hub) — TODO: `campaigns/v0.7.md` filed at release.
- v0.7.0 Discovery Gate run — TODO: `runs/v0.7-ship-date/` filed at release with T0+ convergence evidence.

---

## Closing note

`attested-cortex` is two narratives told as one release:

1. **Cortex-fluent (legibility)** — the substrate becomes more articulate. Loaders that say "load." Capabilities that pre-compute their own description. Schemas at half the token cost.
2. **Attested (trust)** — the substrate becomes cryptographically trustworthy. Per-agent Ed25519 identity. Programmable lifecycle hooks. Enforced namespace inheritance. Sidechain transcripts. AGE-accelerated graph traversal. A2A maturity.

These aren't incidental siblings; they're the same story told from two angles. An NHI fleet needs to know what its memory cortex says about itself **and** that what it says is signed.

This RFC records the design rationale for the trust angle (Decisions 1-4) and inherits the legibility-angle rationale from the v0.6.4 RFC (`docs/v0.6.4/rfc-default-tool-surface-collapse.md`) and the v0.6.5 epic (now superseded). Together with V0.7-EPIC.md (operational), v0.7-nhi-prompts.md (per-task technical detail), and MIGRATION_v0.7.md (operator-facing change log), it forms the complete documentary surface for the v0.7.0 release.

**Codename:** `attested-cortex` — the substrate becomes both more articulate and cryptographically trustworthy in one release.

---

## Glossary

Terms used throughout this RFC, with the v0.7 precise meaning. Cross-references to V0.7-EPIC.md / MIGRATION_v0.7.md / ROADMAP2.md preserved.

| Term | Meaning in v0.7 |
|---|---|
| **`agent_id`** | Operator-supplied immutable identifier per NHI; format `ai:<client>@<host>:pid-<pid>` for hardened defaults; per #196 |
| **AGE** | Apache AGE — Postgres extension for Cypher graph queries; v0.7 detects + uses if present |
| **Allow / Modify / Deny / AskUser** | The four `Decision` variants returned by hooks (G4) and consumed by the gate (K9) |
| **`attest_level`** | Enum `unsigned | self_signed | peer_attested` recording the attestation strength of a stored link |
| **Canonical CBOR** | RFC 8949 §4.2.1 deterministic byte encoding; the link-signing payload format |
| **Cortex-fluent** | The legibility narrative absorbed from the canceled v0.6.5 epic; one of the two narratives in `attested-cortex` |
| **CTE** | SQL recursive Common Table Expression; the v0.6.x KG traversal path retained as the SQLite default in v0.7 |
| **Cutline** | A feature that ships even if everything else slips; v0.7 has one (K1 / G1 inheritance) |
| **Daemon mode** | Hook execution mode where the child process is long-lived and JSON-RPC-framed (vs. `exec` mode which spawns per fire) |
| **Decision** | The unified shape returned by hooks, the gate, and surfaced by the approval API; four variants (Allow/Modify/Deny/AskUser) |
| **Discovery Gate** | The cross-LLM convergence test framework at [alphaonedev/ai-memory-discovery-gate](https://github.com/alphaonedev/ai-memory-discovery-gate); T0 cells per E1-E3 |
| **Ed25519** | Edwards-curve digital signature algorithm; RFC 8032; the signature primitive Track H ships |
| **Exec mode** | Hook execution mode where a subprocess is spawned per fire (vs. `daemon` mode) |
| **Family** | A logical grouping of MCP tools (core / graph / admin / power / full); v0.6.4 introduced; v0.7 extends with loader tools |
| **G1** | The namespace-inheritance enforcement fix; cutline-protected per ROADMAP2 §7.3 |
| **Gate** | The decision surface that runs before a write; consults permission rules (K9) and hook chain (G5) |
| **G12** | The v0.6.3 audit finding that `memory_links.signature` was a dead column; closed by Track H (H6) |
| **Honest disclosure** | Principle 5: capabilities report live state, not advertised intent; v0.6.3.1 introduced; v0.7 inherits |
| **Hook chain** | Multiple hooks fired on the same event; ordered by priority desc; first `Deny` short-circuits (G5) |
| **HMAC** | Keyed-hash message authentication; mandatory on the v0.7 Approval API (K10) |
| **MCP** | Model Context Protocol — the wire protocol ai-memory exposes to LLM clients |
| **NHI** | Non-human identity; the agent class ai-memory's identity hardening targets (per #196) |
| **NHI guardrails** | Capability-expansion controls per the v0.6.4 RFC; phase 1 in v0.6.4; phase 2 deferred to v0.7.1+ |
| **Observed_by** | The agent_id claim on a `memory_link`; verified against public key by inbound verification (H3) |
| **Opt-in** | Principle 1: new behavior defaults off; operators must explicitly enable |
| **Peer-attested** | `attest_level` value when an inbound federated link's signature verified against a known public key |
| **Permission rule** | The v0.7 declarative policy shape that replaces the v0.6.x `governance_policies` shape; converted by K11 |
| **Policy chain walk** | The traversal of `build_namespace_chain` to find the first non-null policy (G1 / K1) |
| **Profile** | A named set of MCP tools registered at server startup (`core`, `graph`, `admin`, `power`, `full`); v0.6.4 introduced |
| **R-series** | Recovery commitments from the v0.6.x charter that vanished in earlier roadmaps; R3/R5 recovered in v0.7 |
| **R3** | Auto-link inference recovery — `post_store` daemon-mode hook (G11) |
| **R5** | Auto-extraction from conversations recovery — `pre_store` hook on transcripts (I5) |
| **Recall budget** | The 50ms p95 ceiling on the recall hot path; v0.6.3 set; v0.7 inherits and protects |
| **SAL** | Storage abstraction layer; the trait surface that lets the substrate target SQLite or Postgres backends |
| **Schema version** | Monotonic integer in the `migrations` table; v0.6.4 = 20; v0.7 ships v22 |
| **Self-signed** | `attest_level` value when the active agent has a keypair and signed an outbound link |
| **`signed_events`** | Append-only audit table introduced in schema v21 (H5); records every signed write |
| **Substrate** | The combined v0.7 system surface — hooks + attestation + transcripts + AGE + permissions + the v0.6.x foundation |
| **TOON** | Token-Optimized Object Notation; v0.9 commitment via R8 (or formally cut) |
| **Transcript** | Raw conversation/reasoning trail stored as zstd-3 BLOB; opt-in per namespace; substrate for R5 |
| **Trust gap** | The cluster of v0.6.3 audit findings about advertised-but-undelivered trust capabilities |
| **Unsigned** | `attest_level` value when the active agent has no keypair (preserves v0.6.4 default) |

---

## Appendix A — Document evolution

| Version | Date | Author | Change |
|---|---|---|---|
| 0.1 | 2026-05-05 | AlphaOne (synthesis) | Initial DRAFT — F6 commit |
| (TBD) | — | track leads | Decisions log entries from open-questions resolution |
| (TBD at release) | v0.7.0 release date | RFC owner | Status flips to APPROVED; locks the design rationale |

---

## Appendix B — How to read this RFC alongside the operational docs

| If you're a... | Read in order |
|---|---|
| **New contributor onboarding to v0.7 work** | This RFC → V0.7-EPIC.md (just your track section) → v0.7-nhi-prompts.md (just your task starter) |
| **Operator planning a v0.6.4 → v0.7 upgrade** | MIGRATION_v0.7.md → this RFC's threat model + compatibility matrix → V0.7-EPIC.md release-readiness checklist |
| **Security reviewer auditing v0.7** | This RFC's threat model → MIGRATION_v0.7.md (to understand operator workflow) → Track H section in V0.7-EPIC.md (implementation detail) → `tests/identity_e2e.rs` (verification evidence) |
| **SDK author updating to v0.7** | MIGRATION_v0.7.md compat section → this RFC compatibility matrix → API_REFERENCE.md (TODO until tracks land) |
| **Engineer designing a downstream hook** | docs/hooks/ (TODO) → this RFC's "Reference design — auto-link detector" section → V0.7-EPIC.md Track G |
| **Procurement / due-diligence reviewer** | This RFC's "Why this release exists — the longer view" → threat model → out-of-scope list |

The RFC is the rationale doc; the EPIC is the operational doc; the migration guide is the operator-facing doc; the per-task NHI prompts are the implementation doc. They are deliberately separate and deliberately cross-linked.

---

*End of RFC.*
