#!/usr/bin/env bash
# cookbook/recursive-learning/02-curator-driven-reflection.sh
#
# v0.7.0 Grand-Slam L3-2 (#675) — recipe 02.
#
# What this proves
#   The curator-driven reflection pass (L2-1, issue #666) wires the
#   substrate primitive demonstrated by recipe 01 into a single
#   operator-friendly CLI verb. The pass clusters co-recalled
#   Observations in a namespace and synthesises typed Reflection
#   memories with signed `reflects_on` provenance — every minted
#   reflection is walkable via the same `verify-reflection-chain`
#   verifier from L1-3.
#
# CI / smoke note
#   `ai-memory curator --reflect` requires a configured LLM (autonomous
#   tier). In the hermetic cookbook environment we run with
#   AI_MEMORY_NO_CONFIG=1 — the pass cleanly reports "no LLM client
#   configured" in its structured JSON report and exits 0 (operator-
#   actionable surface, not a crash). The recipe then mints two real
#   `reflects_on`-bearing reflections via `memory_reflect` over MCP and
#   runs `verify-reflection-chain` on each, demonstrating the *same*
#   signed-edge audit surface the curator daemon produces in
#   production. This is the "inspect + verify each" loop in the L3-2
#   spec — adapted to be runnable without an Ollama dependency in CI.
#
# What it does
#   1. Carves a fresh sqlite DB under .local-runs/cookbook-02-<ts>/.
#   2. Seeds 6 depth-0 observations in two namespaces (3 each) to
#      simulate two distinct clusters the pass would mint reflections
#      over.
#   3. Runs `ai-memory curator --reflect --dry-run --namespace …` and
#      asserts the structured report includes the no-LLM diagnostic
#      under `.errors[0]`.
#   4. Mints two depth-1 reflections (one per namespace) via MCP
#      `memory_reflect` to stand in for what the curator would mint
#      with an LLM available.
#   5. Runs `ai-memory verify-reflection-chain` on each, asserting
#      `chain_depth=1` and `edges_failed=0`.
#   6. Inspects the result memories via `ai-memory get` and prints
#      a verdict block.
#
# Acceptance
#   Exits 0 only when curator dry-run reports correctly, both manually-
#   minted reflections verify cleanly, and `get` returns the
#   `reflection_depth=1` field on each. Exits >0 otherwise.
#
# Hard rules
#   - No /tmp / /var/tmp / /private/tmp writes (project HARD RULE).
#   - Idempotent: every run uses a fresh timestamped subdir.

set -euo pipefail

BOLD=$'\033[1m'; DIM=$'\033[2m'; RED=$'\033[31m'; GREEN=$'\033[32m'
YELLOW=$'\033[33m'; RESET=$'\033[0m'
if [[ ! -t 1 ]] || [[ "${NO_COLOR:-}" == "1" ]]; then
  BOLD="" DIM="" RED="" GREEN="" YELLOW="" RESET=""
fi
step() { printf "%s==> %s%s\n" "$BOLD" "$*" "$RESET"; }
info() { printf "    %s%s%s\n" "$DIM" "$*" "$RESET"; }
ok()   { printf "    %s%s OK%s\n" "$GREEN" "$*" "$RESET"; }
warn() { printf "    %s%s%s\n" "$YELLOW" "$*" "$RESET"; }
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
RUN_DIR="$DEMO_ROOT/cookbook-02-$TS"
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
export AI_MEMORY_NO_CONFIG=1

# ─── 1. Bootstrap ───────────────────────────────────────────────────────
step "1/6  bootstrap demo DB"
"$BIN" --db "$DB" stats >>"$LOG" 2>&1 || true
[[ -f "$DB" ]] || { err "DB not created at $DB"; exit 1; }
ok "fresh DB initialised"

# ─── 2. Seed 6 observations into 2 namespaces ───────────────────────────
NS_A="cookbook/curator-A-$TS"
NS_B="cookbook/curator-B-$TS"
step "2/6  seed 6 depth-0 observations across two namespaces ($NS_A, $NS_B)"
declare -a A_IDS B_IDS
for i in 1 2 3; do
  json="$("$BIN" --db "$DB" --json store \
    --title "obs-A-$i" \
    --content "Cluster-A observation $i: distributed agents converge on shared bounded primitives." \
    --namespace "$NS_A" --tier mid 2>>"$LOG")"
  id="$(printf '%s' "$json" | sed -E 's/.*"id":[[:space:]]*"([^"]+)".*/\1/' | head -n 1)"
  [[ -n "$id" ]] || { err "store failed (A-$i)"; exit 1; }
  A_IDS+=("$id")
done
for i in 1 2 3; do
  json="$("$BIN" --db "$DB" --json store \
    --title "obs-B-$i" \
    --content "Cluster-B observation $i: signed reflects_on edges remain verifiable across export." \
    --namespace "$NS_B" --tier mid 2>>"$LOG")"
  id="$(printf '%s' "$json" | sed -E 's/.*"id":[[:space:]]*"([^"]+)".*/\1/' | head -n 1)"
  [[ -n "$id" ]] || { err "store failed (B-$i)"; exit 1; }
  B_IDS+=("$id")
done
ok "seeded 3 observations in $NS_A and 3 in $NS_B"

# ─── 3. Curator --reflect --dry-run (no-LLM diagnostic) ────────────────
step "3/6  curator --reflect --dry-run --namespace $NS_A (hermetic / no-LLM diagnostic)"
CURATOR_OUT="$RUN_DIR/curator-reflect-dryrun.json"
if ! "$BIN" --db "$DB" curator --reflect --dry-run --namespace "$NS_A" --json >"$CURATOR_OUT" 2>>"$LOG"; then
  err "curator --reflect --dry-run exited non-zero; see $CURATOR_OUT and $LOG"
  exit 3
fi
info "report → $CURATOR_OUT"
# Acceptance: report parses as JSON-ish, includes a structured 'errors'
# array with the no-LLM diagnostic OR proceeds with proposals when an
# LLM is configured. Both outcomes count as a clean run.
if grep -q "no LLM client configured" "$CURATOR_OUT"; then
  ok "no-LLM diagnostic surfaced cleanly in structured report"
  info "(when an LLM is wired in production, the curator mints dry-run proposals here)"
elif grep -q '"dry_run_proposals"' "$CURATOR_OUT"; then
  ok "curator emitted dry-run proposals (LLM is configured)"
else
  err "curator report missing expected fields; head:"
  head -c 400 "$CURATOR_OUT" >&2
  exit 3
fi

# ─── reflect_step helper (same as recipe 01) ────────────────────────────
reflect_step() {
  local call_id="$1" srcs_json="$2" title="$3" content="$4" ns="$5"
  local mcp_in="$RUN_DIR/mcp-reflect-${call_id}.in.jsonl"
  local mcp_out="$RUN_DIR/mcp-reflect-${call_id}.out.jsonl"
  cat >"$mcp_in" <<EOF
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"cookbook-02","version":"1.0"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_reflect","arguments":{"source_ids":${srcs_json},"title":"${title}","content":"${content}","namespace":"${ns}","tier":"long"}}}
EOF
  "$BIN" --db "$DB" mcp --profile full <"$mcp_in" >"$mcp_out" 2>>"$LOG" || true
  awk '/"id":2/' "$mcp_out" | head -n 1
}
parse_field() {
  local raw="$1" field="$2"
  local body unesc
  body="$(printf '%s' "$raw" | sed -nE 's/.*"text":[[:space:]]*"((\\.|[^"\\])*)".*/\1/p' | head -n 1)"
  unesc="$(printf '%s' "$body" | sed -E 's/\\"/"/g; s/\\\\/\\/g')"
  printf '%s' "$unesc" | sed -E "s/.*\"${field}\":[[:space:]]*\"?([^,\"}]+).*/\1/" | head -n 1
}

# ─── 4. Mint one depth-1 reflection per namespace ──────────────────────
step "4/6  mint depth-1 reflection in $NS_A (stand-in for what curator would synthesise)"
SRCS_A="[\"${A_IDS[0]}\",\"${A_IDS[1]}\",\"${A_IDS[2]}\"]"
RA_RAW="$(reflect_step 1 "$SRCS_A" "reflection-A" "Cluster A converges on bounded primitives." "$NS_A")"
RA_ID="$(parse_field "$RA_RAW" "id")"
RA_DEPTH="$(parse_field "$RA_RAW" "reflection_depth")"
[[ -n "$RA_ID" && "$RA_DEPTH" == "1" ]] || { err "depth-1 reflection (A) failed; raw: $RA_RAW"; exit 1; }
ok "minted reflection in $NS_A → id=$RA_ID"

step "    mint depth-1 reflection in $NS_B"
SRCS_B="[\"${B_IDS[0]}\",\"${B_IDS[1]}\",\"${B_IDS[2]}\"]"
RB_RAW="$(reflect_step 2 "$SRCS_B" "reflection-B" "Cluster B's reflects_on edges remain verifiable across export." "$NS_B")"
RB_ID="$(parse_field "$RB_RAW" "id")"
RB_DEPTH="$(parse_field "$RB_RAW" "reflection_depth")"
[[ -n "$RB_ID" && "$RB_DEPTH" == "1" ]] || { err "depth-1 reflection (B) failed; raw: $RB_RAW"; exit 1; }
ok "minted reflection in $NS_B → id=$RB_ID"

# ─── 5. Inspect + verify each minted reflection ─────────────────────────
inspect_and_verify() {
  local refl_id="$1" label="$2"
  step "    inspect $label reflection ($refl_id)"
  local get_json
  get_json="$("$BIN" --db "$DB" --json get "$refl_id" 2>>"$LOG")"
  local depth kind
  depth="$(printf '%s' "$get_json" | sed -nE 's/.*"reflection_depth"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p' | head -n 1)"
  kind="$(printf '%s' "$get_json" | sed -nE 's/.*"memory_kind"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' | head -n 1)"
  info "memory_kind=$kind reflection_depth=$depth"
  if [[ "$depth" != "1" ]]; then
    err "$label inspect: reflection_depth != 1 (got '$depth')"
    return 1
  fi
  ok "$label inspect passes"

  step "    verify-reflection-chain on $label ($refl_id)"
  local vfile="$RUN_DIR/verify-${label}.json"
  if ! "$BIN" --db "$DB" verify-reflection-chain --format json "$refl_id" >"$vfile" 2>>"$LOG"; then
    err "verify-reflection-chain ($label) exited non-zero; see $vfile"
    return 1
  fi
  local cd ef ev
  cd="$(sed -nE 's/.*"chain_depth"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p' "$vfile" | head -n 1)"
  ef="$(sed -nE 's/.*"edges_failed"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p' "$vfile" | head -n 1)"
  ev="$(sed -nE 's/.*"edges_verified"[[:space:]]*:[[:space:]]*([0-9]+).*/\1/p' "$vfile" | head -n 1)"
  info "chain_depth=$cd edges_verified=$ev edges_failed=$ef"
  if [[ "$cd" != "1" || "$ef" != "0" || "${ev:-0}" -lt 1 ]]; then
    err "$label verify failed"
    return 1
  fi
  ok "$label chain integrity verified"
  return 0
}

step "5/6  inspect + verify each minted reflection"
VERIFY_OK=1
inspect_and_verify "$RA_ID" "A" || VERIFY_OK=0
inspect_and_verify "$RB_ID" "B" || VERIFY_OK=0

# ─── 6. Verdict ─────────────────────────────────────────────────────────
step "6/6  verdict"
echo
printf "%s+-- v0.7.0 curator-driven reflection -- reproduction verdict --+%s\n" "$BOLD" "$RESET"
printf "%s| db                      %s| %s\n" "$BOLD" "$RESET" "$DB"
printf "%s| namespaces seeded       %s| %s, %s\n" "$BOLD" "$RESET" "$NS_A" "$NS_B"
printf "%s| curator --reflect dry   %s| OK (structured report)\n" "$BOLD" "$RESET"
printf "%s| reflection A            %s| id=%s\n" "$BOLD" "$RESET" "$RA_ID"
printf "%s| reflection B            %s| id=%s\n" "$BOLD" "$RESET" "$RB_ID"
printf "%s| inspect + verify (A,B)  %s| %s\n" "$BOLD" "$RESET" "$([[ $VERIFY_OK == 1 ]] && echo OK || echo FAIL)"
printf "%s+----------------------------------------------------------------+%s\n" "$BOLD" "$RESET"
echo

if [[ "$VERIFY_OK" != "1" ]]; then
  err "Recipe 02 FAILED — inspect/verify did not green for one or more reflections."
  exit 2
fi

ok "Recipe 02 — curator-driven reflection (smoke variant) reproduced end-to-end."
info "Run with a configured Ollama LLM and AI_MEMORY_NO_CONFIG unset to exercise the full"
info "curator clustering path; the structured report's 'dry_run_proposals' / 'reflections_persisted'"
info "fields then populate."
if [[ "${COOKBOOK_KEEP_DB:-0}" != "1" ]]; then
  rm -rf "$RUN_DIR"
  info "cleaned up $RUN_DIR (set COOKBOOK_KEEP_DB=1 to retain)"
fi
