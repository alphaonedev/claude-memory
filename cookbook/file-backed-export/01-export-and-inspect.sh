#!/usr/bin/env bash
# cookbook/file-backed-export/01-export-and-inspect.sh
#
# v0.7.0 Grand-Slam QW-1 — recipe 01.
#
# What this proves
#   The substrate ships a file-backed reflection chain export. Operators
#   can `cat ~/.ai-memory/reflections/<namespace>/<id>.md` and read what
#   the substrate has synthesised without learning SQL. The export is a
#   derived artefact — the SQL row stays canonical.
#
# What it does
#   1. Carves a fresh sqlite DB under .local-runs/cookbook-qw1-<ts>/.
#   2. Stores 5 source observations at depth=0.
#   3. Drives memory_reflect over MCP stdio JSON-RPC to mint 5
#      depth=1 reflections (one per observation).
#   4. Calls `ai-memory export-reflections --out-dir ./out` to dump
#      every reflection to disk as YAML-frontmatter markdown.
#   5. `cat` one of the exported files so the operator sees the
#      shape (id, depth, attest_level, reflects_on, body).
#   6. `grep -c reflects_on:` across the exported files — asserts
#      every file carries the frontmatter row that names the source(s).
#
# Acceptance
#   Exits 0 only when (a) 5 .md files land under <out-dir>/<ns>/, (b)
#   every file contains `reflects_on:` in its frontmatter, and (c) the
#   `cat` of one file shows the expected fields. Exits >0 on any
#   failure.
#
# Hard rules
#   - No /tmp / /var/tmp / /private/tmp writes (project HARD RULE).
#   - Idempotent: every run uses a fresh timestamped subdir.
#   - Each invocation < 3 min on a fresh ai-memory.
#
# Issue links
#   QW-1 spec:        v0.7.0 Grand-Slam QW-1 (Tencent comparison)
#   Substrate cap:    issues/655
#   Recipe ticket:    QW-1

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
RUN_DIR="$DEMO_ROOT/cookbook-qw1-$TS"
DB="$RUN_DIR/memory.db"
LOG="$RUN_DIR/run.log"
OUT_DIR="$RUN_DIR/out"
mkdir -p "$RUN_DIR"

BIN="${AI_MEMORY_BIN:-$(command -v ai-memory || true)}"
if [[ -z "$BIN" ]] || [[ ! -x "$BIN" ]]; then
  err "ai-memory binary not found. Set AI_MEMORY_BIN=<path> or put 'ai-memory' on PATH."
  exit 65
fi
info "binary:  $BIN"
info "db:      $DB"
info "out-dir: $OUT_DIR"
info "log:     $LOG"

export AI_MEMORY_NO_CONFIG=1

# ─── 1. Bootstrap the demo DB ───────────────────────────────────────────
step "1/6  bootstrap demo DB"
"$BIN" --db "$DB" stats >>"$LOG" 2>&1 || true
if [[ ! -f "$DB" ]]; then
  err "DB not created at $DB; see $LOG"
  exit 1
fi
ok "fresh sqlite DB initialised"

# ─── 2. Seed 5 observations at depth=0 ──────────────────────────────────
NAMESPACE="cookbook/qw1-$TS"
step "2/6  store 5 source observations (depth=0) under $NAMESPACE"
SRC_IDS=()
for i in 1 2 3 4 5; do
  out=$(
    "$BIN" --db "$DB" store \
      --tier mid \
      --namespace "$NAMESPACE" \
      --title "observation-$i" \
      --content "raw observation #$i body" \
      --source cli 2>>"$LOG"
  )
  # `ai-memory store` prints a line shaped `stored: <id> [tier] (ns=...)`.
  id=$(echo "$out" | awk '/^stored:/ {print $2}' | head -n1)
  if [[ -z "$id" ]]; then
    err "could not parse stored id from output: $out"
    exit 2
  fi
  SRC_IDS+=("$id")
done
ok "5 observations stored"

# ─── 3. Drive memory_reflect via MCP to mint 5 depth=1 reflections ─────
step "3/6  mint 5 depth=1 reflections via memory_reflect (MCP stdio)"
RFL_IDS=()
# Build a single stdio JSON-RPC stream: initialize + 5 tools/call.
# Each reflection has its own request id so the responses are
# disambiguated.
MCP_INPUT="$RUN_DIR/mcp-input.json"
{
  printf '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"cookbook-qw1","version":"0.7.0"}}}\n'
  for i in 1 2 3 4 5; do
    src="${SRC_IDS[$((i-1))]}"
    printf '{"jsonrpc":"2.0","id":%d,"method":"tools/call","params":{"name":"memory_reflect","arguments":{"source_ids":["%s"],"title":"reflection-%d","content":"synthesised lesson #%d","namespace":"%s"}}}\n' \
      "$i" "$src" "$i" "$i" "$NAMESPACE"
  done
} >"$MCP_INPUT"

MCP_OUT="$RUN_DIR/mcp-output.json"
"$BIN" --db "$DB" mcp --tier semantic --profile full <"$MCP_INPUT" >"$MCP_OUT" 2>>"$LOG" &
MCP_PID=$!
# Give the server up to 30s to chew through 5 reflect calls.
wait_seconds=0
while [[ $wait_seconds -lt 30 ]]; do
  if ! kill -0 "$MCP_PID" 2>/dev/null; then
    break
  fi
  # Stop early if we already saw 5 successful responses + initialize ack.
  if [[ $(grep -c '"result":{' "$MCP_OUT" 2>/dev/null || echo 0) -ge 6 ]]; then
    kill "$MCP_PID" 2>/dev/null || true
    break
  fi
  sleep 1
  wait_seconds=$((wait_seconds + 1))
done
wait "$MCP_PID" 2>/dev/null || true

# Parse reflection ids from the response stream. The reflect handler
# returns `{"id":"<uuid>","reflection_depth":N,...}` inside content[0].text.
# We pull every 36-char UUID that follows `\"id\":\"` (escaped JSON
# inside the content text payload), then dedupe + drop the source ids.
mapfile -t ALL_UUIDS < <(grep -oE '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' "$MCP_OUT" | sort -u)
for uuid in "${ALL_UUIDS[@]}"; do
  # Skip uuids that are source ids (already in SRC_IDS).
  is_source=0
  for src in "${SRC_IDS[@]}"; do
    if [[ "$src" == "$uuid" ]]; then is_source=1; break; fi
  done
  if [[ $is_source -eq 0 ]]; then
    RFL_IDS+=("$uuid")
  fi
done

if [[ "${#RFL_IDS[@]}" -lt 5 ]]; then
  err "expected 5 reflections, got ${#RFL_IDS[@]}; see $MCP_OUT"
  cat "$MCP_OUT" >&2 || true
  exit 3
fi
ok "5 reflections minted: ${RFL_IDS[*]:0:2} …"

# ─── 4. Export to disk ───────────────────────────────────────────────────
step "4/6  export reflections to $OUT_DIR"
"$BIN" --db "$DB" export-reflections \
  --namespace "$NAMESPACE" \
  --out-dir "$OUT_DIR" \
  --format md \
  --quiet >>"$LOG" 2>&1
ok "export-reflections returned cleanly"

NS_SAFE=$(printf '%s' "$NAMESPACE" | tr '/' '\n' | sed 's/[^A-Za-z0-9._-]/_/g' | paste -sd '/' -)
NS_DIR="$OUT_DIR/$NS_SAFE"
COUNT=$(find "$NS_DIR" -maxdepth 1 -type f -name '*.md' | wc -l | tr -d ' ')
if [[ "$COUNT" -ne 5 ]]; then
  err "expected 5 .md files under $NS_DIR, got $COUNT"
  exit 4
fi
ok "5 .md files landed under $NS_DIR"

# ─── 5. cat one exported file ───────────────────────────────────────────
step "5/6  cat one exported reflection"
SAMPLE=$(find "$NS_DIR" -maxdepth 1 -type f -name '*.md' | head -n1)
printf "    %s\n" "----------------------------------------"
cat "$SAMPLE"
printf "    %s\n" "----------------------------------------"
# Sanity-check the frontmatter fields the spec pins.
for field in "memory_id:" "namespace:" "reflection_depth:" "attest_level:" "created_at:" "agent_id:" "reflects_on:"; do
  if ! grep -q "^$field" "$SAMPLE"; then
    err "frontmatter missing required field: $field"
    exit 5
  fi
done
ok "frontmatter carries all 7 required fields"

# ─── 6. grep for reflects_on: across every file ─────────────────────────
step "6/6  grep -c 'reflects_on:' across exported files"
WITH_EDGE=$(grep -l 'reflects_on:' "$NS_DIR"/*.md | wc -l | tr -d ' ')
if [[ "$WITH_EDGE" -ne 5 ]]; then
  err "expected all 5 files to carry 'reflects_on:'; only $WITH_EDGE did"
  exit 6
fi
ok "every exported file carries the reflects_on edge list"

# ─── Verdict ────────────────────────────────────────────────────────────
printf "\n%s━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━%s\n" "$BOLD" "$RESET"
printf "%sQW-1 verdict: ${GREEN}PASS${RESET}%s\n" "$BOLD" "$RESET"
printf "  files written: 5/5\n"
printf "  frontmatter:   complete\n"
printf "  reflects_on:   5/5\n"
printf "  out-dir:       %s\n" "$NS_DIR"
printf "  log:           %s\n" "$LOG"
printf "%s━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━%s\n\n" "$BOLD" "$RESET"
