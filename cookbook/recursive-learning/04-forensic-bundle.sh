#!/usr/bin/env bash
# cookbook/recursive-learning/04-forensic-bundle.sh
#
# v0.7.0 Grand-Slam L3-2 (#675) — recipe 04.
#
# What this proves
#   The procurement-grade forensic bundle (L2-5, #670). Given any
#   reflection memory id, `ai-memory export-forensic-bundle` writes a
#   deterministic POSIX-ustar tarball containing the memory, every
#   reachable source via `reflects_on` edges, the manifest, the
#   signed-event envelopes, and (optionally) transcripts. The
#   tarball is then independently re-verifiable via `ai-memory
#   verify-forensic-bundle` — re-hashes every file, checks the
#   manifest signature (when present), and re-verifies every edge
#   signature against the bundled `observed_by` public key. A
#   single-byte tamper makes verification refuse with a non-zero exit
#   code; that's the audit-grade guarantee.
#
# What it does
#   1. Carves a fresh sqlite DB under .local-runs/cookbook-04-<ts>/.
#   2. Stores 3 source observations; reflects to depth=1 then depth=2
#      to give the bundle a non-trivial chain (manifest + 2 edge
#      envelopes + the target memory + source memories).
#   3. Runs `ai-memory export-forensic-bundle --memory-id <depth-2-id>
#      --include-reflections --output <bundle.tar>`.
#   4. Runs `ai-memory verify-forensic-bundle <bundle.tar>` — asserts
#      exit 0 AND a "verification OK" line on stdout.
#   5. Makes a corrupted copy of the bundle (flips one byte in the
#      manifest), runs verify again — asserts exit non-zero AND a
#      "verification FAILED" line.
#   6. Prints a verdict block.
#
# Acceptance
#   Exits 0 only when the un-tampered bundle verifies AND the tampered
#   bundle is refused. Exits >0 otherwise.

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
RUN_DIR="$DEMO_ROOT/cookbook-04-$TS"
DB="$RUN_DIR/memory.db"
BUNDLE="$RUN_DIR/forensic-bundle.tar"
BUNDLE_TAMPERED="$RUN_DIR/forensic-bundle-tampered.tar"
LOG="$RUN_DIR/run.log"
mkdir -p "$RUN_DIR"

BIN="${AI_MEMORY_BIN:-$(command -v ai-memory || true)}"
if [[ -z "$BIN" ]] || [[ ! -x "$BIN" ]]; then
  err "ai-memory binary not found. Set AI_MEMORY_BIN=<path> or put 'ai-memory' on PATH."
  exit 65
fi
info "binary: $BIN"
info "db:     $DB"
info "bundle: $BUNDLE"
info "log:    $LOG"
export AI_MEMORY_NO_CONFIG=1

# ─── 1. Bootstrap ───────────────────────────────────────────────────────
step "1/6  bootstrap demo DB"
"$BIN" --db "$DB" stats >>"$LOG" 2>&1 || true
[[ -f "$DB" ]] || { err "DB not created"; exit 1; }
ok "fresh DB initialised"

# ─── reflect_step helper (same shape as recipe 01/02) ───────────────────
reflect_step() {
  local call_id="$1" srcs_json="$2" title="$3" content="$4" ns="$5"
  local mcp_in="$RUN_DIR/mcp-reflect-${call_id}.in.jsonl"
  local mcp_out="$RUN_DIR/mcp-reflect-${call_id}.out.jsonl"
  cat >"$mcp_in" <<EOF
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"cookbook-04","version":"1.0"}}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_reflect","arguments":{"source_ids":${srcs_json},"title":"${title}","content":"${content}","namespace":"${ns}","tier":"long"}}}
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

# ─── 2. Seed observations + 2-deep reflection chain ─────────────────────
NAMESPACE="cookbook-04-$TS"
step "2/6  seed 3 observations and build a depth-2 reflection chain in $NAMESPACE"
SRC_IDS=()
for i in 1 2 3; do
  json="$("$BIN" --db "$DB" --json store \
    --title "obs-$i" \
    --content "Forensic-bundle source $i: signed reflects_on edges and signed_events must survive tarball round-trip." \
    --namespace "$NAMESPACE" --tier mid 2>>"$LOG")"
  id="$(printf '%s' "$json" | sed -E 's/.*"id":[[:space:]]*"([^"]+)".*/\1/' | head -n 1)"
  [[ -n "$id" ]] || { err "store failed (obs-$i)"; exit 1; }
  SRC_IDS+=("$id")
done
ok "seeded 3 observations"

SRCS_JSON="[\"${SRC_IDS[0]}\",\"${SRC_IDS[1]}\",\"${SRC_IDS[2]}\"]"
R1_RAW="$(reflect_step 1 "$SRCS_JSON" "forensic-depth-1" "Substrate pattern: bounded reflection preserves cryptographic provenance." "$NAMESPACE")"
R1_ID="$(parse_field "$R1_RAW" "id")"
[[ -n "$R1_ID" ]] || { err "depth-1 mint failed; raw: $R1_RAW"; exit 1; }
ok "depth-1 reflection minted (id=$R1_ID)"

R2_RAW="$(reflect_step 2 "[\"$R1_ID\"]" "forensic-depth-2" "Meta-pattern: forensic bundles capture the entire signed chain." "$NAMESPACE")"
R2_ID="$(parse_field "$R2_RAW" "id")"
[[ -n "$R2_ID" ]] || { err "depth-2 mint failed; raw: $R2_RAW"; exit 1; }
ok "depth-2 reflection minted (id=$R2_ID)"

# ─── 3. Export forensic bundle ──────────────────────────────────────────
step "3/6  export-forensic-bundle for depth-2 chain → $BUNDLE"
EXPORT_LOG="$RUN_DIR/export.log"
if ! "$BIN" --db "$DB" export-forensic-bundle \
    --memory-id "$R2_ID" \
    --include-reflections \
    --output "$BUNDLE" \
    >"$EXPORT_LOG" 2>>"$LOG"; then
  err "export-forensic-bundle exited non-zero; see $EXPORT_LOG and $LOG"
  exit 3
fi
if [[ ! -f "$BUNDLE" ]]; then
  err "bundle file not written at $BUNDLE"
  exit 3
fi
SIZE="$(stat -f%z "$BUNDLE" 2>/dev/null || stat -c%s "$BUNDLE" 2>/dev/null || echo "?")"
info "bundle written ($SIZE bytes)"
ok "forensic bundle assembled"

# ─── 4. Verify clean bundle ─────────────────────────────────────────────
step "4/6  verify-forensic-bundle (clean copy)"
VERIFY_LOG="$RUN_DIR/verify-clean.log"
if "$BIN" --db "$DB" verify-forensic-bundle "$BUNDLE" >"$VERIFY_LOG" 2>>"$LOG"; then
  if grep -q "verification OK" "$VERIFY_LOG"; then
    ok "clean bundle verified (exit 0, 'verification OK' on stdout)"
  else
    err "verify exited 0 but no 'verification OK' line in $VERIFY_LOG"
    exit 4
  fi
else
  err "verify exited non-zero on a clean bundle; see $VERIFY_LOG"
  exit 4
fi

# ─── 5. Tamper one byte and re-verify ───────────────────────────────────
step "5/6  tamper one byte and re-verify (refusal expected)"
cp "$BUNDLE" "$BUNDLE_TAMPERED"
# Locate a memory-body byte to flip. We can't mutate a ustar header byte
# without breaking the tar parser (anyhow::Error rather than a
# structured 'verification FAILED' report). Instead, find a long ASCII
# substring inside any `memories/*.json` file and overwrite one
# character of it directly in the tarball bytes — the verifier will
# notice the file's SHA-256 no longer matches the manifest's recorded
# hash and append the path to `tampered_files`, which flips
# `report.ok = false`.
#
# We grep for the marker substring "memory_kind" — every memory
# envelope serialises that field, and it appears past byte 7680 (well
# past the manifest region) for any realistic bundle. We then write
# back a perturbed version one character in.
MARKER='memory_kind'
TAMPER_OFFSET=""
if command -v grep >/dev/null && grep -aob -m 1 "$MARKER" "$BUNDLE_TAMPERED" >/dev/null 2>&1; then
  TAMPER_OFFSET="$(grep -aob -m 1 "$MARKER" "$BUNDLE_TAMPERED" 2>/dev/null | head -n 1 | cut -d: -f1)"
fi
if [[ -z "$TAMPER_OFFSET" ]]; then
  # Fallback: pick a byte well past the manifest region (>= 80% in).
  SIZE_T="$(stat -f%z "$BUNDLE_TAMPERED" 2>/dev/null || stat -c%s "$BUNDLE_TAMPERED" 2>/dev/null)"
  TAMPER_OFFSET=$(( (SIZE_T * 8) / 10 ))
fi
# Flip the byte at TAMPER_OFFSET. dd seek=N writes the new byte AT
# offset N; conv=notrunc preserves the surrounding bytes.
ORIG_BYTE=$(dd if="$BUNDLE_TAMPERED" bs=1 skip="$TAMPER_OFFSET" count=1 2>/dev/null \
  | od -An -tu1 | tr -d ' \n')
NEW_BYTE=$(( (ORIG_BYTE + 1) & 0xff ))
printf '%b' "$(printf '\\x%02x' "$NEW_BYTE")" \
  | dd of="$BUNDLE_TAMPERED" bs=1 seek="$TAMPER_OFFSET" count=1 conv=notrunc 2>/dev/null
info "flipped byte at offset $TAMPER_OFFSET ($ORIG_BYTE → $NEW_BYTE)"

VERIFY_TAMPERED_LOG="$RUN_DIR/verify-tampered.log"
set +e
"$BIN" --db "$DB" verify-forensic-bundle "$BUNDLE_TAMPERED" >"$VERIFY_TAMPERED_LOG" 2>>"$LOG"
VERIFY_TAMPERED_RC=$?
set -e
info "verify exit code on tampered bundle: $VERIFY_TAMPERED_RC"
if (( VERIFY_TAMPERED_RC != 0 )) && grep -q "verification FAILED" "$VERIFY_TAMPERED_LOG"; then
  ok "tamper detected: verify exited non-zero AND wrote 'verification FAILED'"
else
  err "tamper NOT detected (rc=$VERIFY_TAMPERED_RC); see $VERIFY_TAMPERED_LOG"
  exit 5
fi

# ─── 6. Verdict ─────────────────────────────────────────────────────────
step "6/6  verdict"
echo
printf "%s+-- v0.7.0 forensic bundle -- reproduction verdict --+%s\n" "$BOLD" "$RESET"
printf "%s| db                      %s| %s\n" "$BOLD" "$RESET" "$DB"
printf "%s| reflection (depth=2)    %s| id=%s\n" "$BOLD" "$RESET" "$R2_ID"
printf "%s| bundle                  %s| %s\n" "$BOLD" "$RESET" "$BUNDLE"
printf "%s| bundle size             %s| %s bytes\n" "$BOLD" "$RESET" "$SIZE"
printf "%s| verify (clean)          %s| OK\n" "$BOLD" "$RESET"
printf "%s| tamper offset           %s| $TAMPER_OFFSET\n" "$BOLD" "$RESET"
printf "%s| verify (tampered)       %s| REFUSED (rc=$VERIFY_TAMPERED_RC, expected non-zero)\n" "$BOLD" "$RESET"
printf "%s+------------------------------------------------------+%s\n" "$BOLD" "$RESET"
echo

ok "Recipe 04 — forensic bundle export + tamper detection reproduced end-to-end."
info "An auditor can re-verify the un-tampered bundle off-line against the bundled"
info "'observed_by' public key — no live ai-memory deployment required."
if [[ "${COOKBOOK_KEEP_DB:-0}" != "1" ]]; then
  rm -rf "$RUN_DIR"
  info "cleaned up $RUN_DIR (set COOKBOOK_KEEP_DB=1 to retain)"
fi
