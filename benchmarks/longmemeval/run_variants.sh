#!/usr/bin/env bash
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
#
# LongMemEval — three variant runner (P8 / v0.6.3.1).
#
# Anchors a fourth row (the published v0.6.3 keyword baseline) and adds
# three new rows: semantic+rerank-off, semantic+rerank-on, autonomous+
# curator-on. Methodology pinned in `methodology.md`.
#
# Usage (reference machine, Apple M2 16GB):
#
#   ./run_variants.sh                 # run all 4 variants, 3 warmup + 5 measure
#   ./run_variants.sh keyword         # single variant
#   ./run_variants.sh semantic-rerank-off semantic-rerank-on
#
# Each run writes:
#   - results/raw-${VARIANT}-pass${N}.csv  (per-pass metrics)
#   - results/median-${VARIANT}.csv         (median across measure passes)
#   - results/summary.csv                   (one row per variant)
#
# Then `results.md` should be updated by hand from `summary.csv`.

set -euo pipefail

# -----------------------------------------------------------------------------
# Configuration (override via env)
# -----------------------------------------------------------------------------
DATASET_PATH="${DATASET_PATH:-/tmp/LongMemEval}"
DATASET_VARIANT="${DATASET_VARIANT:-S}"
WARMUP_PASSES="${WARMUP_PASSES:-3}"
MEASURE_PASSES="${MEASURE_PASSES:-5}"
COOL_DOWN_SEC="${COOL_DOWN_SEC:-300}"     # 5 min between variants for M2 thermal
RESULTS_DIR="${RESULTS_DIR:-$(dirname "$0")/results}"
HARNESS_DIR="$(cd "$(dirname "$0")" && pwd)"

# Model digests (verified separately; see methodology.md §2).
export EMBED_MODEL_MINILM="sentence-transformers/all-MiniLM-L6-v2"
export EMBED_MODEL_MINILM_REV="e4ce9877abf3edfe10b0d82785e83bdcb973e22e"
export EMBED_MODEL_NOMIC="nomic-embed-text:1.5"
export RERANK_MODEL="cross-encoder/ms-marco-MiniLM-L-6-v2"
export RERANK_MODEL_REV="ce0834f22110de6d9222af7a7a03628121708969"
export CURATOR_MODEL="gemma3:4b"
export TOKENIZER="cl100k_base"

mkdir -p "$RESULTS_DIR"

# -----------------------------------------------------------------------------
# Pre-flight
# -----------------------------------------------------------------------------
preflight() {
    if [[ ! -d "$DATASET_PATH" ]]; then
        echo "ERROR: LongMemEval dataset not found at $DATASET_PATH" >&2
        echo "  git clone https://github.com/xiaowu0162/LongMemEval $DATASET_PATH" >&2
        exit 1
    fi
    if ! command -v python3 >/dev/null; then
        echo "ERROR: python3 required" >&2; exit 1
    fi
    if ! python3 -c "import tabulate" 2>/dev/null; then
        echo "WARN: 'tabulate' missing — pip install tabulate" >&2
    fi
    if [[ -n "${REQUIRE_OLLAMA:-}" ]] && ! curl -fs http://localhost:11434/api/tags >/dev/null; then
        echo "ERROR: Ollama not running at http://localhost:11434" >&2; exit 1
    fi
    AI_MEMORY_BIN="${AI_MEMORY_BIN:-$(command -v ai-memory || true)}"
    if [[ -z "$AI_MEMORY_BIN" ]]; then
        AI_MEMORY_BIN="$(cd "$HARNESS_DIR/../.." && pwd)/target/release/ai-memory"
        if [[ ! -x "$AI_MEMORY_BIN" ]]; then
            echo "ERROR: ai-memory binary not found; run 'cargo build --release'" >&2
            exit 1
        fi
    fi
    export AI_MEMORY_BIN
}

# -----------------------------------------------------------------------------
# Variant runners
# -----------------------------------------------------------------------------
# Each runner takes a pass label ($1: warmup-N or measure-N) and writes a CSV
# row to $RESULTS_DIR/raw-${VARIANT}-${PASS}.csv

run_keyword_baseline() {
    local pass="$1"
    local db="/tmp/lme-bench-keyword.db"
    : > "$RESULTS_DIR/raw-keyword-${pass}.csv"
    # Reproduces the published 97.8% R@5 row (LLM-expanded + parallel FTS5).
    REQUIRE_OLLAMA=1 python3 "$HARNESS_DIR/harness_99.py" \
        --dataset-path "$DATASET_PATH" \
        --variant "$DATASET_VARIANT" \
        --output "$RESULTS_DIR/raw-keyword-${pass}.csv"
}

run_semantic_rerank_off() {
    local pass="$1"
    local db="/tmp/lme-bench-semantic-off.db"
    rm -f "$db"
    : > "$RESULTS_DIR/raw-semantic-rerank-off-${pass}.csv"
    AI_MEMORY_DB="$db" \
    AI_MEMORY_FEATURES="semantic" \
    AI_MEMORY_RERANK="off" \
    AI_MEMORY_EMBED_MODEL="$EMBED_MODEL_MINILM" \
        python3 "$HARNESS_DIR/harness.py" \
            --dataset-path "$DATASET_PATH" \
            --variant "$DATASET_VARIANT" \
            --tier semantic \
            -k 1 -k 5 -k 10 -k 20 \
            --output "$RESULTS_DIR/raw-semantic-rerank-off-${pass}.csv"
}

run_semantic_rerank_on() {
    local pass="$1"
    local db="/tmp/lme-bench-semantic-on.db"
    rm -f "$db"
    : > "$RESULTS_DIR/raw-semantic-rerank-on-${pass}.csv"
    AI_MEMORY_DB="$db" \
    AI_MEMORY_FEATURES="semantic" \
    AI_MEMORY_RERANK="on" \
    AI_MEMORY_RERANK_MODEL="$RERANK_MODEL" \
    AI_MEMORY_EMBED_MODEL="$EMBED_MODEL_MINILM" \
        python3 "$HARNESS_DIR/harness.py" \
            --dataset-path "$DATASET_PATH" \
            --variant "$DATASET_VARIANT" \
            --tier semantic \
            -k 1 -k 5 -k 10 -k 20 \
            --output "$RESULTS_DIR/raw-semantic-rerank-on-${pass}.csv"
}

run_autonomous_curator_on() {
    local pass="$1"
    local db="/tmp/lme-bench-autonomous.db"
    rm -f "$db"
    : > "$RESULTS_DIR/raw-autonomous-curator-on-${pass}.csv"
    REQUIRE_OLLAMA=1 \
    AI_MEMORY_DB="$db" \
    AI_MEMORY_FEATURES="autonomous" \
    AI_MEMORY_RERANK="on" \
    AI_MEMORY_CURATOR="on" \
    AI_MEMORY_CURATOR_MODEL="$CURATOR_MODEL" \
    AI_MEMORY_RERANK_MODEL="$RERANK_MODEL" \
    AI_MEMORY_EMBED_MODEL="$EMBED_MODEL_NOMIC" \
        python3 "$HARNESS_DIR/harness.py" \
            --dataset-path "$DATASET_PATH" \
            --variant "$DATASET_VARIANT" \
            --tier autonomous \
            -k 1 -k 5 -k 10 -k 20 \
            --output "$RESULTS_DIR/raw-autonomous-curator-on-${pass}.csv"
}

# -----------------------------------------------------------------------------
# Pass orchestrator
# -----------------------------------------------------------------------------
run_variant_passes() {
    local variant="$1"
    local runner_fn="$2"
    echo ""
    echo "========================================================="
    echo "VARIANT: $variant"
    echo "  warmup: $WARMUP_PASSES   measure: $MEASURE_PASSES"
    echo "========================================================="
    local i
    for ((i = 1; i <= WARMUP_PASSES; i++)); do
        echo "[$variant] warmup pass $i/$WARMUP_PASSES"
        "$runner_fn" "warmup-$i"
    done
    for ((i = 1; i <= MEASURE_PASSES; i++)); do
        echo "[$variant] measure pass $i/$MEASURE_PASSES"
        "$runner_fn" "measure-$i"
    done
    # Compute median R@5 across measurement passes.
    python3 - <<PY
import csv, glob, statistics, sys
rows = []
for path in sorted(glob.glob("$RESULTS_DIR/raw-${variant}-measure-*.csv")):
    with open(path) as fh:
        for r in csv.DictReader(fh):
            if r.get("category") in ("Overall", "overall"):
                rows.append(r)
if not rows:
    print(f"WARN: no Overall rows for $variant", file=sys.stderr); sys.exit(0)
def med(field):
    vals = [float(r[field]) for r in rows if r.get(field) not in (None, "")]
    return statistics.median(vals) if vals else float("nan")
out = {
    "variant": "$variant",
    "n_passes": len(rows),
    "r1": med("r1"), "r5": med("r5"),
    "r10": med("r10"), "r20": med("r20"),
    "qps_median": med("qps") if any("qps" in r for r in rows) else "",
}
with open("$RESULTS_DIR/median-${variant}.csv", "w", newline="") as fh:
    w = csv.DictWriter(fh, fieldnames=list(out.keys()))
    w.writeheader(); w.writerow(out)
print(out)
PY
    echo "[$variant] DONE — median in $RESULTS_DIR/median-${variant}.csv"
}

# -----------------------------------------------------------------------------
# Summary roll-up
# -----------------------------------------------------------------------------
write_summary() {
    python3 - <<'PY'
import csv, glob, os
results_dir = os.environ.get("RESULTS_DIR", "results")
medians = sorted(glob.glob(f"{results_dir}/median-*.csv"))
if not medians: raise SystemExit("no medians yet")
fields = ["variant","n_passes","r1","r5","r10","r20","qps_median"]
with open(f"{results_dir}/summary.csv","w",newline="") as out:
    w = csv.DictWriter(out, fieldnames=fields); w.writeheader()
    for path in medians:
        with open(path) as fh:
            for r in csv.DictReader(fh): w.writerow({k:r.get(k,"") for k in fields})
print(open(f"{results_dir}/summary.csv").read())
PY
}

# -----------------------------------------------------------------------------
# Main
# -----------------------------------------------------------------------------
preflight

declare -A VARIANTS=(
    ["keyword"]="run_keyword_baseline"
    ["semantic-rerank-off"]="run_semantic_rerank_off"
    ["semantic-rerank-on"]="run_semantic_rerank_on"
    ["autonomous-curator-on"]="run_autonomous_curator_on"
)

if [[ $# -eq 0 ]]; then
    selected=("keyword" "semantic-rerank-off" "semantic-rerank-on" "autonomous-curator-on")
else
    selected=("$@")
fi

for v in "${selected[@]}"; do
    if [[ -z "${VARIANTS[$v]:-}" ]]; then
        echo "ERROR: unknown variant '$v' (valid: ${!VARIANTS[*]})" >&2
        exit 2
    fi
    run_variant_passes "$v" "${VARIANTS[$v]}"
    if [[ "$v" != "${selected[-1]}" ]]; then
        echo "Cooling down ${COOL_DOWN_SEC}s before next variant..."
        sleep "$COOL_DOWN_SEC"
    fi
done

write_summary
echo ""
echo "All variants complete. Update benchmarks/longmemeval/results.md from"
echo "  $RESULTS_DIR/summary.csv"
