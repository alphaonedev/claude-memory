#!/usr/bin/env bash
# batman-warmup.sh — exercise the Batman pipeline so the empty
# substrate tables (confidence_shadow_observations, decayed memories)
# fill with real data instead of staying structurally-active /
# behaviorally-empty (#800 Crack 3).
#
# What it does:
#   1. Stores N test memories of varying lengths under --namespace.
#      Long enough to trip Form 2 atomise-before-embed
#      (--auto_atomise_threshold_cl100k default 512); content shaped
#      so Form 6 regex_then_llm classify lands a Concept or Claim,
#      not just Observation.
#   2. Recalls each test memory so Form 5 freshness-decay touches
#      `confidence_decayed_at` and Form 5 shadow records a side-by-
#      side observation in `confidence_shadow_observations` (if
#      shadow_mode env vars are wired).
#   3. Triggers `ai-memory curator --once` so any deferred Form 1
#      sweeps + Form 5 calibration baseline derivations run NOW
#      instead of waiting for the next 300s tick.
#   4. Reports row counts on the populated tables so the operator
#      can compare "before" and "after".
#
# After this script, scripts/batman-mode-acceptance.sh --behavioral
# should observe non-zero rows in:
#   - confidence_shadow_observations (if env vars wired)
#   - memories.confidence_decayed_at IS NOT NULL (after some idle time)
#   - memories.atom_of IS NOT NULL (atoms from Form 2)
#   - memories.memory_kind != 'observation' (after Form 6 classify)
#   - signed_events (Form 1 / Form 7 verdicts)
#
# Usage:
#   scripts/batman-warmup.sh                      # 5 memories in 'main'
#   scripts/batman-warmup.sh --count 20
#   scripts/batman-warmup.sh --namespace ai-memory-mcp --count 10
#   scripts/batman-warmup.sh --db /path/to.db
#
# Scratch convention: writes nothing outside the repo's .local-runs/.

set -uo pipefail

DB="${AI_MEMORY_DB:-$HOME/.claude/ai-memory.db}"
NAMESPACE="${BATMAN_NAMESPACE:-main}"
COUNT=5

while [[ $# -gt 0 ]]; do
    case "$1" in
        --db)        DB="$2"; shift 2 ;;
        --namespace) NAMESPACE="$2"; shift 2 ;;
        --count)     COUNT="$2"; shift 2 ;;
        -h|--help)
            sed -n '2,35p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

step()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
info()  { printf '    %s\n' "$*"; }
ok()    { printf '    \033[32m✓\033[0m %s\n' "$*"; }

sql() { sqlite3 "$DB" "$1" 2>/dev/null; }

# ---------------------------------------------------------------- before ---

step "Before: substrate behavioral state"
BEFORE_SHADOW=$(sql "SELECT COUNT(*) FROM confidence_shadow_observations WHERE namespace='$NAMESPACE';")
BEFORE_DECAYED=$(sql "SELECT COUNT(*) FROM memories WHERE namespace='$NAMESPACE' AND confidence_decayed_at IS NOT NULL;")
BEFORE_ATOMS=$(sql "SELECT COUNT(*) FROM memories WHERE namespace='$NAMESPACE' AND atom_of IS NOT NULL;")
BEFORE_KINDS=$(sql "SELECT memory_kind, COUNT(*) FROM memories WHERE namespace='$NAMESPACE' GROUP BY memory_kind;" | tr '\n' ',' | sed 's/,$//')
BEFORE_SIGNED=$(sql "SELECT COUNT(*) FROM signed_events;")
info "shadow observations:      $BEFORE_SHADOW"
info "decayed memories:         $BEFORE_DECAYED"
info "atom rows:                $BEFORE_ATOMS"
info "memory_kinds in '$NAMESPACE': ${BEFORE_KINDS:-none}"
info "signed_events:            $BEFORE_SIGNED"

# ----------------------------------------------------------- store N ---

step "Storing $COUNT warmup memories in '$NAMESPACE'"

# Test content shapes — variety to exercise Form 6 regex classifier.
declare -a SHAPES=(
    "Concept:Definition of write-time investment: cognitive transforms applied to a memory BEFORE the row hits SQL, including dedup-and-synthesis, atomise-before-embed, multi-step ingest, fact-provenance capture, confidence calibration, type tagging, and substrate-authority enforcement. Six write-side transforms set the recall ceiling that no rerank can fix downstream."
    "Claim:The ai-memory v0.7.0 substrate implements 6 of 6 Batman write-time-investment forms plus a 7th (substrate-authority at write), all IMPLEMENTED per the post-closeout state at HEAD d17725c per docs/internal/batman-framework-audit.md."
    "Entity:The Claude Opus 4.7 model with 1M context, running the PR #753 adversarial procurement-grade audit of the Batman framework against ai-memory v0.7.0 commit 53b4d39, which classified 0/6 cleanly IMPLEMENTED + 4 PARTIAL + 2 ABSENT and fired escalation Trigger 1."
    "Event:On 2026-05-15 the v0.7.0 ship campaign closed at 100% GREEN (issue #700) after PRs #761-#766 closed every Batman gap the audit flagged, lifting the substrate to 6/6 + 7th-form all IMPLEMENTED before tag-cut."
    "Decision:Default state of a fresh v0.7.0 install is Batman-capable, not Batman-active — operator must run the 7-step recipe (operator keygen → sign-seed → enable R001-R004 → curator daemon → optional reflection-pass → namespace standard memory → Form 5 env vars) to flip the substrate from substrate-ready to behavior-active."
    "Relation:GovernancePolicy.auto_atomise_mode='synchronous' implies Form 2 atomise-before-embed fires on every memory_store above auto_atomise_threshold_cl100k tokens, which means atoms get vectors at the addressable granularity Batman Form 4 fact-provenance requires."
    "Conversation:Operator asked 'is this in autonomous mode' — the response surfaced that --tier autonomous was pinned at MCP launch via .claude.json, that the profile = full via config.toml, and that every recall/store/search this session uses the nomic embedder + MiniLM cross-encoder + gemma4:e4b LLM stack."
    "Observation:The keygen↔enable path mismatch wart: ai-memory rules keygen writes the operator key to <config-dir>/operator.key but ai-memory rules enable looks in <config-dir>/keys/operator.key — a one-line UX bug the Batman activation recipe documents and the install-batman-active.sh script works around with a mv."
    "Persona:Honest engineering operator who runs adversarial verification before claiming substrate compliance, biases lower on uncertainty, and treats marketing overcounting (X-post claim '5 of Batman's 6 forms plus a 7th' when audit said 0/6 + PARTIAL 7th) as a procurement-grade finding that demands code-evidence closure, not narrative correction."
    "Reflection:The audit-then-close-the-gaps cycle that PR #753 ran on v0.7.0 is the same dynamic the post-acceptance suite drove on docs/batman-active-mode.md — write a test against the actual schema, watch it fail on aspirational claims, correct the docs in the same commit. Adversarial verification beats aspirational shipping at every grain."
)

I=0
while [[ $I -lt $COUNT ]]; do
    SHAPE="${SHAPES[$((I % ${#SHAPES[@]}))]}"
    KIND_HINT="${SHAPE%%:*}"
    CONTENT="${SHAPE#*:}"
    TITLE="batman warmup #$((I+1)) ($KIND_HINT)"
    OUT=$(ai-memory --db "$DB" store \
        --namespace "$NAMESPACE" \
        --tier mid \
        --title "$TITLE" \
        --content "$CONTENT" \
        --json 2>&1 | grep -vE '^ai-memory: loaded config' | tail -1)
    ID=$(echo "$OUT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    print(d.get('id') or d.get('memory_id') or d.get('memory', {}).get('id', ''))
except: print('')
")
    if [[ -n "$ID" ]]; then
        ok "stored $ID — '$TITLE'"
    else
        info "store failed for #$((I+1)): $(echo "$OUT" | head -1)"
    fi
    I=$((I + 1))
done

# ----------------------------------------------------------- recall ---

step "Recalling to trigger Form 5 freshness-decay touches"
RECALL_OUT=$(ai-memory --db "$DB" recall "batman warmup write-time-investment substrate" \
    --namespace "$NAMESPACE" --limit "$COUNT" 2>&1 | grep -vE '^ai-memory: loaded config' | head -20)
ok "recall fired"

# ----------------------------------------------------------- curator pass ---

step "Curator --once sweep (Form 1 background + Form 5 calibration + Form 6 backfill)"
ai-memory --db "$DB" curator --once --include-namespace "$NAMESPACE" --max-ops 50 2>&1 \
    | grep -vE '^ai-memory: loaded config' | head -10
ok "curator sweep complete"

# ---------------------------------------------------------------- after ----

step "After: substrate behavioral state"
AFTER_SHADOW=$(sql "SELECT COUNT(*) FROM confidence_shadow_observations WHERE namespace='$NAMESPACE';")
AFTER_DECAYED=$(sql "SELECT COUNT(*) FROM memories WHERE namespace='$NAMESPACE' AND confidence_decayed_at IS NOT NULL;")
AFTER_ATOMS=$(sql "SELECT COUNT(*) FROM memories WHERE namespace='$NAMESPACE' AND atom_of IS NOT NULL;")
AFTER_KINDS=$(sql "SELECT memory_kind, COUNT(*) FROM memories WHERE namespace='$NAMESPACE' GROUP BY memory_kind;" | tr '\n' ',' | sed 's/,$//')
AFTER_SIGNED=$(sql "SELECT COUNT(*) FROM signed_events;")

delta() {
    local before=$1 after=$2 label=$3
    local d=$((after - before))
    if [[ $d -gt 0 ]]; then
        ok "$label: $before → $after  (+$d)"
    elif [[ $d -lt 0 ]]; then
        info "$label: $before → $after  ($d)"
    else
        info "$label: $after  (unchanged)"
    fi
}

delta $BEFORE_SHADOW $AFTER_SHADOW  "shadow observations"
delta $BEFORE_DECAYED $AFTER_DECAYED "decayed memories"
delta $BEFORE_ATOMS $AFTER_ATOMS    "atom rows"
delta $BEFORE_SIGNED $AFTER_SIGNED  "signed_events"
info ""
info "memory_kinds in '$NAMESPACE' BEFORE: ${BEFORE_KINDS:-none}"
info "memory_kinds in '$NAMESPACE' AFTER:  ${AFTER_KINDS:-none}"
info ""
info "Run scripts/batman-mode-acceptance.sh --behavioral to assert the pipeline fired."
