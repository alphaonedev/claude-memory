# Competitive Positioning — ai-memory

> **Release tag:** v0.7.0 grand-slam (2026-05-15)
> **Scope:** Honest per-project ICP framing for the agent-memory category. Not
> category combat. Each project below has an optimum user; ai-memory has one
> too. The decision aid at the bottom helps a reader pick correctly.

The agent-memory space has grown crowded enough in 2026 that procurement
officers, platform engineers, and individual developers all reasonably ask:
*"Why this one and not the others?"* This page answers that, project by
project, with the same template:

- **ICP** — who is this project optimised for
- **Strength** — what it does well today
- **Use if** — when to pick it over alternatives
- **ai-memory differs** — what we ship that they do not (or that they do
  not prioritise)

We update this page on every release. The sources behind each claim are
linked inline; the page is meant to be auditable, not aspirational.

---

## Tencent TencentDB Agent Memory

(v0.3.4, 2026-05-13 — the largest recent entrant in the category)

**ICP:** OpenClaw + Hermes plugin users; individual developers on Tencent's
agent platforms.

**Strength:** Published benchmarks (WideSearch +51.52%, SWE-bench +9.93%,
AA-LCR +7.95%, PersonaMem +59% relative). Short-term context compression via
Mermaid canvas + node_id dereference. Layered L0→L3 semantic pyramid
producing persona-as-artifact. White-box file-backed inspection.

**Use if:** building on OpenClaw or Hermes; want Tencent Cloud-backed memory
plugin; want benchmarked OpenClaw-end-to-end pattern.

**ai-memory differs:** MCP-compatible with any agent runtime; procurement-grade
Ed25519 attestation per write; federation across trust boundaries with mTLS
+ W-of-N quorum; substrate-authority enforcement via policy engine; Apache
2.0 substrate forever; multi-tier deployment (keyword / semantic / smart /
autonomous).

---

## mem0

**ICP:** SaaS-first product teams wiring memory into chat UX quickly.

**Strength:** Polished hosted API, fast onboarding, broad SDK coverage, brand
recognition in the agent-memory category.

**Use if:** you want a managed cloud memory backend, are happy with
per-recall pricing, and your data residency / procurement constraints are
loose.

**ai-memory differs:** local-first single binary, zero-token cost until
recall, Apache 2.0 substrate (no vendor risk), federation across
organisations, cryptographic attestation per write, MCP-native (works with
any MCP client, not a proprietary SDK).

---

## Letta (formerly MemGPT)

**ICP:** Research teams and product engineers exploring agent state machines
and recall-quality research.

**Strength:** Strong academic lineage (MemGPT paper), expressive agent state
model, active OSS community, good sub-100ms recall on smaller corpora.

**Use if:** you want a research-grade agent runtime with built-in memory and
are comfortable operating a Python service.

**ai-memory differs:** substrate-first design (the memory layer is the
product, agent runtime is BYO), MCP-native rather than runtime-bundled,
procurement-grade attestation, federation primitive, single Rust binary
operationally.

---

## Hindsight

**ICP:** Trace-replay enthusiasts and post-hoc agent debugging users.

**Strength:** Replay-centric model, good developer ergonomics for
introspecting agent runs.

**Use if:** your primary need is forensic replay of past agent runs and you
do not need live recall as a substrate.

**ai-memory differs:** live recall substrate first (forensic export is a
side-effect, not the focus), policy engine enforcing substrate authority,
multi-tier deployment, federation.

---

## AI Memory Booster

**ICP:** Plugin-style users wanting drop-in memory uplift for an existing
chat product.

**Strength:** Low-friction drop-in, narrow scope, simple value prop.

**Use if:** you have a closed chat product and want recall uplift without
operating any new service.

**ai-memory differs:** designed as a long-lived organisational substrate
(not a chat plugin), federation, attestation, policy engine, single Rust
binary running locally or on your own infra.

---

## agentmemory

**ICP:** Python-first developers wanting a small, hackable memory library.

**Strength:** Minimal surface area, easy to read, easy to fork.

**Use if:** you want a library-level dependency you can audit in an
afternoon, with no service to run.

**ai-memory differs:** ships as a substrate (not a library), MCP server,
policy engine, attestation, federation; the operational model is "stand up
once, many agents share it", not "import into each agent".

---

## Built-in vendor memory (Claude / ChatGPT / Gemini)

**ICP:** End users of a single vendor's chat product who want continuity
without operating anything.

**Strength:** Zero-setup, free-tier inclusion, integrated UX.

**Use if:** you only ever use one vendor, do not need data portability, and
do not need to compose memory across agents.

**ai-memory differs:** vendor-neutral (works with any MCP-compatible
client), portable data (it is yours, on your disk), federation across
agents and across organisations, audit-grade evidence trail.

---

## Decision aid

A two-sentence picker for the procurement officer skimming this page:

- **Use Tencent TencentDB Agent Memory** if you're on OpenClaw or Hermes;
  they ship the best benchmarks in those frameworks today.
- **Use mem0 or Letta** if you want a hosted/research-grade managed memory
  service and procurement / data residency are not a binding constraint.
- **Use AI Memory Booster, agentmemory, or built-in vendor memory** if
  you're in a narrow single-vendor scenario and don't need substrate-level
  guarantees.
- **Use ai-memory** if you need a procurement-grade memory substrate that
  survives vendor changes, supports federation across organisations, ships
  with cryptographic attestation, and composes with any MCP-compatible AI
  client.

The categories overlap on the recall-quality axis but the optimums diverge
quickly past that. Tencent is in OpenClaw's ecosystem; ai-memory is in
MCP-compatible-anywhere with attestation and federation. Different
categories that overlap on the recall-quality axis.

---

## Architectural patterns ai-memory absorbed from Tencent (v0.7.0)

The Tencent v0.3.4 release surfaced three patterns worth absorbing into
ai-memory. Each is a separate quick-win branch landing alongside this page:

- **File-backed export of high-level artifacts** → QW-1
  ([`recursive-learning.md`](./RECURSIVE_LEARNING.md)) — write-through
  artifacts that can be inspected without booting the substrate
- **Persona-as-artifact** (L3 pyramid output) → QW-2
  ([`persona.md`](./persona.md) when landed)
- **Context-offload primitive** (single-key dereference for short-term
  context) → QW-3 ([`context-offload.md`](./context-offload.md) when
  landed); the full short-term compression pattern targets v0.8.0

## Patterns deliberately NOT adopted

Documented so a reader can audit our choice surface:

- **Mermaid as primary symbolic-graph format** — conflicts with our typed
  graph backed by Apache AGE; we keep the typed schema. Mermaid as a
  visualisation export is fine; Mermaid as the canonical graph language is
  not.
- **OpenClaw plugin distribution** — dilutes the MCP-substrate story. We
  ship as an MCP server compatible with any MCP client; framework-specific
  plugins are downstream concerns, not substrate concerns.
- **TypeScript primary surface** — Rust substrate is an architectural
  choice (single binary, no GC pauses, attestation primitives, FFI for
  SDKs). TypeScript SDK is supported as a consumer surface, not as the
  substrate language.

---

## Cross-links

- [`RECURSIVE_LEARNING.md`](./RECURSIVE_LEARNING.md) — where QW-1
  file-backed reflection export composes
- `persona.md` — persona-as-artifact (QW-2, landing alongside this page)
- `context-offload.md` — context-offload primitive (QW-3, landing
  alongside this page)
- [`forensic-export.md`](./forensic-export.md) — forensic bundle and audit
  trail
- [`policy-engine.md`](./policy-engine.md) — substrate-authority
  enforcement

---

*Last reviewed: 2026-05-15 (v0.7.0 grand-slam, QW-4).*
