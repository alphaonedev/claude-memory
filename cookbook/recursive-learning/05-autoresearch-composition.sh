#!/usr/bin/env bash
# cookbook/recursive-learning/05-autoresearch-composition.sh
#
# v0.7.0 Grand-Slam L3-2 (#675) — optional recipe 05 — composition.
#
# What this proves
#   The four primitives demonstrated in recipes 01-04 are *not* four
#   isolated capabilities — they compose into a single end-to-end
#   autoresearch loop:
#
#     synthetic experiment observations
#         → clustering reflection (depth-1)
#         → meta-reflection (depth-2)
#         → skill promotion (Apache-2.0 SKILL.md)
#         → forensic bundle (signed, re-verifiable offline)
#
#   This shape is inspired by Andrej Karpathy's autoresearch framing:
#   an agent runs many small synthetic experiments, the substrate
#   clusters their results into reflections, and the high-confidence
#   reflections crystallise into reusable skills that subsequent
#   experiments can call. The forensic bundle is the audit-grade
#   evidence package for the whole loop.
#
# Attribution
#   The "autoresearch" framing is Karpathy's; see the public
#   discussions on Twitter/X and the underlying RL/agent literature.
#   The substrate primitives (bounded reflection, signed reflects_on
#   edges, skill promotion, forensic bundle) are ai-memory's
#   contributions — see issues #655 / #666 / #670 / #671. The shape
#   of this composition is generic and applies to any autoresearch-
#   style agent; the recipe uses synthetic data so it runs hermetically
#   without an LLM.
#
# What it does
#   1. Carves a fresh sqlite DB under .local-runs/cookbook-05-<ts>/.
#   2. Seeds 6 synthetic "experiment observation" memories (3 in
#      cluster A, 3 in cluster B) — each is a JSON-ish description of
#      an experiment run + outcome.
#   3. Mints two cluster reflections (one per cluster) via
#      memory_reflect.
#   4. Mints one meta-reflection (depth=2) that consolidates the two
#      cluster reflections.
#   5. Promotes the meta-reflection to a SKILL.md via
#      memory_skill_promote_from_reflection (depth-2 → skill).
#   6. Exports a forensic bundle rooted at the meta-reflection.
#   7. Verifies the bundle and asserts a clean verification.
#   8. Verdict block.
#
# Acceptance
#   Exits 0 only when every stage of the composition completes
#   successfully.

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
    exit 64 ;;
esac
TS="$(date +%Y%m%dT%H%M%S)"
RUN_DIR="$DEMO_ROOT/cookbook-05-$TS"
DB="$RUN_DIR/memory.db"
BUNDLE="$RUN_DIR/autoresearch-bundle.tar"
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

# ─── helpers (same shape as recipes 01-04) ──────────────────────────────
mcp_call() {
  local tag="$1" tool="$2" args="$3"
  local mcp_in="$RUN_DIR/mcp-${tag}.in.jsonl"
  local mcp_out="$RUN_DIR/mcp-${tag}.out.jsonl"
  cat >"$mcp_in" <<EOF
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"cookbook-05","version":"1.0"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"${tool}","arguments":${args}}}
EOF
  "$BIN" --db "$DB" mcp --profile full <"$mcp_in" >"$mcp_out" 2>>"$LOG" || true
  awk '/"id":2/' "$mcp_out" | head -n 1
}
parse_field() {
  local raw="$1" field="$2"
  local body unesc
  body="$(printf '%s' "$raw" | sed -nE 's/.*"text":[[:space:]]*"((\\.|[^"\\])*)".*/\1/p' | head -n 1)"
  unesc="$(printf '%s' "$body" | sed -E 's/\\n/\
/g; s/\\"/"/g; s/\\\\/\\/g')"
  printf '%s' "$unesc" \
    | awk -v key="\"$field\"" '
        $0 ~ key {
          sub(/^[^:]*:[[:space:]]*/, "")
          sub(/,[[:space:]]*$/, "")
          if (substr($0,1,1)=="\"") { sub(/^"/, ""); sub(/".*$/, "") }
          gsub(/[[:space:]]+$/, ""); sub(/}$/, "")
          print; exit
        }
      '
}

# ─── 1. Bootstrap ───────────────────────────────────────────────────────
step "1/8  bootstrap demo DB"
"$BIN" --db "$DB" stats >>"$LOG" 2>&1 || true
[[ -f "$DB" ]] || { err "DB not created"; exit 1; }
ok "fresh DB initialised"

# ─── 2. Seed 6 synthetic experiment observations ────────────────────────
NAMESPACE="cookbook-05-autoresearch-$TS"
step "2/8  seed 6 synthetic experiment observations (2 clusters × 3 each)"
EXPERIMENTS_A=(
  "experiment-A-1|batch_size=32 lr=0.001|val_loss=0.182 val_acc=0.943"
  "experiment-A-2|batch_size=64 lr=0.001|val_loss=0.179 val_acc=0.945"
  "experiment-A-3|batch_size=128 lr=0.001|val_loss=0.183 val_acc=0.942"
)
EXPERIMENTS_B=(
  "experiment-B-1|batch_size=32 lr=0.0003|val_loss=0.214 val_acc=0.931"
  "experiment-B-2|batch_size=64 lr=0.0003|val_loss=0.219 val_acc=0.929"
  "experiment-B-3|batch_size=128 lr=0.0003|val_loss=0.222 val_acc=0.927"
)
A_IDS=()
for spec in "${EXPERIMENTS_A[@]}"; do
  IFS='|' read -r title hyper result <<<"$spec"
  json="$("$BIN" --db "$DB" --json store \
    --title "$title" \
    --content "Synthetic autoresearch experiment: hyperparams=[$hyper] result=[$result]." \
    --namespace "$NAMESPACE" --tier mid 2>>"$LOG")"
  id="$(printf '%s' "$json" | sed -E 's/.*"id":[[:space:]]*"([^"]+)".*/\1/' | head -n 1)"
  [[ -n "$id" ]] || { err "store failed ($title)"; exit 1; }
  A_IDS+=("$id")
done
B_IDS=()
for spec in "${EXPERIMENTS_B[@]}"; do
  IFS='|' read -r title hyper result <<<"$spec"
  json="$("$BIN" --db "$DB" --json store \
    --title "$title" \
    --content "Synthetic autoresearch experiment: hyperparams=[$hyper] result=[$result]." \
    --namespace "$NAMESPACE" --tier mid 2>>"$LOG")"
  id="$(printf '%s' "$json" | sed -E 's/.*"id":[[:space:]]*"([^"]+)".*/\1/' | head -n 1)"
  [[ -n "$id" ]] || { err "store failed ($title)"; exit 1; }
  B_IDS+=("$id")
done
ok "seeded ${#A_IDS[@]} (cluster A) + ${#B_IDS[@]} (cluster B) experiments"

# ─── 3. Cluster reflections (depth=1) ──────────────────────────────────
step "3/8  mint cluster reflection over A (lr=0.001) — depth=1"
SRCS_A="[\"${A_IDS[0]}\",\"${A_IDS[1]}\",\"${A_IDS[2]}\"]"
RA_RAW="$(mcp_call "ref-A" "memory_reflect" "{\"source_ids\":$SRCS_A,\"title\":\"cluster-A-summary\",\"content\":\"Cluster A (lr=0.001): val_acc 0.942-0.945 across batch sizes — converges fast, near-flat in batch dim.\",\"namespace\":\"$NAMESPACE\",\"tier\":\"long\"}")"
RA_ID="$(parse_field "$RA_RAW" "id")"
RA_DEPTH="$(parse_field "$RA_RAW" "reflection_depth")"
[[ -n "$RA_ID" && "$RA_DEPTH" == "1" ]] || { err "cluster A reflection failed; raw: $RA_RAW"; exit 1; }
ok "cluster A reflection minted (id=$RA_ID depth=$RA_DEPTH)"

step "    mint cluster reflection over B (lr=0.0003) — depth=1"
SRCS_B="[\"${B_IDS[0]}\",\"${B_IDS[1]}\",\"${B_IDS[2]}\"]"
RB_RAW="$(mcp_call "ref-B" "memory_reflect" "{\"source_ids\":$SRCS_B,\"title\":\"cluster-B-summary\",\"content\":\"Cluster B (lr=0.0003): val_acc 0.927-0.931 across batch sizes — slower, still batch-flat, lower asymptote.\",\"namespace\":\"$NAMESPACE\",\"tier\":\"long\"}")"
RB_ID="$(parse_field "$RB_RAW" "id")"
RB_DEPTH="$(parse_field "$RB_RAW" "reflection_depth")"
[[ -n "$RB_ID" && "$RB_DEPTH" == "1" ]] || { err "cluster B reflection failed; raw: $RB_RAW"; exit 1; }
ok "cluster B reflection minted (id=$RB_ID depth=$RB_DEPTH)"

# ─── 4. Meta-reflection (depth=2) ──────────────────────────────────────
step "4/8  mint meta-reflection consolidating A + B — depth=2"
META_SRCS="[\"$RA_ID\",\"$RB_ID\"]"
RM_RAW="$(mcp_call "ref-meta" "memory_reflect" "{\"source_ids\":$META_SRCS,\"title\":\"autoresearch-meta\",\"content\":\"Across clusters A and B: val_acc is batch-size-insensitive at fixed lr; lr=0.001 dominates lr=0.0003 by ~1.4 points val_acc. Promote lr=0.001 as a default skill heuristic.\",\"namespace\":\"$NAMESPACE\",\"tier\":\"long\"}")"
RM_ID="$(parse_field "$RM_RAW" "id")"
RM_DEPTH="$(parse_field "$RM_RAW" "reflection_depth")"
[[ -n "$RM_ID" && "$RM_DEPTH" == "2" ]] || { err "meta-reflection failed; raw: $RM_RAW"; exit 1; }
ok "meta-reflection minted (id=$RM_ID depth=$RM_DEPTH)"

# ─── 5. Promote meta-reflection → skill ────────────────────────────────
step "5/8  promote meta-reflection → SKILL.md via memory_skill_promote_from_reflection"
SKILL_NAME="lr-default-heuristic"
PROMOTE_ARGS="{\"reflection_id\":\"$RM_ID\",\"skill_name\":\"$SKILL_NAME\",\"skill_description\":\"Autoresearch-derived: prefer lr=0.001 over lr=0.0003 at fixed batch size for this experiment family.\"}"
PROMOTE_RAW="$(mcp_call "promote" "memory_skill_promote_from_reflection" "$PROMOTE_ARGS")"
SKILL_ID="$(parse_field "$PROMOTE_RAW" "skill_id")"
DIGEST="$(parse_field "$PROMOTE_RAW" "digest")"
DEPTH_ORIG="$(parse_field "$PROMOTE_RAW" "original_reflection_depth")"
[[ -n "$SKILL_ID" && -n "$DIGEST" ]] || { err "promotion failed; raw: $PROMOTE_RAW"; exit 1; }
info "skill_id                    = $SKILL_ID"
info "digest                      = $DIGEST"
info "original_reflection_depth   = $DEPTH_ORIG"
ok "depth-2 meta-reflection promoted to SKILL.md"

# ─── 6. Forensic bundle rooted at the meta-reflection ──────────────────
step "6/8  export forensic bundle rooted at the meta-reflection"
EXPORT_LOG="$RUN_DIR/export.log"
if ! "$BIN" --db "$DB" export-forensic-bundle \
    --memory-id "$RM_ID" \
    --include-reflections \
    --output "$BUNDLE" \
    >"$EXPORT_LOG" 2>>"$LOG"; then
  err "export-forensic-bundle exited non-zero; see $EXPORT_LOG and $LOG"
  exit 3
fi
[[ -f "$BUNDLE" ]] || { err "bundle file not written"; exit 3; }
SIZE="$(stat -f%z "$BUNDLE" 2>/dev/null || stat -c%s "$BUNDLE" 2>/dev/null)"
info "bundle written ($SIZE bytes)"
ok "forensic bundle assembled"

# ─── 7. Verify bundle ──────────────────────────────────────────────────
step "7/8  verify-forensic-bundle"
VERIFY_LOG="$RUN_DIR/verify.log"
if "$BIN" --db "$DB" verify-forensic-bundle "$BUNDLE" >"$VERIFY_LOG" 2>>"$LOG"; then
  if grep -q "verification OK" "$VERIFY_LOG"; then
    ok "bundle verifies cleanly"
  else
    err "verify exited 0 but no 'verification OK' line"
    exit 4
  fi
else
  err "verify exited non-zero on a clean bundle; see $VERIFY_LOG"
  exit 4
fi

# ─── 8. Verdict ────────────────────────────────────────────────────────
step "8/8  verdict"
echo
printf "%s+-- v0.7.0 autoresearch composition -- reproduction verdict --+%s\n" "$BOLD" "$RESET"
printf "%s| db                       %s| %s\n" "$BOLD" "$RESET" "$DB"
printf "%s| experiments (A + B)      %s| %d + %d\n" "$BOLD" "$RESET" "${#A_IDS[@]}" "${#B_IDS[@]}"
printf "%s| cluster A reflection     %s| id=%s (depth=1)\n" "$BOLD" "$RESET" "$RA_ID"
printf "%s| cluster B reflection     %s| id=%s (depth=1)\n" "$BOLD" "$RESET" "$RB_ID"
printf "%s| meta-reflection          %s| id=%s (depth=2)\n" "$BOLD" "$RESET" "$RM_ID"
printf "%s| promoted skill           %s| %s\n" "$BOLD" "$RESET" "$SKILL_ID"
printf "%s| skill digest             %s| %s\n" "$BOLD" "$RESET" "$DIGEST"
printf "%s| bundle                   %s| %s (%s bytes)\n" "$BOLD" "$RESET" "$BUNDLE" "$SIZE"
printf "%s| bundle verify            %s| OK\n" "$BOLD" "$RESET"
printf "%s+--------------------------------------------------------------+%s\n" "$BOLD" "$RESET"
echo

ok "Recipe 05 — autoresearch composition reproduced end-to-end."
info "The full loop: synthetic experiments → cluster reflections → meta-reflection → skill"
info "→ forensic bundle, with every artefact cryptographically traceable to the underlying"
info "experiment observations. This is the canonical shape of substrate-supported"
info "autoresearch."
if [[ "${COOKBOOK_KEEP_DB:-0}" != "1" ]]; then
  rm -rf "$RUN_DIR"
  info "cleaned up $RUN_DIR (set COOKBOOK_KEEP_DB=1 to retain)"
fi
