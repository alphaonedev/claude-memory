// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Substrate-level agent-action rules engine (issue #691).
//!
//! The K9 governance pipeline in [`crate::governance`] gates only the
//! six substrate-INTERNAL ops ([`crate::governance::Op`]). It has no
//! insertion point for agent-EXTERNAL actions like Bash command
//! execution, filesystem writes outside the substrate, network
//! requests, or process spawns. Issue #691 RCA: every operator hard
//! rule that has ever been violated in the v0.7.0 campaign (5-6
//! occurrences of `/tmp` writes, low-disk `cargo` runs) lived OUTSIDE
//! the K9 surface.
//!
//! This module adds a second engine — [`check_agent_action`] — that
//! evaluates a declarative table of rules at every external-action
//! entry point. Rules are typed data in the `governance_rules` table
//! (migration `0024_v07_governance_rules.sql`); the engine here is
//! the read path that compiles a rule's `matcher` JSON into an
//! [`AgentAction`] match decision and returns a [`Decision`].
//!
//! # Enforcement language (honest)
//!
//! - **Substrate-INTERNAL ops** ([`memory_store`], [`memory_link`],
//!   etc.): the K9 pipeline is **substrate-authoritative** —
//!   mechanically applied at the write path. The agent cannot
//!   bypass.
//! - **Agent-EXTERNAL ops** (Bash / FilesystemWrite outside the
//!   substrate / NetworkRequest / ProcessSpawn): this engine is
//!   **substrate-rule-bound, harness-mediated**. The rule lives in
//!   the substrate's `governance_rules` table; the harness (Claude
//!   Code PreToolUse hook of type `mcp_tool`) consults the substrate
//!   via [`crate::mcp::tools::check_agent_action`] and honors the
//!   decision. That is mechanical at the **harness hook boundary**
//!   (operator-configured), not at the **agent attention** boundary
//!   (probabilistic).
//!
//! This module ships **callable but un-wired** in the substrate
//! write path. Storage::insert and `create_link_signed` do NOT
//! consult [`check_agent_action`] in this commit — a follow-up PR
//! wires the calls in after the operator runs the test-fleet audit
//! and activates seed rules R001-R004.

use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::governance::rules_store::Rule;
use crate::signed_events::{append_signed_event, payload_hash};

/// Wire-name for the `governance.check` event_type recorded in the
/// `signed_events` audit chain every time [`check_agent_action`]
/// runs. Audit-side dashboards filter on this string.
pub const GOVERNANCE_CHECK_EVENT_TYPE: &str = "governance.check";

// ---------------------------------------------------------------------------
// AgentAction — the agent-external action vocabulary
// ---------------------------------------------------------------------------

/// One agent-external action proposed for evaluation. The harness's
/// PreToolUse hook constructs one of these from the tool input and
/// hands it to [`check_agent_action`] via MCP; the CLI's `rules
/// check` verb does the same locally.
///
/// The variant names are the canonical `kind` strings in the
/// `governance_rules.kind` column (lower_snake). Adding a new variant
/// is wire-compatible — existing rules with unknown kinds are
/// ignored by the engine, not failed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentAction {
    /// A shell command the harness is about to execute. `cwd` is the
    /// resolved working directory when the harness knows it
    /// (Bash-tool calls always carry one; one-shot dispatches may
    /// not).
    Bash {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
    },
    /// A filesystem write outside the substrate (a file create /
    /// edit / append). `byte_estimate` lets a future quota rule
    /// refuse a write that would tip a disk into ENOSPC; today it
    /// is informational.
    FilesystemWrite {
        path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        byte_estimate: Option<u64>,
    },
    /// An outbound network request the harness is about to issue.
    /// `scheme` is the wire scheme (`https`, `http`, etc.) for
    /// future scheme-restrictive rules; the K9 pipeline never
    /// inspects this path.
    NetworkRequest {
        host: String,
        #[serde(default)]
        scheme: String,
    },
    /// A child-process spawn — `cargo build`, `npm install`,
    /// `colima delete`, etc. `binary` is the resolved program name
    /// (not the full path); `args` are the literal argv tail.
    ProcessSpawn {
        binary: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// Extension point for actions outside the four canonical kinds.
    /// `payload` is whatever shape the caller proposes; matcher
    /// rules of kind `custom` consult its `custom_kind` field plus
    /// their own JSON `matches` map. The inner field is named
    /// `custom_kind` rather than `kind` to avoid colliding with the
    /// outer `#[serde(tag = "kind")]` discriminator.
    Custom {
        custom_kind: String,
        payload: serde_json::Value,
    },
}

impl AgentAction {
    /// Canonical lower-snake tag used to look up rules in the
    /// `governance_rules.kind` column. Stable wire format.
    #[must_use]
    pub fn kind(&self) -> &str {
        match self {
            AgentAction::Bash { .. } => "bash",
            AgentAction::FilesystemWrite { .. } => "filesystem_write",
            AgentAction::NetworkRequest { .. } => "network_request",
            AgentAction::ProcessSpawn { .. } => "process_spawn",
            AgentAction::Custom { .. } => "custom",
        }
    }

    /// JSON shape suitable for `signed_events.payload_hash` input.
    /// Stable across versions: the field order is `kind` first then
    /// remaining variant fields. Used both for audit and for the
    /// canonical representation a future signature would commit to.
    ///
    /// # Errors
    ///
    /// Returns an error only if `serde_json` cannot serialize the
    /// variant — in practice never happens with the shapes here.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let val = serde_json::to_value(self)
            .context("agent_action canonical_bytes: serialize AgentAction")?;
        serde_json::to_vec(&val).context("agent_action canonical_bytes: re-serialize Value to vec")
    }
}

// ---------------------------------------------------------------------------
// Decision — the engine output
// ---------------------------------------------------------------------------

/// Outcome of [`check_agent_action`]. Mirrors the [`crate::governance::Decision`]
/// vocabulary but narrower: this engine has no `Modify` (rules can't
/// rewrite an external action) and no `Ask` (the harness path is
/// synchronous — operator-approval queueing is the K10 surface, not
/// this one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    /// Action proceeds. No matching `refuse` rule. There may be
    /// `warn` / `log` rules that emitted to the audit chain but the
    /// caller is cleared to proceed.
    Allow,
    /// Action refused. `rule_id` names the rule whose matcher fired;
    /// `reason` is its operator-authored explanation.
    Refuse { rule_id: String, reason: String },
    /// Action proceeds with a logged warning. `rule_id` + `reason`
    /// are present for the audit row but the harness should not
    /// block.
    Warn { rule_id: String, reason: String },
}

impl Decision {
    /// `true` if the decision blocks the action.
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        matches!(self, Decision::Refuse { .. })
    }

    /// `true` if the decision permits the action (Allow or Warn).
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        !self.is_refusal()
    }
}

// ---------------------------------------------------------------------------
// Severity — the column type in `governance_rules`
// ---------------------------------------------------------------------------

/// Per-rule severity. Drives whether a matched rule blocks the
/// action (`Refuse`), emits a logged warning (`Warn`), or is silent
/// (`Log`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Refuse,
    Warn,
    Log,
}

impl Severity {
    /// Wire string for the `governance_rules.severity` column.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Refuse => "refuse",
            Severity::Warn => "warn",
            Severity::Log => "log",
        }
    }

    /// Parse from the wire string. Returns `None` on unknown values;
    /// the caller is expected to surface a clear loader error.
    #[must_use]
    pub fn from_str(s: &str) -> Option<Severity> {
        match s {
            "refuse" => Some(Severity::Refuse),
            "warn" => Some(Severity::Warn),
            "log" => Some(Severity::Log),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Matchers — per-kind JSON evaluators
// ---------------------------------------------------------------------------

/// Evaluate whether `rule`'s `matcher` JSON applies to `action`.
///
/// Per-kind matcher shapes:
///
/// | AgentAction          | Matcher JSON shape                                            |
/// |----------------------|---------------------------------------------------------------|
/// | `Bash`               | `{"command_regex":"..."}` — substring match on `command`      |
/// | `FilesystemWrite`    | `{"glob":"/tmp/**"}` — tiny glob over `path`                  |
/// | `NetworkRequest`     | `{"host":"evil.example.com"}` — exact host match              |
/// | `ProcessSpawn`       | `{"binary":"cargo","disk_free_min_gib":20}` — binary + disk    |
/// | `Custom`             | `{"kind":"<kind>"}` plus optional caller-specific fields      |
///
/// Returns `false` on a kind/matcher mismatch (e.g. a `bash` rule
/// against a `FilesystemWrite` action) — the caller pre-filters on
/// `kind` so this should not happen, but the engine is defensive.
#[must_use]
pub fn matcher_applies(rule: &Rule, action: &AgentAction) -> bool {
    if rule.kind != action.kind() {
        return false;
    }
    let Ok(matcher) = serde_json::from_str::<serde_json::Value>(&rule.matcher) else {
        // Malformed matcher JSON — treat as non-matching rather than
        // panic. The operator-facing `ai-memory rules add` validates
        // the JSON at write time so this is a defense-in-depth
        // fallback.
        return false;
    };

    match action {
        AgentAction::Bash { command, .. } => match_bash(&matcher, command),
        AgentAction::FilesystemWrite { path, .. } => match_filesystem_write(&matcher, path),
        AgentAction::NetworkRequest { host, .. } => match_network_request(&matcher, host),
        AgentAction::ProcessSpawn { binary, .. } => match_process_spawn(&matcher, binary),
        AgentAction::Custom {
            custom_kind,
            payload,
        } => match_custom(&matcher, custom_kind, payload),
    }
}

fn match_bash(matcher: &serde_json::Value, command: &str) -> bool {
    let Some(needle) = matcher.get("command_regex").and_then(|v| v.as_str()) else {
        return false;
    };
    // We treat the `command_regex` field as a literal substring
    // today — full regex would require pulling in the `regex` crate,
    // which is fine but unnecessary for the seed rules. If a future
    // rule wants regex, a `"command_regex_kind": "regex"` discriminator
    // can switch the engine. Substring is the safe default.
    command.contains(needle)
}

fn match_filesystem_write(matcher: &serde_json::Value, path: &std::path::Path) -> bool {
    let Some(glob) = matcher.get("glob").and_then(|v| v.as_str()) else {
        return false;
    };
    let path_str = path.to_string_lossy();
    crate::governance::glob_matches(glob, &path_str)
}

fn match_network_request(matcher: &serde_json::Value, host: &str) -> bool {
    let Some(target_host) = matcher.get("host").and_then(|v| v.as_str()) else {
        return false;
    };
    // Exact match on host. A future enhancement can add `host_glob`
    // for `*.example.com`-style rules.
    target_host == host
}

fn match_process_spawn(matcher: &serde_json::Value, binary: &str) -> bool {
    let Some(target_binary) = matcher.get("binary").and_then(|v| v.as_str()) else {
        return false;
    };
    if target_binary != binary {
        return false;
    }
    // Optional `disk_free_min_gib`: refuse spawn when free disk on
    // the working volume drops below the threshold. The engine
    // probes `/` (root volume) via `statvfs`-equivalent and converts
    // to GiB. If the probe fails, we treat the rule as NOT matching
    // (avoid spurious refusals on systems where the probe is
    // unsupported); the caller can layer a stricter "refuse on
    // probe failure" policy later.
    if let Some(threshold) = matcher
        .get("disk_free_min_gib")
        .and_then(serde_json::Value::as_u64)
    {
        let free_gib = match disk_free_gib_at_root() {
            Some(g) => g,
            None => return false,
        };
        return free_gib < threshold;
    }
    true
}

fn match_custom(matcher: &serde_json::Value, kind: &str, _payload: &serde_json::Value) -> bool {
    let Some(target_kind) = matcher.get("kind").and_then(|v| v.as_str()) else {
        return false;
    };
    target_kind == kind
}

/// Probe free disk space at `/` in GiB. Returns `None` when the
/// platform does not expose the `statvfs` API or the call fails.
/// Used by [`match_process_spawn`] to evaluate the
/// `disk_free_min_gib` threshold on R004 (cargo refused on low-disk
/// system).
#[must_use]
fn disk_free_gib_at_root() -> Option<u64> {
    disk_free_gib_at_path(std::path::Path::new("/"))
}

/// Probe free disk space at `path` in GiB. Pulled out as a function
/// so tests can exercise the conversion logic against a known path
/// without depending on the root filesystem layout.
#[cfg(unix)]
fn disk_free_gib_at_path(path: &std::path::Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` reads through the C-string pointer and
    // writes to the libc::statvfs struct passed by mutable reference.
    // The struct is zeroed first; the pointer outlives the call.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path.as_ptr()` is a valid NUL-terminated C string
    // for the duration of the call; `&mut buf` is a valid mutable
    // reference. The call writes to `buf` and returns 0 on success.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &raw mut buf) };
    if rc != 0 {
        return None;
    }
    // Free blocks for unprivileged users × fragment size = free bytes.
    let free_bytes = u64::from(buf.f_bavail).saturating_mul(u64::from(buf.f_frsize));
    Some(free_bytes / (1024 * 1024 * 1024))
}

/// Windows / wasm / other-target stub. The seed rule R004 is a
/// no-op on these targets (the `cargo` refusal is a unix-host
/// concern; CI on Windows has its own disk discipline).
#[cfg(not(unix))]
fn disk_free_gib_at_path(_path: &std::path::Path) -> Option<u64> {
    None
}

// ---------------------------------------------------------------------------
// check_agent_action — the public entry point
// ---------------------------------------------------------------------------

/// Evaluate `action` against every enabled rule of matching kind in
/// the `governance_rules` table and return a [`Decision`].
///
/// The combinator is **first-refusal wins**: as soon as a `refuse`
/// rule matches, the engine returns `Refuse` and stops scanning
/// (subsequent matches are not evaluated). If no `refuse` rule
/// matches, the engine returns the first `warn` match (or `Allow`
/// if none).
///
/// Every call — refusal AND allow — emits one row to the
/// `signed_events` audit table with `event_type =
/// "governance.check"` and `payload_hash` over the canonical
/// representation of (action, decision). This is the load-bearing
/// audit chain for the v1.0 procurement review.
///
/// # Errors
///
/// Returns an error if the SQLite query fails or the audit emit
/// fails. A serde encoding error on `canonical_bytes` is propagated.
///
/// # Examples
///
/// ```ignore
/// # use ai_memory::governance::agent_action::{AgentAction, Decision, check_agent_action};
/// # use rusqlite::Connection;
/// let conn: Connection = todo!();
/// let action = AgentAction::FilesystemWrite {
///     path: "/tmp/foo".into(),
///     byte_estimate: None,
/// };
/// let decision = check_agent_action(&conn, "agent:test", &action)?;
/// match decision {
///     Decision::Refuse { rule_id, reason } => {
///         eprintln!("refused by {rule_id}: {reason}");
///     }
///     _ => { /* proceed */ }
/// }
/// # Ok::<_, anyhow::Error>(())
/// ```
pub fn check_agent_action(
    conn: &Connection,
    agent_id: &str,
    action: &AgentAction,
) -> Result<Decision> {
    let kind = action.kind();
    let rules = crate::governance::rules_store::list_enabled_by_kind(conn, kind)
        .with_context(|| format!("check_agent_action: list_enabled_by_kind({kind})"))?;

    let mut first_warn: Option<(String, String)> = None;

    for rule in &rules {
        if !matcher_applies(rule, action) {
            continue;
        }
        let severity = Severity::from_str(&rule.severity).unwrap_or(Severity::Log);
        match severity {
            Severity::Refuse => {
                let decision = Decision::Refuse {
                    rule_id: rule.id.clone(),
                    reason: rule.reason.clone(),
                };
                emit_check_event(conn, agent_id, action, &decision)?;
                return Ok(decision);
            }
            Severity::Warn => {
                if first_warn.is_none() {
                    first_warn = Some((rule.id.clone(), rule.reason.clone()));
                }
            }
            Severity::Log => {
                // Log-only: write the audit row at the end with the
                // final decision (which may still be Allow if no
                // higher-severity rule fires); the per-log row is
                // not emitted separately to avoid amplification.
            }
        }
    }

    let decision = match first_warn {
        Some((rule_id, reason)) => Decision::Warn { rule_id, reason },
        None => Decision::Allow,
    };
    emit_check_event(conn, agent_id, action, &decision)?;
    Ok(decision)
}

/// Append a `governance.check` row to `signed_events`. Helper so
/// every exit point in [`check_agent_action`] is symmetric (audit
/// chain is otherwise lossy on the Refuse short-circuit path).
fn emit_check_event(
    conn: &Connection,
    agent_id: &str,
    action: &AgentAction,
    decision: &Decision,
) -> Result<()> {
    // Canonical representation: serialize {action, decision} as a
    // stable JSON object and hash it. A future format-agility
    // change recomputes the hash over a different canonical
    // encoding without touching the call sites.
    let canonical = serde_json::json!({
        "action": action,
        "decision": decision,
    });
    let bytes =
        serde_json::to_vec(&canonical).context("emit_check_event: serialize canonical payload")?;
    let hash = payload_hash(&bytes);
    let event = crate::signed_events::SignedEvent {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        event_type: GOVERNANCE_CHECK_EVENT_TYPE.to_string(),
        payload_hash: hash,
        signature: None,
        attest_level: "unsigned".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    append_signed_event(conn, &event).context("emit_check_event: append_signed_event")?;
    Ok(())
}

/// Convenience for tests + the future K10 wiring: count how many
/// rules match the given action without running side effects.
/// Skips the audit emit (read-only).
///
/// # Errors
///
/// Returns an error if the SQLite query fails.
pub fn count_matching_rules(conn: &Connection, action: &AgentAction) -> Result<usize> {
    let kind = action.kind();
    let rules = crate::governance::rules_store::list_enabled_by_kind(conn, kind)
        .with_context(|| format!("count_matching_rules: list_enabled_by_kind({kind})"))?;
    Ok(rules.iter().filter(|r| matcher_applies(r, action)).count())
}

/// Read-side helper: return the most-recent `governance.check`
/// audit row for `agent_id` (or any agent when `agent_id` is None).
/// Used by the MCP `rule_list` tool to surface "last check" info
/// in the operator UI.
///
/// # Errors
///
/// Returns an error if the SQLite query fails.
pub fn most_recent_check(conn: &Connection, agent_id: Option<&str>) -> Result<Option<String>> {
    let row: Option<String> = if let Some(aid) = agent_id {
        conn.query_row(
            "SELECT timestamp FROM signed_events \
             WHERE event_type = ?1 AND agent_id = ?2 \
             ORDER BY timestamp DESC LIMIT 1",
            rusqlite::params![GOVERNANCE_CHECK_EVENT_TYPE, aid],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    } else {
        conn.query_row(
            "SELECT timestamp FROM signed_events \
             WHERE event_type = ?1 \
             ORDER BY timestamp DESC LIMIT 1",
            rusqlite::params![GOVERNANCE_CHECK_EVENT_TYPE],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    };
    Ok(row)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::governance::rules_store;

    /// Build a fresh in-memory connection with the governance_rules
    /// table and the signed_events table — the engine's only two
    /// dependencies. Avoids pulling in the full migration ladder
    /// (which would also drag in fts5 / hnsw / etc.).
    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE governance_rules (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 matcher TEXT NOT NULL,
                 severity TEXT NOT NULL,
                 reason TEXT NOT NULL,
                 namespace TEXT NOT NULL DEFAULT '_global',
                 created_by TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 enabled INTEGER NOT NULL DEFAULT 1,
                 signature BLOB,
                 attest_level TEXT NOT NULL DEFAULT 'unsigned'
             );
             CREATE TABLE signed_events (
                 id TEXT PRIMARY KEY,
                 agent_id TEXT NOT NULL,
                 event_type TEXT NOT NULL,
                 payload_hash BLOB NOT NULL,
                 signature BLOB,
                 attest_level TEXT NOT NULL DEFAULT 'unsigned',
                 timestamp TEXT NOT NULL
             );",
        )
        .unwrap();
        conn
    }

    fn add_rule(
        conn: &Connection,
        id: &str,
        kind: &str,
        matcher: &str,
        severity: &str,
        enabled: bool,
    ) {
        rules_store::insert(
            conn,
            &Rule {
                id: id.to_string(),
                kind: kind.to_string(),
                matcher: matcher.to_string(),
                severity: severity.to_string(),
                reason: format!("{id}: test"),
                namespace: "_global".to_string(),
                created_by: "test".to_string(),
                created_at: 0,
                enabled,
                signature: None,
                attest_level: "unsigned".to_string(),
            },
        )
        .unwrap();
    }

    #[test]
    fn agent_action_kind_strings_are_stable() {
        assert_eq!(
            AgentAction::Bash {
                command: "ls".into(),
                cwd: None
            }
            .kind(),
            "bash"
        );
        assert_eq!(
            AgentAction::FilesystemWrite {
                path: "/x".into(),
                byte_estimate: None
            }
            .kind(),
            "filesystem_write"
        );
        assert_eq!(
            AgentAction::NetworkRequest {
                host: "h".into(),
                scheme: "https".into()
            }
            .kind(),
            "network_request"
        );
        assert_eq!(
            AgentAction::ProcessSpawn {
                binary: "b".into(),
                args: vec![]
            }
            .kind(),
            "process_spawn"
        );
        assert_eq!(
            AgentAction::Custom {
                custom_kind: "k".into(),
                payload: serde_json::json!({})
            }
            .kind(),
            "custom"
        );
    }

    #[test]
    fn severity_roundtrip() {
        for s in &[Severity::Refuse, Severity::Warn, Severity::Log] {
            assert_eq!(Severity::from_str(s.as_str()), Some(*s));
        }
        assert_eq!(Severity::from_str("nope"), None);
    }

    #[test]
    fn allow_when_no_rule_matches() {
        let conn = fresh_conn();
        let action = AgentAction::Bash {
            command: "ls -la".into(),
            cwd: None,
        };
        let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
        assert_eq!(decision, Decision::Allow);
        assert!(decision.is_allowed());
    }

    #[test]
    fn refuse_filesystem_write_glob_match() {
        let conn = fresh_conn();
        add_rule(
            &conn,
            "R001",
            "filesystem_write",
            r#"{"glob":"/tmp/**"}"#,
            "refuse",
            true,
        );
        let action = AgentAction::FilesystemWrite {
            path: "/tmp/foo.txt".into(),
            byte_estimate: None,
        };
        let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
        assert!(decision.is_refusal());
        match decision {
            Decision::Refuse { rule_id, .. } => assert_eq!(rule_id, "R001"),
            _ => panic!("expected refuse"),
        }
    }

    #[test]
    fn allow_filesystem_write_outside_glob() {
        let conn = fresh_conn();
        add_rule(
            &conn,
            "R001",
            "filesystem_write",
            r#"{"glob":"/tmp/**"}"#,
            "refuse",
            true,
        );
        let action = AgentAction::FilesystemWrite {
            path: "/Users/foo/safe.txt".into(),
            byte_estimate: None,
        };
        let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn disabled_rule_does_not_match() {
        let conn = fresh_conn();
        add_rule(
            &conn,
            "R001",
            "filesystem_write",
            r#"{"glob":"/tmp/**"}"#,
            "refuse",
            false, // disabled
        );
        let action = AgentAction::FilesystemWrite {
            path: "/tmp/foo".into(),
            byte_estimate: None,
        };
        let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn warn_rule_returns_warn_not_refuse() {
        let conn = fresh_conn();
        add_rule(
            &conn,
            "W001",
            "bash",
            r#"{"command_regex":"rm -rf"}"#,
            "warn",
            true,
        );
        let action = AgentAction::Bash {
            command: "rm -rf /opt/scratch".into(),
            cwd: None,
        };
        let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
        match decision {
            Decision::Warn { rule_id, .. } => assert_eq!(rule_id, "W001"),
            _ => panic!("expected warn"),
        }
    }

    #[test]
    fn refuse_wins_over_warn_when_both_match() {
        let conn = fresh_conn();
        add_rule(
            &conn,
            "W001",
            "bash",
            r#"{"command_regex":"rm"}"#,
            "warn",
            true,
        );
        add_rule(
            &conn,
            "R900",
            "bash",
            r#"{"command_regex":"rm -rf /"}"#,
            "refuse",
            true,
        );
        let action = AgentAction::Bash {
            command: "rm -rf /".into(),
            cwd: None,
        };
        let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
        assert!(decision.is_refusal());
    }

    #[test]
    fn process_spawn_binary_match() {
        let conn = fresh_conn();
        add_rule(
            &conn,
            "R-cargo",
            "process_spawn",
            r#"{"binary":"cargo"}"#,
            "refuse",
            true,
        );
        let action = AgentAction::ProcessSpawn {
            binary: "cargo".into(),
            args: vec!["build".into()],
        };
        let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
        assert!(decision.is_refusal());
    }

    #[test]
    fn process_spawn_binary_mismatch_allows() {
        let conn = fresh_conn();
        add_rule(
            &conn,
            "R-cargo",
            "process_spawn",
            r#"{"binary":"cargo"}"#,
            "refuse",
            true,
        );
        let action = AgentAction::ProcessSpawn {
            binary: "npm".into(),
            args: vec!["install".into()],
        };
        let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn network_request_exact_host_match() {
        let conn = fresh_conn();
        add_rule(
            &conn,
            "R-evil",
            "network_request",
            r#"{"host":"evil.example.com"}"#,
            "refuse",
            true,
        );
        let action = AgentAction::NetworkRequest {
            host: "evil.example.com".into(),
            scheme: "https".into(),
        };
        let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
        assert!(decision.is_refusal());

        let allow_action = AgentAction::NetworkRequest {
            host: "good.example.com".into(),
            scheme: "https".into(),
        };
        let allow_decision = check_agent_action(&conn, "agent:t", &allow_action).unwrap();
        assert_eq!(allow_decision, Decision::Allow);
    }

    #[test]
    fn custom_action_matches_on_kind() {
        let conn = fresh_conn();
        add_rule(
            &conn,
            "R-custom",
            "custom",
            r#"{"kind":"approve_deploy"}"#,
            "refuse",
            true,
        );
        let action = AgentAction::Custom {
            custom_kind: "approve_deploy".into(),
            payload: serde_json::json!({"env": "prod"}),
        };
        let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
        assert!(decision.is_refusal());
    }

    #[test]
    fn check_emits_signed_event() {
        let conn = fresh_conn();
        let action = AgentAction::Bash {
            command: "ls".into(),
            cwd: None,
        };
        let _ = check_agent_action(&conn, "agent:test", &action).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM signed_events WHERE event_type = ?1 AND agent_id = ?2",
                rusqlite::params![GOVERNANCE_CHECK_EVENT_TYPE, "agent:test"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn refuse_short_circuit_still_emits_event() {
        let conn = fresh_conn();
        add_rule(
            &conn,
            "R001",
            "filesystem_write",
            r#"{"glob":"/tmp/**"}"#,
            "refuse",
            true,
        );
        let action = AgentAction::FilesystemWrite {
            path: "/tmp/x".into(),
            byte_estimate: None,
        };
        let _ = check_agent_action(&conn, "agent:t", &action).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM signed_events WHERE event_type = ?1",
                rusqlite::params![GOVERNANCE_CHECK_EVENT_TYPE],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn count_matching_rules_skips_audit() {
        let conn = fresh_conn();
        add_rule(
            &conn,
            "R1",
            "bash",
            r#"{"command_regex":"foo"}"#,
            "refuse",
            true,
        );
        let action = AgentAction::Bash {
            command: "foo bar".into(),
            cwd: None,
        };
        assert_eq!(count_matching_rules(&conn, &action).unwrap(), 1);
        // No audit row written by count.
        let audit_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM signed_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(audit_count, 0);
    }

    #[test]
    fn malformed_matcher_does_not_panic() {
        let conn = fresh_conn();
        add_rule(&conn, "R-bad", "bash", "not json", "refuse", true);
        let action = AgentAction::Bash {
            command: "anything".into(),
            cwd: None,
        };
        let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn matcher_applies_kind_mismatch_returns_false() {
        let rule = Rule {
            id: "R".to_string(),
            kind: "bash".to_string(),
            matcher: r#"{"command_regex":"x"}"#.to_string(),
            severity: "refuse".to_string(),
            reason: "r".to_string(),
            namespace: "_global".to_string(),
            created_by: "test".to_string(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "unsigned".to_string(),
        };
        let action = AgentAction::FilesystemWrite {
            path: "/x".into(),
            byte_estimate: None,
        };
        assert!(!matcher_applies(&rule, &action));
    }

    #[test]
    fn canonical_bytes_includes_kind() {
        let a = AgentAction::Bash {
            command: "ls".into(),
            cwd: None,
        };
        let bytes = a.canonical_bytes().unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("\"kind\""), "got {s}");
        assert!(s.contains("\"bash\""), "got {s}");
    }

    #[test]
    fn most_recent_check_empty_returns_none() {
        let conn = fresh_conn();
        assert_eq!(most_recent_check(&conn, None).unwrap(), None);
        assert_eq!(most_recent_check(&conn, Some("agent:x")).unwrap(), None);
    }

    #[test]
    fn most_recent_check_returns_latest() {
        let conn = fresh_conn();
        let action = AgentAction::Bash {
            command: "x".into(),
            cwd: None,
        };
        check_agent_action(&conn, "agent:a", &action).unwrap();
        assert!(most_recent_check(&conn, Some("agent:a")).unwrap().is_some());
        assert!(most_recent_check(&conn, Some("agent:b")).unwrap().is_none());
        assert!(most_recent_check(&conn, None).unwrap().is_some());
    }

    #[test]
    fn decision_serializes_as_tagged_enum() {
        let d = Decision::Refuse {
            rule_id: "R1".to_string(),
            reason: "no".to_string(),
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["decision"], "refuse");
        assert_eq!(v["rule_id"], "R1");
        let allow = Decision::Allow;
        let av = serde_json::to_value(&allow).unwrap();
        assert_eq!(av["decision"], "allow");
    }
}
