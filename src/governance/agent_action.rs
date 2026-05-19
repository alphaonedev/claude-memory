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
//! # Wired-state (v0.7.0 7th-form closeout — issue #760)
//!
//! This module is now **wired at the harness boundary** across four
//! daemon-side wire-points enumerated in issue #691:
//!
//! | Wire-point                          | AgentAction variant   | File:line                                   |
//! |-------------------------------------|-----------------------|---------------------------------------------|
//! | Skill manifest emission             | `FilesystemWrite`     | `src/mcp/tools/skill_export.rs:162,209`     |
//! | Federation peer POST                | `NetworkRequest`      | `src/federation/sync.rs:66`                 |
//! | Hooks subprocess spawn              | `ProcessSpawn`        | `src/hooks/executor.rs:399,783`             |
//! | LLM (Ollama / OpenAI) HTTP          | `NetworkRequest`      | `src/llm.rs:421`                            |
//!
//! Every wire-point calls [`crate::governance::wire_check::check`]
//! BEFORE the external action proceeds. The daemon `bootstrap_serve`
//! installs ONE [`crate::governance::wire_check::GOVERNANCE_PRE_ACTION`]
//! closure that consults [`check_agent_action_no_audit`] against the
//! operator-signed `governance_rules` table. CLI one-shot binaries
//! never install the hook so direct operator ops stay unimpeded.
//!
//! The substrate-INTERNAL `Custom("memory_write")` gate runs through
//! the parallel [`crate::storage::GOVERNANCE_PRE_WRITE`] hook.
//!
//! Seed rules R001-R004 land at `enabled = 0` per migration
//! `0024_v07_governance_rules.sql`. The operator activates them via
//! `ai-memory governance install-defaults` (or per-rule via
//! `ai-memory rules enable <id> --sign` after running `rules keygen`).
//! Until activation the wire is mechanically inert — the audit-honest
//! property is that the wire EXISTS and is consulted on every external
//! action, not that any specific rule fires by default.

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
/// | AgentAction          | Matcher JSON shape                                                      |
/// |----------------------|-------------------------------------------------------------------------|
/// | `Bash`               | `{"command_substring":"..."}` — literal substring match on `command`    |
/// | `FilesystemWrite`    | `{"glob":"/tmp/**"}` — tiny glob over `path`                            |
/// | `NetworkRequest`     | `{"host":"evil.example.com"}` — exact host match                        |
/// | `ProcessSpawn`       | `{"binary":"cargo","disk_free_min_gib":20,"args_contain":"..."}` — binary + disk + optional argv substring |
/// | `Custom`             | `{"kind":"<kind>"}` plus optional caller-specific fields                |
///
/// # Bash field naming (SEC-12 / COR-10, Cluster D, issue #767)
///
/// The substring-match field is `command_substring`. The legacy name
/// `command_regex` is accepted as a SILENT alias for one ship cycle
/// so existing operator configs continue to load — the engine never
/// treated the value as a regex (always a literal substring). New
/// configs MUST use `command_substring`. The CLI loader emits a
/// deprecation warning when it sees the legacy name. See
/// [`validate_command_substring`] for the regex-metacharacter
/// rejection that the CLI add path enforces.
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
        AgentAction::ProcessSpawn { binary, args } => match_process_spawn(&matcher, binary, args),
        AgentAction::Custom {
            custom_kind,
            payload,
        } => match_custom(&matcher, custom_kind, payload),
    }
}

/// SEC-12 (Cluster D, issue #767) — operator-facing validator for
/// the `command_substring` matcher value. Rejects any regex
/// metacharacter the field name (`command_regex` pre-rename) used to
/// suggest the engine supported — `. * + ? [ ] ( ) ^ $ |`. The
/// engine has always treated the value as a literal substring; the
/// validator catches an operator who pastes a real regex expecting
/// it to work and would otherwise silently produce a never-matching
/// rule (e.g. `rm\s+-rf` is never a substring of `rm -rf /`).
///
/// Backslash is permitted (Windows paths, escape sequences in
/// operator-authored shell snippets) but a backslash followed by a
/// regex metacharacter is still flagged — the operator likely meant
/// "literal `.`" expecting the engine to honour the escape, which it
/// does not.
///
/// # Errors
///
/// Returns `Err(String)` describing the offending character (and
/// position) for the operator-facing CLI message. The caller surfaces
/// the error verbatim to stderr + exits non-zero.
pub fn validate_command_substring(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("command_substring must not be empty".to_string());
    }
    // Regex metacharacters the legacy `command_regex` name suggested
    // the engine honoured. The engine has always done substring; the
    // validator catches the operator misuse.
    const FORBIDDEN: &[char] = &['.', '*', '+', '?', '[', ']', '(', ')', '^', '$', '|', '\\'];
    if let Some(pos) = value.find(|c: char| FORBIDDEN.contains(&c)) {
        let offending = value.as_bytes()[pos] as char;
        return Err(format!(
            "command_substring rejects regex metacharacter {offending:?} at byte {pos}: \
             the matcher is a LITERAL substring match (despite the legacy `command_regex` \
             field name). Quote the literal text you want to match, e.g. `\"rm -rf\"` \
             rather than `\"rm\\s+-rf\"`. If you need true regex semantics, file an issue \
             — the engine will gain a typed `command_regex` discriminator in a future ship."
        ));
    }
    Ok(())
}

fn match_bash(matcher: &serde_json::Value, command: &str) -> bool {
    // SEC-12 (Cluster D, issue #767) — accept the new canonical
    // `command_substring` AND the legacy alias `command_regex` so
    // existing operator configs continue to load through the ship
    // cycle that renames the field. New configs MUST use
    // `command_substring`; the CLI add path warns when it sees the
    // legacy name.
    let needle = matcher
        .get("command_substring")
        .or_else(|| matcher.get("command_regex"))
        .and_then(|v| v.as_str());
    let Some(needle) = needle else {
        return false;
    };
    // The matcher value is a LITERAL substring (never a regex —
    // despite the legacy field name). The CLI add path validates
    // operator-supplied values with [`validate_command_substring`].
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

fn match_process_spawn(matcher: &serde_json::Value, binary: &str, args: &[String]) -> bool {
    let Some(target_binary) = matcher.get("binary").and_then(|v| v.as_str()) else {
        return false;
    };
    if target_binary != binary {
        return false;
    }
    // SEC-13 (Cluster D, issue #767) — optional `args_contain`
    // matcher. When present, the rule fires ONLY if the joined argv
    // tail (space-separated, lossy String) contains the substring.
    // Same literal-substring contract as the bash matcher — full
    // regex is intentionally out of scope.
    if let Some(needle) = matcher.get("args_contain").and_then(|v| v.as_str()) {
        let joined = args.join(" ");
        if !joined.contains(needle) {
            return false;
        }
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
// RuleEngine — unified rule-load + decision-routing core (issue #850)
// ---------------------------------------------------------------------------

/// Refactor Wave-2 Tier-A2 (issue #850) — unified rule engine consumed
/// by every governance entry point.
///
/// Before this refactor each of the three callsites that consult
/// `governance_rules` (the substrate `GOVERNANCE_PRE_WRITE` hook, the
/// `wire_check` agent-external hook, and the audited `check_agent_action`
/// MCP / CLI surface) duplicated the rule-load + first-refusal-wins
/// loop in its own function (`check_agent_action`,
/// `check_agent_action_no_audit`, `check_agent_action_deferred`).
/// Adding a new severity variant or matcher field meant touching three
/// near-identical loops. The `RuleEngine` collapses the load + routing
/// logic into one place; the three legacy free functions remain as
/// thin wrappers so the public API is wire-stable.
///
/// `rules` holds the snapshot of enabled rules of the *target kind*
/// (the engine is constructed per-action, not per-table — kind-scoped
/// loading matches the existing `list_enabled_by_kind` shape and
/// preserves the signature-verification side effects in
/// [`crate::governance::rules_store::list_enabled_by_kind`]).
///
/// The combinator is **first-refusal-wins** with `warn` falling
/// through and `log` being silent — identical semantics to the
/// pre-refactor inline loops.
pub struct RuleEngine {
    rules: Vec<Rule>,
}

impl RuleEngine {
    /// Construct an engine scoped to a single `AgentAction`'s kind.
    ///
    /// Reads the enabled rule rows of matching `kind` from
    /// `governance_rules` via
    /// [`crate::governance::rules_store::list_enabled_by_kind`]; the
    /// signature-verification gate (L1-6 bypass-impossibility
    /// invariant) runs inside that helper and is preserved verbatim.
    ///
    /// # Errors
    ///
    /// Propagates any SQLite error from `list_enabled_by_kind`.
    pub fn load_for_action(conn: &Connection, action: &AgentAction) -> Result<Self> {
        let kind = action.kind();
        let rules = crate::governance::rules_store::list_enabled_by_kind(conn, kind).with_context(
            || format!("RuleEngine::load_for_action: list_enabled_by_kind({kind})"),
        )?;
        Ok(Self { rules })
    }

    /// Construct an engine directly from a pre-loaded rules slice.
    /// Useful for tests that want to skip the SQLite round-trip or
    /// for future callsites that already hold a cached rule list.
    #[must_use]
    pub fn from_rules(rules: Vec<Rule>) -> Self {
        Self { rules }
    }

    /// Evaluate `action` against the loaded rules. Returns the
    /// first-refusal-wins [`Decision`].
    ///
    /// `agent_id` is unused by the matcher today but threaded through
    /// so future agent-scoped matchers (operator allow-lists, agent
    /// quotas) can consult it without an API break.
    #[must_use]
    pub fn evaluate(&self, _agent_id: &str, action: &AgentAction) -> Decision {
        let mut first_warn: Option<(String, String)> = None;
        for rule in &self.rules {
            if !matcher_applies(rule, action) {
                continue;
            }
            let severity = Severity::from_str(&rule.severity).unwrap_or(Severity::Log);
            match severity {
                Severity::Refuse => {
                    return Decision::Refuse {
                        rule_id: rule.id.clone(),
                        reason: rule.reason.clone(),
                    };
                }
                Severity::Warn => {
                    if first_warn.is_none() {
                        first_warn = Some((rule.id.clone(), rule.reason.clone()));
                    }
                }
                Severity::Log => {
                    // Log-only: silent in the engine. Audited entry
                    // points still emit the final decision's signed
                    // event below; per-log emission would amplify.
                }
            }
        }
        match first_warn {
            Some((rule_id, reason)) => Decision::Warn { rule_id, reason },
            None => Decision::Allow,
        }
    }

    /// Borrow the loaded rule slice. Used by [`count_matching_rules`]
    /// and by tests that want to assert load-side behaviour without
    /// running the matcher.
    #[must_use]
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }
}

// ---------------------------------------------------------------------------
// check_agent_action — the public entry point
// ---------------------------------------------------------------------------

/// Evaluate `action` against every enabled rule of matching kind in
/// the `governance_rules` table and return a [`Decision`].
///
/// Thin wrapper over [`RuleEngine::load_for_action`] +
/// [`RuleEngine::evaluate`]; the audit-emit side effect is the only
/// reason this entry point exists distinct from the `_no_audit`
/// variant.
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
    let engine = RuleEngine::load_for_action(conn, action)
        .with_context(|| format!("check_agent_action: load engine for {}", action.kind()))?;
    let decision = engine.evaluate(agent_id, action);
    emit_check_event(conn, agent_id, action, &decision)?;
    // v0.7.0 #697 — fire-and-forget forensic emit. The forensic sink
    // is process-wide, independent of the in-flight Connection (it
    // appends to its own file with no SQLite involvement), so there's
    // no deadlock risk from inside the storage hook either.
    emit_forensic_decision(agent_id, action, &decision);
    Ok(decision)
}

/// v0.7.0 #697 — translate a `(action, decision)` into the forensic
/// log shape and emit. No-op when the forensic sink is uninitialised.
fn emit_forensic_decision(agent_id: &str, action: &AgentAction, decision: &Decision) {
    let (decision_str, rule_id) = match decision {
        Decision::Allow => ("allow", String::new()),
        Decision::Refuse { rule_id, .. } => ("refuse", rule_id.clone()),
        Decision::Warn { rule_id, .. } => ("warn", rule_id.clone()),
    };
    // payload is `{action, decision_detail}` — keeps the forensic row
    // self-describing without depending on cross-table joins for a
    // SIEM walking the chain.
    let payload = serde_json::json!({
        "action": action,
        "decision_detail": decision,
    });
    crate::governance::audit::record_decision(
        agent_id,
        decision_str,
        action.kind(),
        &rule_id,
        payload,
    );
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
        ..crate::signed_events::SignedEvent::default()
    };
    append_signed_event(conn, &event).context("emit_check_event: append_signed_event")?;
    Ok(())
}

/// v0.7.0 L1-6 Deliverable E — read-only variant of [`check_agent_action`]
/// suitable for the substrate pre-write hook path.
///
/// Identical to [`check_agent_action`] except it does NOT emit a
/// `governance.check` row to `signed_events`. Two reasons the
/// pre-write hook can't use the full audit path:
///
///   1. Re-entrancy. The hook fires INSIDE `storage::insert` —
///      i.e. while the caller already holds the substrate's
///      `Connection`. Calling `append_signed_event` on a sibling
///      connection would race the write lock under WAL; calling it
///      on the same connection would corrupt the in-flight INSERT's
///      statement state.
///   2. Symmetry. The substrate-INTERNAL gate path is already
///      audited at every callsite (handlers/http.rs and mcp/tools/store.rs
///      both emit an `AuditAction::Store` row on success / a typed
///      MemoryError on failure). A second emit here would amplify.
///
/// First-refusal-wins combinator: same as the audited path. Returns
/// `Decision::Refuse { rule_id, reason }` for the first `refuse`
/// match, `Decision::Warn { rule_id, reason }` for the first `warn`
/// match when no refusal fires, otherwise `Decision::Allow`.
///
/// # Errors
///
/// Returns an error if the SQLite query for enabled rules fails.
pub fn check_agent_action_no_audit(conn: &Connection, action: &AgentAction) -> Result<Decision> {
    let engine = RuleEngine::load_for_action(conn, action).with_context(|| {
        format!(
            "check_agent_action_no_audit: load engine for {}",
            action.kind()
        )
    })?;
    // No agent_id is available on the read-only pre-write hook path.
    // Pass the empty string — the engine treats agent_id as opaque
    // until a future agent-scoped matcher consults it.
    let decision = engine.evaluate("", action);
    // v0.7.0 #697 — forensic emit even on the no-audit path. The
    // forensic sink is process-wide and writes to its own file (not
    // SQLite), so the deadlock concern that motivated `_no_audit`
    // doesn't apply.
    emit_forensic_decision("", action, &decision);
    Ok(decision)
}

/// v0.7.0 Policy-Engine Item 3 — deferred-audit variant of
/// [`check_agent_action_no_audit`] used by the substrate
/// `GOVERNANCE_PRE_WRITE` hook (issue #691 follow-up).
///
/// Identical matching semantics to [`check_agent_action_no_audit`]:
/// reads from the connection passed in (single-use, hot-path
/// no-allocation on the Allow leg). On a refusal it ALSO submits a
/// [`crate::governance::deferred_audit::DeferredAuditEvent`] to the
/// supplied queue so the background drainer can chain-log the
/// refusal to `signed_events` AFTER the in-flight write
/// transaction has released its lock.
///
/// # Why this exists
///
/// The `GOVERNANCE_PRE_WRITE` storage hook fires INSIDE
/// `storage::insert`, while the substrate's writer connection is
/// held under `Arc<Mutex<Connection>>`. Calling
/// `append_signed_event` on that same connection would re-enter the
/// in-flight INSERT and deadlock. The `_no_audit` variant solved
/// the deadlock but at the cost of dropping the chain-log property
/// for storage refusals. This variant fixes that by deferring the
/// audit write to a background tokio task with its OWN
/// `Connection` (SQLite WAL allows parallel writers).
///
/// On Allow / Warn paths the queue is NOT touched — the
/// load-bearing audit emit only happens on `Refuse`.
///
/// # Errors
///
/// Returns an error if the rules-table SELECT fails. The deferred
/// audit submit is fire-and-forget (it never errors out to the
/// caller; a closed receiver bumps a metric counter and emits a
/// tracing::warn).
pub fn check_agent_action_deferred(
    conn: &Connection,
    agent_id: &str,
    action: &AgentAction,
    queue: &crate::governance::deferred_audit::DeferredAuditQueue,
) -> Result<Decision> {
    let decision = check_agent_action_no_audit(conn, action)?;
    if decision.is_refusal() {
        // Fire-and-forget submit. Never blocks the storage write
        // path. The queue is process-wide and clone-cheap.
        queue.submit_refusal(agent_id, action, &decision);
    }
    Ok(decision)
}

/// Convenience for tests + the future K10 wiring: count how many
/// rules match the given action without running side effects.
/// Skips the audit emit (read-only).
///
/// # Errors
///
/// Returns an error if the SQLite query fails.
pub fn count_matching_rules(conn: &Connection, action: &AgentAction) -> Result<usize> {
    let engine = RuleEngine::load_for_action(conn, action)
        .with_context(|| format!("count_matching_rules: load engine for {}", action.kind()))?;
    Ok(engine
        .rules()
        .iter()
        .filter(|r| matcher_applies(r, action))
        .count())
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
                 timestamp TEXT NOT NULL,
                 -- v34 (V-4 closeout, #698) — cross-row chain columns.
                 prev_hash BLOB,
                 sequence INTEGER
             );",
        )
        .unwrap();
        conn
    }

    /// Issue #819 — short alias for the test-only thread-local guard
    /// that forces [`rules_store::resolve_operator_pubkey`] to return
    /// `None`. Tests that insert unsigned rules and expect
    /// `check_agent_action` to honor them must hold this guard for
    /// their full body, otherwise on dev hosts with a real
    /// `operator.key.pub` staged at the platform config path the
    /// L1-6 signature gate will skip the unsigned fixtures and the
    /// assertions will fail (test failures don't reproduce on
    /// clean-HOME CI; the guard makes the local dev loop match CI).
    #[must_use = "the guard must be held for the scope of the test"]
    fn no_operator_pubkey() -> rules_store::ForceNoOperatorPubkeyGuard {
        rules_store::force_no_operator_pubkey_for_test()
    }

    /// Issue #899 — guard against cross-test forensic-sink bleed.
    ///
    /// Every test that calls [`check_agent_action`] (or
    /// [`check_agent_action_no_audit`]) indirectly fires
    /// [`crate::governance::audit::record_decision`] via
    /// [`emit_forensic_decision`]. If a sibling test in
    /// `governance::audit::tests` has just initialised the
    /// process-wide forensic sink at its tempdir, this thread's
    /// `record_decision` would land a row in that sibling's
    /// tempdir — bleeding the sibling's row count.
    ///
    /// Tests that exercise `check_agent_action*` MUST hold this
    /// lock for the duration of the call. The lock is the same
    /// `OnceLock<Mutex<()>>` `audit::tests` uses, so the two
    /// modules now serialise their access to the shared sink.
    /// Acquire pattern mirrors `no_operator_pubkey`:
    ///
    /// ```ignore
    /// let _forensic = forensic_lock();
    /// let _no_pubkey = no_operator_pubkey();
    /// let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
    /// ```
    #[must_use = "the guard must be held for the scope of the test"]
    fn forensic_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::governance::audit::forensic_sink_test_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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
        let _forensic = forensic_lock();
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
        let _forensic = forensic_lock();
        let _no_pubkey = no_operator_pubkey();
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
        let _forensic = forensic_lock();
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
        let _forensic = forensic_lock();
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
        let _forensic = forensic_lock();
        let _no_pubkey = no_operator_pubkey();
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
        let _forensic = forensic_lock();
        let _no_pubkey = no_operator_pubkey();
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
        let _forensic = forensic_lock();
        let _no_pubkey = no_operator_pubkey();
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
        let _forensic = forensic_lock();
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
        let _forensic = forensic_lock();
        let _no_pubkey = no_operator_pubkey();
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
        let _forensic = forensic_lock();
        let _no_pubkey = no_operator_pubkey();
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
        let _forensic = forensic_lock();
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
        let _forensic = forensic_lock();
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
        let _no_pubkey = no_operator_pubkey();
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
        let _forensic = forensic_lock();
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
        let _forensic = forensic_lock();
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

    // -----------------------------------------------------------------
    // L1-6 Deliverable E — check_agent_action_no_audit coverage
    // (substrate pre-write hook consults this variant; identical
    // matching semantics, zero side effects on `signed_events`)
    // -----------------------------------------------------------------

    #[test]
    fn no_audit_allow_when_no_rule_matches() {
        let _forensic = forensic_lock();
        let conn = fresh_conn();
        let action = AgentAction::Bash {
            command: "ls".into(),
            cwd: None,
        };
        let decision = check_agent_action_no_audit(&conn, &action).unwrap();
        assert_eq!(decision, Decision::Allow);
        let audit_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM signed_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(audit_count, 0, "no_audit variant must not write audit rows");
    }

    #[test]
    fn no_audit_refuses_with_same_shape_as_audited_path() {
        let _forensic = forensic_lock();
        let _no_pubkey = no_operator_pubkey();
        let conn = fresh_conn();
        add_rule(
            &conn,
            "R-test",
            "custom",
            r#"{"kind":"memory_write"}"#,
            "refuse",
            true,
        );
        let action = AgentAction::Custom {
            custom_kind: "memory_write".into(),
            payload: serde_json::json!({"namespace": "secrets/api"}),
        };
        let decision = check_agent_action_no_audit(&conn, &action).unwrap();
        match decision {
            Decision::Refuse { rule_id, reason } => {
                assert_eq!(rule_id, "R-test");
                assert!(reason.contains("R-test"), "reason: {reason}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
        let audit_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM signed_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(audit_count, 0, "refusal in no_audit variant must not write");
    }

    #[test]
    fn no_audit_disabled_rule_yields_allow() {
        let _forensic = forensic_lock();
        let conn = fresh_conn();
        add_rule(
            &conn,
            "R-disabled",
            "custom",
            r#"{"kind":"memory_write"}"#,
            "refuse",
            false,
        );
        let action = AgentAction::Custom {
            custom_kind: "memory_write".into(),
            payload: serde_json::json!({}),
        };
        let decision = check_agent_action_no_audit(&conn, &action).unwrap();
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn no_audit_warn_returned_when_no_refuse_matches() {
        let _forensic = forensic_lock();
        let _no_pubkey = no_operator_pubkey();
        let conn = fresh_conn();
        add_rule(
            &conn,
            "W-test",
            "custom",
            r#"{"kind":"memory_write"}"#,
            "warn",
            true,
        );
        let action = AgentAction::Custom {
            custom_kind: "memory_write".into(),
            payload: serde_json::json!({}),
        };
        let decision = check_agent_action_no_audit(&conn, &action).unwrap();
        match decision {
            Decision::Warn { rule_id, .. } => assert_eq!(rule_id, "W-test"),
            other => panic!("expected Warn, got {other:?}"),
        }
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

    #[test]
    fn matcher_applies_returns_false_on_kind_mismatch() {
        let rule = Rule {
            id: "R".into(),
            kind: "bash".into(),
            matcher: r#"{"command_regex":"rm"}"#.into(),
            severity: "refuse".into(),
            reason: "r".into(),
            namespace: "_global".into(),
            created_by: "test".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "unsigned".into(),
        };
        let action = AgentAction::FilesystemWrite {
            path: "/x".into(),
            byte_estimate: None,
        };
        assert!(!matcher_applies(&rule, &action));
    }

    #[test]
    fn matcher_applies_returns_false_on_malformed_matcher_json() {
        let rule = Rule {
            id: "R".into(),
            kind: "bash".into(),
            matcher: "{not valid json".into(),
            severity: "refuse".into(),
            reason: "r".into(),
            namespace: "_global".into(),
            created_by: "test".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "unsigned".into(),
        };
        let action = AgentAction::Bash {
            command: "ls".into(),
            cwd: None,
        };
        assert!(!matcher_applies(&rule, &action));
    }

    #[test]
    fn matcher_applies_bash_with_missing_field_returns_false() {
        let rule = Rule {
            id: "R".into(),
            kind: "bash".into(),
            matcher: r#"{"other_field":"x"}"#.into(),
            severity: "refuse".into(),
            reason: "r".into(),
            namespace: "_global".into(),
            created_by: "test".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "unsigned".into(),
        };
        let action = AgentAction::Bash {
            command: "ls".into(),
            cwd: None,
        };
        assert!(!matcher_applies(&rule, &action));
    }

    #[test]
    fn matcher_applies_network_request_exact_host() {
        let rule = Rule {
            id: "R".into(),
            kind: "network_request".into(),
            matcher: r#"{"host":"evil.example.com"}"#.into(),
            severity: "refuse".into(),
            reason: "r".into(),
            namespace: "_global".into(),
            created_by: "test".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "unsigned".into(),
        };
        let evil = AgentAction::NetworkRequest {
            host: "evil.example.com".into(),
            scheme: "https".into(),
        };
        let good = AgentAction::NetworkRequest {
            host: "good.example.com".into(),
            scheme: "https".into(),
        };
        assert!(matcher_applies(&rule, &evil));
        assert!(!matcher_applies(&rule, &good));
    }

    #[test]
    fn matcher_applies_process_spawn_with_binary_only() {
        let rule = Rule {
            id: "R".into(),
            kind: "process_spawn".into(),
            matcher: r#"{"binary":"cargo"}"#.into(),
            severity: "refuse".into(),
            reason: "r".into(),
            namespace: "_global".into(),
            created_by: "test".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "unsigned".into(),
        };
        let cargo = AgentAction::ProcessSpawn {
            binary: "cargo".into(),
            args: vec!["build".into()],
        };
        let other = AgentAction::ProcessSpawn {
            binary: "ls".into(),
            args: vec![],
        };
        assert!(matcher_applies(&rule, &cargo));
        assert!(!matcher_applies(&rule, &other));
    }

    #[test]
    fn matcher_applies_process_spawn_with_missing_binary_field() {
        let rule = Rule {
            id: "R".into(),
            kind: "process_spawn".into(),
            matcher: r#"{}"#.into(),
            severity: "refuse".into(),
            reason: "r".into(),
            namespace: "_global".into(),
            created_by: "test".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "unsigned".into(),
        };
        let action = AgentAction::ProcessSpawn {
            binary: "cargo".into(),
            args: vec![],
        };
        assert!(!matcher_applies(&rule, &action));
    }

    #[test]
    fn matcher_applies_filesystem_write_missing_glob_field() {
        let rule = Rule {
            id: "R".into(),
            kind: "filesystem_write".into(),
            matcher: r#"{"other":"x"}"#.into(),
            severity: "refuse".into(),
            reason: "r".into(),
            namespace: "_global".into(),
            created_by: "test".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "unsigned".into(),
        };
        let action = AgentAction::FilesystemWrite {
            path: "/x".into(),
            byte_estimate: None,
        };
        assert!(!matcher_applies(&rule, &action));
    }

    #[test]
    fn matcher_applies_custom_missing_kind_field() {
        let rule = Rule {
            id: "R".into(),
            kind: "custom".into(),
            matcher: r#"{}"#.into(),
            severity: "refuse".into(),
            reason: "r".into(),
            namespace: "_global".into(),
            created_by: "test".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "unsigned".into(),
        };
        let action = AgentAction::Custom {
            custom_kind: "memory_write".into(),
            payload: serde_json::json!({}),
        };
        assert!(!matcher_applies(&rule, &action));
    }

    #[test]
    fn count_matching_rules_returns_count() {
        let _no_pubkey = no_operator_pubkey();
        let conn = fresh_conn();
        add_rule(
            &conn,
            "R1",
            "bash",
            r#"{"command_regex":"rm"}"#,
            "refuse",
            true,
        );
        add_rule(
            &conn,
            "R2",
            "bash",
            r#"{"command_regex":"rm"}"#,
            "warn",
            true,
        );
        add_rule(
            &conn,
            "R3",
            "bash",
            r#"{"command_regex":"ls"}"#,
            "refuse",
            true,
        );
        let action = AgentAction::Bash {
            command: "rm -rf".into(),
            cwd: None,
        };
        let count = count_matching_rules(&conn, &action).unwrap();
        assert_eq!(count, 2, "two rules match 'rm', one matches 'ls'");
    }

    #[test]
    fn count_matching_rules_zero_when_no_rules() {
        let conn = fresh_conn();
        let action = AgentAction::Bash {
            command: "ls".into(),
            cwd: None,
        };
        let count = count_matching_rules(&conn, &action).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn decision_matches_for_each_variant() {
        let w = Decision::Warn {
            rule_id: "W".into(),
            reason: "warn".into(),
        };
        assert!(matches!(w, Decision::Warn { .. }));
        let allow = Decision::Allow;
        assert!(matches!(allow, Decision::Allow));
        assert!(allow.is_allowed());
        let refuse = Decision::Refuse {
            rule_id: "R".into(),
            reason: "no".into(),
        };
        assert!(refuse.is_refusal());
    }

    #[test]
    fn severity_as_str_round_trip() {
        for s in [Severity::Refuse, Severity::Warn, Severity::Log] {
            let back = Severity::from_str(s.as_str()).unwrap();
            assert_eq!(s, back);
        }
    }

    #[test]
    fn agent_action_serialize_round_trip_for_each_variant() {
        let actions = [
            AgentAction::Bash {
                command: "ls".into(),
                cwd: None,
            },
            AgentAction::FilesystemWrite {
                path: "/tmp/x".into(),
                byte_estimate: Some(1024),
            },
            AgentAction::NetworkRequest {
                host: "h.example.com".into(),
                scheme: "https".into(),
            },
            AgentAction::ProcessSpawn {
                binary: "cargo".into(),
                args: vec!["build".into()],
            },
            AgentAction::Custom {
                custom_kind: "memory_write".into(),
                payload: serde_json::json!({"ns": "a"}),
            },
        ];
        for a in &actions {
            let json = serde_json::to_value(a).unwrap();
            assert!(json.is_object(), "action should serialize as object");
            // Has discriminator field.
            assert!(
                json["type"].is_string() || json["kind"].is_string() || json.get("type").is_some()
            );
        }
    }

    // -----------------------------------------------------------------
    // Refactor Wave-2 Tier-A2 (issue #850) — RuleEngine unit coverage.
    // The three entry-point wrappers (check_agent_action,
    // check_agent_action_no_audit, check_agent_action_deferred) all
    // route through RuleEngine now; the tests above already exercise
    // them at the wrapper boundary. The cases below pin the engine's
    // direct semantics so a future regression in the wrapper layer
    // shows up at the engine level too.
    // -----------------------------------------------------------------

    #[test]
    fn rule_engine_from_rules_evaluate_allow_when_no_match() {
        let engine = RuleEngine::from_rules(vec![]);
        let decision = engine.evaluate(
            "agent:t",
            &AgentAction::Bash {
                command: "ls".into(),
                cwd: None,
            },
        );
        assert_eq!(decision, Decision::Allow);
        assert!(engine.rules().is_empty());
    }

    #[test]
    fn rule_engine_first_refusal_wins_over_warn() {
        let warn_rule = Rule {
            id: "W1".into(),
            kind: "bash".into(),
            matcher: r#"{"command_substring":"rm"}"#.into(),
            severity: "warn".into(),
            reason: "warn-rm".into(),
            namespace: "_global".into(),
            created_by: "test".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "unsigned".into(),
        };
        let refuse_rule = Rule {
            id: "R1".into(),
            kind: "bash".into(),
            matcher: r#"{"command_substring":"rm -rf"}"#.into(),
            severity: "refuse".into(),
            reason: "refuse-rm-rf".into(),
            namespace: "_global".into(),
            created_by: "test".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "unsigned".into(),
        };
        // Order rules so warn comes first — first-refusal-wins must
        // still return refuse regardless of slice order.
        let engine = RuleEngine::from_rules(vec![warn_rule, refuse_rule]);
        let decision = engine.evaluate(
            "agent:t",
            &AgentAction::Bash {
                command: "rm -rf /tmp/x".into(),
                cwd: None,
            },
        );
        match decision {
            Decision::Refuse { rule_id, .. } => assert_eq!(rule_id, "R1"),
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    #[test]
    fn rule_engine_warn_when_only_warn_matches() {
        let rule = Rule {
            id: "W1".into(),
            kind: "bash".into(),
            matcher: r#"{"command_substring":"rm"}"#.into(),
            severity: "warn".into(),
            reason: "warn-rm".into(),
            namespace: "_global".into(),
            created_by: "test".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "unsigned".into(),
        };
        let engine = RuleEngine::from_rules(vec![rule]);
        let decision = engine.evaluate(
            "agent:t",
            &AgentAction::Bash {
                command: "rm /tmp/x".into(),
                cwd: None,
            },
        );
        match decision {
            Decision::Warn { rule_id, reason } => {
                assert_eq!(rule_id, "W1");
                assert_eq!(reason, "warn-rm");
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    fn rule_engine_log_severity_is_silent() {
        let rule = Rule {
            id: "L1".into(),
            kind: "bash".into(),
            matcher: r#"{"command_substring":"ls"}"#.into(),
            severity: "log".into(),
            reason: "log-ls".into(),
            namespace: "_global".into(),
            created_by: "test".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "unsigned".into(),
        };
        let engine = RuleEngine::from_rules(vec![rule]);
        let decision = engine.evaluate(
            "agent:t",
            &AgentAction::Bash {
                command: "ls -la".into(),
                cwd: None,
            },
        );
        // Log-only rules do not produce Warn or Refuse — engine
        // collapses to Allow.
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn rule_engine_load_for_action_round_trips_through_sqlite() {
        let _no_pubkey = no_operator_pubkey();
        let conn = fresh_conn();
        add_rule(
            &conn,
            "R-engine",
            "filesystem_write",
            r#"{"glob":"/tmp/**"}"#,
            "refuse",
            true,
        );
        let action = AgentAction::FilesystemWrite {
            path: "/tmp/engine.txt".into(),
            byte_estimate: None,
        };
        let engine = RuleEngine::load_for_action(&conn, &action).unwrap();
        // Engine carries exactly the kind-scoped rule we inserted.
        assert_eq!(engine.rules().len(), 1);
        assert_eq!(engine.rules()[0].id, "R-engine");
        let decision = engine.evaluate("agent:t", &action);
        match decision {
            Decision::Refuse { rule_id, .. } => assert_eq!(rule_id, "R-engine"),
            other => panic!("expected Refuse, got {other:?}"),
        }
    }
}
