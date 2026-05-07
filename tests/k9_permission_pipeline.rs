// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7.0 K9 — unified permission system pipeline tests.
//
// Validates the composition rule documented in `src/permissions.rs`:
//
//   1. First Deny across any source wins.
//   2. Otherwise: Modify (hook-only) wins next.
//   3. Otherwise: explicit Allow short-circuits Ask.
//   4. Otherwise: Ask falls through to mode default —
//      Enforce promotes Ask to Deny; Advisory/Off promote to Allow.
//
// Each scenario builds a `PermissionContext`, an explicit rule
// list, and (where relevant) an explicit `&[HookDecision]`, then
// asserts the resulting `Decision`. The `evaluate_with` overload
// is used so the tests don't poke the process-wide registry, and
// can therefore run in parallel without a serialization mutex.

use ai_memory::config::PermissionsMode;
use ai_memory::hooks::decision::{HookDecision, ModifyPayload};
use ai_memory::hooks::events::MemoryDelta;
use ai_memory::permissions::{
    Decision, Op, PermissionContext, PermissionRule, Permissions, RuleDecision,
};

fn ctx(op: Op, ns: &str, agent: &str) -> PermissionContext {
    PermissionContext {
        op,
        namespace: ns.to_string(),
        agent_id: agent.to_string(),
        payload: serde_json::json!({}),
    }
}

fn rule(
    ns: &str,
    op: &str,
    agent: &str,
    decision: RuleDecision,
    reason: Option<&str>,
) -> PermissionRule {
    PermissionRule {
        namespace_pattern: ns.to_string(),
        op: op.to_string(),
        agent_pattern: agent.to_string(),
        decision,
        reason: reason.map(str::to_string),
    }
}

/// Case 1: a rule with `decision = "deny"` matching the namespace,
/// op, and agent short-circuits the pipeline before mode or
/// hooks resolve. The reason surfaces verbatim.
#[test]
fn k9_rule_deny_short_circuits() {
    let rules = vec![rule(
        "secrets/*",
        "memory_store",
        "ai:*",
        RuleDecision::Deny,
        Some("ai agents may not write to secrets"),
    )];
    let d = Permissions::evaluate_with(
        &ctx(Op::MemoryStore, "secrets/api", "ai:claude"),
        &[],
        &rules,
        PermissionsMode::Enforce,
    );
    match d {
        Decision::Deny(reason) => {
            assert!(
                reason.contains("ai agents may not write to secrets"),
                "deny reason should surface verbatim, got: {reason}"
            );
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

/// Case 2: a rule with `decision = "allow"` matching context
/// returns Allow even when no hook spoke and mode is Enforce
/// (allow short-circuits the Ask fall-through).
#[test]
fn k9_rule_allow_returns_allow() {
    let rules = vec![rule(
        "public/*",
        "memory_store",
        "*",
        RuleDecision::Allow,
        None,
    )];
    let d = Permissions::evaluate_with(
        &ctx(Op::MemoryStore, "public/blog", "human:alice"),
        &[],
        &rules,
        PermissionsMode::Enforce,
    );
    assert_eq!(d, Decision::Allow);
}

/// Case 3: a hook returning Deny overrides a rule returning Allow.
/// First-deny-wins across all sources is the load-bearing safety
/// invariant — if any source says no, the answer is no.
#[test]
fn k9_hook_deny_overrides_rule_allow() {
    let rules = vec![rule(
        "public/*",
        "memory_store",
        "*",
        RuleDecision::Allow,
        None,
    )];
    let hook = HookDecision::Deny {
        reason: "PII scan failed".into(),
        code: 451,
    };
    let d = Permissions::evaluate_with(
        &ctx(Op::MemoryStore, "public/blog", "human:alice"),
        &[hook],
        &rules,
        PermissionsMode::Enforce,
    );
    match d {
        Decision::Deny(reason) => {
            assert_eq!(reason, "PII scan failed", "hook deny reason must surface");
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

/// Case 4: with no rule and no hook speaking, the pipeline falls
/// through to the mode default. Enforce defaults to Allow when
/// nothing matched (rules are opt-in: namespaces without an
/// explicit rule are unaffected). Advisory and Off behave the
/// same. This mirrors the v0.6.x posture for upgraders.
#[test]
fn k9_mode_default_fallback_when_no_source_speaks() {
    for mode in [
        PermissionsMode::Enforce,
        PermissionsMode::Advisory,
        PermissionsMode::Off,
    ] {
        let d = Permissions::evaluate_with(
            &ctx(Op::MemoryStore, "global", "human:alice"),
            &[],
            &[],
            mode,
        );
        assert_eq!(d, Decision::Allow, "mode {mode:?} default must be Allow");
    }
}

/// Case 5: a hook returning Modify with a delta is surfaced as
/// `Decision::Modify(delta)` with the delta intact. The composer
/// applies the delta downstream of the evaluator.
#[test]
fn k9_hook_modify_returns_modify_with_delta() {
    let delta = MemoryDelta {
        tags: Some(vec!["pii-redacted".into()]),
        priority: Some(7),
        ..Default::default()
    };
    let hook = HookDecision::Modify(ModifyPayload {
        delta: delta.clone(),
    });
    let d = Permissions::evaluate_with(
        &ctx(Op::MemoryStore, "public/blog", "human:alice"),
        &[hook],
        &[],
        PermissionsMode::Enforce,
    );
    match d {
        Decision::Modify(applied) => {
            assert_eq!(applied.tags, delta.tags);
            assert_eq!(applied.priority, delta.priority);
        }
        other => panic!("expected Modify, got {other:?}"),
    }
}

/// Case 6: a rule returning Ask under Enforce mode promotes to
/// Deny so strict-mode operators don't accidentally proceed past
/// an unanswered prompt. The reason is preserved for the audit
/// trail.
#[test]
fn k9_ask_escalates_to_deny_under_enforce() {
    let rules = vec![rule(
        "sensitive/*",
        "memory_delete",
        "*",
        RuleDecision::Ask,
        Some("delete from sensitive/* requires approval"),
    )];
    let d = Permissions::evaluate_with(
        &ctx(Op::MemoryDelete, "sensitive/audit", "ai:bot"),
        &[],
        &rules,
        PermissionsMode::Enforce,
    );
    match d {
        Decision::Deny(reason) => {
            assert!(
                reason.contains("ask escalated to deny under enforce mode"),
                "enforce-mode escalation must be visible in reason, got: {reason}"
            );
            assert!(
                reason.contains("delete from sensitive/* requires approval"),
                "original prompt text must be preserved in escalated deny, got: {reason}"
            );
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

/// Case 7: a rule returning Ask under Advisory mode surfaces as
/// `Decision::Ask(prompt)` so the K10 approval pipeline (or the
/// CLI's interactive prompt path) can prompt the operator. Advisory
/// is the default mode for v0.7.0 upgraders.
#[test]
fn k9_ask_surfaces_under_advisory_mode() {
    let rules = vec![rule(
        "sensitive/*",
        "memory_consolidate",
        "*",
        RuleDecision::Ask,
        Some("consolidating sensitive memories needs review"),
    )];
    let d = Permissions::evaluate_with(
        &ctx(Op::MemoryConsolidate, "sensitive/notes", "human:alice"),
        &[],
        &rules,
        PermissionsMode::Advisory,
    );
    match d {
        Decision::Ask(prompt) => {
            assert_eq!(prompt, "consolidating sensitive memories needs review");
        }
        other => panic!("expected Ask, got {other:?}"),
    }
}

/// Case 8: longest-pattern-wins specificity. With two matching
/// rules — a permissive `**` Allow and a restrictive
/// `secrets/api/*` Deny — the longer literal prefix takes
/// precedence so the deny wins. Mirrors the documented "rule
/// specificity" tie-break in `src/permissions.rs`.
#[test]
fn k9_longest_pattern_wins_specificity() {
    let rules = vec![
        rule("**", "memory_store", "*", RuleDecision::Allow, None),
        rule(
            "secrets/api/*",
            "memory_store",
            "*",
            RuleDecision::Deny,
            Some("specific deny beats catch-all allow"),
        ),
    ];
    let d = Permissions::evaluate_with(
        &ctx(Op::MemoryStore, "secrets/api/v1", "ai:bot"),
        &[],
        &rules,
        PermissionsMode::Enforce,
    );
    // Deny-first across sources is the actual ordering rule —
    // both Allow and Deny match here, and Deny wins regardless of
    // specificity. The specificity tie-breaker matters when
    // multiple rules of the *same* decision compete (e.g. two
    // Denies with different reasons); the chosen reason should be
    // the more specific one.
    match d {
        Decision::Deny(reason) => {
            assert!(
                reason.contains("specific deny beats catch-all allow"),
                "deny reason must come from the matching rule, got: {reason}"
            );
        }
        other => panic!("expected Deny, got {other:?}"),
    }
}

/// H8 (#628 blocker) — a hook returning `Modify { namespace: <ns> }`
/// where `<ns>` is denied by a rule must NOT be allowed to launder
/// the write into the denied namespace. The pipeline pins the
/// original namespace at entry, lets the rule pass evaluate against
/// it, and then — if a Modify proposes a different namespace —
/// re-evaluates the rules against the new namespace before accepting
/// the modification. A deny on the destination escalates the result
/// from `Modify` to `Deny`.
#[test]
fn k9_modify_cannot_escalate_into_denied_namespace() {
    // Rules: deny stores into `secrets/*`, allow stores into
    // `public/*`. The starting namespace is `public/blog` — the rule
    // pass returns Allow. A hook proposes Modify { namespace:
    // "secrets/x" } and the H8 fix must catch the escalation.
    let rules = vec![
        rule(
            "secrets/*",
            "memory_store",
            "*",
            RuleDecision::Deny,
            Some("ai agents may not write to secrets"),
        ),
        rule("public/*", "memory_store", "*", RuleDecision::Allow, None),
    ];
    let hook = HookDecision::Modify(ModifyPayload {
        delta: MemoryDelta {
            namespace: Some("secrets/x".into()),
            ..Default::default()
        },
    });
    let d = Permissions::evaluate_with(
        &ctx(Op::MemoryStore, "public/blog", "ai:claude"),
        &[hook],
        &rules,
        PermissionsMode::Enforce,
    );
    match d {
        Decision::Deny(reason) => {
            assert!(
                reason.contains("namespace escalation rejected"),
                "expected H8 escalation marker in deny reason, got: {reason}"
            );
            assert!(
                reason.contains("secrets/x"),
                "deny reason should name the rejected destination namespace, got: {reason}"
            );
        }
        other => panic!("expected Deny (escalation rejected), got {other:?}"),
    }
}

/// H8 — a Modify proposing a namespace that is also allowed by the
/// rule pipeline still surfaces as `Decision::Modify(delta)`. The
/// fix only rejects on a Deny re-evaluation; legitimate
/// namespace-rewrite hooks continue to work.
#[test]
fn k9_modify_into_allowed_namespace_still_succeeds() {
    let rules = vec![
        rule("public/*", "memory_store", "*", RuleDecision::Allow, None),
        rule("staging/*", "memory_store", "*", RuleDecision::Allow, None),
    ];
    let hook = HookDecision::Modify(ModifyPayload {
        delta: MemoryDelta {
            namespace: Some("staging/x".into()),
            tags: Some(vec!["rewritten".into()]),
            ..Default::default()
        },
    });
    let d = Permissions::evaluate_with(
        &ctx(Op::MemoryStore, "public/blog", "ai:claude"),
        &[hook],
        &rules,
        PermissionsMode::Enforce,
    );
    match d {
        Decision::Modify(delta) => {
            assert_eq!(delta.namespace.as_deref(), Some("staging/x"));
            assert_eq!(delta.tags.as_deref(), Some(&["rewritten".to_string()][..]));
        }
        other => panic!("expected Modify, got {other:?}"),
    }
}

/// Bonus: Off mode short-circuits the entire pipeline, even past a
/// rule that would otherwise deny. This is the documented freeze-
/// thaw escape hatch (mirrors K3 `Off` semantics — the gate is
/// skipped entirely).
#[test]
fn k9_off_mode_skips_entire_pipeline() {
    let rules = vec![rule(
        "**",
        "memory_archive",
        "*",
        RuleDecision::Deny,
        Some("would-deny"),
    )];
    let d = Permissions::evaluate_with(
        &ctx(Op::MemoryArchive, "secrets/api", "ai:claude"),
        &[],
        &rules,
        PermissionsMode::Off,
    );
    assert_eq!(d, Decision::Allow, "Off mode must short-circuit to Allow");
}
