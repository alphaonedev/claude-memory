# GPU Integration Roadmap — multi-vendor, v0.7.0-frozen reference

> **Status:** v0.7.0-frozen REFERENCE. Tracker, not a release commitment.
> This document captures the strategic plan as it stood at the
> `local/install-815-816` branch HEAD on 2026-05-18. It is the in-repo
> permalink for [issue #652](https://github.com/alphaonedev/ai-memory-mcp/issues/652)
> ("TABLED — GPU integration roadmap") and consolidates the multi-vendor
> hardware matrix, Enterprise topology classes, phased delivery plan, and
> benchmark methodology. The original RFC-style discussion lives on the
> A2A campaign repo at
> [`docs/GPU-INTEGRATION-ROADMAP.md`](https://github.com/alphaonedev/ai-memory-a2a-v0.7.0/blob/main/docs/GPU-INTEGRATION-ROADMAP.md).
>
> **Operator disposition (2026-05-18):** Close #652 as documented under
> this permalink. Re-open or supersede when DigitalOcean GPU access lands
> AND v0.7.1 Plan C (Mac M4 + Linux postgres+AGE) ships. No engineering
> effort allocated against this roadmap during v0.7.0; the document is a
> reference for the next operator activation.
>
> **Companion architecture RFC:**
> [#651](https://github.com/alphaonedev/ai-memory-mcp/issues/651)
> (pluggable inference backend trait). This roadmap is the
> *operational + multi-vendor benchmarking* wrapper around #651's
> daemon-level architecture.

---

## TL;DR

- **Ollama is the right unifier for v0.7.0 / v0.7.1.** It runs on every
  platform, ships growing native acceleration (auto-MLX on Apple
  Silicon Metal-4 in 0.23+, llama.cpp.cuda on Linux NVIDIA,
  llama.cpp.rocm on AMD). Operators love its ergonomics. Single-node
  and multi-node Enterprise architectures are well served by Ollama
  HTTP today.
- **For swarm / hive Enterprise topologies** (dozens to thousands of
  ai-memory daemons sharing inference compute), Ollama is insufficient.
  The org needs vLLM PagedAttention for multi-tenant throughput,
  TensorRT-LLM for NVIDIA optimal paths, in-process inference
  (`candle`, `mistralrs`, `mlx-rs`) for daemons with sub-millisecond
  budgets, and GPU memory budgeting / queue isolation so one swarm
  member can't starve others. This is **v0.9.0+** scope.
- **For ULTRA-1 ultra-autonomous tier** (50 ms p95 recall budget with
  LLM in hot-path), even MLX-on-M4 numbers don't fit. MTP + speculative
  decoding + a distilled <1B specialty model are the unlocks. **v1.0.0**
  scope, dependent on the distilled-model layer at #654.
- **Multi-vendor parity matters for Enterprise.** Customers will not be
  uniform. Apple Silicon, NVIDIA datacenter, AMD MI300X-class, and
  Windows-on-NVIDIA all need first-class paths.
- **Strategy: stage over v0.7.1 → v0.8 → v0.9 → v1.0 milestones.**
  Ollama remains the always-available default; no big-bang rewrite.

---

## Enterprise AI Agent architectures planned for

| Topology | Description | Inference shape | Required milestone |
|---|---|---|---|
| **Single** | one ai-memory daemon, one user | local Ollama | v0.7.0 (shipped) |
| **Multi-node** | several daemons, federation, shared workspace | Ollama HTTP per daemon | v0.7.0 (shipped) |
| **Swarm** | dozens of daemons, shared inference cluster | vLLM / TensorRT-LLM cluster, per-tenant GPU budget | **v0.9.0** (pluggable backend + remote vLLM/TensorRT-LLM + multi-tenancy) |
| **Hive** | hundreds-to-thousands daemons, distributed mesh, possibly cross-region | inference as managed service; daemon talks to nearest pool | **v0.9.x+** (geo-aware inference routing, mesh telemetry) |
| **Ultra-Autonomous (ULTRA-1)** | LLM in recall hot-path on every read; 50 ms p95 budget | MTP + speculative decoding + distilled model | **v1.0.0** (MTP + speculative + distilled layer) |

---

## Hardware backend matrix (vendor coverage)

### Apple Silicon (Mac)

- **M3 / M4 / M5:** Ollama 0.23+ MLX (Metal 4) — verified live on the
  operator's M4 Mac Mini.
- **M1 / M2:** Ollama 0.23+ MLX (Metal 3).
- **Direct `mlx-rs` 0.25 in-process:** ~10-20% faster than Ollama HTTP
  for cold-thinking; optional v0.8.1 cargo feature.

### Linux NVIDIA

- **Consumer (RTX 3xxx / 4xxx):** Ollama or LM Studio + TensorRT-LLM /
  Jan.ai.
- **Pro (L4 / L40 / L40S / RTX Ada):** vLLM (PagedAttention).
- **Datacenter (A100 / H100 / H200):** **vLLM is the sole choice at
  scale** — TensorRT-LLM is a viable alternative for NVIDIA-optimised
  paths but loses portability.

### Linux AMD

- **Consumer (RX 7900 XT / XTX):** Ollama (llama.cpp.rocm) or LM Studio
  + Vulkan.
- **Datacenter (MI250X / MI300X):** **vLLM-rocm or TensorRT-LLM-rocm
  (when ported).** ROCm closing the gap with CUDA but not at parity yet.

### Windows

- **NVIDIA RTX 30 / 40:** LM Studio + TensorRT-LLM, Jan.ai, Ollama,
  NVIDIA ChatRTX.
- **AMD RX 7000:** LM Studio + Vulkan, Ollama (Windows native).

### Cloud GPU — DigitalOcean (currently blocked)

DigitalOcean account does **NOT** currently have GPU droplet access.
Every GPU SKU returns `422 Size is not available in this region` across
every region. When access lands, test BOTH NVIDIA AND AMD SKUs for
multi-vendor parity:

| SKU | Vendor | VRAM | $/hr | Track |
|---|---|---|---|---|
| `gpu-4000adax1-20gb` | NVIDIA RTX 4000 Ada | 20 GB | $0.76 | Track Q-Nvidia (4× quad) |
| `gpu-l40sx1-48gb` | NVIDIA L40S | 48 GB | $1.57 | Track Q-Nvidia postgres host |
| `gpu-h100x1-80gb` | NVIDIA H100 | 80 GB | $3.39 | Headline NVIDIA |
| **`gpu-mi300x1-192gb`** | **AMD MI300X** | **192 GB** | **$1.99** | **Track Q-AMD** |
| `gpu-h100x8-640gb` | 8× NVIDIA H100 | 640 GB | $23.92 | Hyperscale parity |
| `gpu-mi300x8-1536gb` | 8× AMD MI300X | 1.5 TB | $15.92 | Hyperscale AMD |

**Triggers to retest DO access:** support ticket approval, account
spend prerequisites met, region availability changes.

**Alternative providers (if DO denies indefinitely):** RunPod
($0.34/hr RTX 4000 Ada, instant signup), Lambda Labs, Vast.ai (spot
$0.18/hr), AWS g5/g6, GCP A2, Azure ND.

---

## Phased delivery plan (when activated)

| Version | Scope | LOE |
|---|---|---|
| v0.7.0 (shipped) | Ollama default; auto-MLX on Mac M3/M4 | done |
| **v0.7.1** | Plan C cert (LAN: Mac M4 + Linux postgres+AGE; autonomous tier live) | 2-3 weeks |
| v0.7.2 | Patch deferrals (G5 final, NHI-D-quota, NHI-D-PRIO-CLAMP, NHI-D-search) | 1-2 weeks |
| **v0.8.0** | Pluggable inference trait + Ollama default + 2 in-process backends (`candle`, `llama-cpp-rs`) | **6-8 weeks** |
| v0.8.1 | `mlx-rs` Apple-only optimisation | +1-2 weeks |
| **v0.9.0** | Enterprise polish (mTLS / multi-tenancy / GPU mem budgeting / SLO + circuit breaker / signed weights / audit) | **5-7 weeks** |
| v0.9.1 | Remote backends (vLLM, TensorRT-LLM HTTP clients) | 2-3 weeks |
| v0.9.2 | DO GPU multi-vendor cert (Track Q-Nvidia + Q-AMD) | 1-2 weeks |
| **v1.0.0** | MTP + speculative decoding + ULTRA-1 hot-path | **5-7 weeks** |
| v1.0.x | Hive-scale telemetry + geo-aware inference routing | 3-4 weeks |

**Total v0.7 → v1.0 inference modernisation:** ~25-32 focused weeks
(~6-8 months focused, ~12-15 calendar months alongside other v0.8/v0.9
work).

---

## Performance requirements per tier

| Tier | Operation | p95 budget | Current Ollama-MLX | Gap |
|---|---|---|---|---|
| Semantic | recall (vector + lexical) | 100 ms | ~50 ms | OK |
| Autonomous | auto_tag (single LLM) | 1500 ms | ~800-1500 ms | OK (borderline) |
| Autonomous | consolidate (multi-LLM) | 5 s | ~3-5 s | OK (borderline) |
| Autonomous | expand_query (LLM rewrite) | 1500 ms | ~600-1200 ms | OK |
| **ULTRA-1** | recall (hot-path LLM) | **50 ms** | ~100-500 ms | **2-10× over** |

ULTRA-1 is the architectural feasibility test. Current MLX-on-M4
numbers don't fit; MTP halves the gap; distilled models close it.

---

## Benchmark plan (when GPU access lands)

```bash
cargo bench --bench inference -- \
    --models gemma4:e4b,gemma4:e2b,gemma3:4b \
    --backends ollama,candle,mistralrs,llama_cpp_rs,mlx_rs \
    --workloads auto_tag,consolidate,expand_query,detect_contradiction,smart_load \
    --metrics p50,p99,tok_per_sec,time_to_first_token,gpu_mem_peak
```

**Required benchmark runs (in order of priority):**

1. **bench-mac-m4** — local M4 Mac Mini (verify Ollama-MLX baseline).
2. **bench-do-nvidia** — `gpu-4000adax1-20gb` × 1 (single-card NVIDIA).
3. **bench-do-amd** — `gpu-mi300x1-192gb` × 1 (single-card AMD).
4. **bench-do-h100** — `gpu-h100x1-80gb` (datacenter NVIDIA).
5. **bench-vllm-cluster** — 8× h100 vLLM PagedAttention (hyperscale).
6. **bench-mtp-mlx** — gemma4:e4b + MTP
   (`OLLAMA_MLX_MTP_MAX_DRAFT_TOKENS=4`).
7. **bench-mtp-vllm** — H100 + speculative decode.
8. **bench-distilled-1b** — distilled-1b on candle/mistralrs (ULTRA-1
   hot-path).

**Output:** per-backend × per-workload latency + throughput matrix,
published alongside cert results.

---

## Risk register (top items)

| # | Risk | Probability | Impact | Mitigation |
|---|---|---|---|---|
| 1 | DO GPU access never lands | medium | medium | RunPod / Vast.ai alternatives; community-supplied bench results from non-DO operators |
| 2 | vLLM / TensorRT-LLM operational complexity blocks Enterprise adoption | medium | high | Stage in v0.9.0 with first-class operator-friendly defaults; ship reference docker-compose stacks |
| 3 | AMD ROCm parity lags CUDA at v0.9.2 cert time | medium | medium | Document NVIDIA-first parity matrix with AMD as opt-in; do not block release on AMD parity |
| 4 | ULTRA-1 50 ms p95 unreachable without #654 (distilled model) | high | low (v1.0 scope) | Gate v1.0.0 release on #654 unblock; tier remains opt-in |
| 5 | Multi-tenancy / GPU budgeting requires substantive new substrate APIs | medium | medium | Land v0.8.0 pluggable trait FIRST; v0.9.0 layers tenancy on top of trait |
| 6 | Operator can't reproduce bench results without same hardware | high | low | Publish full benchmark scripts + dockerised harnesses; reproduce-on-LAN test plan |

---

## Disposition + traceability

- **Issue #652** (this roadmap, TABLED): closes 2026-05-18 with link to
  this document as the in-repo permalink. Disposition: WONTFIX-NOW;
  re-opens when operator activates the roadmap (likely post-v0.7.1).
- **Issue #651** (RFC pluggable inference backend trait): deferred to
  v0.8.0 per the phased plan above. Operator-approval-required at
  v0.8.0 scope-cut.
- **Issue #654** (distilled hot-path model + attested model-weight
  supply chain): TABLED → strategic IP swap per memory `338278f5-1d42-
  4e95-88c5-84d5fc3b1f53` (2026-05-17). Re-evaluated as a v1.0.0
  prerequisite when the v0.9.x cert cycle completes.
- **Issue #805** (Gap #4 from NHI viewpoint RFC #802): autonomous-tier
  latency tax depends on #654 unblock. Defer to v0.8.0 per
  cross-reference.

## Provenance

Document captured by Claude Opus 4.7 (1M context) on 2026-05-18 as part
of Initiative #9 quick-wins burst. Strategy + matrix content carried
forward verbatim from issue #652 body and the A2A campaign repo
roadmap document. No new engineering work allocated; the document is
the v0.7.0-frozen reference for the next operator activation.
