# post-ship-converge (v0.7.0.1 — closes #625)

Cross-platform Rust implementation of the v0.7.0 task E2 post-ship
distribution-channel convergence verifier. Probes the cargo / brew /
GitHub-release channels for a freshly-cut release and asserts they
all converge on the same version string. The originally-planned bash
script (PR #622, never merged) was superseded by this crate.

This is a *standalone* crate, not a Cargo workspace member — same
precedent as `tools/transcript-extractor/` (I5),
`tools/auto-link-detector/` (G11), and the sibling
`tools/t0-orchestrate/` (E1). It is excluded from the published
`ai-memory` package by the parent `Cargo.toml`'s `include = [...]`
allowlist.

## Usage

```bash
cargo run --manifest-path tools/post-ship-converge/Cargo.toml -- --dry-run --version 0.7.0
cargo run --manifest-path tools/post-ship-converge/Cargo.toml -- --version 0.7.0
cargo run --manifest-path tools/post-ship-converge/Cargo.toml -- --version 0.7.0 --method brew
```

See [`docs/v0.7/POST-SHIP-CONVERGENCE.md`](../../docs/v0.7/POST-SHIP-CONVERGENCE.md)
for the full operator runbook.
