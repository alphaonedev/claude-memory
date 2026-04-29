---
sidebar_position: 3
title: Backup & restore
description: Protect your memory database.
---

# Backup & restore

ai-memory's database is a single SQLite file in WAL mode. Standard SQLite backup techniques apply.

## Hot backup (running database)

Use the SQLite backup API or `.backup` command — safe with WAL:

```bash
sqlite3 ~/.local/share/ai-memory/memories.db ".backup '/backup/memories.db'"
```

## Cold backup (database stopped)

```bash
systemctl stop ai-memory
cp ~/.local/share/ai-memory/memories.db* /backup/
systemctl start ai-memory
```

Copy the `.db`, `.db-wal`, and `.db-shm` files together.

## JSON export

Portable, format-stable:

```bash
ai-memory export > /backup/memories-$(date +%F).jsonl
ai-memory import < /backup/memories-2026-04-13.jsonl
```

Use `--trust-source` on import only when restoring a backup you fully control (preserves original `agent_id`).

## Restoration

```bash
systemctl stop ai-memory
cp /backup/memories.db ~/.local/share/ai-memory/memories.db
systemctl start ai-memory
ai-memory list --limit 1   # verify
```
