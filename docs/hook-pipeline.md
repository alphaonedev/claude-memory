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
- **Config file:** `~/.config/ai-memory/hooks.toml` — hot-reloadable.

## Configuration

```toml
[[hook]]
event = "post_store"
command = "/usr/local/bin/auto-link-detector"
priority = 100
timeout_ms = 5000
mode = "daemon"          # daemon | exec
enabled = true
namespace = "team/*"     # glob match
secret_redact = true     # opt-in to stderr redaction
```

Fields:

- **`event`** — one of the 25 events below.
- **`command`** — absolute path to the helper binary.
- **`priority`** — higher fires first; first `Deny` short-circuits the chain.
- **`timeout_ms`** — wall-clock budget per call; exceeded → `Deny{code: "hook_timeout"}`.
- **`mode`** — `daemon` (long-lived subprocess, stdin JSON-RPC) or `exec` (one-shot fork+exec).
- **`enabled`** — soft-disable without removing the row.
- **`namespace`** — glob pattern; chain is filtered before invocation.
- **`secret_redact`** — opt-in stderr scrubbing (see hardening note below).

## 25-event matrix

The 20 baseline events:

| Event | Phase | Fires on |
|---|---|---|
| `pre_store` / `post_store` | write | `memory_store`, `memory_update` (when content changes) |
| `pre_recall` / `post_recall` | read | `memory_recall`, family-loader recall |
| `pre_recall_expand` | read | query-expansion path (G10) |
| `pre_search` / `post_search` | read | `memory_search` |
| `pre_delete` / `post_delete` | write | `memory_delete` |
| `pre_promote` / `post_promote` | write | tier promotion (manual + auto) |
| `pre_link` / `post_link` | write | `memory_link` |
| `pre_consolidate` / `post_consolidate` | write | `memory_consolidate` |
| `pre_governance_decision` / `post_governance_decision` | gate | governance pipeline |
| `on_index_eviction` | maintenance | HNSW eviction |
| `pre_archive` | write | archive-on-GC + manual archive |
| `pre_transcript_store` / `post_transcript_store` | write | transcript sidechain writes |

The 5 grand-slam additions:

| Event | Track | Fires on |
|---|---|---|
| `pre_recall_expand` | G10 | query-expansion synthesise step |
| `pre_reflect` / `post_reflect` | Recursive-learning Task 6/8 | `memory_reflect` |
| `pre_compaction` / `on_compaction_rollback` | L1-7 | curator compaction pipeline |

## Hot-path constraint

`post_recall` and `post_search` default to `mode = "daemon"`. The
v0.6.3 recall p95 budget is 50 ms; the daemon subprocess keeps the
hook chain off the synchronous fork/exec path. `mode = "exec"` is
permitted for these events but requires the explicit setting — the
default is intentionally biased toward latency-preserving behavior.

## Security hardening

- **Stderr redaction** — the executor scrubs stderr through a
  regex-based pass before forwarding to the daemon log when
  `secret_redact = true`. Mitigates the v0.7.0 reconciliation finding
  (commit `cbe934c`).
- **Timeout enforcement** — hooks past their `timeout_ms` are killed
  with `SIGKILL` after `SIGTERM` + 200 ms grace.
- **Substrate authority** — hook decisions are advisory unless the
  substrate explicitly elevates them (e.g., the 7th-form
  `storage::insert` pre-write hook gates on the rule corpus, not on
  arbitrary user hooks).

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
4. **Restart `ai-memory mcp`** (or wait for hot-reload — the config
   file is watched).
5. **Verify** with `ai-memory mcp call memory_capabilities '{"schema_version":"3"}' | jq '.hooks'`
   — the `enabled_events` field lists every event with at least one
   registered hook.

## Migration

A v0.6.4 → v0.7.0 install with no `hooks.toml` is a no-op at the
lifecycle layer. To opt in, follow the operator workflow above. To
verify hooks are NOT firing on a write path, set `RUST_LOG=ai_memory::hooks=debug`
and watch for the `chain_skipped: empty` log line.

See also: [`docs/MIGRATION_v0.7.md` §"Hook pipeline (opt-in)"](MIGRATION_v0.7.md#hook-pipeline-opt-in),
the canonical inventory in
[`docs/internal/v070-feature-inventory.md` §"Feature: Hook pipeline (Track G, 25 events)"](internal/v070-feature-inventory.md).
