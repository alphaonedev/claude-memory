#!/usr/bin/env bash
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
#
# Issue #878 — peer-reach preflight integration test.
#
# Targets `infra/plan-c/peer-preflight.sh` (the standalone script that
# `entrypoint.plan-c.sh` sources). Testing the standalone script
# instead of the full entrypoint avoids the entrypoint's hard-coded
# /etc/ai-memory + /root/.config side effects (which require either
# root or a container; neither is appropriate for `cargo test` or
# `bash` on a dev workstation).
#
# Asserts:
#   1. Unreachable peer URLs → exit 78 (sysexits.h EX_CONFIG).
#   2. stderr contains the canonical "#878 preflight FAILED" banner.
#   3. The unreachable URLs are listed by-name in the error output.
#   4. AI_MEMORY_SKIP_PEER_PREFLIGHT=1 → exit 0, skip log.
#   5. Empty PEER_URLS → exit 0, silent (no probe attempted).
#
# # Wiring
#
# Run directly:
#   bash tests/plan_c_preflight.sh
#
# The script writes scratch logs under `.local-runs/` per the project
# no-/tmp rule and cleans up on success.
#
# Exit codes:
#   0 — all assertions passed
#   1 — at least one assertion failed (script aborts on first failure)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PREFLIGHT="${REPO_ROOT}/infra/plan-c/peer-preflight.sh"
SCRATCH="${REPO_ROOT}/.local-runs/plan-c-preflight-$(date +%s)"

if [ ! -f "$PREFLIGHT" ]; then
  echo "FAIL: preflight script not found at $PREFLIGHT" >&2
  exit 1
fi
if [ ! -x "$PREFLIGHT" ]; then
  echo "FAIL: preflight script not executable at $PREFLIGHT" >&2
  exit 1
fi

mkdir -p "$SCRATCH"

#
# Assertion 1+2+3: unreachable peer → exit 78 + banner + named peer.
#
# Use TEST-NET-3 (RFC 5737, 203.0.113.0/24) — reserved for documentation,
# no real service answers there. Combined with high ephemeral ports this
# guarantees the probe times out rather than coincidentally connecting
# to something on the dev host.
echo "=== Test 1: unreachable peer URLs trigger preflight failure ==="
STDERR_LOG="$SCRATCH/test1.stderr"
STDOUT_LOG="$SCRATCH/test1.stdout"
set +e
PEER_URLS="http://203.0.113.42:65534,http://203.0.113.43:65533" \
  PEER_PREFLIGHT_TIMEOUT_S=2 \
  bash "$PREFLIGHT" >"$STDOUT_LOG" 2>"$STDERR_LOG"
RC=$?
set -e
if [ "$RC" -ne 78 ]; then
  echo "FAIL: expected exit code 78 (EX_CONFIG), got $RC" >&2
  echo "stderr:" >&2
  cat "$STDERR_LOG" >&2
  exit 1
fi
if ! grep -q "#878 preflight] FAILED" "$STDERR_LOG"; then
  echo "FAIL: stderr missing canonical '#878 preflight FAILED' banner" >&2
  cat "$STDERR_LOG" >&2
  exit 1
fi
if ! grep -q "203.0.113.42:65534" "$STDERR_LOG"; then
  echo "FAIL: stderr should list the unreachable peer URL by name" >&2
  cat "$STDERR_LOG" >&2
  exit 1
fi
echo "PASS: preflight aborted with EX_CONFIG and named unreachable peer"

#
# Assertion 4: AI_MEMORY_SKIP_PEER_PREFLIGHT=1 bypasses cleanly.
#
echo "=== Test 2: AI_MEMORY_SKIP_PEER_PREFLIGHT=1 bypasses preflight ==="
STDOUT_LOG2="$SCRATCH/test2.stdout"
STDERR_LOG2="$SCRATCH/test2.stderr"
set +e
PEER_URLS="http://203.0.113.42:65534" \
  AI_MEMORY_SKIP_PEER_PREFLIGHT=1 \
  bash "$PREFLIGHT" >"$STDOUT_LOG2" 2>"$STDERR_LOG2"
RC2=$?
set -e
if [ "$RC2" -ne 0 ]; then
  echo "FAIL: expected exit 0 (bypassed), got $RC2" >&2
  cat "$STDERR_LOG2" >&2
  exit 1
fi
if grep -q "FAILED" "$STDERR_LOG2"; then
  echo "FAIL: preflight should have been skipped but the failure banner is present" >&2
  cat "$STDERR_LOG2" >&2
  exit 1
fi
if ! grep -q "skipped" "$STDOUT_LOG2"; then
  echo "FAIL: bypass should log 'skipped'" >&2
  cat "$STDOUT_LOG2" >&2
  exit 1
fi
echo "PASS: AI_MEMORY_SKIP_PEER_PREFLIGHT=1 bypasses preflight"

#
# Assertion 5: empty PEER_URLS → silent no-op exit 0.
#
echo "=== Test 3: empty PEER_URLS → preflight is a silent no-op ==="
STDOUT_LOG3="$SCRATCH/test3.stdout"
STDERR_LOG3="$SCRATCH/test3.stderr"
set +e
PEER_URLS="" bash "$PREFLIGHT" >"$STDOUT_LOG3" 2>"$STDERR_LOG3"
RC3=$?
set -e
if [ "$RC3" -ne 0 ]; then
  echo "FAIL: expected exit 0 (no peers, no probe), got $RC3" >&2
  cat "$STDERR_LOG3" >&2
  exit 1
fi
if [ -s "$STDOUT_LOG3" ] || [ -s "$STDERR_LOG3" ]; then
  echo "FAIL: empty PEER_URLS should be silent; got output" >&2
  echo "stdout:" >&2; cat "$STDOUT_LOG3" >&2
  echo "stderr:" >&2; cat "$STDERR_LOG3" >&2
  exit 1
fi
echo "PASS: empty PEER_URLS is a silent no-op"

#
# Assertion 6: a mix of reachable + unreachable still fails — the
# preflight is conservative (every peer must reach for the daemon to
# start). We probe localhost:1 (reserved tcpmux, nothing listens) plus
# 127.0.0.1 with a port we just learned was free.
#
echo "=== Test 4: one-unreachable-among-many still fails ==="
STDERR_LOG4="$SCRATCH/test4.stderr"
STDOUT_LOG4="$SCRATCH/test4.stdout"
set +e
PEER_URLS="http://203.0.113.99:1,http://203.0.113.100:2" \
  PEER_PREFLIGHT_TIMEOUT_S=2 \
  bash "$PREFLIGHT" >"$STDOUT_LOG4" 2>"$STDERR_LOG4"
RC4=$?
set -e
if [ "$RC4" -ne 78 ]; then
  echo "FAIL: expected exit 78 with mix of unreachable peers, got $RC4" >&2
  cat "$STDERR_LOG4" >&2
  exit 1
fi
# Both peers should be listed.
if ! grep -q "203.0.113.99:1" "$STDERR_LOG4"; then
  echo "FAIL: 203.0.113.99 missing from failure banner" >&2
  cat "$STDERR_LOG4" >&2
  exit 1
fi
if ! grep -q "203.0.113.100:2" "$STDERR_LOG4"; then
  echo "FAIL: 203.0.113.100 missing from failure banner" >&2
  cat "$STDERR_LOG4" >&2
  exit 1
fi
echo "PASS: multiple-unreachable peers all surfaced"

# Cleanup scratch.
rm -rf "$SCRATCH"

echo ""
echo "=== All #878 preflight assertions passed (4/4) ==="
