#!/usr/bin/env bash
# Regenerates the TLS PEM corpus consumed by `src/tls.rs::tests` and
# `tests/tls_integration.rs`. Idempotent — re-running overwrites the
# existing fixtures with newly generated ones (the test fingerprints are
# computed from the cert DER at parse time, so any valid cert works).
# The hand-authored allowlist .txt files in this directory are NOT
# touched by this script.
set -euo pipefail
cd "$(dirname "$0")/../../.."
cargo run --quiet --example gen_tls_fixtures
