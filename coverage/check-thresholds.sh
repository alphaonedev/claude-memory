#!/usr/bin/env bash
# coverage/check-thresholds.sh — v0.7.0 L0.7-7 discipline keystone.
#
# Reads coverage/thresholds.toml + a cargo llvm-cov --json output and
# fails (exit 1) if any module's current line coverage dropped below
# its threshold. Passes (exit 0) when all per-module thresholds hold
# AND the global floor is met.
#
# Usage:
#   bash coverage/check-thresholds.sh [thresholds.toml] [cov.json]
#
# Defaults:
#   thresholds.toml = coverage/thresholds.toml
#   cov.json        = coverage/current.json
#
# Discipline (per playbook §8):
#   Thresholds rise across releases; NEVER fall.
#   If a module drops below its threshold in a future release, CI fails
#   and the offending commit cannot merge until coverage is restored.
#   Lowering a threshold requires explicit operator approval.

set -eu

THRESHOLDS_TOML="${1:-coverage/thresholds.toml}"
COV_JSON="${2:-coverage/current.json}"

if [ ! -f "$THRESHOLDS_TOML" ]; then
  echo "ERROR: thresholds file not found: $THRESHOLDS_TOML" >&2
  exit 2
fi

if [ ! -f "$COV_JSON" ]; then
  echo "ERROR: coverage JSON not found: $COV_JSON" >&2
  echo "  hint: run 'cargo llvm-cov --features sal,sal-postgres --lib --tests --json --output-path $COV_JSON --workspace'" >&2
  exit 2
fi

if ! command -v jq >/dev/null 2>&1; then
  echo "ERROR: jq is required but not installed" >&2
  exit 2
fi

# ---- Global floor ---------------------------------------------------------
global_min=$(grep -E '^min_line_coverage[[:space:]]*=' "$THRESHOLDS_TOML" \
  | head -1 | awk -F= '{print $2}' | awk '{print $1}' | tr -d ' ')

if [ -z "$global_min" ]; then
  echo "ERROR: could not parse min_line_coverage from $THRESHOLDS_TOML" >&2
  exit 2
fi

global_actual=$(jq -r '.data[0].totals.lines.percent' "$COV_JSON")
if [ -z "$global_actual" ] || [ "$global_actual" = "null" ]; then
  echo "ERROR: could not parse global line coverage from $COV_JSON" >&2
  exit 2
fi

if awk "BEGIN { exit !($global_actual >= $global_min) }"; then
  printf 'GLOBAL: %.2f%% >= %.2f%%  PASS\n' "$global_actual" "$global_min"
else
  printf 'GLOBAL: %.2f%% <  %.2f%%  FAIL\n' "$global_actual" "$global_min"
  exit 1
fi

# ---- Per-module checks ----------------------------------------------------
# Format expected in thresholds.toml:
#   "<path>" = <pct>  # optional comment
# Lines outside the [modules] table are ignored; only lines that match
# the quoted-key = number pattern are evaluated.

exit_code=0
pass_count=0
fail_count=0
warn_count=0

# Read coverage JSON once and produce a tabbed (path \t pct) stream we can
# look up cheaply. We anchor on '/src/<path>' so the relative key in the
# toml matches whether the workspace path is /Users/.../src/foo.rs or
# /home/runner/.../src/foo.rs.
cov_table=$(jq -r '
  .data[0].files[]
  | [(.filename | sub(".*/src/"; "")), (.summary.lines.percent)]
  | @tsv
' "$COV_JSON")

lookup_pct() {
  # $1: module path (relative to src/)
  printf '%s\n' "$cov_table" \
    | awk -F'\t' -v p="$1" '$1 == p { print $2; exit }'
}

while IFS= read -r line; do
  # Strip leading whitespace + skip pure comment / blank lines fast.
  case "$line" in
    \#*|"") continue ;;
  esac
  # Match: "<path>" = <pct>  with optional trailing comment
  if [[ "$line" =~ ^[[:space:]]*\"([^\"]+)\"[[:space:]]*=[[:space:]]*([0-9]+(\.[0-9]+)?) ]]; then
    path="${BASH_REMATCH[1]}"
    threshold="${BASH_REMATCH[2]}"

    current=$(lookup_pct "$path")

    if [ -z "$current" ]; then
      printf 'WARN: %-44s not in coverage report (skipping)\n' "$path"
      warn_count=$((warn_count + 1))
      continue
    fi

    if awk "BEGIN { exit !($current >= $threshold) }"; then
      pass_count=$((pass_count + 1))
    else
      printf 'FAIL: %-44s measured %.2f%% < threshold %s%%\n' \
        "$path" "$current" "$threshold"
      fail_count=$((fail_count + 1))
      exit_code=1
    fi
  fi
done < "$THRESHOLDS_TOML"

echo "---"
printf 'PER-MODULE: %d pass, %d fail, %d warn\n' \
  "$pass_count" "$fail_count" "$warn_count"

if [ "$exit_code" -eq 0 ]; then
  echo "All per-module thresholds: PASS"
else
  echo "Per-module thresholds: FAIL — at least one module dropped below its floor."
  echo "Per playbook §8: thresholds rise across releases; NEVER fall."
  echo "Lowering a threshold requires explicit operator approval in the PR description."
fi

exit "$exit_code"
