#!/bin/bash
# Pre-flights then launch the autonomous campaign runner detached via nohup.
#
# Pre-flight checks (refuses to start if any fail):
#   - charter file exists
#   - dev repo on release/v0.6.3
#   - memory MCP shows Connected
#   - signing test commit succeeds (and is reset)
#   - campaign-log worktree exists or can be created
#   - agentic-mem-labs has user.email/user.name set locally

set -eu

REPO="$(cd "$(dirname "$0")/.." && pwd)"
STATE="$REPO/.agentic"
PID_FILE="$STATE/runner.pid"
KILL="$STATE/kill-switch"
LOG_DIR="$STATE/logs"

DEV_BRANCH="${DEV_BRANCH:-release/v0.6.3}"
CHARTER_REPO="${CHARTER_REPO:-/var/root/dev/agentic-mem-labs}"
CHARTER_PATH="${CHARTER_PATH:-strategy/2026-04-25/ai-memory-v0.6.3-grand-slam.md}"
LOG_WORKTREE="${LOG_WORKTREE:-/var/root/dev/agentic-mem-labs-log}"
LOG_BRANCH="${LOG_BRANCH:-campaign-log/v0.6.3}"
LOG_DIR_REL="${LOG_DIR_REL:-campaign-log/v0.6.3}"

mkdir -p "$LOG_DIR" "$STATE/state"

err() { echo "ERROR: $*" >&2; exit 2; }

# 1. charter present
[ -f "$CHARTER_REPO/$CHARTER_PATH" ] || err "charter not found: $CHARTER_REPO/$CHARTER_PATH"

# 2. dev branch correct
current_branch="$(git -C "$REPO" rev-parse --abbrev-ref HEAD)"
[ "$current_branch" = "$DEV_BRANCH" ] || err "current branch is '$current_branch', expected '$DEV_BRANCH'. Run: git -C $REPO checkout $DEV_BRANCH"

# 3. memory MCP healthy
claude mcp list 2>&1 | grep -q "memory:.*Connected" || err "memory MCP server is not Connected — run 'claude mcp list' to investigate"

# 4. signing works on dev repo
git -C "$REPO" commit --allow-empty -S -m "preflight signing test" >/dev/null 2>&1 || err "signing smoke test failed on dev repo — check git config and SSH key"
git -C "$REPO" reset --hard HEAD~1 >/dev/null 2>&1

# 5. agentic-mem-labs has identity for the agent's report commits
labs_email="$(git -C "$CHARTER_REPO" config --get user.email 2>/dev/null || true)"
labs_name="$(git -C "$CHARTER_REPO" config --get user.name 2>/dev/null || true)"
[ -n "$labs_email" ] || err "agentic-mem-labs has no user.email — run: git -C $CHARTER_REPO config user.email alphaonedev@users.noreply.github.com"
[ -n "$labs_name" ] || err "agentic-mem-labs has no user.name — run: git -C $CHARTER_REPO config user.name alphaonedev"

# 6. signing works on agentic-mem-labs too
git -C "$CHARTER_REPO" commit --allow-empty -S -m "preflight signing test" >/dev/null 2>&1 || err "signing smoke test failed on agentic-mem-labs — check git config and SSH key"
git -C "$CHARTER_REPO" reset --hard HEAD~1 >/dev/null 2>&1

# 7. campaign-log worktree present (or createable)
if [ ! -d "$LOG_WORKTREE/.git" ] && [ ! -f "$LOG_WORKTREE/.git" ]; then
  echo "campaign-log worktree at $LOG_WORKTREE will be created on first iteration."
fi

# 8. runner not already running
if [ -f "$PID_FILE" ] && kill -0 "$(cat "$PID_FILE")" 2>/dev/null; then
  err "runner already running (pid=$(cat "$PID_FILE")). Use ops/stop.sh first."
fi
rm -f "$KILL" "$PID_FILE"

nohup "$REPO/ops/run-campaign.sh" \
  >> "$LOG_DIR/runner.out" 2>> "$LOG_DIR/runner.err" &
echo "started runner pid=$! — tail -f $LOG_DIR/runner.out"
echo "kill switch: touch $KILL  (or ops/stop.sh)"
