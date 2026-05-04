# Closer W12-H — small-modules round 2

Branch: `cov-90pct-w12/small-round2` (worktree: `/Users/fate/ai-memory-mcp.w12-H`).
Base: `origin/cov-90pct-w11/consolidated`.

## Tests added per module

| Module           | Tests added |
| ---------------- | ----------: |
| embeddings.rs    | 9           |
| errors.rs        | 9           |
| hnsw.rs          | 8           |
| toon.rs          | 16          |
| replication.rs   | 7           |
| models.rs        | 12          |
| metrics.rs       | 7           |
| **Total**        | **68**      |

All tests are `#[cfg(test)]`-gated and appended to existing modules. No production
code modified.

## Coverage post (Δ from W11 baseline)

| Module          | Pre (W11) | Post (W12-H) | Δ    | Target | Met |
| --------------- | --------: | -----------: | ---: | -----: | --- |
| embeddings.rs   |    89.18% |       91.70% | +2.52| 95%+   | no  |
| errors.rs       |    87.65% |      100.00% | +12.35| 95%+  | yes |
| hnsw.rs         |    92.77% |       95.52% | +2.75| 96%+   | no (close) |
| toon.rs         |    93.02% |       99.07% | +6.05| 96%+   | yes |
| replication.rs  |    97.93% |       98.80% | +0.87| 99%+   | no (close) |
| models.rs       |    90.71% |       95.64% | +4.93| 95%+   | yes |
| metrics.rs      |    92.68% |       94.09% | +1.41| 96%+   | no  |
| Codebase        |       n/a |       90.00% | n/a  | n/a    | —   |

### Why three targets fell short

- **embeddings.rs (91.70% / target 95%)**: The remaining ~38 uncovered lines are
  all inside `Embedder::new_local()` and `Embedder::embed_local()` — they require
  the real `MiniLM-L6-v2` model files (~80 MB) downloaded from HuggingFace Hub.
  Without those files (CI doesn't pre-download them), every `Tensor::new` /
  `BertModel::load` / `model.forward(...)` line is unreachable. We added a
  fallback-success test (`load_from_fallback_succeeds_when_files_present`) and
  a mismatched-dimension fuse test, plus four corner-case tests, but the model
  bodies themselves stay grey.
- **hnsw.rs (95.52% / target 96%)**: The remaining 13 uncovered lines are
  mutex-poisoned branches (`Err(poisoned) => poisoned.into_inner()`) and the
  MAX_ENTRIES=100k eviction path (which would need a 100k-entry insert in a
  unit test, well outside the time budget). The auto-rebuild branch at
  REBUILD_THRESHOLD=200 is now covered.
- **metrics.rs (94.09% / target 96%)**: The 15 uncovered lines are all the
  `?` propagations in `try_new()` — `register(...)` returns `Err` only on
  duplicate-name conflict against a fresh `Registry::new()`, which is
  unreachable in practice. Every reachable observation/inc helper now has a
  test.

## Quality gates

| Gate                                 | Status |
| ------------------------------------ | ------ |
| `cargo fmt --check`                  | passes |
| `cargo clippy --bin ai-memory --lib -- -D warnings -D clippy::pedantic` | passes |
| `cargo test --lib -- --test-threads=2` | 1217 passed, 0 failed |

## Surprises / deviations

The `embeddings.rs` shortfall is structural — getting from 91.7% → 95% requires
either pre-downloading model weights into CI or stubbing the `BertModel`
forward path, neither of which fits "test-only, no new deps". The
`load_from_fallback_succeeds_when_files_present` test does temporarily set
`HOME` (serialized via a process-local Mutex), which is the smallest viable
way to hit the success branch without inventing a `cfg(test)` injection seam
in production code. The `metrics.rs` shortfall is similar: every uncovered
line is a propagated registration failure inside `try_new()` that only fires
on a duplicate-name conflict against a fresh registry — physically unreachable
unless we mutate the registration path.

Three of seven targets exceeded their goals (`errors.rs` from 87.65% to a clean
100%, `toon.rs` to 99.07%, `models.rs` to 95.64%). Two more landed within
~0.5pp of target (`hnsw.rs` 95.52%, `replication.rs` 98.80%). The remaining
two are bounded by reachability, not test thoroughness.
