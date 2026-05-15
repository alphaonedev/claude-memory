#!/usr/bin/env bash
# cookbook/atomisation/01-basic-flow.sh
#
# v0.7.0 WT-1-G — atomisation substrate primitive (end-to-end flow).
#
# What this proves
#   The WT-1 atomisation pipeline decomposes a long memory into atomic
#   children, archives the parent, and surfaces atoms in place of the
#   archived parent at recall time. The forensic-bundle exporter
#   captures the full parent -> atoms chain offline so a downstream
#   auditor reconstructs the decomposition without DB access.
#
# What it does
#   1. Carves a fresh sqlite DB under .local-runs/cookbook-atomisation-<ts>/.
#   2. Drives the WT-1-B Atomiser engine directly via the in-tree
#      `examples/atomise_roundtrip.rs` harness (deterministic stub
#      curator — no Ollama dependency, recipe is hermetic).
#   3. Asserts: parent.atomised_into > 0; len(atom_of children) == N;
#      default recall skips the archived parent; the include_archived
#      flag re-surfaces it; the forensic bundle includes the parent
#      envelope, every atom envelope, and the signed_events folder.
#
# Acceptance
#   Exits 0 only when every invariant above holds. Exits >0 on any
#   failure (each invariant carries a distinct exit code in the
#   example harness — see the source for the mapping).
#
# LLM dependency
#   None. The recipe injects a deterministic stub curator so the
#   substrate plumbing is exercised end-to-end without an Ollama
#   round-trip. The production curator (`LlmCurator` with Gemma 4 +
#   tiktoken-rs) is exercised by the `tests/atomisation/curator.rs`
#   acceptance suite. Audit-honest note: a recipe variant that drives
#   the real `ai-memory atomise` CLI against a live Gemma backend is
#   tracked for the v0.7.0 dogfood log; this recipe is plumbing-only
#   so it stays runnable on every host.
#
# Hard rules
#   - No /tmp / /var/tmp / /private/tmp writes (project HARD RULE).
#   - Idempotent: every run uses a fresh timestamped subdir.
#   - Self-contained: runs on a fresh checkout with no prior cookbook
#     state. Each invocation < 1 min on a fresh ai-memory checkout
#     (cargo cache permitting).

set -euo pipefail

# ─── Pretty output helpers ──────────────────────────────────────────────
BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; GREEN=$'\033[32m'
YELLOW=$'\033[33m'; RESET=$'\033[0m'
if [[ ! -t 1 ]] || [[ "${NO_COLOR:-}" == "1" ]]; then
  BOLD="" DIM="" RED="" GREEN="" YELLOW="" RESET=""
fi
step() { printf "%s==> %s%s\n" "$BOLD" "$*" "$RESET"; }
info() { printf "    %s%s%s\n" "$DIM" "$*" "$RESET"; }
ok()   { printf "    %s%s OK%s\n" "$GREEN" "$*" "$RESET"; }
err()  { printf "%s%s FAIL%s\n" "$RED" "$*" "$RESET" >&2; }

# ─── Resolve paths ──────────────────────────────────────────────────────
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
RUN_DIR="$DEMO_ROOT/cookbook-atomisation-$TS"
DB="$RUN_DIR/memory.db"
REPORT="$RUN_DIR/report.json"
LOG="$RUN_DIR/run.log"
mkdir -p "$RUN_DIR"

info "run dir: $RUN_DIR"
info "db:      $DB"
info "report:  $REPORT"

# AI_MEMORY_NO_CONFIG=1 keeps the demo hermetic — no embedder/LLM
# initialisation from the operator's ~/.config/ai-memory/config.toml.
export AI_MEMORY_NO_CONFIG=1

# ─── 1. drive the substrate engine ──────────────────────────────────────
step "1/3  run atomisation roundtrip harness (in-tree example)"
FEATURES="${AI_MEMORY_FEATURES:-sal,sal-postgres}"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}"
export CARGO_TARGET_DIR

(
  cd "$REPO"
  cargo run --quiet --features "$FEATURES" --example atomise_roundtrip -- \
    --db "$DB" \
    --report "$REPORT"
) >>"$LOG" 2>&1
ok "harness completed (exit 0)"

# ─── 2. inspect the structured report ───────────────────────────────────
step "2/3  inspect harness report"
if [[ ! -s "$REPORT" ]]; then
  err "report file missing or empty at $REPORT"
  exit 1
fi

# Parse single fields out of the JSON without taking a hard jq
# dependency. The fields below are emitted on a single line in the
# pretty-printed report, so a simple grep + sed pulls them out.
read_num() { # $1 = key
  sed -nE "s/.*\"$1\"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p" "$REPORT" | head -n 1
}
read_bool() { # $1 = key
  sed -nE "s/.*\"$1\"[[:space:]]*:[[:space:]]*(true|false).*/\1/p" "$REPORT" | head -n 1
}

ATOM_COUNT="$(read_num atom_count)"
ATOMISED_INTO="$(read_num atomised_into)"
ATOM_OF_ROWS="$(read_num atom_of_row_count)"
PARENT_SKIPPED="$(read_bool parent_skipped_by_default)"
PARENT_WITH_FLAG="$(read_bool parent_visible_with_include_archived)"
CHAIN_INCLUDED="$(read_bool chain_envelope_included)"

info "atom_count=$ATOM_COUNT  atomised_into=$ATOMISED_INTO  atom_of_rows=$ATOM_OF_ROWS"
info "recall parent_skipped_by_default=$PARENT_SKIPPED  parent_visible_with_include_archived=$PARENT_WITH_FLAG"
info "forensic chain_envelope_included=$CHAIN_INCLUDED"

FAIL=0
[[ "${ATOM_COUNT:-0}" -ge 2 ]] || { err "atom_count must be >= 2 (curator floor)"; FAIL=1; }
[[ "${ATOMISED_INTO:-0}" -ge 2 ]] || { err "parent.atomised_into must be >= 2"; FAIL=1; }
[[ "${ATOM_OF_ROWS:-0}" -ge 2 ]] || { err "atom_of children must be >= 2"; FAIL=1; }
[[ "${PARENT_SKIPPED:-false}" == "true" ]] || { err "default recall must skip archived parent"; FAIL=1; }
[[ "${PARENT_WITH_FLAG:-false}" == "true" ]] || { err "include_archived=true must re-surface parent"; FAIL=1; }
[[ "${CHAIN_INCLUDED:-false}" == "true" ]] || { err "forensic bundle must include parent + atom envelopes"; FAIL=1; }

if [[ "$FAIL" -ne 0 ]]; then
  err "one or more invariants failed — see $REPORT and $LOG"
  exit 2
fi
ok "all six WT-1 invariants hold"

# ─── 3. verdict ─────────────────────────────────────────────────────────
step "3/3  verdict"
echo
printf "%s+-- v0.7.0 WT-1 atomisation flow -- reproduction verdict --+%s\n" "$BOLD" "$RESET"
printf "%s| db                        %s| %s\n" "$BOLD" "$RESET" "$DB"
printf "%s| atom_count                %s| %s\n" "$BOLD" "$RESET" "$ATOM_COUNT"
printf "%s| parent.atomised_into      %s| %s\n" "$BOLD" "$RESET" "$ATOMISED_INTO"
printf "%s| atom_of children          %s| %s\n" "$BOLD" "$RESET" "$ATOM_OF_ROWS"
printf "%s| recall: parent skipped    %s| %s\n" "$BOLD" "$RESET" "$PARENT_SKIPPED"
printf "%s| recall: archived re-surfaces %s| %s\n" "$BOLD" "$RESET" "$PARENT_WITH_FLAG"
printf "%s| forensic chain envelope   %s| %s\n" "$BOLD" "$RESET" "$CHAIN_INCLUDED"
printf "%s+-------------------------------------------------------------+%s\n" "$BOLD" "$RESET"
echo
ok "Recipe 01 — WT-1 atomisation flow reproduced end-to-end."
info "Re-run to mint a fresh DB under a new timestamped subdir."

if [[ "${COOKBOOK_KEEP_DB:-0}" != "1" ]]; then
  rm -rf "$RUN_DIR"
  info "cleaned up $RUN_DIR (set COOKBOOK_KEEP_DB=1 to retain)"
fi
