# Issue #487 — End-to-End Smoke Report (PR-9c, Audit Agent C)

**Date:** 2026-04-30
**Base SHA:** `d974112abbb56b2a036fcc32a15aebe101ea5efa` (release/v0.6.3.1, tip after all 8 PRs merged)
**Branch:** `release/v0.6.3.1-issue-487-pr9c-e2e-smoke`
**Binary under test:** `/Users/fate/ai-memory-mcp/.claude/worktrees/agent-a2594d69f868c3983/target/release/ai-memory` (built by `cargo build --release --bin ai-memory` on this branch — NOT the host's `/opt/homebrew/bin/ai-memory` symlink)
**Binary version (self-reported):** `ai-memory 0.6.3+patch.1`
**Host platform:** Darwin 25.3.0 arm64 (macOS 26.3.1)
**Workspace tempdir:** `/tmp/audit-c-smoke-work/` (all DBs, settings.json fixtures, fake `$HOME` trees, and audit/log directories live under this root — the host's real `~/.claude/ai-memory.db` and `~/.claude/settings.json` were never touched).

All steps were executed sequentially with `AI_MEMORY_NO_CONFIG=1` (or a fake `$HOME` pointing at a temp config) so the host's user config never bled into results.

---

## Summary

| Step | Description | Result |
|------|-------------|--------|
| 1 | Seed 3 memories in fresh DB | **PASS** |
| 2 | `boot` ok manifest + memory titles | **PASS** |
| 3 | `boot` warn manifest for missing DB | **PASS** |
| 4 | `install claude-code` dry-run / apply / uninstall round-trip | **PASS** |
| 5 | `wrap echo -- "hello"` end-to-end | **PASS** |
| 6 | Audit chain emits `session_boot` for `boot` | **PASS** |
| 7 | Log directory resolution precedence (CLI > env > config > platform default) | **PASS** |

**Overall: 7 of 7 steps PASSED.** No critical failures. Two minor observations are recorded under "Observations" below; neither warrants a bug filing.

---

## Step 1 — Seed a fresh DB

**Command** (issued three times with distinct titles in namespace `audit-c-smoke`):

```
AI_MEMORY_NO_CONFIG=1 ai-memory --db /tmp/audit-c-smoke-work/test.db store \
    --tier {mid|long} --namespace audit-c-smoke \
    --title <title> --content <body> --tags smoke,...
```

**Captured stdout:**

```
stored: de0d7986-fbe6-482b-8e4f-1d3d946e8e74 [mid] (ns=audit-c-smoke)   ← smoke-mem-alpha
stored: 98282ddc-d50a-47e4-aec8-602ea554fbdb [mid] (ns=audit-c-smoke)   ← smoke-mem-beta
stored: f59ba533-8482-4715-9665-3f45fb922393 [long] (ns=audit-c-smoke)  ← smoke-mem-gamma
```

Three exit codes: all 0. DB file created at `/tmp/audit-c-smoke-work/test.db` (188 KiB after seeding).

**Result: PASS.**

---

## Step 2 — `boot` ok manifest

**Command:**

```
AI_MEMORY_NO_CONFIG=1 ai-memory --db /tmp/audit-c-smoke-work/test.db boot --namespace audit-c-smoke --limit 5
```

**Captured stdout (line-numbered):**

```
1  # ai-memory boot: ok
2  #   version:    0.6.3+patch.1
3  #   db:         /tmp/audit-c-smoke-work/test.db (schema=v19, 3 memories)
4  #   tier:       semantic (embedder=sentence-transformers/all-MiniLM-L6-v2, reranker=none, llm=none)
5  #   latency:    1ms
6  #   namespace:  audit-c-smoke (loaded 3 memories)
7  - [long/f59ba533] smoke-mem-gamma (ns=audit-c-smoke, p=5, just now)
8  - [mid/98282ddc] smoke-mem-beta (ns=audit-c-smoke, p=5, just now)
9  - [mid/de0d7986] smoke-mem-alpha (ns=audit-c-smoke, p=5, just now)
```

**Assertions:**

- Line 1: starts with `# ai-memory boot: ok` — **OK**.
- Lines 2–6: PR-4's manifest fields all present (`version:`, `db:`, `tier:`, `latency:`, `namespace:`) — **OK**.
- Lines 7–9: all three seeded titles (`smoke-mem-gamma`, `smoke-mem-beta`, `smoke-mem-alpha`) appear in the body — **OK**.

Exit code: 0. Stderr: empty.

**Result: PASS.**

---

## Step 3 — `boot` warn manifest

**Command:**

```
AI_MEMORY_NO_CONFIG=1 ai-memory --db /nonexistent/path/to/db.sqlite boot --namespace audit-c-smoke --limit 5
```

**Captured stdout (line-numbered):**

```
1  # ai-memory boot: warn
2  #   version:    0.6.3+patch.1
3  #   db:         /nonexistent/path/to/db.sqlite (schema=<unavailable>, <unavailable> memories)
4  #   tier:       semantic (embedder=sentence-transformers/all-MiniLM-L6-v2, reranker=none, llm=none)
5  #   latency:    0ms
6  #   namespace:  audit-c-smoke (db unavailable — see `ai-memory doctor`)
```

**Captured stderr (line-numbered):**

```
1  ai-memory boot: db unavailable at /nonexistent/path/to/db.sqlite: failed to open database
```

**Assertions:**

- Line 1: status string `# ai-memory boot: warn` — **OK**.
- Line 2 (version) and line 4 (tier) still surface even though the DB cannot be opened — **OK**.

Exit code: 0 (boot deliberately exits 0 on a missing DB so a hook never wedges the agent).

**Result: PASS.**

---

## Step 4 — `install claude-code` round-trip

A pre-existing `settings.json` was written into a tempdir with two unrelated user keys (`theme`, `_userPref`) so the round-trip can be verified.

### 4a. Dry-run

**Command:**

```
ai-memory install claude-code --config /tmp/audit-c-smoke-work/settings.json
```

**Captured stdout (excerpt — full diff):**

```
1   ai-memory install: dry-run for claude-code install at /tmp/audit-c-smoke-work/settings.json
2   --- before
3   +++ after
4    {
...
8   +  "hooks": {
9   -}
10  +    "SessionStart": [
11  +      {
12  +        "// ai-memory:managed-block:end": "Do not edit. Managed by `ai-memory install`. https://github.com/alphaonedev/ai-memory-mcp/issues/487",
13  +        "// ai-memory:managed-block:start": "Do not edit. Managed by `ai-memory install`. https://github.com/alphaonedev/ai-memory-mcp/issues/487",
...
18  +        "hooks": [
19  +          {
20  +            "command": "ai-memory boot --quiet --limit 10 --budget-tokens 4096",
21  +            "type": "command"
22  +          }
23  +        ],
24  +        "matcher": "*"
25  +      }
26  +    ]
27  +  },
28  +  "theme": "dark"
29  +}
30  ai-memory install: re-run with --apply to write the changes
```

**Assertions:**

- Lines 8–26 contain a `SessionStart` hook block for the managed-block region — **OK**.
- File hash (sha256) before == file hash after dry-run — **OK** (`10fe9797…3356c` both reads).

### 4b. Apply

**Command:**

```
ai-memory install claude-code --config /tmp/audit-c-smoke-work/settings.json --apply
```

**Captured stdout:**

```
1  ai-memory install: install applied to /tmp/audit-c-smoke-work/settings.json
2  ai-memory install: backup at /tmp/audit-c-smoke-work/settings.json.bak.20260430T145813.180Z
```

**Settings.json after apply (line-numbered):**

```
 1  {
 2    "_userPref": "preserve-me",
 3    "hooks": {
 4      "SessionStart": [
 5        {
 6          "// ai-memory:managed-block:end": "Do not edit. Managed by `ai-memory install`. https://github.com/alphaonedev/ai-memory-mcp/issues/487",
 7          "// ai-memory:managed-block:start": "Do not edit. Managed by `ai-memory install`. https://github.com/alphaonedev/ai-memory-mcp/issues/487",
 8          "// ai-memory:managed-keys": [
 9            "matcher",
10            "hooks"
11          ],
12          "hooks": [
13            {
14              "command": "ai-memory boot --quiet --limit 10 --budget-tokens 4096",
15              "type": "command"
16            }
17          ],
18          "matcher": "*"
19        }
20      ]
21    },
22    "theme": "dark"
23  }
```

The managed marker block (`// ai-memory:managed-block:start` / `:end`) is present at lines 6–7 — **OK**. User keys `_userPref` and `theme` are both preserved at lines 2 and 22.

### 4c. Uninstall

**Command:**

```
ai-memory install claude-code --config /tmp/audit-c-smoke-work/settings.json --uninstall --apply
```

**Captured stdout:**

```
1  ai-memory install: uninstall applied to /tmp/audit-c-smoke-work/settings.json
2  ai-memory install: backup at /tmp/audit-c-smoke-work/settings.json.bak.20260430T145813.187Z
```

**Settings.json after uninstall:**

```
1  {
2    "_userPref": "preserve-me",
3    "theme": "dark"
4  }
```

**Restoration check:** the restored file is JSON-equivalent to the pre-install original. Comparing under `python3 -c 'json.dumps(...,sort_keys=True)'`:

- Before:    `{"_userPref": "preserve-me", "theme": "dark"}`
- Restored:  `{"_userPref": "preserve-me", "theme": "dark"}`

Identical content. Two non-content differences are inherent to JSON pretty-printing and acceptable under "modulo whitespace":

1. The installer canonicalised key ordering alphabetically (original had `theme` before `_userPref`; restored has `_userPref` first).
2. Trailing newline is retained.

Both differences are pure JSON semantics, not state loss.

**Result: PASS.**

---

## Step 5 — `wrap echo -- "hello"` end-to-end

**Command** (run from `cd /tmp/audit-c-smoke-work`):

```
AI_MEMORY_NO_CONFIG=1 ai-memory --db /tmp/audit-c-smoke-work/test.db wrap echo -- "hello"
```

`wrap` builds a system message from `boot` output, then spawns `echo` with `--system <message>` followed by the trailing args. Because the agent name is literally `echo`, echo simply prints all its argv joined by spaces.

**Captured stdout (line-numbered):**

```
1  --system You have access to ai-memory, a persistent memory system. The recent context loaded for you appears below. Reference it when relevant to the user's request.
2
3  # ai-memory boot: info
4  #   version:    0.6.3+patch.1
5  #   db:         /tmp/audit-c-smoke-work/test.db (schema=v19, 3 memories)
6  #   tier:       semantic (embedder=sentence-transformers/all-MiniLM-L6-v2, reranker=none, llm=none)
7  #   latency:    19ms
8  #   namespace:  global (fallback: loaded 1 memory from global Long tier)
9  - [long/f59ba533] smoke-mem-gamma (ns=audit-c-smoke, p=5, just now) hello
```

Note: `echo` joins its argv with single spaces, so `hello` appears glued to the end of line 9.

**Assertions:**

- (a) Exit code: 0 — **OK**.
- (b) Boot context is present (lines 3–9 contain the manifest plus a memory body row) followed by `hello` (last token on line 9) — **OK**. The argv emitted by `wrap` was clearly `["--system", "<preamble + boot manifest + body>", "hello"]`, exactly as PR-6 specifies for the default `SystemFlag` strategy.

Note on namespace: because `wrap` invokes `boot` with no explicit `--namespace`, `auto_namespace` resolved to `global` (the cwd basename `audit-c-smoke-work` doesn't match a git remote name); the boot helper then took the documented "tier=Long fallback" path and surfaced the seeded `smoke-mem-gamma` memory from the long tier. This is exactly the documented PR-6 contract.

**Result: PASS.**

---

## Step 6 — Audit chain emits `session_boot` for `boot`

A fake `$HOME` was constructed at `/tmp/audit-c-smoke-work/fake-home/` containing `.config/ai-memory/config.toml`:

```toml
[audit]
enabled = true
path = "/tmp/audit-c-smoke-work/audit-logs"
```

**Step:** run `boot` against the seeded DB to trigger the audit emission, then `audit verify`.

**Audit log line emitted (after `boot`):**

```
{"schema_version":1,
 "timestamp":"2026-04-30T14:59:09.993544+00:00",
 "sequence":1,
 "actor":{"agent_id":"anonymous","synthesis_source":"explicit_or_default"},
 "action":"session_boot",
 "target":{"memory_id":"*","namespace":"audit-c-smoke"},
 "outcome":"allow",
 "prev_hash":"0000000000000000000000000000000000000000000000000000000000000000",
 "self_hash":"56d9e7aab1783e6c0a6d65779ca03f6f67e8802dfd2e8f8757a6ae75e587ff99"}
```

**Verify command:**

```
HOME=/tmp/audit-c-smoke-work/fake-home ai-memory audit verify
```

**Captured stdout (line 1):**

```
1  audit verify OK: 1 line(s) verified at /tmp/audit-c-smoke-work/audit-logs/audit.log
```

**Captured `audit tail --action session_boot --format text` stdout:**

```
1  2026-04-30T14:59:09.993544+00:00 seq=1 anonymous session_boot ns=audit-c-smoke id=* outcome=Allow
```

**Assertions:**

- Audit log exists at the configured path — **OK**.
- Chain verifies (`prev_hash` = chain head, `self_hash` matches) — **OK**.
- At least one event with `action: session_boot` is present — **OK**.

**Result: PASS.**

---

## Step 7 — Log directory resolution precedence

PR-5 addendum (`d6d9088d`) defines the precedence: `CLI flag > AI_MEMORY_LOG_DIR env > [logging] path in config.toml > platform default`.

The `logs cat` subcommand reads from the resolved log directory. By placing a unique marker file `ai-memory.log` in each candidate directory and observing which marker `logs cat` emits, we can prove the resolver picks the correct layer at each step.

| Setup | ENV_DIR marker | FLAG_DIR marker | CFG_DIR marker | PLATFORM marker |
|-------|---------------|-----------------|---------------|----------------|
| (a) seed | `MARKER_ENV` | `MARKER_FLAG` | `MARKER_CFG` | `MARKER_PLATFORM` |

### 7a — env wins over config (verified via real log writes)

`HOME=$FAKE_HOME AI_MEMORY_LOG_DIR=$ENV_DIR ai-memory --db ... boot --quiet`. The fake home's config has `[logging] enabled = true; path = $CFG_DIR`. After running, `$ENV_DIR/ai-memory.log` was created (size 0; appender was constructed but no log line landed for a one-shot subcommand) and `$CFG_DIR` remained empty. **OK.**

### 7b — flag overrides env

```
HOME=$FAKE_HOME AI_MEMORY_LOG_DIR=$ENV_DIR ai-memory logs --log-dir $FLAG_DIR cat
```

Captured stdout:

```
1  MARKER_FLAG
```

Only `MARKER_FLAG` appeared; `MARKER_ENV` and `MARKER_CFG` did not. **OK.**

### 7c — env wins over config (no flag)

```
HOME=$FAKE_HOME AI_MEMORY_LOG_DIR=$ENV_DIR ai-memory logs cat
```

Captured stdout:

```
1  MARKER_ENV
```

`MARKER_ENV` appeared; `MARKER_CFG` did not. **OK.**

### 7d — config wins over platform default (no env, no flag)

```
env -u AI_MEMORY_LOG_DIR HOME=$FAKE_HOME ai-memory logs cat
```

Captured stdout:

```
1  MARKER_CFG
```

`MARKER_CFG` appeared; `MARKER_ENV` did not. **OK.**

### 7e — platform default wins (no env, no flag, no config setting)

A fresh `$HOME` was used (no `[logging] path` in its auto-generated default config), and the macOS platform default `~/Library/Logs/ai-memory/` was seeded with `MARKER_PLATFORM`:

```
env -u AI_MEMORY_LOG_DIR HOME=$EMPTY_HOME ai-memory logs cat
```

Captured stdout:

```
1  MARKER_PLATFORM
```

The platform default for macOS resolved to `~/Library/Logs/ai-memory/`, matching `src/log_paths.rs` documentation. **OK.**

**Result: PASS.**

---

## Observations (informational, not failures)

1. **`install --uninstall` reorders JSON keys alphabetically** rather than preserving original key insertion order. The brief said "modulo whitespace"; in practice it's "modulo whitespace AND alphabetical key order". This is standard `serde_json::to_string_pretty` behavior on a `Map<String, Value>` (BTreeMap-like under-the-hood) and harmless — the resulting JSON object is semantically equivalent. Calling out for documentation accuracy if anyone references "byte-identical restore" downstream.

2. **`boot --db /nonexistent/path` exits 0 (not non-zero)** even though the DB cannot be opened, with the warn manifest emitted to stdout and a single diagnostic line on stderr. This is the documented contract — boot is invoked from agent SessionStart hooks and must never wedge the user's first turn — but operators expecting a non-zero exit code on DB unavailability should use `ai-memory doctor` (which does exit non-zero on critical findings) instead of `boot`.

3. **`wrap` resolves `auto_namespace` from CWD** (no flag exists to forward an explicit namespace). When the wrapping shell doesn't `cd` into a project root first, boot falls back to global Long-tier memories. Recipes in `docs/integrations/` already document this; called out here for completeness.

---

## Discovered gaps

None. All seven steps PASSED.

---

## Test artifact provenance

All raw stdout/stderr captures are in `/tmp/audit-c-smoke-work/step{2..7}*.{stdout,stderr}` on the test host (FROSTYi.local) at the time of execution. They were not committed to the repo (ephemeral tempdir contents), but the line numbers cited above match the captured files byte-for-byte.

Tempdir layout at end of run:

```
/tmp/audit-c-smoke-work/
├── test.db                                        # seeded DB (3 memories)
├── settings.json                                  # post-uninstall
├── settings.json.bak.20260430T145813.180Z         # pre-apply backup
├── settings.json.bak.20260430T145813.187Z         # pre-uninstall backup
├── audit-logs/audit.log                           # one session_boot event
├── log-env/ai-memory.log                          # MARKER_ENV
├── log-flag/ai-memory.log                         # MARKER_FLAG
├── log-cfg/ai-memory.log                          # MARKER_CFG
├── fake-home/ .config/ai-memory/config.toml       # audit-enabled
├── fake-home2/.config/ai-memory/config.toml       # logging-enabled
├── fake-home3/.config/ai-memory/config.toml       # logging-disabled, path=cfg-dir
├── empty-home/Library/Logs/ai-memory/ai-memory.log # MARKER_PLATFORM
└── step{2..7}*.{stdout,stderr}
```

The host's real `~/.claude/ai-memory.db` and `~/.claude/settings.json` were NOT touched at any point.
