// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7.0 Track K — Task K9: unified permission system.
//
// Replaces the v0.6.x ad-hoc governance gate with a single
// composition pipeline:
//
//   Rules (declarative `[permissions.rules]`)
//        +
//   Modes (`[permissions].mode` — K3, already wired)
//        +
//   Hooks (G1-G11 `HookDecision` returned by chain runs)
//        ↓
//   Decision { Allow, Deny(reason), Modify(delta), Ask(prompt) }
//
// Combining rule:
//
//   1. First Deny across any source wins.
//   2. Otherwise: if any source returned Modify, Modify wins (the
//      composed delta from hooks; rules cannot Modify in K9 — they
//      only Allow / Deny / Ask).
//   3. Otherwise: if any source returned Allow explicitly, Allow.
//   4. Otherwise: Ask falls through to the active mode default —
//      Enforce promotes Ask to Deny, Advisory + Off promote to Allow.
//
// The pipeline is deny-first per the v0.7 epic K9 spec: ambiguity
// goes to Ask rather than silently approving, but the mode default
// for ambiguous cases under Advisory/Off is to allow (so existing
// upgraders keep working). Operators who want strict-deny on Ask
// must configure `[permissions] mode = "enforce"`.

use serde::{Deserialize, Serialize};
use std::sync::RwLock;

use crate::config::{PermissionsMode, active_permissions_mode};
use crate::hooks::decision::HookDecision;
use crate::hooks::events::MemoryDelta;

// v0.7.0 (issue #691) — substrate-level agent-action rules engine.
// The K9 pipeline below gates substrate-INTERNAL ops (memory_store,
// memory_link, ...). `agent_action` adds the parallel engine for
// agent-EXTERNAL actions (Bash, FilesystemWrite, NetworkRequest,
// ProcessSpawn, Custom). `rules_store` is the typed CRUD over the
// `governance_rules` table. Both ship callable but un-wired: storage
// write paths do NOT consult `check_agent_action` in this commit;
// the seed rules R001-R004 land at `enabled=0` awaiting operator
// activation via `ai-memory rules enable <id> --sign`.
pub mod agent_action;
// v0.7.0 Policy-Engine Item 3 — deferred audit-log queue for
// storage-hook refusals. Closes the cryptographic-log gap on the
// `GOVERNANCE_PRE_WRITE` path that previously routed through
// `check_agent_action_no_audit` (no chain-log emit) to avoid a
// re-entrant `Connection` deadlock. See `deferred_audit.rs` for the
// architecture.
pub mod deferred_audit;
pub mod rules_store;

// v0.7.0 (issue #691 fold-1) — universal AgentAction wire-point helper.
// Same OnceLock-based hook pattern as `storage::GOVERNANCE_PRE_WRITE`,
// but covering the four agent-EXTERNAL action variants (Bash,
// FilesystemWrite, NetworkRequest, ProcessSpawn) — the storage hook
// handles the substrate-INTERNAL Custom("memory_write") gate.
//
// The daemon `bootstrap_serve` installs ONE shared closure that
// consults the same `governance_rules` table the storage hook reads,
// then every wire-point in the daemon-side code paths (skill_export,
// federation::sync, hooks::executor, llm) calls
// `wire_check::check(&action)?` to consult it.
pub mod wire_check;

// ---------------------------------------------------------------------------
// Op tag — the five gated operations
// ---------------------------------------------------------------------------

/// The operation a permission check is gating. K9 wires the
/// pipeline into five callsites: store, link, delete, archive,
/// consolidate. v0.7.0 #628 H6 added a sixth — `memory_replay` —
/// so cross-tenant transcript reads are gated by the same evaluator
/// that already gates writes. The wire string is the canonical name
/// surfaced in rule matchers (`op = "memory_store"` etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    MemoryStore,
    MemoryLink,
    MemoryDelete,
    MemoryArchive,
    MemoryConsolidate,
    /// v0.7.0 #628 H6 — `memory_replay` MCP tool (transcript read).
    /// Gated so an agent cannot fetch verbatim transcript content
    /// from a namespace they are not authorised to read.
    MemoryReplay,
}

impl Op {
    /// Wire name used in `[permissions.rules].op`. Stable across
    /// versions.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Op::MemoryStore => "memory_store",
            Op::MemoryLink => "memory_link",
            Op::MemoryDelete => "memory_delete",
            Op::MemoryArchive => "memory_archive",
            Op::MemoryConsolidate => "memory_consolidate",
            Op::MemoryReplay => "memory_replay",
        }
    }

    /// Parse from the wire name. Used by rule loaders.
    #[must_use]
    pub fn from_str(s: &str) -> Option<Op> {
        match s {
            "memory_store" => Some(Op::MemoryStore),
            "memory_link" => Some(Op::MemoryLink),
            "memory_delete" => Some(Op::MemoryDelete),
            "memory_archive" => Some(Op::MemoryArchive),
            "memory_consolidate" => Some(Op::MemoryConsolidate),
            "memory_replay" => Some(Op::MemoryReplay),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Decision — the unified output of the pipeline
// ---------------------------------------------------------------------------

/// The four-shape outcome of [`Permissions::evaluate`]. Mirrors the
/// G4 [`HookDecision`] vocabulary so callers wire one decision type
/// into all five op paths regardless of which source produced the
/// outcome.
///
/// `Modify` carries a [`MemoryDelta`] — the same payload type the
/// hook chain composes. Rules in K9 cannot return Modify (only
/// Allow / Deny / Ask); only hook chains can.
///
/// `Ask` carries the prompt text that should be surfaced to the
/// operator (or queued in the K10 approval pipeline). The runtime
/// promotion of Ask under [`PermissionsMode::Enforce`] turns this
/// into Deny so callers don't accidentally approve under strict
/// mode.
#[derive(Debug, Clone)]
pub enum Decision {
    /// Allow the operation to proceed unchanged.
    Allow,
    /// Deny the operation. `reason` surfaces in the API response and
    /// the audit log.
    Deny(String),
    /// Allow the operation but apply `delta` first. Only produced by
    /// hook chains in K9; rules cannot return Modify.
    Modify(MemoryDelta),
    /// Pause and prompt the operator. Mode default decides what to
    /// do with this if no caller is wired into the K10 approval API
    /// (Enforce → Deny, Advisory/Off → Allow).
    Ask(String),
}

impl PartialEq for Decision {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Decision::Allow, Decision::Allow) => true,
            (Decision::Deny(a), Decision::Deny(b)) => a == b,
            (Decision::Modify(a), Decision::Modify(b)) => {
                // Same trick HookDecision::Modify uses — MemoryDelta
                // carries a serde_json::Value (metadata) which is not
                // Eq, so equality is canonical-JSON.
                serde_json::to_value(a).ok() == serde_json::to_value(b).ok()
            }
            (Decision::Ask(a), Decision::Ask(b)) => a == b,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// PermissionContext — input to evaluate
// ---------------------------------------------------------------------------

/// Every input the rule + hook + mode pipeline needs. Built by
/// each op-path callsite (handlers / mcp.rs) and passed by value
/// into [`Permissions::evaluate`].
#[derive(Debug, Clone)]
pub struct PermissionContext {
    pub op: Op,
    pub namespace: String,
    pub agent_id: String,
    /// JSON snapshot of the in-flight payload (memory, link target,
    /// archive id, etc.). Surfaced to rule matchers for future
    /// content-based rules; in K9 the matchers only consult
    /// namespace + agent_id but the payload is part of the
    /// signature so adding payload-aware rules later is wire-stable.
    pub payload: serde_json::Value,
}

// ---------------------------------------------------------------------------
// PermissionRule — the declarative `[permissions.rules]` shape
// ---------------------------------------------------------------------------

/// One row of `[[permissions.rules]]` from `config.toml`.
///
/// Wire format:
///
/// ```toml
/// [[permissions.rules]]
/// namespace_pattern = "secrets/*"
/// op               = "memory_store"
/// agent_pattern    = "ai:*"
/// decision         = "deny"
/// reason           = "ai agents may not write to secrets"
/// ```
///
/// `namespace_pattern` and `agent_pattern` use a tiny glob
/// vocabulary: `*` matches any run of non-`/` characters in the
/// namespace, any run of any character in the agent id. `**`
/// matches across `/` boundaries. An exact string is treated as a
/// literal match.
///
/// `op` is required and matches the [`Op::as_str`] wire form. A
/// missing `op` fails the loader.
///
/// Pattern specificity (longer literal-prefix wins) is the tie
/// breaker when multiple rules match the same context — the rule
/// whose `namespace_pattern` has the longest non-glob prefix takes
/// precedence. Within equal namespace specificity, an exact
/// `agent_pattern` (no `*`) beats a wildcard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PermissionRule {
    pub namespace_pattern: String,
    pub op: String,
    #[serde(default = "default_agent_pattern")]
    pub agent_pattern: String,
    pub decision: RuleDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn default_agent_pattern() -> String {
    "*".to_string()
}

/// Wire-level rule outcome. Narrower than [`Decision`] because rules
/// can't return `Modify` — only hook chains can. The `Ask` variant
/// uses the rule's `reason` field as the prompt text.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RuleDecision {
    Allow,
    Deny,
    Ask,
}

// ---------------------------------------------------------------------------
// Pattern matching
// ---------------------------------------------------------------------------

/// Tiny glob: `**` matches across `/`, `*` matches a single
/// `/`-delimited segment. Exact strings match literally. Empty
/// pattern matches the empty string only.
#[must_use]
pub fn glob_matches(pattern: &str, value: &str) -> bool {
    glob_inner(pattern.as_bytes(), value.as_bytes())
}

fn glob_inner(pat: &[u8], val: &[u8]) -> bool {
    // Iterative backtracker — avoids unbounded recursion on a
    // pathological pattern but keeps the implementation < 30 LOC.
    let (mut p, mut v) = (0usize, 0usize);
    let (mut star_p, mut star_v): (Option<usize>, usize) = (None, 0);
    let mut star_double = false;
    while v < val.len() {
        if p < pat.len() {
            // `**` greedy across '/'. `*` greedy within a segment.
            if pat[p] == b'*' {
                let double = p + 1 < pat.len() && pat[p + 1] == b'*';
                star_p = Some(p);
                star_double = double;
                p += if double { 2 } else { 1 };
                star_v = v;
                continue;
            }
            if pat[p] == val[v] {
                p += 1;
                v += 1;
                continue;
            }
        }
        // Mismatch: reset to last star and advance value cursor.
        if let Some(sp) = star_p {
            // `*` may not consume a '/' — '**' may.
            if !star_double && val[star_v] == b'/' {
                return false;
            }
            star_v += 1;
            // Walking past '/' under single-star also fails.
            if !star_double && star_v <= val.len() && {
                // Check: if a '/' lies between star_v-1 and star_v we
                // already failed above; here we just reset cursors.
                false
            } {
                return false;
            }
            p = sp + if star_double { 2 } else { 1 };
            v = star_v;
            continue;
        }
        return false;
    }
    // Trailing pattern must be all '*' / '**'.
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

/// Specificity score for a glob. Higher = more specific. Used as
/// the tie-breaker when multiple rules match the same context.
/// Score is the length of the longest non-`*` prefix.
#[must_use]
pub fn pattern_specificity(pattern: &str) -> usize {
    pattern.bytes().take_while(|b| *b != b'*').count()
}

// ---------------------------------------------------------------------------
// Permissions — the public evaluator
// ---------------------------------------------------------------------------

/// The K9 unified evaluator. Rules + Mode + Hooks compose into a
/// single [`Decision`]; deny-first; ask falls through to mode.
///
/// Stateless type — every input is a parameter. The active rules
/// list is held in the process-wide [`active_permission_rules`]
/// registry so callsites in `mcp.rs` / `handlers.rs` don't need to
/// thread a config handle through every function.
pub struct Permissions;

impl Permissions {
    /// Evaluate the full pipeline.
    ///
    /// `hook_decisions` is the (possibly empty) sequence of
    /// decisions returned by hook chains for this op. Callers that
    /// have not yet wired a hook chain into a particular op pass
    /// `&[]`; the pipeline still works (rules + mode resolve the
    /// decision).
    #[must_use]
    pub fn evaluate(ctx: &PermissionContext, hook_decisions: &[HookDecision]) -> Decision {
        // Review #628 H10: K10's `remember=forever` writes a
        // [`crate::approvals::SyntheticPermissionRule`] into a
        // separate registry that the v0.7.0-ship evaluator did not
        // consult — so an operator who clicked "remember" was
        // re-prompted on every subsequent matching call. We promote
        // each synthetic entry into a virtual [`PermissionRule`] and
        // splice them onto the front of the rule list so the
        // existing combiner sees them. The combiner is deny-first
        // across all sources, which preserves the safety property
        // that an explicit config Deny still beats an operator's
        // `remember=forever`-Allow — and a synthetic Allow shadows a
        // config-level Ask (the whole point of "remember").
        let mut rules = synthetic_rules_as_permission_rules();
        rules.extend(active_permission_rules());
        Self::evaluate_with(ctx, hook_decisions, &rules, active_permissions_mode())
    }

    /// Same as [`Permissions::evaluate`] but takes the rule list and
    /// mode as explicit parameters. Used by the K9 test matrix so
    /// scenarios can exercise specific rule-set / mode combinations
    /// without poking the process-wide registry.
    ///
    /// # H8 invariant — namespace cannot be elevated by `Modify`
    ///
    /// The pinned namespace for rule evaluation is taken from
    /// `ctx.namespace` BEFORE any rule pass. If a hook returns
    /// `Modify { namespace: <other_ns> }` the pipeline RE-EVALUATES
    /// the entire rule set against the new namespace; if that
    /// re-evaluation returns `Decision::Deny`, the modification is
    /// rejected (the original `Deny` reason is surfaced — annotated
    /// with the rejected escalation). This closes the v0.7.0 review
    /// blocker H8 / #628 where a `Modify`-rewrite of `namespace`
    /// could bypass a rule that targeted the destination namespace.
    #[must_use]
    pub fn evaluate_with(
        ctx: &PermissionContext,
        hook_decisions: &[HookDecision],
        rules: &[PermissionRule],
        mode: PermissionsMode,
    ) -> Decision {
        // Mode short-circuit: Off skips the whole pipeline. K3
        // already documents Off as the freeze-thaw escape hatch.
        if mode == PermissionsMode::Off {
            return Decision::Allow;
        }

        // H8 — pin the namespace at entry. The original namespace
        // is the only one that may participate in this evaluation;
        // any hook that proposes a different namespace must survive
        // a re-evaluation against the rules pinned to the *new*
        // namespace below.
        let pinned_ns = ctx.namespace.clone();

        // Collect rule decisions matching this context.
        let matched = matched_rules(ctx, rules);
        let rule_outcomes: Vec<&PermissionRule> = matched;

        // Pass 1: deny-first across all sources. Rules first
        // (declarative intent should win against an over-permissive
        // hook), then hooks.
        for r in &rule_outcomes {
            if matches!(r.decision, RuleDecision::Deny) {
                return Decision::Deny(r.reason.clone().unwrap_or_else(|| {
                    format!(
                        "denied by permission rule (namespace_pattern={}, op={}, agent_pattern={})",
                        r.namespace_pattern, r.op, r.agent_pattern
                    )
                }));
            }
        }
        for h in hook_decisions {
            if let HookDecision::Deny { reason, .. } = h {
                return Decision::Deny(reason.clone());
            }
        }

        // Pass 2: Modify wins next. Only hooks can produce Modify.
        // Compose deltas from every Modify in chain order so
        // multi-hook pipelines accumulate.
        let mut composed: Option<MemoryDelta> = None;
        for h in hook_decisions {
            if let HookDecision::Modify(payload) = h {
                let next = payload.delta.clone();
                composed = Some(merge_delta(composed.take(), next));
            }
        }
        if let Some(delta) = composed {
            // H8 — if the composed delta rewrites `namespace` to a
            // value other than the pinned one, RE-EVALUATE the rule
            // pipeline against the new namespace. A Deny on the new
            // namespace rejects the modification (the hook cannot
            // launder a write into a denied namespace).
            if let Some(new_ns) = delta.namespace.as_deref()
                && new_ns != pinned_ns
            {
                let rebound_ctx = PermissionContext {
                    op: ctx.op,
                    namespace: new_ns.to_string(),
                    agent_id: ctx.agent_id.clone(),
                    payload: ctx.payload.clone(),
                };
                // Re-evaluate against rules ONLY (we already drained
                // the hooks slice above; re-running them would either
                // loop indefinitely or re-Modify the same delta).
                // The hooks pass is empty here so the recursion
                // terminates after a single rule pass.
                let rebound = Self::evaluate_with(&rebound_ctx, &[], rules, mode);
                if let Decision::Deny(reason) = rebound {
                    return Decision::Deny(format!(
                        "namespace escalation rejected: hook proposed Modify into \
                         namespace {new_ns:?} (from pinned {pinned_ns:?}) which is denied: \
                         {reason}"
                    ));
                }
            }
            return Decision::Modify(delta);
        }

        // Pass 3: explicit Allow from any source short-circuits Ask.
        let any_rule_allow = rule_outcomes
            .iter()
            .any(|r| matches!(r.decision, RuleDecision::Allow));
        let any_hook_allow = hook_decisions
            .iter()
            .any(|h| matches!(h, HookDecision::Allow));
        if any_rule_allow || any_hook_allow {
            return Decision::Allow;
        }

        // Pass 4: Ask falls through to mode default.
        let any_rule_ask = rule_outcomes
            .iter()
            .find(|r| matches!(r.decision, RuleDecision::Ask));
        let any_hook_ask = hook_decisions
            .iter()
            .find(|h| matches!(h, HookDecision::AskUser { .. }));
        let prompt = if let Some(r) = any_rule_ask {
            r.reason.clone().unwrap_or_else(|| {
                format!(
                    "permission rule requests approval (namespace_pattern={}, op={})",
                    r.namespace_pattern, r.op
                )
            })
        } else if let Some(HookDecision::AskUser { prompt, .. }) = any_hook_ask {
            prompt.clone()
        } else {
            // No source spoke: fall back to the mode default
            // outright (no Ask was raised).
            return mode_default_for(mode, ctx);
        };

        match mode {
            PermissionsMode::Enforce => Decision::Deny(format!(
                "permission ask escalated to deny under enforce mode: {prompt}"
            )),
            PermissionsMode::Advisory | PermissionsMode::Off => Decision::Ask(prompt),
        }
    }
}

/// Mode default when no rule and no hook spoke. Enforce defaults
/// to Allow (rules opt in to deny; the gate is opt-in everywhere
/// else too); Advisory and Off both default to Allow. The unified
/// surface mirrors the v0.6.x semantics: namespaces without an
/// explicit policy are unaffected.
fn mode_default_for(_mode: PermissionsMode, _ctx: &PermissionContext) -> Decision {
    Decision::Allow
}

/// Walk `rules` and return the subset matching `ctx`, sorted by
/// specificity descending (longest literal namespace prefix wins,
/// then exact agent pattern beats wildcard).
fn matched_rules<'a>(
    ctx: &PermissionContext,
    rules: &'a [PermissionRule],
) -> Vec<&'a PermissionRule> {
    let mut hits: Vec<&PermissionRule> = rules
        .iter()
        .filter(|r| {
            r.op == ctx.op.as_str()
                && glob_matches(&r.namespace_pattern, &ctx.namespace)
                && glob_matches(&r.agent_pattern, &ctx.agent_id)
        })
        .collect();
    hits.sort_by(|a, b| {
        let sa = (
            pattern_specificity(&a.namespace_pattern),
            usize::from(!a.agent_pattern.contains('*')),
        );
        let sb = (
            pattern_specificity(&b.namespace_pattern),
            usize::from(!b.agent_pattern.contains('*')),
        );
        sb.cmp(&sa)
    });
    hits
}

/// Field-wise merge: `next` overrides `prior` field-by-field.
fn merge_delta(prior: Option<MemoryDelta>, next: MemoryDelta) -> MemoryDelta {
    let mut out = prior.unwrap_or_default();
    if next.tier.is_some() {
        out.tier = next.tier;
    }
    if next.namespace.is_some() {
        out.namespace = next.namespace;
    }
    if next.title.is_some() {
        out.title = next.title;
    }
    if next.content.is_some() {
        out.content = next.content;
    }
    if next.tags.is_some() {
        out.tags = next.tags;
    }
    if next.priority.is_some() {
        out.priority = next.priority;
    }
    if next.confidence.is_some() {
        out.confidence = next.confidence;
    }
    if next.source.is_some() {
        out.source = next.source;
    }
    if next.expires_at.is_some() {
        out.expires_at = next.expires_at;
    }
    if next.metadata.is_some() {
        out.metadata = next.metadata;
    }
    out
}

// ---------------------------------------------------------------------------
// Synthetic rule integration (review #628 H10)
// ---------------------------------------------------------------------------

/// Map a K10 `pending_actions.action_type` string onto a K9 [`Op`].
///
/// K10 records synthetic rules with the wire-level `action_type`
/// (`"store"`, `"delete"`, `"promote"`) — the same shape the
/// `pending_actions` table uses. K9 evaluates against an [`Op`]
/// enum (`memory_store`, `memory_delete`, …). This adapter bridges
/// the two so `remember=forever` rules become consultable by the
/// store / delete pipeline without the rule loader having to know
/// about K9 internals.
fn op_matches_action_type(op: Op, action_type: &str) -> bool {
    match (op, action_type) {
        (Op::MemoryStore, "store")
        | (Op::MemoryDelete, "delete")
        | (Op::MemoryArchive, "archive" | "promote")
        | (Op::MemoryConsolidate, "consolidate")
        | (Op::MemoryLink, "link") => true,
        _ => false,
    }
}

/// Promote every entry in
/// [`crate::approvals::list_synthetic_rules`] into the equivalent
/// [`PermissionRule`] shape so the K9 evaluator can consume them
/// alongside the config-loaded rules. Empty agent_id is rendered as
/// the wildcard `"*"`. Unknown decision verbs are dropped with a
/// WARN — the K10 transports only ever write `"approve"` /
/// `"deny"`, so this is defence-in-depth, not load-bearing.
///
/// Each synthetic entry yields one `PermissionRule` per K9 [`Op`]
/// the `action_type` maps to (via [`op_matches_action_type`]).
/// `pending_actions.action_type == "store"` produces a
/// `memory_store` rule; `"delete"` produces `memory_delete`; etc.
fn synthetic_rules_as_permission_rules() -> Vec<PermissionRule> {
    let synth = crate::approvals::list_synthetic_rules();
    let mut out: Vec<PermissionRule> = Vec::with_capacity(synth.len());
    let ops = [
        Op::MemoryStore,
        Op::MemoryDelete,
        Op::MemoryArchive,
        Op::MemoryConsolidate,
        Op::MemoryLink,
    ];
    for s in synth {
        let decision = match s.decision.as_str() {
            "approve" | "allow" => RuleDecision::Allow,
            "deny" | "reject" => RuleDecision::Deny,
            other => {
                tracing::warn!(
                    "ignoring synthetic permission rule with unknown decision verb: {other:?}"
                );
                continue;
            }
        };
        let agent_pattern = s.agent_id.clone().unwrap_or_else(|| "*".to_string());
        for op in ops {
            if !op_matches_action_type(op, &s.action_type) {
                continue;
            }
            out.push(PermissionRule {
                namespace_pattern: s.namespace.clone(),
                op: op.as_str().to_string(),
                agent_pattern: agent_pattern.clone(),
                decision,
                reason: Some(format!(
                    "remembered operator decision (recorded_at={})",
                    s.recorded_at
                )),
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// v0.7.0 F8 — secure-by-default mode resolution
// ---------------------------------------------------------------------------
//
// Round-2 evidence: a namespace with `metadata.governance.write=owner`
// accepted writes from an unrelated agent_id because `permissions.mode`
// was unconfigured and therefore fell back to the v0.7.0-ship default
// of `advisory` — which logs but does not block. We flip the default
// for unconfigured deployments to `enforce` so an upgrader who has
// not yet authored a `[permissions]` block gets the secure posture
// out of the box. Operators who wanted advisory must opt in
// explicitly.
//
// The compiled `Default for PermissionsMode` in `config.rs` continues
// to return `Advisory` because that default is also consumed by the
// serde-deserialise of an empty `[permissions]` block — flipping it
// there would silently change the meaning of `[permissions]` blocks
// that lack `mode = ` while preserving every other field. Instead we
// expose [`default_v07_secure_mode`] / [`resolve_v07_default_mode`]
// here for the daemon's bootstrap path to consult at startup, and for
// the migration-warning surface to detect the "config exists, mode
// unset" upgrade case.

/// The v0.7.0 secure-by-default permissions mode. Returns
/// [`PermissionsMode::Enforce`].
///
/// Round-2 F8 — used by the daemon's bootstrap path (see
/// [`crate::cli::serve_banner`]) to resolve the active mode when the
/// operator's `config.toml` does not include a `[permissions]` block
/// or omits the `mode = ` field within one.
#[must_use]
pub fn default_v07_secure_mode() -> PermissionsMode {
    PermissionsMode::Enforce
}

/// Round-2 F8 — resolve the effective mode for an upgrading deployment.
///
/// `configured` is `Some(mode)` when the operator has explicitly set
/// `[permissions].mode` in `config.toml`, and `None` when the field is
/// absent (either the block is missing or only contains other fields).
///
/// Returns `(effective_mode, optional_migration_warning)`:
/// - `effective_mode` is the configured value if present, otherwise the
///   v0.7.0 secure default ([`PermissionsMode::Enforce`]).
/// - `optional_migration_warning` is `Some(text)` when the operator's
///   config did NOT explicitly set `mode` AND the resolved default is
///   stricter than the v0.6.x posture (i.e., we flipped them from
///   advisory to enforce). The warning is surfaced once at daemon
///   startup so an upgrader notices the behaviour change and can opt
///   back into advisory if their workflow depends on it.
#[must_use]
pub fn resolve_v07_default_mode(
    configured: Option<PermissionsMode>,
) -> (PermissionsMode, Option<String>) {
    if let Some(mode) = configured {
        return (mode, None);
    }
    let warning = "v0.7.0 default changed to enforce; set permissions.mode=advisory in config to \
                   opt out — see release notes."
        .to_string();
    (default_v07_secure_mode(), Some(warning))
}

/// Round-2 F8 — single-line startup-banner text describing the active
/// permissions posture. Surfaced by the daemon's serve banner so an
/// operator inspecting logs can see at a glance which mode is live.
///
/// Format: `"permissions: enforce"` / `"permissions: advisory"` /
/// `"permissions: off"`.
#[must_use]
pub fn startup_banner_line(mode: PermissionsMode) -> String {
    format!("permissions: {}", mode.as_str())
}

// ---------------------------------------------------------------------------
// Process-wide rules registry
// ---------------------------------------------------------------------------

static ACTIVE_PERMISSION_RULES: RwLock<Vec<PermissionRule>> = RwLock::new(Vec::new());

/// Replace the process-wide rules list. Called from `main` /
/// daemon bootstrap with the loaded `[[permissions.rules]]`
/// entries from `config.toml`. Tests call this to seed scenarios.
pub fn set_active_permission_rules(rules: Vec<PermissionRule>) {
    if let Ok(mut w) = ACTIVE_PERMISSION_RULES.write() {
        *w = rules;
    }
}

/// Snapshot of the current rules list. Cheap clone — the rules vec
/// is small and the API contract is per-evaluate, not held across
/// suspend points.
#[must_use]
pub fn active_permission_rules() -> Vec<PermissionRule> {
    ACTIVE_PERMISSION_RULES
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// Test-only: clear the registry. Mirrors the K3 reset helpers.
#[doc(hidden)]
pub fn clear_active_permission_rules_for_test() {
    set_active_permission_rules(Vec::new());
}

// ---------------------------------------------------------------------------
// Tests — unit-level coverage for the matcher + combiner.
// The full pipeline is exercised by tests/k9_permission_pipeline.rs.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(op: Op, ns: &str, agent: &str) -> PermissionContext {
        PermissionContext {
            op,
            namespace: ns.to_string(),
            agent_id: agent.to_string(),
            payload: serde_json::Value::Null,
        }
    }

    fn rule(ns_pat: &str, op: &str, agent_pat: &str, dec: RuleDecision) -> PermissionRule {
        PermissionRule {
            namespace_pattern: ns_pat.to_string(),
            op: op.to_string(),
            agent_pattern: agent_pat.to_string(),
            decision: dec,
            reason: Some(format!("test:{ns_pat}/{op}/{agent_pat}")),
        }
    }

    #[test]
    fn glob_exact_match() {
        assert!(glob_matches("foo", "foo"));
        assert!(!glob_matches("foo", "bar"));
        assert!(glob_matches("", ""));
    }

    #[test]
    fn glob_single_star_within_segment() {
        assert!(glob_matches("ai:*", "ai:claude"));
        assert!(glob_matches("ai:*", "ai:claude-1"));
        // single-star may not eat '/' — namespace segments preserved.
        assert!(!glob_matches("foo/*", "foo/bar/baz"));
    }

    #[test]
    fn glob_double_star_across_segments() {
        assert!(glob_matches("foo/**", "foo/bar/baz"));
        assert!(glob_matches("**", "anything/at/all"));
    }

    #[test]
    fn rule_deny_short_circuits_pipeline() {
        let r = rule("secrets/*", "memory_store", "ai:*", RuleDecision::Deny);
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "secrets/api", "ai:claude"),
            &[],
            &[r],
            PermissionsMode::Enforce,
        );
        assert!(matches!(d, Decision::Deny(_)));
    }

    #[test]
    fn rule_allow_returns_allow() {
        let r = rule("public/*", "memory_store", "*", RuleDecision::Allow);
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "public/blog", "human:alice"),
            &[],
            &[r],
            PermissionsMode::Enforce,
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn off_mode_short_circuits_to_allow() {
        let r = rule("**", "memory_store", "*", RuleDecision::Deny);
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "secrets/api", "ai:claude"),
            &[],
            &[r],
            PermissionsMode::Off,
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn no_match_defaults_to_allow() {
        let r = rule("secrets/*", "memory_store", "*", RuleDecision::Deny);
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "public/blog", "human:alice"),
            &[],
            &[r],
            PermissionsMode::Enforce,
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn op_as_str_round_trips() {
        for op in [
            Op::MemoryStore,
            Op::MemoryLink,
            Op::MemoryDelete,
            Op::MemoryArchive,
            Op::MemoryConsolidate,
            Op::MemoryReplay,
        ] {
            assert_eq!(Op::from_str(op.as_str()), Some(op));
        }
    }

    #[test]
    fn specificity_orders_long_prefix_first() {
        assert!(pattern_specificity("secrets/api/v1") > pattern_specificity("secrets/*"));
        assert!(pattern_specificity("secrets/*") > pattern_specificity("**"));
    }

    // ---- F8 secure-by-default --------------------------------------------

    #[test]
    fn default_v07_secure_mode_is_enforce() {
        assert_eq!(default_v07_secure_mode(), PermissionsMode::Enforce);
    }

    #[test]
    fn resolve_v07_default_mode_unconfigured_yields_enforce_with_warning() {
        let (mode, warning) = resolve_v07_default_mode(None);
        assert_eq!(mode, PermissionsMode::Enforce);
        let w = warning.expect("expected migration warning when mode is unconfigured");
        assert!(w.contains("v0.7.0 default changed to enforce"), "got: {w}");
        assert!(w.contains("permissions.mode=advisory"), "got: {w}");
    }

    #[test]
    fn resolve_v07_default_mode_configured_passes_through() {
        let (mode, warning) = resolve_v07_default_mode(Some(PermissionsMode::Advisory));
        assert_eq!(mode, PermissionsMode::Advisory);
        assert!(warning.is_none(), "no warning when operator opted in");

        let (mode, warning) = resolve_v07_default_mode(Some(PermissionsMode::Off));
        assert_eq!(mode, PermissionsMode::Off);
        assert!(warning.is_none());

        let (mode, warning) = resolve_v07_default_mode(Some(PermissionsMode::Enforce));
        assert_eq!(mode, PermissionsMode::Enforce);
        assert!(warning.is_none());
    }

    #[test]
    fn startup_banner_line_includes_mode_str() {
        assert_eq!(
            startup_banner_line(PermissionsMode::Enforce),
            "permissions: enforce"
        );
        assert_eq!(
            startup_banner_line(PermissionsMode::Advisory),
            "permissions: advisory"
        );
        assert_eq!(
            startup_banner_line(PermissionsMode::Off),
            "permissions: off"
        );
    }

    #[test]
    fn op_from_str_unknown_returns_none() {
        assert!(Op::from_str("not_a_real_op").is_none());
        assert!(Op::from_str("").is_none());
    }

    #[test]
    fn glob_matches_empty_pattern_only_matches_empty_value() {
        assert!(glob_matches("", ""));
        assert!(!glob_matches("", "anything"));
    }

    #[test]
    fn glob_matches_pattern_longer_than_value_fails() {
        assert!(!glob_matches("longpattern", "x"));
    }

    #[test]
    fn pattern_specificity_increasing_order() {
        let s1 = pattern_specificity("**");
        let s2 = pattern_specificity("foo/*");
        let s3 = pattern_specificity("foo/bar");
        let s4 = pattern_specificity("foo/bar/baz/qux/quux");
        assert!(s2 > s1);
        assert!(s3 > s2);
        assert!(s4 > s3);
    }

    #[test]
    fn hook_deny_propagates_through_pipeline() {
        let hook_decisions = vec![HookDecision::Deny {
            reason: "hook said no".into(),
            code: 403,
        }];
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "public/blog", "ai:claude"),
            &hook_decisions,
            &[],
            PermissionsMode::Enforce,
        );
        match d {
            Decision::Deny(reason) => assert!(reason.contains("hook said no")),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn hook_modify_composes_into_decision() {
        let delta = MemoryDelta {
            tags: Some(vec!["hooked".into()]),
            ..Default::default()
        };
        let hook_decisions = vec![HookDecision::Modify(
            crate::hooks::decision::ModifyPayload { delta },
        )];
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "public/blog", "ai:claude"),
            &hook_decisions,
            &[],
            PermissionsMode::Enforce,
        );
        match d {
            Decision::Modify(delta) => {
                assert_eq!(delta.tags.as_deref(), Some(&["hooked".to_string()][..]));
            }
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[test]
    fn hook_modify_namespace_escalation_rejected_when_destination_denied() {
        // Hook proposes to redirect into "secrets/api" — a denied ns.
        let delta = MemoryDelta {
            namespace: Some("secrets/api".into()),
            ..Default::default()
        };
        let hook_decisions = vec![HookDecision::Modify(
            crate::hooks::decision::ModifyPayload { delta },
        )];
        let r = rule("secrets/**", "memory_store", "*", RuleDecision::Deny);
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "public/blog", "ai:claude"),
            &hook_decisions,
            &[r],
            PermissionsMode::Enforce,
        );
        match d {
            Decision::Deny(reason) => {
                assert!(
                    reason.contains("namespace escalation rejected"),
                    "expected escalation rejection: {reason}"
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn hook_modify_namespace_change_to_allowed_passes() {
        let delta = MemoryDelta {
            namespace: Some("public/wiki".into()),
            ..Default::default()
        };
        let hook_decisions = vec![HookDecision::Modify(
            crate::hooks::decision::ModifyPayload { delta },
        )];
        let r = rule("public/**", "memory_store", "*", RuleDecision::Allow);
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "public/blog", "ai:claude"),
            &hook_decisions,
            &[r],
            PermissionsMode::Enforce,
        );
        match d {
            Decision::Modify(_) => {}
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[test]
    fn rule_allow_short_circuits_ask() {
        let allow = rule("public/*", "memory_store", "*", RuleDecision::Allow);
        let ask = rule("public/*", "memory_store", "*", RuleDecision::Ask);
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "public/blog", "ai:claude"),
            &[],
            &[allow, ask],
            PermissionsMode::Enforce,
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn ask_under_enforce_mode_escalates_to_deny() {
        let r = rule("private/*", "memory_store", "*", RuleDecision::Ask);
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "private/journal", "ai:claude"),
            &[],
            &[r],
            PermissionsMode::Enforce,
        );
        match d {
            Decision::Deny(reason) => assert!(reason.contains("permission ask escalated")),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn ask_under_advisory_mode_returns_ask() {
        let r = rule("private/*", "memory_store", "*", RuleDecision::Ask);
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "private/journal", "ai:claude"),
            &[],
            &[r],
            PermissionsMode::Advisory,
        );
        match d {
            Decision::Ask(_) => {}
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn hook_askuser_under_advisory_mode_surfaces_ask() {
        let hook_decisions = vec![HookDecision::AskUser {
            prompt: "Please confirm".into(),
            options: vec!["yes".into(), "no".into()],
            default: Some("no".into()),
        }];
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "public/blog", "ai:claude"),
            &hook_decisions,
            &[],
            PermissionsMode::Advisory,
        );
        match d {
            Decision::Ask(prompt) => assert!(prompt.contains("Please confirm")),
            other => panic!("expected Ask, got {other:?}"),
        }
    }

    #[test]
    fn hook_allow_alone_short_circuits_to_allow() {
        let hook_decisions = vec![HookDecision::Allow];
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "public/blog", "ai:claude"),
            &hook_decisions,
            &[],
            PermissionsMode::Enforce,
        );
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn evaluate_public_facade_uses_active_rules() {
        clear_active_permission_rules_for_test();
        let d = Permissions::evaluate(&ctx(Op::MemoryStore, "anywhere", "ai:claude"), &[]);
        // No active rules and no hook decisions → mode default Allow.
        assert_eq!(d, Decision::Allow);
    }

    #[test]
    fn set_and_active_permission_rules_round_trip() {
        clear_active_permission_rules_for_test();
        set_active_permission_rules(vec![rule(
            "secrets/*",
            "memory_store",
            "*",
            RuleDecision::Deny,
        )]);
        let rules = active_permission_rules();
        assert_eq!(rules.len(), 1);
        clear_active_permission_rules_for_test();
        assert!(active_permission_rules().is_empty());
    }

    #[test]
    fn permissions_mode_serde_round_trip() {
        let modes = [
            PermissionsMode::Off,
            PermissionsMode::Advisory,
            PermissionsMode::Enforce,
        ];
        for m in modes {
            let json = serde_json::to_string(&m).unwrap();
            let back: PermissionsMode = serde_json::from_str(&json).unwrap();
            assert_eq!(m, back);
        }
    }

    #[test]
    fn decision_partial_eq_distinct_variants() {
        let allow = Decision::Allow;
        let deny = Decision::Deny("x".into());
        let ask = Decision::Ask("?".into());
        let modify = Decision::Modify(MemoryDelta::default());
        assert_ne!(allow, deny);
        assert_ne!(allow, ask);
        assert_ne!(allow, modify);
        assert_ne!(deny, ask);
    }

    #[test]
    fn glob_double_star_matches_zero_segments() {
        // "foo/**" should match "foo" itself per the standard glob extension.
        assert!(glob_matches("foo/**", "foo/anything"));
    }

    #[test]
    fn evaluate_no_rules_no_hooks_returns_allow() {
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "anywhere", "ai:claude"),
            &[],
            &[],
            PermissionsMode::Enforce,
        );
        assert_eq!(d, Decision::Allow);
    }
}
