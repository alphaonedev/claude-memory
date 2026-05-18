#!/usr/bin/env bash
# cookbook/atomisation/02-cli-atomise-recall-flow.sh
#
# v0.7.0 WT-1-G — atomisation cookbook recipe 02.
#
# What this proves
#   Recall-time visibility flip. The substrate's default recall path
#   SKIPS archived parents and surfaces atoms in their place; passing
#   include_archived=true re-surfaces the parent. The recipe drives
#   the invariant end-to-end via the in-tree harness.
#
# Acceptance
#   Exits 0 only when the default recall omits the archived parent
#   AND the include-archived recall surfaces it.
#
# Hard rules
#   - No /tmp / /var/tmp / /private/tmp writes (project HARD RULE).
#   - Idempotent: every run uses a fresh timestamped subdir.

set -euo pipefail

BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; GREEN=$'\033[32m'; RESET=$'\033[0m'
if [[ ! -t 1 ]] || [[ "${NO_COLOR:-}" == "1" ]]; then
  BOLD="" DIM="" RED="" GREEN="" RESET=""
fi
step() { printf "%s==> %s%s\n" "$BOLD" "$*" "$RESET"; }
info() { printf "    %s%s%s\n" "$DIM" "$*" "$RESET"; }
ok()   { printf "    %s%s OK%s\n" "$GREEN" "$*" "$RESET"; }
err()  { printf "%s%s FAIL%s\n" "$RED" "$*" "$RESET" >&2; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$SCRIPT_DIR/../.." && pwd)"

DEMO_ROOT="${AI_MEMORY_DEMO_ROOT:-$REPO/.local-runs}"
case "$DEMO_ROOT" in
  /tmp|/tmp/*|/var/tmp|/var/tmp/*|/private/tmp|/private/tmp/*)
    err "AI_MEMORY_DEMO_ROOT=$DEMO_ROOT resolves to a tmpfs path; refused (project HARD RULE)."
    exit 64
    ;;
esac

TS="$(date +%Y%m%dT%H%M%S)"
RUN_DIR="$DEMO_ROOT/cookbook-atomisation-02-$TS"
DB="$RUN_DIR/memory.db"
REPORT="$RUN_DIR/report.json"
LOG="$RUN_DIR/run.log"
mkdir -p "$RUN_DIR"

info "run dir: $RUN_DIR"
export AI_MEMORY_NO_CONFIG=1

step "1/3  run atomisation roundtrip harness"
FEATURES="${AI_MEMORY_FEATURES:-sal,sal-postgres}"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}"
export CARGO_TARGET_DIR

(
  cd "$REPO"
  cargo run --quiet --features "$FEATURES" --example atomise_roundtrip -- \
    --db "$DB" \
    --report "$REPORT"
) >>"$LOG" 2>&1
ok "harness completed"

step "2/3  default recall must skip the archived parent"
if [[ ! -s "$REPORT" ]]; then
  err "report file missing at $REPORT"
  exit 1
fi

read_bool() {
  sed -nE "s/.*\"$1\"[[:space:]]*:[[:space:]]*(true|false).*/\1/p" "$REPORT" | head -n 1
}
PARENT_SKIPPED="$(read_bool parent_skipped_by_default)"
PARENT_VISIBLE="$(read_bool parent_visible_with_include_archived)"

info "default-recall parent_skipped=$PARENT_SKIPPED"
info "include-archived parent_visible=$PARENT_VISIBLE"

FAIL=0
[[ "${PARENT_SKIPPED:-false}" == "true" ]] || { err "default recall must skip archived parent"; FAIL=1; }
[[ "${PARENT_VISIBLE:-false}" == "true" ]] || { err "include_archived flag must re-surface parent"; FAIL=1; }

if [[ "$FAIL" -ne 0 ]]; then
  err "recall-visibility invariant slipped — see $REPORT and $LOG"
  exit 2
fi
ok "recall visibility flip verified end-to-end"

step "3/3  verdict"
echo
printf "%s+-- WT-1 cookbook 02 — recall visibility flip -----+%s\n" "$BOLD" "$RESET"
printf "%s| default recall: parent skipped    %s| %s\n" "$BOLD" "$RESET" "$PARENT_SKIPPED"
printf "%s| include_archived: parent visible  %s| %s\n" "$BOLD" "$RESET" "$PARENT_VISIBLE"
printf "%s+--------------------------------------------------+%s\n" "$BOLD" "$RESET"
echo
ok "Recipe 02 — recall visibility flip reproduced end-to-end."

if [[ "${COOKBOOK_KEEP_DB:-0}" != "1" ]]; then
  rm -rf "$RUN_DIR"
  info "cleaned up $RUN_DIR (set COOKBOOK_KEEP_DB=1 to retain)"
fi
