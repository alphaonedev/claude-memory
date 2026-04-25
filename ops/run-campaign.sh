#!/bin/bash
# Autonomous Claude Code campaign runner — single iteration loop.
#
# Per iteration the runner:
#   1. Refreshes the charter repo (agentic-mem-labs main, read-only).
#   2. Refreshes the dev repo (ai-memory-mcp release/v0.6.3).
#   3. Refreshes the campaign-log worktree (agentic-mem-labs campaign-log/v0.6.3).
#   4. Bumps the iteration counter.
#   5. Spawns headless `claude -p --dangerously-skip-permissions` with three --add-dir
#      and a prompt that requires:
#        a. RECALL specific memories before work
#        b. Read charter
#        c. Read last iteration report (continuity)
#        d. Pick + implement ONE charter item on a campaign/<slug> branch
#        e. Record significant decisions to memory throughout
#        f. Write iteration report → /var/root/dev/agentic-mem-labs-log/campaign-log/v0.6.3/iter-NNNN.md
#        g. Update INDEX.md
#        h. Commit (signed) + push the report on campaign-log/v0.6.3 branch
#   6. Sleeps the configured interval.
# Stops cleanly when .agentic/kill-switch exists.

set -u

REPO="$(cd "$(dirname "$0")/.." && pwd)"
STATE="$REPO/.agentic"
KILL="$STATE/kill-switch"
LOG_DIR="$STATE/logs"
PID_FILE="$STATE/runner.pid"
ITER_COUNTER="$STATE/state/iter-counter"

DEV_BRANCH="${DEV_BRANCH:-release/v0.6.3}"
CHARTER_REPO="${CHARTER_REPO:-/var/root/dev/agentic-mem-labs}"
CHARTER_PATH="${CHARTER_PATH:-strategy/2026-04-25/ai-memory-v0.6.3-grand-slam.md}"
LOG_WORKTREE="${LOG_WORKTREE:-/var/root/dev/agentic-mem-labs-log}"
LOG_BRANCH="${LOG_BRANCH:-campaign-log/v0.6.3}"
LOG_DIR_REL="${LOG_DIR_REL:-campaign-log/v0.6.3}"
INTERVAL_SECS="${INTERVAL_SECS:-300}"
MAX_TURNS="${MAX_TURNS:-40}"
MEMORY_NAMESPACE="${MEMORY_NAMESPACE:-campaign-v063}"

mkdir -p "$LOG_DIR" "$STATE/state"
[ ! -f "$ITER_COUNTER" ] && echo 0 > "$ITER_COUNTER"
echo $$ > "$PID_FILE"
cleanup() { rm -f "$PID_FILE"; }
trap cleanup EXIT

cd "$REPO"

log() {
  local f="$LOG_DIR/$(date +%F).log"
  echo "$(date -Iseconds) $*" | tee -a "$f"
}

ensure_log_worktree() {
  if [ ! -d "$LOG_WORKTREE/.git" ] && [ ! -f "$LOG_WORKTREE/.git" ]; then
    log "creating campaign-log worktree at $LOG_WORKTREE on $LOG_BRANCH"
    git -C "$CHARTER_REPO" fetch --quiet origin "$LOG_BRANCH" 2>/dev/null || true
    if git -C "$CHARTER_REPO" rev-parse --verify "origin/$LOG_BRANCH" >/dev/null 2>&1; then
      git -C "$CHARTER_REPO" worktree add "$LOG_WORKTREE" -b "$LOG_BRANCH" "origin/$LOG_BRANCH" 2>>"$LOG_DIR/git.err" || \
      git -C "$CHARTER_REPO" worktree add "$LOG_WORKTREE" "$LOG_BRANCH" 2>>"$LOG_DIR/git.err"
    else
      git -C "$CHARTER_REPO" worktree add "$LOG_WORKTREE" -b "$LOG_BRANCH" main 2>>"$LOG_DIR/git.err"
      mkdir -p "$LOG_WORKTREE/$LOG_DIR_REL"
      cat > "$LOG_WORKTREE/$LOG_DIR_REL/INDEX.md" <<'INDEX_EOF'
# Campaign Log — ai-memory v0.6.3

Append-only audit trail of autonomous iterations. One row per iteration.

| Iter | Date (UTC) | Status | Branch | PR | Summary |
|---:|---|:-:|---|---:|---|
INDEX_EOF
      git -C "$LOG_WORKTREE" add "$LOG_DIR_REL/INDEX.md"
      git -C "$LOG_WORKTREE" commit -S -m "chore(campaign-log): bootstrap v0.6.3 audit trail" >/dev/null
      git -C "$LOG_WORKTREE" push -u origin "$LOG_BRANCH" 2>>"$LOG_DIR/git.err" || true
    fi
  fi
  git -C "$LOG_WORKTREE" fetch --quiet origin "$LOG_BRANCH" 2>>"$LOG_DIR/git.err" || true
  git -C "$LOG_WORKTREE" pull --quiet --ff-only origin "$LOG_BRANCH" 2>>"$LOG_DIR/git.err" || true
}

PROMPT_TEMPLATE='You are running ITERATION ITER_NUM_PLACEHOLDER of an autonomous AI NHI development campaign on ai-memory-mcp at REPO_PATH_PLACEHOLDER.

══════════════════════════════════════════════════════════════════════════════
STEP 1 (REQUIRED, FIRST) — LOAD MEMORIES
══════════════════════════════════════════════════════════════════════════════

Use the `memory` MCP server to recall context BEFORE doing anything else.
Run, in order:

  - Recall (semantic) in namespace "MEMORY_NAMESPACE_PLACEHOLDER":
      query 1: "campaign overview approvals scope hard rules"
      query 2: "last completed iteration task blockers next"
      query 3: "code quality standards conventions signing"
  - List recent memories in namespace "MEMORY_NAMESPACE_PLACEHOLDER" (limit 25),
    sorted by recency. Read titles and contents. Identify:
      - the last iteration number recorded
      - what task it shipped or blocked on
      - any "future" / "deferred" entries that may now be unblocked

If memory recall returns nothing, that means this is iteration 1 — proceed
with charter as the only context.

══════════════════════════════════════════════════════════════════════════════
STEP 2 — READ THE CHARTER + LAST ITERATION REPORT
══════════════════════════════════════════════════════════════════════════════

  Charter: CHARTER_FULL_PATH_PLACEHOLDER (read end-to-end)
  Last iteration report (if any): LOG_WORKTREE_PLACEHOLDER/LOG_DIR_REL_PLACEHOLDER/iter-LAST.md
    (find the highest-numbered iter-NNNN.md file under that directory)

══════════════════════════════════════════════════════════════════════════════
STEP 3 — INSPECT REPO STATE
══════════════════════════════════════════════════════════════════════════════

  cd REPO_PATH_PLACEHOLDER
  git fetch origin && git checkout DEV_BRANCH_PLACEHOLDER && git pull --ff-only
  git status; git log --oneline -20

══════════════════════════════════════════════════════════════════════════════
STEP 4 — PICK ONE CONCRETE TASK FROM THE CHARTER
══════════════════════════════════════════════════════════════════════════════

Pick exactly one item that is unblocked and not yet shipped. If you cannot
find one, set status="no-op" for the report and skip to STEP 8.

══════════════════════════════════════════════════════════════════════════════
STEP 5 — IMPLEMENT ON A FEATURE BRANCH
══════════════════════════════════════════════════════════════════════════════

  Default flow:
    git checkout -b campaign/<short-slug> DEV_BRANCH_PLACEHOLDER
    <implement>
    cargo fmt && cargo clippy --all-targets -- -D warnings
    cargo test --all
    git add -A && git commit -S -m "<conventional commit message>"
    git push -u origin campaign/<short-slug>
    gh pr create --base DEV_BRANCH_PLACEHOLDER --title "..." --body "..."
    # Once CI is green and you are confident:
    gh pr merge --squash --delete-branch <pr-number>

  Trivial fix flow (sub-10-line cosmetic only):
    git checkout DEV_BRANCH_PLACEHOLDER
    <edit>; git commit -S -am "<message>"; git push

══════════════════════════════════════════════════════════════════════════════
STEP 6 — RECORD TO MEMORY THROUGHOUT (NOT JUST AT THE END)
══════════════════════════════════════════════════════════════════════════════

Throughout the iteration, use the `memory` MCP `store` tool (namespace
"MEMORY_NAMESPACE_PLACEHOLDER") for:

  - Significant decisions with rationale ("chose X over Y because Z")
  - Files modified, tests added, PRs opened/merged (one entry per PR)
  - Blockers encountered + how you resolved them or why you stopped
  - Out-of-charter ideas → tier=mid, tags="future,deferred"

At end of iteration, store a single SUMMARY memory titled
  "Iteration ITER_NUM_PLACEHOLDER — <one-line outcome>"
with content covering: branch, PR(s), files, tests, status, next.

══════════════════════════════════════════════════════════════════════════════
STEP 7 — WRITE THE ITERATION REPORT (REQUIRED)
══════════════════════════════════════════════════════════════════════════════

Write a markdown report at:
  LOG_WORKTREE_PLACEHOLDER/LOG_DIR_REL_PLACEHOLDER/iter-ITER_NUM_PLACEHOLDER.md

Use this exact template:

```markdown
# Iteration ITER_NUM_PLACEHOLDER — <UTC timestamp>

**Charter section:** <heading or line ref from charter>
**Status:** ✅ shipped / 🚧 partial / ⏸ blocked / 🛑 stopped / ⚪ no-op

## What I did

<2-5 sentence terse summary>

## Files changed

- `path/to/file` — why

## Tests

- `cargo test ...` — passed / failed (n tests)

## Git activity

- Branch: `campaign/<slug>` (or direct on DEV_BRANCH_PLACEHOLDER)
- Commits: <sha1..sha2>
- PR: #N — <state>
- Merged: yes/no

## Memory entries written this iteration

- `<id>` — <title>
- `<id>` — <title>

## Blockers

<terse list, or "none">

## Next iteration should

<one or two concrete pointers>

## Approx token usage

~Xk
```

══════════════════════════════════════════════════════════════════════════════
STEP 8 — UPDATE INDEX + COMMIT THE REPORT TO agentic-mem-labs
══════════════════════════════════════════════════════════════════════════════

  cd LOG_WORKTREE_PLACEHOLDER
  # Append a row to LOG_DIR_REL_PLACEHOLDER/INDEX.md:
  #   | ITER_NUM_PLACEHOLDER | <UTC date> | <status emoji> | campaign/<slug> | #N | <one-liner> |
  git add LOG_DIR_REL_PLACEHOLDER/iter-ITER_NUM_PLACEHOLDER.md LOG_DIR_REL_PLACEHOLDER/INDEX.md
  git commit -S -m "campaign-log: iter ITER_NUM_PLACEHOLDER — <one-line outcome>"
  git push origin LOG_BRANCH_PLACEHOLDER

If the push is rejected (someone else pushed in the meantime):
  git pull --rebase origin LOG_BRANCH_PLACEHOLDER && git push origin LOG_BRANCH_PLACEHOLDER

Do NOT force-push. Do NOT open a PR for this branch — it is append-only audit.

══════════════════════════════════════════════════════════════════════════════
HARD RULES — non-negotiable, repeated to you every iteration
══════════════════════════════════════════════════════════════════════════════

Branches (ai-memory-mcp):
  • Push/merge only into DEV_BRANCH_PLACEHOLDER. NEVER main, develop, release/v0.6.2, or other release/*.
  • Feature branches: campaign/<slug>.
  • NEVER `git push --force` or `--force-with-lease`.

Branches (agentic-mem-labs):
  • Read main (charter). Write only to LOG_BRANCH_PLACEHOLDER via the worktree at LOG_WORKTREE_PLACEHOLDER.
  • NEVER commit to agentic-mem-labs main, NEVER edit strategy/ docs, NEVER edit the charter, NEVER touch the-standard/ or relocated-from-public*/.
  • The campaign-log branch is append-only. NEVER rewrite or delete past iteration files.

Releases (forbidden this campaign):
  • NEVER tag v* on either repo.
  • NEVER `gh release create/edit`.
  • NEVER `gh workflow run`.
  • NEVER edit .github/workflows/release*.yml.
  • NEVER `cargo publish` / `npm publish`.

Filesystem:
  • NEVER `rm -rf` outside REPO_PATH_PLACEHOLDER or LOG_WORKTREE_PLACEHOLDER.
  • NEVER write to /etc, /usr, /System, /Library, ~/.aws, ~/.ssh, ~/.config/gh, .credentials.json.
  • NEVER `git config --global` or modify ~/.gitconfig.
  • NEVER `gh auth` reconfigure or rotate the gh token.

Quality:
  • All commits SSH-signed (existing global config handles this — do not change it).
  • Conventional commit messages.
  • Tests must pass before pushing the dev-branch PR.
  • cargo fmt + cargo clippy --all-targets -- -D warnings clean.
  • No unwrap() in non-test code without an explicit invariant comment.
  • One logical change per dev-repo commit.

Scope:
  • Only items in the charter. Out-of-scope ideas → memory as "future".
  • One development task per iteration, then stop.

══════════════════════════════════════════════════════════════════════════════
EXIT CONDITIONS
══════════════════════════════════════════════════════════════════════════════

  • Charter says "complete"/"done" → write iteration report with status=⚪ no-op,
    record memory, commit + push the report, exit clean.
  • Repo in unexpected state → STOP, write iteration report with status=🛑 stopped,
    explain in the report, record memory, commit + push the report, exit.
  • Hard-rule violation candidate → STOP. Same as above.

Begin with STEP 1 (memory recall). Do not skip ahead.'

iter_num_padded() {
  printf '%04d' "$1"
}

iter=0
log "runner started (pid=$$ repo=$REPO branch=$DEV_BRANCH interval=${INTERVAL_SECS}s log_branch=$LOG_BRANCH)"

while true; do
  if [ -f "$KILL" ]; then
    log "kill-switch present at $KILL — exiting"
    break
  fi

  # Refresh charter repo (read-only main).
  if [ -d "$CHARTER_REPO/.git" ]; then
    git -C "$CHARTER_REPO" fetch --quiet origin 2>>"$LOG_DIR/git.err" || true
    git -C "$CHARTER_REPO" checkout --quiet main 2>>"$LOG_DIR/git.err" || true
    git -C "$CHARTER_REPO" reset --quiet --hard origin/main 2>>"$LOG_DIR/git.err" || true
  fi

  if [ ! -f "$CHARTER_REPO/$CHARTER_PATH" ]; then
    log "charter not found at $CHARTER_REPO/$CHARTER_PATH — sleeping ${INTERVAL_SECS}s"
    sleep "$INTERVAL_SECS"
    continue
  fi

  # Ensure log worktree exists and is current.
  ensure_log_worktree

  # Refresh dev repo + ensure on DEV_BRANCH.
  git -C "$REPO" fetch --quiet origin 2>>"$LOG_DIR/git.err" || true
  if ! git -C "$REPO" diff-index --quiet HEAD --; then
    log "WARN: dev repo has uncommitted local changes — agent will see them"
  fi

  # Bump iteration counter.
  iter_global=$(cat "$ITER_COUNTER")
  iter_global=$((iter_global + 1))
  echo "$iter_global" > "$ITER_COUNTER"
  iter=$((iter+1))

  ITER_PADDED=$(iter_num_padded "$iter_global")
  iter_log="$LOG_DIR/$(date +%F).iter-${ITER_PADDED}.log"
  log "=== iteration $ITER_PADDED (session iter $iter) starting → $iter_log ==="

  # Render the prompt with all placeholders substituted.
  PROMPT="${PROMPT_TEMPLATE//REPO_PATH_PLACEHOLDER/$REPO}"
  PROMPT="${PROMPT//CHARTER_FULL_PATH_PLACEHOLDER/$CHARTER_REPO/$CHARTER_PATH}"
  PROMPT="${PROMPT//DEV_BRANCH_PLACEHOLDER/$DEV_BRANCH}"
  PROMPT="${PROMPT//ITER_NUM_PLACEHOLDER/$ITER_PADDED}"
  PROMPT="${PROMPT//LOG_WORKTREE_PLACEHOLDER/$LOG_WORKTREE}"
  PROMPT="${PROMPT//LOG_DIR_REL_PLACEHOLDER/$LOG_DIR_REL}"
  PROMPT="${PROMPT//LOG_BRANCH_PLACEHOLDER/$LOG_BRANCH}"
  PROMPT="${PROMPT//MEMORY_NAMESPACE_PLACEHOLDER/$MEMORY_NAMESPACE}"

  AI_MEMORY_DB=/var/root/.claude/ai-memory.db \
  AI_MEMORY_AGENT_ID=campaign-runner \
  ITER_NUM="$ITER_PADDED" \
  claude -p "$PROMPT" \
    --dangerously-skip-permissions \
    --add-dir "$REPO" \
    --add-dir "$CHARTER_REPO" \
    --add-dir "$LOG_WORKTREE" \
    --max-turns "$MAX_TURNS" \
    >> "$iter_log" 2>&1
  rc=$?

  log "=== iteration $ITER_PADDED ended (exit=$rc) ==="

  # Daily log volume guard: pause 1h if today exceeds 500MB.
  today_kb=$(du -sk "$LOG_DIR/$(date +%F)".* 2>/dev/null | awk '{s+=$1} END {print s+0}')
  if [ "$today_kb" -gt 512000 ]; then
    log "log volume for today exceeds 500MB — pausing 1 hour"
    sleep 3600
  fi

  sleep "$INTERVAL_SECS"
done

log "runner stopped"
