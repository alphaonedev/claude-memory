#!/usr/bin/env bash
# cookbook/recursive-learning/03-reflection-to-skill-promote.sh
#
# v0.7.0 Grand-Slam L3-2 (#675) — recipe 03 — the keystone.
#
# What this proves
#   The closing loop of recursive learning: a Reflection-kind memory
#   (synthesised by `memory_reflect`) promotes into an Apache-2.0
#   Agent Skill via `memory_skill_promote_from_reflection`. The
#   constructed skill exports to a SKILL.md folder via
#   `memory_skill_export`. Re-registering that folder via
#   `memory_skill_register` produces a **BYTE-IDENTICAL SHA-256
#   digest** — the keystone acceptance contract for L2-6 (#671).
#
#   When the third-party `skills-ref` validator is on PATH the recipe
#   ALSO runs `skills-ref validate <export-folder>` and fails the run
#   if the validator rejects. When absent it logs a SKIP. Mirrors the
#   same convention `tests/skill_test.rs` follows for L1-5.
#
# What it does
#   1. Carves a fresh sqlite DB under .local-runs/cookbook-03-<ts>/.
#   2. Stores 3 source observations.
#   3. Drives `memory_reflect` over MCP to produce a depth=1 reflection
#      over the 3 sources.
#   4. Drives `memory_skill_promote_from_reflection` over MCP to
#      promote the reflection into a skill; captures `skill_id` and
#      `digest` from the envelope.
#   5. Drives `memory_skill_export` over MCP to write the skill to
#      `$RUN_DIR/exported-skill/`; asserts the export digest matches
#      the promotion digest.
#   6. If `skills-ref` is on PATH: runs `skills-ref validate
#      <export>`; fails the run on rejection.
#   7. Drives `memory_skill_register` over MCP on a SECOND fresh DB
#      (`memory-2.db`) so the (namespace, name) collision doesn't
#      trigger a supersession; captures the re-registered digest.
#   8. Asserts re-registered digest == promotion digest
#      (round-trip).
#   9. Prints a verdict block.
#
# Acceptance
#   Exits 0 only when the promotion succeeds, the export succeeds with
#   matching digest, optional skills-ref validation passes when
#   installed, AND the re-registration on the fresh DB produces a
#   byte-identical digest.

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
RUN_DIR="$DEMO_ROOT/cookbook-03-$TS"
DB1="$RUN_DIR/memory.db"
DB2="$RUN_DIR/memory-2.db"
EXPORT_DIR="$RUN_DIR/exported-skill"
LOG="$RUN_DIR/run.log"
mkdir -p "$RUN_DIR"

BIN="${AI_MEMORY_BIN:-$(command -v ai-memory || true)}"
if [[ -z "$BIN" ]] || [[ ! -x "$BIN" ]]; then
  err "ai-memory binary not found. Set AI_MEMORY_BIN=<path> or put 'ai-memory' on PATH."
  exit 65
fi
info "binary: $BIN"
info "db (1): $DB1"
info "db (2): $DB2"
info "log:    $LOG"
export AI_MEMORY_NO_CONFIG=1

# ─── helper: drive a single MCP tools/call against a chosen DB ─────────
# Usage: mcp_call <db-path> <tag> <tool_name> <json_args> -> raw response body line
mcp_call() {
  local db="$1" tag="$2" tool="$3" args="$4"
  local mcp_in="$RUN_DIR/mcp-${tag}.in.jsonl"
  local mcp_out="$RUN_DIR/mcp-${tag}.out.jsonl"
  cat >"$mcp_in" <<EOF
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"cookbook-03","version":"1.0"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"${tool}","arguments":${args}}}
EOF
  "$BIN" --db "$db" mcp --profile full <"$mcp_in" >"$mcp_out" 2>>"$LOG" || true
  awk '/"id":2/' "$mcp_out" | head -n 1
}

# Parse a JSON field out of the MCP tools/call response body
# (the body is a stringified JSON inside content[0].text). The inner
# JSON is pretty-printed with literal `\n` escapes; we convert those to
# real newlines so each field sits on its own line and the per-field
# regex captures cleanly.
parse_field() {
  local raw="$1" field="$2"
  local body unesc
  body="$(printf '%s' "$raw" | sed -nE 's/.*"text":[[:space:]]*"((\\.|[^"\\])*)".*/\1/p' | head -n 1)"
  # Un-escape: \n -> newline, \" -> ", \\ -> \
  unesc="$(printf '%s' "$body" | sed -E 's/\\n/\
/g; s/\\"/"/g; s/\\\\/\\/g')"
  printf '%s' "$unesc" \
    | awk -v key="\"$field\"" '
        $0 ~ key {
          # Strip leading text up through the colon.
          sub(/^[^:]*:[[:space:]]*/, "")
          # Strip trailing comma if present.
          sub(/,[[:space:]]*$/, "")
          # Strip surrounding double-quotes if present.
          if (substr($0,1,1)=="\"") {
            sub(/^"/, "")
            sub(/".*$/, "")
          }
          # Strip a trailing }/whitespace tail leftover from the
          # final key in the object.
          gsub(/[[:space:]]+$/, "")
          sub(/}$/, "")
          print
          exit
        }
      '
}

# ─── 1. Bootstrap DB 1 ──────────────────────────────────────────────────
step "1/9  bootstrap demo DB"
"$BIN" --db "$DB1" stats >>"$LOG" 2>&1 || true
[[ -f "$DB1" ]] || { err "DB1 not created"; exit 1; }
ok "fresh DB initialised: $DB1"

# ─── 2. Seed 3 observations ─────────────────────────────────────────────
NAMESPACE="cookbook-03-$TS"
step "2/9  store 3 source observations in $NAMESPACE"
SRC_IDS=()
for i in 1 2 3; do
  json="$("$BIN" --db "$DB1" --json store \
    --title "src-$i" \
    --content "Observation $i: substrate-bounded reflection promotes into a reusable skill via L2-6." \
    --namespace "$NAMESPACE" --tier mid 2>>"$LOG")"
  id="$(printf '%s' "$json" | sed -E 's/.*"id":[[:space:]]*"([^"]+)".*/\1/' | head -n 1)"
  [[ -n "$id" ]] || { err "store failed (src-$i)"; exit 1; }
  SRC_IDS+=("$id")
done
ok "seeded 3 source observations"

# ─── 3. Mint depth=1 reflection ─────────────────────────────────────────
step "3/9  reflect over 3 sources → depth=1"
SRCS_JSON="[\"${SRC_IDS[0]}\",\"${SRC_IDS[1]}\",\"${SRC_IDS[2]}\"]"
REFL_ARGS='{"source_ids":'$SRCS_JSON',"title":"closing-loop-reflection","content":"Closing-loop pattern: bounded reflection synthesises into a reusable Apache-2.0 Agent Skill.","namespace":"'$NAMESPACE'","tier":"long"}'
REFL_RAW="$(mcp_call "$DB1" "reflect" "memory_reflect" "$REFL_ARGS")"
REFL_ID="$(parse_field "$REFL_RAW" "id")"
REFL_DEPTH="$(parse_field "$REFL_RAW" "reflection_depth")"
if [[ -z "$REFL_ID" || "$REFL_DEPTH" != "1" ]]; then
  err "depth-1 reflection failed; raw: $REFL_RAW"
  exit 1
fi
ok "minted depth-1 reflection id=$REFL_ID"

# ─── 4. Promote reflection → skill ──────────────────────────────────────
step "4/9  promote reflection → skill via memory_skill_promote_from_reflection"
SKILL_NAME="cookbook-pattern"
PROMOTE_ARGS='{"reflection_id":"'"$REFL_ID"'","skill_name":"'"$SKILL_NAME"'","skill_description":"Cookbook recipe 03: reflection-to-skill closing-loop demonstration."}'
PROMOTE_RAW="$(mcp_call "$DB1" "promote" "memory_skill_promote_from_reflection" "$PROMOTE_ARGS")"
SKILL_ID="$(parse_field "$PROMOTE_RAW" "skill_id")"
DIGEST_PROMOTED="$(parse_field "$PROMOTE_RAW" "digest")"
SOURCES_ATTACHED="$(parse_field "$PROMOTE_RAW" "sources_attached")"
DERIVED_FROM="$(parse_field "$PROMOTE_RAW" "derived_from_reflection_id")"
if [[ -z "$SKILL_ID" || -z "$DIGEST_PROMOTED" ]]; then
  err "promotion failed; raw: $PROMOTE_RAW"
  exit 1
fi
info "skill_id=$SKILL_ID"
info "digest (promoted)        = $DIGEST_PROMOTED"
info "sources_attached         = $SOURCES_ATTACHED"
info "derived_from_reflection  = $DERIVED_FROM"
if [[ "$SOURCES_ATTACHED" != "3" ]]; then
  err "expected sources_attached=3, got '$SOURCES_ATTACHED'"
  exit 1
fi
if [[ "$DERIVED_FROM" != "$REFL_ID" ]]; then
  err "derived_from_reflection_id mismatch: got '$DERIVED_FROM' want '$REFL_ID'"
  exit 1
fi
ok "promotion produced skill with 3 attached source references"

# ─── 5. Export skill → folder ───────────────────────────────────────────
step "5/9  export skill → $EXPORT_DIR"
mkdir -p "$EXPORT_DIR"
EXPORT_ARGS='{"skill_id":"'"$SKILL_ID"'","target_folder":"'"$EXPORT_DIR"'"}'
EXPORT_RAW="$(mcp_call "$DB1" "export" "memory_skill_export" "$EXPORT_ARGS")"
DIGEST_EXPORTED="$(parse_field "$EXPORT_RAW" "digest")"
if [[ -z "$DIGEST_EXPORTED" ]]; then
  err "export failed; raw: $EXPORT_RAW"
  exit 1
fi
info "digest (exported)        = $DIGEST_EXPORTED"
if [[ "$DIGEST_EXPORTED" != "$DIGEST_PROMOTED" ]]; then
  err "exported digest != promoted digest"
  exit 1
fi
# Check expected artefacts exist
for required in "SKILL.md" "resources/references/source_0.md" "resources/references/source_1.md" "resources/references/source_2.md"; do
  if [[ ! -f "$EXPORT_DIR/$required" ]]; then
    err "expected exported artefact missing: $EXPORT_DIR/$required"
    exit 1
  fi
done
ok "export produced SKILL.md + 3 reference resources with matching digest"

# ─── 6. Optional: skills-ref validate ───────────────────────────────────
step "6/9  (optional) skills-ref validate"
SKILLS_REF_OK="SKIP"
if command -v skills-ref >/dev/null 2>&1; then
  if skills-ref validate "$EXPORT_DIR" >>"$LOG" 2>&1; then
    SKILLS_REF_OK="PASS"
    ok "skills-ref validate passed"
  else
    err "skills-ref validate REJECTED the promoted skill; see $LOG"
    exit 4
  fi
else
  warn "skills-ref not on PATH — skipped (mirrors tests/skill_test.rs L1-5 pattern)"
fi

# ─── 7. Re-register on a fresh DB ───────────────────────────────────────
step "7/9  re-register exported folder on a fresh DB (DB2)"
"$BIN" --db "$DB2" stats >>"$LOG" 2>&1 || true
[[ -f "$DB2" ]] || { err "DB2 not created"; exit 1; }
REREG_ARGS='{"folder_path":"'"$EXPORT_DIR"'"}'
REREG_RAW="$(mcp_call "$DB2" "rereg" "memory_skill_register" "$REREG_ARGS")"
DIGEST_REREG="$(parse_field "$REREG_RAW" "digest")"
if [[ -z "$DIGEST_REREG" ]]; then
  err "re-registration failed; raw: $REREG_RAW"
  exit 1
fi
info "digest (re-registered)   = $DIGEST_REREG"

# ─── 8. Assert round-trip digest match ──────────────────────────────────
step "8/9  assert promote → export → re-register identical digest"
if [[ "$DIGEST_REREG" != "$DIGEST_PROMOTED" ]]; then
  err "ROUND-TRIP FAILED: re-registered digest != promotion digest"
  err "  promoted:      $DIGEST_PROMOTED"
  err "  re-registered: $DIGEST_REREG"
  exit 5
fi
ok "KEYSTONE: round-trip digest identical (byte-for-byte SHA-256 match)"

# ─── 9. Verdict ─────────────────────────────────────────────────────────
step "9/9  verdict"
echo
printf "%s+-- v0.7.0 reflection-to-skill -- reproduction verdict --+%s\n" "$BOLD" "$RESET"
printf "%s| db1                     %s| %s\n" "$BOLD" "$RESET" "$DB1"
printf "%s| db2                     %s| %s\n" "$BOLD" "$RESET" "$DB2"
printf "%s| reflection (depth=1)    %s| id=%s\n" "$BOLD" "$RESET" "$REFL_ID"
printf "%s| skill_id                %s| %s\n" "$BOLD" "$RESET" "$SKILL_ID"
printf "%s| digest (promoted)       %s| %s\n" "$BOLD" "$RESET" "$DIGEST_PROMOTED"
printf "%s| digest (exported)       %s| %s\n" "$BOLD" "$RESET" "$DIGEST_EXPORTED"
printf "%s| digest (re-registered)  %s| %s\n" "$BOLD" "$RESET" "$DIGEST_REREG"
printf "%s| round-trip identical    %s| OK\n" "$BOLD" "$RESET"
printf "%s| skills-ref validate     %s| %s\n" "$BOLD" "$RESET" "$SKILLS_REF_OK"
printf "%s+----------------------------------------------------------+%s\n" "$BOLD" "$RESET"
echo

ok "Recipe 03 — reflection-to-skill closing-loop reproduced end-to-end."
info "The skill was promoted from the reflection, exported as a SKILL.md folder,"
info "and re-registered on a SECOND fresh DB with a byte-identical SHA-256 digest."
info "This is the L2-6 keystone acceptance for v0.7.0 (#671)."

if [[ "${COOKBOOK_KEEP_DB:-0}" != "1" ]]; then
  rm -rf "$RUN_DIR"
  info "cleaned up $RUN_DIR (set COOKBOOK_KEEP_DB=1 to retain)"
fi
