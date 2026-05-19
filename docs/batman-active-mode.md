# Activating Batman Mode — full-spectrum write-time-investment substrate

> v0.7.0 ships **6 of 6 of Batman's write-time-investment forms + the 7th**
> (substrate-authority at write) — all `IMPLEMENTED` per the post-closeout
> state in [`docs/internal/batman-framework-audit.md`](internal/batman-framework-audit.md).
> A default install is **Batman-capable, not Batman-active**: opt-ins are
> off, the operator key doesn't exist, R001–R004 seed rules are unsigned
> and disabled, the curator daemon isn't running, and per-namespace
> policies for Form 5 shadow_mode + Form 6 auto_classify default off.
>
> This document is the operator-facing how-to for going from
> Batman-capable → Batman-active on a single node (T1/T2 in the
> [architectures matrix](https://alphaonedev.github.io/ai-memory-mcp/architectures.html)).
> Tracking: [issue #800](https://github.com/alphaonedev/ai-memory-mcp/issues/800).

## What is Batman Mode?

"Batman" refers to the 6-form write-time-investment framework published
in *"Pay at write time, read for free"*. The thesis: memory quality is
set at WRITE time, not retrieval time. No amount of rerank fixes a
substrate that ingested duplicates, un-atomized blobs, untyped facts,
no provenance, no confidence, and no governance.

ai-memory v0.7.0 implements all 6 + adds a 7th (substrate-authority over
agent-EXTERNAL actions at write time). The forms:

| Form | What it does | Activation gate |
|---|---|---|
| 1 — Online dedup-and-synthesis | One batch action-emitting LLM call BEFORE write (add/update/delete/no-op verbs). | **Always on** in MCP write path (autonomous tier) |
| 2 — Synchronous atomise-before-embed | Source is decomposed into atoms BEFORE embedding so atoms get vectors. | **Schema always on; behavior gated on `GovernancePolicy.auto_atomise = true` + `auto_atomise_mode = "synchronous"` in a namespace standard memory** |
| 3 — Multi-step ingest orchestrator | Prompt-cache reuse + explicit-trust deterministic-then-LLM helpers. | **Always available** as `memory_ingest_multistep` MCP tool (`Family::Power`+) |
| 4 — Fact-provenance | `citations` + `source_uri` + `source_span` (atom-grain span) columns. | **Always on** in store schema (sqlite v38+) |
| 5 — Confidence + shadow-mode + freshness decay | Auto-confidence calibration; shadow-mode side-by-side scoring; exponential freshness decay on recall. | **Three process-level env vars** on the MCP server + curator daemon: `AI_MEMORY_AUTO_CONFIDENCE=1`, `AI_MEMORY_CONFIDENCE_SHADOW=1`, `AI_MEMORY_CONFIDENCE_DECAY=1` |
| 6 — MemoryKind vocabulary | 10-variant enum: `Observation` / `Reflection` / `Persona` / `Concept` / `Entity` / `Claim` / `Relation` / `Event` / `Conversation` / `Decision`. | Vocabulary always in schema (`memories.memory_kind`); **auto-classify gated on `GovernancePolicy.auto_classify_kind = "regex_then_llm"` (or `regex_only`) in a namespace standard memory** |
| 7 — Substrate-authority at write | `check_agent_action` consulted at substrate-internal write paths; v0.8.0 wires the agent-EXTERNAL surface. | **Operator key + R001–R004 signed + enabled** |

Forms 1, 3, 4 are on by default in the MCP write path the moment you
launch with `--tier autonomous`. Forms 2, 5, 6, 7 need explicit
activation — Form 2 + 6 via a namespace standard memory, Form 5 via
process env vars on the MCP + curator, Form 7 via the operator key and
the four seed rules. This guide covers all four activations and how to
make them permanent.

## Prerequisites

- ai-memory `v0.7.0` or later (`ai-memory --version`)
- Ollama running locally with `gemma4:e4b` and `nomic-embed-text-v1.5`
  pulled (or your equivalent autonomous-tier stack)
- The MCP server launched with `--tier autonomous` and a non-`core`
  profile (`--profile full` exposes all 71 tools at v0.7.0; `power` is
  the recommended ceiling for normal operators)
- `~/.config/ai-memory/config.toml` exists (default location; macOS uses
  `~/Library/Application Support/ai-memory/` for keys but `~/.config/`
  for the config file)

Recommended `config.toml`:

```toml
tier = "autonomous"
db = "~/.claude/ai-memory.db"
ollama_url = "http://localhost:11434"
embed_url = "http://localhost:11434"
embedding_model = "nomic_embed_v15"
llm_model = "gemma4:e4b"
cross_encoder = true
default_namespace = "main"

[mcp]
profile = "full"
```

## The 7-step recipe

> **Heads-up — known wart in v0.7.0.** `ai-memory rules keygen` writes
> the operator key to `<config-dir>/operator.key`, but `ai-memory rules
> enable` looks in `<config-dir>/keys/operator.key`. Step 1 below
> includes the one-line `mv` workaround. Tracking issue: see
> [§"Known wart"](#known-wart) at the end of this document.

Set the DB path once so the rest of the recipe doesn't have to repeat
it:

```bash
export AI_MEMORY_DB=~/.claude/ai-memory.db
```

### Step 1 — Generate the operator keypair

The operator key is the Ed25519 keypair that signs governance rule
mutations. Without it, `rules enable / disable / remove / add` all
refuse with `governance.no_operator_key`. Read-only verbs (`list`,
`check`) work unsigned.

```bash
ai-memory rules keygen
```

Output:

```
Ed25519 operator key generated: <fingerprint> -> <key-dir>/operator.key
{
  "fingerprint": "...",
  "path": ".../operator.key",
  "public_path": ".../operator.key.pub"
}
```

**Workaround for the v0.7.0 path mismatch (macOS / Linux):**

```bash
# macOS
KEYDIR="$HOME/Library/Application Support/ai-memory/keys"
# Linux
# KEYDIR="$HOME/.config/ai-memory/keys"

mv "$(dirname "$KEYDIR")/operator.key"     "$KEYDIR/operator.key"
mv "$(dirname "$KEYDIR")/operator.key.pub" "$KEYDIR/operator.key.pub"
ls -la "$KEYDIR/"
```

Expected files in the keys directory after the move:

| File | Mode | Purpose |
|---|---|---|
| `operator.key` | `0600` | 32-byte Ed25519 seed (private — guard it) |
| `operator.key.pub` | `0644` | Base64url public verifier |
| `daemon.priv` / `daemon.pub` | `0600` / `0644` | Per-daemon agent identity (separate from the operator key) |

### Step 2 — Sign the seed rules

R001–R004 ship in the `governance_rules` table with `enabled=0` and
`attest_level='unsigned'`. `sign-seed` flips them to
`attest_level='operator_signed'` and populates `signature_b64`. It does
**not** enable them — that's step 3.

```bash
ai-memory rules sign-seed
```

Output (one block per rule):

```json
{
  "rules": [
    { "id": "R001", "attest_level": "operator_signed", "signed_now": true },
    { "id": "R002", "attest_level": "operator_signed", "signed_now": true },
    { "id": "R003", "attest_level": "operator_signed", "signed_now": true },
    { "id": "R004", "attest_level": "operator_signed", "signed_now": true }
  ],
  "signed_now": 4
}
```

The seed rules and what they enforce:

| Rule | Kind | Refuses |
|---|---|---|
| R001 | `filesystem_write` | Any write under `/tmp/**` |
| R002 | `filesystem_write` | Any write under `/var/tmp/**` |
| R003 | `filesystem_write` | Any write under `/private/tmp/**` (macOS realpath of `/tmp`) |
| R004 | `process_spawn` | `cargo` when free disk is under 20 GiB |

### Step 3 — Enable each rule

```bash
for r in R001 R002 R003 R004; do
  ai-memory rules enable --id "$r" --sign
done
```

Each enable prints the full rule JSON with `enabled: true` and a
populated `signature_b64`. The `--sign` flag is mandatory for mutation
verbs — see the `governance.no_operator_key` error path if the operator
key isn't on disk.

### Step 4 — Smoke-test Form 7 enforcement

```bash
# Should REFUSE under R001 (writes to /tmp blocked)
ai-memory rules check \
  --kind filesystem_write \
  --payload '{"path":"/tmp/foo.txt"}' \
  --agent-id smoke-test
# → {"decision":"refuse","reason":"...no /tmp writes...","rule_id":"R001"}

# Should ALLOW (path outside the refused set)
ai-memory rules check \
  --kind filesystem_write \
  --payload "{\"path\":\"$HOME/.local-runs/foo.txt\"}" \
  --agent-id smoke-test
# → {"decision":"allow"}
```

If both return as expected, Form 7 is live.

### Step 5 — Start the curator daemon

The curator is the autonomous upkeep loop. It runs Form 1 background
sweeps (post-write dedup detection on rows the in-process write-path
already saw), Form 5 freshness-decay sweeps, Form 6 auto-classify
backfills on rows that arrived before a namespace's `auto_classify`
policy was set, and Rule-5 consolidation. The `--max-ops` flag caps
LLM-invoking operations per cycle.

```bash
mkdir -p ~/Library/Logs/ai-memory   # macOS; Linux uses ~/.local/state/ai-memory/log

ai-memory curator --daemon \
  --interval-secs 300 \
  --max-ops 100 \
  >> ~/Library/Logs/ai-memory/curator.log 2>&1 &
```

For permanent operation across reboot, skip the manual launch above and
use the OS service manager — see [§"Making it permanent"](#making-it-permanent)
below.

### Step 6 — (Optional) Start the reflection-pass curator

The reflection-pass curator clusters co-recalled `Observation` memories
and synthesises typed `Reflection` memories with `reflects_on`
provenance. It runs at a longer interval than the main curator (the
LLM work per cycle is heavier).

```bash
ai-memory curator --reflect --all-namespaces \
  --interval-secs 1800 \
  --max-depth 3 \
  >> ~/Library/Logs/ai-memory/curator-reflect.log 2>&1 &
```

The `--max-depth` flag is the curator-side reflection-depth ceiling.
Per-namespace `max_reflection_depth` policy is enforced on top — this
flag refuses to propose reflections that would exceed an
operator-supplied cap so the curator never burns an LLM round-trip on a
doomed write.

### Step 7 — Per-namespace standard memory (Forms 2 + 6) and process env vars (Form 5)

These are two distinct activation mechanisms because they live in two
different parts of the system.

**Form 5 — process env vars on the MCP server + curator daemon**

Form 5 is opt-in via three independent env vars read by the substrate
at hot-path checkpoints. Set them on both the MCP server invocation
(so writes get auto-confidence and shadow observations) and the
curator daemon (so the decay sweep runs):

| Env var | Effect |
|---|---|
| `AI_MEMORY_AUTO_CONFIDENCE=1` | Per-source-namespace baseline confidence derived from calibration history rather than caller-supplied default |
| `AI_MEMORY_CONFIDENCE_SHADOW=1` | Side-by-side record of caller-supplied vs. system-derived confidence into `confidence_shadow_observations` |
| `AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE=1.0` | Optional sampling knob (0.0 → 1.0); defaults to a low fraction so existing installs don't explode the side-channel volume |
| `AI_MEMORY_CONFIDENCE_DECAY=1` | Exponential freshness decay applied at recall time; the curator sweeps stale rows in the background |

For the MCP server launched by Claude Code, set them in the MCP entry
of `~/.claude.json`:

```json
{
  "mcpServers": {
    "memory": {
      "command": "ai-memory",
      "args": ["--db", "~/.claude/ai-memory.db", "mcp", "--tier", "autonomous"],
      "env": {
        "AI_MEMORY_AUTO_CONFIDENCE": "1",
        "AI_MEMORY_CONFIDENCE_SHADOW": "1",
        "AI_MEMORY_CONFIDENCE_DECAY": "1"
      }
    }
  }
}
```

For the curator daemon under launchd / systemd, add the same three
variables to the `EnvironmentVariables` dict in the plist (macOS) or
the `Environment=` lines of the unit (Linux) — see
[§"Making it permanent"](#making-it-permanent).

**Forms 2 and 6 — namespace standard memory with `GovernancePolicy`**

A namespace standard is a regular memory whose `metadata.governance`
field carries the policy. `memory_namespace_set_standard` points a
namespace at that memory's id. The full `GovernancePolicy` surface
relevant to Batman:

| Field | Form | Value to flip on |
|---|---|---|
| `auto_atomise` | 2 | `true` |
| `auto_atomise_mode` | 2 | `"synchronous"` (vs. `"deferred"` or `"off"`) |
| `auto_atomise_threshold_cl100k` | 2 | e.g. `512` (tokens; below this, no atomisation) |
| `auto_atomise_max_atom_tokens` | 2 | e.g. `256` |
| `auto_classify_kind` | 6 | `"regex_then_llm"` (or `"regex_only"`) |
| `max_reflection_depth` | recursive-learning | e.g. `3` (default ceiling) |

Two paths to set this:

**Option A — via the MCP `memory_namespace_set_standard` tool (recommended)**

From any connected MCP agent (or `ai-memory mcp` stdio call), first
create the policy memory with the governance shape, then point the
namespace at it:

```jsonc
// 1. memory_store
{
  "namespace": "main",
  "title": "batman-active standard for main",
  "content": "Namespace standard: Form 2 sync atomise + Form 6 regex+LLM classify.",
  "metadata": {
    "governance": {
      "auto_atomise": true,
      "auto_atomise_mode": "synchronous",
      "auto_atomise_threshold_cl100k": 512,
      "auto_atomise_max_atom_tokens": 256,
      "auto_classify_kind": "regex_then_llm",
      "max_reflection_depth": 3,
      "write": "owner",
      "promote": "any",
      "delete": "owner",
      "approver": "human",
      "inherit": true
    }
  }
}
// → returns {"id": "<UUID>"}

// 2. memory_namespace_set_standard
{ "namespace": "main", "id": "<UUID-from-step-1>" }
```

**Option B — via SQL (last resort, bypasses validation)**

```bash
sqlite3 "$AI_MEMORY_DB" <<'SQL'
-- replace with a freshly-minted UUID
INSERT OR REPLACE INTO namespace_meta(namespace, standard_id, updated_at)
VALUES ('main', '<uuid-of-policy-memory>', datetime('now'));
SQL
```

The `write/promote/delete/approver/inherit` keys are the v0.6.x
operator governance policy and remain required by the validator
(`memory_namespace_set_standard` will refuse a payload missing them).
Repeat per namespace you want Batman-active.

## Verification

After steps 1–7, verify Batman-active state in one block:

```bash
# 1. Rules enabled + signed
ai-memory rules list --json | jq '[.result[] | {id, enabled, attest_level}]'
# Expect: all 4 enabled=true, attest_level="operator_signed"

# 2. Curator processes alive
pgrep -fl ai-memory
# Expect: at least the curator --daemon line; reflection-pass if you started it

# 3. Form 7 enforcement live (negative test)
ai-memory rules check --kind filesystem_write \
  --payload '{"path":"/tmp/x"}' --agent-id verify
# Expect: {"decision":"refuse","rule_id":"R001"}

# 4. Namespace standards populated (Forms 2 + 6)
sqlite3 "$AI_MEMORY_DB" \
  "SELECT namespace, standard_id FROM namespace_meta WHERE standard_id IS NOT NULL;"
# Expect: a row per Batman-active namespace, standard_id = the policy memory's UUID

# 5. Form 5 env vars wired into the MCP launch
python3 -c "
import json
cfg = json.load(open('$HOME/.claude.json'))
env = cfg.get('mcpServers', {}).get('memory', {}).get('env', {})
needed = ['AI_MEMORY_AUTO_CONFIDENCE', 'AI_MEMORY_CONFIDENCE_SHADOW', 'AI_MEMORY_CONFIDENCE_DECAY']
print({k: env.get(k, 'MISSING') for k in needed})
"
# Expect: {'AI_MEMORY_AUTO_CONFIDENCE': '1', 'AI_MEMORY_CONFIDENCE_SHADOW': '1', 'AI_MEMORY_CONFIDENCE_DECAY': '1'}
```

The acceptance suite at
[`scripts/batman-mode-acceptance.sh`](../scripts/batman-mode-acceptance.sh)
codifies all of the above into 22+ checks across the seven forms +
upkeep + permanence. Run it after each activation step to see what's
green and what's still dormant.

## Making it permanent

The operator key, signed/enabled rules, and namespace policies all live
on disk (key) or in SQLite (rules + namespace_standards) and survive
reboot. The curator daemon does not — it needs the OS service manager.

### macOS — launchd LaunchAgent

`~/Library/LaunchAgents/dev.alphaone.ai-memory.curator.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.alphaone.ai-memory.curator</string>

  <key>ProgramArguments</key>
  <array>
    <string>/Users/YOU/.local/bin/ai-memory</string>
    <string>--db</string>
    <string>/Users/YOU/.claude/ai-memory.db</string>
    <string>curator</string>
    <string>--daemon</string>
    <string>--interval-secs</string>
    <string>300</string>
    <string>--max-ops</string>
    <string>100</string>
  </array>

  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>/Users/YOU/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>
    <key>HOME</key>
    <string>/Users/YOU</string>
  </dict>

  <key>RunAtLoad</key><true/>

  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key><false/>
    <key>Crashed</key><true/>
  </dict>

  <key>ThrottleInterval</key><integer>30</integer>

  <key>StandardOutPath</key>
  <string>/Users/YOU/Library/Logs/ai-memory/curator.log</string>
  <key>StandardErrorPath</key>
  <string>/Users/YOU/Library/Logs/ai-memory/curator.log</string>

  <key>ProcessType</key><string>Background</string>
  <key>Nice</key><integer>5</integer>
</dict>
</plist>
```

Load:

```bash
launchctl bootstrap "gui/$(id -u)" \
  ~/Library/LaunchAgents/dev.alphaone.ai-memory.curator.plist

launchctl print "gui/$(id -u)/dev.alphaone.ai-memory.curator" \
  | grep -E '(state|pid) ='
```

Unload (when you want to stop):

```bash
launchctl bootout "gui/$(id -u)/dev.alphaone.ai-memory.curator"
```

For the optional reflection-pass curator, duplicate the plist as
`dev.alphaone.ai-memory.curator-reflect.plist` with `--reflect
--all-namespaces --interval-secs 1800` swapped into ProgramArguments.

### Linux — systemd user unit

`~/.config/systemd/user/ai-memory-curator.service`:

```ini
[Unit]
Description=ai-memory autonomous curator
After=network.target

[Service]
Type=simple
ExecStart=%h/.local/bin/ai-memory \
  --db %h/.claude/ai-memory.db curator --daemon \
  --interval-secs 300 --max-ops 100
Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin
StandardOutput=append:%h/.local/state/ai-memory/log/curator.log
StandardError=append:%h/.local/state/ai-memory/log/curator.log
Restart=on-failure
RestartSec=30s
Nice=5

[Install]
WantedBy=default.target
```

Load:

```bash
mkdir -p ~/.local/state/ai-memory/log
systemctl --user daemon-reload
systemctl --user enable --now ai-memory-curator.service
systemctl --user status ai-memory-curator.service
```

For reboot-survival on a headless box, also run
`loginctl enable-linger $USER` so user services run without a login
session.

### Windows — Task Scheduler

```powershell
$Action = New-ScheduledTaskAction `
  -Execute "C:\Users\YOU\.local\bin\ai-memory.exe" `
  -Argument "--db C:\Users\YOU\.claude\ai-memory.db curator --daemon --interval-secs 300 --max-ops 100"

$Trigger = New-ScheduledTaskTrigger -AtLogOn -User "YOU"

$Settings = New-ScheduledTaskSettingsSet `
  -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) `
  -StartWhenAvailable -DontStopOnIdleEnd

Register-ScheduledTask `
  -TaskName "ai-memory curator" `
  -Action $Action -Trigger $Trigger -Settings $Settings `
  -Description "ai-memory autonomous curator daemon"
```

## Rollback

To return to Batman-capable-but-inactive:

```bash
# Stop the daemons
launchctl bootout "gui/$(id -u)/dev.alphaone.ai-memory.curator" 2>/dev/null
launchctl bootout "gui/$(id -u)/dev.alphaone.ai-memory.curator-reflect" 2>/dev/null
# Linux: systemctl --user disable --now ai-memory-curator.service

# Disable rules
for r in R001 R002 R003 R004; do
  ai-memory rules disable --id "$r" --sign
done

# (Optional) Flip namespace policies off
sqlite3 "$AI_MEMORY_DB" "UPDATE namespace_standards SET \
  shadow_mode_enabled=0, auto_classify_enabled=0, freshness_decay_enabled=0;"
```

The operator key on disk is left alone — keep it, rotate it via a fresh
`ai-memory rules keygen` if you suspect compromise, or `rm` it if you
genuinely want to retire the operator role on this node. Disabled rules
remain in the database with their signatures intact; `enable` flips
them back on without re-signing.

## Form-by-form activation map

| Form | What it does | After this recipe |
|---|---|---|
| 1 — Online dedup-and-synthesis | Already in MCP write path | ACTIVE (autonomous tier) |
| 2 — Synchronous atomise-before-embed | Already in MCP write path | ACTIVE |
| 3 — Multi-step ingest | Already in MCP write path | ACTIVE |
| 4 — Citations + source-URI + atom-grain | Already in store schema | ACTIVE |
| 5 — Confidence + freshness decay | Curator daemon + namespace `shadow_mode` | ACTIVE |
| 6 — MemoryKind auto-classify | Namespace `auto_classify` policy | ACTIVE (curator backfills idle rows) |
| 7 — Agent-EXTERNAL governance | Operator key + R001–R004 enabled | ACTIVE (substrate-INTERNAL only at v0.7.0; agent-EXTERNAL Layer-4 surface wires fully at v0.8.0 per [#697](https://github.com/alphaonedev/ai-memory-mcp/issues/697)) |

The qualifier on Form 7 matters: at v0.7.0, the wired surface is
substrate-internal (any path that calls `memory_store` / `memory_link`
/ `memory_delete` / `memory_archive` / `memory_consolidate` /
`memory_replay`). The agent-EXTERNAL Layer-4 surface (Bash /
FilesystemWrite outside the substrate / NetworkRequest / ProcessSpawn)
is `callable_now=false` until the v0.8.0 harness-boundary wiring lands.
Operators can already enforce the same rules at the harness boundary
themselves via a `PreToolUse` hook that shells out to `ai-memory rules
check`; the [cookbook recipe](https://github.com/alphaonedev/ai-memory-mcp/tree/feat/v0.7.0-grand-slam/cookbook/agent-external-governance)
documents that pattern.

## Known wart

`ai-memory rules keygen` writes the operator key to
`<config-dir>/operator.key`, but `ai-memory rules enable` looks in
`<config-dir>/keys/operator.key`. The Step 1 workaround moves the files
into the expected location. The fix is a one-line change — either
keygen should write into `keys/` or enable should fall back to
`<config-dir>/operator.key`. Tracked separately on the issue list under
the `bug` / `governance` labels.

## Related documentation

- [`docs/governance.md`](governance.md) — operator-facing governance index (modes, where to read what)
- [`docs/policy-engine.md`](policy-engine.md) — 7th-form policy engine deep-dive (substrate-authoritative rules)
- [`docs/governance/agent-action-rules.md`](governance/agent-action-rules.md) — agent-action rule catalogue
- [`docs/internal/batman-framework-audit.md`](internal/batman-framework-audit.md) — adversarial audit that drove the Forms 1–6 + 7th-form closeout wave (PR #753)
- [`docs/internal/v070-feature-inventory.md`](internal/v070-feature-inventory.md) — canonical post-wave feature inventory
- [`docs/atomisation.md`](atomisation.md) — Form 2 internals
- [`docs/multistep-ingest.md`](multistep-ingest.md) — Form 3 internals
- [`docs/provenance.md`](provenance.md) — Form 4 internals
- [`docs/confidence-calibration.md`](confidence-calibration.md) — Form 5 internals
- [`docs/memory-kind-vocab.md`](memory-kind-vocab.md) — Form 6 internals
