// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-3 — daily TTL sweep for `offloaded_blobs`.
//!
//! The sweep removes rows where `stored_at + ttl_seconds < now`,
//! bounded at [`MAX_PER_RUN`] deletions per pass with a [`SLEEP_BETWEEN_DELETES`]
//! gap between deletes so the connection lock window stays short
//! under contended writes (matches the K2 pending-actions sweeper
//! discipline).
//!
//! Spawned by `daemon_runtime::bootstrap_serve` alongside the GC and
//! transcript-lifecycle loops; aborted on shutdown.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::offload::sweep_expired;

/// Cadence between sweeps. Daily — matches the prompt's "daily task"
/// directive. Operators that want a shorter cadence (testing,
/// disaster-recovery exercises) call [`spawn`] with an override.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Maximum number of rows deleted per sweep pass. 1000 keeps the
/// outer loop bounded in pathological backlog scenarios (a thousand
/// rows times the 10 ms sleep = 10 seconds wall clock, well under
/// the daily cadence).
pub const MAX_PER_RUN: usize = 1000;

/// Sleep between consecutive deletes. 10 ms lets concurrent writes
/// land between the SELECT-then-DELETE pairs that make up the sweep
/// body so the connection mutex isn't held for the whole pass.
pub const SLEEP_BETWEEN_DELETES: Duration = Duration::from_millis(10);

/// Spawn the daily sweep loop. Returns a [`JoinHandle`] the caller
/// aborts on shutdown.
///
/// The state lock is held only for the duration of each pass; the
/// in-pass `std::thread::sleep` between deletes happens INSIDE that
/// lock window, which is acceptable because the sweep is a single
/// background thread and not a hot data-plane path. v0.8.0 may
/// move the per-row sleep outside the lock if the offload write
/// volume grows.
#[must_use]
pub fn spawn<T>(state: Arc<Mutex<T>>, interval: Duration) -> JoinHandle<()>
where
    T: SweepAdapter + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let now_unix = chrono::Utc::now().timestamp();
            let lock = state.lock().await;
            match lock.run_sweep(now_unix) {
                Ok(0) => {}
                Ok(n) => tracing::info!(
                    target: "offload.ttl_sweep",
                    "TTL sweep removed {n} expired offloaded blob(s)"
                ),
                Err(e) => tracing::warn!(
                    target: "offload.ttl_sweep",
                    "TTL sweep failed: {e}"
                ),
            }
        }
    })
}

/// Trait wrapping the daemon's `(Connection, ...)` tuple so the
/// sweep is testable without depending on the full daemon state
/// shape. The concrete production `Db` (alias for the daemon's
/// `(Connection, PathBuf, ResolvedTtl, bool)` tuple) implements
/// this via the blanket impl below.
pub trait SweepAdapter {
    fn run_sweep(&self, now_unix: i64) -> anyhow::Result<usize>;
}

impl SweepAdapter
    for (
        rusqlite::Connection,
        std::path::PathBuf,
        crate::config::ResolvedTtl,
        bool,
    )
{
    fn run_sweep(&self, now_unix: i64) -> anyhow::Result<usize> {
        sweep_expired(&self.0, now_unix, MAX_PER_RUN, SLEEP_BETWEEN_DELETES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal test adapter — uses a bare connection so the sweep
    /// surface is exercised end-to-end without standing up the full
    /// daemon `Db` shape.
    struct ConnAdapter(rusqlite::Connection);
    impl SweepAdapter for ConnAdapter {
        fn run_sweep(&self, now_unix: i64) -> anyhow::Result<usize> {
            sweep_expired(&self.0, now_unix, MAX_PER_RUN, Duration::ZERO)
        }
    }

    #[test]
    fn run_sweep_is_idempotent_on_empty_table() {
        let conn = crate::storage::open(std::path::Path::new(":memory:")).unwrap();
        let adapter = ConnAdapter(conn);
        let n = adapter.run_sweep(0).unwrap();
        assert_eq!(n, 0);
        let n2 = adapter.run_sweep(0).unwrap();
        assert_eq!(n2, 0);
    }

    #[test]
    fn run_sweep_removes_expired_row() {
        let conn = crate::storage::open(std::path::Path::new(":memory:")).unwrap();
        let off = crate::offload::ContextOffloader::new(
            &conn,
            None,
            crate::offload::OffloadConfig::default(),
        );
        let r = off.offload("expiring", "ns", Some(1), "ai:alice").unwrap();
        let adapter = ConnAdapter(conn);
        // Sweep at stored_at + 60s to guarantee expiry.
        let n = adapter.run_sweep(r.stored_at + 60).unwrap();
        assert_eq!(n, 1);
    }
}
