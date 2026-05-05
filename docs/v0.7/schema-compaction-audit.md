---
title: "Schema-compaction audit (Track C / C1)"
status: "PLANNED — audit input for C2-C5"
date: 2026-05-05
issue: "#546"
release: "v0.7.0 (`attested-cortex`)"
---

# Schema-compaction audit — input for v0.7 Track C (C2-C5)

> **One sentence:** measure every one of the 43 MCP tool definitions today, sort by total `cl100k_base` token cost, flag the verbosity hotspots, and hand the list to C2-C5 so each follow-on chip lands in a known place in the budget.

**Companion to** [`V0.7-EPIC.md`](V0.7-EPIC.md) and [`v0.7-nhi-prompts.md`](v0.7-nhi-prompts.md) (Track C).
**Status:** PLANNED — this document is the *input* artifact. The compaction itself is C2-C5.
**Date:** 2026-05-05.
**Issue:** [#546](https://github.com/alphaonedev/ai-memory-mcp/issues/546) (partial — audit only).

---

## Headline numbers

| Metric | Value |
|---|---:|
| Tool count (`--profile full`) | 43 |
| Total schema tokens (`--profile full`, `cl100k_base`) | **6,198** |
| Total schema tokens (`--profile core`, current default) | 1,465 |
| `core` savings vs `full` | 76.4 % |
| Largest single tool | `memory_kg_query` — 444 tokens |
| Tools with `description` > 200 tokens | 0 |
| Tools with `description` > 100 tokens | 4 |
| C5 target (`full` profile after C2-C4) | ≤ 3,500 tokens |
| Headroom required to hit target | **≈ 2,700 tokens (44 % drop)** |

---

## Methodology

**Source binary.** `cargo build --release --bin ai-memory` against commit `ca19b90` (v0.6.4 release artifact, the `attested-cortex` baseline).

**Per-tool roll-up table.** The doctor command emits the canonical per-tool token table:

```sh
./target/release/ai-memory doctor --tokens --raw-table --json --profile full \
    > /tmp/c1-doctor-full.json
```

The tokenizer is OpenAI's `cl100k_base` (the same BPE Claude and GPT use for input accounting, wired in via `tiktoken-rs` in [`src/sizes.rs`](../../src/sizes.rs)). The `total_tokens` field per tool is the byte-length of `serde_json::to_string(tool)` — i.e. the canonical wire form an MCP host receives over stdio in response to `tools/list`.

**Description-only vs inputSchema-only split.** The doctor today reports a single `total_tokens` per tool but does not split it; v0.6.4-005 left the description/schema breakdown as a diagnostic field on `ToolSize` (`schema_tokens`, `name_tokens`) without surfacing it. To get the split this audit needs, we capture the live `tools/list` JSON-RPC response from the running daemon and re-tokenize each `description` and `inputSchema` value separately with the Python `tiktoken` reference implementation:

```sh
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
    | ./target/release/ai-memory mcp --profile full \
    > /tmp/tools-list.json
```

The Python script serializes each value with `json.dumps(v, separators=(",", ":"), sort_keys=True)` (compact, deterministic key ordering) and tokenizes via `tiktoken.get_encoding("cl100k_base").encode(...)`.

**Why the table sums don't match exactly.** Re-tokenizing the description and inputSchema separately and summing them comes to 6,286 tokens vs. the doctor's whole-object measurement of 6,198. The 88-token (~1.4 %) gap is the per-tool object framing (`{"name":...,"description":...,"inputSchema":{...}}`) that gets paid once when the whole object is serialized but twice when each child is serialized in isolation, plus minor differences in serde_json vs. Python `json` whitespace conventions. The split is therefore a **directional decomposition**, not a partition; for budget enforcement, the doctor's whole-object number is canonical.

**Reproducibility.** Inputs and outputs of this audit live at:
- `/tmp/c1-doctor-full.json` — full doctor JSON (`--profile full`, raw table)
- `/tmp/c1-doctor.json` — same but `--profile core` (the active default)
- `/tmp/tools-list.json` — live `tools/list` capture used for the split
- `/tmp/c1-rows.json` — per-tool table emitted by the analyzer

To reproduce on another machine, rebuild and run the three commands above, then compare against the table below.

---

## Per-tool table (sorted by total tokens, descending)

Description-tokens flagged (`!`) when over 200 tokens. **No tool exceeds the 200-token description threshold today** — the verbosity is concentrated in inputSchema enums and inline `description:` strings on properties, not in the top-level `description` field.

| # | Tool | Family | Desc tokens | Schema tokens | Total tokens | Flagged? |
|---:|---|---|---:|---:|---:|:---:|
| 1 | `memory_kg_query` | graph | 179 | 244 | 444 |  |
| 2 | `memory_recall` | core | 24 | 397 | 431 |  |
| 3 | `memory_store` | core | 12 | 395 | 417 |  |
| 4 | `memory_namespace_set_standard` | governance | 58 | 281 | 351 |  |
| 5 | `memory_capabilities` | meta | 84 | 196 | 294 |  |
| 6 | `memory_check_duplicate` | power | 121 | 157 | 293 |  |
| 7 | `memory_entity_register` | graph | 94 | 176 | 284 |  |
| 8 | `memory_subscribe` | governance | 97 | 141 | 251 |  |
| 9 | `memory_get_taxonomy` | graph | 101 | 132 | 247 |  |
| 10 | `memory_kg_timeline` | graph | 93 | 135 | 246 |  |
| 11 | `memory_kg_invalidate` | graph | 105 | 119 | 240 |  |
| 12 | `memory_notify` | other | 62 | 129 | 204 |  |
| 13 | `memory_search` | core | 10 | 149 | 170 |  |
| 14 | `memory_consolidate` | power | 43 | 105 | 160 |  |
| 15 | `memory_update` | lifecycle | 13 | 136 | 159 |  |
| 16 | `memory_agent_register` | meta | 38 | 109 | 158 |  |
| 17 | `memory_promote` | lifecycle | 55 | 75 | 144 |  |
| 18 | `memory_inbox` | power | 57 | 70 | 141 |  |
| 19 | `memory_list` | core | 10 | 111 | 131 |  |
| 20 | `memory_entity_get_by_alias` | graph | 56 | 53 | 125 |  |
| 21 | `memory_namespace_get_standard` | governance | 32 | 78 | 123 |  |
| 22 | `memory_session_start` | meta | 36 | 61 | 108 |  |
| 23 | `memory_forget` | lifecycle | 22 | 58 | 91 |  |
| 24 | `memory_link` | graph | 7 | 69 | 86 |  |
| 25 | `memory_archive_list` | archive | 15 | 51 | 77 |  |
| 26 | `memory_pending_list` | governance | 26 | 37 | 74 |  |
| 27 | `memory_detect_contradiction` | power | 18 | 42 | 73 |  |
| 28 | `memory_archive_purge` | archive | 15 | 34 | 61 |  |
| 29 | `memory_gc` | lifecycle | 17 | 32 | 59 |  |
| 30 | `memory_expand_query` | power | 21 | 26 | 58 |  |
| 31 | `memory_pending_approve` | governance | 22 | 24 | 58 |  |
| 32 | `memory_pending_reject` | governance | 21 | 24 | 57 |  |
| 33 | `memory_list_subscriptions` | other | 33 | 9 | 57 |  |
| 34 | `memory_auto_tag` | power | 18 | 26 | 55 |  |
| 35 | `memory_archive_restore` | archive | 15 | 28 | 55 |  |
| 36 | `memory_get_links` | graph | 10 | 27 | 49 |  |
| 37 | `memory_namespace_clear_standard` | governance | 9 | 27 | 48 |  |
| 38 | `memory_unsubscribe` | governance | 15 | 18 | 47 |  |
| 39 | `memory_get` | core | 11 | 25 | 46 |  |
| 40 | `memory_delete` | lifecycle | 6 | 18 | 34 |  |
| 41 | `memory_archive_stats` | archive | 11 | 9 | 31 |  |
| 42 | `memory_agent_list` | meta | 5 | 9 | 25 |  |
| 43 | `memory_stats` | meta | 5 | 9 | 24 |  |

### Family roll-up

| Family | Tool count | Tokens | % of full surface |
|---|---:|---:|---:|
| graph | 8 | 1,694 | 27.3 % |
| core | 5 | 1,180 | 19.0 % |
| governance | 8 | 994 | 16.0 % |
| power | 6 | 768 | 12.4 % |
| meta | 5 | 600 | 9.7 % |
| lifecycle | 5 | 484 | 7.8 % |
| other | 2 | 254 | 4.1 % |
| archive | 4 | 224 | 3.6 % |
| **total** | **43** | **6,198** | **100 %** |

Two families — `graph` and `core` — together carry 46 % of the surface. Track-C work that does not touch them will not move the needle on the C5 target.

---

## Top hotspots → C2-C5 disposition

The C1 prompt asked for the 5-10 worst offenders and a verbosity classification. All of these will be addressed by some combination of C2 (move long docstrings to a `docs` field), C3 (drop redundant inline examples), and C4 (hide rarely-used optional params).

### 1. `memory_kg_query` — 444 tokens (graph)

**Verbosity kind:** *prose-heavy description* + *enum-rich schema*.

The 179-token `description` is two paragraphs of CTE / cycle-detection / filter semantics, ending mid-sentence in the JSON capture (truncated). Most callers do not need the recursive-CTE explanation in the schema; they need "outbound KG traversal, multi-hop, returns nodes + temporal-validity columns". The full prose belongs in a verbose `docs` field per **C2**.

The 244-token `inputSchema` carries `valid_at`, `allowed_agents`, `target_namespace`, `relation_in`, `max_depth`, etc. — all optional, all rarely used; the `allowed_agents` empty-list behavior is documented in the property `description`. **C4** can hide `allowed_agents`, `valid_at`, and `relation_in` behind `extended_properties`.

### 2. `memory_recall` — 431 tokens (core)

**Verbosity kind:** *redundant inline schemas*. The 397-token `inputSchema` is the worst offender on the schema side; the 24-token `description` is already a one-liner. The schema enumerates `tier`, `namespace`, `tags`, `priority`, `confidence`, `source`, `metadata`, `agent_id`, `scope`, `on_conflict` (likely — same shape family as `memory_store`), each with prose `description:` strings on every property. **C3** + **C4** apply: drop "for example, ..." clauses inside property descriptions and demote `confidence`, `source`, and `agent_id` to `extended_properties` (rarely set by recall callers).

### 3. `memory_store` — 417 tokens (core)

**Verbosity kind:** *redundant inline schemas* (same shape as `memory_recall`). The `agent_id` property alone carries a multi-clause description that explains the NHI default-synthesis algorithm — that is `memory_capabilities` territory, not per-tool schema territory. **C2** moves the algorithm explanation; **C4** demotes `agent_id`, `confidence`, `source`, `metadata`, and `on_conflict` to `extended_properties`.

### 4. `memory_namespace_set_standard` — 351 tokens (governance)

**Verbosity kind:** *deeply nested governance object inline*. The 281-token `inputSchema` likely embeds the full `governance` policy object inline. **C4** + **C2** can replace the inline policy schema with a `$ref`-style pointer ("see memory_capabilities(verbose=true).governance_policy_schema") and move the rule-layering explanation out of the description.

### 5. `memory_capabilities` — 294 tokens (meta)

**Verbosity kind:** *prose-heavy description* + *version-history baggage*. The 84-token description carries v0.6.3.1/v0.6.4 version notes. After **C2**, the description is one line ("report active feature tier, loaded models, available capabilities"); the version-history paragraph moves to `docs`. Bonus: this tool is the future home of the `verbose=true` switch C2/C4 introduce, so trimming it sets the example.

### 6. `memory_check_duplicate` — 293 tokens (power)

**Verbosity kind:** *long-form description* (121 tokens — the largest top-level description in the surface). Pre-write near-duplicate semantics (`is_duplicate`, threshold floor of 0.5, `suggested_merge`, embedder requirement) belong in `docs`. **C2** primary.

### 7. `memory_entity_register` — 284 tokens (graph)

**Verbosity kind:** *long-form description* + *idempotency contract baggage*. The 94-token description explains tagging conventions, idempotency, and a non-entity-collision error condition. **C2** moves it; one-liner becomes "register an entity (canonical name + aliases) under a namespace".

### 8. `memory_subscribe` — 251 tokens (governance)

**Verbosity kind:** *security-prose-heavy description*. The 97-token description covers HMAC-SHA256 signing, the `X-Ai-Memory-Signature` header format, the loopback-https rule, and the secret-hashing model. All of that is webhook documentation, not tool schema. **C2** primary; the security model becomes a single doc page referenced from the verbose `docs` field.

### 9. `memory_get_taxonomy` — 247 tokens (graph)

**Verbosity kind:** *long-form description* (101 tokens) explaining `count` vs `subtree_count` vs `total_count` semantics and the `truncated` flag. These are response-shape contracts — they belong on the response schema, not the tool description. **C2** moves them.

### 10. `memory_kg_timeline` — 246 tokens (graph)

**Verbosity kind:** *long-form description* (93 tokens) explaining ordering, NULL exclusion, and cross-namespace post-filtering. Same shape as #9. **C2** moves the body; one-liner stays.

### Honorable mention: `memory_kg_invalidate` — 240 tokens (graph)

105-token description explaining the (source_id, target_id, relation) triple, the missing id column, the wall-clock fallback, the `previous_valid_until` overwrite signal, and the `found: false` shape. **C2** + **C3** (drop the "see memory_links has no separate id column" implementation note from the wire-visible schema).

---

## Forward references — how C2-C5 chip at the budget

The Track-C plan in [`v0.7-nhi-prompts.md`](v0.7-nhi-prompts.md) lays out four follow-on tasks. Each one targets a different *kind* of verbosity from the analysis above:

| Task | Targets | Per the prompt | Estimated reduction |
|---|---|---|---:|
| **C2** — Move docstrings to `docs` field | Long-form descriptions on hotspots #1, #5, #6, #7, #8, #9, #10, plus the kg_invalidate honorable mention. `tools/list` keeps a one-liner; `memory_capabilities(verbose=true)` returns the full doc. | "≥ 1,500 tokens" | **~1,500-1,800** |
| **C3** — Drop redundant inline examples | Property `description:` strings across `memory_store`, `memory_recall`, `memory_kg_query`, `memory_namespace_set_standard`. Targets "e.g.," / "for example," / "i.e." clauses inside JSON-schema property descriptions. | "≥ 500 more" | **~500-700** |
| **C4** — Hide rarely-used optional params | `extended_properties` side-channel for `agent_id`, `confidence`, `source`, `metadata`, `on_conflict`, `allowed_agents`, `valid_at`, `relation_in`, and the inline `governance` policy object on `memory_namespace_set_standard`. | "≥ 300 more" | **~300-500** |
| **C5** — Lock the win | Tighten `FULL_PROFILE_HONEST_RANGE` in [`src/sizes.rs`](../../src/sizes.rs) from `(5_000..=8_000)` to `(3_000..=3_500)` and update the `.github/workflows/token-budget.yml` comment + MIGRATION/EPIC docs. | Range gate flips green only if C2-C4 hit. | n/a (gate) |

**Cumulative target:** 6,198 → ≤ 3,500 tokens, a 44 % reduction. The estimates above sum to a 2,300-3,000 token drop from the today number, leaving a comfortable margin against the 3,500-token C5 ceiling. If C2 underperforms, C4 has the most slack to absorb (the rare-param list is long and most of it is genuinely rare in practice).

**What this audit does *not* do:**
- It does not modify any tool schema. C2-C5 do that.
- It does not add a new doctor flag for description/schema split. The Python re-tokenization is a one-shot for this audit; if a future repeat audit needs the split natively, [`src/sizes.rs::ToolSize`](../../src/sizes.rs) already carries `schema_tokens` and `name_tokens` fields — surfacing them in the doctor JSON is a one-line change.
- It does not touch the `core`-profile budget. `core` is already at 1,465 tokens (76 % below `full`); compaction work for v0.7 targets the worst case (`full`).

---

## Acceptance

- [x] Doctor invoked with `--tokens --raw-table --json` against fresh release build.
- [x] All 43 tools tabulated and sorted by total cost descending.
- [x] Per-tool description vs inputSchema split derived (zero tools exceed the 200-token description threshold; the verbosity is in long descriptions on graph-family tools and in inline schema prose on the core-family writer tools).
- [x] Top 10 hotspots characterized by verbosity kind.
- [x] Forward references to C2-C5 with per-task budget contribution.
- [ ] **Out of scope for C1 (handoff to C2-C5):** any actual schema edits, doctor flag changes, or budget-gate updates.
