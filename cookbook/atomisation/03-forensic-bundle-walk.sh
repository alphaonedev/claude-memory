#!/usr/bin/env bash
# cookbook/atomisation/03-forensic-bundle-walk.sh
#
# v0.7.0 WT-1-G — atomisation cookbook recipe 03.
#
# What this proves
#   Audit-chain visibility. The atomisation pipeline emits two
#   `atomisation_complete` rows into `signed_events` and the
#   `derives_from` edges land in `memory_links` with the curator's
#   signing posture stamped. An offline auditor can replay the full
#   parent → atoms lineage from the forensic bundle.
#
# Acceptance
#   Exits 0 only when the forensic bundle contained the parent and
#   atom envelopes (chain_envelope_included == true).
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
RUN_DIR="$DEMO_ROOT/cookbook-atomisation-03-$TS"
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

step "2/3  forensic bundle must include parent + atom envelopes"
if [[ ! -s "$REPORT" ]]; then
  err "report file missing at $REPORT"
  exit 1
fi

read_num() {
  sed -nE "s/.*\"$1\"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p" "$REPORT" | head -n 1
}
read_bool() {
  sed -nE "s/.*\"$1\"[[:space:]]*:[[:space:]]*(true|false).*/\1/p" "$REPORT" | head -n 1
}

ATOM_COUNT="$(read_num atom_count)"
ATOMISED_INTO="$(read_num atomised_into)"
ATOM_OF_ROWS="$(read_num atom_of_row_count)"
CHAIN_INCLUDED="$(read_bool chain_envelope_included)"

info "atom_count=$ATOM_COUNT  parent.atomised_into=$ATOMISED_INTO  atom_of_rows=$ATOM_OF_ROWS"
info "forensic chain_envelope_included=$CHAIN_INCLUDED"

FAIL=0
[[ "${ATOM_COUNT:-0}" -ge 2 ]] || { err "atom_count must be >= 2"; FAIL=1; }
[[ "${ATOM_OF_ROWS:-0}" -eq "${ATOM_COUNT:-0}" ]] \
  || { err "atom_of_rows ($ATOM_OF_ROWS) must equal atom_count ($ATOM_COUNT) — lineage gap"; FAIL=1; }
[[ "${CHAIN_INCLUDED:-false}" == "true" ]] \
  || { err "forensic bundle must include parent + atom envelopes (chain_envelope_included=false)"; FAIL=1; }

if [[ "$FAIL" -ne 0 ]]; then
  err "audit-chain invariant slipped — see $REPORT and $LOG"
  exit 2
fi
ok "forensic chain replay walks parent + ${ATOM_COUNT} atom envelopes"

step "3/3  verdict"
echo
printf "%s+-- WT-1 cookbook 03 — forensic bundle walk -----+%s\n" "$BOLD" "$RESET"
printf "%s| atom_count                       %s| %s\n" "$BOLD" "$RESET" "$ATOM_COUNT"
printf "%s| atom_of lineage rows             %s| %s\n" "$BOLD" "$RESET" "$ATOM_OF_ROWS"
printf "%s| forensic chain envelope included %s| %s\n" "$BOLD" "$RESET" "$CHAIN_INCLUDED"
printf "%s+------------------------------------------------+%s\n" "$BOLD" "$RESET"
echo
ok "Recipe 03 — forensic bundle walk reproduced end-to-end."

if [[ "${COOKBOOK_KEEP_DB:-0}" != "1" ]]; then
  rm -rf "$RUN_DIR"
  info "cleaned up $RUN_DIR (set COOKBOOK_KEEP_DB=1 to retain)"
fi
