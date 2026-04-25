# ai-memory-mcp v0.6.3 Grand-Slam Campaign — Runbook

How to run Claude Code CLI 24x7 as an "AI NHI" developer against `ai-memory-mcp`'s `release/v0.6.3` branch, scoped strictly to the charter at `agentic-mem-labs/strategy/2026-04-25/ai-memory-v0.6.3-grand-slam.md`.

> **High-blast-radius automation.** The agent runs with `--dangerously-skip-permissions` and has merge authority on `release/v0.6.3`. Read [Risks](#risks-read-this) before starting.

---

## Approval scope (granted by user, 2026-04-25)

| Action | Allowed? |
|---|---|
| Read/write files inside `/var/root/dev/ai-memory-mcp/` | ✅ |
| Read files inside `/var/root/dev/agentic-mem-labs/` (charter) | ✅ |
| Write iteration reports under `agentic-mem-labs/campaign-log/v0.6.3/` (via worktree) | ✅ |
| Commit + push to `campaign-log/v0.6.3` branch on agentic-mem-labs | ✅ |
| Commit to `release/v0.6.3` of ai-memory-mcp (signed) | ✅ |
| Open PRs targeting `release/v0.6.3` of ai-memory-mcp | ✅ |
| Merge PRs into `release/v0.6.3` (squash, delete branch) | ✅ |
| `git push --force` / `--force-with-lease` | ❌ |
| Commit/push/merge into ai-memory-mcp `main`, `develop`, other `release/*` | ❌ |
| Commit to agentic-mem-labs `main` or any other branch | ❌ |
| Edit charter or anything under `strategy/`, `the-standard/`, `relocated-from-public*/` | ❌ |
| Create or push `v*` tags | ❌ |
| `gh release create/edit`, `gh workflow run` | ❌ |
| Edit `.github/workflows/release*.yml` | ❌ |
| `cargo publish`, `npm publish` | ❌ |
| Touch system locations, secrets, gh auth, global git config | ❌ |

**Releases are the human's job.** This campaign builds the work; cutting `v0.6.3` is not in scope.

---

## What this is — full-spectrum flow

Per iteration the runner:

1. **Refreshes** charter (`agentic-mem-labs` main, read-only) and dev (`ai-memory-mcp release/v0.6.3`).
2. **Refreshes the campaign-log worktree** at `/var/root/dev/agentic-mem-labs-log` pinned to branch `campaign-log/v0.6.3` of agentic-mem-labs (created on first iteration if absent).
3. **Bumps a global iteration counter** stored at `.agentic/state/iter-counter`.
4. **Spawns** `claude -p --dangerously-skip-permissions` with three `--add-dir` paths and a prompt that mandates 8 steps (see below).

The agent's 8-step iteration:

| # | Step | Side effect |
|--:|---|---|
| 1 | **Load memories** (recall 3 specific queries + list recent in `campaign-v063` namespace) | Read-only |
| 2 | Read charter + last iteration report | Read-only |
| 3 | Inspect dev repo state (`git fetch`, status, log) | Read-only |
| 4 | Pick **one** unblocked charter item | — |
| 5 | Implement on `campaign/<slug>` branch → tests → signed commit → push → PR → squash-merge into `release/v0.6.3` (or, for sub-10-line cosmetic, direct signed commit to `release/v0.6.3`) | **Writes to ai-memory-mcp** |
| 6 | **Record to memory throughout** (decisions, files, PRs, blockers, "future" items) — not only at end | Writes to `ai-memory.db` |
| 7 | **Write iteration report** at `agentic-mem-labs-log/campaign-log/v0.6.3/iter-NNNN.md` from a fixed template | **Writes to log worktree** |
| 8 | **Append to INDEX.md** + signed commit + push to `campaign-log/v0.6.3` (no PR — append-only audit trail) | **Pushes to agentic-mem-labs** |

After the agent exits, the runner sleeps `INTERVAL_SECS` and loops.

---

## Components

| File | Purpose |
|---|---|
| `ops/run-campaign.sh` | The loop. Refreshes both repos + worktree, increments counter, spawns claude with the full 8-step prompt. |
| `ops/start.sh` | Pre-flights (charter exists, dev branch correct, MCP `Connected`, signing test on **both** repos succeeds, agentic-mem-labs has identity) → `nohup` launch. |
| `ops/stop.sh` | Touches kill-switch, drains 30 s, SIGTERM, SIGKILL if needed. |
| `ops/com.alphaone.claude-campaign.plist` | macOS launchd daemon for true 24x7 with auto-restart. |
| `ops/RUNBOOK.md` | This document. |

Runtime state, gitignored:

```
.agentic/
├── kill-switch                    # touch to stop
├── runner.pid                     # PID of nohup runner
├── state/
│   └── iter-counter               # rolling iteration number
└── logs/
    ├── YYYY-MM-DD.log             # daily heartbeat
    ├── YYYY-MM-DD.iter-NNNN.log   # per-iteration full claude output
    ├── git.err                    # stderr from git commands
    ├── runner.out / runner.err    # nohup launcher output
    └── launchd.out / launchd.err  # when run via launchd
```

External worktree (sibling, not inside the repo):

```
/var/root/dev/agentic-mem-labs-log/   # git worktree, branch campaign-log/v0.6.3
└── campaign-log/v0.6.3/
    ├── INDEX.md                       # rolling table of every iteration
    └── iter-NNNN.md                   # one report per iteration
```

---

## Prerequisites (verified 2026-04-25)

- `claude` v2.1.119 at `/opt/homebrew/bin/claude`
- `gh` authed as `alphaonedev`
- ai-memory-mcp cloned to `/var/root/dev/ai-memory-mcp` on `release/v0.6.3` at `9b03d63`
- agentic-mem-labs cloned to `/var/root/dev/agentic-mem-labs` (charter source)
- Local git identity in **both** repos: `alphaonedev <alphaonedev@users.noreply.github.com>`
- SSH ed25519 signing tested on both repos; signing key `~/.ssh/id_ed25519.pub`
- `memory` MCP server registered at user scope, autonomous tier, `gemma4:e2b` LLM
- Native auto-memory disabled (`autoMemoryEnabled: false`)

The `campaign-log/v0.6.3` branch on agentic-mem-labs and the worktree directory are **created on first iteration** if not pre-existing.

---

## Quickstart

### 1. Confirm pre-flight (read-only)

```bash
cd /var/root/dev/ai-memory-mcp
git rev-parse --abbrev-ref HEAD                       # → release/v0.6.3
ls /var/root/dev/agentic-mem-labs/strategy/2026-04-25/ai-memory-v0.6.3-grand-slam.md
claude mcp list | grep memory                          # → ✓ Connected
git config user.email && git config user.name          # → alphaonedev / alphaonedev@users.noreply...
git -C /var/root/dev/agentic-mem-labs config user.email && git -C /var/root/dev/agentic-mem-labs config user.name
```

### 2. Make scripts executable + lint

```bash
chmod +x ops/*.sh
bash -n ops/run-campaign.sh ops/start.sh ops/stop.sh
plutil -lint ops/com.alphaone.claude-campaign.plist
```

### 3. Start (foreground, supervised)

```bash
ops/start.sh
tail -f .agentic/logs/runner.out .agentic/logs/$(date +%F).log
```

`start.sh` runs eight pre-flight checks. If any fails it refuses to launch.

### 4. Start (true 24x7 via launchd)

Recommended only after a successful supervised run.

```bash
sudo cp ops/com.alphaone.claude-campaign.plist /Library/LaunchDaemons/
sudo chown root:wheel /Library/LaunchDaemons/com.alphaone.claude-campaign.plist
sudo chmod 644 /Library/LaunchDaemons/com.alphaone.claude-campaign.plist
sudo launchctl bootstrap system /Library/LaunchDaemons/com.alphaone.claude-campaign.plist
```

`KeepAlive` is wired to the kill-switch path — `touch .agentic/kill-switch` and launchd will let it exit cleanly without respawning.

### 5. Stop

```bash
ops/stop.sh
# or for launchd:
sudo launchctl bootout system /Library/LaunchDaemons/com.alphaone.claude-campaign.plist
```

### 6. Pause without uninstalling

```bash
touch .agentic/kill-switch
# resume with:
rm .agentic/kill-switch && ops/start.sh
```

---

## Tunables

| Env var | Default | Effect |
|---|---|---|
| `DEV_BRANCH` | `release/v0.6.3` | Target branch on ai-memory-mcp |
| `CHARTER_REPO` | `/var/root/dev/agentic-mem-labs` | Read-only charter source |
| `CHARTER_PATH` | `strategy/2026-04-25/ai-memory-v0.6.3-grand-slam.md` | Charter file relative to `CHARTER_REPO` |
| `LOG_WORKTREE` | `/var/root/dev/agentic-mem-labs-log` | Sibling worktree where iteration reports are written |
| `LOG_BRANCH` | `campaign-log/v0.6.3` | Branch on agentic-mem-labs that the worktree pins |
| `LOG_DIR_REL` | `campaign-log/v0.6.3` | Path inside the log branch where reports + INDEX live |
| `INTERVAL_SECS` | `300` (5 min) | Sleep between iterations |
| `MAX_TURNS` | `40` | Cap on agent turns per iteration |
| `MEMORY_NAMESPACE` | `campaign-v063` | ai-memory namespace for campaign memories |

Daily log volume cap: 500 MB → loop pauses 1 hour. Adjust in `run-campaign.sh` if needed.

---

## Iteration report template

The agent uses this template verbatim (from the prompt) for each `iter-NNNN.md`:

```markdown
# Iteration NNNN — <UTC timestamp>

**Charter section:** <heading or line ref>
**Status:** ✅ shipped / 🚧 partial / ⏸ blocked / 🛑 stopped / ⚪ no-op

## What I did
<2-5 sentence summary>

## Files changed
- `path/to/file` — why

## Tests
- `cargo test ...` — passed/failed (n tests)

## Git activity
- Branch: `campaign/<slug>` (or direct on release/v0.6.3)
- Commits: <sha range>
- PR: #N — <state>
- Merged: yes/no

## Memory entries written this iteration
- `<id>` — <title>

## Blockers
<list or "none">

## Next iteration should
<one or two pointers>

## Approx token usage
~Xk
```

---

## Hard rules baked into every iteration's prompt

These survive `--dangerously-skip-permissions` because they are prompt-level constraints, repeated each iteration. They cannot be overridden by the charter.

**Branches (ai-memory-mcp):** push/merge **only** into `release/v0.6.3`; never `main`/`develop`/other `release/*`; feature branches `campaign/<slug>`; never force-push.

**Branches (agentic-mem-labs):** read `main` (charter); write **only** to `campaign-log/v0.6.3` via the worktree; never commit to `main`; never edit `strategy/`, `the-standard/`, `relocated-from-public*/`; campaign-log is append-only.

**Releases (forbidden):** no `v*` tags, no `gh release`, no `gh workflow run`, no edits to `.github/workflows/release*.yml`, no `cargo publish` / `npm publish`.

**Filesystem:** never `rm -rf` outside dev repo or log worktree; never write to `/etc`, `/usr`, `/System`, `/Library`, `~/.aws`, `~/.ssh`, `~/.config/gh`, `.credentials.json`; never `git config --global` or modify `~/.gitconfig`; never `gh auth` reconfigure.

**Code quality:** tests pass before push; `cargo fmt && cargo clippy --all-targets -- -D warnings` clean; no `unwrap()` in non-test code without an invariant comment; conventional commits; one logical change per dev-repo commit; all commits SSH-signed.

**Scope:** only items in the charter; out-of-scope ideas → `tier=mid, tags="future,deferred"` memory; one task per iteration, then stop.

---

## Observability

```bash
# Loop heartbeat
tail -f /var/root/dev/ai-memory-mcp/.agentic/logs/$(date +%F).log

# Most recent iteration's full claude output
ls -t /var/root/dev/ai-memory-mcp/.agentic/logs/*.iter-*.log | head -1 | xargs tail -f

# Iteration counter
cat /var/root/dev/ai-memory-mcp/.agentic/state/iter-counter

# Iteration reports (the human-readable audit trail)
ls /var/root/dev/agentic-mem-labs-log/campaign-log/v0.6.3/
cat /var/root/dev/agentic-mem-labs-log/campaign-log/v0.6.3/INDEX.md

# Git activity on dev repo
git -C /var/root/dev/ai-memory-mcp log --oneline -50 release/v0.6.3
git -C /var/root/dev/ai-memory-mcp branch --list 'campaign/*'

# Campaign memories
ai-memory --db /var/root/.claude/ai-memory.db list --namespace campaign-v063 --json | jq '.[] | {id, title, content}'

# PRs
gh pr list --repo alphaonedev/ai-memory-mcp --base release/v0.6.3 --state all --limit 50

# Token cost (interactive)
claude   # then /cost
```

---

## Budget

- Account: Claude Max PRO ($200/mo) + Extra Usage credit (~$416 on file 2026-04-25)
- No hard dollar cap in the runner. Watch the Anthropic billing console.
- To slow burn: raise `INTERVAL_SECS` (e.g. 900) or lower `MAX_TURNS` (e.g. 25).

---

## Risks (read this)

- **Token spend.** `MAX_TURNS=40` and `INTERVAL_SECS=300` cap a single iteration; daily log cap pauses on 500 MB. Neither is a hard dollar cap.
- **Bad merge on `release/v0.6.3`.** Use `git revert`. Do **not** rebase or force-push.
- **Branch sprawl.** Default flow uses `--delete-branch`. Periodic: `git -C /var/root/dev/ai-memory-mcp branch --merged release/v0.6.3 | grep '^  campaign/' | xargs -r -n1 git -C /var/root/dev/ai-memory-mcp branch -d`.
- **Worktree drift.** If someone manually edits files in `/var/root/dev/agentic-mem-labs-log/` outside the agent flow, the runner's `pull --ff-only` will reject. Resolve with `git -C /var/root/dev/agentic-mem-labs-log pull --rebase`.
- **Charter drift.** Updates flow on next iteration via main refresh. Keep it crisp.
- **Stuck on broken state.** Agent told to STOP and write a stopped-status report. Watch for repeated 🛑 / ⚪ statuses in INDEX.md.
- **Memory contradictions.** Curator dedupes; surface flagged via `ai-memory pending`.
- **Hard rules are prompt-level, not a sandbox.** Mitigations: no production credentials on this node; all commits signed (auditable); release CI prompt-blocked. **Do not run on a node with prod access.**

**Kill switch:** `touch /var/root/dev/ai-memory-mcp/.agentic/kill-switch`. Loop checks at top of every iteration.

---

## Resetting the campaign

```bash
ops/stop.sh
git checkout release/v0.6.3 && git pull --ff-only

# Local feature-branch cleanup
git branch --list 'campaign/*' | xargs -r -n1 git branch -D

# Remote PR + branch cleanup
gh pr list --base release/v0.6.3 --state open --search "head:campaign/" --json number -q '.[].number' \
  | xargs -I{} gh pr close {} --delete-branch

# Memory cleanup (clears campaign namespace)
ai-memory --db /var/root/.claude/ai-memory.db forget --pattern '%campaign-v063%' --confirm

# Iteration counter reset
echo 0 > .agentic/state/iter-counter

# Local logs
rm -rf .agentic/logs/*

# Iteration reports stay on the campaign-log/v0.6.3 branch as historical audit.
# To wipe them too:  cd /var/root/dev/agentic-mem-labs-log
#                    git rm -r campaign-log/v0.6.3/iter-*.md && git commit -S -m "..." && git push
# (Keeping INDEX.md is recommended.)
```

---

## Status as of 2026-04-25 (pre-launch)

- ✅ Repo cloned & on `release/v0.6.3`
- ✅ Local identity + SSH signing verified on both repos
- ✅ Harness scripts written, runner spec'd for full-spectrum flow (memory recall → development → memory record → iteration report → push to campaign-log)
- ✅ `.gitignore` updated to exclude `.agentic/`
- ✅ launchd plist staged (not yet installed)
- ✅ Charter present
- ⏳ `chmod +x ops/*.sh` and first `ops/start.sh` — flipped by user, not the AI
- ⏳ `campaign-log/v0.6.3` branch on agentic-mem-labs — created by runner on first iteration
- ⏳ Cross-encoder neural model download — to be revisited (lexical fallback in use)
