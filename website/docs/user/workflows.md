---
sidebar_position: 6
title: Common workflows
description: Consolidate, link, supersede, archive, mine.
---

# Workflows

## Consolidate related memories

```bash
ai-memory consolidate "id1,id2,id3" \
  -T "Consolidated: deployment lessons" \
  -s "Combined notes from three deploy retros"
```

Source memories are deleted; a `derived_from` link preserves provenance.

## Resolve a contradiction

When two memories disagree, mark one as superseding:

```bash
ai-memory resolve <winner-id> <loser-id>
```

The loser is demoted; recalls return the winner.

## Archive vs delete

GC archives expired memories before deleting them (default). Restore with:

```bash
ai-memory archive list
ai-memory archive restore <id>
```

## Mine historical conversations

Import facts from existing Claude / ChatGPT / Slack exports:

```bash
ai-memory mine --source claude --input ~/Downloads/claude-export.json
```

Auto-detects format. Adds `mined_from` tag for provenance.

## Bulk forget

```bash
ai-memory forget --namespace stale-project --tier short
```

Filters: `--namespace`, `--pattern`, `--tier`. Archives before delete by default.
