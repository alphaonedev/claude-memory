---
sidebar_position: 8
title: Contributing
description: How to contribute to ai-memory.
---

# Contributing

## TL;DR

1. Fork + branch off **`develop`** (not `main` — main is production releases only)
2. Make your change
3. All four gates pass (see [Building](./building))
4. PR with the AI-involvement section if AI-assisted (see [Governance model](./governance-model))
5. Maintainer review
6. Merge to `develop`

## Where the rules live

| File | What |
|---|---|
| [`CONTRIBUTING.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/CONTRIBUTING.md) | Contributor procedures |
| [`docs/AI_DEVELOPER_WORKFLOW.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/AI_DEVELOPER_WORKFLOW.md) | 8-phase AI session workflow |
| [`docs/AI_DEVELOPER_GOVERNANCE.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/AI_DEVELOPER_GOVERNANCE.md) | Authority classes, attribution, hard prohibitions |
| [`docs/ENGINEERING_STANDARDS.md`](https://github.com/alphaonedev/ai-memory-mcp/blob/main/docs/ENGINEERING_STANDARDS.md) | Code, test, security, release standards |

## Branching

- **`main`** — production releases only
- **`develop`** — integration; PRs target this
- **`release/v*`** — release trains (e.g., `release/v0.6.0`)
- **`patch/*`** — patch branches off main
- **`hotfix/*`** — hotfix branches

## Commit format

```
<type>: <summary>

<body>

Co-Authored-By: <name> <email>
```

Types: `feat`, `fix`, `docs`, `style`, `refactor`, `test`, `chore`, `perf`.

## License + CLA

Apache-2.0. By contributing, you agree to the [CLA](https://github.com/alphaonedev/ai-memory-mcp/blob/main/CLA.md).

## Issues

- File via [github.com/alphaonedev/ai-memory-mcp/issues](https://github.com/alphaonedev/ai-memory-mcp/issues)
- Use the `bug` / `enhancement` / `question` / `documentation` labels
- Severity labels: `critical` / `high` / `medium` / `low` / `security`
