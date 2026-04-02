#!/bin/bash
# Claude Code hook: auto-recall relevant memories on session start
# Add to .claude/settings.json under hooks.PreToolUse or run manually

DB="${AI_MEMORY_DB:-ai-memory.db}"
BINARY="${AI_MEMORY_BIN:-ai-memory}"

# Auto-detect namespace from git
NS=$($BINARY --db "$DB" --json store --tier short -T "_ns_probe" --content "probe" --source hook 2>/dev/null | grep -o '"namespace":"[^"]*"' | head -1 | cut -d'"' -f4)
[ -z "$NS" ] && NS="global"

# Clean up probe
$BINARY --db "$DB" forget --pattern "_ns_probe" 2>/dev/null

# Recall recent context for this namespace
$BINARY --db "$DB" recall "session context project overview" --namespace "$NS" --limit 5 --json 2>/dev/null
