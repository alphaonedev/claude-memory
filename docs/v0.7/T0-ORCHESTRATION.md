# T0 cross-LLM orchestration (v0.7.0 task E1)

> **Status:** SHIPPING with v0.7.0 — E1
> **Date:** 2026-05-06
> **Binary:** [`tools/t0-orchestrate/`](../../tools/t0-orchestrate/) → `ai-memory-t0`
> **CI counterpart:** [`tests/calibration_t0.rs`](../../tests/calibration_t0.rs)
>
> **Cross-platform port (#625):** the original `scripts/t0-orchestrate.sh`
> was reimplemented as a standalone Rust binary so the dry-run harness
> check runs on every CI platform (Windows runners ship no bash).

The Discovery Gate **T0 calibration cells** in
`tests/calibration_t0.rs` pin the canonical capabilities-v3 phrasings
(A1 `summary` + A2 `to_describe_to_user`) against fixture inputs
running on the local substrate. Those tests are deterministic and run
in CI on every PR.

E1 wraps the same Discovery Gate questions into an **out-of-band
orchestration harness** that exercises them against four live frontier
LLMs. The goal is to validate that the canonical phrasings (the A1, A2
strings + the per-tool short descriptions from C2) are correctly
understood and reproduced by every major frontier reasoning-class
model — not just by the local fixture-driven test cells.

This is a script, not a runtime change. No tools, schemas, webhooks,
or hook events are added by E1.

---

## LLMs covered

| Provider  | Model              | Env var               |
|-----------|--------------------|-----------------------|
| Anthropic | Claude Sonnet 4.6  | `ANTHROPIC_API_KEY`   |
| OpenAI    | GPT-5              | `OPENAI_API_KEY`      |
| Google    | Gemini 3           | `GOOGLE_API_KEY`      |
| xAI       | Grok 4.3           | `XAI_API_KEY`         |

The four-vendor coverage matches the v0.6.5/v0.7.0 NHI Discovery Gate
observation matrix. Adding a fifth vendor is a one-line append to the
`LLMS` constant in
[`tools/t0-orchestrate/src/main.rs`](../../tools/t0-orchestrate/src/main.rs).

---

## Setup

```bash
export ANTHROPIC_API_KEY=sk-ant-...
export OPENAI_API_KEY=sk-...
export GOOGLE_API_KEY=...
export XAI_API_KEY=xai-...

# Build the local substrate so the orchestrator can pull live
# capabilities-v3 payloads to use as system context for each LLM call.
cargo build --release

# Build the orchestrator binary itself.
cargo build --manifest-path tools/t0-orchestrate/Cargo.toml --release
```

The orchestrator binary is pure Rust; live mode uses the in-process
`reqwest` HTTP client. No external `curl` or `jq` dependency.

---

## Running

```bash
# Live run — all four LLMs, six Discovery Gate questions each.
cargo run --manifest-path tools/t0-orchestrate/Cargo.toml --release --

# Restrict to one provider (debugging, partial outages, key rotation).
cargo run --manifest-path tools/t0-orchestrate/Cargo.toml --release -- --llm claude

# Dry-run — no API calls, prints the plan + result file paths.
# Used by tests/e1_orchestration_dry_run.rs to validate harness
# structure without spending API tokens or requiring keys.
cargo run --manifest-path tools/t0-orchestrate/Cargo.toml --release -- --dry-run

# Once built, you can invoke the binary directly as well:
tools/t0-orchestrate/target/release/ai-memory-t0 --dry-run
```

Skipped LLMs (env var unset) print `skip <llm> — <ENV> unset` and
continue. A single missing key never aborts the run.

---

## Output layout

```
results/t0/
  claude-20260506T120000Z.json    # one per LLM per run
  gpt5-20260506T120000Z.json
  gemini-20260506T120000Z.json
  grok-20260506T120000Z.json
  summary-20260506T120000Z.md     # cross-LLM scorecard
```

Each `<llm>-<ts>.json` file is:

```json
{
  "llm": "claude",
  "model": "claude-sonnet-4-6",
  "timestamp": "20260506T120000Z",
  "results": [
    { "qid": "T0-A2-CORE", "profile": "core",
      "question": "What tools do you have available right now? …",
      "response": "I can directly use 7 memory tools right now …",
      "passed": 3, "total": 3 }
  ]
}
```

The `summary-<ts>.md` file is a markdown table — one row per
`(llm, qid)` pair — that you can paste directly into a release-notes
appendix or a Discovery Gate observation cell.

---

## How to interpret results

Each Discovery Gate question carries one or more **expected fragments**
(canonical substrings the response should contain). The `passed/total`
numbers in the summary report how many fragments matched.

| `passed/total` | Meaning |
|----------------|---------|
| `total/total`  | LLM reproduced the canonical phrasing — pass. |
| `0/total`      | LLM ignored or misunderstood the canonical strings — investigate the system-context payload first; if the payload looks right, the LLM may need a clearer per-tool short description (open a C2 ticket). |
| Mixed          | LLM paraphrased; check whether the missing fragment is load-bearing (e.g., a recovery path name) or cosmetic (e.g., the tone constraint). |

The acceptance threshold per the v0.7.0 ship gate is **≥95% across all
four LLMs combined** (E2 measures this post-ship).

---

## When to re-run

Re-run the orchestrator after any change that could shift LLM
convergence on the canonical phrasings:

1. The phrasings themselves change in `docs/v0.7/canonical-phrasings.md`
   or in `src/mcp.rs::build_capabilities_{summary,describe_to_user}`.
2. A new tool family is added (loaded/unloaded counts shift — the T0
   tests update their expected counts; orchestration should
   re-confirm LLM convergence on the new numbers).
3. A frontier model rev lands on one of the four covered providers
   (Claude N+1, GPT N+1, Gemini N+1, Grok N+1). Add the new model id
   to the `LLMS` constant in
   [`tools/t0-orchestrate/src/main.rs`](../../tools/t0-orchestrate/src/main.rs)
   and re-run.
4. The per-tool short descriptions ship from C2 (orchestration becomes
   the load-bearing check that LLMs render those descriptions sanely
   to end users).

The CI-side `tests/calibration_t0.rs` cells catch substrate drift
**before** you re-run this orchestrator. If those tests are red, fix
them first — a live cross-LLM run against a drifted substrate burns
API budget on a question you already know the answer to.

---

## Refs

- [v0.7.0 epic](./V0.7-EPIC.md) — track E
- [`docs/v0.7/canonical-phrasings.md`](./canonical-phrasings.md) — the
  A1/A2 strings the orchestrator validates against
- [`tests/calibration_t0.rs`](../../tests/calibration_t0.rs) — the
  CI-side T0 cells the orchestrator wraps
- [`tests/e1_orchestration_dry_run.rs`](../../tests/e1_orchestration_dry_run.rs)
  — minimal Rust test that runs `--dry-run` and asserts harness shape
