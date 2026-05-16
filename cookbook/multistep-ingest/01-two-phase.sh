#!/usr/bin/env bash
# cookbook/multistep-ingest/01-two-phase.sh
#
# v0.7.0 Form 3 (issue #756) — multi-step ingest orchestrator
# (end-to-end flow, deterministic helpers + mocked LLM).
#
# What this proves
#   The Form 3 substrate threads deterministic helpers (Jaccard
#   overlap, FTS classifier) through their stage chain first, then
#   dispatches LLM stages whose prompts share a prefix (so the
#   prompt-cache key stays stable across stages within a run) and
#   carry the helper output verbatim under the explicit-trust
#   instruction. The recipe drives both the two-phase variant
#   (Understand-Anything exemplar) and the four-step variant (OpenKB
#   exemplar) end-to-end.
#
# What it does
#   1. Carves a fresh scratch dir under .local-runs/cookbook-multistep-<ts>/.
#   2. Builds the `multistep_ingest_roundtrip` example (a thin harness
#      that drives `IngestExecutor::run` with a `MockLlmDispatch`).
#   3. Runs the two-phase variant and asserts the produced report
#      records exactly one distinct cache key.
#   4. Runs the four-step variant and asserts the same — three LLM
#      stages share one cache key.
#
# Acceptance
#   Exits 0 only when both variants produce a report whose
#   `prompt_cache_consistent` is `true` and `distinct_cache_keys`
#   array has length 1. Exits non-zero on any failure.
#
# LLM dependency
#   None. The recipe injects a deterministic `MockLlmDispatch` so the
#   substrate plumbing is exercised without an Ollama round-trip. The
#   production path's `OllamaDispatch` wraps the project's
#   `OllamaClient::generate` and is exercised by the
#   `tests/form_3_multistep_ingest.rs` acceptance suite + (post-ship)
#   the live-LLM dogfood.

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
scratch_root="${SCRATCH_ROOT:-${repo_root}/.local-runs}"
ts="$(date -u +%Y%m%dT%H%M%SZ)"
run_dir="${scratch_root}/cookbook-multistep-${ts}"
mkdir -p "${run_dir}"

export TMPDIR="${scratch_root}/tmp"
mkdir -p "${TMPDIR}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${repo_root}/target}"

echo "==> Form 3 multi-step ingest cookbook recipe"
echo "    repo: ${repo_root}"
echo "    scratch: ${run_dir}"

cd "${repo_root}"

echo "==> Building the multistep_ingest_roundtrip example"
cargo build --quiet --example multistep_ingest_roundtrip

example_bin="${CARGO_TARGET_DIR}/debug/examples/multistep_ingest_roundtrip"
if [[ ! -x "${example_bin}" ]]; then
    echo "ERROR: example binary not found at ${example_bin}" >&2
    exit 11
fi

run_variant() {
    local variant="$1"
    local report="${run_dir}/${variant}.json"
    echo "==> Running variant: ${variant}"
    "${example_bin}" --variant "${variant}" --report "${report}"
    if [[ ! -s "${report}" ]]; then
        echo "ERROR: report missing for variant=${variant}" >&2
        exit 12
    fi
    # Acceptance gate: prompt_cache_consistent must be true and
    # distinct_cache_keys must have exactly one entry.
    local consistent
    consistent="$(python3 -c "import json,sys;d=json.load(open('${report}'));print(d['prompt_cache_consistent'])")"
    if [[ "${consistent}" != "True" ]]; then
        echo "ERROR: variant=${variant} prompt_cache_consistent != true" >&2
        exit 13
    fi
    local distinct_count
    distinct_count="$(python3 -c "import json,sys;d=json.load(open('${report}'));print(len(d['distinct_cache_keys']))")"
    if [[ "${distinct_count}" != "1" ]]; then
        echo "ERROR: variant=${variant} distinct_cache_keys length=${distinct_count}, expected 1" >&2
        exit 14
    fi
    local stages
    stages="$(python3 -c "import json,sys;d=json.load(open('${report}'));print(d['stages_run'])")"
    echo "    variant=${variant} stages_run=${stages} distinct_cache_keys=1 prompt_cache_consistent=true"
}

run_variant two_phase
run_variant four_step

echo "==> Form 3 cookbook recipe passed"
echo "    reports: ${run_dir}/two_phase.json + ${run_dir}/four_step.json"
