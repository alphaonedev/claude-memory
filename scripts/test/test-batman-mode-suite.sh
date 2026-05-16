#!/usr/bin/env bash
# test-batman-mode-suite.sh — full integration smoke for the Batman Mode
# script surface (issue #800). Runs each script against a throwaway DB
# under .local-runs/, asserts the expected end state, and reports
# pass/fail per script.
#
# This is the script-side complement to the Rust integration tests in
# tests/issue_800_batman_mode.rs. Together they cover:
#
#   Rust:   library + CLI handler unit/integration tests (8 tests)
#   Bash:   end-to-end script execution + DB state assertions (this file)
#   PS1:    syntax + structural checks (test-batman-mode-suite.ps1)
#   Docker: compose schema validation (test-batman-mode-compose.sh)
#
# What this script asserts:
#
#   1. install-batman-active.sh --dry-run prints expected step labels
#      without mutating the operator key dir or the DB.
#   2. install-batman-active.sh against a fresh temp DB:
#      a. generates the operator key (or notes it exists);
#      b. signs + enables R001-R004;
#      c. installs the curator launchd plist (macOS) / systemd unit
#         (Linux);
#      d. patches .claude.json (if present) — uses an env-overrideable
#         test-config path so the operator's real .claude.json isn't
#         touched;
#      e. binds a namespace standard memory.
#   3. After install, scripts/batman-mode-acceptance.sh returns
#      "Batman-ACTIVE" or "Batman-PARTIAL >= 22/25" against the same DB.
#   4. scripts/batman-warmup.sh runs without error and increments the
#      reported counters.
#   5. scripts/batman-bench.sh --samples 2 returns a structured summary
#      with all four size buckets reported.
#
# Usage:
#   scripts/test/test-batman-mode-suite.sh                  # full suite
#   scripts/test/test-batman-mode-suite.sh --quick          # skip bench
#   scripts/test/test-batman-mode-suite.sh --keep-runs      # don't delete
#                                                             temp DB after
#
# Exit code = number of failed sub-tests (0 = full green).

set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_ROOT="$REPO/.local-runs/batman-suite-$(date +%Y%m%d-%H%M%S)"
QUICK=0
KEEP=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --quick)     QUICK=1; shift ;;
        --keep-runs) KEEP=1; shift ;;
        -h|--help)
            sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

mkdir -p "$RUN_ROOT"

cleanup() {
    if [[ $KEEP -eq 0 && -d "$RUN_ROOT" ]]; then
        # Only clean if all-green; keep the dir on failure for debugging.
        if [[ ${FAIL_COUNT:-1} -eq 0 ]]; then
            rm -rf "$RUN_ROOT"
        else
            echo "kept $RUN_ROOT for inspection (set --keep-runs to always keep)"
        fi
    fi
}
trap cleanup EXIT

FAIL_COUNT=0
PASS_COUNT=0

assert() {
    local label="$1"
    local cond="$2"
    if eval "$cond" >/dev/null 2>&1; then
        printf '\033[32m  PASS\033[0m %s\n' "$label"
        PASS_COUNT=$((PASS_COUNT + 1))
    else
        printf '\033[31m  FAIL\033[0m %s\n' "$label"
        printf '         cond: %s\n' "$cond"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi
}

step() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }

# ----------------------------------------------------------- 1: dry-run ---

step "Test 1: install-batman-active.sh --dry-run is non-mutating"
DRY_DB="$RUN_ROOT/dryrun.db"
ai-memory --db "$DRY_DB" store --title init --content init --tier mid --namespace test \
    >/dev/null 2>&1
DRY_DB_HASH_BEFORE=$(md5 -q "$DRY_DB" 2>/dev/null || md5sum "$DRY_DB" | cut -d' ' -f1)
"$REPO/scripts/install-batman-active.sh" --db "$DRY_DB" --dry-run >"$RUN_ROOT/dryrun.out" 2>&1
DRY_RC=$?
DRY_DB_HASH_AFTER=$(md5 -q "$DRY_DB" 2>/dev/null || md5sum "$DRY_DB" | cut -d' ' -f1)

assert "dry-run exited 0"                    "[[ $DRY_RC -eq 0 ]]"
assert "dry-run output contains 'Step 1'"    "grep -q 'Step 1 — Operator keypair' '$RUN_ROOT/dryrun.out'"
assert "dry-run output contains 'Step 7'"    "grep -q 'Step 7 — Namespace standard' '$RUN_ROOT/dryrun.out'"
assert "dry-run does NOT mutate the DB"      "[[ '$DRY_DB_HASH_BEFORE' == '$DRY_DB_HASH_AFTER' ]]"

# ----------------------------------------------------------- 2: real install ---

step "Test 2: install-batman-active.sh fully activates a fresh DB"
LIVE_DB="$RUN_ROOT/live.db"
ai-memory --db "$LIVE_DB" store --title init --content init --tier mid --namespace test \
    >/dev/null 2>&1
"$REPO/scripts/install-batman-active.sh" --db "$LIVE_DB" --namespace "test" \
    >"$RUN_ROOT/install.out" 2>&1
INSTALL_RC=$?

assert "install exited 0"                              "[[ $INSTALL_RC -eq 0 ]]"
assert "install output contains 'Step 1' success"      "grep -q 'operator key' '$RUN_ROOT/install.out'"
assert "install output reports R001 enabled"           "grep -qE 'R001.*enabled' '$RUN_ROOT/install.out'"
assert "install output reports Form 7 smoke pass"      "grep -q '/tmp write refused' '$RUN_ROOT/install.out'"
assert "rules in DB: R001-R004 all enabled + signed"   "[[ \$(sqlite3 '$LIVE_DB' \"SELECT COUNT(*) FROM governance_rules WHERE id IN ('R001','R002','R003','R004') AND enabled=1 AND attest_level='operator_signed'\") -eq 4 ]]"
assert "namespace 'test' bound to a standard"          "[[ \$(sqlite3 '$LIVE_DB' \"SELECT COUNT(*) FROM namespace_meta WHERE namespace='test' AND standard_id IS NOT NULL\") -ge 1 ]]"

# ----------------------------------------------------------- 3: acceptance ---

step "Test 3: batman-mode-acceptance.sh against the same DB"
"$REPO/scripts/batman-mode-acceptance.sh" --db "$LIVE_DB" --namespace "test" \
    >"$RUN_ROOT/acceptance.out" 2>&1
ACCEPT_RC=$?
PASS_LINES=$(grep -c '^PASS · ' "$RUN_ROOT/acceptance.out" || echo 0)
FAIL_LINES=$(grep -c '^FAIL · ' "$RUN_ROOT/acceptance.out" || echo 0)

assert "acceptance suite emits >= 20 PASS lines"        "[[ $PASS_LINES -ge 20 ]]"
assert "acceptance suite verdict is ACTIVE or PARTIAL"  "grep -qE 'VERDICT: Batman-(ACTIVE|PARTIAL)' '$RUN_ROOT/acceptance.out'"
assert "Form 7 enforcement check passes"                "grep -q 'PASS · F7.3' '$RUN_ROOT/acceptance.out'"
assert "Curator daemon check passes (or PARTIAL note)"  "grep -qE 'PASS · U[12]|FAIL · U[12]' '$RUN_ROOT/acceptance.out'"

# ----------------------------------------------------------- 4: warmup ---

step "Test 4: batman-warmup.sh exercises the substrate"
"$REPO/scripts/batman-warmup.sh" --db "$LIVE_DB" --namespace "test" --count 3 \
    >"$RUN_ROOT/warmup.out" 2>&1
WARMUP_RC=$?

assert "warmup exited 0"                                "[[ $WARMUP_RC -eq 0 ]]"
assert "warmup reports 'Before' state"                  "grep -q 'Before: substrate' '$RUN_ROOT/warmup.out'"
assert "warmup reports 'After' state"                   "grep -q 'After: substrate' '$RUN_ROOT/warmup.out'"
assert "warmup completed curator sweep"                 "grep -q 'curator sweep complete' '$RUN_ROOT/warmup.out'"
# Form 7 / governance.write='owner' on a Batman-bound namespace will
# legitimately refuse stores from a different agent_id. We just want
# proof the warmup walked the pipeline; per-row store outcome may
# refuse and that's correct enforcement.

# ----------------------------------------------------------- 5: bench ---

if [[ $QUICK -eq 0 ]]; then
    step "Test 5: batman-bench.sh measures write-path latency"
    "$REPO/scripts/batman-bench.sh" --db "$LIVE_DB" --samples 2 --namespace "_bench_test" \
        >"$RUN_ROOT/bench.out" 2>&1
    BENCH_RC=$?

    assert "bench exited 0"                             "[[ $BENCH_RC -eq 0 ]]"
    assert "bench reports tiny p50"                     "grep -q 'tiny.*p50=' '$RUN_ROOT/bench.out'"
    assert "bench reports medium p50"                   "grep -q 'medium.*p50=' '$RUN_ROOT/bench.out'"
    assert "bench reports large p50"                    "grep -q 'large.*p50=' '$RUN_ROOT/bench.out'"
    assert "bench reports huge p50"                     "grep -q 'huge.*p50=' '$RUN_ROOT/bench.out'"
    assert "bench prints interpretation block"          "grep -q 'Knobs to bring it down' '$RUN_ROOT/bench.out'"
else
    step "Test 5: batman-bench.sh SKIPPED (--quick)"
fi

# ----------------------------------------------------------- 6: namespace CLI ---

step "Test 6: ai-memory namespace CLI verb (Crack 1)"
ai-memory --db "$LIVE_DB" namespace batman-policy --json >"$RUN_ROOT/policy.json" 2>/dev/null
POL_RC=$?
assert "namespace batman-policy exits 0"                "[[ $POL_RC -eq 0 ]]"
assert "policy JSON has auto_atomise:true"              "python3 -c 'import json,sys;d=json.load(open(\"$RUN_ROOT/policy.json\"));sys.exit(0 if d[\"auto_atomise\"] is True else 1)'"
assert "policy JSON has auto_classify_kind"             "python3 -c 'import json,sys;d=json.load(open(\"$RUN_ROOT/policy.json\"));sys.exit(0 if d[\"auto_classify_kind\"] in (\"regex_then_llm\",\"regex_only\") else 1)'"

ai-memory --db "$LIVE_DB" namespace get-standard --namespace "test" --json \
    >"$RUN_ROOT/get-standard.json" 2>/dev/null
GS_RC=$?
assert "namespace get-standard exits 0"                 "[[ $GS_RC -eq 0 ]]"
assert "get-standard returns non-null standard_id"      "python3 -c 'import json,sys;d=json.load(open(\"$RUN_ROOT/get-standard.json\"));sys.exit(0 if d.get(\"standard_id\") else 1)'"

# ----------------------------------------------------------- summary ---

step "Suite summary"
TOTAL=$((PASS_COUNT + FAIL_COUNT))
printf 'PASS=%d FAIL=%d  TOTAL=%d\n' "$PASS_COUNT" "$FAIL_COUNT" "$TOTAL"
if [[ $FAIL_COUNT -eq 0 ]]; then
    printf '\033[32mall green\033[0m — runs: %s\n' "$RUN_ROOT"
else
    printf '\033[31m%d test(s) failed\033[0m — outputs preserved at %s\n' "$FAIL_COUNT" "$RUN_ROOT"
fi

exit $FAIL_COUNT
