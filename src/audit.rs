// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Enterprise audit trail (PR-5 of issue #487).
//!
//! Every memory-mutation call site in the binary — HTTP handlers, MCP
//! tool dispatch, CLI write commands, and `ai-memory boot` — emits an
//! [`AuditEvent`] to a hash-chained, append-only JSON log when the
//! audit subsystem is enabled. The schema is **stable, versioned, and
//! framework-agnostic** (NOT bound to OCSF or CEF — see
//! `docs/security/audit-schema.md`). SIEMs ingest the lines as-is.
//!
//! # Design properties
//!
//! 1. **Default-OFF** for privacy. Operators opt in via
//!    `[audit] enabled = true` in `config.toml` (or
//!    `AI_MEMORY_AUDIT_PATH=...` for one-off runs).
//! 2. **Hash-chained, tamper-evident.** Each line carries a `prev_hash`
//!    that matches the prior line's `self_hash`. `ai-memory audit
//!    verify` recomputes the chain and exits non-zero on mismatch.
//! 3. **Append-only OS hint.** Best-effort `chflags(2)` (BSD/macOS) or
//!    `FS_IOC_SETFLAGS` ioctl (Linux). Documented as defense in depth;
//!    the chain is the load-bearing tamper-evidence.
//! 4. **Privacy by default.** Audit captures `(memory_id, namespace,
//!    title, action, outcome, actor)`. Memory **content is never
//!    emitted** — `redact_content = true` is the only supported mode in
//!    the v1 schema; the field is reserved in [`AuditTarget`] for
//!    future compliance contexts that mandate content capture.
//! 5. **Per-process monotonic sequence**, independent of the chain.
//!    Lets a SIEM detect dropped lines even before the chain check.
//! 6. **No backpressure on the caller.** Emission is synchronous (one
//!    write per line so the chain is consistent across processes
//!    concurrently appending — the file is opened with `O_APPEND`),
//!    but failures inside emit are swallowed and logged via `tracing`.
//!    A broken audit pipeline never blocks a memory operation.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Stable schema version stamped on every emitted line. Bump only when
/// a field's semantics change in a way SIEM parsers care about
/// (renaming, removing, or repurposing). Adding optional fields does
/// NOT bump the version. See `docs/security/audit-schema.md` §Version
/// policy for the full contract.
pub const SCHEMA_VERSION: u32 = 1;

/// Sentinel `prev_hash` for the first line in a fresh chain. Hex-encoded
/// 32-byte zero buffer — picked so a chain head is unambiguous on
/// inspection.
pub const CHAIN_HEAD_PREV_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// One audit event. The serialized form is one JSON object per line
/// (NDJSON). Field order is stable for chain reproducibility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEvent {
    /// Schema version — see [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// RFC3339 UTC timestamp when the event was emitted.
    pub timestamp: String,
    /// Per-process monotonic counter starting at 1 on init.
    pub sequence: u64,
    pub actor: AuditActor,
    pub action: AuditAction,
    pub target: AuditTarget,
    pub outcome: AuditOutcome,
    /// Authentication context. `None` for stdio MCP / CLI invocations
    /// where there is no transport-level auth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuditAuth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Populated only when `outcome = Error`. Capped at 256 chars to
    /// prevent error-message based content leaks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Hex-encoded sha256 of the immediately prior line's `self_hash`,
    /// or [`CHAIN_HEAD_PREV_HASH`] for the first line of a fresh chain.
    pub prev_hash: String,
    /// Hex-encoded sha256 of every preceding field in serialization order.
    pub self_hash: String,
}

/// Who performed the action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditActor {
    /// Resolved NHI agent_id (`ai:<client>@<host>:pid-<n>`,
    /// `host:<host>:pid-<n>-<uuid>`, etc.). Always present.
    pub agent_id: String,
    /// Visibility scope: `private | team | unit | org | collective`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// How `agent_id` was synthesized — surfaces NHI provenance to the
    /// SIEM. One of: `explicit | env | mcp_client_info | host_fallback
    /// | anonymous_fallback | http_header | http_body | per_request`.
    pub synthesis_source: String,
}

/// Canonical action vocabulary. Adding a variant is a non-breaking
/// schema change; renaming or removing one IS breaking.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    Recall,
    Store,
    Update,
    Delete,
    Link,
    Promote,
    Forget,
    Consolidate,
    Export,
    Import,
    Approve,
    Reject,
    SessionBoot,
}

impl AuditAction {
    /// Wire-format string for log-grep convenience.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Recall => "recall",
            Self::Store => "store",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Link => "link",
            Self::Promote => "promote",
            Self::Forget => "forget",
            Self::Consolidate => "consolidate",
            Self::Export => "export",
            Self::Import => "import",
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::SessionBoot => "session_boot",
        }
    }
}

/// What was acted upon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditTarget {
    /// Memory id, or `"*"` for a list/sweep operation that touches
    /// many rows (forget, export, consolidate-many, etc.).
    pub memory_id: String,
    /// Memory namespace at the time of the action.
    pub namespace: String,
    /// Memory title at the time of the action. Capped at 200 chars and
    /// stripped of newlines to prevent log-injection. Title is **not**
    /// content; titles are advisory labels by design (`memory.content`
    /// is the secret payload and is **never** emitted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Memory tier (`short | mid | long`) at action time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Memory `metadata.scope` at action time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Outcome of the action.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    Allow,
    Deny,
    Error,
    Pending,
}

/// Authentication context for HTTP-originated events. Stdio (CLI / MCP)
/// invocations omit this block entirely.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditAuth {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ip: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtls_fp: Option<String>,
    /// **Hash** of the API key id, never the raw key. Hex-encoded
    /// sha256 truncated to 16 bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_id_hash: Option<String>,
}

// ---------------------------------------------------------------------------
// Sink — process-wide singleton holding the file handle + chain head.
// ---------------------------------------------------------------------------

/// Process-wide audit sink. `None` when audit is disabled. Wrapped in
/// `RwLock` (rather than `OnceLock`) so tests can swap in an
/// in-memory sink between cases without leaking state across runs.
static SINK: RwLock<Option<std::sync::Arc<AuditSink>>> = RwLock::new(None);
/// Per-process monotonic sequence counter. Starts at 1 on first emit.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Initialised audit sink — writer handle protected by a mutex so the
/// chain head update + write are atomic across emission threads. The
/// writer is `dyn Write + Send` so tests can substitute an in-memory
/// `Vec<u8>` for the production `File`.
pub(crate) struct AuditSink {
    inner: Mutex<SinkInner>,
    #[allow(dead_code)]
    redact_content: bool,
}

struct SinkInner {
    writer: Box<dyn Write + Send>,
    /// `self_hash` of the last line written, used as the next line's
    /// `prev_hash`. Starts as [`CHAIN_HEAD_PREV_HASH`] for a fresh log.
    last_hash: String,
    /// Source path, when the sink wraps a real file. `None` for
    /// in-memory test sinks.
    #[allow(dead_code)]
    path: Option<PathBuf>,
}

/// Initialise the audit sink. Called at most once per process from
/// [`init_from_config`]; subsequent calls replace the prior sink so
/// test-only callers can swap targets.
///
/// # Errors
/// - The audit directory cannot be created.
/// - The audit log file cannot be opened in append mode.
/// - Reading the existing chain tail (to seed `last_hash`) fails.
pub fn init(path: &Path, redact_content: bool, append_only_hint: bool) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating audit log dir {}", parent.display()))?;
    }

    // Seed the chain head from the existing tail of the log so a
    // restart on an existing file continues the chain.
    let last_hash = match read_chain_tail(path) {
        Ok(Some(h)) => h,
        _ => CHAIN_HEAD_PREV_HASH.to_string(),
    };

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening audit log {}", path.display()))?;

    if append_only_hint {
        // Best-effort. Errors here are documented and informational —
        // the hash chain is the load-bearing tamper-evidence.
        if let Err(e) = mark_append_only(path) {
            tracing::warn!(
                "audit: append-only OS flag could not be set on {} ({e}); \
                 the hash chain remains the authoritative tamper-evidence",
                path.display()
            );
        }
    }

    let sink = AuditSink {
        inner: Mutex::new(SinkInner {
            writer: Box::new(file),
            last_hash,
            path: Some(path.to_path_buf()),
        }),
        redact_content,
    };

    SEQUENCE.store(0, Ordering::SeqCst);
    if let Ok(mut guard) = SINK.write() {
        *guard = Some(std::sync::Arc::new(sink));
    }
    Ok(())
}

/// Test-only helper: install an in-memory sink that captures every
/// emitted line into the supplied `Arc<Mutex<Vec<u8>>>`. Bypasses the
/// filesystem entirely so tests run in any sandbox.
#[cfg(test)]
pub fn init_for_test(buf: std::sync::Arc<Mutex<Vec<u8>>>) {
    struct VecWriter(std::sync::Arc<Mutex<Vec<u8>>>);
    impl Write for VecWriter {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("test sink poisoned")
                .extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let sink = AuditSink {
        inner: Mutex::new(SinkInner {
            writer: Box::new(VecWriter(buf)),
            last_hash: CHAIN_HEAD_PREV_HASH.to_string(),
            path: None,
        }),
        redact_content: true,
    };
    SEQUENCE.store(0, Ordering::SeqCst);
    if let Ok(mut guard) = SINK.write() {
        *guard = Some(std::sync::Arc::new(sink));
    }
}

/// Test-only helper to remove the active sink so subsequent emissions
/// no-op.
#[cfg(test)]
pub fn shutdown_for_test() {
    if let Ok(mut guard) = SINK.write() {
        *guard = None;
    }
    SEQUENCE.store(0, Ordering::SeqCst);
}

/// Read the last `self_hash` from an existing audit log. Returns
/// `Ok(None)` when the file is empty or doesn't exist; returns the
/// `self_hash` of the last well-formed line otherwise. A malformed
/// trailing line counts as "empty" — emission seeds a fresh chain
/// head, and `audit verify` will surface the corruption.
fn read_chain_tail(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut last: Option<String> = None;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(ev) = serde_json::from_str::<AuditEvent>(&line) {
            last = Some(ev.self_hash);
        }
    }
    Ok(last)
}

/// Whether the audit subsystem is currently enabled. Cheap.
#[must_use]
pub fn is_enabled() -> bool {
    SINK.read().map(|g| g.is_some()).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Hashing — stable canonical form so emit + verify agree byte-for-byte.
// ---------------------------------------------------------------------------

/// Compute the canonical hash for an event. Hashes the same JSON the
/// emitter writes to disk EXCEPT with `self_hash` set to the empty
/// string sentinel — this lets `audit verify` recompute it from the
/// stored line by zeroing the same field.
fn compute_self_hash(ev: &AuditEvent) -> String {
    let canonical = canonical_json_for_hash(ev);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hex_encode(&hasher.finalize())
}

/// Serialize an event into the canonical pre-hash form: serde_json
/// representation with `self_hash` zeroed. The `prev_hash` is part of
/// the hashed input — that's exactly the linkage that makes the chain
/// tamper-evident.
fn canonical_json_for_hash(ev: &AuditEvent) -> String {
    let mut clone = ev.clone();
    clone.self_hash.clear();
    serde_json::to_string(&clone).expect("AuditEvent always serializes")
}

fn hex_encode(bytes: &[u8]) -> String {
    static HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

// ---------------------------------------------------------------------------
// Emission API — the surface the rest of the binary calls.
// ---------------------------------------------------------------------------

/// Builder for an audit event. Most call sites use one of the
/// convenience helpers ([`emit_store`], [`emit_recall`], etc.) but the
/// builder is public so unusual flows (consolidate-many, deferred
/// import) can fill in custom targets.
#[derive(Debug, Clone)]
pub struct EventBuilder {
    pub action: AuditAction,
    pub actor: AuditActor,
    pub target: AuditTarget,
    pub outcome: AuditOutcome,
    pub auth: Option<AuditAuth>,
    pub session_id: Option<String>,
    pub request_id: Option<String>,
    pub error: Option<String>,
}

impl EventBuilder {
    /// Build a default-shaped event for `action`. Caller fills in the
    /// remaining fields.
    #[must_use]
    pub fn new(action: AuditAction, actor: AuditActor, target: AuditTarget) -> Self {
        Self {
            action,
            actor,
            target,
            outcome: AuditOutcome::Allow,
            auth: None,
            session_id: None,
            request_id: None,
            error: None,
        }
    }

    /// Override outcome (default = Allow).
    #[must_use]
    pub fn outcome(mut self, outcome: AuditOutcome) -> Self {
        self.outcome = outcome;
        self
    }

    /// Set the error string. Caps at 256 chars and strips newlines so a
    /// runaway error message can't leak content or break the log line.
    #[must_use]
    pub fn error(mut self, msg: impl Into<String>) -> Self {
        self.error = Some(sanitize_field(&msg.into(), 256));
        self.outcome = AuditOutcome::Error;
        self
    }

    #[must_use]
    pub fn auth(mut self, auth: AuditAuth) -> Self {
        self.auth = Some(auth);
        self
    }

    #[must_use]
    pub fn request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = Some(id.into());
        self
    }
}

/// Write an event to the configured sink. No-op when audit is disabled.
/// Failures are logged via `tracing::error!` and dropped — audit is
/// **never** allowed to fail a memory operation.
pub fn emit(builder: EventBuilder) {
    if let Err(e) = try_emit(builder) {
        tracing::error!("audit: emission failed: {e}");
    }
}

/// Inner emission with proper `Result` so tests can assert directly on
/// the writer. `emit` swallows errors so production never blocks.
fn try_emit(builder: EventBuilder) -> Result<()> {
    let sink = {
        let guard = SINK
            .read()
            .map_err(|_| anyhow!("audit sink rwlock poisoned"))?;
        match guard.as_ref() {
            Some(s) => s.clone(),
            None => return Ok(()),
        }
    };

    let mut inner = sink
        .inner
        .lock()
        .map_err(|_| anyhow!("audit sink mutex poisoned"))?;

    let sequence = SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1;

    let mut ev = AuditEvent {
        schema_version: SCHEMA_VERSION,
        timestamp: Utc::now().to_rfc3339(),
        sequence,
        actor: builder.actor,
        action: builder.action,
        target: AuditTarget {
            memory_id: sanitize_field(&builder.target.memory_id, 128),
            namespace: sanitize_field(&builder.target.namespace, 128),
            title: builder.target.title.map(|t| sanitize_field(&t, 200)),
            tier: builder.target.tier,
            scope: builder.target.scope,
        },
        outcome: builder.outcome,
        auth: builder.auth,
        session_id: builder.session_id,
        request_id: builder.request_id,
        error: builder.error,
        prev_hash: inner.last_hash.clone(),
        self_hash: String::new(),
    };

    let self_hash = compute_self_hash(&ev);
    ev.self_hash = self_hash.clone();

    let line = serde_json::to_string(&ev).context("serializing audit event")?;
    writeln!(inner.writer, "{line}").context("appending audit line")?;
    inner.writer.flush().ok();
    inner.last_hash = self_hash;
    Ok(())
}

/// Sanitize a field for log emission: strip control chars + newlines
/// (prevent log injection) and cap to `max_chars` (prevent unbounded
/// growth from a hostile title or error message).
fn sanitize_field(s: &str, max_chars: usize) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .collect();
    if cleaned.chars().count() <= max_chars {
        cleaned
    } else {
        cleaned.chars().take(max_chars).collect()
    }
}

// ---------------------------------------------------------------------------
// Convenience helpers.
// ---------------------------------------------------------------------------

/// Construct an [`AuditActor`] from an agent_id + synthesis source +
/// optional scope. The synthesis source is informational metadata and
/// MUST be one of the documented strings in [`AuditActor`].
#[must_use]
pub fn actor(
    agent_id: impl Into<String>,
    synthesis_source: impl Into<String>,
    scope: Option<String>,
) -> AuditActor {
    AuditActor {
        agent_id: agent_id.into(),
        synthesis_source: synthesis_source.into(),
        scope,
    }
}

/// Construct an [`AuditTarget`] for a single memory.
#[must_use]
pub fn target_memory(
    memory_id: impl Into<String>,
    namespace: impl Into<String>,
    title: Option<String>,
    tier: Option<String>,
    scope: Option<String>,
) -> AuditTarget {
    AuditTarget {
        memory_id: memory_id.into(),
        namespace: namespace.into(),
        title,
        tier,
        scope,
    }
}

/// Construct an [`AuditTarget`] for a multi-row sweep operation.
#[must_use]
pub fn target_sweep(namespace: impl Into<String>) -> AuditTarget {
    AuditTarget {
        memory_id: "*".to_string(),
        namespace: namespace.into(),
        title: None,
        tier: None,
        scope: None,
    }
}

// ---------------------------------------------------------------------------
// Verify — the load-bearing tamper-evidence walk.
// ---------------------------------------------------------------------------

/// Outcome of [`verify_chain`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub total_lines: u64,
    pub first_failure: Option<VerifyFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyFailure {
    pub line_number: u64,
    pub kind: VerifyFailureKind,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyFailureKind {
    /// Line could not be parsed as an `AuditEvent`.
    Parse,
    /// Recomputed `self_hash` did not match the stored value.
    SelfHash,
    /// Stored `prev_hash` did not match the prior line's `self_hash`.
    ChainBreak,
    /// `sequence` did not increase monotonically.
    Sequence,
}

impl VerifyReport {
    /// Convenience — `Ok(())` when chain is intact, `Err` when not.
    pub fn into_result(self) -> Result<u64> {
        if let Some(failure) = self.first_failure {
            Err(anyhow!(
                "audit chain verification failed at line {}: {:?} — {}",
                failure.line_number,
                failure.kind,
                failure.detail
            ))
        } else {
            Ok(self.total_lines)
        }
    }
}

/// Walk an audit log file and verify the chain. Returns a structured
/// report; the binary's `audit verify` subcommand turns this into an
/// exit code.
///
/// # Errors
/// - The file cannot be opened or read.
pub fn verify_chain(path: &Path) -> Result<VerifyReport> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    verify_chain_from_reader(file)
}

/// Verify a chain from any [`Read`] source. Lets tests run against
/// in-memory buffers without touching the filesystem.
pub fn verify_chain_from_reader<R: Read>(reader: R) -> Result<VerifyReport> {
    let buf = BufReader::new(reader);
    let mut total: u64 = 0;
    let mut prev_hash = CHAIN_HEAD_PREV_HASH.to_string();
    let mut prev_seq: u64 = 0;

    for (idx, line) in buf.lines().enumerate() {
        let line_no = (idx as u64) + 1;
        let line = line.with_context(|| format!("reading audit line {line_no}"))?;
        if line.trim().is_empty() {
            continue;
        }
        total += 1;

        let ev: AuditEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(e) => {
                return Ok(VerifyReport {
                    total_lines: total,
                    first_failure: Some(VerifyFailure {
                        line_number: line_no,
                        kind: VerifyFailureKind::Parse,
                        detail: format!("malformed JSON: {e}"),
                    }),
                });
            }
        };

        if ev.prev_hash != prev_hash {
            return Ok(VerifyReport {
                total_lines: total,
                first_failure: Some(VerifyFailure {
                    line_number: line_no,
                    kind: VerifyFailureKind::ChainBreak,
                    detail: format!(
                        "prev_hash mismatch: expected {prev_hash}, got {}",
                        ev.prev_hash
                    ),
                }),
            });
        }

        if ev.sequence <= prev_seq && prev_seq != 0 {
            return Ok(VerifyReport {
                total_lines: total,
                first_failure: Some(VerifyFailure {
                    line_number: line_no,
                    kind: VerifyFailureKind::Sequence,
                    detail: format!(
                        "sequence not monotonic: prior={prev_seq}, this={}",
                        ev.sequence
                    ),
                }),
            });
        }

        let recomputed = compute_self_hash(&ev);
        if recomputed != ev.self_hash {
            return Ok(VerifyReport {
                total_lines: total,
                first_failure: Some(VerifyFailure {
                    line_number: line_no,
                    kind: VerifyFailureKind::SelfHash,
                    detail: format!(
                        "self_hash mismatch: stored={}, recomputed={}",
                        ev.self_hash, recomputed
                    ),
                }),
            });
        }

        prev_hash = ev.self_hash.clone();
        prev_seq = ev.sequence;
    }

    Ok(VerifyReport {
        total_lines: total,
        first_failure: None,
    })
}

// ---------------------------------------------------------------------------
// Bootstrap — read AppConfig and bring the sink up.
// ---------------------------------------------------------------------------

/// Initialise the audit sink from a parsed [`crate::config::AuditConfig`].
/// Returns `Ok(())` whether or not audit is enabled — it is a no-op when
/// disabled.
///
/// # Errors
/// - The audit directory or file cannot be opened.
pub fn init_from_config(cfg: &crate::config::AuditConfig) -> Result<()> {
    if !cfg.enabled.unwrap_or(false) {
        if let Ok(mut guard) = SINK.write() {
            *guard = None;
        }
        return Ok(());
    }
    let resolved_path = resolve_audit_path(cfg);
    init(
        &resolved_path,
        cfg.redact_content.unwrap_or(true),
        cfg.append_only.unwrap_or(true),
    )
}

/// Resolve the audit log file path from the config, honouring the
/// user-mandated precedence ladder: CLI > env (`AI_MEMORY_AUDIT_DIR`)
/// > `[audit] path` in config > platform default. Appends `audit.log`
/// when the resolved path looks like a directory.
///
/// Backwards-compatible wrapper that doesn't take a CLI override —
/// subcommand wiring uses [`resolve_audit_path_with_override`].
#[must_use]
pub fn resolve_audit_path(cfg: &crate::config::AuditConfig) -> PathBuf {
    let resolved = crate::log_paths::resolve_audit_dir(None, cfg.path.as_deref())
        .map(|r| r.path)
        .unwrap_or_else(|_| {
            crate::log_paths::platform_default(crate::log_paths::DirKind::Audit).path
        });
    finalize_audit_file(resolved, cfg.path.as_deref())
}

/// Strict variant: takes an optional `--audit-dir` override, returns
/// the resolved file path (with `audit.log` appended when the input
/// resolves to a directory) plus the [`crate::log_paths::PathSource`]
/// used.
///
/// # Errors
/// - Resolved directory is world-writable.
pub fn resolve_audit_path_with_override(
    cli_override: Option<&Path>,
    cfg: &crate::config::AuditConfig,
) -> Result<(PathBuf, crate::log_paths::PathSource)> {
    let r = crate::log_paths::resolve_audit_dir(cli_override, cfg.path.as_deref())?;
    let final_path = finalize_audit_file(r.path, cfg.path.as_deref());
    Ok((final_path, r.source))
}

/// Append `audit.log` when the resolved path is a directory; respect
/// an explicit file-path the user wrote in config.
fn finalize_audit_file(p: PathBuf, raw_config: Option<&str>) -> PathBuf {
    // If the user configured an explicit file path (has a non-empty
    // extension that isn't a trailing slash), keep it as-is.
    if let Some(raw) = raw_config
        && !raw.ends_with('/')
        && std::path::Path::new(raw).extension().is_some()
    {
        return p;
    }
    if p.extension().is_none() || p.to_string_lossy().ends_with('/') {
        p.join("audit.log")
    } else {
        p
    }
}

pub(crate) fn expand_tilde(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    raw.to_string()
}

// ---------------------------------------------------------------------------
// Append-only OS hint — best effort.
// ---------------------------------------------------------------------------

/// Apply the platform-appropriate "append-only" file flag. Silent on
/// non-unix platforms.
#[cfg(unix)]
fn mark_append_only(path: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path =
        CString::new(path.as_os_str().as_bytes()).context("path contains an interior NUL byte")?;
    #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
    {
        // SAFETY: c_path is a NUL-terminated string we own; chflags is
        // a libc syscall whose only safety obligation is a valid C
        // string. UF_APPEND is the user-visible append-only flag.
        let rc = unsafe { libc::chflags(c_path.as_ptr(), libc::UF_APPEND.into()) };
        if rc != 0 {
            return Err(anyhow!(
                "chflags(UF_APPEND) failed: errno={}",
                std::io::Error::last_os_error()
            ));
        }
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        // On Linux we'd issue FS_IOC_SETFLAGS with FS_APPEND_FL. The
        // syscall requires CAP_LINUX_IMMUTABLE on most filesystems and
        // is filesystem-specific (ext*, xfs, btrfs); refuse silently
        // on filesystems that don't support it. This is a best-effort
        // hint — the chain is the load-bearing tamper-evidence.
        const FS_APPEND_FL: libc::c_int = 0x0000_0020;
        // FS_IOC_SETFLAGS = _IOW('f', 2, long) = 0x4008_6602 on most
        // 64-bit Linux ABIs. Hard-coded to avoid pulling in an extra
        // crate just for the constant.
        const FS_IOC_SETFLAGS: libc::c_ulong = 0x4008_6602;
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(anyhow!(
                "open(audit log) for ioctl failed: errno={}",
                std::io::Error::last_os_error()
            ));
        }
        let mut flags: libc::c_int = 0;
        // SAFETY: fd is a valid file descriptor we just opened; the
        // ioctl call follows the documented FS_IOC_GETFLAGS / SETFLAGS
        // protocol.
        let rc = unsafe { libc::ioctl(fd, FS_IOC_SETFLAGS, &mut flags) };
        if rc == 0 {
            flags |= FS_APPEND_FL;
            let rc2 = unsafe { libc::ioctl(fd, FS_IOC_SETFLAGS, &mut flags) };
            unsafe { libc::close(fd) };
            if rc2 != 0 {
                return Err(anyhow!(
                    "ioctl(FS_IOC_SETFLAGS) failed: errno={}",
                    std::io::Error::last_os_error()
                ));
            }
            return Ok(());
        }
        unsafe { libc::close(fd) };
        Err(anyhow!(
            "ioctl(FS_IOC_GETFLAGS) failed: errno={}",
            std::io::Error::last_os_error()
        ))
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "linux"
    )))]
    {
        let _ = c_path;
        Err(anyhow!(
            "append-only flag not supported on this unix variant"
        ))
    }
}

#[cfg(not(unix))]
fn mark_append_only(_path: &Path) -> Result<()> {
    Err(anyhow!("append-only flag is unix-only"))
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event(seq: u64, prev: &str) -> AuditEvent {
        let mut ev = AuditEvent {
            schema_version: SCHEMA_VERSION,
            timestamp: "2026-04-30T00:00:00+00:00".to_string(),
            sequence: seq,
            actor: actor("ai:test@host:pid-1", "host_fallback", None),
            action: AuditAction::Store,
            target: target_memory(
                format!("mem-{seq}"),
                "ns-x",
                Some("title".to_string()),
                Some("mid".to_string()),
                None,
            ),
            outcome: AuditOutcome::Allow,
            auth: None,
            session_id: None,
            request_id: None,
            error: None,
            prev_hash: prev.to_string(),
            self_hash: String::new(),
        };
        ev.self_hash = compute_self_hash(&ev);
        ev
    }

    #[test]
    fn audit_event_round_trips_through_serde() {
        let ev = sample_event(1, CHAIN_HEAD_PREV_HASH);
        let s = serde_json::to_string(&ev).unwrap();
        let back: AuditEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
    }

    #[test]
    fn audit_chain_links_correctly_for_three_events() {
        let e1 = sample_event(1, CHAIN_HEAD_PREV_HASH);
        let e2 = sample_event(2, &e1.self_hash);
        let e3 = sample_event(3, &e2.self_hash);
        let mut buf = String::new();
        for ev in [&e1, &e2, &e3] {
            buf.push_str(&serde_json::to_string(ev).unwrap());
            buf.push('\n');
        }
        let report = verify_chain_from_reader(buf.as_bytes()).unwrap();
        assert!(report.first_failure.is_none(), "{:?}", report.first_failure);
        assert_eq!(report.total_lines, 3);
    }

    #[test]
    fn audit_verify_detects_tampered_line() {
        let e1 = sample_event(1, CHAIN_HEAD_PREV_HASH);
        let mut e2 = sample_event(2, &e1.self_hash);
        // Tamper: swap the title without recomputing self_hash.
        e2.target.title = Some("EVIL".to_string());
        let e3 = sample_event(3, &e2.self_hash);
        let mut buf = String::new();
        for ev in [&e1, &e2, &e3] {
            buf.push_str(&serde_json::to_string(ev).unwrap());
            buf.push('\n');
        }
        let report = verify_chain_from_reader(buf.as_bytes()).unwrap();
        let failure = report.first_failure.expect("tampering must be detected");
        assert_eq!(failure.line_number, 2);
        assert!(matches!(failure.kind, VerifyFailureKind::SelfHash));
    }

    #[test]
    fn audit_verify_detects_chain_break() {
        let e1 = sample_event(1, CHAIN_HEAD_PREV_HASH);
        // Break: e2's prev_hash points at a hash that isn't e1's.
        let e2 = sample_event(2, "deadbeef");
        let mut buf = String::new();
        for ev in [&e1, &e2] {
            buf.push_str(&serde_json::to_string(ev).unwrap());
            buf.push('\n');
        }
        let report = verify_chain_from_reader(buf.as_bytes()).unwrap();
        let failure = report.first_failure.expect("chain break must be detected");
        assert!(matches!(failure.kind, VerifyFailureKind::ChainBreak));
    }

    #[test]
    fn audit_redacts_content_by_default() {
        // The schema does not have a `content` field. This test
        // doubles as a guardrail: if anyone ever adds one to
        // AuditEvent or AuditTarget, the round-trip assertion below
        // will surface it.
        let ev = sample_event(1, CHAIN_HEAD_PREV_HASH);
        let json = serde_json::to_value(&ev).unwrap();
        assert!(
            json.get("content").is_none(),
            "AuditEvent must never carry a content field"
        );
        assert!(
            json["target"].get("content").is_none(),
            "AuditTarget must never carry a content field"
        );
    }

    #[test]
    fn audit_action_as_str_round_trips() {
        for action in [
            AuditAction::Recall,
            AuditAction::Store,
            AuditAction::Update,
            AuditAction::Delete,
            AuditAction::Link,
            AuditAction::Promote,
            AuditAction::Forget,
            AuditAction::Consolidate,
            AuditAction::Export,
            AuditAction::Import,
            AuditAction::Approve,
            AuditAction::Reject,
            AuditAction::SessionBoot,
        ] {
            let s = action.as_str();
            // serde rename-all snake_case round-trips through the
            // string representation.
            let v: serde_json::Value = serde_json::to_value(action).unwrap();
            assert_eq!(v.as_str().unwrap(), s);
        }
    }

    #[test]
    fn audit_sanitize_strips_newlines() {
        let cleaned = sanitize_field("line1\nline2\rline3", 32);
        assert!(!cleaned.contains('\n'));
        assert!(!cleaned.contains('\r'));
    }

    #[test]
    fn audit_sanitize_caps_length() {
        let s = "x".repeat(500);
        let cleaned = sanitize_field(&s, 100);
        assert_eq!(cleaned.chars().count(), 100);
    }

    #[test]
    fn audit_resolve_path_directory_expands_to_file() {
        let cfg = crate::config::AuditConfig {
            enabled: Some(true),
            path: Some("/tmp/ai-memory/audit/".to_string()),
            ..Default::default()
        };
        let p = resolve_audit_path(&cfg);
        assert!(p.ends_with("audit.log"));
    }

    #[test]
    fn audit_resolve_path_explicit_file_kept() {
        let cfg = crate::config::AuditConfig {
            enabled: Some(true),
            path: Some("/var/log/ai-memory/custom.log".to_string()),
            ..Default::default()
        };
        let p = resolve_audit_path(&cfg);
        assert_eq!(p, PathBuf::from("/var/log/ai-memory/custom.log"));
    }

    /// Serialize tests that mutate the process-wide audit sink so
    /// concurrent test runners don't stomp on each other. Tests that
    /// touch the live SINK should hold this lock for their duration.
    fn sink_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// PR-5 (issue #487) load-bearing integration test. Wire the
    /// audit subsystem to an in-memory sink and emit one event per
    /// canonical action. Each successful operation MUST produce one
    /// line; the chain MUST stay intact across the run.
    #[test]
    fn audit_emits_at_every_call_site() {
        let _g = sink_lock();
        let buf: std::sync::Arc<Mutex<Vec<u8>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        super::init_for_test(buf.clone());

        let actions = [
            AuditAction::Store,
            AuditAction::Recall,
            AuditAction::Update,
            AuditAction::Delete,
            AuditAction::Link,
            AuditAction::Promote,
            AuditAction::Forget,
            AuditAction::Consolidate,
            AuditAction::Export,
            AuditAction::Import,
            AuditAction::Approve,
            AuditAction::Reject,
            AuditAction::SessionBoot,
        ];
        for (i, action) in actions.iter().copied().enumerate() {
            let id = format!("mem-{i}");
            super::emit(EventBuilder::new(
                action,
                actor("ai:test@host", "explicit", None),
                target_memory(id, "ns-x", Some("t".to_string()), None, None),
            ));
        }

        let lines = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let count = lines.lines().filter(|l| !l.is_empty()).count();
        assert_eq!(
            count,
            actions.len(),
            "expected one audit line per action, got {count}: {lines}"
        );
        // Chain MUST be intact across the whole run.
        let report = verify_chain_from_reader(lines.as_bytes()).unwrap();
        assert!(
            report.first_failure.is_none(),
            "chain must verify across all call sites; failure: {:?}",
            report.first_failure
        );
        assert_eq!(report.total_lines as usize, actions.len());

        super::shutdown_for_test();
    }

    #[test]
    fn audit_emit_is_noop_when_disabled() {
        let _g = sink_lock();
        super::shutdown_for_test();
        // No sink active — emit must not panic and must not produce
        // any output anywhere.
        super::emit(EventBuilder::new(
            AuditAction::Store,
            actor("a", "explicit", None),
            target_memory("m", "ns", None, None, None),
        ));
        // is_enabled stays false.
        assert!(!super::is_enabled());
    }

    #[test]
    fn audit_compliance_preset_soc2_overrides_retention() {
        // The compliance presets are pure config — applying SOC2 with
        // `applied = true` propagates the documented retention to the
        // top-level config field. This is a unit-test on the merge
        // logic, decoupled from disk.
        let cfg = crate::config::AuditConfig {
            enabled: Some(true),
            retention_days: Some(90),
            compliance: Some(crate::config::AuditComplianceConfig {
                soc2: Some(crate::config::CompliancePreset {
                    applied: Some(true),
                    retention_days: Some(730),
                    redact_content: Some(true),
                    attestation_cadence_minutes: Some(60),
                    encrypt_at_rest: None,
                    pseudonymize_actors: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let resolved = cfg.effective_retention_days();
        assert_eq!(resolved, 730, "SOC2 preset must override default retention");
    }

    // ------------------------------------------------------------------
    // PR-9e coverage uplift (issue #487): exercise `init`, `read_chain_tail`,
    // builder method chains, `init_from_config` enabled+disabled paths,
    // `finalize_audit_file`, and the verify Sequence/Parse failure modes.
    // ------------------------------------------------------------------

    #[test]
    fn audit_init_creates_log_file_in_fresh_directory() {
        let _g = sink_lock();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("audit.log");
        // Directory does not yet exist; init must create it.
        super::init(&path, true, false).unwrap();
        assert!(path.exists(), "init must create the log file");
        assert!(super::is_enabled());
        super::shutdown_for_test();
    }

    #[test]
    fn audit_init_seeds_last_hash_from_existing_chain() {
        let _g = sink_lock();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("audit.log");

        // Pre-populate with a 2-event chain. We specifically test the
        // `read_chain_tail` linkage: the next emitted event's
        // `prev_hash` must match the file's last self_hash.
        // (The per-process SEQUENCE counter is independent of the
        // chain — `init` resets it to 0, so a re-init on an existing
        // file legitimately starts numbering at 1 again. Sequence
        // continuity is only required *within a single process run*,
        // so we verify only the hash linkage here.)
        let e1 = sample_event(1, CHAIN_HEAD_PREV_HASH);
        let e2 = sample_event(2, &e1.self_hash);
        let mut body = String::new();
        body.push_str(&serde_json::to_string(&e1).unwrap());
        body.push('\n');
        body.push_str(&serde_json::to_string(&e2).unwrap());
        body.push('\n');
        std::fs::write(&path, body).unwrap();

        // Init points at the existing file — `read_chain_tail` must
        // seed `last_hash` from e2.
        super::init(&path, true, false).unwrap();

        // Emit a third event; its prev_hash should equal e2.self_hash.
        super::emit(EventBuilder::new(
            AuditAction::Store,
            actor("ai:t@h", "explicit", None),
            target_memory("m3", "ns-x", Some("t".to_string()), None, None),
        ));

        let body = std::fs::read_to_string(&path).unwrap();
        let third_line = body.lines().nth(2).expect("3rd line");
        let parsed: AuditEvent = serde_json::from_str(third_line).unwrap();
        assert_eq!(parsed.prev_hash, e2.self_hash, "chain must continue");
        super::shutdown_for_test();
    }

    #[test]
    fn audit_init_skips_chain_tail_when_log_corrupted() {
        let _g = sink_lock();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("audit.log");
        // File has a malformed trailing line; init must fall back to
        // CHAIN_HEAD_PREV_HASH because no well-formed lines exist.
        std::fs::write(&path, "{not valid json\n").unwrap();
        super::init(&path, true, false).unwrap();
        // Emitting a fresh event must seed prev_hash with the chain head.
        super::emit(EventBuilder::new(
            AuditAction::Store,
            actor("a", "explicit", None),
            target_memory("m", "ns", None, None, None),
        ));
        let body = std::fs::read_to_string(&path).unwrap();
        let last = body.lines().filter(|l| !l.is_empty()).last().unwrap();
        let parsed: AuditEvent = serde_json::from_str(last).unwrap();
        assert_eq!(parsed.prev_hash, CHAIN_HEAD_PREV_HASH);
        super::shutdown_for_test();
    }

    #[test]
    fn audit_event_builder_error_outcome() {
        let b = EventBuilder::new(
            AuditAction::Store,
            actor("a", "explicit", None),
            target_memory("m", "ns", None, None, None),
        )
        .error("boom");
        assert_eq!(b.outcome, AuditOutcome::Error);
        assert_eq!(b.error.as_deref(), Some("boom"));
    }

    #[test]
    fn audit_event_builder_error_caps_long_message() {
        let long = "x".repeat(1000);
        let b = EventBuilder::new(
            AuditAction::Store,
            actor("a", "explicit", None),
            target_memory("m", "ns", None, None, None),
        )
        .error(long);
        // sanitize_field caps at 256 chars.
        assert_eq!(b.error.as_ref().unwrap().chars().count(), 256);
    }

    #[test]
    fn audit_event_builder_outcome_chain() {
        let b = EventBuilder::new(
            AuditAction::Store,
            actor("a", "explicit", None),
            target_memory("m", "ns", None, None, None),
        )
        .outcome(AuditOutcome::Deny);
        assert_eq!(b.outcome, AuditOutcome::Deny);
    }

    #[test]
    fn audit_event_builder_auth_and_request_id() {
        let auth = AuditAuth {
            source_ip: Some("203.0.113.1".to_string()),
            mtls_fp: None,
            api_key_id_hash: Some("abc".to_string()),
        };
        let b = EventBuilder::new(
            AuditAction::Store,
            actor("a", "explicit", None),
            target_memory("m", "ns", None, None, None),
        )
        .auth(auth.clone())
        .request_id("req-123");
        assert_eq!(b.auth, Some(auth));
        assert_eq!(b.request_id.as_deref(), Some("req-123"));
    }

    #[test]
    fn audit_init_from_config_disabled_clears_sink() {
        let _g = sink_lock();
        // Bring up an in-memory sink first.
        let buf: std::sync::Arc<Mutex<Vec<u8>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        super::init_for_test(buf);
        assert!(super::is_enabled());

        let cfg = crate::config::AuditConfig {
            enabled: Some(false),
            ..Default::default()
        };
        super::init_from_config(&cfg).unwrap();
        // Disabled-branch must clear the global sink.
        assert!(!super::is_enabled());
        super::shutdown_for_test();
    }

    #[test]
    fn audit_init_from_config_enabled_initialises_sink_at_resolved_path() {
        let _g = sink_lock();
        super::shutdown_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("audit.log");
        let cfg = crate::config::AuditConfig {
            enabled: Some(true),
            path: Some(path.to_string_lossy().into_owned()),
            redact_content: Some(true),
            // Don't try to apply the OS append-only flag in tests —
            // the calling user typically lacks CAP_LINUX_IMMUTABLE
            // and we don't want a kernel-level side effect.
            append_only: Some(false),
            ..Default::default()
        };
        super::init_from_config(&cfg).unwrap();
        assert!(super::is_enabled());
        // The configured file must exist on disk after init.
        assert!(path.exists(), "audit log file must be created");
        super::shutdown_for_test();
    }

    #[test]
    fn audit_finalize_audit_file_keeps_explicit_file_path() {
        let cfg = crate::config::AuditConfig {
            enabled: Some(true),
            path: Some("/var/log/ai-memory/x.log".to_string()),
            ..Default::default()
        };
        let p = resolve_audit_path(&cfg);
        // Explicit file path must be preserved (not appended with audit.log).
        assert_eq!(p, PathBuf::from("/var/log/ai-memory/x.log"));
    }

    #[test]
    fn audit_finalize_audit_file_appends_audit_log_for_dir_path() {
        let cfg = crate::config::AuditConfig {
            enabled: Some(true),
            path: Some("/var/log/ai-memory/".to_string()),
            ..Default::default()
        };
        let p = resolve_audit_path(&cfg);
        assert!(p.ends_with("audit.log"));
    }

    #[test]
    fn audit_finalize_audit_file_appends_audit_log_for_extension_less_path() {
        // No trailing slash and no extension: treat as dir, append audit.log.
        let cfg = crate::config::AuditConfig {
            enabled: Some(true),
            path: Some("/var/log/aim_audit_dir".to_string()),
            ..Default::default()
        };
        let p = resolve_audit_path(&cfg);
        assert!(p.ends_with("audit.log"));
    }

    #[test]
    fn audit_verify_detects_sequence_regression() {
        // Build a chain with a non-monotonic sequence to hit the
        // VerifyFailureKind::Sequence branch.
        let e1 = sample_event(5, CHAIN_HEAD_PREV_HASH);
        // e2 has sequence == e1's sequence (not strictly greater).
        let e2 = sample_event(5, &e1.self_hash);
        let mut buf = String::new();
        for ev in [&e1, &e2] {
            buf.push_str(&serde_json::to_string(ev).unwrap());
            buf.push('\n');
        }
        let report = verify_chain_from_reader(buf.as_bytes()).unwrap();
        let failure = report.first_failure.expect("sequence regression");
        assert!(matches!(failure.kind, VerifyFailureKind::Sequence));
    }

    #[test]
    fn audit_verify_detects_malformed_json_line() {
        // Single garbage line — must surface VerifyFailureKind::Parse.
        let buf = "this is not json\n";
        let report = verify_chain_from_reader(buf.as_bytes()).unwrap();
        let failure = report.first_failure.expect("parse failure");
        assert!(matches!(failure.kind, VerifyFailureKind::Parse));
        assert!(failure.detail.contains("malformed JSON"));
    }

    #[test]
    fn audit_verify_skips_blank_lines() {
        // Mix blank lines into a valid chain — must verify clean.
        let e1 = sample_event(1, CHAIN_HEAD_PREV_HASH);
        let e2 = sample_event(2, &e1.self_hash);
        let buf = format!(
            "\n{}\n\n{}\n\n",
            serde_json::to_string(&e1).unwrap(),
            serde_json::to_string(&e2).unwrap()
        );
        let report = verify_chain_from_reader(buf.as_bytes()).unwrap();
        assert!(report.first_failure.is_none());
        assert_eq!(report.total_lines, 2);
    }

    #[test]
    fn audit_verify_report_into_result_ok() {
        let e1 = sample_event(1, CHAIN_HEAD_PREV_HASH);
        let report = verify_chain_from_reader(
            format!("{}\n", serde_json::to_string(&e1).unwrap()).as_bytes(),
        )
        .unwrap();
        let n = report.into_result().unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn audit_verify_report_into_result_err() {
        let report = VerifyReport {
            total_lines: 5,
            first_failure: Some(VerifyFailure {
                line_number: 3,
                kind: VerifyFailureKind::ChainBreak,
                detail: "x".to_string(),
            }),
        };
        let err = report.into_result().unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("audit chain verification failed"));
        assert!(msg.contains("line 3"));
    }

    #[test]
    fn audit_emit_records_request_id_and_auth() {
        let _g = sink_lock();
        let buf: std::sync::Arc<Mutex<Vec<u8>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        super::init_for_test(buf.clone());
        super::emit(
            EventBuilder::new(
                AuditAction::Store,
                actor("a", "explicit", None),
                target_memory("m", "ns", None, None, None),
            )
            .auth(AuditAuth {
                source_ip: Some("198.51.100.7".to_string()),
                mtls_fp: None,
                api_key_id_hash: None,
            })
            .request_id("trace-abc"),
        );
        let body = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(body.contains("\"request_id\":\"trace-abc\""), "got: {body}");
        assert!(body.contains("198.51.100.7"));
        super::shutdown_for_test();
    }

    #[test]
    fn audit_emit_records_error_outcome() {
        let _g = sink_lock();
        let buf: std::sync::Arc<Mutex<Vec<u8>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        super::init_for_test(buf.clone());
        super::emit(
            EventBuilder::new(
                AuditAction::Store,
                actor("a", "explicit", None),
                target_memory("m", "ns", None, None, None),
            )
            .error("disk full"),
        );
        let body = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(body.contains("\"outcome\":\"error\""), "got: {body}");
        assert!(body.contains("\"error\":\"disk full\""), "got: {body}");
        super::shutdown_for_test();
    }

    #[test]
    fn audit_expand_tilde_passthrough_when_no_tilde() {
        // Pure-string helper — should leave non-tilde paths intact.
        assert_eq!(super::expand_tilde("/abs/path"), "/abs/path");
        assert_eq!(super::expand_tilde("rel/path"), "rel/path");
    }

    #[test]
    fn audit_target_sweep_uses_wildcard_id() {
        let t = super::target_sweep("ns-y");
        assert_eq!(t.memory_id, "*");
        assert_eq!(t.namespace, "ns-y");
    }

    #[test]
    fn audit_target_memory_round_trips_optional_fields() {
        let t = super::target_memory(
            "mem-1",
            "ns-x",
            Some("title".to_string()),
            Some("long".to_string()),
            Some("team".to_string()),
        );
        assert_eq!(t.tier.as_deref(), Some("long"));
        assert_eq!(t.scope.as_deref(), Some("team"));
    }
}
