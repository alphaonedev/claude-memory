// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7 Track G — Task G10: pre_recall_expand hot-path hook helper.
//
// G10 wires the new [`HookEvent::PreRecallExpand`] (events.rs) into
// the recall hot path. The fire site is `mcp::handle_recall`; the
// helper here is the seam between that call site and G5's
// `HookChain::fire`.
//
// # Why a helper module
//
// `handle_recall` is a long, sync-heavy function with many call
// sites and several test paths. Inlining the hook fire would
// (a) require threading the hook chain + executor registry into
// the function signature (cascading into every caller) and
// (b) duplicate the payload-marshalling logic between this and the
// future G11 / G7+ wiring tasks. Pulling the firing into a single
// function lets the call site stay a one-liner and keeps the
// daemon-mode contract testable in isolation.
//
// # Daemon mode is mandatory in production
//
// `PreRecallExpand` is classified as [`crate::hooks::EventClass::HotPath`]
// (50ms class deadline). A subprocess fork+exec on Linux costs
// ~5-10ms cold and ~1-2ms warm; a single misbehaving exec-mode
// hook would consume the entire budget before the child even
// processes the payload. Operators MUST configure the hook in
// `mode = "daemon"` — the chain's per-hook budget enforcement
// (G6) ensures a misconfigured exec-mode hook still respects the
// 50ms ceiling, but the operator-visible behaviour will be
// "every recall trips the budget".

use serde_json::Value;

use super::chain::{ChainResult, HookChain};
use super::events::{HookEvent, RecallExpandQuery};
use super::executor::ExecutorRegistry;

// ---------------------------------------------------------------------------
// Outcome of running the pre_recall_expand chain
// ---------------------------------------------------------------------------

/// What the helper reports back to `handle_recall`.
///
/// `Allow` and `Modified` both let the recall proceed; the only
/// difference is whether the in-flight `(query, namespace, k)`
/// triple was rewritten by a hook. `Denied` halts the recall —
/// the caller is expected to return an empty result with the
/// `reason` surfaced via the recall response's `meta.diagnostic`
/// block (G5's chain-level Deny semantics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreRecallOutcome {
    /// The chain returned `Allow` (or no hooks were configured).
    /// The recall proceeds with the original triple.
    Allow,
    /// At least one hook returned `Modify`. The recall proceeds
    /// with the rewritten triple — any of `query`, `namespace`,
    /// `k` may have been changed.
    Modified {
        query: String,
        namespace: String,
        k: u32,
    },
    /// A hook returned `Deny`. The recall is short-circuited; the
    /// caller surfaces an empty result with a diagnostic.
    Denied { reason: String, code: i32 },
}

impl PreRecallOutcome {
    /// The resolved query string the recall should run with. For
    /// `Denied` the value is the *original* query — callers should
    /// only use it for logging, not for the actual recall (the
    /// recall must be skipped).
    #[must_use]
    pub fn query(&self, original: &str) -> String {
        match self {
            PreRecallOutcome::Allow | PreRecallOutcome::Denied { .. } => original.to_string(),
            PreRecallOutcome::Modified { query, .. } => query.clone(),
        }
    }

    /// The resolved namespace.
    #[must_use]
    pub fn namespace(&self, original: &str) -> String {
        match self {
            PreRecallOutcome::Allow | PreRecallOutcome::Denied { .. } => original.to_string(),
            PreRecallOutcome::Modified { namespace, .. } => namespace.clone(),
        }
    }

    /// The resolved limit.
    #[must_use]
    pub fn k(&self, original: u32) -> u32 {
        match self {
            PreRecallOutcome::Allow | PreRecallOutcome::Denied { .. } => original,
            PreRecallOutcome::Modified { k, .. } => *k,
        }
    }

    /// Whether the recall must be skipped (i.e. the chain Denied).
    #[must_use]
    pub fn is_denied(&self) -> bool {
        matches!(self, PreRecallOutcome::Denied { .. })
    }
}

// ---------------------------------------------------------------------------
// apply_pre_recall_expand — fire the hot-path chain
// ---------------------------------------------------------------------------

/// Fire the [`HookEvent::PreRecallExpand`] chain on the recall hot
/// path.
///
/// The hot-path budget is enforced by G6's chain runner (the chain's
/// `fire` method stamps a 50ms wall-clock ceiling at entry; the
/// caller does not need to add a second `tokio::time::timeout`
/// around this call).
///
/// ## Modify semantics
///
/// A hook returns `HookDecision::Modify` with a [`super::events::MemoryDelta`]
/// — but `MemoryDelta` was designed for the `pre_store` shape
/// (memory fields). For `pre_recall_expand` we reuse three of its
/// fields with overloaded meaning:
///
///   * `MemoryDelta::content`   → rewritten `query` text
///   * `MemoryDelta::namespace` → rewritten `namespace`
///   * `MemoryDelta::priority`  → rewritten `k` (cast `i32 → u32`,
///     non-positive values fall back to the original `k`)
///
/// This overload is documented here rather than forking
/// `MemoryDelta` into a per-event family because (a) the chain
/// runner's delta merge is generic over `MemoryDelta` and forking
/// would cascade through G5 + G6, and (b) the hot-path payload is
/// narrow enough that a typed per-hook payload was rejected during
/// G2 design discussion. Future G* tasks may revisit if more
/// per-event payload shapes accrue.
///
/// ## Return shape
///
/// See [`PreRecallOutcome`]. `Allow` and `Modified` both let the
/// recall proceed; `Denied` halts it and the caller surfaces the
/// reason in the response's diagnostic block.
pub async fn apply_pre_recall_expand(
    query: &str,
    namespace: &str,
    k: u32,
    chain: &HookChain,
    registry: &mut ExecutorRegistry,
) -> PreRecallOutcome {
    // No hooks configured — fast path. The G6 chain runner would
    // also early-return, but skipping the JSON marshal here keeps
    // the no-hook recall path zero-overhead.
    if chain.hooks().is_empty() {
        return PreRecallOutcome::Allow;
    }

    let payload_struct = RecallExpandQuery {
        query: query.to_string(),
        namespace: namespace.to_string(),
        k,
    };
    let payload = serde_json::to_value(&payload_struct).unwrap_or_else(|_| Value::Null);

    let result = chain
        .fire(HookEvent::PreRecallExpand, payload, registry)
        .await;

    match result {
        ChainResult::Allow => PreRecallOutcome::Allow,
        ChainResult::ModifiedAllow(delta) => {
            let new_query = delta.content.unwrap_or_else(|| query.to_string());
            let new_namespace = delta.namespace.unwrap_or_else(|| namespace.to_string());
            let new_k = match delta.priority {
                Some(p) if p > 0 => u32::try_from(p).unwrap_or(k),
                _ => k,
            };
            PreRecallOutcome::Modified {
                query: new_query,
                namespace: new_namespace,
                k: new_k,
            }
        }
        ChainResult::Deny { reason, code } => PreRecallOutcome::Denied { reason, code },
        ChainResult::AskUser { .. } => {
            // Hot-path hooks can't pause for an operator prompt
            // inside a 50ms budget; surface AskUser as Allow with
            // a tracing warning so the misconfigured hook is
            // visible without breaking the recall.
            tracing::warn!(
                "hooks: pre_recall_expand returned AskUser; degrading to Allow \
                 (operator prompts are incompatible with the recall hot path)"
            );
            PreRecallOutcome::Allow
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_allow_uses_original_triple() {
        let o = PreRecallOutcome::Allow;
        assert_eq!(o.query("orig"), "orig");
        assert_eq!(o.namespace("ns"), "ns");
        assert_eq!(o.k(7), 7);
        assert!(!o.is_denied());
    }

    #[test]
    fn outcome_modified_returns_rewritten_triple() {
        let o = PreRecallOutcome::Modified {
            query: "rewrite".into(),
            namespace: "team/x".into(),
            k: 25,
        };
        assert_eq!(o.query("orig"), "rewrite");
        assert_eq!(o.namespace("ns"), "team/x");
        assert_eq!(o.k(7), 25);
        assert!(!o.is_denied());
    }

    #[test]
    fn outcome_denied_falls_back_to_original_for_logging() {
        let o = PreRecallOutcome::Denied {
            reason: "blocked".into(),
            code: 451,
        };
        // Caller should NOT actually run the recall — but for
        // logging the original triple is what we surface.
        assert_eq!(o.query("orig"), "orig");
        assert_eq!(o.namespace("ns"), "ns");
        assert_eq!(o.k(7), 7);
        assert!(o.is_denied());
    }

    #[tokio::test]
    async fn empty_chain_is_allow_fast_path() {
        let chain = HookChain::new(vec![]);
        let mut reg = ExecutorRegistry::new();
        let out = apply_pre_recall_expand("hello", "default", 10, &chain, &mut reg).await;
        assert_eq!(out, PreRecallOutcome::Allow);
    }
}
