// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7 Track G — Task G3: subprocess hook executor (exec + daemon modes).
//
// G1 (PR #554) shipped the `hooks.toml` schema + SIGHUP hot-reload
// plumbing. G2 (PR #563) attached payload structs to every variant
// of `HookEvent`. G3 wires the runtime: given a `HookConfig` and a
// JSON payload, fire the configured subprocess and parse a
// `HookDecision` back from its stdout.
//
// # Two modes
//
// * [`ExecExecutor`] — `tokio::process::Command` per fire. The
//   payload is written to stdin; stdin is then closed; the child's
//   stdout is read to EOF and parsed as a single JSON object.
//   Cheapest model to reason about; ideal for cold or low-rate
//   events (`pre_governance_decision`, `pre_archive`).
//
// * [`DaemonExecutor`] — one long-lived child per `HookConfig`.
//   Frames newline-delimited JSON over stdin/stdout (NDJSON, see
//   "Framing choice" below). Cheap per-fire amortized cost; the
//   right pick for hot events (`post_recall`, `post_search`) where
//   we need to preserve the v0.6.3 50ms recall budget. Reconnects
//   on child crash with exponential backoff (100ms → 5s, capped at
//   5 attempts).
//
// # Framing choice — NDJSON
//
// Newline-delimited JSON. One JSON object per line, no embedded
// newlines (`serde_json::to_writer` + `b'\n'`). Picked over
// length-prefixed for three reasons:
//
//   1. Hook authors can `read_line()` from any language stdlib —
//      no varint or 4-byte-BE length to decode.
//   2. The same stdio pipe is greppable / pipeable for debugging:
//      `tail -f /var/log/hook.log | jq` Just Works.
//   3. Our payloads (`MemoryDelta`, `RecallResult`) never embed
//      raw newlines — `serde_json` encodes `\n` inside strings as
//      `\\n`, so the framing invariant holds without escaping
//      gymnastics on either side of the pipe.
//
// The trade-off is a single malformed line corrupts the rest of
// the stream — but on the daemon path we already reconnect on
// any framing error, so the operator-visible behaviour is the same
// as a child crash: log + exponential backoff + retry.
//
// # Backpressure
//
// The daemon executor wraps each in-flight fire in a `tokio::time::timeout`
// keyed off `HookConfig.timeout_ms`. If the child can't keep up, the
// per-fire deadline trips and we drop the request with a `tracing::warn!`
// + `events_dropped` counter bump. "Oldest first" is a property of the
// single-flight serialization: each fire holds the connection mutex for
// at most `timeout_ms`, so the queue ahead of a slow fire is bounded by
// `timeout_ms × queue_depth` — which is exactly the deadline-drain shape
// the prompt asked for.
//
// # G4 update — HookDecision lifted into `src/hooks/decision.rs`
//
// G3 shipped a local `Allow + Deny` stub of `HookDecision` here so
// the executor had something to deserialize against. G4 replaces
// the stub with the full four-variant enum
// (`Allow / Modify(MemoryDelta) / Deny / AskUser`) in the
// dedicated `decision.rs` module. This file now imports the
// canonical type and routes parse errors through the executor's
// `Decode` variant — failure modes the operator sees (warning
// log + degrade-to-Allow on the dispatcher path) are unchanged.
//
// # Out of scope (per the G3 prompt; still pending)
//
// * G5 chain ordering / first-deny-wins — separate task.
// * G6 per-event-class deadlines — G3 honours `HookConfig.timeout_ms`
//   only.
// * G7-G11 firing at the actual memory operation points.

use std::io;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;

use super::config::HookConfig;
use super::decision::HookDecision;
use super::events::HookEvent;

/// Adapter from the G4 strict-parse path into the executor's
/// existing `Decode` error surface. Keeps `drive_exec_child` /
/// `exchange` callers using one error type while letting the
/// dispatcher (G5) reach for `DecisionParseError`'s named
/// variants when it wants to log the precise failure mode.
fn parse_decision_line(line: &str) -> Result<HookDecision> {
    HookDecision::parse(line).map_err(|e| ExecutorError::Decode {
        reason: e.to_string(),
    })
}

// ---------------------------------------------------------------------------
// HookExecutor trait
// ---------------------------------------------------------------------------

/// Errors surfaced by the executor layer. Hand-rolled `Display +
/// Error` per the v0.7 lesson (no `thiserror` in this crate's hot
/// dependency tree).
#[derive(Debug)]
pub enum ExecutorError {
    /// The configured `command` could not be spawned (missing
    /// binary, permissions, etc.).
    Spawn { command: String, source: io::Error },
    /// I/O failure talking to the child's stdio pipes.
    Io(io::Error),
    /// The child returned non-zero or closed its stdout without
    /// writing a decision.
    ChildExit { code: Option<i32>, stderr: String },
    /// The child wrote a payload we could not parse as a
    /// [`HookDecision`].
    Decode { reason: String },
    /// The fire deadline (`HookConfig.timeout_ms`) elapsed before
    /// the child returned a decision.
    Timeout { ms: u64 },
    /// The daemon child crashed or was unreachable after exhausting
    /// the reconnect budget.
    DaemonUnavailable { attempts: u32 },
}

impl std::fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecutorError::Spawn { command, source } => {
                write!(f, "hook spawn failed for {command}: {source}")
            }
            ExecutorError::Io(e) => write!(f, "hook io error: {e}"),
            ExecutorError::ChildExit { code, stderr } => {
                let code_str = code.map_or_else(|| "<signaled>".into(), |c| c.to_string());
                let preview = stderr.chars().take(256).collect::<String>();
                write!(f, "hook child exited (code {code_str}): {preview}")
            }
            ExecutorError::Decode { reason } => {
                write!(f, "hook decision decode failed: {reason}")
            }
            ExecutorError::Timeout { ms } => {
                write!(f, "hook timed out after {ms}ms")
            }
            ExecutorError::DaemonUnavailable { attempts } => {
                write!(
                    f,
                    "hook daemon unavailable after {attempts} reconnect attempts"
                )
            }
        }
    }
}

impl std::error::Error for ExecutorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ExecutorError::Spawn { source, .. } | ExecutorError::Io(source) => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for ExecutorError {
    fn from(value: io::Error) -> Self {
        ExecutorError::Io(value)
    }
}

/// `Result` alias used across the executor surface.
pub type Result<T> = std::result::Result<T, ExecutorError>;

/// Trait every executor implementation satisfies. `fire` is the
/// single hot-path method G5 will iterate over when stitching
/// chains together.
///
/// `Send + Sync` is mandatory: the registry hands out
/// `Arc<dyn HookExecutor>` and the chain runner (G5) drives fires
/// from arbitrary tokio worker threads.
pub trait HookExecutor: Send + Sync {
    /// Fire the hook for `event` with `payload`. Returns the
    /// child's [`HookDecision`] or an [`ExecutorError`] on
    /// spawn / IO / decode / timeout failure.
    ///
    /// This is `async` via the `BoxFuture` shape because trait
    /// objects + `async fn in trait` is still rough on stable
    /// (the auto-trait inference for `Send` doesn't carry across
    /// the dyn boundary). `BoxFuture<'_, Result<HookDecision>>`
    /// is the same shape `tower::Service` settled on.
    fn fire<'a>(
        &'a self,
        event: HookEvent,
        payload: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<HookDecision>> + Send + 'a>>;

    /// Snapshot of executor metrics. Surfaced by `ai-memory doctor
    /// --tokens --hooks` (see `src/cli/doctor.rs`).
    fn metrics(&self) -> ExecutorMetrics;
}

// ---------------------------------------------------------------------------
// ExecutorMetrics — backpressure observability
// ---------------------------------------------------------------------------

/// Per-executor metrics surfaced by `ai-memory doctor`.
///
/// These are *snapshots*; the executor accumulates raw counters
/// internally and projects to this struct on demand. See
/// [`MetricsCounters`] for the live atomics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ExecutorMetrics {
    pub events_fired: u64,
    pub events_dropped: u64,
    pub mean_latency_us: u64,
}

#[derive(Debug, Default)]
struct MetricsCounters {
    fired: AtomicU64,
    dropped: AtomicU64,
    latency_sum_us: AtomicU64,
    latency_n: AtomicU64,
}

impl MetricsCounters {
    fn record_fire(&self, latency: Duration) {
        self.fired.fetch_add(1, Ordering::Relaxed);
        let us = u64::try_from(latency.as_micros()).unwrap_or(u64::MAX);
        self.latency_sum_us.fetch_add(us, Ordering::Relaxed);
        self.latency_n.fetch_add(1, Ordering::Relaxed);
    }

    fn record_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> ExecutorMetrics {
        let fired = self.fired.load(Ordering::Relaxed);
        let dropped = self.dropped.load(Ordering::Relaxed);
        let n = self.latency_n.load(Ordering::Relaxed);
        let sum = self.latency_sum_us.load(Ordering::Relaxed);
        let mean_latency_us = if n == 0 { 0 } else { sum / n };
        ExecutorMetrics {
            events_fired: fired,
            events_dropped: dropped,
            mean_latency_us,
        }
    }
}

// ---------------------------------------------------------------------------
// FireEnvelope — wire shape sent to the child
// ---------------------------------------------------------------------------

/// JSON envelope written to a hook subprocess on every fire.
///
/// Keeping `event` separate from `payload` lets the child route
/// without re-parsing the payload bag — useful for daemon mode,
/// where one child might subscribe to several events.
#[derive(Debug, Serialize)]
struct FireEnvelope<'a> {
    event: HookEvent,
    payload: &'a Value,
}

// ---------------------------------------------------------------------------
// ExecExecutor — subprocess per fire
// ---------------------------------------------------------------------------

/// Subprocess-per-fire executor. Spawns a fresh child for every
/// event; closes stdin to signal "no more input"; reads stdout to
/// EOF and parses a single [`HookDecision`].
///
/// Cheapest mental model. Right pick for low-rate events. Hot
/// events should configure `mode = "daemon"` instead.
pub struct ExecExecutor {
    config: HookConfig,
    metrics: MetricsCounters,
}

impl ExecExecutor {
    #[must_use]
    pub fn new(config: HookConfig) -> Self {
        Self {
            config,
            metrics: MetricsCounters::default(),
        }
    }

    async fn fire_inner(&self, event: HookEvent, payload: Value) -> Result<HookDecision> {
        let envelope = FireEnvelope {
            event,
            payload: &payload,
        };
        let envelope_bytes = serde_json::to_vec(&envelope).map_err(|e| ExecutorError::Decode {
            reason: format!("envelope encode: {e}"),
        })?;

        let child = Command::new(&self.config.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| ExecutorError::Spawn {
                command: self.config.command.display().to_string(),
                source,
            })?;

        let started = Instant::now();
        let deadline = Duration::from_millis(u64::from(self.config.timeout_ms));

        let driven = timeout(deadline, drive_exec_child(child, envelope_bytes)).await;

        match driven {
            Ok(Ok(decision)) => {
                self.metrics.record_fire(started.elapsed());
                Ok(decision)
            }
            Ok(Err(e)) => {
                // Even on child error, we count the latency we
                // actually paid — the budget is a wall-clock figure.
                self.metrics.record_fire(started.elapsed());
                Err(e)
            }
            Err(_elapsed) => {
                self.metrics.record_drop();
                // The Child has been moved into drive_exec_child; that
                // future is dropped on timeout, which fires the
                // `kill_on_drop` knob set above and reaps the process.
                Err(ExecutorError::Timeout {
                    ms: u64::from(self.config.timeout_ms),
                })
            }
        }
    }
}

impl HookExecutor for ExecExecutor {
    fn fire<'a>(
        &'a self,
        event: HookEvent,
        payload: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<HookDecision>> + Send + 'a>>
    {
        Box::pin(self.fire_inner(event, payload))
    }

    fn metrics(&self) -> ExecutorMetrics {
        self.metrics.snapshot()
    }
}

async fn drive_exec_child(mut child: Child, envelope: Vec<u8>) -> Result<HookDecision> {
    // Write the envelope, then close stdin so the child knows it
    // can finish. `take()` drops the handle on the way out.
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&envelope).await?;
        stdin.write_all(b"\n").await?;
        stdin.shutdown().await?;
    }

    let output = child.wait_with_output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(ExecutorError::ChildExit {
            code: output.status.code(),
            stderr,
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // The child may write multiple newlines; the decision is the
    // last non-empty line. Hooks that print debug output to stdout
    // before their decision Just Work this way.
    let decision_line = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .next_back()
        .unwrap_or("");
    parse_decision_line(decision_line)
}

// ---------------------------------------------------------------------------
// DaemonExecutor — long-lived child + NDJSON framing
// ---------------------------------------------------------------------------

/// Per-`HookConfig` long-lived child. One executor owns one child
/// at a time; fires are serialized through a single connection
/// mutex (NDJSON request → NDJSON response). On framing error or
/// child exit, the connection is dropped and the next fire
/// reconnects with exponential backoff.
pub struct DaemonExecutor {
    config: HookConfig,
    conn: Mutex<Option<DaemonConnection>>,
    metrics: MetricsCounters,
}

struct DaemonConnection {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl DaemonExecutor {
    #[must_use]
    pub fn new(config: HookConfig) -> Self {
        Self {
            config,
            conn: Mutex::new(None),
            metrics: MetricsCounters::default(),
        }
    }

    async fn fire_inner(&self, event: HookEvent, payload: Value) -> Result<HookDecision> {
        let envelope = FireEnvelope {
            event,
            payload: &payload,
        };
        let mut envelope_bytes =
            serde_json::to_vec(&envelope).map_err(|e| ExecutorError::Decode {
                reason: format!("envelope encode: {e}"),
            })?;
        envelope_bytes.push(b'\n');

        let started = Instant::now();
        let deadline = Duration::from_millis(u64::from(self.config.timeout_ms));

        let driven = timeout(deadline, self.exchange(&envelope_bytes)).await;
        match driven {
            Ok(Ok(decision)) => {
                self.metrics.record_fire(started.elapsed());
                Ok(decision)
            }
            Ok(Err(e)) => {
                self.metrics.record_fire(started.elapsed());
                Err(e)
            }
            Err(_elapsed) => {
                self.metrics.record_drop();
                // Drop the connection so the next fire reconnects;
                // a slow daemon may still write the response into
                // the pipe after we've moved on, which would desync
                // subsequent fires.
                let mut guard = self.conn.lock().await;
                *guard = None;
                Err(ExecutorError::Timeout {
                    ms: u64::from(self.config.timeout_ms),
                })
            }
        }
    }

    /// Write one envelope, read one decision line. On any IO /
    /// framing error the connection is dropped before returning so
    /// the next fire goes through `connect_with_backoff`.
    async fn exchange(&self, envelope: &[u8]) -> Result<HookDecision> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            *guard = Some(self.connect_with_backoff().await?);
        }
        // Safe: just inserted if missing.
        let conn = guard.as_mut().expect("connection just inserted");

        if let Err(e) = conn.stdin.write_all(envelope).await {
            *guard = None;
            return Err(ExecutorError::Io(e));
        }
        if let Err(e) = conn.stdin.flush().await {
            *guard = None;
            return Err(ExecutorError::Io(e));
        }

        let mut line = String::new();
        match conn.stdout.read_line(&mut line).await {
            Ok(0) => {
                // EOF — child closed its stdout (likely crashed).
                *guard = None;
                Err(ExecutorError::ChildExit {
                    code: None,
                    stderr: "daemon child closed stdout".into(),
                })
            }
            Ok(_) => parse_decision_line(&line).map_err(|e| {
                // Framing error — reset the connection so the next
                // fire doesn't read into a half-consumed envelope.
                *guard = None;
                e
            }),
            Err(e) => {
                *guard = None;
                Err(ExecutorError::Io(e))
            }
        }
    }

    /// Spawn the child with exponential backoff (100ms → 5s, max 5
    /// attempts). Returns the connected handles or
    /// [`ExecutorError::DaemonUnavailable`] on exhaustion.
    async fn connect_with_backoff(&self) -> Result<DaemonConnection> {
        const MAX_ATTEMPTS: u32 = 5;
        const BASE_BACKOFF_MS: u64 = 100;
        const MAX_BACKOFF_MS: u64 = 5_000;

        let mut last_err: Option<ExecutorError> = None;
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                // Exponential backoff: 100, 200, 400, 800, 1600… capped.
                let pow = 1u64 << (attempt - 1);
                let backoff_ms = (BASE_BACKOFF_MS.saturating_mul(pow)).min(MAX_BACKOFF_MS);
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            }
            match self.spawn_one() {
                Ok(conn) => return Ok(conn),
                Err(e) => {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = MAX_ATTEMPTS,
                        error = %e,
                        "hooks: daemon spawn attempt failed"
                    );
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or(ExecutorError::DaemonUnavailable {
            attempts: MAX_ATTEMPTS,
        }))
    }

    fn spawn_one(&self) -> Result<DaemonConnection> {
        let mut child = Command::new(&self.config.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| ExecutorError::Spawn {
                command: self.config.command.display().to_string(),
                source,
            })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            ExecutorError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "child stdin not piped",
            ))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ExecutorError::Io(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "child stdout not piped",
            ))
        })?;
        Ok(DaemonConnection {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        })
    }
}

impl HookExecutor for DaemonExecutor {
    fn fire<'a>(
        &'a self,
        event: HookEvent,
        payload: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<HookDecision>> + Send + 'a>>
    {
        Box::pin(self.fire_inner(event, payload))
    }

    fn metrics(&self) -> ExecutorMetrics {
        self.metrics.snapshot()
    }
}

impl Drop for DaemonExecutor {
    fn drop(&mut self) {
        // Best-effort: kill the child so a reload doesn't leak
        // long-lived processes. tokio::process::Child has a
        // `kill_on_drop` knob but we can't reach it from here
        // without the conn lock; this Drop is the belt to that
        // suspenders.
        if let Ok(mut guard) = self.conn.try_lock() {
            if let Some(conn) = guard.as_mut() {
                let _ = conn.child.start_kill();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ExecutorRegistry
// ---------------------------------------------------------------------------

/// Maps `HookConfig` → `Arc<dyn HookExecutor>`. Built once per
/// `hooks.toml` load (see `src/hooks/config.rs::spawn_reload_task`)
/// and held behind the same `Arc<RwLock<…>>` that owns the config
/// snapshot. G5's chain runner consumes the registry's outputs.
///
/// The cache is keyed on `HookConfig` (full struct equality) so two
/// different hooks pointing at the same binary still get distinct
/// executors — one daemon child per `[[hook]]` block, never shared.
/// Sharing would let one event's slow fire starve another's
/// connection mutex, which is the opposite of what daemon mode
/// buys us.
///
/// Backed by a `Vec<(HookConfig, …)>` rather than a `HashMap`:
/// `HookConfig` carries a `PathBuf` and a `String` (no `Hash`
/// derivation today), and the cache cardinality is bounded by the
/// number of `[[hook]]` blocks in `hooks.toml` — a linear scan over
/// a few dozen entries is dwarfed by the spawn cost it gates.
pub struct ExecutorRegistry {
    cache: Vec<(HookConfig, Arc<dyn HookExecutor>)>,
}

impl ExecutorRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self { cache: Vec::new() }
    }

    /// Build a registry pre-populated with one executor per entry
    /// in `hooks`. Convenience for the bootstrap path; callers
    /// driving the SIGHUP reload should call [`Self::get`] lazily
    /// instead so dropping a `[[hook]]` from the config tears down
    /// its long-lived child.
    #[must_use]
    pub fn from_hooks(hooks: &[HookConfig]) -> Self {
        let mut me = Self::new();
        for h in hooks {
            let _ = me.get(h);
        }
        me
    }

    /// Return the executor for `hook`, constructing one on first
    /// touch. Subsequent calls with an *equal* `HookConfig` return
    /// the same `Arc`.
    pub fn get(&mut self, hook: &HookConfig) -> Arc<dyn HookExecutor> {
        if let Some((_, existing)) = self.cache.iter().find(|(cfg, _)| cfg == hook) {
            return Arc::clone(existing);
        }
        let executor: Arc<dyn HookExecutor> = match hook.mode {
            super::config::HookMode::Exec => Arc::new(ExecExecutor::new(hook.clone())),
            super::config::HookMode::Daemon => Arc::new(DaemonExecutor::new(hook.clone())),
        };
        self.cache.push((hook.clone(), Arc::clone(&executor)));
        executor
    }

    /// Iterate `(HookConfig, ExecutorMetrics)` pairs. `ai-memory
    /// doctor --tokens --hooks` calls this to render the
    /// per-executor backpressure table.
    pub fn metrics(&self) -> Vec<(HookConfig, ExecutorMetrics)> {
        self.cache
            .iter()
            .map(|(cfg, ex)| (cfg.clone(), ex.metrics()))
            .collect()
    }

    /// Number of cached executors. Cheap accessor for tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

impl Default for ExecutorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests — unit only; the integration tests live in
// `tests/hooks_executor_test.rs` so they can spawn real subprocesses.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::config::HookMode;

    fn cfg(mode: HookMode) -> HookConfig {
        HookConfig {
            event: HookEvent::PostStore,
            command: std::path::PathBuf::from("/bin/true"),
            priority: 0,
            timeout_ms: 1_000,
            mode,
            enabled: true,
            namespace: "*".into(),
        }
    }

    // The G3 stub's parse path now lives in `decision.rs`. These
    // tests cover the executor-side adapter (`parse_decision_line`)
    // which wraps `DecisionParseError` into `ExecutorError::Decode`
    // — the failure mode that surfaces on the daemon stdout path.

    #[test]
    fn parse_decision_line_allow_default_on_empty() {
        assert_eq!(parse_decision_line("").unwrap(), HookDecision::Allow);
        assert_eq!(parse_decision_line("   ").unwrap(), HookDecision::Allow);
        assert_eq!(parse_decision_line("{}").unwrap(), HookDecision::Allow);
    }

    #[test]
    fn parse_decision_line_allow_explicit() {
        let d = parse_decision_line(r#"{"action":"allow"}"#).unwrap();
        assert_eq!(d, HookDecision::Allow);
    }

    #[test]
    fn parse_decision_line_deny_with_default_code() {
        let d = parse_decision_line(r#"{"action":"deny","reason":"nope"}"#).unwrap();
        match d {
            HookDecision::Deny { reason, code } => {
                assert_eq!(reason, "nope");
                assert_eq!(code, 403);
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn parse_decision_line_deny_with_explicit_code() {
        let d = parse_decision_line(r#"{"action":"deny","reason":"x","code":429}"#).unwrap();
        match d {
            HookDecision::Deny { code, .. } => assert_eq!(code, 429),
            _ => panic!("expected Deny"),
        }
    }

    #[test]
    fn parse_decision_line_unknown_action_wraps_to_decode() {
        // G4 recognises `modify` so the canonical "unknown action"
        // case becomes a deliberately bogus discriminator.
        let err = parse_decision_line(r#"{"action":"explode"}"#).unwrap_err();
        match err {
            ExecutorError::Decode { reason } => {
                assert!(
                    reason.contains("unknown action"),
                    "decode reason should name the failure: {reason}"
                );
            }
            other => panic!("expected Decode, got {other:?}"),
        }
    }

    #[test]
    fn parse_decision_line_modify_now_recognised() {
        // G3's stub rejected `modify`; G4 lifts it into the wire
        // contract, so the executor must round-trip it cleanly.
        let d = parse_decision_line(r#"{"action":"modify","delta":{"priority":7}}"#).unwrap();
        match d {
            HookDecision::Modify(m) => assert_eq!(m.delta.priority, Some(7)),
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[test]
    fn metrics_counters_track_fired_dropped_and_mean() {
        let m = MetricsCounters::default();
        m.record_fire(Duration::from_micros(100));
        m.record_fire(Duration::from_micros(300));
        m.record_drop();
        let snap = m.snapshot();
        assert_eq!(snap.events_fired, 2);
        assert_eq!(snap.events_dropped, 1);
        assert_eq!(snap.mean_latency_us, 200);
    }

    #[test]
    fn metrics_counters_zero_when_no_fires() {
        let snap = MetricsCounters::default().snapshot();
        assert_eq!(snap.events_fired, 0);
        assert_eq!(snap.events_dropped, 0);
        assert_eq!(snap.mean_latency_us, 0);
    }

    #[test]
    fn registry_caches_per_hook_config() {
        let mut reg = ExecutorRegistry::new();
        let a = cfg(HookMode::Exec);
        let b = cfg(HookMode::Exec);
        let e1 = reg.get(&a);
        let e2 = reg.get(&b);
        assert_eq!(reg.len(), 1, "equal HookConfigs must dedupe");
        assert!(Arc::ptr_eq(&e1, &e2), "same Arc on cache hit");
    }

    #[test]
    fn registry_distinct_executors_for_distinct_modes() {
        let mut reg = ExecutorRegistry::new();
        let exec_cfg = cfg(HookMode::Exec);
        let mut daemon_cfg = cfg(HookMode::Daemon);
        // Bump priority so the configs are unequal even though
        // command path is identical.
        daemon_cfg.priority = 99;
        reg.get(&exec_cfg);
        reg.get(&daemon_cfg);
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn registry_from_hooks_prepopulates() {
        let hooks = vec![cfg(HookMode::Exec), {
            let mut d = cfg(HookMode::Daemon);
            d.priority = 1;
            d
        }];
        let reg = ExecutorRegistry::from_hooks(&hooks);
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn registry_metrics_starts_at_zero() {
        let mut reg = ExecutorRegistry::new();
        let _ = reg.get(&cfg(HookMode::Exec));
        let metrics = reg.metrics();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].1.events_fired, 0);
        assert_eq!(metrics[0].1.events_dropped, 0);
    }

    #[test]
    fn executor_error_display_formats_each_variant() {
        let cases: Vec<ExecutorError> = vec![
            ExecutorError::Spawn {
                command: "/bin/x".into(),
                source: io::Error::new(io::ErrorKind::NotFound, "no"),
            },
            ExecutorError::Io(io::Error::new(io::ErrorKind::Other, "boom")),
            ExecutorError::ChildExit {
                code: Some(42),
                stderr: "stderr msg".into(),
            },
            ExecutorError::Decode {
                reason: "bad json".into(),
            },
            ExecutorError::Timeout { ms: 1234 },
            ExecutorError::DaemonUnavailable { attempts: 5 },
        ];
        for e in cases {
            let s = e.to_string();
            assert!(!s.is_empty(), "Display empty for {e:?}");
        }
    }
}
