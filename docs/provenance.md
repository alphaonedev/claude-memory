# Fact-Provenance (Form 4)

v0.7.0 closes Batman's **Form 4 — fact-provenance** to IMPLEMENTED. The
Form 4 brief asks: when an agent stores a memory, can the substrate
record *where that fact came from*, alongside who recorded it and
when? Up through v0.6.x ai-memory captured three of the four
fact-grain fields: a `source` role label ("user" / "claude" /
"api" / …), a `created_at` capture timestamp, and a `confidence`
score. The audit (PR #753) flagged the missing fourth: a first-class
`citations` field, a URI-form pointer that distinguishes "where did
this claim come from" from "which role asserted it", and atom-grain
span offsets into the parent source body so a downstream consumer can
re-derive the cited slice.

This release lands all three under issue #757. The shape mirrors the
scholarly-citation convention and the substrate's existing typed-link
discipline.

## What we added

Three new columns on the `memories` table (sqlite schema v38 /
postgres schema v37):

| Column | Type | Meaning |
|--------|------|---------|
| `citations` | `TEXT NOT NULL DEFAULT '[]'` | JSON-encoded array of `Citation` envelopes. |
| `source_uri` | `TEXT NULL` | First-class URI-form pointer to the cited source body. |
| `source_span` | `TEXT NULL` | JSON `{start, end}` byte-range into the parent source body. |

The Rust shape on `crate::models::Memory` mirrors the columns
verbatim. Legacy rows take the SQL default for `citations` (empty
array) and `NULL` for the other two; no application-side backfill is
required.

## Fact-provenance vs action-provenance

These two surfaces are complementary, not duplicate:

* **Fact-provenance** answers: *where did this claim come from?* It
  carries the URL or doc id of the cited source, the timestamp at
  which the agent read it, an optional SHA-256 hash so a verifier
  can detect source drift, and an optional byte-range span pinning
  the exact quote. Form 4 is the fact-provenance form.

* **Action-provenance** answers: *who performed this write, when, and
  with what canonical hash?* It rides on the `signed_events`
  append-only chain (v0.7.0 H5 / V-4 closeout). Form 7 (substrate
  authority enforcement) consumes action-provenance; an Ed25519
  signature on the canonical-bytes encoding gives tamper-evidence at
  the write boundary.

A memory minted from a curated dataset will carry *both*: a
`citations` entry pointing at the dataset, and a `signed_events` row
recording the write itself.

## Citation shape

```rust
pub struct Citation {
    pub uri: String,                  // URI form — see below
    pub accessed_at: String,          // RFC3339
    pub hash: Option<String>,         // SHA-256 hex (64 chars)
    pub span: Option<SourceSpan>,     // byte-range into the cited body
}

pub struct SourceSpan {
    pub start: usize,
    pub end: usize,
}
```

`crate::validate::validate_citation` enforces:

* `uri` parses as one of the typed schemes accepted by
  `validate_source_uri` (`uri:` / `doc:` / `file:`).
* `accessed_at` parses as RFC3339.
* `hash` (when present) is exactly 64 lowercase hex characters.
* `span` (when present) satisfies `start < end`.

`MAX_CITATIONS_PER_MEMORY = 64` caps the column size; an operator
authoring legitimate fact-grain provenance rarely needs more than a
handful on a single memory.

## Source-URI semantics

`source_uri` is a first-class URI-form pointer that names *where the
source body lives*. It is deliberately **distinct from the existing
`source` column**, which is a role label (the writer's identity, not
the cited content's location). Three accepted schemes:

* `uri:<...>` — HTTP(S) URL or any absolute URI.
* `doc:<...>` — substrate document id (typically a memory id, e.g.
  the parent of an atom).
* `file:<...>` — filesystem path.

Bare strings without a scheme are rejected by
`validate_source_uri`, so a caller can never accidentally stuff a
role label like `"user"` into the URI column. The substrate
preserves the role label in `source` and the URI in `source_uri`
side-by-side.

## Atom-grain span computation

The WT-1-B atomisation engine stamps `source_span` on every atom it
emits. Approach:

1. Run the curator pass to decompose the parent source body into
   atomic propositions (no change from WT-1-B).
2. For each atom, search `parent_source.content[cursor..]` for the
   atom's text verbatim. When found, the byte-range becomes the
   atom's `source_span`, and the cursor advances so a subsequent
   atom that quotes the same prefix doesn't latch onto the same
   offset.
3. When verbatim search misses (the curator paraphrased, or
   whitespace differs), the substrate falls back to `None` for the
   span but still stamps `source_uri = doc:<parent-id>` so the
   lineage edge survives without the byte-range.
4. Each atom inherits the parent's `citations` array — the same
   supporting evidence applies to every decomposed proposition.

The half-open `[start, end)` convention matches Rust's slice
semantics: a downstream consumer can recover the cited slice with
`parent.content[span.start..span.end]`.

## Recall filters

The recall surface (CLI / MCP / HTTP) accepts two new optional
filters that compose with every existing predicate (namespace,
tags, time-window, visibility, tier, archived/atom toggle):

* `--has-citations` (CLI) / `has_citations: true` (MCP/HTTP) — keep
  only memories whose `citations` array is non-empty. Useful for
  surfacing the fact-grade subset of a recall result.
* `--source-uri-prefix <prefix>` / `source_uri_prefix: <prefix>` —
  keep only memories whose `source_uri` column begins with this
  exact string. Typical use: `doc:` to find every atom or memory
  pointing at a substrate doc; `uri:https://` to find memories
  citing an HTTP source.

The filters run in Rust after the substrate-level recall query
returns, so the existing `db::recall` / `db::recall_hybrid`
substrate signatures stay stable. No new MCP tool is registered —
the baseline tool count (69 total / 68 visible) is preserved.

## Forensic bundle export

`crate::forensic::bundle::MemoryEnvelope` now carries `citations`
(always emitted as a JSON array, empty when the row holds none),
`source_uri` (omitted when NULL), and `source_span` (omitted when
NULL). An auditor opening the bundle sees the same fact-provenance
shape that the substrate carries on the row, with no
cross-reference required.

## Backward compatibility

Pure additive on legacy rows. The SQL `DEFAULT '[]'` on `citations`
and the `Option<...>` shape on the other two columns mean every
pre-v38 row reads cleanly without an application-side backfill.
serde defaults on the Rust struct keep federation peers happy: a
pre-v38 binary deserialising a v38-shaped payload sees the new
fields as default values.
