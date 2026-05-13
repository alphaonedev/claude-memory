#!/usr/bin/env bash
# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
#
# scripts/update-adoption-metrics.sh
#
# Polls the public APIs of GitHub, crates.io, npm, and (where
# available) Homebrew Analytics, then writes the result to
# docs/adoption-metrics.json. The companion dashboard at
# docs/adoption.html reads that JSON via fetch() at load.
#
# Closes roadmap gap F2 (issue #692). Operator-controlled cadence:
# typical cron entry is daily at 06:00 UTC.
#
# Hard rules:
#   - Read-only access to public APIs. No authenticated writes.
#   - No outbound calls from the ai-memory binary; this script is the
#     ONLY surface that touches third-party APIs, and it runs only
#     when the operator invokes it.
#   - GITHUB_TOKEN is optional but strongly recommended (60 req/hr
#     unauthenticated vs 5000 req/hr authenticated).
#   - Network errors degrade gracefully: a missing field is omitted
#     from the output rather than causing the script to exit non-zero
#     (the dashboard already renders placeholders for missing keys).

set -uo pipefail

readonly REPO="${ADOPTION_REPO:-alphaonedev/ai-memory-mcp}"
readonly CRATE="${ADOPTION_CRATE:-ai-memory}"
readonly NPM_PKG="${ADOPTION_NPM_PKG:-ai-memory-mcp}"
readonly OUT="${ADOPTION_OUT:-docs/adoption-metrics.json}"
readonly TS="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

readonly GH_HEADERS=(-H "Accept: application/vnd.github+json" -H "X-GitHub-Api-Version: 2022-11-28")
if [ -n "${GITHUB_TOKEN:-}" ]; then
    GH_HEADERS+=(-H "Authorization: Bearer $GITHUB_TOKEN")
fi

if ! command -v curl >/dev/null 2>&1; then
    echo "ERROR: curl is required. Install curl and re-run." >&2
    exit 2
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "ERROR: jq is required. Install jq and re-run." >&2
    exit 3
fi

curl_json() {
    # $1: URL; rest: extra curl args. Returns body on stdout (empty
    # string on any failure — caller checks for empty).
    local url="$1"; shift
    curl -sf -m 20 "$url" "$@" 2>/dev/null || echo ""
}

mkdir -p "$(dirname "$OUT")"

# ---------------------------------------------------------------
# GitHub: stars, forks, contributors, issues
# ---------------------------------------------------------------
github_block='{}'
gh_repo=$(curl_json "https://api.github.com/repos/$REPO" "${GH_HEADERS[@]}")
if [ -n "$gh_repo" ]; then
    stars=$(echo "$gh_repo"   | jq -r '.stargazers_count // empty')
    forks=$(echo "$gh_repo"   | jq -r '.forks_count // empty')
    issues=$(echo "$gh_repo"  | jq -r '.open_issues_count // empty')
    github_block=$(jq -n \
        --arg stars "$stars" \
        --arg forks "$forks" \
        --arg issues "$issues" \
        '{
            stars:      ($stars  | tonumber? // null),
            forks:      ($forks  | tonumber? // null),
            issues_open:($issues | tonumber? // null)
        }')
fi

# Contributors count (anonymous endpoint: number of items in the
# `contributors` listing). Cap at first 100 (GitHub paginates).
gh_contributors=$(curl_json "https://api.github.com/repos/$REPO/contributors?per_page=100&anon=1" "${GH_HEADERS[@]}")
if [ -n "$gh_contributors" ]; then
    count=$(echo "$gh_contributors" | jq 'length // 0')
    github_block=$(echo "$github_block" | jq --argjson c "$count" '. + { contributors: $c }')
fi

# ---------------------------------------------------------------
# crates.io
# ---------------------------------------------------------------
crates_block='{}'
crates_resp=$(curl_json "https://crates.io/api/v1/crates/$CRATE")
if [ -n "$crates_resp" ]; then
    dl_total=$(echo "$crates_resp" | jq -r '.crate.downloads // empty')
    latest=$(echo "$crates_resp"   | jq -r '.crate.max_stable_version // .crate.newest_version // empty')
    crates_block=$(jq -n \
        --arg dl "$dl_total" \
        --arg v  "$latest" \
        '{
            downloads_total: ($dl | tonumber? // null),
            latest_version:  ($v | select(. != "") // null)
        }')
fi

# ---------------------------------------------------------------
# npm registry (downloads/range API is public)
# ---------------------------------------------------------------
npm_block='{}'
npm_30d=$(curl_json "https://api.npmjs.org/downloads/point/last-month/$NPM_PKG")
if [ -n "$npm_30d" ]; then
    count=$(echo "$npm_30d" | jq -r '.downloads // empty')
    npm_block=$(jq -n --arg d "$count" '{ downloads_30d: ($d | tonumber? // null) }')
fi

# ---------------------------------------------------------------
# Homebrew Analytics (available only if the formula is in homebrew/core;
# tap-hosted formulae do not surface here. Best-effort.)
# ---------------------------------------------------------------
homebrew_block='{ "status": "channel live (see brew install ai-memory)" }'
hb_resp=$(curl_json "https://formulae.brew.sh/api/formula/ai-memory.json")
if [ -n "$hb_resp" ]; then
    hb_version=$(echo "$hb_resp" | jq -r '.versions.stable // empty')
    if [ -n "$hb_version" ]; then
        homebrew_block=$(jq -n --arg v "$hb_version" '{
            status:  "live",
            version: $v
        }')
    fi
fi

# ---------------------------------------------------------------
# Distribution channels we do NOT have a public stats API for
# (placeholders surfaced as "live" so the dashboard shows the
# channel exists; operator updates installs manually if desired).
# ---------------------------------------------------------------
copr_block=$(jq -n '{ status: "live (dnf copr enable alphaonedev/ai-memory)" }')
docker_block=$(jq -n '{ status: "live (ghcr.io/alphaonedev/ai-memory)" }')
apt_block=$(jq -n    '{ status: "live (Jim Bridger PPA)" }')

# ---------------------------------------------------------------
# Activity signals (rolling 30 days)
# ---------------------------------------------------------------
activity_block='{}'
since=$(date -u -d '30 days ago' +%Y-%m-%dT%H:%M:%SZ 2>/dev/null \
    || date -u -v-30d +%Y-%m-%dT%H:%M:%SZ 2>/dev/null \
    || echo "")
if [ -n "$since" ]; then
    commits=$(curl_json "https://api.github.com/repos/$REPO/commits?since=$since&per_page=100" "${GH_HEADERS[@]}")
    if [ -n "$commits" ]; then
        n=$(echo "$commits" | jq 'length // 0')
        activity_block=$(echo "$activity_block" | jq --argjson c "$n" '. + { commits_30d: $c }')
    fi
    prs=$(curl_json "https://api.github.com/search/issues?q=repo:$REPO+is:pr+is:merged+merged:>$since" "${GH_HEADERS[@]}")
    if [ -n "$prs" ]; then
        n=$(echo "$prs" | jq -r '.total_count // 0')
        activity_block=$(echo "$activity_block" | jq --argjson c "$n" '. + { prs_merged_30d: $c }')
    fi
    releases=$(curl_json "https://api.github.com/repos/$REPO/releases?per_page=100" "${GH_HEADERS[@]}")
    if [ -n "$releases" ]; then
        n=$(echo "$releases" | jq --arg s "$since" '[ .[] | select(.published_at >= $s) ] | length')
        activity_block=$(echo "$activity_block" | jq --argjson c "$n" '. + { releases_30d: $c }')
    fi
fi

# ---------------------------------------------------------------
# Assemble final JSON
# ---------------------------------------------------------------
jq -n \
    --arg updated_at "$TS" \
    --argjson github   "$github_block" \
    --argjson crates   "$crates_block" \
    --argjson npm      "$npm_block" \
    --argjson homebrew "$homebrew_block" \
    --argjson copr     "$copr_block" \
    --argjson docker   "$docker_block" \
    --argjson apt      "$apt_block" \
    --argjson activity "$activity_block" \
    '{
        updated_at: $updated_at,
        github:     $github,
        crates:     $crates,
        npm:        $npm,
        homebrew:   $homebrew,
        copr:       $copr,
        docker:     $docker,
        apt:        $apt,
        activity:   $activity
    }' > "$OUT"

echo "wrote $OUT (updated_at=$TS)"
