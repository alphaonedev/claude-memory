#!/usr/bin/env bash
# t0-orchestrate.sh — Discovery Gate T0 calibration cells across 4 LLMs.
#
# v0.7.0 task E1. The CI-side T0 cells in tests/calibration_t0.rs pin the
# canonical capabilities-v3 phrasings (A1 `summary` + A2
# `to_describe_to_user`) against fixture inputs running on the local
# substrate. E1 wraps those same questions into an orchestration harness
# that exercises them against four live frontier LLMs:
#
#   - Anthropic Claude Sonnet 4.6   (ANTHROPIC_API_KEY)
#   - OpenAI GPT-5                  (OPENAI_API_KEY)
#   - Google Gemini 3               (GOOGLE_API_KEY)
#   - xAI Grok 4.3                  (XAI_API_KEY)
#
# Goal: validate that the canonical phrasings (A1, A2 strings + per-tool
# short descriptions from C2) are correctly understood by every major
# frontier LLM. This is a SCRIPT + HARNESS, not a runtime change. The
# actual cross-LLM run happens out-of-band — operator runs this script
# with the four API keys in env. Results land under results/t0/.
#
# Usage:
#   scripts/t0-orchestrate.sh                # live run, all 4 LLMs
#   scripts/t0-orchestrate.sh --dry-run      # print plan, no API calls
#   scripts/t0-orchestrate.sh --llm claude   # restrict to one LLM
#
# See docs/v0.7/T0-ORCHESTRATION.md for setup, interpretation, and
# re-run cadence.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS_DIR="${REPO}/results/t0"
TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
DRY_RUN=0
ONLY_LLM=""

usage() {
  sed -n '2,32p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1; shift ;;
    --llm)     ONLY_LLM="${2:-}"; shift 2 ;;
    -h|--help) usage ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

# ---------------------------------------------------------------------------
# Discovery Gate questions — taken from tests/calibration_t0.rs.
# Each question pairs with an expected canonical phrasing that the LLM
# response is grep-checked against (substring match — LLMs paraphrase
# the framing but should reproduce the load-bearing fragments verbatim).
# ---------------------------------------------------------------------------

# Q-id        | Source cell in tests/calibration_t0.rs
# T0-A2-CORE  | t0_describe_to_user_core_profile_canonical_phrasing
# T0-A2-FULL  | t0_describe_to_user_full_profile_canonical_phrasing
# T0-A2-GRAPH | t0_describe_to_user_graph_profile_canonical_phrasing
# T0-A2-NJG   | t0_describe_to_user_omits_mcp_jargon_across_profiles
# T0-A1-CORE  | t0_summary_core_profile_lists_four_recovery_paths
# T0-CONTRACT | t0_v3_contract_both_strings_present_under_every_named_profile

QUESTIONS=(
  "T0-A2-CORE|core|What tools do you have available right now? Answer in one sentence to a non-technical user."
  "T0-A2-FULL|full|What tools do you have available right now? Answer in one sentence to a non-technical user."
  "T0-A2-GRAPH|graph|What tools do you have available right now? Answer in one sentence to a non-technical user."
  "T0-A2-NJG|core|Describe your memory tools to me without using any internal jargon."
  "T0-A1-CORE|core|If you needed to use a memory tool that isn't currently loaded, what are all the recovery paths available?"
  "T0-CONTRACT|core|Confirm both your operator-facing summary and your user-facing description fields are populated."
)

EXPECTED_FRAGMENTS=(
  "T0-A2-CORE|7 memory tools right now"
  "T0-A2-CORE|43 more"
  "T0-A2-CORE|available on demand"
  "T0-A2-FULL|all 50 memory tools right now"
  "T0-A2-FULL|Nothing more to load"
  "T0-A2-GRAPH|18 memory tools right now"
  "T0-A2-GRAPH|32 more"
  "T0-A2-NJG|memory tools"
  "T0-A1-CORE|--profile <family>"
  "T0-A1-CORE|memory_load_family"
  "T0-A1-CORE|memory_smart_load"
  "T0-A1-CORE|JSON-RPC -32601"
  "T0-CONTRACT|summary"
  "T0-CONTRACT|to_describe_to_user"
)

# ---------------------------------------------------------------------------
# LLM endpoints — (id|model|env_var|api_url) tuples.
# Chat-completions-style POST bodies live in build_request().
# ---------------------------------------------------------------------------
LLMS=(
  "claude|claude-sonnet-4-6|ANTHROPIC_API_KEY|https://api.anthropic.com/v1/messages"
  "gpt5|gpt-5|OPENAI_API_KEY|https://api.openai.com/v1/chat/completions"
  "gemini|gemini-3|GOOGLE_API_KEY|https://generativelanguage.googleapis.com/v1/models/gemini-3:generateContent"
  "grok|grok-4-3|XAI_API_KEY|https://api.x.ai/v1/chat/completions"
)

# ---------------------------------------------------------------------------
# Fetch the canonical capabilities-v3 payload from the local binary so
# every LLM gets identical system context. In dry-run, we substitute a
# placeholder (no build needed) — the dry-run path is for testing the
# orchestrator structure, not the LLMs themselves.
# ---------------------------------------------------------------------------
load_capabilities_payload() {
  local profile="$1"
  if [[ "$DRY_RUN" -eq 1 ]]; then
    printf '{"profile":"%s","schema_version":"3","summary":"<dry-run>","to_describe_to_user":"<dry-run>"}' "$profile"
    return
  fi
  local bin="${REPO}/target/release/ai-memory"
  if [[ ! -x "$bin" ]]; then
    bin="${REPO}/target/debug/ai-memory"
  fi
  if [[ ! -x "$bin" ]]; then
    echo "warn: no built ai-memory binary; cargo build first" >&2
    printf '{"profile":"%s","schema_version":"3","summary":"<unavailable>","to_describe_to_user":"<unavailable>"}' "$profile"
    return
  fi
  printf '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_capabilities","arguments":{"accept":"v3"}}}\n' \
    | AI_MEMORY_NO_CONFIG=1 "$bin" mcp --profile "$profile" 2>/dev/null \
    | head -1
}

# ---------------------------------------------------------------------------
# Score a response against the expected fragments for its question id.
# Outputs space-separated "passed total" counts.
# ---------------------------------------------------------------------------
score_response() {
  local qid="$1"; local response="$2"
  local total=0; local passed=0
  for entry in "${EXPECTED_FRAGMENTS[@]}"; do
    local eid="${entry%%|*}"
    local frag="${entry#*|}"
    [[ "$eid" == "$qid" ]] || continue
    total=$((total + 1))
    if [[ "$response" == *"$frag"* ]]; then
      passed=$((passed + 1))
    fi
  done
  echo "$passed $total"
}

# ---------------------------------------------------------------------------
# Per-provider request body builders. Each takes the system context +
# the user question and prints a curl invocation suitable for `eval`
# under live mode. In dry-run we just print the plan.
# ---------------------------------------------------------------------------
plan_call() {
  local llm_id="$1"; local model="$2"; local env_var="$3"; local url="$4"
  local qid="$5"; local profile="$6"; local question="$7"
  cat <<EOF
  - llm:      $llm_id
    model:    $model
    api_url:  $url
    auth_env: $env_var
    qid:      $qid
    profile:  $profile
    question: $question
EOF
}

# Live POST — provider-specific body shape. Returns the assistant text
# extracted by jq. Designed defensively: HTTP/network errors yield an
# empty string so scoring records a 0/N for the question rather than
# crashing the whole run.
do_call() {
  local llm_id="$1"; local model="$2"; local env_var="$3"; local url="$4"
  local system_ctx="$5"; local question="$6"
  local key="${!env_var:-}"
  if [[ -z "$key" ]]; then
    echo "" ; return
  fi

  local body
  case "$llm_id" in
    claude)
      body=$(jq -n --arg m "$model" --arg s "$system_ctx" --arg q "$question" \
        '{model:$m, max_tokens:1024, system:$s, messages:[{role:"user", content:$q}]}')
      curl -sS --max-time 60 "$url" \
        -H "x-api-key: $key" \
        -H "anthropic-version: 2023-06-01" \
        -H "content-type: application/json" \
        -d "$body" 2>/dev/null | jq -r '.content[0].text // ""'
      ;;
    gpt5)
      body=$(jq -n --arg m "$model" --arg s "$system_ctx" --arg q "$question" \
        '{model:$m, messages:[{role:"system",content:$s},{role:"user",content:$q}]}')
      curl -sS --max-time 60 "$url" \
        -H "Authorization: Bearer $key" \
        -H "content-type: application/json" \
        -d "$body" 2>/dev/null | jq -r '.choices[0].message.content // ""'
      ;;
    gemini)
      body=$(jq -n --arg s "$system_ctx" --arg q "$question" \
        '{system_instruction:{parts:[{text:$s}]}, contents:[{parts:[{text:$q}]}]}')
      curl -sS --max-time 60 "${url}?key=${key}" \
        -H "content-type: application/json" \
        -d "$body" 2>/dev/null | jq -r '.candidates[0].content.parts[0].text // ""'
      ;;
    grok)
      body=$(jq -n --arg m "$model" --arg s "$system_ctx" --arg q "$question" \
        '{model:$m, messages:[{role:"system",content:$s},{role:"user",content:$q}]}')
      curl -sS --max-time 60 "$url" \
        -H "Authorization: Bearer $key" \
        -H "content-type: application/json" \
        -d "$body" 2>/dev/null | jq -r '.choices[0].message.content // ""'
      ;;
    *) echo "" ;;
  esac
}

# ---------------------------------------------------------------------------
# Main orchestration loop.
# ---------------------------------------------------------------------------
mkdir -p "$RESULTS_DIR"

echo "==> t0-orchestrate"
echo "    timestamp: $TIMESTAMP"
echo "    dry-run:   $DRY_RUN"
echo "    only-llm:  ${ONLY_LLM:-<all>}"
echo "    results:   $RESULTS_DIR"
echo "    questions: ${#QUESTIONS[@]}"
echo

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "plan:"
  for llm_entry in "${LLMS[@]}"; do
    IFS='|' read -r llm_id model env_var url <<<"$llm_entry"
    [[ -z "$ONLY_LLM" || "$ONLY_LLM" == "$llm_id" ]] || continue
    for q_entry in "${QUESTIONS[@]}"; do
      IFS='|' read -r qid profile question <<<"$q_entry"
      plan_call "$llm_id" "$model" "$env_var" "$url" "$qid" "$profile" "$question"
    done
  done
  echo
  echo "expected_fragments: ${#EXPECTED_FRAGMENTS[@]}"
  echo "results_template:   $RESULTS_DIR/<llm>-${TIMESTAMP}.json"
  echo "summary_template:   $RESULTS_DIR/summary-${TIMESTAMP}.md"
  echo "==> dry-run complete (no API calls made)"
  exit 0
fi

# Live mode: jq is required for body construction + response extraction.
command -v jq >/dev/null 2>&1 || { echo "jq required for live mode" >&2; exit 3; }
command -v curl >/dev/null 2>&1 || { echo "curl required for live mode" >&2; exit 3; }

SUMMARY_MD="${RESULTS_DIR}/summary-${TIMESTAMP}.md"
{
  echo "# T0 cross-LLM orchestration — ${TIMESTAMP}"
  echo
  echo "| LLM | Question | Profile | Passed | Total |"
  echo "|---|---|---|---|---|"
} >"$SUMMARY_MD"

for llm_entry in "${LLMS[@]}"; do
  IFS='|' read -r llm_id model env_var url <<<"$llm_entry"
  [[ -z "$ONLY_LLM" || "$ONLY_LLM" == "$llm_id" ]] || continue

  if [[ -z "${!env_var:-}" ]]; then
    echo "skip $llm_id — $env_var unset"
    continue
  fi

  out_file="${RESULTS_DIR}/${llm_id}-${TIMESTAMP}.json"
  echo "==> $llm_id ($model) -> $out_file"

  printf '{"llm":"%s","model":"%s","timestamp":"%s","results":[' "$llm_id" "$model" "$TIMESTAMP" >"$out_file"
  first=1

  for q_entry in "${QUESTIONS[@]}"; do
    IFS='|' read -r qid profile question <<<"$q_entry"
    system_ctx="$(load_capabilities_payload "$profile")"
    response="$(do_call "$llm_id" "$model" "$env_var" "$url" "$system_ctx" "$question")"
    read -r passed total <<<"$(score_response "$qid" "$response")"
    [[ $first -eq 1 ]] || printf ',' >>"$out_file"
    first=0
    jq -n \
      --arg qid "$qid" --arg profile "$profile" --arg q "$question" \
      --arg r "$response" --argjson p "$passed" --argjson t "$total" \
      '{qid:$qid, profile:$profile, question:$q, response:$r, passed:$p, total:$t}' \
      >>"$out_file"
    echo "| $llm_id | $qid | $profile | $passed | $total |" >>"$SUMMARY_MD"
    echo "    $qid ($profile): $passed/$total"
  done

  printf ']}' >>"$out_file"
done

echo
echo "==> summary: $SUMMARY_MD"
