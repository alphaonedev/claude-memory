# `transcript-extractor` — v0.7 R5 reference pre_store hook

`transcript-extractor` is the **reference implementation** of the
`pre_store` extraction substrate for v0.7's R5 commitment. It is
*not* a production-grade extractor. The binary exists to demonstrate
the substrate wiring (envelope → decision round-trip, opt-in gating,
metadata-bag derivation) so subsequent tasks can swap in a richer
heuristic without touching the production hook pipeline.

## What it does

1. Reads a JSON `FireEnvelope` from stdin (the same shape
   `src/hooks/executor.rs::FireEnvelope` writes to every hook
   subprocess).
2. Recognises the in-flight memory as a transcript via any of three
   signals:
   - `metadata.kind == "transcript"` (explicit), or
   - `namespace` starts with `transcript/` / `transcripts/`, or
   - the first 512 chars of `content` carry a dialogue speaker
     marker (`User:`, `Assistant:`, `<|user|>`, etc.).
3. Splits the content into paragraphs, scores each by a token-bag
   density heuristic, keeps the top-K (`K = 3` by default;
   override via `EXTRACTOR_TOP_K`).
4. Returns `{"action":"modify","delta":{"metadata":{
   "extracted_memories":[ ... ]}}}` — the survivors are appended to
   the in-flight memory's metadata bag, preserving any keys an
   upstream hook already wrote.

## What it deliberately does **not** do

- **No LLM call.** A production extractor would invoke
  `OllamaClient::generate` (existing infrastructure in `src/llm.rs`)
  with a topic-extraction prompt and synthesise candidate memories
  from the model output. The reference impl uses a deterministic
  bag-of-words heuristic so the substrate test can run in CI
  without an Ollama daemon.
- **No embedding-similarity scoring.** The R5 prompt mentions
  embedding-similarity as one of the heuristic options; the impl
  here uses token overlap so the binary stays free of any ANN /
  embedding dependency. Wire-shape derivations carry a
  per-candidate `score` field a follow-up extractor can repopulate
  from cosine similarity without changing the wire contract.
- **Does not mint standalone memory rows.** The pre_store hook
  contract surfaces a single `Modify(MemoryDelta)` — the impl
  surfaces derived candidates inside the `metadata.extracted_memories`
  bag rather than creating sibling rows. Minting rows requires
  touching the production store path (G3-G11 own that). A future
  `post_store` companion hook will walk `extracted_memories` and
  persist each entry plus a `derived_from` link.
- **No `memory_transcript_links` writes.** The candidate carries
  `span_start` / `span_end` byte offsets the future production
  hook can wire into the I2 join table; the reference impl just
  forwards them.

## Modes

```bash
# One-shot (matches src/hooks/executor.rs::ExecExecutor)
echo '{"event":"pre_store","payload":{...}}' | transcript-extractor

# Daemon (matches DaemonExecutor — newline-delimited JSON in/out)
transcript-extractor --daemon
```

## Opt-in

The extractor is **off by default**. Operators wire it in two
places:

1. `hooks.toml` — register the binary as a `pre_store` hook. See
   `docs/hooks/` for the canonical schema (G1).
2. `config.toml` — flip `auto_extract = true` for the namespace(s)
   that should drive extraction:

   ```toml
   [transcripts.namespaces."agent/claude"]
   auto_extract = true
   default_ttl_secs = 86400

   [transcripts.namespaces."team/eng/*"]
   auto_extract = true
   ```

   Resolution follows the same precedence as `default_ttl_secs`
   (exact match → longest `prefix/*` → `*` wildcard → off). See
   `TranscriptsConfig::auto_extract_for` in `src/config.rs`.

When the namespace flag is `false` (or unset) the extractor binary
itself still executes, but the production `pre_store` chain
short-circuits before invoking it — so the daemon child never
sees an envelope it shouldn't process.

## Limitations the reference acknowledges

- Token-bag scoring is English-leaning and Latin-script only.
  Multilingual transcripts will under-extract.
- `paragraphs_with_spans` requires blank-line separation; chats
  formatted as one-line-per-turn without blank lines will be
  treated as a single paragraph and short-circuit to `Allow`.
- The 16-character paragraph floor and 80-character title cap are
  hard-coded; production tuning is a follow-up task.
- Stop-word list is small and English-only.

## Testing

```bash
cd tools/transcript-extractor
cargo test
```

The unit suite covers: envelope round-trip in both modes, all three
transcript-classification signals, candidate count clipping via
`EXTRACTOR_TOP_K`, metadata-key preservation, malformed-input
graceful degrade to `Allow`, and byte-span correctness.

The main-crate integration test
(`tests/transcript_extractor.rs`) builds this binary and exercises
the end-to-end stdio contract against the same `FireEnvelope`
shape the production executor writes.
