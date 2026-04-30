# ai-memory enterprise audit trail

PR-5 of issue [#487](https://github.com/alphaonedev/ai-memory-mcp/issues/487).
A turnkey, enterprise-class security audit trail and operational
logging facility for AI memory activity across every AI agent that
talks to ai-memory.

This is the **operator** doc: how to turn it on, what it does, how to
ship the lines into your SIEM, and how the regulatory mappings line
up. The **developer** schema reference lives in
[`audit-schema.md`](./audit-schema.md).

---

## At a glance

| Subsystem | Default | Purpose |
|---|---|---|
| Operational logs (`tracing::*` → file) | OFF | Capture every `tracing::info!` / `tracing::warn!` / `tracing::error!` to a rotating on-disk file. Suitable for Splunk / Datadog / Elastic / Loki ingestion. |
| Security audit trail | OFF | One hash-chained, tamper-evident JSON line per memory mutation. SIEM-grade evidence for SOC2 / HIPAA / GDPR / FedRAMP. |

Both are **default-OFF for privacy.** No log lines hit the disk
without a deliberate config opt-in.

---

## Quickstart

```toml
# ~/.config/ai-memory/config.toml

[logging]
enabled = true
path = "~/.local/state/ai-memory/logs/"
max_files = 30
retention_days = 90
structured = true                 # JSON lines for SIEM ingest
level = "info"

[audit]
enabled = true
path = "~/.local/state/ai-memory/audit/"
schema_version = 1
redact_content = true
hash_chain = true
attestation_cadence_minutes = 60
append_only = true

[audit.compliance.soc2]
applied = true
retention_days = 730
attestation_cadence_minutes = 60
```

Restart the daemon (or any new CLI invocation picks up the new
config). Verify:

```bash
ai-memory audit path                    # prints resolved log path
ai-memory store --title 'hello' --content 'world'
ai-memory audit tail --lines 5          # shows the store event
ai-memory audit verify                  # exits 0 on intact chain
```

---

## What gets audited

Every memory mutation. The full action vocabulary:

- `store` — new memory written
- `update` — existing memory modified
- `delete` — memory tombstoned
- `recall` / `search` / `list` / `get` / `session_boot` — read access (one event per query, capturing namespace + actor; targets are aggregate `"*"` for list-style ops)
- `link` / `promote` / `forget` / `consolidate` — derived mutations
- `export` / `import` — bulk operations (one summary event)
- `approve` / `reject` — governance state transitions
- `session_boot` — `ai-memory boot` invocations (every AI agent's first turn)

Each event captures:

- **Who.** Resolved NHI agent_id + synthesis source (`mcp_client_info`, `http_header`, `host_fallback`, …) so a SIEM can trace claims back to the transport.
- **What.** Action + outcome (`allow | deny | error | pending`).
- **Where.** Memory id (or `*`), namespace, title (advisory label only — **never content**), tier, scope.
- **How.** Auth context for HTTP-originated events (peer IP, mTLS fingerprint, hashed API key id). Stdio (CLI / MCP) emissions omit auth entirely.
- **When.** RFC3339 UTC timestamp + per-process monotonic sequence number.
- **Tamper-evidence.** `prev_hash` + `self_hash` form a sha256 chain; verify with `ai-memory audit verify`.

## What is NEVER audited

- `memory.content` (the secret payload). The schema has no content
  field. `redact_content = true` is the only supported v1 mode.
- Raw API keys, raw mTLS private keys, raw passwords.
- Free-form caller-supplied strings outside the documented fields.

---

## Threat model

| Adversary | Defense |
|---|---|
| Local attacker edits one line | `self_hash` recomputation fails on `audit verify`; precise line number surfaces |
| Local attacker inserts a forged line | The next line's `prev_hash` no longer matches the inserted line's `self_hash` |
| Local attacker deletes one line | The line after the deletion has a `prev_hash` from a now-gone source line |
| Local attacker truncates the tail | The chain is consistent up to truncation, but periodic `CHECKPOINT.sig` markers (every `attestation_cadence_minutes`) bound rollback when paired with off-host attestation |
| Root attacker rewrites the entire file | **Not defended.** Ship the lines off-host to an immutable SIEM in real time. The on-host chain still cross-checks the SIEM record. |
| Process crashes mid-write | The `O_APPEND` write is atomic at the line level; partial writes never produce a malformed event. The chain may stop mid-stream but `audit verify` surfaces the cleanly-truncated tail without a false positive. |

The append-only OS flag (`chflags +UF_APPEND` on BSD/macOS,
`FS_IOC_SETFLAGS +FS_APPEND_FL` on Linux) is **best-effort defense in
depth**. The hash chain is the load-bearing tamper-evidence.

---

## Log directory resolution

End users can set the operational-log directory **and** the audit-log
directory at every layer of the configuration stack. This is a
**user-mandated** addendum to PR-5 — operators always retain control
over where logs land regardless of how `ai-memory` was installed or
launched.

### Precedence (highest wins)

| Priority | Layer | Operational logs | Audit log |
|---:|---|---|---|
| 1 | **CLI flag** | `ai-memory logs --log-dir <PATH> …` | `ai-memory audit --audit-dir <PATH> …` |
| 2 | **Environment variable** | `AI_MEMORY_LOG_DIR` | `AI_MEMORY_AUDIT_DIR` |
| 3 | **`config.toml`** | `[logging] path = "…"` | `[audit] path = "…"` |
| 4 | **Platform default** | per-OS table below | per-OS table below |

The resolver also recognises an `INVOCATION_ID` environment variable
(set by `systemd` for unit-managed processes). When present *and*
`/var/log/ai-memory/` is writable, the platform-default branch picks
`/var/log/ai-memory/` instead of the per-user XDG path. This lets a
`systemd` service with `LogsDirectory=ai-memory` write logs to the
canonical system path without any extra configuration.

`AI_MEMORY_LOG_DIR` and `AI_MEMORY_AUDIT_DIR` are read with
`std::env::var_os`, so non-UTF-8 paths on Windows pass through to
`PathBuf` unchanged.

### Platform defaults

| OS | Operational logs | Audit log |
|---|---|---|
| **Linux** (and BSD / illumos / other Unix) | `${XDG_STATE_HOME:-$HOME/.local/state}/ai-memory/logs/` | `${XDG_STATE_HOME:-$HOME/.local/state}/ai-memory/audit/` |
| **macOS** | `~/Library/Logs/ai-memory/` | `~/Library/Logs/ai-memory/audit/` |
| **Windows** | `%LOCALAPPDATA%\ai-memory\logs\` | `%LOCALAPPDATA%\ai-memory\audit\` |
| **systemd-managed daemon** (any OS, `INVOCATION_ID` set, `/var/log/ai-memory/` writable) | `/var/log/ai-memory/logs/` | `/var/log/ai-memory/audit/` |

### Worked examples

**Laptop dev (no config — accept the default).**

```bash
$ ai-memory audit path
/Users/alice/Library/Logs/ai-memory/audit/audit.log

$ ai-memory logs tail --lines 5
# tails ~/Library/Logs/ai-memory/ai-memory.log.YYYY-MM-DD
```

**Docker container with a host-mounted log volume.** Mount the host
directory into a stable container path, then point `ai-memory` at it
with `AI_MEMORY_LOG_DIR` so the env-injected path wins over any
baked-in `config.toml`:

```bash
docker run -d \
  -v /var/log/ai-memory-host:/var/log/ai-memory \
  -e AI_MEMORY_LOG_DIR=/var/log/ai-memory/logs \
  -e AI_MEMORY_AUDIT_DIR=/var/log/ai-memory/audit \
  ghcr.io/alphaonedev/ai-memory:0.6.3
```

**Kubernetes pod with `emptyDir` volume.** Project the volume into
`/var/log/ai-memory/` and point both env vars at the matching
subdirectories. Use a sidecar log shipper (Promtail, Filebeat,
Fluentbit) to forward both streams off-pod before termination.

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: ai-memory
spec:
  containers:
    - name: ai-memory
      image: ghcr.io/alphaonedev/ai-memory:0.6.3
      env:
        - name: AI_MEMORY_LOG_DIR
          value: /var/log/ai-memory/logs
        - name: AI_MEMORY_AUDIT_DIR
          value: /var/log/ai-memory/audit
      volumeMounts:
        - name: ai-memory-logs
          mountPath: /var/log/ai-memory
  volumes:
    - name: ai-memory-logs
      emptyDir: {}
```

**systemd unit with `LogsDirectory=`.** systemd creates and chowns the
directory to the unit's `User=`; `ai-memory` auto-detects via
`INVOCATION_ID` and lands logs in `/var/log/ai-memory/`:

```ini
[Service]
ExecStart=/usr/local/bin/ai-memory serve
User=ai-memory
LogsDirectory=ai-memory
LogsDirectoryMode=0700
```

No env vars or `config.toml` paths required — the platform-default
branch picks `/var/log/ai-memory/` because `INVOCATION_ID` is set and
the directory is writable.

**Override at the CLI for a one-off run** (debugging, audit forensics):

```bash
ai-memory audit --audit-dir /tmp/ai-memory-forensics verify
ai-memory logs --log-dir /tmp/ai-memory-debug tail --follow
```

### Security guard: no world-writable directories

The resolver **refuses** to write to a directory whose Unix permissions
include the world-writable bit (`mode & 0o002 != 0`). World-writable
log destinations are a pivot target — any local user could append
forged events, truncate the chain, or replace files atomically. The
error message names the resolution layer that landed there so the
operator can fix the right config:

```
Error: log directory /tmp/foo is world-writable (mode 0777); refusing
for security. Resolved via: CLI flag (--log-dir / --audit-dir).
Pick a non-world-writable directory and re-run.
```

When `ai-memory` creates the directory itself, it applies mode `0700`
on Unix. On Windows the default ACL (Authenticated Users only) is
sufficient.

---

## Operator CLI

### `ai-memory audit verify`

Walks the audit log, recomputes every line's `self_hash`, and asserts
each `prev_hash` matches the prior line's `self_hash`. Exits:

- `0` — chain intact
- `2` — chain broken (precise line + failure kind printed)
- non-zero with anyhow context — I/O error

```bash
$ ai-memory audit verify
audit verify OK: 1428 line(s) verified at /home/op/.local/state/ai-memory/audit/audit.log

$ ai-memory audit verify --json
{"status":"ok","total_lines":1428,"path":"…/audit.log"}

$ ai-memory audit verify   # after a tamper
audit verify FAIL at line 203: SelfHash — self_hash mismatch: stored=ab…, recomputed=cd…
```

### `ai-memory audit tail`

Print recent events, optionally filtered:

```bash
ai-memory audit tail --lines 100 --action store
ai-memory audit tail --namespace finance --format json | jq .
ai-memory audit tail --actor 'ai:claude-code@laptop'
```

### `ai-memory audit path`

Prints the resolved audit log path. Convenient for SIEM ingestion
configuration scripts. Honours the same `--audit-dir <PATH>` override
as every other `ai-memory audit` subcommand, so you can point at an
ad-hoc location for one-off inspection:

```bash
ai-memory audit --audit-dir /var/lib/forensics/2026-04-30 path
```

### `ai-memory logs tail [--follow]`

Tail and (optionally) stream operational logs. Accepts the global
`--log-dir <PATH>` override. See the **Log directory resolution**
section above for the full precedence ladder.

### `ai-memory logs archive`

zstd-compresses rotated log files past the configured
`retention_days`. Idempotent.

### `ai-memory logs purge --before <date>`

Delete archived logs older than `<date>`. Surfaces a
**audit-gap warning** when the cutoff date overlaps the configured
audit retention horizon — deleting audit logs creates a compliance
hole the next `audit verify` (or external attestation) will surface.

---

## SIEM ingestion guide

The audit and operational log lines are plain UTF-8 JSON. Any SIEM
that ingests JSON ingests this. Recipes for the four most common:

### Splunk Universal Forwarder

`inputs.conf`:

```conf
[monitor:///home/op/.local/state/ai-memory/audit/audit.log]
sourcetype = ai-memory:audit
index = security_audit
disabled = 0

[monitor:///home/op/.local/state/ai-memory/logs/ai-memory.log.*]
sourcetype = ai-memory:ops
index = ai_ops
disabled = 0
```

`props.conf`:

```conf
[ai-memory:audit]
INDEXED_EXTRACTIONS = json
TIMESTAMP_FIELDS = timestamp
KV_MODE = none
```

### Datadog Agent

`/etc/datadog-agent/conf.d/ai_memory.d/conf.yaml`:

```yaml
logs:
  - type: file
    path: /home/op/.local/state/ai-memory/audit/audit.log
    service: ai-memory
    source: ai-memory-audit
    log_processing_rules:
      - type: include_at_match
        name: keep_all
        pattern: ".*"
  - type: file
    path: /home/op/.local/state/ai-memory/logs/ai-memory.log*
    service: ai-memory
    source: ai-memory-ops
```

Pair with the [JSON parser]([https://docs.datadoghq.com/logs/log_configuration/parsing/](https://docs.datadoghq.com/logs/log_configuration/parsing/))
for the audit pipeline.

### Elastic Filebeat

`filebeat.yml`:

```yaml
filebeat.inputs:
  - type: filestream
    id: ai-memory-audit
    paths:
      - /home/op/.local/state/ai-memory/audit/audit.log
    parsers:
      - ndjson:
          target: ai_memory_audit
          add_error_key: true
    fields:
      service: ai-memory
      stream: audit
  - type: filestream
    id: ai-memory-ops
    paths:
      - /home/op/.local/state/ai-memory/logs/ai-memory.log*
    fields:
      service: ai-memory
      stream: operational
```

### Loki / Promtail

`promtail.yaml`:

```yaml
scrape_configs:
  - job_name: ai-memory-audit
    static_configs:
      - targets: [localhost]
        labels:
          service: ai-memory
          stream: audit
          __path__: /home/op/.local/state/ai-memory/audit/audit.log
    pipeline_stages:
      - json:
          expressions:
            timestamp: timestamp
            action: action
            actor: actor.agent_id
            namespace: target.namespace
            outcome: outcome
      - timestamp:
          source: timestamp
          format: RFC3339
      - labels:
          action:
          outcome:

  - job_name: ai-memory-ops
    static_configs:
      - targets: [localhost]
        labels:
          service: ai-memory
          stream: operational
          __path__: /home/op/.local/state/ai-memory/logs/ai-memory.log*
```

---

## Regulatory mapping

The compliance presets propagate well-known retention and cadence
controls into the effective config. Set `applied = true` for the
relevant preset; ai-memory picks the most-conservative value when
multiple presets are active.

| Preset | Citation | Retention | Cadence | Notes |
|---|---|---|---|---|
| `soc2` | TSC CC7.2 | 2 years | 60 min | Continuous monitoring of audit logs. |
| `hipaa` | 45 CFR §164.316(b)(2) | 6 years | — | Pair with `--features sqlcipher` for required at-rest crypto. |
| `gdpr` | Art. 30 + Art. 5(1)(e) | 3 years | — | `pseudonymize_actors` reserved for v0.7+. |
| `fedramp` | NIST SP 800-53 AU-11 / AU-12 | 3 years | 30 min | High-water mark for federal civilian / DoD IL2-IL5. |

The presets are configuration only. Compliance certification still
requires the broader control environment (access reviews, change
management, incident response). The audit trail is one piece of the
evidence package, not the whole thing.

---

## Operational runbook

### Rotation

The rolling appender writes one file per `rotation` cadence (default
daily). `max_files` retained on disk; older files are removed by the
appender. `ai-memory logs archive` zstd-compresses files past
`retention_days` for cold-storage handoff to the SIEM.

### Verification cadence

Run `ai-memory audit verify` from a SIEM-monitored cron / systemd
timer at least daily. A failure is a P0 — somebody touched the file.

```service
# /etc/systemd/system/ai-memory-audit-verify.service
[Unit]
Description=Verify ai-memory audit chain

[Service]
Type=oneshot
ExecStart=/usr/local/bin/ai-memory audit verify --json
SyslogIdentifier=ai-memory-audit-verify
```

```service
# /etc/systemd/system/ai-memory-audit-verify.timer
[Unit]
Description=Hourly ai-memory audit chain verification
[Timer]
OnCalendar=hourly
[Install]
WantedBy=timers.target
```

### Off-host attestation

Ship every line to an immutable off-host store (SIEM, S3 Object Lock,
WORM appliance) in real time. The on-host hash chain serves as a
cross-check for the off-host record.

### Incident response

A failed `audit verify` means the audit log has been tampered with.
The chain itself tells you where (precise line number + failure kind).
Cross-reference the timestamp with:

1. The off-host SIEM ingest stream (the immutable copy the on-host
   chain cross-checks against).
2. Operating-system audit (auditd / OSSEC / EndPoint EDR) for
   unauthorized writes to the log path.
3. `ai-memory doctor` for related runtime anomalies.
