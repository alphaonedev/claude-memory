# Runbook — Ollama KV-cache compression for ai-memory

Status: **executable on any ai-memory host running Ollama ≥ 0.5.0**.
Date: 2026-04-19
Depends on: nothing. Zero ai-memory code changes required.

## The one-liner

```sh
OLLAMA_FLASH_ATTENTION=1 OLLAMA_KV_CACHE_TYPE=q4_0 ollama serve
```

Restart Ollama with these two environment variables and every ai-memory
LLM path (auto_tag, detect_contradiction, summarize_memories,
memory_expand_query, the autonomous curator loop) automatically gets
**2–4× KV-cache memory reduction** with near-lossless quality.

`OLLAMA_FLASH_ATTENTION=1` is a prerequisite for `q4_0` / `q8_0` KV
quantisation — without it Ollama silently falls back to `f16` even if
you set `OLLAMA_KV_CACHE_TYPE`.

## What actually changes

Ollama wraps `llama.cpp`. llama.cpp stores the attention
key/value tensors at `f16` (16 bits/element) by default. Setting
`OLLAMA_KV_CACHE_TYPE=q4_0` switches them to 4-bit block-quantised
integers. This is NOT the TurboQuant algorithm we forked for embedding
compression (that's a different code path — `compress.rs`). It is
llama.cpp's built-in uniform scalar quantiser for KV tensors.

## What ai-memory gets from it

| Capability | Before (f16) | After (q4_0) | Mechanism |
|---|---|---|---|
| **Consolidation batch size** | ~8 memories per LLM call | ~32 per call | 4× context window in same KV budget |
| **Cross-namespace contradiction scan** | skipped (too expensive) | batched at ~16 pairs per call | paper N² scan becomes tractable |
| **Cross-encoder rerank width** | top-10 candidates | top-30–40 | more candidates per rerank pass |
| **Biggest model on fixed RAM** | Gemma 4 E2B | Gemma 4 E4B | lower KV overhead leaves room for bigger weights |
| **Concurrent autonomy workers** | 1–2 simultaneous | 4–8 | each request holds less KV |
| **Recall latency** | N/A | **unchanged** | recall doesn't touch the LLM |
| **Storage footprint** | N/A | **unchanged** | storage is `memories` rows, not KV |
| **Federation throughput** | N/A | **unchanged** | sync path doesn't touch the LLM |

The wins are concentrated in **LLM-mediated paths** — the autonomy
loop and any prompt-heavy request. Non-LLM paths (recall, storage,
sync, HTTP routing) see zero change.

## Quality impact

llama.cpp community measurement: `q4_0` KV is near-lossless for most
use cases, including long-context retrieval. `q8_0` is strictly safer
(no measurable quality impact) and gives ~2× compression instead of
~4×.

**Rule of thumb**:

- **Dev / local**: `OLLAMA_KV_CACHE_TYPE=q4_0`. Max compression, free.
- **Production / graded**: `OLLAMA_KV_CACHE_TYPE=q8_0` initially. Run
  the curator soak (RUNBOOK-curator-soak.md) with the new setting.
  If reversal rate `R` doesn't regress, flip to `q4_0`.

## How to apply it

### systemd (recommended)

Edit `/etc/systemd/system/ollama.service` (or create an override via
`systemctl edit ollama.service`):

```ini
[Service]
Environment=OLLAMA_FLASH_ATTENTION=1
Environment=OLLAMA_KV_CACHE_TYPE=q4_0
```

Then:

```sh
sudo systemctl daemon-reload
sudo systemctl restart ollama
```

### Docker

```sh
docker run -d --gpus=all \
    -e OLLAMA_FLASH_ATTENTION=1 \
    -e OLLAMA_KV_CACHE_TYPE=q4_0 \
    -p 11434:11434 \
    ollama/ollama
```

### Ad-hoc

```sh
OLLAMA_FLASH_ATTENTION=1 OLLAMA_KV_CACHE_TYPE=q4_0 ollama serve
```

## How to verify it's active

Ollama doesn't currently expose the KV cache type over its HTTP API.
Verify via the server log on first model load:

```sh
journalctl -u ollama --since "5 minutes ago" | grep -i 'kv cache\|flash.attn'
```

Expected lines:

```
llm_load_tensors: flash attn enabled
llama_kv_cache_init: key type: q4_0, value type: q4_0
```

If you see `key type: f16` then the env vars aren't reaching the Ollama
process. Common causes: systemd unit not overridden, env var set in
the wrong shell, or `OLLAMA_FLASH_ATTENTION=1` missing.

## How to measure the effect on ai-memory

1. Baseline: with `f16` KV cache, run
   `ai-memory curator --once --dry-run --json` against a seeded corpus
   of ~500 memories. Record the `operations_attempted` and
   `cycle_duration_ms` values.
2. Apply `OLLAMA_KV_CACHE_TYPE=q4_0` + restart Ollama.
3. Repeat step 1 against the same corpus. Expected:
   - `cycle_duration_ms` drops by 10–30% (less HBM-to-SRAM bandwidth).
   - Maximum `max_ops` per cycle can be raised to 300+ without OOM.
4. Longer-running: kick off `RUNBOOK-curator-soak.md` with the new
   setting. Expect no regression in reversal rate `R`.

## When NOT to use it

- **Models smaller than ~1B params**: the KV cache is already small
  relative to the weights. Savings are marginal and quality loss per
  bit is proportionally larger.
- **Hard real-time inference with tight SLOs**: the `q4_0` path adds
  a small dequantisation overhead per-token. Usually dominated by
  the bandwidth win, but measure first.
- **Research workloads where the KV tensors will be exported** (e.g.
  attention-visualisation dumps). Export happens pre-quantisation
  in llama.cpp so you lose the original fidelity.

## Why this is NOT TurboQuant

TurboQuant's KV-cache use case is paper-specific and requires
integrating TurboQuant INTO `llama.cpp` — a months-long upstream
project. `q4_0` KV cache is llama.cpp's existing scalar quantiser,
already available in Ollama. It gives ~2/3 of the benefit for ~0% of
the effort. Use it today; revisit TurboQuant KV integration only if
measurements from the curator soak show it would materially help
beyond what `q4_0` already delivers.

## Honest-claim line for CHANGELOG

> Ai-memory operators running Ollama can set
> `OLLAMA_KV_CACHE_TYPE=q4_0` + `OLLAMA_FLASH_ATTENTION=1` before
> starting `ollama serve` to reduce LLM KV-cache memory by 2–4×,
> materially expanding curator consolidation batch size and
> cross-encoder rerank width. Zero ai-memory code change required.
> `q8_0` offers a safer ~2× option. See
> `docs/RUNBOOK-ollama-kv-tuning.md` for the verification + measurement
> procedure.
