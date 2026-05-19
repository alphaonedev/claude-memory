#!/usr/bin/env bash
# batman-bench.sh — measure Batman-active write-path latency (#800 Crack 4).
#
# Form 1 (online dedup-and-synthesis LLM call) + Form 2 (sync atomise-
# before-embed) + Form 6 (regex_then_llm classify) all run BEFORE the
# SQL insert. Cumulative latency cost is the operator-visible price of
# write-time investment.
#
# This script measures wall-clock latency of `ai-memory store` against
# four content sizes (small / medium / large / huge), N samples each,
# and reports p50/p95/p99 wall time per size. Operators compare the
# numbers against the documented budgets in `docs/PERFORMANCE.md` and
# decide whether to:
#   - turn down auto_classify_kind from regex_then_llm to regex_only
#     (Form 6 cheaper path), or
#   - flip auto_atomise_mode from synchronous to deferred (Form 2
#     fires on a background thread instead), or
#   - wait for #654 (distilled 300M hot-path model) to ship.
#
# Reports p50/p95/p99 in ms per size bucket.
#
# Usage:
#   scripts/batman-bench.sh                       # 10 samples per size
#   scripts/batman-bench.sh --samples 30
#   scripts/batman-bench.sh --namespace bench-batman --db /tmp/x.db
#
# Default --namespace is `_bench_batman_active`, which won't pollute
# your real namespace if you re-run.

set -uo pipefail

DB="${AI_MEMORY_DB:-$HOME/.claude/ai-memory.db}"
NAMESPACE="${BATMAN_NAMESPACE:-_bench_batman_active}"
SAMPLES=10

while [[ $# -gt 0 ]]; do
    case "$1" in
        --db)        DB="$2"; shift 2 ;;
        --namespace) NAMESPACE="$2"; shift 2 ;;
        --samples)   SAMPLES="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

step() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
ok()   { printf '    \033[32m✓\033[0m %s\n' "$*"; }

# A test memory body of N tokens (cl100k_base ~= 4 chars/token; use words)
body() {
    local target_chars=$1
    local seed="batman-mode-bench write-time-investment substrate atomise auto-classify confidence calibration governance signed-events nomic-embed gemma4 reranker MiniLM cross-encoder operator-key Ed25519 launchd curator-daemon namespace-standard memory-kind freshness-decay shadow-mode "
    local out=""
    while [[ ${#out} -lt $target_chars ]]; do
        out="${out}${seed}"
    done
    echo "${out:0:$target_chars}"
}

# Run one store, return wall-clock ms via `date +%s%3N` (linux) or
# python (cross-platform).
time_one() {
    local title=$1 content=$2
    python3 - <<PY
import subprocess, time
start = time.monotonic()
r = subprocess.run(
    ["ai-memory", "--db", "$DB", "store",
     "--namespace", "$NAMESPACE",
     "--tier", "mid",
     "--title", "${title}",
     "--content", "${content}",
     "--json"],
    capture_output=True, text=True,
)
elapsed_ms = (time.monotonic() - start) * 1000
print(f"{elapsed_ms:.1f}")
PY
}

# Compute p50/p95/p99 from a list of numbers via python (no numpy needed).
percentiles() {
    python3 - <<PY
import sys
vals = sorted([float(x) for x in "$1".split() if x])
n = len(vals)
def pct(p):
    if n == 0: return 0.0
    idx = max(0, min(n-1, int(round(p/100 * (n-1)))))
    return vals[idx]
print(f"p50={pct(50):.0f}ms p95={pct(95):.0f}ms p99={pct(99):.0f}ms min={min(vals):.0f}ms max={max(vals):.0f}ms")
PY
}

step "Batman write-path latency bench"
ok "DB:        $DB"
ok "namespace: $NAMESPACE"
ok "samples:   $SAMPLES per size bucket"

declare -A SIZES=(
    [tiny]=128     # below auto_atomise_threshold_cl100k=512
    [medium]=2048  # well above threshold; ~512 tokens
    [large]=8192   # ~2k tokens; multiple atoms
    [huge]=32768   # ~8k tokens; many atoms
)

declare -A RESULTS

for label in tiny medium large huge; do
    chars=${SIZES[$label]}
    step "size=$label (~$chars chars / $((chars / 4)) cl100k tokens)"
    content=$(body $chars)
    runs=""
    for i in $(seq 1 $SAMPLES); do
        t=$(time_one "bench-$label-$i-$(date +%s%3N 2>/dev/null || date +%s)000" "$content")
        runs="$runs $t"
    done
    summary=$(percentiles "$runs")
    RESULTS[$label]="$summary"
    ok "$summary"
done

step "Summary"
printf '    %-10s %s\n' "size" "latency profile (ms)"
printf '    %-10s %s\n' "----" "--------------------"
for label in tiny medium large huge; do
    printf '    %-10s %s\n' "$label" "${RESULTS[$label]}"
done

step "Interpretation"
echo "    tiny  → Form 1 dedup-synthesis only (below atomise threshold)"
echo "    medium → adds Form 2 (1-2 atoms) + Form 6 classify"
echo "    large → Form 2 amortised across many atoms — embed dominates"
echo "    huge  → Form 2 + Form 6 both scale with content; latency tax = atom_count × LLM_p95"
echo ""
echo "    Knobs to bring it down:"
echo "      ai-memory namespace batman-policy --classify-mode regex_only \\"
echo "         | <bind-via-set-standard --governance>"
echo "      AI_MEMORY_AUTO_CONFIDENCE=0  (skip Form 5 derive step)"
echo "      auto_atomise_mode = 'deferred'  (Form 2 off the write path)"
echo ""
echo "    The distilled 300M hot-path model (#654) is the structural fix"
echo "    that brings the autonomous-tier curator p95 from ~30s cold to"
echo "    <100ms warm without sacrificing the Form 1/2/5/6 cognitive surface."
