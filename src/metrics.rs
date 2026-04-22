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

    #[allow(clippy::too_many_lines)]
    fn try_new() -> prometheus::Result<Self> {
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
        })
    }
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
        assert!(std::ptr::eq(r1 as *const _, r2 as *const _));
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
}
