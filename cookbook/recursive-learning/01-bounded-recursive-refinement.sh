#!/usr/bin/env bash
# cookbook/recursive-learning/01-bounded-recursive-refinement.sh
#
# v0.7.0 Grand-Slam L3-2 (#675) — recipe 01.
#
# What this proves
#   The substrate (not the application) enforces the recursive-refinement
#   depth cap. An agent can call memory_reflect up to the configured
#   ceiling and is refused at cap+1. The external `verify-reflection-chain`
#   verb walks the resulting `reflects_on` edges backward to depth 0,
#   re-verifies every Ed25519 signature, and emits a structured chain-
#   integrity report — exit 0 only when the whole chain checks out.
#
# What it does
#   1. Carves a fresh sqlite DB under .local-runs/cookbook-01-<ts>/.
#   2. Stores 3 source observations at depth=0.
#   3. Drives memory_reflect over MCP stdio JSON-RPC to depth=1 / 2 / 3.
#   4. Attempts depth=4 — substrate must refuse with
#      REFLECTION_DEPTH_EXCEEDED.
#   5. Walks the depth=3 chain via `ai-memory verify-reflection-chain`
#      (JSON output) and asserts chain_depth=3, edges_failed=0.
#   6. Prints a verdict block.
#
# Acceptance
#   Exits 0 only when all four depth steps behave correctly AND the
#   external verifier reports a clean walk. Exits >0 on any failure.
#
# Hard rules
#   - No /tmp / /var/tmp / /private/tmp writes (project HARD RULE).
#   - Idempotent: every run uses a fresh timestamped subdir.
#   - Each invocation < 10 min on a fresh ai-memory (typical: ~30 s).
#
# Issue links
#   L1-3 verifier:    issues/667
#   Substrate cap:    issues/655
#   Recipe ticket:    issues/675

set -euo pipefail

# ─── Pretty output helpers ──────────────────────────────────────────────
BOLD=$'\033[1m'
DIM=$'\033[2m'
RED=$'\033[31m'
GREEN=$'\033[32m'
YELLOW=$'\033[33m'
RESET=$'\033[0m'
if [[ ! -t 1 ]] || [[ "${NO_COLOR:-}" == "1" ]]; then
  BOLD="" DIM="" RED="" GREEN="" YELLOW="" RESET=""
fi
step() { printf "%s==> %s%s\n" "$BOLD" "$*" "$RESET"; }
info() { printf "    %s%s%s\n" "$DIM" "$*" "$RESET"; }
ok()   { printf "    %s%s OK%s\n" "$GREEN" "$*" "$RESET"; }
warn() { printf "    %s%s%s\n" "$YELLOW" "$*" "$RESET"; }
err()  { printf "%s%s FAIL%s\n" "$RED" "$*" "$RESET" >&2; }

# ─── Resolve paths and binary ───────────────────────────────────────────
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
RUN_DIR="$DEMO_ROOT/cookbook-01-$TS"
DB="$RUN_DIR/memory.db"
LOG="$RUN_DIR/run.log"
mkdir -p "$RUN_DIR"

BIN="${AI_MEMORY_BIN:-$(command -v ai-memory || true)}"
if [[ -z "$BIN" ]] || [[ ! -x "$BIN" ]]; then
  err "ai-memory binary not found. Set AI_MEMORY_BIN=<path> or put 'ai-memory' on PATH."
  exit 65
fi
info "binary: $BIN"
info "db:     $DB"
info "log:    $LOG"

# AI_MEMORY_NO_CONFIG=1 keeps the demo hermetic — no embedder/LLM
# initialisation from the operator's ~/.config/ai-memory/config.toml.
export AI_MEMORY_NO_CONFIG=1

# ─── 1. Bootstrap the demo DB ───────────────────────────────────────────
step "1/6  bootstrap demo DB"
"$BIN" --db "$DB" stats >>"$LOG" 2>&1 || true
if [[ ! -f "$DB" ]]; then
  err "DB not created at $DB; see $LOG"
  exit 1
fi
ok "fresh sqlite DB initialised"

# ─── 2. Seed 3 observations at depth=0 ──────────────────────────────────
step "2/6  store 3 source observations (depth=0)"
NAMESPACE="cookbook/recursive-learning-01-$TS"
SRC_IDS=()
for i in 1 2 3; do
  TITLE="src-$i"
  CONTENT="Observation $i: agents profit from substrate-native primitives that bound their own recursion."
  json="$("$BIN" --db "$DB" --json store \
    --title "$TITLE" \
    --content "$CONTENT" \
    --namespace "$NAMESPACE" \
    --tier mid \
    2>>"$LOG")"
  id="$(printf '%s' "$json" | sed -E 's/.*"id":[[:space:]]*"([^"]+)".*/\1/' | head -n 1)"
  if [[ -z "$id" || "$id" == "$json" ]]; then
    err "store failed for $TITLE — output: $json"
    exit 1
  fi
  SRC_IDS+=("$id")
  ok "stored $TITLE → id=$id"
done

# ─── reflect_step helper ────────────────────────────────────────────────
# `reflect_step <call_id> <sources_json_array> <title> <content>` drives
# one memory_reflect MCP call and writes the raw response to stdout.
reflect_step() {
  local call_id="$1"
  local srcs_json="$2"
  local title="$3"
  local content="$4"

  local mcp_in="$RUN_DIR/mcp-reflect-${call_id}.in.jsonl"
  local mcp_out="$RUN_DIR/mcp-reflect-${call_id}.out.jsonl"
  cat >"$mcp_in" <<EOF
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"cookbook-01","version":"1.0"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_reflect","arguments":{"source_ids":${srcs_json},"title":"${title}","content":"${content}","namespace":"${NAMESPACE}","tier":"long"}}}
EOF
  "$BIN" --db "$DB" mcp --profile full <"$mcp_in" >"$mcp_out" 2>>"$LOG" || true
  awk '/"id":2/' "$mcp_out" | head -n 1
}

# Parse fields out of the MCP tools/call response body (a stringified
# JSON inside content[0].text).
parse_field() {
  # $1 = full mcp response; $2 = field name; emits value or empty.
  local raw="$1"
  local field="$2"
  local body unesc
  body="$(printf '%s' "$raw" | sed -nE 's/.*"text":[[:space:]]*"((\\.|[^"\\])*)".*/\1/p' | head -n 1)"
  unesc="$(printf '%s' "$body" | sed -E 's/\\"/"/g; s/\\\\/\\/g')"
  printf '%s' "$unesc" | sed -E "s/.*\"${field}\":[[:space:]]*\"?([^,\"}]+).*/\1/" | head -n 1
}

# ─── 3. depth=1 reflection over the 3 sources ───────────────────────────
step "3/6  reflect over 3 sources → depth=1"
SRCS_JSON="[\"${SRC_IDS[0]}\",\"${SRC_IDS[1]}\",\"${SRC_IDS[2]}\"]"
R1_RAW="$(reflect_step 1 "$SRCS_JSON" "reflection-depth-1" "Pattern across the three sources: bounded recursion is the substrate's job, not the agent's.")"
R1_ID="$(parse_field "$R1_RAW" "id")"
R1_DEPTH="$(parse_field "$R1_RAW" "reflection_depth")"
if [[ -z "$R1_ID" || "$R1_DEPTH" != "1" ]]; then
  err "depth-1 reflection failed; raw response: $R1_RAW"
  exit 1
fi
ok "depth-1 reflection minted → id=$R1_ID (depth=$R1_DEPTH)"

# ─── 4. depth=2 / depth=3 ───────────────────────────────────────────────
step "4/6  reflect on depth-1 → depth=2"
R2_RAW="$(reflect_step 2 "[\"$R1_ID\"]" "reflection-depth-2" "Meta-pattern: reflections themselves are reflectable, up to the cap.")"
R2_ID="$(parse_field "$R2_RAW" "id")"
R2_DEPTH="$(parse_field "$R2_RAW" "reflection_depth")"
if [[ -z "$R2_ID" || "$R2_DEPTH" != "2" ]]; then
  err "depth-2 reflection failed; raw response: $R2_RAW"
  exit 1
fi
ok "depth-2 reflection minted → id=$R2_ID (depth=$R2_DEPTH)"

step "5/6  reflect on depth-2 → depth=3 (at the default cap)"
R3_RAW="$(reflect_step 3 "[\"$R2_ID\"]" "reflection-depth-3" "Substrate-level discipline: cap=3 is the ceiling; this is the last legal depth.")"
R3_ID="$(parse_field "$R3_RAW" "id")"
R3_DEPTH="$(parse_field "$R3_RAW" "reflection_depth")"
if [[ -z "$R3_ID" || "$R3_DEPTH" != "3" ]]; then
  err "depth-3 reflection failed; raw response: $R3_RAW"
  exit 1
fi
ok "depth-3 reflection minted → id=$R3_ID (depth=$R3_DEPTH)"

# ─── Refusal at depth=4 ─────────────────────────────────────────────────
step "    attempt depth=4 (substrate MUST refuse with REFLECTION_DEPTH_EXCEEDED)"
R4_RAW="$(reflect_step 4 "[\"$R3_ID\"]" "reflection-depth-4" "Should not survive — cap is 3.")"
REFUSAL_OK=0
if printf '%s' "$R4_RAW" | grep -q "REFLECTION_DEPTH_EXCEEDED"; then
  ok "depth-4 refused with REFLECTION_DEPTH_EXCEEDED"
  REFUSAL_OK=1
else
  err "depth-4 was NOT refused; raw response: $R4_RAW"
fi

# ─── 6. External verifier walk ──────────────────────────────────────────
step "6/6  verify-reflection-chain over the depth-3 chain"
VR_JSON_PATH="$RUN_DIR/verify-reflection-chain.json"
if ! "$BIN" --db "$DB" verify-reflection-chain --format json "$R3_ID" >"$VR_JSON_PATH" 2>>"$LOG"; then
  err "verify-reflection-chain exited non-zero; see $VR_JSON_PATH and $LOG"
  exit 3
fi
CHAIN_DEPTH="$(sed -nE 's/.*"chain_depth"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p' "$VR_JSON_PATH" | head -n 1)"
EDGES_FAILED="$(sed -nE 's/.*"edges_failed"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p' "$VR_JSON_PATH" | head -n 1)"
EDGES_VERIFIED="$(sed -nE 's/.*"edges_verified"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p' "$VR_JSON_PATH" | head -n 1)"
N_MEMORIES="$(sed -nE 's/.*"n_memories"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p' "$VR_JSON_PATH" | head -n 1)"
info "report → $VR_JSON_PATH (chain_depth=$CHAIN_DEPTH edges_verified=$EDGES_VERIFIED edges_failed=$EDGES_FAILED n_memories=$N_MEMORIES)"

VERIFY_OK=0
if [[ "$CHAIN_DEPTH" == "3" && "$EDGES_FAILED" == "0" && "$EDGES_VERIFIED" -ge "1" ]]; then
  ok "chain integrity verified (depth=3, edges_failed=0, edges_verified=$EDGES_VERIFIED)"
  VERIFY_OK=1
else
  err "chain integrity check failed (chain_depth=$CHAIN_DEPTH edges_failed=$EDGES_FAILED edges_verified=$EDGES_VERIFIED)"
fi

# ─── Verdict ────────────────────────────────────────────────────────────
step "verdict"
echo
printf "%s+-- v0.7.0 bounded recursive refinement -- reproduction verdict --+%s\n" "$BOLD" "$RESET"
printf "%s| db                   %s| %s\n" "$BOLD" "$RESET" "$DB"
printf "%s| namespace            %s| %s\n" "$BOLD" "$RESET" "$NAMESPACE"
printf "%s| depth=1 reflection   %s| id=%s\n" "$BOLD" "$RESET" "$R1_ID"
printf "%s| depth=2 reflection   %s| id=%s\n" "$BOLD" "$RESET" "$R2_ID"
printf "%s| depth=3 reflection   %s| id=%s (cap)\n" "$BOLD" "$RESET" "$R3_ID"
printf "%s| depth=4 refusal      %s| %s\n" "$BOLD" "$RESET" "$([[ $REFUSAL_OK == 1 ]] && echo OK || echo FAIL)"
printf "%s| verify-chain         %s| %s\n" "$BOLD" "$RESET" "$([[ $VERIFY_OK == 1 ]] && echo OK || echo FAIL)"
printf "%s+--------------------------------------------------------------------+%s\n" "$BOLD" "$RESET"
echo

if [[ "$REFUSAL_OK" != "1" || "$VERIFY_OK" != "1" ]]; then
  err "Recipe 01 FAILED — substrate did not enforce one of the acceptance contracts."
  exit 2
fi

ok "Recipe 01 — bounded recursive refinement reproduced end-to-end."
info "Re-run to mint a fresh DB under a new timestamped subdir."
if [[ "${COOKBOOK_KEEP_DB:-0}" != "1" ]]; then
  rm -rf "$RUN_DIR"
  info "cleaned up $RUN_DIR (set COOKBOOK_KEEP_DB=1 to retain)"
fi
