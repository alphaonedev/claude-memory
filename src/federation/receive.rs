// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Post-partition catchup poller: spawn_catchup_loop, catchup_once,
//! urlencoding_encode.

use std::sync::Arc;
use std::time::Duration;

use super::FederationConfig;

/// v0.6.0.1 (#320) — post-partition catchup poller.
///
/// Previously a node rejoining the mesh after SIGSTOP / network blip / restart
/// would only receive NEW writes that arrived AFTER resume; anything the
/// other peers wrote during the outage stayed on those peers. r14 scenario-14
/// observed this as node-3 seeing 2/20 writes post-SIGCONT.
///
/// This loop periodically calls `GET /api/v1/sync/since?peer=<local>` against
/// each configured peer, applying returned memories via `insert_if_newer`.
/// The `since` value is the receiver-side vector clock entry for that peer,
/// so we never re-pull already-applied rows. First catchup after a restart
/// runs with `since=None`, pulling a capped snapshot (limit=500).
///
/// Interval is operator-tunable via `--catchup-interval-secs`. 0 disables.
/// The loop is a best-effort background task: errors are logged but never
/// propagated. In the happy path a partitioned node converges within one
/// interval after resume.
///
/// This is deliberately NOT a substitute for the synchronous quorum-write
/// path — it's a safety net for the tail. Normal writes still fan out via
/// `broadcast_store_quorum`; catchup only fires for rows that DIDN'T land
/// during the original write deadline.
pub fn spawn_catchup_loop(
    config: FederationConfig,
    db: crate::handlers::Db,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    // Pre-existing no-sal build break (caught by the #625 port subagent
    // 2026-05-11): the historical bootstrap path forwarded through
    // `spawn_catchup_loop_with_store`, which is `#[cfg(feature = "sal")]`
    // only. With `sal` off the call site is unresolved. Inline the
    // tokio::spawn loop here so the sqlite-only build compiles. Under
    // `sal` we still route through the store-aware variant so
    // postgres-backed daemons keep the M3 routing fix.
    #[cfg(feature = "sal")]
    {
        spawn_catchup_loop_with_store(config, db, None, interval)
    }
    #[cfg(not(feature = "sal"))]
    {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            loop {
                catchup_once(&config, &db).await;
                tokio::time::sleep(interval).await;
            }
        })
    }
}

/// v0.7.0 M3 — same as [`spawn_catchup_loop`] but accepts an optional
/// SAL-trait store handle. When `store` is `Some`, applied memories are
/// written through `store.apply_remote_memory` (which routes through the
/// active backend — postgres on `--store-url postgres://` deployments,
/// sqlite otherwise). When `None`, the legacy `db::insert_if_newer` path
/// over the shared rusqlite connection is preserved verbatim.
///
/// The split exists so the bootstrap can keep the historical
/// `spawn_catchup_loop` signature (used by tests) intact while
/// postgres-backed daemons get the routing fix.
#[cfg(feature = "sal")]
pub fn spawn_catchup_loop_with_store(
    config: FederationConfig,
    db: crate::handlers::Db,
    store: Option<Arc<dyn crate::store::MemoryStore>>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Small upfront delay so the first catchup doesn't fire before the
        // HTTP server has bound — avoids spurious "connection refused" on
        // node-1 during rolling start of a fresh cluster.
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            catchup_once_with_store(&config, &db, store.as_ref()).await;
            tokio::time::sleep(interval).await;
        }
    })
}

/// Legacy two-arg wrapper preserved so existing tests + non-SAL builds
/// keep dispatching through the sqlite path. Postgres-backed daemons
/// should invoke [`catchup_once_with_store`] directly via
/// [`spawn_catchup_loop_with_store`].
#[cfg_attr(not(test), allow(dead_code))]
pub(super) async fn catchup_once(config: &FederationConfig, db: &crate::handlers::Db) {
    #[cfg(feature = "sal")]
    {
        catchup_once_with_store(config, db, None).await;
    }
    #[cfg(not(feature = "sal"))]
    {
        catchup_once_legacy(config, db).await;
    }
}

#[cfg(feature = "sal")]
pub(super) async fn catchup_once_with_store(
    config: &FederationConfig,
    db: &crate::handlers::Db,
    store: Option<&Arc<dyn crate::store::MemoryStore>>,
) {
    let local_id = config.sender_agent_id.clone();
    for peer in &config.peers {
        // Rebuild the peer's base URL from sync_push_url to get the
        // /api/v1/sync/since endpoint without recomputing peer config.
        let base = peer
            .sync_push_url
            .trim_end_matches("/api/v1/sync/push")
            .to_string();

        // Load our local vector-clock entry for this peer so we only pull
        // the delta. First-time-ever runs with no prior clock pull a full
        // snapshot (capped below by ?limit=500 on the peer side).
        let since_opt: Option<String> = {
            let lock = db.lock().await;
            match crate::db::sync_state_load(&lock.0, &local_id) {
                Ok(clock) => clock.entries.get(&peer.id).cloned(),
                Err(_) => None,
            }
        };

        let url = match since_opt.as_deref() {
            Some(s) => format!(
                "{base}/api/v1/sync/since?since={}&peer={local_id}",
                urlencoding_encode(s)
            ),
            None => format!("{base}/api/v1/sync/since?peer={local_id}"),
        };

        let resp = match config.client.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                tracing::debug!(
                    "catchup: peer {} returned HTTP {} — skipping this tick",
                    peer.id,
                    r.status()
                );
                continue;
            }
            Err(e) => {
                tracing::debug!("catchup: peer {} unreachable: {e}", peer.id);
                continue;
            }
        };

        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("catchup: peer {} returned unparseable body: {e}", peer.id);
                continue;
            }
        };

        let memories = match body.get("memories").and_then(|v| v.as_array()) {
            Some(arr) => arr.clone(),
            None => continue,
        };

        if memories.is_empty() {
            continue;
        }

        let mut applied = 0usize;
        let mut latest_ts: Option<String> = None;

        // v0.7.0 M3 — when a SAL store handle is supplied (postgres-
        // backed daemons) we dispatch each row through
        // `store.apply_remote_memory`, which routes the write to the
        // active backend instead of always landing in the local sqlite
        // file. Default-None preserves the legacy behavior (sqlite via
        // `db::insert_if_newer`) for daemons that don't yet have a SAL
        // handle plumbed through (e.g. v0.6.x configurations).
        if let Some(store) = store {
            let ctx = crate::store::CallerContext::for_agent("federation-catchup");
            for raw in &memories {
                let mem: crate::models::Memory = match serde_json::from_value(raw.clone()) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("catchup: unparseable memory from peer {}: {e}", peer.id);
                        continue;
                    }
                };
                if crate::validate::validate_memory(&mem).is_err() {
                    continue;
                }
                if latest_ts
                    .as_deref()
                    .is_none_or(|cur| mem.updated_at.as_str() > cur)
                {
                    latest_ts = Some(mem.updated_at.clone());
                }
                match store.apply_remote_memory(&ctx, &mem).await {
                    Ok(_) => applied += 1,
                    Err(e) => {
                        tracing::warn!(
                            "catchup: apply_remote_memory failed for peer {}: {e}",
                            peer.id
                        );
                    }
                }
            }
            if let Some(ts) = latest_ts.as_deref() {
                let lock = db.lock().await;
                if let Err(e) = crate::db::sync_state_observe(&lock.0, &local_id, &peer.id, ts) {
                    tracing::warn!("catchup: sync_state_observe failed for {}: {e}", peer.id);
                }
            }
        } else {
            let lock = db.lock().await;
            for raw in &memories {
                let mem: crate::models::Memory = match serde_json::from_value(raw.clone()) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("catchup: unparseable memory from peer {}: {e}", peer.id);
                        continue;
                    }
                };
                if crate::validate::validate_memory(&mem).is_err() {
                    continue;
                }
                if latest_ts
                    .as_deref()
                    .is_none_or(|cur| mem.updated_at.as_str() > cur)
                {
                    latest_ts = Some(mem.updated_at.clone());
                }
                if crate::db::insert_if_newer(&lock.0, &mem).is_ok() {
                    applied += 1;
                }
            }
            if let Some(ts) = latest_ts.as_deref()
                && let Err(e) = crate::db::sync_state_observe(&lock.0, &local_id, &peer.id, ts)
            {
                tracing::warn!("catchup: sync_state_observe failed for {}: {e}", peer.id);
            }
        }

        if applied > 0 {
            tracing::info!(
                "catchup: applied {applied} memories from peer {} (since={})",
                peer.id,
                since_opt.as_deref().unwrap_or("<full-snapshot>"),
            );
        }
    }
}

/// v0.7.0 M3 — non-SAL fallback. Default sqlite-only path is preserved
/// verbatim for builds without `--features sal`. The signature parallels
/// the SAL variant minus the `store` parameter so callers compiled
/// against the legacy posture continue to dispatch through the local
/// rusqlite connection.
#[cfg(not(feature = "sal"))]
async fn catchup_once_legacy(config: &FederationConfig, db: &crate::handlers::Db) {
    let local_id = config.sender_agent_id.clone();
    for peer in &config.peers {
        let base = peer
            .sync_push_url
            .trim_end_matches("/api/v1/sync/push")
            .to_string();

        let since_opt: Option<String> = {
            let lock = db.lock().await;
            match crate::db::sync_state_load(&lock.0, &local_id) {
                Ok(clock) => clock.entries.get(&peer.id).cloned(),
                Err(_) => None,
            }
        };

        let url = match since_opt.as_deref() {
            Some(s) => format!(
                "{base}/api/v1/sync/since?since={}&peer={local_id}",
                urlencoding_encode(s)
            ),
            None => format!("{base}/api/v1/sync/since?peer={local_id}"),
        };

        let resp = match config.client.get(&url).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                tracing::debug!(
                    "catchup: peer {} returned HTTP {} — skipping this tick",
                    peer.id,
                    r.status()
                );
                continue;
            }
            Err(e) => {
                tracing::debug!("catchup: peer {} unreachable: {e}", peer.id);
                continue;
            }
        };

        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("catchup: peer {} returned unparseable body: {e}", peer.id);
                continue;
            }
        };

        let memories = match body.get("memories").and_then(|v| v.as_array()) {
            Some(arr) => arr.clone(),
            None => continue,
        };

        if memories.is_empty() {
            continue;
        }

        let mut applied = 0usize;
        let mut latest_ts: Option<String> = None;
        {
            let lock = db.lock().await;
            for raw in &memories {
                let mem: crate::models::Memory = match serde_json::from_value(raw.clone()) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!("catchup: unparseable memory from peer {}: {e}", peer.id);
                        continue;
                    }
                };
                if crate::validate::validate_memory(&mem).is_err() {
                    continue;
                }
                if latest_ts
                    .as_deref()
                    .is_none_or(|cur| mem.updated_at.as_str() > cur)
                {
                    latest_ts = Some(mem.updated_at.clone());
                }
                if crate::db::insert_if_newer(&lock.0, &mem).is_ok() {
                    applied += 1;
                }
            }
            if let Some(ts) = latest_ts.as_deref()
                && let Err(e) = crate::db::sync_state_observe(&lock.0, &local_id, &peer.id, ts)
            {
                tracing::warn!("catchup: sync_state_observe failed for {}: {e}", peer.id);
            }
        }

        if applied > 0 {
            tracing::info!(
                "catchup: applied {applied} memories from peer {} (since={})",
                peer.id,
                since_opt.as_deref().unwrap_or("<full-snapshot>"),
            );
        }
    }
}

// Minimal RFC 3986 percent-encoder for the `since` timestamp. Only covers
// what RFC 3339 + our namespace/id charsets can produce. We intentionally
// avoid pulling in a url-encoding crate for a 12-character string.
pub(super) fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 6);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}
