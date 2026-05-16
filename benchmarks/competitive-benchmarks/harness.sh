#!/usr/bin/env bash
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
#
# benchmarks/competitive-benchmarks/harness.sh
#
# Driver script for the v0.7.0 launch-day competitive benchmark run.
#
# Status: SCAFFOLDING ONLY. The competitor invocation sections are
# placeholders. The full run lands at v0.7.0 launch — see README.md
# §"Status — what ships in v0.7.0" for the rationale and §"Launch-day
# plan" for the resolved environment.
#
# Closes roadmap gap F1-scaffolding (issue #692).

set -euo pipefail

readonly CORPUS_DIR="${COMPETITIVE_CORPUS:-../longmemeval/corpus-240}"
readonly OUT_DIR="${COMPETITIVE_OUT:-./results-$(date -u +%Y%m%dT%H%M%SZ)}"
readonly K=5

mkdir -p "$OUT_DIR"

# ----- HARD RULE: refuse to write into a tmpfs path -----
case "$OUT_DIR" in
    /tmp/*|/var/tmp/*|/private/tmp/*)
        echo "ERROR: output dir $OUT_DIR is under a tmpfs path. Set COMPETITIVE_OUT." >&2
        exit 2
        ;;
esac

# Stack-under-test rows. Each row's runner is a function defined below
# (the function returns exit 100 to indicate "not yet implemented" —
# the runner remains scaffolded until launch-day install pins land).

declare -a STACKS=(ai-memory agentmemory mem0 letta)

run_ai_memory() {
    # TODO(launch-day v0.7.0): wire the existing longmemeval/harness.py
    # harness with --output-csv pointing at "$OUT_DIR/ai-memory.csv".
    # The shipped harness already emits R@5/R@10/MRR; this runner only
    # needs to forward the 240-observation slice and capture wall-clock.
    echo "[ai-memory]   runner not yet wired (launch-day v0.7.0)"
    return 100
}

run_agentmemory() {
    # TODO(launch-day v0.7.0): create a fresh venv, `pip install
    # agentmemory==<pinned>` (pin TBD at launch), import the API,
    # ingest the 240 observations, run the question set, emit
    # "$OUT_DIR/agentmemory.csv" in the same schema as ai-memory.
    echo "[agentmemory] runner not yet wired (launch-day v0.7.0)"
    return 100
}

run_mem0() {
    # TODO(launch-day v0.7.0): create a fresh venv, `pip install
    # mem0ai==<pinned>`, configure the vector backend to chroma
    # (default), ingest, query, emit "$OUT_DIR/mem0.csv".
    echo "[mem0]        runner not yet wired (launch-day v0.7.0)"
    return 100
}

run_letta() {
    # TODO(launch-day v0.7.0): create a fresh venv, `pip install
    # letta==<pinned>`, start the Letta server in single-tenant mode
    # on an ephemeral port, ingest the 240 observations into one
    # agent, run the question set against that agent's memory, emit
    # "$OUT_DIR/letta.csv".
    echo "[letta]       runner not yet wired (launch-day v0.7.0)"
    return 100
}

step() {
    echo ""
    echo "==> $1"
}

step "competitive-benchmarks harness — v0.7.0 launch-day driver (scaffolding)"
echo "corpus:  $CORPUS_DIR"
echo "out:     $OUT_DIR"
echo "k:       $K"
echo "stacks:  ${STACKS[*]}"

# Verify corpus exists. The corpus directory lands under
# benchmarks/longmemeval/corpus-240/ when the LongMemEval harness has
# already been run at least once. If it does not exist we surface a
# clear error rather than producing a zero-row comparison table.
if [ ! -d "$CORPUS_DIR" ]; then
    echo ""
    echo "WARN: $CORPUS_DIR does not exist."
    echo "      The competitive-benchmark harness reuses the LongMemEval"
    echo "      240-observation slice. Either:"
    echo "       (a) run benchmarks/longmemeval/harness.py to materialize"
    echo "           the slice, or"
    echo "       (b) export COMPETITIVE_CORPUS to point at an existing"
    echo "           slice directory."
    echo ""
    echo "      Continuing as a scaffolding-only run; each per-stack"
    echo "      runner will report not-yet-wired and exit 100."
    echo ""
fi

# Drive every stack runner. None are wired yet; collect the not-yet-wired
# signal so the operator sees the full scaffolding output.
declare -i not_wired=0
for stack in "${STACKS[@]}"; do
    step "run_$stack"
    if "run_$stack"; then
        :
    else
        rc=$?
        if [ "$rc" = "100" ]; then
            not_wired=$((not_wired + 1))
        else
            echo "  runner for $stack failed with rc=$rc" >&2
        fi
    fi
done

step "summary"
echo "scaffolding complete: $not_wired of ${#STACKS[@]} runners are stubs (expected for v0.7.0 scaffolding ship)."
echo "see README.md §'Launch-day plan' for the v0.7.0 launch wire-up."

# Exit 0 — scaffolding is the deliverable for this commit. Launch-day
# CI flips this to "exit non-zero if any runner stub remains" once the
# install pins land.
exit 0
