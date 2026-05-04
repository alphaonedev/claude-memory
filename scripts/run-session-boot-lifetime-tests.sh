#!/usr/bin/env bash
# Runs the full session-boot lifetime test suite locally.
#
# This is the local mirror of `.github/workflows/session-boot-lifetime.yml`.
# Use it to dry-run the suite before pushing, or to debug a CI failure on
# the same machine where you'd reproduce it.
#
# Exit codes:
#   0 — all green
#   1 — a contract or lifecycle test failed
#   2 — a platform-gated test was skipped (informational; not a hard fail)
#   3 — fatal harness error (e.g. cargo not on PATH)
#
# Issue #487 PR-3 (session-boot lifetime suite).

set -euo pipefail

# Find the repo root regardless of where this script is invoked from.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

if ! command -v cargo >/dev/null 2>&1; then
    echo "fatal: cargo not on PATH" >&2
    exit 3
fi

# Quiet user config so embedder cold-load can't gate test startup.
export AI_MEMORY_NO_CONFIG=1

echo "==> 1/3  cargo test --test boot_primitive_contract"
cargo test --test boot_primitive_contract

echo "==> 2/3  cargo test --test recipe_contract"
cargo test --test recipe_contract

echo "==> 3/3  cargo test --test boot_lifecycle"
cargo test --test boot_lifecycle

# Optional gated live-agent smoke. Off by default — running it requires
# the `claude` binary on PATH plus a real ANTHROPIC_API_KEY. When skipped,
# we emit a notice line but still exit 0.
if [[ "${E2E_AGENT_TESTS:-}" == "1" ]]; then
    echo "==> opt: cargo test --test live_agent_smoke --features e2e"
    cargo test --test live_agent_smoke --features e2e
else
    echo "skip: E2E_AGENT_TESTS unset; skipping live-agent smoke (informational)"
fi

echo
echo "session-boot lifetime suite: all green"
