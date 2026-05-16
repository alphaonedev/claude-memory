#!/usr/bin/env bash
# cookbook/context-offload/01-offload-large-tool-output.sh
#
# v0.7.0 QW-3 — context-offload substrate primitive (offload + deref).
#
# What this proves
#   The substrate-level offload+deref engine round-trips a 100 KB
#   synthetic tool output through the `offloaded_blobs` table without
#   loss, refuses to dereference a tampered row, and writes audit
#   rows into `signed_events` for both the offload and the deref.
#
#   v0.8.0 short-term-context-compression (Mermaid canvas + auto-
#   cadence + node_id integration) builds on this plumbing.
#
# What it does
#   1. Carves a fresh sqlite DB under .local-runs/cookbook-offload-<ts>/.
#   2. Generates a 100 KB synthetic blob (deterministic content).
#   3. Builds a tiny Rust harness that drives the substrate engine
#      directly so the recipe stays runnable without an Ollama
#      dependency or a long-running daemon (matches recipe 02's
#      "no LLM" carve-out in cookbook/recursive-learning).
#   4. Asserts: round-trip SHA256 matches AND a tampered row is
#      refused with the IntegrityFailed variant.
#
# Acceptance
#   Reproducible in <2 minutes from a clean checkout. Exits 0 on a
#   green round-trip + green tamper-refusal; exits >0 on any failure.
#
# Hard rules
#   - No /tmp / /var/tmp / /private/tmp writes (project HARD RULE).
#   - Idempotent: every run uses a fresh timestamped subdir.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
TS="$(date -u +%Y%m%dT%H%M%SZ)"
RUN_DIR="${REPO_ROOT}/.local-runs/cookbook-offload-${TS}"
mkdir -p "${RUN_DIR}"

echo "[setup] run dir: ${RUN_DIR}"

# Step 1 — synthesise a 100 KB deterministic blob. `yes | head` gives
# us a stable repeating-pattern body. zstd will collapse this hard;
# the recipe still exercises the compress->store->fetch->decompress->
# SHA256 verify path which is what we care about.
BLOB_FILE="${RUN_DIR}/synthetic-100kb.txt"
# `yes | head` would SIGPIPE under `set -o pipefail`; build the
# 100 KB body in pure bash so the recipe is portable.
python3 -c '
import sys
line = "the quick brown fox jumps over the lazy dog 0123456789\n"
out = (line * ((102400 // len(line)) + 1))[:102400]
sys.stdout.write(out)
' > "${BLOB_FILE}"
EXPECTED_BYTES=$(wc -c < "${BLOB_FILE}" | tr -d ' ')
echo "[setup] synthetic blob: ${BLOB_FILE} (${EXPECTED_BYTES} bytes)"

# Step 2 — drive the substrate engine via cargo run. The
# `--example offload_roundtrip` example below is the canonical
# in-tree harness; it lives at examples/offload_roundtrip.rs so a
# downstream reader can inspect it without pulling the whole crate.
cd "${REPO_ROOT}"
cargo run --release --example offload_roundtrip -- \
    --db "${RUN_DIR}/offload.db" \
    --input "${BLOB_FILE}" \
    --output "${RUN_DIR}/round-tripped.txt" \
    --report "${RUN_DIR}/report.json"

# Step 3 — assert round-trip integrity.
if ! cmp -s "${BLOB_FILE}" "${RUN_DIR}/round-tripped.txt"; then
    echo "[FAIL] round-trip mismatch" >&2
    exit 1
fi
echo "[ok] round-trip integrity confirmed (byte-equal)"

# Step 4 — tamper test. The example also reports a "tamper_refused"
# flag in its JSON report; verify it.
TAMPER_OK=$(grep -c '"tamper_refused": *true' "${RUN_DIR}/report.json" || true)
if [[ "${TAMPER_OK}" -ne 1 ]]; then
    echo "[FAIL] tamper-refusal not reported by harness" >&2
    cat "${RUN_DIR}/report.json" >&2
    exit 1
fi
echo "[ok] tamper-refusal confirmed (deref rejected mutated blob)"

echo ""
echo "=========================================================="
echo "VERDICT: context-offload substrate primitive — GREEN"
echo "  DB:       ${RUN_DIR}/offload.db"
echo "  Report:   ${RUN_DIR}/report.json"
echo "=========================================================="
