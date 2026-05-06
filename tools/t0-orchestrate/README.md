# t0-orchestrate (v0.7.0.1 — closes #625)

Cross-platform Rust replacement for the original `scripts/t0-orchestrate.sh`
(E1, PR #621). Fans the v0.7 Discovery Gate questions out to four
frontier LLMs (Claude / GPT-5 / Gemini / Grok) and scores each
response against the canonical capabilities-v3 fragments pinned in
[`tests/calibration_t0.rs`](../../tests/calibration_t0.rs).

This is a *standalone* crate, not a Cargo workspace member — same
precedent as `tools/transcript-extractor/` (I5) and
`tools/auto-link-detector/` (G11). It is excluded from the published
`ai-memory` package by the parent `Cargo.toml`'s `include = [...]`
allowlist.

## Usage

```bash
cargo run --manifest-path tools/t0-orchestrate/Cargo.toml -- --dry-run
cargo run --manifest-path tools/t0-orchestrate/Cargo.toml -- --llm claude
cargo run --manifest-path tools/t0-orchestrate/Cargo.toml -- --dry-run --out plan.json
```

See [`docs/v0.7/T0-ORCHESTRATION.md`](../../docs/v0.7/T0-ORCHESTRATION.md)
for the full operator runbook.
