# Platform-specific notes

`ai-memory` runs anywhere Rust + SQLite run, which in practice covers every
mainstream agent host. Each platform has its own conventions for binary
paths, config locations, and shell semantics. This doc captures
platform-specific differences for the
[session-boot integration recipes](README.md).

## Platform support matrix

| Platform | Status | Binary location (typical) | Default DB path | Hook scripting |
|---|---|---|---|---|
| **macOS** (Apple Silicon + Intel) | First-class — primary dogfood platform | `/opt/homebrew/bin/ai-memory` (Apple Silicon Homebrew) or `/usr/local/bin/ai-memory` (Intel Homebrew) | `${HOME}/.claude/ai-memory.db` | `bash` (default) — Claude Code's `SessionStart` hook command runs in the user's default shell |
| **Linux** (glibc, x86_64 + aarch64) | First-class — covered by CI | `/usr/local/bin/ai-memory` (manual install) or `~/.cargo/bin/ai-memory` (cargo install) | `${HOME}/.claude/ai-memory.db` | `bash` |
| **Linux** (musl, e.g. Alpine) | Supported — static-linked binary recommended | per package manager | `${HOME}/.claude/ai-memory.db` | `sh`/`ash` — POSIX-compatible only |
| **Windows** (10/11, native) | Supported — see Windows-specific notes below | `C:\Users\<user>\.cargo\bin\ai-memory.exe` (cargo install) or wherever the user dropped the release zip | `%USERPROFILE%\.claude\ai-memory.db` | PowerShell or `cmd.exe`. `bash` only via WSL |
| **Windows** (WSL2) | First-class — equivalent to Linux | as Linux (above) | as Linux | `bash` |
| **Docker** / containers | First-class — official image planned, see "Container deployments" below | `/usr/local/bin/ai-memory` inside the image | `/data/ai-memory.db` (volume-mounted) | depends on host |
| **BSD** (FreeBSD, OpenBSD, NetBSD) | Best-effort — should build cleanly via `cargo build --release` but not regularly tested | `/usr/local/bin/ai-memory` (manual install) | `${HOME}/.claude/ai-memory.db` | `sh` |
| **iOS / Android** | Not supported | n/a | n/a | n/a |

## macOS specifics

Most recipes in this directory assume macOS conventions (Homebrew binary,
`~/.claude/` config root). Production-tested on FROSTYi.local (Apple Silicon)
through the v0.6.3.1 dogfood workflow. No special notes — the recipes
"just work."

## Linux specifics

- The `ai-memory` binary is self-contained (statically links SQLite,
  bundles tokenizer assets in the binary). One-step install via
  `cargo install ai-memory` or via the release tarball.
- `~/.claude/` is the convention regardless of the agent host (same
  directory works for Claude Code on Linux, Cursor, Cline, etc.).
- For systemd-managed agents (running ai-memory as a daemon under a
  service unit), see [`docs/INSTALL.md`](../INSTALL.md). For session-boot
  integration the daemon mode is irrelevant — boot calls are stdio
  one-shots.

## Windows specifics

The integration recipes change on native Windows because
`SessionStart` hook commands run in PowerShell (or `cmd.exe`),
not in `bash`. Three things differ:

### 1. Path syntax in `~/.claude/settings.json`

Use forward slashes or escape backslashes — JSON requires escapes. Either
of these works:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "C:/Users/<user>/.cargo/bin/ai-memory.exe boot --quiet --limit 10"
          }
        ]
      }
    ]
  }
}
```

Or use the binary name alone if it's on `%PATH%`:

```json
{
  "command": "ai-memory boot --quiet --limit 10"
}
```

### 2. Default DB path env var

```json
{
  "env": {
    "AI_MEMORY_DB": "%USERPROFILE%\\.claude\\ai-memory.db"
  }
}
```

(Claude Code expands `%USERPROFILE%` before passing to the hook.)

### 3. PowerShell wrapper for the programmatic recipes

The `bash` snippets in
[`codex-cli.md`](codex-cli.md), [`claude-agent-sdk.md`](claude-agent-sdk.md),
etc. need PowerShell equivalents. Pattern:

```powershell
$bootContext = & ai-memory boot --quiet --limit 10 --format text 2>$null
if ($LASTEXITCODE -eq 0 -and $bootContext) {
    $systemMessage = "You are a helpful assistant.`n`n## Recent context (ai-memory)`n$bootContext"
} else {
    $systemMessage = "You are a helpful assistant."
}
```

(Same pattern works on Windows + Linux + macOS PowerShell 7+.)

## WSL2 specifics

Treat as Linux. The catch: each WSL distro has its own `~/.claude/` root.
If you also use Claude Code on the Windows side, you'll have two separate
ai-memory DBs unless you point both at the same path (e.g. via
`AI_MEMORY_DB=//wsl$/Ubuntu/home/<user>/.claude/ai-memory.db` from
Windows). Recommended: pick one side as the source of truth.

## Container deployments

Running ai-memory inside a container changes the DB persistence model:
without a volume mount, the DB lives inside the container and dies with
it. For session-boot integration the recipe pattern is:

```dockerfile
FROM rust:1.85-slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --release --bin ai-memory

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/ai-memory /usr/local/bin/
VOLUME ["/data"]
ENV AI_MEMORY_DB=/data/ai-memory.db
ENTRYPOINT ["ai-memory"]
```

Then the host mounts `/data` to a persistent volume and the agent host
calls `docker exec <container> ai-memory boot --quiet` for the hook —
or, more commonly, runs `ai-memory` natively on the host and only uses
the container for the daemon mode.

The official image lives in `docker/Dockerfile` (TODO — track in #487
follow-ups).

## BSD specifics

`ai-memory` is expected to build and run on FreeBSD, OpenBSD, and NetBSD
via `cargo build --release` — Rust + rusqlite cover the platform — but is
not regularly tested. Treat as Linux for recipe purposes; file an issue
if you hit BSD-specific friction (path conventions, signal handling, FTS5
build flags) and we'll add explicit coverage.

## Lifetime test matrix (PR-3)

The session-boot lifetime test suite (PR-3 of issue #487) runs the
universal contract tests on a CI matrix:

- `ubuntu-latest` (Linux x86_64)
- `macos-latest` (Apple Silicon)
- `windows-latest` (native Windows)

Tests exercise: boot exit codes, status-header shape, recipe JSON
validity, namespace inference, budget clamp, status diagnostics. The live
agent smoke test (gated under `--features e2e`) currently runs only on
macOS where the dogfood Claude Code install lives; expanding to Linux + Windows
is tracked in #487 follow-ups.

## Related

- [`README.md`](README.md) — agent matrix and the universal `ai-memory boot` primitive.
- [`../INSTALL.md`](../INSTALL.md) — full install instructions per platform.
- Issue #487 — RCA + lifetime suite + cross-files.
