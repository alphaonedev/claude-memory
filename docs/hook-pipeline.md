# Hook pipeline (Track G — 25 lifecycle events)

v0.7.0 ships a programmable extension surface that fires on every
substrate lifecycle point. Hooks return one of `Allow`,
`Modify(delta)`, `Deny{reason, code}`, or `AskUser{prompt, options, default}`.
Default off — a v0.7.0 install with no `hooks.toml` behaves
identically to v0.6.4 at the lifecycle layer.

- **Code paths:** [`src/hooks/mod.rs`](../src/hooks/mod.rs),
  [`src/hooks/chain.rs`](../src/hooks/chain.rs),
  [`src/hooks/config.rs`](../src/hooks/config.rs),
  [`src/hooks/decision.rs`](../src/hooks/decision.rs),
  [`src/hooks/events.rs`](../src/hooks/events.rs),
  [`src/hooks/executor.rs`](../src/hooks/executor.rs),
  [`src/hooks/recall.rs`](../src/hooks/recall.rs),
  [`src/hooks/timeouts.rs`](../src/hooks/timeouts.rs),
  pre-store hook subtree under [`src/hooks/pre_store/`](../src/hooks/pre_store/),
  post-reflect hook subtree under [`src/hooks/post_reflect/`](../src/hooks/post_reflect/).
- **Helper binary:** [`tools/auto-link-detector/`](../tools/auto-link-detector/)
  is the R3 reference `pre_link` hook (~775 LoC).
- **Capability registry entry:** `CapabilityHooks` in
  [`src/config.rs:703`](../src/config.rs).
- **Config file:** `~/.config/ai-memory/hooks.toml` — hot-reloadable
  via `SIGHUP` ([`src/hooks/config.rs:411`](../src/hooks/config.rs)).

## Configuration

```toml
[[hook]]
event = "post_store"
command = "/usr/local/bin/auto-link-detector"
priority = 100
timeout_ms = 5000
mode = "daemon"          # daemon | exec (optional; default per event class)
enabled = true
namespace = "team/*"     # glob match (today: non-empty string accepted)
fail_mode = "open"       # open (default) | closed
```

Fields ([`src/hooks/config.rs:174-190`](../src/hooks/config.rs)):

- **`event`** — one of the 25 events below.
- **`command`** — absolute path to the helper binary.
- **`priority`** — higher fires first; first `Deny` short-circuits the chain.
- **`timeout_ms`** — wall-clock budget per call; capped at
  `MAX_TIMEOUT_MS = 30_000` ([`src/hooks/config.rs:138`](../src/hooks/config.rs)).
  Exceeded → executor returns `Timeout`; chain converts per `fail_mode`.
- **`mode`** — `daemon` (long-lived subprocess, stdin JSON-RPC) or
  `exec` (one-shot fork+exec). Optional in TOML; missing values resolve
  via `default_mode_for_event` ([`src/hooks/config.rs:157`](../src/hooks/config.rs))
  — daemon for hot-path events (`post_recall`, `post_search`,
  `pre_recall_expand`), exec otherwise.
- **`enabled`** — soft-disable without removing the row.
- **`namespace`** — glob pattern; chain is filtered before invocation.
  Validation is shape-only today (`validate_hook` at
  [`src/hooks/config.rs:297`](../src/hooks/config.rs)); the runtime
  matcher is a substrate-side prefix check.
- **`fail_mode`** — `open` (default; executor errors → chain logs
  warning, treats hook as `Allow`) or `closed` (executor errors →
  chain `Deny` and short-circuit). Use `closed` only for
  compliance-critical hooks (PII redaction, regulated-tenant access
  control) where silent fail-open is worse than a hard refusal.
  Defined at [`src/hooks/config.rs:111-122`](../src/hooks/config.rs).

## 25-event matrix

The 20 baseline events:

| Event | Phase | Class | Fires on |
|---|---|---|---|
| `pre_store` / `post_store` | write | Write | `memory_store`, `memory_update` (when content changes) |
| `pre_recall` / `post_recall` | read | Read | `memory_recall`, family-loader recall |
| `pre_search` / `post_search` | read | Read | `memory_search` |
| `pre_delete` / `post_delete` | write | Write | `memory_delete` |
| `pre_promote` / `post_promote` | write | Write | tier promotion (manual + auto) |
| `pre_link` / `post_link` | write | Write | `memory_link` |
| `pre_consolidate` / `post_consolidate` | write | Write | `memory_consolidate` |
| `pre_governance_decision` / `post_governance_decision` | gate | Write | governance pipeline |
| `on_index_eviction` | maintenance | Index | HNSW eviction |
| `pre_archive` | write | Write | archive-on-GC + manual archive |
| `pre_transcript_store` / `post_transcript_store` | write | Transcript | transcript sidechain writes |

The 5 grand-slam additions:

| Event | Track | Class | Fires on |
|---|---|---|---|
| `pre_recall_expand` | G10 | **HotPath** | query-expansion synthesise step |
| `pre_reflect` / `post_reflect` | Recursive-learning Task 6/8 | Write | `memory_reflect` |
| `pre_compaction` / `on_compaction_rollback` | L1-7 | Write | curator compaction pipeline |

The discriminator strings and the `HookEvent` enum live at
[`src/hooks/events.rs:73`](../src/hooks/events.rs); the canonical wire
shapes for every event's payload (`MemoryDelta`, `RecallQuery`,
`SearchResult`, `ReflectDelta`, `CompactionDelta`, …) start at
[`src/hooks/events.rs:231`](../src/hooks/events.rs) and run to line
640.

## Decision-class semantics

Every hook returns a `HookDecision`
([`src/hooks/decision.rs:88`](../src/hooks/decision.rs)):

- **`Allow`** — chain proceeds to the next hook (or to the substrate
  if this was the last one).
- **`Modify(delta)`** — chain proceeds, but the in-flight payload is
  rewritten using the hook's delta. Only legal on `pre_*` events
  whose payload type implements the modify protocol (e.g.
  `pre_store` carries `MemoryDelta`, `pre_link` carries `LinkDelta`).
- **`Deny{reason, code}`** — chain short-circuits; the substrate
  refuses the operation and surfaces `code` to the caller. The
  `reason` string is operator-log-only and may carry diagnostic
  detail; the `code` is the wire-safe identifier the caller switches
  on.
- **`AskUser{prompt, options, default}`** — chain pauses pending an
  operator decision. Today the only consumer is the K10 SSE approval
  loop ([`docs/k10-sse-approvals.md`](k10-sse-approvals.md)). The
  default is applied if the K10 sweeper expires the row before an
  operator answers.

`is_pre_event` ([`src/hooks/decision.rs:369`](../src/hooks/decision.rs))
is the canonical predicate for "may this event return `Modify`" — the
chain runner rejects `Modify` decisions on `post_*` events.

## Per-class deadline budgets

The chain runner reads `event_class(event)`
([`src/hooks/timeouts.rs:137`](../src/hooks/timeouts.rs)) at fire
entry and computes a wall-clock ceiling on the *entire* chain. Per-hook
budgets are derived by `per_hook_budget_ms`
([`src/hooks/timeouts.rs:223`](../src/hooks/timeouts.rs)) and shrink
monotonically as earlier hooks consume time:

| Class | Deadline | Events |
|---|---|---|
| `Write` | **5,000 ms** | store/delete/promote/link/consolidate/governance/archive/reflect/compaction |
| `Read` | **2,000 ms** | recall/search |
| `Index` | **1,000 ms** | `on_index_eviction` |
| `Transcript` | **5,000 ms** | `pre_transcript_store`, `post_transcript_store` |
| `HotPath` | **50 ms** | `pre_recall_expand` (only inhabitant today) |

The HotPath ceiling is the v0.6.3 recall p95 budget — a hook that
can't return a decision in 50ms cannot be wired on the read path
without blowing SLO. The class deadline is the **whole-chain**
ceiling; individual hook `timeout_ms` values may be smaller. A hook's
effective per-call budget is `min(timeout_ms, remaining_chain_ms)`.

When `per_hook_budget_ms` returns `None`, the chain has already
exhausted its class deadline before this hook even fired. The runner
increments the process-wide
`timeout_violations_total` counter
([`src/hooks/timeouts.rs:265-273`](../src/hooks/timeouts.rs)) and
fails open (treats the missed hook as `Allow`). The doctor surface
reads this counter for the "did we trip a budget since boot" panel.

## Hot-path constraint

`post_recall` and `post_search` default to `mode = "daemon"`
([`src/hooks/config.rs:157`](../src/hooks/config.rs)). The v0.6.3
recall p95 budget is 50 ms; the daemon subprocess keeps the hook
chain off the synchronous fork/exec path. `mode = "exec"` is
permitted for these events but requires the explicit setting — the
default is intentionally biased toward latency-preserving behavior.

`pre_recall_expand` defaults to `daemon` for the same reason but is
classed as `HotPath` rather than `Read` (50 ms whole-chain ceiling vs
2 s), so a misconfigured exec-mode expansion hook still cannot park
the recall path for a full second.

## Hot-reload (SIGHUP)

`spawn_reload_task` ([`src/hooks/config.rs:411`](../src/hooks/config.rs))
listens for `SIGHUP` on Linux/macOS and atomically swaps the chain's
config snapshot via `ArcSwap`. Read-side dispatch resolves the
snapshot once per fire, so a reload mid-fire never tears: any
in-flight chain finishes against the old config; new chains see the
new config. On non-Unix targets the function is a no-op
([`src/hooks/config.rs:473`](../src/hooks/config.rs)).

**Race window discussion.** Between the operator's `kill -HUP <pid>`
and the chain snapshot swap there is a sub-millisecond window where a
new chain fire may have already loaded the pre-swap snapshot. This is
intentional — the alternative (locking writers out of the chain
during reload) would convert hot-reload into a brief outage on a
busy daemon. The operator-visible consequence: a single in-flight
chain may run against the pre-reload config even after `SIGHUP` is
delivered. Verification of the swap is via the `tracing::info!`
emitted by the reload task ("hooks: reloaded config on SIGHUP") — if
the operator wants strict observability they grep for the line
before treating the reload as effective.

On parse failure (TOML error, validation error, missing file) the
reload task logs a warning and **keeps the previous config**
([`src/hooks/config.rs:459`](../src/hooks/config.rs)). The daemon
never reloads to an empty config because of operator typo — silent
hook removal would be a security regression.

## Security hardening

- **Stderr redaction** — the executor scrubs stderr through a
  regex-based pass before forwarding to the daemon log when
  `secret_redact = true`. Mitigates the v0.7.0 reconciliation finding
  (commit `cbe934c`). Pinned by
  [`tests/g3_hooks_stderr_drain.rs`](../tests/g3_hooks_stderr_drain.rs).
- **Timeout enforcement** — hooks past their `timeout_ms` are killed
  with `SIGKILL` after `SIGTERM` + 200 ms grace. Pinned by
  [`tests/hooks_timeout_budget.rs`](../tests/hooks_timeout_budget.rs).
- **Substrate authority** — hook decisions are advisory unless the
  substrate explicitly elevates them (e.g., the 7th-form
  `storage::insert` pre-write hook gates on the rule corpus, not on
  arbitrary user hooks). User-supplied hooks cannot bypass governance.

## Tests

Pinned by [`tests/hooks_executor_test.rs`](../tests/hooks_executor_test.rs),
[`tests/hooks_hot_reload.rs`](../tests/hooks_hot_reload.rs),
[`tests/hooks_pre_recall.rs`](../tests/hooks_pre_recall.rs),
[`tests/hooks_timeout_budget.rs`](../tests/hooks_timeout_budget.rs),
[`tests/g3_hooks_stderr_drain.rs`](../tests/g3_hooks_stderr_drain.rs),
[`tests/g11_auto_link_detector.rs`](../tests/g11_auto_link_detector.rs).

## Operator workflow

1. **Author the helper binary.** Use the auto-link-detector as the
   reference (`tools/auto-link-detector/src/main.rs`). Speak JSON-RPC
   over stdin/stdout for `mode = "daemon"`; one-shot exec for
   `mode = "exec"`.
2. **Drop the binary on `PATH`** and `chmod +x`.
3. **Edit `~/.config/ai-memory/hooks.toml`** with the row schema above.
4. **Reload** with `kill -HUP $(pgrep -f 'ai-memory mcp')` or restart
   the daemon. The reload task logs `hooks: reloaded config on SIGHUP`
   on success.
5. **Verify** with `ai-memory mcp call memory_capabilities '{"schema_version":"3"}' | jq '.hooks'`
   — the `enabled_events` field lists every event with at least one
   registered hook.

## Tuning guidance

Recommended `priority` bands:

- **`priority = 1000+`** — security / compliance hooks. Run first so
  they can `Deny` before a Modify hook rewrites the payload past
  their checks.
- **`priority = 100-999`** — semantic-extraction hooks
  (auto-tagging, auto-link, embeddings). Run after compliance, before
  observability.
- **`priority = 1-99`** — observability / metrics hooks. Run last;
  they see the final payload that the substrate is about to commit.

Recommended `timeout_ms` per event class:

| Class | Recommended `timeout_ms` | Rationale |
|---|---|---|
| Write | 500-2000 | Most hooks finish in <100ms; budget gives headroom for an Ollama call or a network lookup. |
| Read | 200-1000 | Read path is hot; keep budgets tight. |
| HotPath (`pre_recall_expand`) | 30-45 | Class ceiling is 50ms; leave 5-20ms headroom for chain overhead. |
| Index | 100-500 | Background loop; small payloads. |
| Transcript | 500-2000 | Same as Write; transcript payloads can be larger. |

For deployment sizes:

- **Small (1-5 agents)** — single `daemon`-mode auto-link-detector
  hook is plenty. Default `fail_mode = "open"` keeps the substrate
  resilient to hook bugs.
- **Medium (10-50 agents)** — multiple hooks per event acceptable;
  watch `timeout_violations_total` weekly. If non-zero, either tighten
  individual `timeout_ms` or move long-running work to a post-write
  queue.
- **Large (100+ agents, regulated tenant)** — compliance hooks at
  `fail_mode = "closed"` so a buggy hook produces a hard refusal
  rather than silent fail-open. Pair with an on-call alert on
  `timeout_violations_total` delta > 0.

## Troubleshooting

| Symptom | Likely cause | Diagnostic recipe |
|---|---|---|
| Hook not firing | Namespace mismatch, `enabled = false`, or capability not surfaced | `memory_capabilities | jq '.hooks.enabled_events'` — if event missing, config didn't load. Then `RUST_LOG=ai_memory::hooks=debug` and watch `chain_skipped` reason. |
| Hook fires but result ignored | Returned `Modify` on a `post_*` event | Check `decision.rs:369` `is_pre_event` — `Modify` is only valid on pre-events. Daemon log carries the rejection reason. |
| `chain_skipped: empty` on every write | No `hooks.toml`, or zero matching rows | This is the **expected v0.6.4-equivalent behavior**. Confirms hooks aren't quietly firing. |
| Recall p95 regressed after enabling hook | Hook is `mode = "exec"` on a hot-path event | Switch to `mode = "daemon"`. If already daemon, reduce `timeout_ms` and inspect helper-binary tracing for the slow path. |
| `timeout_violations_total` growing | A hook's class deadline tripping | Compare to per-hook `ExecutorMetrics` ([`src/hooks/executor.rs:380`](../src/hooks/executor.rs)) to identify the slow hook; widen its `timeout_ms` (cap is 30s) or migrate work off the synchronous path. |
| Daemon-mode hook respawn loop | Helper binary panics on framed stdin | Inspect daemon log for the `executor: daemon spawn failed` line. Fix the helper, redeploy, `SIGHUP`. The chain fails open in the meantime (per `fail_mode = "open"` default). |
| Reload didn't pick up new hook | TOML parse error | Look for `hooks: SIGHUP reload failed; keeping previous config` in the log. Validate the file with `cat ~/.config/ai-memory/hooks.toml | toml --check` (or `taplo lint`). |

## Operator runbook (3am procedures)

**A hook is denying every write — substrate appears stuck.**

1. Set `RUST_LOG=ai_memory::hooks=debug` (`pkill -USR2 ai-memory` if
   you have the dynamic-log signal wired; otherwise restart).
2. `tail -F` the daemon log and grep for `hook_denied`. The log
   line carries the hook name, event, and `code`.
3. To unblock: edit `~/.config/ai-memory/hooks.toml`, set
   `enabled = false` on the offending row, `kill -HUP`. Confirm via
   `memory_capabilities`.
4. RCA after the bleeding stops. The auto-link-detector ships with a
   `--dry-run` mode for replay verification.

**Reload appears to have hung.**

`SIGHUP` reload is async and idempotent. If you don't see the
`hooks: reloaded config on SIGHUP` log line within ~1s, the most
likely cause is a TOML parse error — look for the warning line.
Re-issue `kill -HUP` after fixing the file. If the daemon process
itself is unresponsive, fall through to standard restart procedure
(`scripts/dogfood-rebuild.sh` documents the live-binary swap dance).

**Hot-path latency regressed; suspect a hook.**

1. `memory_capabilities | jq '.hooks.enabled_events'` — confirm which
   events have hooks attached.
2. Temporarily disable hot-path hooks (`enabled = false` on
   `post_recall` / `post_search` / `pre_recall_expand` rows), `SIGHUP`.
3. Re-measure recall p95. If recovered, the hook was the cause.
4. Look at per-hook `ExecutorMetrics` for the trip — usually the
   helper binary blocked on an upstream (Ollama, network). Move the
   work async or relax the SLO.

## Migration

A v0.6.4 → v0.7.0 install with no `hooks.toml` is a no-op at the
lifecycle layer. To opt in, follow the operator workflow above. To
verify hooks are NOT firing on a write path, set `RUST_LOG=ai_memory::hooks=debug`
and watch for the `chain_skipped: empty` log line.

See also: [`docs/MIGRATION_v0.7.md` §"Hook pipeline (opt-in)"](MIGRATION_v0.7.md#hook-pipeline-opt-in),
the canonical inventory in
[`docs/internal/v070-feature-inventory.md` §"Feature: Hook pipeline (Track G, 25 events)"](internal/v070-feature-inventory.md),
the SSE approval pipeline that consumes `AskUser` decisions at
[`docs/k10-sse-approvals.md`](k10-sse-approvals.md), the
transcript-store hook reference at
[`docs/sidechain-transcripts.md`](sidechain-transcripts.md), the K8
quotas substrate that gates write events after hook decisions at
[`docs/k8-quotas.md`](k8-quotas.md), the federation hardening that
applies the same hook chain to inbound peer writes at
[`docs/federation.md`](federation.md), and the signed-events chain
that records every governance-gated write at
[`docs/signed-events-v4.md`](signed-events-v4.md).
