# Claude Memory Integration

This project is `claude-memory` — a persistent memory daemon for Claude Code.

## Memory Daemon

A Rust daemon runs at `http://127.0.0.1:9077` with a SQLite-backed memory store.
The CLI binary is at `/opt/cybercommand/bin/claude-memory` (or `claude-memory` if in PATH).

## How to Use Memory

### At session start — recall relevant context:
```bash
claude-memory --db /opt/cybercommand/claude-memory.db recall "<current project or task context>"
```

### When you learn something important — store it:
```bash
claude-memory --db /opt/cybercommand/claude-memory.db store \
  --tier long \
  --namespace "<project-name>" \
  --title "What you learned" \
  --content "The details" \
  --source claude \
  --priority 7
```

### Memory tiers:
- `short` — ephemeral, expires in 6 hours (debugging context, current task state)
- `mid` — working knowledge, expires in 7 days (sprint goals, recent decisions)
- `long` — permanent (architecture, user preferences, hard-won lessons)

### When the user corrects you — store as high-priority long-term:
```bash
claude-memory --db /opt/cybercommand/claude-memory.db store \
  --tier long --priority 9 --source user \
  --title "User correction: <what>" \
  --content "<the correction and why>"
```

### Namespace auto-detection:
If you omit `--namespace`, it auto-detects from the git remote or directory name.

### Key commands:
- `recall "<context>"` — fuzzy search with OR semantics, returns ranked results
- `search "<exact terms>"` — AND search for precise matches
- `list --namespace <ns>` — browse all memories for a project
- `promote <id>` — promote a memory to long-term
- `forget --namespace <ns> --pattern "<text>"` — bulk delete by pattern
- `consolidate <id1>,<id2>,... --title "Summary" --summary "Combined learning"` — merge multiple memories into one
- `stats` — overview of memory state
