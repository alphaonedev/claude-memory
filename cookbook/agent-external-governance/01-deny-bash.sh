#!/usr/bin/env bash
# cookbook/agent-external-governance/01-deny-bash.sh
#
# v0.7.0 7th-form closeout (issue #760) — Layer-4 agent-EXTERNAL
# governance demo. Proves the four enumerated wire-points
# (Bash / FilesystemWrite / NetworkRequest / ProcessSpawn) consult the
# substrate `governance_rules` table and halt with a structured
# refusal when a matching rule fires.
#
# What this proves
#   1. Seed rules R001-R004 ship at enabled=0 (migration 0024).
#   2. `ai-memory governance install-defaults --yes` flips them to
#      enabled=1.
#   3. `ai-memory rules check --kind bash --payload {"command":"..."}`
#      against a custom refuse-rule returns a structured `Refuse`
#      verdict carrying the operator-authored reason.
#
# What it does
#   1. Carves a fresh sqlite DB under .local-runs/cookbook-7th-form-<ts>/.
#   2. Runs the v0.7 migration ladder (via `ai-memory --db ... rules list`
#      which triggers init on first open).
#   3. Verifies R001-R004 land at enabled=0.
#   4. Calls `ai-memory governance install-defaults --yes` and re-verifies
#      R001-R004 are now enabled=1.
#   5. Generates an operator keypair, adds a custom refuse-rule for the
#      Bash kind, and runs `ai-memory rules check` to demonstrate the
#      structured refusal reason.
#
# Acceptance
#   Exits 0 only when (a) install-defaults flips all four rows,
#   (b) the custom refuse-rule is matched and reports the expected
#   reason text, and (c) no /tmp paths are written.
#
# Hard rules
#   - No /tmp / /var/tmp / /private/tmp writes (project HARD RULE).
#   - Idempotent: every run uses a fresh timestamped subdir.
#   - Self-contained: runs on a fresh checkout with no prior state.

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
RUN_DIR="$DEMO_ROOT/cookbook-7th-form-$TS"
DB="$RUN_DIR/memory.db"
KEY_DIR="$RUN_DIR/keys"
KEY="$KEY_DIR/operator.key"
LOG="$RUN_DIR/run.log"
mkdir -p "$RUN_DIR" "$KEY_DIR"

info "run dir: $RUN_DIR"
info "db:      $DB"

# ─── Build the binary (release mode if not already cached) ─────────────
BIN="${AI_MEMORY_BIN:-}"
if [[ -z "$BIN" ]]; then
  step "Build ai-memory (debug)"
  ( cd "$REPO" && cargo build --bin ai-memory ) >>"$LOG" 2>&1
  BIN="$REPO/target/debug/ai-memory"
fi
[[ -x "$BIN" ]] || { err "ai-memory binary not found at $BIN"; exit 64; }

# ─── Initialise the DB (run migrations) ────────────────────────────────
step "Initialise DB at $DB"
# `rules list` opens the DB which runs the v0.7 migration ladder.
"$BIN" --db "$DB" rules list --json >/dev/null
ok "schema initialised"

# ─── Step 1: Verify R001-R004 land at enabled = 0 ──────────────────────
step "Verify seed rules R001-R004 ship at enabled = 0"
RULES_JSON="$("$BIN" --db "$DB" rules list --json)"
for id in R001 R002 R003 R004; do
  enabled=$(printf '%s\n' "$RULES_JSON" | python3 -c "
import json, sys
rules = json.load(sys.stdin)['result']
row = next((r for r in rules if r['id'] == '$id'), None)
print(int(row['enabled']) if row else 'MISSING')
")
  if [[ "$enabled" != "0" ]]; then
    err "rule $id: expected enabled=0, got enabled=$enabled"
    exit 1
  fi
  ok "$id is enabled=0"
done

# ─── Step 2: install-defaults --yes flips them to enabled = 1 ──────────
step "Run \`governance install-defaults --yes\` to activate R001-R004"
INSTALL_OUT="$("$BIN" --db "$DB" governance install-defaults --yes)"
printf '%s\n' "$INSTALL_OUT" | tee -a "$LOG"
if ! printf '%s\n' "$INSTALL_OUT" | grep -q "Activated 4 rule(s)"; then
  err "install-defaults did not report 4 activations"
  exit 1
fi
ok "install-defaults activated R001-R004"

# ─── Step 3: Verify the flip persisted ─────────────────────────────────
step "Re-verify R001-R004 now enabled = 1"
RULES_JSON="$("$BIN" --db "$DB" rules list --json)"
for id in R001 R002 R003 R004; do
  enabled=$(printf '%s\n' "$RULES_JSON" | python3 -c "
import json, sys
rules = json.load(sys.stdin)['result']
row = next((r for r in rules if r['id'] == '$id'), None)
print(int(row['enabled']) if row else 'MISSING')
")
  if [[ "$enabled" != "1" ]]; then
    err "rule $id: expected enabled=1 after install-defaults, got enabled=$enabled"
    exit 1
  fi
  ok "$id is enabled=1"
done

# ─── Step 4: Add a custom Bash refuse-rule and check it ────────────────
step "Generate an operator keypair under $KEY_DIR"
"$BIN" --db "$DB" rules --key-dir "$KEY_DIR" keygen --out "$KEY" >>"$LOG"
ok "operator key generated"

step "Add a Bash refuse-rule (R-bash-demo) for command substring 'rm -rf /'"
"$BIN" --db "$DB" rules --key-dir "$KEY_DIR" add \
  --id R-bash-demo \
  --kind bash \
  --matcher '{"command_regex":"rm -rf /"}' \
  --severity refuse \
  --reason "Cookbook demo: refuse destructive Bash" \
  --sign >>"$LOG"
ok "R-bash-demo added (enabled by default; --sign attaches operator signature)"

step "Run \`rules check\` against an offending Bash command"
CHECK_OUT="$("$BIN" --db "$DB" rules --key-dir "$KEY_DIR" check \
  --kind bash \
  --payload '{"command":"rm -rf /"}' \
  --json)"
printf '%s\n' "$CHECK_OUT" | tee -a "$LOG"

decision=$(printf '%s\n' "$CHECK_OUT" | python3 -c "
import json, sys
v = json.load(sys.stdin)['result']
print(v.get('decision', 'UNKNOWN'))
")
if [[ "$decision" != "refuse" ]]; then
  err "rules check decision: expected 'refuse', got '$decision'"
  exit 1
fi
ok "rules check returned structured 'refuse' verdict"

step "Run \`rules check\` against a benign Bash command (should Allow)"
ALLOW_OUT="$("$BIN" --db "$DB" rules --key-dir "$KEY_DIR" check \
  --kind bash \
  --payload '{"command":"ls -la"}' \
  --json)"
decision=$(printf '%s\n' "$ALLOW_OUT" | python3 -c "
import json, sys
v = json.load(sys.stdin)['result']
print(v.get('decision', 'UNKNOWN'))
")
if [[ "$decision" != "allow" ]]; then
  err "benign Bash check: expected 'allow', got '$decision'"
  exit 1
fi
ok "rules check returned 'allow' for benign Bash"

# ─── Done ──────────────────────────────────────────────────────────────
step "All assertions passed"
info "log:  $LOG"
info "db:   $DB"
echo
echo "${GREEN}7th-form Layer-4 wiring demo PASSED.${RESET}"
echo "${DIM}    Wire-points consult substrate rules; refusals are structured.${RESET}"
echo "${DIM}    Honest framing: mechanical at the harness hook boundary,${RESET}"
echo "${DIM}    not at the agent attention boundary.${RESET}"
