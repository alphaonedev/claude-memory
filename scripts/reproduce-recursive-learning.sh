#!/usr/bin/env bash
# reproduce-recursive-learning.sh — end-to-end demo of the v0.7.0
# recursive-learning primitive (issue #655).
#
# What this does:
#   1. Builds --release (sal-postgres feature on by default so the
#      PostgresStore::reflect parity path compiles; sqlite is the
#      runtime backend).
#   2. Creates a fresh sqlite DB under
#      `.local-runs/repro-recursive-learning-<timestamp>/memory.db`
#      (NOT /tmp — the project HARD RULE in CLAUDE.md forbids
#      agent-created files on tmpfs).
#   3. Inserts 3 sample memories at depth=0 via the CLI `store` verb.
#   4. Drives the MCP server over stdio JSON-RPC to call
#      `memory_reflect` on those three sources, producing a
#      reflection at depth=1.
#   5. Reflects on the depth=1 reflection → depth=2.
#   6. Reflects on the depth=2 reflection → depth=3 (at the default
#      cap).
#   7. Reflects on the depth=3 reflection → depth=4 (refuses with
#      `REFLECTION_DEPTH_EXCEEDED`).
#   8. Prints a clearly-formatted verdict block.
#
# Idempotent: every run uses a fresh timestamped subdir, so re-runs
# do not collide.
#
# Tasks 1-4 of the v0.7.0 recursive-learning epic (commits
# f5d8a9e, 630a6db, b51a3f3, 3dc76f3) supply the substrate; this
# script exercises them end-to-end against a real binary. Tasks 5/6
# (signed_events audit row + pre_reflect/post_reflect hooks) land
# on the same branch and will be observable from the same demo DB
# once they merge — no script change required for Task 5 (audit row
# lands on the same `db::reflect` cap-refusal path the depth=4 step
# already exercises).

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TS="$(date +%Y%m%dT%H%M%S)"
RUN_DIR="$REPO/.local-runs/repro-recursive-learning-$TS"
DB="$RUN_DIR/memory.db"
LOG="$RUN_DIR/run.log"
BIN="$REPO/target/release/ai-memory"
KEEP_DB="${REPRO_KEEP_DB:-0}"

# ─── Pretty output helpers ──────────────────────────────────────────
BOLD=$'\033[1m'
DIM=$'\033[2m'
RED=$'\033[31m'
GREEN=$'\033[32m'
YELLOW=$'\033[33m'
BLUE=$'\033[34m'
RESET=$'\033[0m'
if [[ ! -t 1 ]] || [[ "${NO_COLOR:-}" == "1" ]]; then
  BOLD=""
  DIM=""
  RED=""
  GREEN=""
  YELLOW=""
  BLUE=""
  RESET=""
fi

step() { printf "%s==> %s%s\n" "$BOLD" "$*" "$RESET"; }
info() { printf "    %s%s%s\n" "$DIM" "$*" "$RESET"; }
ok() { printf "    %s%s ✓%s\n" "$GREEN" "$*" "$RESET"; }
warn() { printf "    %s%s%s\n" "$YELLOW" "$*" "$RESET"; }
err() { printf "%s%s ✗%s\n" "$RED" "$*" "$RESET" >&2; }

# ─── 1. Build the release binary ────────────────────────────────────
mkdir -p "$RUN_DIR"
step "1/8  cargo build --release --features sal-postgres"
info "build log → $LOG"
if (cd "$REPO" && cargo build --release --features sal-postgres) >>"$LOG" 2>&1; then
  ok "binary built: $BIN"
else
  err "build failed — see $LOG"
  exit 1
fi

# ─── 2. Spin up a fresh sqlite DB under .local-runs/ ────────────────
step "2/8  create fresh sqlite DB"
info "db path: $DB"
# `--db` will create the file on first open; the boot path runs the
# v29 schema (Task 1/8 ladder). No /tmp.
"$BIN" --db "$DB" stats >/dev/null 2>>"$LOG" || true
if [[ -f "$DB" ]]; then
  ok "fresh DB initialised"
else
  err "DB not created at $DB"
  exit 1
fi

# ─── 3. Insert 3 sample memories at depth=0 via the CLI store verb ──
step "3/8  store 3 sample memories at depth=0"
NAMESPACE="repro/recursive-learning-$TS"
SRC_IDS=()
for i in 1 2 3; do
  TITLE="source-$i"
  CONTENT="Sample observation $i: agents profit from substrate-native primitives that bound their own recursion."
  json="$("$BIN" --db "$DB" --json store \
    --title "$TITLE" \
    --content "$CONTENT" \
    --namespace "$NAMESPACE" \
    --tier mid \
    2>>"$LOG")"
  id="$(printf '%s' "$json" | sed -E 's/.*"id":[[:space:]]*"([^"]+)".*/\1/')"
  if [[ -z "$id" || "$id" == "$json" ]]; then
    err "store failed for $TITLE — output: $json"
    exit 1
  fi
  SRC_IDS+=("$id")
  ok "stored $TITLE → id=$id"
done

# ─── 4. Drive memory_reflect over MCP stdio to depth=1 ──────────────
# Build a list of JSON-RPC frames piped into a single `ai-memory mcp`
# session. The MCP server returns line-delimited JSON; we extract the
# id-keyed response of interest with awk.
#
# `reflect_step <call_id> <source_id_json_array> <title>` returns the
# reflection memory id by parsing the tools/call response body. On
# refusal it returns an empty id and prints the error.
reflect_step() {
  local call_id="$1"
  local srcs_json="$2"
  local title="$3"
  local content="$4"

  # Build a 3-frame MCP session: initialize → tools/call memory_reflect → close.
  local mcp_in mcp_out
  mcp_in="$RUN_DIR/mcp-reflect-${call_id}.in.jsonl"
  mcp_out="$RUN_DIR/mcp-reflect-${call_id}.out.jsonl"

  cat >"$mcp_in" <<EOF
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"repro-recursive-learning","version":"1.0"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_reflect","arguments":{"source_ids":${srcs_json},"title":"${title}","content":"${content}","namespace":"${NAMESPACE}","tier":"long"}}}
EOF

  # Profile `full` so memory_reflect is loaded.
  AI_MEMORY_NO_CONFIG=1 "$BIN" --db "$DB" mcp --profile full \
    <"$mcp_in" >"$mcp_out" 2>>"$LOG" || true

  # Locate the response with id=2 (tools/call) and dump the body text.
  local body
  body="$(awk '/"id":2/' "$mcp_out" | head -n 1)"
  printf '%s' "$body"
}

step "4/8  reflect on 3 sources → depth=1"
SRCS_JSON="[\"${SRC_IDS[0]}\",\"${SRC_IDS[1]}\",\"${SRC_IDS[2]}\"]"
R1_RAW="$(reflect_step 1 "$SRCS_JSON" "reflection-depth-1" "Pattern across the three sources: bounded recursion is the substrate's job, not the agent's.")"
R1_BODY="$(printf '%s' "$R1_RAW" | sed -nE 's/.*"text":[[:space:]]*"((\\.|[^"\\])*)".*/\1/p' | head -n 1)"
# The body is a stringified JSON inside the MCP content payload — strip the outer escapes:
R1_BODY_UNESC="$(printf '%s' "$R1_BODY" | sed -E 's/\\"/"/g; s/\\\\/\\/g')"
R1_ID="$(printf '%s' "$R1_BODY_UNESC" | sed -E 's/.*"id":[[:space:]]*"([^"]+)".*/\1/' | head -n 1)"
R1_DEPTH="$(printf '%s' "$R1_BODY_UNESC" | sed -E 's/.*"reflection_depth":[[:space:]]*([0-9]+).*/\1/' | head -n 1)"
if [[ -z "$R1_ID" || "$R1_DEPTH" != "1" ]]; then
  err "depth-1 reflection failed; raw response:\n$R1_RAW"
  exit 1
fi
ok "depth-1 reflection minted → id=$R1_ID (reflection_depth=$R1_DEPTH)"

# ─── 5. Recursive reflection — depth=2 ──────────────────────────────
step "5/8  reflect on the depth-1 reflection → depth=2"
R2_RAW="$(reflect_step 2 "[\"$R1_ID\"]" "reflection-depth-2" "Meta-pattern: even reflections themselves are reflectable, up to the cap.")"
R2_BODY="$(printf '%s' "$R2_RAW" | sed -nE 's/.*"text":[[:space:]]*"((\\.|[^"\\])*)".*/\1/p' | head -n 1)"
R2_BODY_UNESC="$(printf '%s' "$R2_BODY" | sed -E 's/\\"/"/g; s/\\\\/\\/g')"
R2_ID="$(printf '%s' "$R2_BODY_UNESC" | sed -E 's/.*"id":[[:space:]]*"([^"]+)".*/\1/' | head -n 1)"
R2_DEPTH="$(printf '%s' "$R2_BODY_UNESC" | sed -E 's/.*"reflection_depth":[[:space:]]*([0-9]+).*/\1/' | head -n 1)"
if [[ -z "$R2_ID" || "$R2_DEPTH" != "2" ]]; then
  err "depth-2 reflection failed; raw response:\n$R2_RAW"
  exit 1
fi
ok "depth-2 reflection minted → id=$R2_ID (reflection_depth=$R2_DEPTH)"

# ─── 6. Recursive reflection — depth=3 (at the default cap) ─────────
step "6/8  reflect on the depth-2 reflection → depth=3 (at the default cap)"
R3_RAW="$(reflect_step 3 "[\"$R2_ID\"]" "reflection-depth-3" "Substrate-level discipline: cap=3 is the ceiling; this depth is the last legal one.")"
R3_BODY="$(printf '%s' "$R3_RAW" | sed -nE 's/.*"text":[[:space:]]*"((\\.|[^"\\])*)".*/\1/p' | head -n 1)"
R3_BODY_UNESC="$(printf '%s' "$R3_BODY" | sed -E 's/\\"/"/g; s/\\\\/\\/g')"
R3_ID="$(printf '%s' "$R3_BODY_UNESC" | sed -E 's/.*"id":[[:space:]]*"([^"]+)".*/\1/' | head -n 1)"
R3_DEPTH="$(printf '%s' "$R3_BODY_UNESC" | sed -E 's/.*"reflection_depth":[[:space:]]*([0-9]+).*/\1/' | head -n 1)"
if [[ -z "$R3_ID" || "$R3_DEPTH" != "3" ]]; then
  err "depth-3 reflection failed; raw response:\n$R3_RAW"
  exit 1
fi
ok "depth-3 reflection minted → id=$R3_ID (reflection_depth=$R3_DEPTH)"

# ─── 7. Refusal — depth=4 must be rejected ──────────────────────────
step "7/8  reflect on the depth-3 reflection → depth=4 (must refuse with REFLECTION_DEPTH_EXCEEDED)"
R4_RAW="$(reflect_step 4 "[\"$R3_ID\"]" "reflection-depth-4" "Should not survive — cap is 3, this attempts 4.")"
# The error path returns the error in the result.content[0].text (the
# MCP handler turns ReflectError::DepthExceeded into a string-prefixed
# user-readable message). We assert the substring REFLECTION_DEPTH_EXCEEDED.
if printf '%s' "$R4_RAW" | grep -q "REFLECTION_DEPTH_EXCEEDED"; then
  ok "depth-4 refused with REFLECTION_DEPTH_EXCEEDED (cap-enforcement working)"
  REFUSAL_OK=1
else
  err "depth-4 was NOT refused; raw response:\n$R4_RAW"
  REFUSAL_OK=0
fi

# ─── 8. Verdict block ───────────────────────────────────────────────
step "8/8  verdict"
echo
printf "%s+-------------------------------------------------------------+%s\n" "$BOLD" "$RESET"
printf "%s| v0.7.0 recursive-learning -- reproduction verdict           |%s\n" "$BOLD" "$RESET"
printf "%s+-------------------------------------------------------------+%s\n" "$BOLD" "$RESET"
printf "%s| %s db                  %s| %s %s\n" "$BOLD" "$RESET" "$BOLD" "$RESET" "$DB"
printf "%s| %s namespace           %s| %s %s\n" "$BOLD" "$RESET" "$BOLD" "$RESET" "$NAMESPACE"
printf "%s| %s source memories     %s| %s 3 (depth=0)\n" "$BOLD" "$RESET" "$BOLD" "$RESET"
printf "%s| %s depth=1 reflection  %s| %s ${GREEN}OK${RESET}  id=%s\n" "$BOLD" "$RESET" "$BOLD" "$RESET" "$R1_ID"
printf "%s| %s depth=2 reflection  %s| %s ${GREEN}OK${RESET}  id=%s\n" "$BOLD" "$RESET" "$BOLD" "$RESET" "$R2_ID"
printf "%s| %s depth=3 reflection  %s| %s ${GREEN}OK${RESET}  id=%s (at default cap)\n" "$BOLD" "$RESET" "$BOLD" "$RESET" "$R3_ID"
if [[ "$REFUSAL_OK" == "1" ]]; then
  printf "%s| %s depth=4 refusal     %s| %s ${GREEN}OK${RESET}  REFLECTION_DEPTH_EXCEEDED\n" "$BOLD" "$RESET" "$BOLD" "$RESET"
else
  printf "%s| %s depth=4 refusal     %s| %s ${RED}FAIL${RESET}  CAP NOT ENFORCED\n" "$BOLD" "$RESET" "$BOLD" "$RESET"
fi
printf "%s+-------------------------------------------------------------+%s\n" "$BOLD" "$RESET"
echo

if [[ "$REFUSAL_OK" != "1" ]]; then
  err "Cap-enforcement test FAILED — substrate did not reject the depth-4 attempt."
  exit 2
fi

ok "All 4 levels of the recursive-learning primitive exercised end-to-end."
info "Reflection memories and reflects_on edges are visible via:"
info "  $BIN --db $DB list --namespace $NAMESPACE"
info "  $BIN --db $DB --json get $R3_ID | jq '.reflection_depth'"
info "Tasks 5+6 (signed_events audit row + hook events) will be observable"
info "from the same DB once they land — no script changes required."

if [[ "$KEEP_DB" != "1" ]]; then
  info "Cleaning up demo DB (set REPRO_KEEP_DB=1 to retain)."
  rm -rf "$RUN_DIR"
  ok "removed $RUN_DIR"
else
  info "Retained $RUN_DIR (REPRO_KEEP_DB=1)."
fi
