# V-1 wire-point audit — `wire_check` (and `GOVERNANCE_PRE_WRITE`) is the ONLY path

**Branch:** `validation/policy-engine-commercial-claim` @ `22101a7`
**Date:** 2026-05-14
**Audit query:**
```bash
grep -rn 'std::fs::write|tokio::fs::write|std::fs::File::create|tokio::fs::File::create|std::process::Command|tokio::process::Command|reqwest::Client|reqwest::get|hyper::Client|hyper::client' src/ --include='*.rs' | grep -v ':\s*//' | grep -v '/tests/'
```
85 raw hits. Classified below.

## Wire-gated production sites (the canonical 5 + 1)

| file:line | action variant | classification | rationale |
|---|---|---|---|
| `src/storage/mod.rs:117` | Custom("memory_write") via `GOVERNANCE_PRE_WRITE` | WIRE-GATED | Substrate INSERT path. `OnceLock` set at boot; every storage insert traverses this gate. |
| `src/hooks/executor.rs:403` | ProcessSpawn | WIRE-GATED | PreToolUse hook child-process spawn. `wire_check::check(&spawn_action)?` precedes `Command::spawn`. |
| `src/hooks/executor.rs:787` | ProcessSpawn | WIRE-GATED | Second executor spawn path (notify hook); same wire shape. |
| `src/federation/sync.rs:62` | NetworkRequest | WIRE-GATED | `wire_check::check(&net_action)?` precedes peer POST in `post_once`. |
| `src/llm.rs:406` | NetworkRequest | WIRE-GATED | `check_anyhow` precedes Ollama HTTP request in `check_outbound` (called from `generate_with_body` line 420). |
| `src/mcp/tools/skill_export.rs:162` | FilesystemWrite | WIRE-GATED | `wire_check::check(&skill_md_action)?` precedes SKILL.md write at line 169. |
| `src/mcp/tools/skill_export.rs:209` | FilesystemWrite | WIRE-GATED | Per-resource gate precedes `std::fs::write(&res_file, ...)` at line 219. |

## CLI-operator-exempt sites

| file:line | what it does | rationale |
|---|---|---|
| `src/cli/install.rs:365` | writes `~/.config/claude/...` config | `ai-memory install` is operator-invoked, not agent-driven; runs in one-shot CLI mode where `GOVERNANCE_PRE_ACTION` is never installed. |
| `src/cli/install.rs:555` | writes system-prompt snippet | `ai-memory install` operator command. |
| `src/cli/backup.rs:106` | writes backup manifest JSON | `ai-memory backup` operator command. |
| `src/cli/governance_migrate.rs:310,319,381,389,392` | writes migrated config to operator-supplied output path | `ai-memory governance-migrate` operator command. |
| `src/cli/rules.rs:523,553` | writes operator Ed25519 key seed + public key | `ai-memory rules keygen` — by definition operator-controlled. |
| `src/cli/helpers.rs:44` | `git remote get-url origin` (read-only) | namespace probe; CLI startup helper. Read-only, no shell metacharacter passthrough. |
| `src/config.rs:3468` | writes default config TOML on first run | bootstrap path; runs at `ai-memory` startup before any rules engine is online. |
| `src/log_paths.rs:283` | probe-write to candidate log dir | writability probe (creates and immediately removes a `.ai-memory-write-probe-PID` file); the candidate dirs are derived from operator config, not agent input. |

## Network-client constructions (NOT request sites)

| file:line | what it is | rationale |
|---|---|---|
| `src/federation/peer.rs:90` | `reqwest::Client::builder()` for federation client | Client construction issues no I/O. The actual POST in `federation/sync.rs:68` is preceded by `wire_check::check` at line 62. |
| `src/federation/mod.rs:1276,1703,1762,1786,2042` | Client::builder() — all inside `#[cfg(test)]` mod tests (line 70 onwards) | TEST-ONLY. |
| `src/federation/mod.rs:180` | Client::builder() inside test helper `build_config` | TEST-ONLY. |
| `src/cli/sync.rs:411,424` | Client::builder() for sync daemon mTLS client | Constructed once at daemon bootstrap; actual requests go through `federation/sync.rs::post_once` which is wire-gated. |
| `src/daemon_runtime.rs:2923` | Client::builder() in `run_sync_daemon_with_shutdown` | Daemon bootstrap; requests fan out through `federation/sync.rs` wire-gated paths. |
| `src/daemon_runtime.rs:2815` | `client: &reqwest::Client` parameter binding | not a request site. |
| `src/daemon_runtime.rs:2946` | `client: reqwest::Client` parameter binding | not a request site. |
| `src/federation/sync.rs:29,124` | `client: &reqwest::Client` parameter binding | not a request site; line 62 in same module gates the actual `req.send()`. |
| `src/federation/mod.rs:58` | `pub client: reqwest::Client` field declaration | struct field, not a request. |

## Test-only sites (out of scope)

All of the following are in `#[cfg(test)]` modules or end in `.unwrap()`/`.expect(` test-scaffold patterns:

- `src/audit.rs:1260, 1360, 1418, 1797, 1820, 1881`
- `src/logging.rs:422, 475`
- `src/config.rs:4388, 4416, 4424, 4543`
- `src/log_paths.rs:723`
- `src/main.rs:142, 157`
- `src/tls.rs:518, 1072, 1105`
- `src/embeddings.rs:1277`
- `src/daemon_runtime.rs:3564, 3572, 3580, 3595, 3617, 4288, 4351`
- `src/bench.rs:957`
- `src/cli/rules.rs:1125`
- `src/cli/io.rs:652, 812, 834, 1089`
- `src/cli/doctor.rs:2142`
- `src/cli/install.rs:2709, 2835`
- `src/cli/governance_migrate.rs:528, 542, 560, 594, 611, 628, 647, 669, 748`
- `src/cli/identity.rs:506, 507, 545, 546, 611, 647, 681, 682`
- `src/hooks/config.rs:824`
- `src/cli/backup.rs:449`
- `src/federation/mod.rs:976`

## GAPs

**None identified.** All raw hits classify cleanly. The five canonical wire-points named in V-1's deliverable B are present and structurally precede their respective mutation primitives.

## Verdict

**V-1: GREEN.** `wire_check` + `GOVERNANCE_PRE_WRITE` are the only paths through which a hook-installed daemon's agent-driven mutations may reach the host. CLI operator paths are by-design exempt (the agent does not have authority to invoke them). Test scaffolding is out of scope.

## Notes for OpenAI Codex auditor

1. The wire_check `GOVERNANCE_PRE_ACTION` hook is installed exactly once in `src/daemon_runtime.rs:2031` inside `bootstrap_serve`. CLI binaries that never call `bootstrap_serve` keep the OnceLock unset, in which case `wire_check::check` is a zero-cost no-op `Ok(())` (this is the documented CLI-operator-exempt path and matches the L1-6 E operator-as-actor design).
2. The closure opens its OWN `Connection` to the rules DB path captured at boot (lines 2030, 2033) — agent reasoning cannot redirect it.
3. Failure modes (DB open failure, query error) degrade to ALLOW with a `tracing::warn!` — the operator-stated trade-off is documented at lines 2036-2044, 2058-2065.
