#!/usr/bin/env bash
# post-ship-converge.sh — v0.7 Track E, task E2.
#
# Post-ship convergence verification. After a v0.7.x release tag lands
# (Track F task F5) the published `ai-memory` binary on crates.io may
# behave subtly differently from the CI binary — packaging metadata,
# `[package].include` exclusions, feature-flag defaults, and target-cpu
# baselines are all "passes-CI but breaks-for-users" hazards.
#
# This script installs the published binary on a fresh-as-possible
# environment and replays the 6 canonical Discovery Gate questions
# against it. If every answer matches the canonical phrasing pinned in
# docs/v0.7/canonical-phrasings.md the verdict is GREEN; any drift is
# RED and triggers the escalation path documented in
# docs/v0.7/POST-SHIP-CONVERGENCE.md.
#
# Usage:
#   scripts/post-ship-converge.sh --version <X.Y.Z> [--dry-run] [--method cargo|brew|binary]
#
# Sister to:
#   scripts/t0-orchestrate.sh (E1) — same 6-question set, run against
#                                    the in-repo cargo build, not the
#                                    published artifact.
#
# Output: structured JSON on stdout, human-readable verdict on stderr.

set -euo pipefail

# ---------------------------------------------------------------------
# Defaults / argument parsing
# ---------------------------------------------------------------------
VERSION=""
DRY_RUN=0
INSTALL_METHOD="cargo"   # cargo | brew | binary

usage() {
    cat <<'USAGE' >&2
post-ship-converge.sh — v0.7 E2 post-ship convergence verification.

Required:
  --version <X.Y.Z>     Published ai-memory crate/tag version to verify.

Optional:
  --dry-run             Skip install + spawn; emit the question set and
                        the JSON verdict envelope with `dry_run: true`.
                        Used by tests/e2_post_ship_dry_run.rs.
  --method <m>          Install method: cargo (default), brew, binary.
                        cargo  → `cargo install ai-memory --version $V`
                        brew   → `brew install alphaonedev/tap/ai-memory@$V`
                        binary → curl prebuilt artefact from GitHub releases
  -h | --help           This message.

Exit codes:
  0   GREEN — all 6 questions converged on canonical phrasing
  2   RED   — at least one drift detected (see JSON for which)
  3   USAGE — bad arguments
  4   INSTALL — installation step failed
USAGE
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)
            VERSION="${2:-}"
            shift 2
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --method)
            INSTALL_METHOD="${2:-}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "post-ship-converge: unknown argument: $1" >&2
            usage
            exit 3
            ;;
    esac
done

if [[ -z "$VERSION" ]]; then
    echo "post-ship-converge: --version <X.Y.Z> is required" >&2
    usage
    exit 3
fi

case "$INSTALL_METHOD" in
    cargo|brew|binary) ;;
    *)
        echo "post-ship-converge: --method must be one of: cargo, brew, binary" >&2
        exit 3
        ;;
esac

# ---------------------------------------------------------------------
# The 6 canonical Discovery Gate questions (sister to E1).
#
# Q1..Q3 are the user-facing T0-A2 calibration cells (one per
# representative profile: core/graph/full). Q4 is the operator-facing
# T0-A1 cell on --profile core. Q5 is the T0-NO-JARGON tone gate
# applied to --profile full. Q6 is the structural T0-CONTRACT cell on
# --profile core.
#
# Each question is { id, profile, accept, expect_kind, expect } where:
#   expect_kind = "exact"     — full describe_to_user / summary string
#                 "contains"  — substring presence test
#                 "absent"    — substring MUST NOT appear
#                 "schema"    — JSON shape (schema_version=="3" + fields)
# ---------------------------------------------------------------------
QUESTIONS_JSON=$(cat <<'JSON'
[
  {
    "id": "Q1-T0-A2-CORE",
    "profile": "core",
    "accept": "v3",
    "field": "to_describe_to_user",
    "expect_kind": "exact",
    "expect": "I can directly use 7 memory tools right now (store, recall, list, get, search, ...). 43 more (update, delete, forget, gc, etc.) are available on demand — I can load them if you ask for something that needs them, or you can restart the server with a different profile."
  },
  {
    "id": "Q2-T0-A2-GRAPH",
    "profile": "graph",
    "accept": "v3",
    "field": "to_describe_to_user",
    "expect_kind": "exact",
    "expect": "I can directly use 18 memory tools right now (store, recall, list, get, search, ...). 32 more (update, delete, forget, gc, etc.) are available on demand — I can load them if you ask for something that needs them, or you can restart the server with a different profile."
  },
  {
    "id": "Q3-T0-A2-FULL",
    "profile": "full",
    "accept": "v3",
    "field": "to_describe_to_user",
    "expect_kind": "exact",
    "expect": "I can directly use all 50 memory tools right now (store, recall, list, get, search, ...). Nothing more to load — the full memory surface is already active."
  },
  {
    "id": "Q4-T0-A1-CORE-RECOVERY-PATHS",
    "profile": "core",
    "accept": "v3",
    "field": "summary",
    "expect_kind": "contains",
    "expect": [
      "(a) restart the server with --profile <family>",
      "(b) call memory_load_family(family=<name>) — preferred",
      "(c) call memory_smart_load(intent='<plain language>') — easiest",
      "(d) call the tool by name and recover from JSON-RPC -32601"
    ]
  },
  {
    "id": "Q5-T0-NO-JARGON-FULL",
    "profile": "full",
    "accept": "v3",
    "field": "to_describe_to_user",
    "expect_kind": "absent",
    "expect": [
      "--profile <family>",
      "memory_load_family",
      "memory_smart_load",
      "JSON-RPC",
      "-32601",
      "tools/list",
      "memory_"
    ]
  },
  {
    "id": "Q6-T0-CONTRACT-CORE",
    "profile": "core",
    "accept": "v3",
    "field": "(envelope)",
    "expect_kind": "schema",
    "expect": {
      "schema_version": "3",
      "must_be_nonempty_string": ["summary", "to_describe_to_user"]
    }
  }
]
JSON
)

# ---------------------------------------------------------------------
# Install step (skipped under --dry-run)
# ---------------------------------------------------------------------
BIN_PATH=""
INSTALL_LOG=""
install_published_binary() {
    local install_dir
    install_dir="$(mktemp -d -t ai-memory-e2-XXXXXX)"
    case "$INSTALL_METHOD" in
        cargo)
            INSTALL_LOG="cargo install ai-memory --version $VERSION --root $install_dir"
            if ! cargo install ai-memory --version "$VERSION" --root "$install_dir" >&2; then
                echo "post-ship-converge: cargo install failed" >&2
                exit 4
            fi
            BIN_PATH="$install_dir/bin/ai-memory"
            ;;
        brew)
            INSTALL_LOG="brew install alphaonedev/tap/ai-memory@$VERSION"
            if ! brew install "alphaonedev/tap/ai-memory@$VERSION" >&2; then
                echo "post-ship-converge: brew install failed" >&2
                exit 4
            fi
            BIN_PATH="$(brew --prefix)/bin/ai-memory"
            ;;
        binary)
            local os arch url
            os="$(uname -s | tr '[:upper:]' '[:lower:]')"
            arch="$(uname -m)"
            url="https://github.com/alphaonedev/ai-memory-mcp/releases/download/v${VERSION}/ai-memory-${os}-${arch}.tar.gz"
            INSTALL_LOG="curl $url"
            if ! curl -fsSL "$url" -o "$install_dir/ai-memory.tar.gz" >&2; then
                echo "post-ship-converge: binary download failed: $url" >&2
                exit 4
            fi
            tar -xzf "$install_dir/ai-memory.tar.gz" -C "$install_dir" >&2
            BIN_PATH="$install_dir/ai-memory"
            chmod +x "$BIN_PATH"
            ;;
    esac
    if [[ ! -x "$BIN_PATH" ]]; then
        echo "post-ship-converge: installed binary not executable: $BIN_PATH" >&2
        exit 4
    fi
}

# ---------------------------------------------------------------------
# Replay one question against the installed binary via stdio MCP.
#
# Returns 0 if the response matched the expected canonical phrasing,
# non-zero otherwise. Prints the per-question result JSON on stdout.
#
# Under --dry-run the function emits a synthetic skipped result so the
# JSON envelope shape is still well-formed (used by the dry-run test).
# ---------------------------------------------------------------------
ask_one() {
    local qid="$1" profile="$2" expected_kind="$3"
    if [[ "$DRY_RUN" -eq 1 ]]; then
        printf '{"id":"%s","profile":"%s","kind":"%s","status":"SKIPPED_DRY_RUN"}' \
            "$qid" "$profile" "$expected_kind"
        return 0
    fi

    # Real run: spawn the installed binary as an MCP stdio server and
    # send a tools/call for memory_capabilities with accept=v3 and the
    # requested profile. Compare the relevant field against $expected.
    local req
    req=$(printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_capabilities","arguments":{"accept":"v3","profile":"%s"}}}\n' "$profile")

    local response
    response=$(echo "$req" | "$BIN_PATH" mcp 2>/dev/null | tail -1)

    # Per-question matching is delegated to the assert_match() helper
    # below which knows how to interpret each expect_kind.
    if assert_match "$qid" "$response"; then
        printf '{"id":"%s","profile":"%s","kind":"%s","status":"PASS"}' \
            "$qid" "$profile" "$expected_kind"
        return 0
    else
        printf '{"id":"%s","profile":"%s","kind":"%s","status":"FAIL","response":%s}' \
            "$qid" "$profile" "$expected_kind" "$response"
        return 1
    fi
}

# Per-question matching. Reads the question metadata from $QUESTIONS_JSON
# (parsed via jq when available; falls back to a minimal grep-based
# matcher otherwise — the dry-run path doesn't exercise this).
assert_match() {
    local qid="$1" response="$2"
    if ! command -v jq >/dev/null 2>&1; then
        # Without jq we can only do trivial substring presence; fail
        # closed so a real run never silently passes on a half-baked
        # CI image.
        echo "post-ship-converge: jq not found; cannot verify $qid" >&2
        return 1
    fi
    local kind expected field
    kind=$(echo "$QUESTIONS_JSON"   | jq -r --arg id "$qid" '.[] | select(.id==$id) | .expect_kind')
    field=$(echo "$QUESTIONS_JSON"  | jq -r --arg id "$qid" '.[] | select(.id==$id) | .field')
    expected=$(echo "$QUESTIONS_JSON"| jq -c --arg id "$qid" '.[] | select(.id==$id) | .expect')

    local actual
    if [[ "$field" == "(envelope)" ]]; then
        actual="$response"
    else
        actual=$(echo "$response" | jq -r --arg f "$field" '.. | objects | select(has($f)) | .[$f]' | head -1)
    fi

    case "$kind" in
        exact)
            local want
            want=$(echo "$expected" | jq -r '.')
            [[ "$actual" == "$want" ]]
            ;;
        contains)
            local needle
            while read -r needle; do
                [[ "$actual" == *"$needle"* ]] || return 1
            done < <(echo "$expected" | jq -r '.[]')
            ;;
        absent)
            local needle
            while read -r needle; do
                [[ "$actual" != *"$needle"* ]] || return 1
            done < <(echo "$expected" | jq -r '.[]')
            ;;
        schema)
            local schema_v
            schema_v=$(echo "$response" | jq -r '.. | objects | select(has("schema_version")) | .schema_version' | head -1)
            [[ "$schema_v" == "3" ]] || return 1
            for nonempty_field in summary to_describe_to_user; do
                local v
                v=$(echo "$response" | jq -r --arg f "$nonempty_field" '.. | objects | select(has($f)) | .[$f]' | head -1)
                [[ -n "$v" && "$v" != "null" ]] || return 1
            done
            ;;
        *)
            return 1
            ;;
    esac
}

# ---------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------

if [[ "$DRY_RUN" -eq 0 ]]; then
    install_published_binary
fi

RESULTS="["
PASS_COUNT=0
FAIL_COUNT=0
FIRST=1

# Iterate the question id list in order so the verdict JSON is stable.
QIDS=(Q1-T0-A2-CORE Q2-T0-A2-GRAPH Q3-T0-A2-FULL Q4-T0-A1-CORE-RECOVERY-PATHS Q5-T0-NO-JARGON-FULL Q6-T0-CONTRACT-CORE)
PROFILES=(core graph full core full core)
KINDS=(exact exact exact contains absent schema)

for i in "${!QIDS[@]}"; do
    qid="${QIDS[$i]}"
    profile="${PROFILES[$i]}"
    kind="${KINDS[$i]}"
    result=$(ask_one "$qid" "$profile" "$kind") || true
    [[ $FIRST -eq 1 ]] && FIRST=0 || RESULTS+=","
    RESULTS+="$result"
    if echo "$result" | grep -q '"status":"PASS"'; then
        PASS_COUNT=$((PASS_COUNT+1))
    elif echo "$result" | grep -q '"status":"FAIL"'; then
        FAIL_COUNT=$((FAIL_COUNT+1))
    fi
done
RESULTS+="]"

if [[ "$DRY_RUN" -eq 1 ]]; then
    VERDICT="DRY_RUN"
elif [[ "$FAIL_COUNT" -eq 0 ]]; then
    VERDICT="GREEN"
else
    VERDICT="RED"
fi

# Final structured envelope on stdout.
cat <<JSON
{
  "task": "v0.7-E2",
  "version": "$VERSION",
  "install_method": "$INSTALL_METHOD",
  "dry_run": $([[ "$DRY_RUN" -eq 1 ]] && echo true || echo false),
  "verdict": "$VERDICT",
  "pass_count": $PASS_COUNT,
  "fail_count": $FAIL_COUNT,
  "question_count": 6,
  "results": $RESULTS
}
JSON

# Human verdict on stderr.
echo "post-ship-converge: verdict=$VERDICT version=$VERSION pass=$PASS_COUNT/6 fail=$FAIL_COUNT/6 dry_run=$([[ "$DRY_RUN" -eq 1 ]] && echo true || echo false)" >&2

if [[ "$VERDICT" == "RED" ]]; then
    exit 2
fi
exit 0
