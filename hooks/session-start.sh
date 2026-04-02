#!/bin/bash
# AI Memory hook: auto-recall relevant memories on session start
# Works with any MCP-compatible AI client
set -euo pipefail

DB="${AI_MEMORY_DB:-ai-memory.db}"
BINARY="${AI_MEMORY_BIN:-ai-memory}"

# Validate binary exists
if ! command -v "$BINARY" &>/dev/null; then
    exit 0
fi

# Validate DB path doesn't contain dangerous characters
if [[ "$DB" == *".."* ]] || [[ "$DB" == /* && "$DB" != "$HOME"* && "$DB" != /tmp/* ]]; then
    echo "warning: suspicious AI_MEMORY_DB path, skipping" >&2
    exit 0
fi

# Auto-detect namespace from git
NS=$("$BINARY" --db "$DB" --json store --tier short -T "_ns_probe" --content "probe" --source hook 2>/dev/null | grep -o '"namespace":"[^"]*"' | head -1 | cut -d'"' -f4) || true
# Validate namespace contains only safe characters
if [[ -n "$NS" && ! "$NS" =~ ^[a-zA-Z0-9._-]+$ ]]; then
    NS="global"
fi
[ -z "$NS" ] && NS="global"

# Clean up probe
"$BINARY" --db "$DB" forget --pattern "_ns_probe" 2>/dev/null || true

# Recall recent context for this namespace
"$BINARY" --db "$DB" recall "session context project overview" --namespace "$NS" --limit 5 --json 2>/dev/null || true
