# Memory-Kind Vocabulary (Form 6, issue #759)

v0.7.x extends the substrate's `MemoryKind` enum from the original
three lifecycle variants (`Observation` / `Reflection` / `Persona`)
with the seven-variant Batman taxonomy extension. The full set is now:

| variant | purpose |
| --- | --- |
| `observation` | direct note from the caller — the default. |
| `reflection`  | curator-synthesised summary over lower-depth peers. |
| `persona`     | curator-generated entity profile (QW-2). |
| `concept`     | abstract definition / vocabulary term. |
| `entity`      | named real-world thing (person, org, product, system). |
| `claim`       | factual assertion the caller is committing to. |
| `relation`    | typed pair / triple anchored in the memory substrate. |
| `event`       | temporally-bounded happening. |
| `conversation`| captured dialogue turn. |
| `decision`    | choice point with rationale (L1-6 reservation). |

The first three are the v0.7.0 lifecycle variants and are unchanged.
The seven new variants give downstream readers a richer
filter-by-kind surface aligned with the Batman framework's exemplar
(Tolaria's frontmatter-as-type schema).

## Schema impact: none

The `memories.memory_kind TEXT` column has no CHECK constraint on
either the SQLite or Postgres backends, so the new variants land as
new string values on the existing column. No migration required;
schema version stays at v37 / v18 respectively. Backward compat:

* Old rows with no `memory_kind` value read as `Observation` (the
  SQL `DEFAULT 'observation'`).
* Future variants emitted by a newer client to an older binary
  read as `Observation` via the `unwrap_or_default()` fallback in
  `row_to_memory`.
* Old binaries reading a new variant from the DB also fall through
  to `Observation` — the wire shape stays compatible across version
  drift.

## Recall filter

The new `kinds` parameter on `memory_recall` (MCP), `?kinds=…` (HTTP
GET), and `kinds: …` (HTTP POST body) accepts either:

* a comma-separated string: `"concept,entity,claim"`
* a JSON array: `["concept", "entity", "claim"]`
* the literal `"all"` (case-insensitive) ⇒ no filter (equivalent to
  omission)

OR-of-kinds within the param; AND with the other filters (namespace,
tags, time-window, visibility). Unknown tokens are silently dropped
so a newer client emitting a future variant doesn't break recall on
an older binary.

### MCP

```jsonc
{
  "tool": "memory_recall",
  "args": {
    "context": "policy on token rotation",
    "kinds": ["claim", "decision"]
  }
}
```

### HTTP

```http
GET /api/v1/memories/recall?q=policy+rotation&kinds=claim,decision
```

```jsonc
POST /api/v1/memories/recall
{
  "context": "policy on token rotation",
  "kinds": ["claim", "decision"]
}
```

### CLI

```bash
ai-memory recall "policy on token rotation" --kind claim,decision
```

## Auto-classify pre-store hook

The substrate ships a namespace-policy-gated pre-store hook
([`auto_classify_kind`](../src/hooks/pre_store/auto_classify_kind.rs))
that may rewrite a stored memory's `memory_kind` from the default
`Observation` to a more specific Batman-taxonomy variant. Three
policy modes, set on the namespace standard's `metadata.governance`
JSON blob under `auto_classify_kind`:

```jsonc
{
  "governance": {
    "auto_classify_kind": "off" | "regex_only" | "regex_then_llm"
  }
}
```

* **`off` (default).** Substrate quiet — caller-supplied (or default
  `Observation`) kind stands.
* **`regex_only`.** Deterministic regex heuristics. ~tens of
  microseconds per call; safe to run on every write. Fires only
  when the content carries a strong signal (e.g. `is_a` ⇒
  `Concept`, `happened on` ⇒ `Event`, `X says:` ⇒ `Conversation`,
  `decided to` ⇒ `Decision`, `depends on` ⇒ `Relation`). Misses
  keep the row at `Observation`.
* **`regex_then_llm`.** Regex first; if no heuristic fires, fall
  through to a single-shot LLM classifier. Opt-in only — the
  substrate never spawns an LLM round-trip on a namespace whose
  policy is `off` or `regex_only`. The LLM round-trip path is
  feature-gated on `llm.classify_kind`; if a runtime doesn't
  carry a classifier, the hook degrades to `regex_only` semantics
  silently (logged at debug).

The caller-supplied `kind` parameter on `memory_store` always wins
— the hook only fills in `Observation` (the default) when no kind
was set. This keeps explicit-typing callers in full control while
giving operators an opt-in path to classify legacy / unstructured
content automatically.

### Operator surface

The substrate exposes the recall-filter and auto-classify wiring
under the `memory_kind_vocab` block of the v3 capabilities
response. Operators can read the live state via:

```bash
ai-memory mcp call memory_capabilities '{"schema_version":"3"}' | jq .memory_kind_vocab
```

(The v0.7-alpha drafts referenced `ai-memory doctor --capabilities=v3`;
that flag was not shipped. The MCP `memory_capabilities` tool is the
canonical inspection surface — it works against any running daemon
regardless of profile because `memory_capabilities` is on the
`ALWAYS_ON_TOOLS` allowlist.)

```jsonc
{
  "vocabulary": ["observation", "reflection", "persona", "concept",
                 "entity", "claim", "relation", "event",
                 "conversation", "decision"],
  "recall_filter": "implemented",
  "cli_filter": "implemented",
  "auto_classify": "implemented",
  "auto_classify_modes": ["off", "regex_only", "regex_then_llm"]
}
```

## Forward-compat reservations

`Decision` is the only L1-6 reservation in the v0.7.x set. The
L1-6 work (v0.8.0) will likely add columns for rationale /
alternatives on top of the variant; binaries that ship the
variant now can already type-tag decisions so downstream readers
get a stable filter surface from day one.

## Why no schema bump

The original L1-1 work (v0.7.0) landed the `memory_kind TEXT NOT
NULL DEFAULT 'observation'` column under migration 0025 / 0018
without a CHECK constraint. That was a deliberate forward-compat
choice: new variants land as new column values; no migration is
required to widen the accepted set. The decision is documented in
the L1-1 commit and validated by Form 6's no-migration ship.

Cold mountain.
