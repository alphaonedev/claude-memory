# Post-ship distribution-channel convergence (v0.7.0 task E2)

> **Status:** SHIPPING with v0.7.0.1 — E2 cross-platform Rust binary
> (closes #625; supersedes the never-merged bash variant in PR #622)
> **Date:** 2026-05-06
> **Tool:** [`tools/post-ship-converge/`](../../tools/post-ship-converge/)
> (binary `post-ship-converge`)

After every `release/vX.Y.Z` tag-cut, the same version string has to
land on three independent distribution channels:

1. **`crates.io`** — `cargo install ai-memory` pulls the new package.
2. **Homebrew tap** (`alphaonedev/homebrew-tap/Formula/ai-memory.rb`)
   — `brew install ai-memory` pulls the new bottle.
3. **GitHub release tarball** — `binstall` and the manual download
   page surface the new prebuilt binary archive for each target
   triple.

If any of the three lags (cargo accepted but brew formula PR not
merged, binary release artifacts missing for one platform, etc.)
the post-ship comms (release notes, blog post, social) has to
hold until convergence — otherwise a fraction of users land on a
stale version with no obvious indication of which channel
diverged.

`post-ship-converge` polls the metadata endpoint of each channel,
extracts the advertised version, and reports whether all three
agree with the expected version supplied via `--version`.

---

## Setup

Pure Rust — no runtime dependency on `bash`, `curl`, or `jq`.
Build it (or run it directly with `cargo run`):

```bash
cargo build --manifest-path tools/post-ship-converge/Cargo.toml --release
```

---

## Running

```bash
# Live run — probe all three channels for v0.7.0.
cargo run --manifest-path tools/post-ship-converge/Cargo.toml --release \
  -- --version 0.7.0

# Restrict to one channel (poll-while-waiting after a partial publish).
cargo run --manifest-path tools/post-ship-converge/Cargo.toml --release \
  -- --version 0.7.0 --method brew

# Dry-run — no network, prints the plan + result file path.
# Used by tests/e2_post_ship_dry_run.rs to validate harness shape
# on every PR without hitting the live registries.
cargo run --manifest-path tools/post-ship-converge/Cargo.toml \
  -- --dry-run --version 0.7.0

# After install (cargo install --path tools/post-ship-converge):
post-ship-converge --version 0.7.0
post-ship-converge --version 0.7.0 --method cargo
post-ship-converge --dry-run --version 0.7.0 --out plan.json
```

The live run prints one line per channel and exits non-zero if any
channel diverges from `--version`. CI for the post-ship workflow
treats that exit code as the convergence gate — the workflow loops
until either all channels converge or a timeout expires.

---

## Channels probed

| Method  | Channel                  | Metadata endpoint                                                                                                  |
|---------|--------------------------|--------------------------------------------------------------------------------------------------------------------|
| cargo   | crates.io                | `https://crates.io/api/v1/crates/ai-memory` — `crate.max_stable_version`                                           |
| brew    | Homebrew tap             | `https://raw.githubusercontent.com/alphaonedev/homebrew-tap/main/Formula/ai-memory.rb` — first `version "X.Y.Z"`   |
| binary  | GitHub release tarball   | `https://api.github.com/repos/alphaonedev/ai-memory-mcp/releases/tags/v<VER>` — `tag_name` (with leading `v` trim) |

---

## Output layout

```
results/post-ship/
  converge-20260506T120000Z.json     # one per run
```

Each report is:

```json
{
  "mode": "live",
  "timestamp": "20260506T120000Z",
  "expected_version": "0.7.0",
  "all_converged": true,
  "observed": {
    "cargo":  "0.7.0",
    "brew":   "0.7.0",
    "binary": "0.7.0"
  }
}
```

In `--dry-run --out plan.json` mode the binary writes the
orchestration plan instead — three entries (one per channel) with
`method`, `channel`, `metadata_url`, `expected_version` fields.

---

## When to re-run

- After every `release/vX.Y.Z` tag is cut, run `post-ship-converge
  --version X.Y.Z` until it reports `all channels converged`.
- After any change to the publish pipeline that could affect when
  one channel lands relative to another (formula bump PR, binary
  release workflow, cargo publish step).

---

## Refs

- [v0.7.0 epic](./V0.7-EPIC.md) — track E
- [`tests/e2_post_ship_dry_run.rs`](../../tests/e2_post_ship_dry_run.rs)
  — minimal Rust test that runs `--dry-run` and asserts harness shape
- Issue [#625](https://github.com/alphaonedev/ai-memory-mcp/issues/625)
  — the v0.7.0.1 cross-platform port, original brief
