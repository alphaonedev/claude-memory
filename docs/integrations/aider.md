# Aider — programmatic system-message prepend via `--message-file`

**Category 3 (programmatic).** 100% reliable when implemented.

[Aider](https://aider.chat) is a CLI pair-programmer that drives the
edit loop against a local repo. It does not host MCP servers and does
not document a session-start hook, but it does support
`--message-file <path>` to inject a primer message at conversation start
— exactly the surface the boot recipe needs.

The recipe: write `ai-memory boot` output to a tempfile and pass that
file to `aider --message-file`. The Rust-native cross-platform
equivalent is `ai-memory wrap aider` (PR-6 of issue #487) — same
semantics, no shell required.

## Wrapper script

Save as `~/.local/bin/aider-with-memory` and make it executable:

```bash
#!/usr/bin/env bash
# Wraps `aider` with ai-memory boot context injected via --message-file.
# Recipe shown in bash for clarity; PR-6 of issue #487 ships an
# `ai-memory wrap aider` Rust subcommand with identical semantics.
set -euo pipefail

TMP=$(mktemp -t ai-memory-boot.XXXXXX)
trap 'rm -f "$TMP"' EXIT

# Header is preserved so the user sees ok/info/warn status in the chat
# transcript. --message-file content becomes the first user-side turn,
# so we phrase it as context rather than a directive.
{
  echo "## Recent context from ai-memory (read-only, prepended at session start)"
  echo
  ai-memory boot --quiet --format text --limit 10 || true
  echo
  echo "Reference the above when relevant to the user's request."
} > "$TMP"

exec aider --message-file "$TMP" "$@"
```

Then alias `aider` to this wrapper, or invoke `aider-with-memory`
instead. For the Rust-native version (works on Windows, no shell,
no tempfile lifetime issues):

```bash
ai-memory wrap aider -- <aider args>
```

## Why `--message-file` and not `--read`

Aider also supports `--read <file>` to add files to the chat context as
read-only sources. That would also work, but `--message-file` is closer
to the semantic the recipe wants: a primer turn at session start, not a
permanent file in the edit window. `--read` would surface boot context
in every subsequent file diff, which is noisier than the recipe needs.

If you specifically want the boot context to persist across `/clear`
inside an aider session, use `--read` against a stable path instead of
the tempfile pattern above:

```bash
ai-memory boot --quiet --format text --limit 10 > ~/.aider/memory-boot.txt
exec aider --read ~/.aider/memory-boot.txt "$@"
```

Trade-off: stale context until you re-run `ai-memory boot`. The
tempfile pattern always reflects current memory state at launch.

## Quick install

Manual install only until PR-2's installer follow-up adds explicit
aider support. The wrapper script above is the manual form; track the
installer issue for one-line bootstrap.

## End-user diagnostic

Aider streams the `--message-file` contents into the chat as the first
turn, so the `ai-memory boot` status header is visible in the
transcript. The four headers documented in [`README.md`](README.md)
tell `ok` / `info-empty` / `info-greenfield` / `warn-db` apart. If you
see neither header nor body, the wrapper didn't run — check `which
aider` resolves to the wrapper.

## Limitations

- Aider does not host MCP servers; `ai-memory-mcp` cannot be registered
  as a tool here. Mid-session recall (beyond the boot prepend) would
  require Aider to grow MCP support upstream.
- `--message-file` content counts against the context window of the
  underlying model. Tune `--budget-tokens` if you see truncation.
- The tempfile is cleaned up via `trap` on shell exit; if `aider`
  daemonizes (it doesn't today, but version-dependent), you may need to
  switch to a stable path.
- Aider's `/clear` command wipes the chat — including the
  `--message-file` primer. Re-run aider (or use the `--read` pattern
  above) to reload boot context.

## Better, when Aider lands a session-start hook

We have an open feature request at the Aider repo to add a documented
session-start hook (cross-filed from issue #487). When that ships,
replace the wrapper with a hook entry pointing at:

```bash
ai-memory boot --quiet --no-header --limit 10 --budget-tokens 4096
```

This recipe will be updated in place once the hook lands.

## Related

- [`README.md`](README.md) — integration matrix and the universal
  primitive.
- [`codex-cli.md`](codex-cli.md) — same wrapper-script pattern.
- Issue #487 — RCA + cross-files for the Aider hook request.
