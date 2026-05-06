# `auto-link-detector` — v0.7 R3 reference post_store hook

`auto-link-detector` is the **reference implementation** of the
`post_store` auto-link inference substrate for v0.7's R3
commitment. It is *not* a production-grade detector. The binary
exists to demonstrate the substrate wiring (envelope → decision
round-trip, opt-in gating, metadata-bag derivation) so subsequent
tasks can swap in a richer heuristic without touching the
production hook pipeline.

## What it does

1. Reads a JSON `FireEnvelope` from stdin (the same shape
   `src/hooks/executor.rs::FireEnvelope` writes to every hook
   subprocess).
2. Pulls the just-stored memory's `id`, `namespace`, and `content`
   out of the payload.
3. Walks an optional `payload.recent_namespace_memories` bag (an
   array of recent neighbours the executor may surface alongside
   the just-stored row — capped at N=50 by the production wiring;
   the detector re-enforces an internal cap so a misconfigured
   executor can't fan the heuristic out unbounded).
4. For each candidate, computes Jaccard similarity over normalised
   lowercase tokens. If similarity exceeds the threshold (default
   `0.4`; override via `AUTOLINK_SIMILARITY_THRESHOLD`), proposes
   an `auto-related` link.
5. Returns
   `{"action":"modify","delta":{"metadata":{"auto_related_links":[ ... ]}}}`
   — proposals ride inside the metadata bag preserving any keys
   an upstream hook already wrote.

## Proposal shape

Each entry in `auto_related_links` looks like:

```json
{
  "source": "mem-new",
  "target": "mem-neighbour",
  "kind": "auto-related",
  "attest_level": "R3",
  "score": 0.62
}
```

* `source` is always the just-stored row's id.
* `target` is the neighbour the heuristic matched.
* `kind = "auto-related"` is the constant link type the R3 brief
  reserves for heuristic proposals.
* `attest_level = "R3"` is a sentinel naming the commitment that
  produced the proposal. It is **not** the H-track cryptographic
  attestation enum (`unsigned` / `self_signed` / `peer_attested`)
  — a downstream minter that lands the proposal as a real
  `memory_links` row maps `R3` → `unsigned` (or whatever the
  operator's policy dictates). The reference impl never claims
  `self_signed` or above; that would be a security regression
  because there is no agent identity behind the proposal.
* `score` is the Jaccard value so a follow-up scorer can rank
  or filter without recomputing.

## What it deliberately does **not** do

- **No embedding-similarity scoring.** The R3 brief mentions
  cosine similarity over the existing HNSW index as one of the
  heuristic options; the impl here uses Jaccard token overlap so
  the binary stays free of any ANN / embedding dependency.
  Wire-shape proposals carry a per-link `score` field a follow-up
  detector can repopulate from cosine without changing the wire
  contract.
- **No LLM scoring.** A production detector would invoke
  `OllamaClient::generate` (existing infrastructure in
  `src/llm.rs`) with a relatedness prompt to upgrade `score`
  before emitting. The reference impl uses a deterministic
  heuristic so the substrate test can run in CI without an
  Ollama daemon.
- **Does not call `memory_link` directly.** The proposals ride in
  the `Modify` delta's metadata bag. A follow-up production hop
  (post-G11) will add a persister that walks
  `auto_related_links` and calls `db::create_link` transactionally
  with the original store. Today's executor degrades `Modify` on
  `post_store` to `Allow` per `src/hooks/decision.rs` — the
  proposals nonetheless surface on stdout for the chain log and
  the integration test harness.
- **Does not query the database.** The neighbour list arrives
  pre-collected in `payload.recent_namespace_memories`. The
  production wiring task will surface that bag from
  `db::list_recent_in_namespace`; the reference impl just
  consumes whatever the executor hands it.

## Modes

```bash
# One-shot (matches src/hooks/executor.rs::ExecExecutor)
echo '{"event":"post_store","payload":{...}}' | auto-link-detector

# Daemon (matches DaemonExecutor — newline-delimited JSON in/out)
auto-link-detector --daemon
```

## Opt-in — `hooks.toml`

The detector is **off by default**. Operators wire it as a
`post_store` hook in `hooks.toml`:

```toml
[[hooks.post_store]]
command = "/usr/local/bin/auto-link-detector"
mode = "daemon"          # or "exec" for low-rate write paths
priority = 50
timeout_ms = 1500
namespace = "team/eng/*" # optional — gate by namespace pattern
fail_mode = "open"       # never deny a store because the detector tripped
```

See `docs/hooks/` for the canonical schema (G1).

## Tunables

| Env knob                          | Default | Effect                                                   |
|-----------------------------------|--------:|----------------------------------------------------------|
| `AUTOLINK_SIMILARITY_THRESHOLD`   |   `0.4` | Minimum Jaccard score above which a proposal is emitted. |

Out-of-range values (NaN, ≤0.0, ≥1.0) collapse to the default — a
misconfigured hook can't disable the gate by setting the
threshold to 0.0.

## Limitations the reference acknowledges

- Token-bag scoring is English-leaning and Latin-script only.
  Multilingual content will under-link.
- Stop-word list is small and English-only.
- The detector caps at 50 candidates and 16 proposals per fire;
  production tuning is a follow-up task.
- No semantic / cross-language matching. Two memories saying
  "Postgres autovacuum tuning" and "configuration du nettoyage
  automatique de Postgres" will not link.

## Testing

```bash
cd tools/auto-link-detector
cargo test
```

The unit suite covers the five R3 acceptance cases (high
similarity, low similarity, identical content, empty content,
malformed input) plus the heuristic edge cases: self-id
suppression, cross-namespace skip, empty token bag, proposal
cap, metadata-key preservation, and round-trip in both stdio
modes.

The main-crate integration test
(`tests/g11_auto_link_detector.rs`) builds this binary and
exercises the end-to-end stdio contract against the same
`FireEnvelope` shape the production executor writes.
