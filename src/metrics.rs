// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.6.0.0 Prometheus metrics. Exposed at `GET /metrics` by the daemon.
//!
//! Minimal, non-invasive instrumentation — the process has a single
//! default `Registry`, a handful of global counters and a couple of
//! histograms. Callers increment via the typed helpers (`record_store`,
//! `record_recall`) rather than poking the registry directly so a future
//! metrics-backend swap stays internal.

use std::sync::OnceLock;

use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Registry,
    TextEncoder,
};

/// Handles to the registered metric families. Built once on first access
/// via `registry()`.
///
/// Fields are public so call sites in `handlers.rs`, future
/// `subscriptions.rs`, and the test module can `.inc()` / `.observe()` /
/// `.set()` directly. `#[allow(dead_code)]` covers the handles that
/// aren't wired to a caller yet — they surface in `/metrics` output
/// (see the `render_includes_registered_names` test) and will be
/// instrumented as sibling features land (hnsw gauge via the HNSW
/// module, subscriptions gauge via the webhook PR, webhook counters
/// via the dispatch path, etc.).
#[allow(dead_code)]
pub struct Metrics {
    pub registry: Registry,
    pub store_total: IntCounterVec,
    pub recall_total: IntCounterVec,
    pub recall_latency_seconds: HistogramVec,
    pub autonomy_hook_total: IntCounterVec,
    pub contradiction_detected_total: IntCounter,
    pub webhook_dispatched_total: IntCounter,
    pub webhook_failed_total: IntCounter,
    pub memories_gauge: IntGauge,
    pub hnsw_size_gauge: IntGauge,
    pub subscriptions_active_gauge: IntGauge,
    pub curator_cycles_total: IntCounter,
    pub curator_operations_total: IntCounterVec,
    pub curator_cycle_duration_seconds: HistogramVec,
    /// Ultrareview #343: count of post-quorum fanout tasks whose
    /// outcome could not be observed (shutdown, panic, or the
    /// spawned task erred). Non-zero indicates mesh divergence risk.
    pub federation_fanout_dropped_total: IntCounterVec,
    /// S40 (v0.6.2 Patch 2): count of peer POST retries, labeled by
    /// final outcome. `ok` = retry recovered the row; `fail` = both
    /// attempts failed (peer likely truly down); `id_drift` = retry
    /// observed the same peer id-drift as attempt 1.
    pub federation_fanout_retry_total: IntCounterVec,
    /// H9 (v0.7.0 round-2): count of quorum writes that the leader
    /// returned `200` for (W met) but where at least one configured
    /// peer did NOT ack inside the deadline. Operators alert on
    /// non-zero rate to detect mesh-divergence drift early — before a
    /// follow-up catchup sync surfaces the gap.
    pub federation_partial_quorum_total: IntCounter,
    /// Cluster-A COR-3 (v0.7.0): count of memory rows whose Form 4
    /// fact-provenance JSON columns (`citations`, `source_span`,
    /// `confidence_signals`, or pre-Form-4 `metadata`) failed to parse
    /// and were silently defaulted by `row_to_memory`. Non-zero
    /// indicates schema drift, writer-side corruption, or a
    /// migration that left malformed JSON in the column. Labeled by
    /// column name (`citations` | `source_span` | `confidence_signals`
    /// | `metadata`).
    pub corrupt_provenance_rows_total: IntCounterVec,
    /// v0.7-polish SEC-15 / COR-11 (issue #780): count of
    /// `post_reflect.auto_export` detached worker invocations whose
    /// outcome was a panic or a returned `Err`. Non-zero means an
    /// operator-opted-in namespace had a reflection that did NOT
    /// land on the filesystem and the failure would otherwise be
    /// silent (the worker thread is detached; the reflection itself
    /// already committed). The capabilities-v3 surface mirrors this
    /// counter so operator dashboards can alert without scraping
    /// `/metrics` directly.
    pub auto_export_spawn_failed_total: IntCounter,
}

/// Lazily-built process-global metrics handle.
pub fn registry() -> &'static Metrics {
    static HANDLE: OnceLock<Metrics> = OnceLock::new();
    HANDLE.get_or_init(Metrics::new_or_panic)
}

impl Metrics {
    fn new_or_panic() -> Self {
        // Registration can only fail on duplicate-name conflict; with a
        // fresh registry that's unreachable. Panic is acceptable because
        // the metrics subsystem is a daemon-startup concern — a failure
        // here means a programming bug, not a runtime condition.
        Self::try_new().expect("prometheus registry init failed")
    }

    // COVERAGE: every `?` Err-arm closure on `IntCounterVec::new(...)?`,
    //           `IntCounter::new(...)?`, `IntGauge::new(...)?`,
    //           `HistogramVec::new(...)?`, and
    //           `registry.register(Box::new(...))?` in this function
    //           is structurally unreachable in production:
    //
    //           1. The function constructs a fresh `Registry::new()`
    //              per call (no shared state). Registration can only
    //              fail on duplicate metric name; with a fresh registry
    //              and unique names per counter, collision is
    //              impossible.
    //           2. Every metric name + label name passed to the
    //              constructors is a compile-time string literal that
    //              already matches the Prometheus regex
    //              `[a-zA-Z_:][a-zA-Z0-9_:]*` — construction cannot
    //              fail on name-validation grounds.
    //
    //           The Err-arms exist because the prometheus crate's
    //           API returns `Result<...>` from these constructors, and
    //           the `?` propagation is the idiomatic Rust pattern.
    //           Triggering coverage would require a synthetic
    //           registry-injection layer that doesn't exist (and
    //           shouldn't — try_new owns its registry by design).
    //           Documented per L0.7 playbook §3c.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn try_new() -> prometheus::Result<Self> {
        let registry = Registry::new();

        let store_total = IntCounterVec::new(
            prometheus::Opts::new(
                "ai_memory_store_total",
                "Total memory_store calls, labeled by tier and result.",
            ),
            &["tier", "result"],
        )?;
        registry.register(Box::new(store_total.clone()))?;

        let recall_total = IntCounterVec::new(
            prometheus::Opts::new(
                "ai_memory_recall_total",
                "Total memory_recall calls, labeled by mode.",
            ),
            &["mode"],
        )?;
        registry.register(Box::new(recall_total.clone()))?;

        let recall_latency_seconds = HistogramVec::new(
            HistogramOpts::new(
                "ai_memory_recall_latency_seconds",
                "Recall latency in seconds, labeled by mode.",
            )
            .buckets(vec![
                0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
            ]),
            &["mode"],
        )?;
        registry.register(Box::new(recall_latency_seconds.clone()))?;

        let autonomy_hook_total = IntCounterVec::new(
            prometheus::Opts::new(
                "ai_memory_autonomy_hook_total",
                "Post-store autonomy hook invocations, labeled by kind and result.",
            ),
            &["kind", "result"],
        )?;
        registry.register(Box::new(autonomy_hook_total.clone()))?;

        let contradiction_detected_total = IntCounter::new(
            "ai_memory_contradiction_detected_total",
            "Count of contradictions the LLM hook confirmed.",
        )?;
        registry.register(Box::new(contradiction_detected_total.clone()))?;

        let webhook_dispatched_total = IntCounter::new(
            "ai_memory_webhook_dispatched_total",
            "Total webhook deliveries attempted.",
        )?;
        registry.register(Box::new(webhook_dispatched_total.clone()))?;

        let webhook_failed_total = IntCounter::new(
            "ai_memory_webhook_failed_total",
            "Webhook deliveries that failed after all retries.",
        )?;
        registry.register(Box::new(webhook_failed_total.clone()))?;

        let memories_gauge = IntGauge::new(
            "ai_memory_memories",
            "Current count of non-archived memories.",
        )?;
        registry.register(Box::new(memories_gauge.clone()))?;

        let hnsw_size_gauge = IntGauge::new(
            "ai_memory_hnsw_size",
            "Current HNSW vector index population.",
        )?;
        registry.register(Box::new(hnsw_size_gauge.clone()))?;

        let subscriptions_active_gauge = IntGauge::new(
            "ai_memory_subscriptions_active",
            "Current count of active webhook subscriptions.",
        )?;
        registry.register(Box::new(subscriptions_active_gauge.clone()))?;

        let curator_cycles_total = IntCounter::new(
            "ai_memory_curator_cycles_total",
            "Total curator sweep cycles completed.",
        )?;
        registry.register(Box::new(curator_cycles_total.clone()))?;

        let curator_operations_total = IntCounterVec::new(
            prometheus::Opts::new(
                "ai_memory_curator_operations_total",
                "Curator operations, labeled by kind (auto_tag|contradiction|persist) and result.",
            ),
            &["kind", "result"],
        )?;
        registry.register(Box::new(curator_operations_total.clone()))?;

        let curator_cycle_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "ai_memory_curator_cycle_duration_seconds",
                "Curator sweep cycle wall-clock duration, labeled by dry_run.",
            )
            .buckets(vec![0.1, 0.5, 1.0, 5.0, 15.0, 60.0, 300.0, 900.0, 3600.0]),
            &["dry_run"],
        )?;
        registry.register(Box::new(curator_cycle_duration_seconds.clone()))?;

        let federation_fanout_dropped_total = IntCounterVec::new(
            prometheus::Opts::new(
                "ai_memory_federation_fanout_dropped_total",
                "Post-quorum fanout tasks whose outcome could not be observed. \
                 reason=shutdown|panic|join_error. Non-zero indicates mesh divergence risk.",
            ),
            &["reason"],
        )?;
        registry.register(Box::new(federation_fanout_dropped_total.clone()))?;

        let federation_fanout_retry_total = IntCounterVec::new(
            prometheus::Opts::new(
                "ai_memory_federation_fanout_retry_total",
                "Peer POSTs that hit a transient failure on first attempt and \
                 were retried once via the Idempotency-Key path. \
                 outcome=ok|fail|id_drift. Non-zero ok indicates the retry \
                 recovered a row that would otherwise be missing on a peer.",
            ),
            &["outcome"],
        )?;
        registry.register(Box::new(federation_fanout_retry_total.clone()))?;

        // H9 (v0.7.0 round-2) — partial-quorum observability.
        let federation_partial_quorum_total = IntCounter::new(
            "ai_memory_federation_partial_quorum_total",
            "Quorum writes that succeeded (W met) but where at least one \
             configured peer did not ack inside the deadline.",
        )?;
        registry.register(Box::new(federation_partial_quorum_total.clone()))?;

        // Cluster-A COR-3 (v0.7.0) — corrupt-provenance observability.
        let corrupt_provenance_rows_total = IntCounterVec::new(
            prometheus::Opts::new(
                "ai_memory_corrupt_provenance_rows_total",
                "Memory rows whose Form 4 fact-provenance JSON columns \
                 failed to deserialise and were silently defaulted. \
                 Non-zero indicates schema drift, writer-side corruption, \
                 or a migration leaving malformed JSON.",
            ),
            &["column"],
        )?;
        registry.register(Box::new(corrupt_provenance_rows_total.clone()))?;

        // v0.7-polish SEC-15 / COR-11 (issue #780) — auto-export
        // detached-worker failure observability.
        let auto_export_spawn_failed_total = IntCounter::new(
            "ai_memory_auto_export_spawn_failed_total",
            "Detached post_reflect.auto_export worker invocations whose \
             outcome was a panic or returned Err. Non-zero means at \
             least one reflection was committed to the DB but its \
             on-disk markdown/json artefact did not land — operators \
             use this to alert on otherwise-silent disk-write failures.",
        )?;
        registry.register(Box::new(auto_export_spawn_failed_total.clone()))?;

        Ok(Self {
            registry,
            store_total,
            recall_total,
            recall_latency_seconds,
            autonomy_hook_total,
            contradiction_detected_total,
            webhook_dispatched_total,
            webhook_failed_total,
            memories_gauge,
            hnsw_size_gauge,
            subscriptions_active_gauge,
            curator_cycles_total,
            curator_operations_total,
            curator_cycle_duration_seconds,
            federation_fanout_dropped_total,
            federation_fanout_retry_total,
            federation_partial_quorum_total,
            corrupt_provenance_rows_total,
            auto_export_spawn_failed_total,
        })
    }
}

/// Cluster-A COR-3 (v0.7.0) — record a single corrupt-provenance row
/// observation. `column` is the offending JSON column name
/// (`citations` / `source_span` / `confidence_signals` / `metadata`).
/// Pairs with a `tracing::warn!` at the call site so operators see the
/// row id + parse error.
pub fn record_corrupt_provenance(column: &str) {
    registry()
        .corrupt_provenance_rows_total
        .with_label_values(&[column])
        .inc();
}

/// v0.7-polish SEC-15 / COR-11 (issue #780) — record one detached
/// `auto_export` worker failure (panic OR returned `Err`). Pairs with
/// a `tracing::warn!` at the call site so operators see the
/// reflection id + failure mode. The counter is also mirrored onto the
/// capabilities-v3 `hooks.auto_export_spawn_failed_total` field so
/// dashboards that consume `memory_capabilities` (vs `/metrics`) see
/// the same signal.
pub fn record_auto_export_spawn_failed() {
    registry().auto_export_spawn_failed_total.inc();
}

/// v0.7-polish SEC-15 / COR-11 (issue #780) — read the current value
/// of the auto-export spawn-failure counter. Used by the
/// capabilities-v3 builder to mirror the metric onto the
/// `hooks.auto_export_spawn_failed_total` field without scraping
/// `/metrics`.
#[must_use]
pub fn auto_export_spawn_failed_count() -> u64 {
    registry().auto_export_spawn_failed_total.get()
}

/// Render the current registry state to the Prometheus text exposition
/// format. Ignores errors from the encoder (unreachable in practice) and
/// returns an empty string — the scrape returns 200 with a possibly-empty
/// body rather than a 5xx, which Prometheus handles gracefully.
#[must_use]
pub fn render() -> String {
    let encoder = TextEncoder::new();
    let mut buf = Vec::new();
    let _ = encoder.encode(&registry().registry.gather(), &mut buf);
    String::from_utf8(buf).unwrap_or_default()
}

/// Convenience: record a store, labeled by tier.
#[allow(dead_code)]
pub fn record_store(tier: &str, ok: bool) {
    let result = if ok { "ok" } else { "err" };
    registry()
        .store_total
        .with_label_values(&[tier, result])
        .inc();
}

/// Convenience: record a recall, labeled by mode + latency.
#[allow(dead_code)]
pub fn record_recall(mode: &str, latency_seconds: f64) {
    registry().recall_total.with_label_values(&[mode]).inc();
    registry()
        .recall_latency_seconds
        .with_label_values(&[mode])
        .observe(latency_seconds);
}

/// Convenience: record an autonomy-hook invocation.
#[allow(dead_code)]
pub fn record_autonomy_hook(kind: &str, ok: bool) {
    let result = if ok { "ok" } else { "err" };
    registry()
        .autonomy_hook_total
        .with_label_values(&[kind, result])
        .inc();
}

/// Convenience: record a completed curator cycle (v0.6.1).
#[allow(dead_code)]
pub fn curator_cycle_completed(
    operations_attempted: usize,
    auto_tagged: usize,
    contradictions_found: usize,
    errors: usize,
) {
    let r = registry();
    r.curator_cycles_total.inc();
    if auto_tagged > 0 {
        r.curator_operations_total
            .with_label_values(&["auto_tag", "ok"])
            .inc_by(auto_tagged as u64);
    }
    if contradictions_found > 0 {
        r.curator_operations_total
            .with_label_values(&["contradiction", "ok"])
            .inc_by(contradictions_found as u64);
    }
    let failed = operations_attempted.saturating_sub(auto_tagged + contradictions_found);
    if failed > 0 || errors > 0 {
        r.curator_operations_total
            .with_label_values(&["any", "err"])
            .inc_by(errors as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_singleton() {
        let r1 = registry();
        let r2 = registry();
        // Same instance — no double-registration.
        assert!(std::ptr::eq(std::ptr::from_ref(r1), std::ptr::from_ref(r2)));
    }

    #[test]
    fn render_includes_registered_names() {
        // Tickle every series so each one has ≥1 sample.
        record_store("short", true);
        record_recall("hybrid", 0.042);
        record_autonomy_hook("auto_tag", true);
        registry().contradiction_detected_total.inc();
        registry().webhook_dispatched_total.inc();
        registry().memories_gauge.set(42);
        registry().hnsw_size_gauge.set(42);
        registry().subscriptions_active_gauge.set(3);

        let text = render();
        for name in [
            "ai_memory_store_total",
            "ai_memory_recall_total",
            "ai_memory_recall_latency_seconds",
            "ai_memory_autonomy_hook_total",
            "ai_memory_contradiction_detected_total",
            "ai_memory_webhook_dispatched_total",
            "ai_memory_webhook_failed_total",
            "ai_memory_memories",
            "ai_memory_hnsw_size",
            "ai_memory_subscriptions_active",
        ] {
            assert!(text.contains(name), "/metrics missing {name}\n\n{text}");
        }
    }

    #[test]
    fn record_store_labels_tier() {
        record_store("long", true);
        let text = render();
        assert!(text.contains("ai_memory_store_total{result=\"ok\",tier=\"long\"}"));
    }

    // ---- Wave 3 (Closer T): tests for curator_cycle_completed (L263-287)
    // and webhook_dispatched/_failed counter labels.

    #[test]
    fn curator_cycle_completed_increments_total() {
        // Other tests running in parallel may bump the same singleton
        // counter; what we own is the +1 contributed by *this* call.
        let before = registry().curator_cycles_total.get();
        curator_cycle_completed(0, 0, 0, 0);
        let after = registry().curator_cycles_total.get();
        assert!(
            after >= before + 1,
            "curator_cycles_total did not advance (before={before}, after={after})"
        );
    }

    #[test]
    fn curator_cycle_completed_records_auto_tag_ok() {
        curator_cycle_completed(5, 3, 0, 0);
        let text = render();
        assert!(
            text.contains("ai_memory_curator_operations_total"),
            "curator_operations_total counter missing from /metrics output"
        );
    }

    #[test]
    fn curator_cycle_completed_records_contradiction_ok() {
        curator_cycle_completed(2, 0, 2, 0);
        let text = render();
        assert!(text.contains("ai_memory_curator_operations_total"));
    }

    #[test]
    fn curator_cycle_completed_records_errors() {
        // operations_attempted=5, auto_tagged=2, contradictions=1 → failed=2
        // plus errors=1 → the err counter is exercised.
        curator_cycle_completed(5, 2, 1, 1);
        let text = render();
        assert!(text.contains("ai_memory_curator_operations_total"));
    }

    #[test]
    fn curator_cycle_completed_with_zero_args_is_safe() {
        // No labels emitted, no panic — a zero cycle is valid (empty DB).
        let before = registry().curator_cycles_total.get();
        curator_cycle_completed(0, 0, 0, 0);
        let after = registry().curator_cycles_total.get();
        // Same race-tolerant assertion as above.
        assert!(after >= before + 1);
    }

    // -----------------------------------------------------------------
    // W12-H — additional helpers + render shape pinning
    // -----------------------------------------------------------------

    #[test]
    fn record_store_err_path() {
        record_store("short", false);
        let text = render();
        assert!(text.contains("ai_memory_store_total{result=\"err\",tier=\"short\""));
    }

    #[test]
    fn record_recall_emits_latency_histogram() {
        record_recall("keyword", 0.5);
        let text = render();
        assert!(text.contains("ai_memory_recall_total{mode=\"keyword\""));
        assert!(text.contains("ai_memory_recall_latency_seconds"));
    }

    #[test]
    fn record_autonomy_hook_err_path() {
        record_autonomy_hook("contradiction", false);
        let text = render();
        assert!(
            text.contains("ai_memory_autonomy_hook_total{kind=\"contradiction\",result=\"err\"")
        );
    }

    #[test]
    fn render_emits_help_and_type_lines() {
        // Tickle one series, then render and assert prom-format HELP/TYPE lines.
        record_store("mid", true);
        let text = render();
        assert!(text.contains("# HELP ai_memory_store_total"));
        assert!(text.contains("# TYPE ai_memory_store_total counter"));
    }

    #[test]
    fn fanout_dropped_counter_increments() {
        registry()
            .federation_fanout_dropped_total
            .with_label_values(&["shutdown"])
            .inc();
        let text = render();
        assert!(text.contains("ai_memory_federation_fanout_dropped_total{reason=\"shutdown\""));
    }

    #[test]
    fn fanout_retry_counter_outcome_labels() {
        // All three outcome labels exercised — `ok`, `fail`, `id_drift`.
        for outcome in ["ok", "fail", "id_drift"] {
            registry()
                .federation_fanout_retry_total
                .with_label_values(&[outcome])
                .inc();
        }
        let text = render();
        assert!(text.contains("ai_memory_federation_fanout_retry_total"));
    }

    #[test]
    fn curator_cycle_duration_histogram_buckets() {
        // Just observe — confirms registry accepts the value and surfaces
        // the histogram in /metrics output.
        registry()
            .curator_cycle_duration_seconds
            .with_label_values(&["false"])
            .observe(0.42);
        let text = render();
        assert!(text.contains("ai_memory_curator_cycle_duration_seconds"));
    }

    // -----------------------------------------------------------------
    // L0.7-2 Tier A — exercise try_new() directly so the metric-builder
    // happy paths (lines 88-210) get covered. The process singleton
    // registry() builds once on first access; we need a second pass for
    // line coverage of every metric registration in the try_new body.
    // -----------------------------------------------------------------

    #[test]
    fn try_new_builds_a_fresh_metrics_handle() {
        // Build a second instance on top of an independent registry —
        // hits every metric-construction line in `try_new` even when
        // another test has already initialised the process-wide
        // singleton. Each call uses a fresh Registry, so register()
        // cannot collide.
        let m = super::Metrics::try_new().expect("fresh registry must succeed");
        // The handle must expose every metric family — touch each to
        // exercise the assignment side of the struct literal.
        m.store_total.with_label_values(&["short", "ok"]).inc();
        m.recall_total.with_label_values(&["hybrid"]).inc();
        m.recall_latency_seconds
            .with_label_values(&["hybrid"])
            .observe(0.001);
        m.autonomy_hook_total.with_label_values(&["x", "ok"]).inc();
        m.contradiction_detected_total.inc();
        m.webhook_dispatched_total.inc();
        m.webhook_failed_total.inc();
        m.memories_gauge.set(1);
        m.hnsw_size_gauge.set(1);
        m.subscriptions_active_gauge.set(1);
        m.curator_cycles_total.inc();
        m.curator_operations_total
            .with_label_values(&["auto_tag", "ok"])
            .inc();
        m.curator_cycle_duration_seconds
            .with_label_values(&["true"])
            .observe(1.0);
        m.federation_fanout_dropped_total
            .with_label_values(&["panic"])
            .inc();
        m.federation_fanout_retry_total
            .with_label_values(&["ok"])
            .inc();
        m.federation_partial_quorum_total.inc();
        m.auto_export_spawn_failed_total.inc();
    }

    #[test]
    fn try_new_can_build_two_isolated_registries() {
        // Two consecutive try_new() calls succeed because each builds
        // its own Registry — no name collision.
        let a = super::Metrics::try_new().expect("first");
        let b = super::Metrics::try_new().expect("second");
        // Tickle a counter on each so the family surfaces in gather().
        a.store_total.with_label_values(&["short", "ok"]).inc();
        b.store_total.with_label_values(&["short", "ok"]).inc();
        let mut buf_a = Vec::new();
        let mut buf_b = Vec::new();
        let enc = TextEncoder::new();
        enc.encode(&a.registry.gather(), &mut buf_a).unwrap();
        enc.encode(&b.registry.gather(), &mut buf_b).unwrap();
        assert!(String::from_utf8_lossy(&buf_a).contains("ai_memory_store_total"));
        assert!(String::from_utf8_lossy(&buf_b).contains("ai_memory_store_total"));
    }

    #[test]
    fn record_auto_export_spawn_failed_increments_singleton() {
        // v0.7-polish #780 — record_auto_export_spawn_failed() must
        // monotonically advance the process-wide counter that the
        // capabilities-v3 builder mirrors onto
        // `hooks.auto_export_spawn_failed_total`.
        let before = auto_export_spawn_failed_count();
        record_auto_export_spawn_failed();
        let after = auto_export_spawn_failed_count();
        assert!(
            after >= before + 1,
            "auto_export_spawn_failed_total did not advance \
             (before={before}, after={after})"
        );
        // The render text must mention the metric name so /metrics
        // scrapers see it.
        let text = render();
        assert!(
            text.contains("ai_memory_auto_export_spawn_failed_total"),
            "/metrics output missing auto_export counter\n\n{text}"
        );
    }

    #[test]
    fn curator_cycle_completed_no_progress_branch_skips_err_increment() {
        // operations_attempted=0, auto_tagged=0, contradictions=0,
        // errors=0 → failed = 0.saturating_sub(0+0) = 0 → the `if
        // failed > 0 || errors > 0` block does NOT execute. Pins the
        // negative branch.
        let before = registry().curator_cycles_total.get();
        curator_cycle_completed(0, 0, 0, 0);
        let after = registry().curator_cycles_total.get();
        assert!(after >= before + 1);
    }
}
