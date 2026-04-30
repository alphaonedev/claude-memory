#!/usr/bin/env bash
# dogfood-rebuild.sh — build the current branch and make it the active
# `ai-memory` on this node. Used to run release/v0.6.x.y branches in
# production against the operator's own memory DB before tag-cut.
#
# Workflow per release/v0.6.3.1 (Justin, 2026-04-29): every release
# branch should be dogfooded by the maintainer for at least 24h before
# tag-cut so that any migration / capability / wire-format regression
# surfaces in real use, not just CI.
#
# What this does:
#   1. Builds --release in the current checkout.
#   2. Backs up the live MCP database.
#   3. Runs the new binary against the backup DB to dry-run migrations
#      and confirm `list` returns rows.
#   4. Re-points /opt/homebrew/bin/ai-memory at target/release/ai-memory
#      (idempotent — safe to re-run).
#   5. Reports running MCP processes that need a restart to pick up the
#      new binary.
#
# What this does NOT do:
#   - Kill the running MCP process (would self-DOS Claude Code's
#     memory server). Restart Claude Code or the MCP daemon manually.
#   - Touch the live DB. Migrations only run when an actual ai-memory
#     process opens it — happens automatically on the next MCP restart.
#   - bump Cargo.toml version. That's a tag-cut concern.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIVE_DB="${AI_MEMORY_DB:-$HOME/.claude/ai-memory.db}"
BACKUP_DB="/tmp/ai-memory-dogfood-test-$(date +%Y%m%dT%H%M%S).db"
HOMEBREW_BIN="/opt/homebrew/bin/ai-memory"

cd "$REPO"

branch="$(git branch --show-current)"
head_sha="$(git rev-parse --short HEAD)"
echo "==> dogfood-rebuild — branch=$branch head=$head_sha"
echo

echo "==> 1/5  cargo build --release"
cargo build --release
echo

echo "==> 2/5  backup live DB → $BACKUP_DB"
if [[ -f "$LIVE_DB" ]]; then
  cp "$LIVE_DB" "$BACKUP_DB"
  schema_before=$(sqlite3 "$BACKUP_DB" "SELECT MAX(version) FROM schema_version" 2>/dev/null || echo "?")
  echo "    backup OK; schema_version before = $schema_before"
else
  echo "    no live DB at $LIVE_DB; skipping migration dry-run"
  BACKUP_DB=""
fi
echo

echo "==> 3/5  migration dry-run against backup"
if [[ -n "$BACKUP_DB" ]]; then
  "$REPO/target/release/ai-memory" --db "$BACKUP_DB" list --limit 1 >/dev/null 2>&1
  schema_after=$(sqlite3 "$BACKUP_DB" "SELECT MAX(version) FROM schema_version")
  echo "    schema_version after  = $schema_after"
  if [[ "$schema_before" != "$schema_after" ]]; then
    echo "    MIGRATIONS APPLIED: v$schema_before → v$schema_after on backup."
  else
    echo "    no migrations needed (already at v$schema_after)."
  fi
fi
echo

echo "==> 4/5  re-point $HOMEBREW_BIN → target/release/ai-memory"
if [[ -L "$HOMEBREW_BIN" ]]; then
  current="$(readlink "$HOMEBREW_BIN")"
  if [[ "$current" == "$REPO/target/release/ai-memory" ]]; then
    echo "    symlink already correct; nothing to do"
  else
    echo "    current symlink: $current"
    if [[ "$current" == ../Cellar/* ]]; then
      brew unlink ai-memory >/dev/null 2>&1 || true
    fi
    ln -sfn "$REPO/target/release/ai-memory" "$HOMEBREW_BIN"
    echo "    re-pointed to $REPO/target/release/ai-memory"
  fi
elif [[ -f "$HOMEBREW_BIN" ]]; then
  echo "    WARN: $HOMEBREW_BIN is a regular file, not a symlink. Manual intervention required."
  exit 1
else
  ln -sfn "$REPO/target/release/ai-memory" "$HOMEBREW_BIN"
  echo "    created symlink"
fi
echo "    \$(which ai-memory) = $(which ai-memory)"
echo

echo "==> 5/5  running ai-memory MCP processes (restart required to pick up new binary)"
pids=$(pgrep -f "ai-memory.*mcp" || true)
if [[ -z "$pids" ]]; then
  echo "    no running MCP processes — next launch will use the new binary"
else
  echo "$pids" | while read -r pid; do
    echo "    PID $pid: $(ps -p "$pid" -o command= 2>/dev/null | head -c 200)"
  done
  echo
  echo "    To activate the new binary:"
  echo "      • Restart your Claude Code session (cleanest), OR"
  echo "      • kill <PID>  (Claude Code will respawn the MCP)"
fi
echo

echo "==> done. Dogfood active."
echo "    Verify with: ai-memory doctor --json | jq .summary"
echo "    Backup of live DB at: ${BACKUP_DB:-<none>}"
