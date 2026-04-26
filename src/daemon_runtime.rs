// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Test-friendly entry points for the daemon bodies.
//!
//! `main.rs` houses `serve()`, `cmd_sync_daemon()`, and `cmd_curator()`. Each
//! is hardcoded to install a `tokio::signal::ctrl_c()` watcher, which makes
//! the daemons impossible to drive from an in-process integration test (the
//! test would need to deliver a real signal, which on POSIX requires
//! subprocess isolation, which in turn loses `cargo-llvm-cov` attribution).
//!
//! This module mirrors the daemon-body logic with a `tokio::sync::Notify`
//! shutdown trigger that the test harness can fire programmatically. The
//! production binary continues to run the `main.rs` paths verbatim — these
//! helpers exist purely so `tests/integration.rs::test_daemon_cmd_*` can
//! exercise the same library-level code (`build_router`, `db::sync_*`,
//! `curator::run_daemon`) in-process and have the coverage attributed.
//!
//! Keep these signatures minimal: integration tests should construct the
//! same state the production daemon does (an `AppState`, a peer URL, a
//! `CuratorConfig`) and pass it in. No CLI parsing here — that stays in
//! `main.rs`.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::Notify;

use crate::handlers::{ApiKeyState, AppState};

/// Run the HTTP daemon (plain HTTP, no TLS) with a programmable shutdown.
///
/// Mirrors the `else` branch of `serve()` in `main.rs` (the non-TLS path,
/// covering lines 1326-1338 of v0.6.3). Builds the production `Router`
/// via `build_router`, binds a `TcpListener` to `addr`, and runs
/// `axum::serve` with a graceful-shutdown future that resolves when
/// `shutdown.notify_one()` is called.
///
/// Tests pass an OS-assigned port (e.g. `127.0.0.1:0` is not supported here
/// because we need a known port for the health probe — pick one via
/// `free_port()` and pass `127.0.0.1:<port>`). The function returns when
/// shutdown completes; callers can `tokio::spawn` it and `notify` to stop.
pub async fn serve_http_with_shutdown(
    addr: &str,
    api_key_state: ApiKeyState,
    app_state: AppState,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let app = crate::build_router(api_key_state, app_state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown.notified().await;
        })
        .await
        .context("axum::serve")?;
    Ok(())
}

/// Run a single sync cycle against one peer — pull then push.
///
/// Lifted verbatim (modulo path-of-Path-vs-PathBuf) from
/// `main.rs::sync_cycle_once` so the integration sync-daemon test can drive
/// it without subprocess. The signature matches the private main.rs helper
/// 1:1 to keep call sites identical.
pub async fn sync_cycle_once(
    client: &reqwest::Client,
    db_path: &Path,
    local_agent_id: &str,
    peer_url: &str,
    api_key: Option<&str>,
    batch_size: usize,
) -> Result<()> {
    let peer_url = peer_url.trim_end_matches('/');

    // --- PULL --------------------------------------------------------
    let since = {
        let conn = crate::db::open(db_path)?;
        crate::db::sync_state_load(&conn, local_agent_id)?
            .entries
            .get(peer_url)
            .cloned()
    };

    let mut pull_url = format!(
        "{peer_url}/api/v1/sync/since?limit={batch_size}&peer={}",
        urlencoding_minimal(local_agent_id)
    );
    if let Some(ref s) = since {
        pull_url.push_str("&since=");
        pull_url.push_str(&urlencoding_minimal(s));
    }

    let mut req = client.get(&pull_url).header("x-agent-id", local_agent_id);
    if let Some(key) = api_key {
        req = req.header("x-api-key", key);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("sync-daemon: pull status {}", resp.status());
    }
    let pulled: SyncSinceResponse = resp.json().await?;
    let pull_count = pulled.memories.len();
    let latest_pulled = pulled.memories.last().map(|m| m.updated_at.clone());

    {
        let conn = crate::db::open(db_path)?;
        for mem in &pulled.memories {
            if crate::validate::validate_memory(mem).is_ok() {
                let _ = crate::db::insert_if_newer(&conn, mem);
            }
        }
        if let Some(ref at) = latest_pulled {
            crate::db::sync_state_observe(&conn, local_agent_id, peer_url, at)?;
        }
    }

    // --- PUSH --------------------------------------------------------
    let last_pushed = {
        let conn = crate::db::open(db_path)?;
        crate::db::sync_state_last_pushed(&conn, local_agent_id, peer_url)
    };
    let outgoing = {
        let conn = crate::db::open(db_path)?;
        crate::db::memories_updated_since(&conn, last_pushed.as_deref(), batch_size)?
    };
    let push_count = outgoing.len();
    let latest_pushed = outgoing.last().map(|m| m.updated_at.clone());

    if !outgoing.is_empty() {
        let body = serde_json::json!({
            "sender_agent_id": local_agent_id,
            "sender_clock": { "entries": {} },
            "memories": outgoing,
            "dry_run": false,
        });
        let mut req = client
            .post(format!("{peer_url}/api/v1/sync/push"))
            .header("x-agent-id", local_agent_id)
            .header("content-type", "application/json")
            .json(&body);
        if let Some(key) = api_key {
            req = req.header("x-api-key", key);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("sync-daemon: push status {}", resp.status());
        }
        if let Some(at) = latest_pushed {
            let conn = crate::db::open(db_path)?;
            crate::db::sync_state_record_push(&conn, local_agent_id, peer_url, &at)?;
        }
    }

    tracing::info!("sync-daemon: peer={peer_url} pulled={pull_count} pushed={push_count}");
    Ok(())
}

/// Run the sync-daemon main loop with a programmable shutdown.
///
/// Mirrors the body of `cmd_sync_daemon()` in `main.rs` (lines 3329-3374
/// of v0.6.3): for each cycle, fan out a `JoinSet` across `peers`, then
/// race a sleep against the shutdown notify. Returns when the notify
/// fires. The integration test can build a one-cycle test by setting
/// `interval_secs=1` and notifying after a short tokio sleep.
pub async fn run_sync_daemon_with_shutdown(
    db_path: PathBuf,
    local_agent_id: String,
    peers: Vec<String>,
    api_key: Option<String>,
    interval_secs: u64,
    batch_size: usize,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;
    run_sync_daemon_with_shutdown_using_client(
        client,
        db_path,
        local_agent_id,
        peers,
        api_key,
        interval_secs,
        batch_size,
        shutdown,
    )
    .await
}

/// Variant of [`run_sync_daemon_with_shutdown`] that takes a caller-built
/// `reqwest::Client`. The production `cmd_sync_daemon()` constructs an
/// mTLS-aware client (via `build_rustls_client_config`) and threads it
/// in here so the helper drives the same loop body the test version
/// drives — keeping `daemon_runtime` as the single source of truth for
/// the sync-daemon loop while preserving the production TLS contract.
pub async fn run_sync_daemon_with_shutdown_using_client(
    client: reqwest::Client,
    db_path: PathBuf,
    local_agent_id: String,
    peers: Vec<String>,
    api_key: Option<String>,
    interval_secs: u64,
    batch_size: usize,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let interval = interval_secs.max(1);
    let batch_size = batch_size.max(1);

    let db_path_owned: Arc<Path> = Arc::from(db_path.as_path());
    let local_agent_id_arc: Arc<str> = Arc::from(local_agent_id.as_str());
    let api_key_arc: Option<Arc<str>> = api_key.as_deref().map(Arc::from);
    let peers_arc: Vec<Arc<str>> = peers.iter().map(|s| Arc::from(s.as_str())).collect();
    loop {
        let mut set: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
        for peer_url in &peers_arc {
            let client = client.clone();
            let db_path = db_path_owned.clone();
            let local_agent_id = local_agent_id_arc.clone();
            let peer_url = peer_url.clone();
            let api_key = api_key_arc.clone();
            set.spawn(async move {
                if let Err(e) = sync_cycle_once(
                    &client,
                    &db_path,
                    &local_agent_id,
                    &peer_url,
                    api_key.as_deref(),
                    batch_size,
                )
                .await
                {
                    tracing::warn!("sync-daemon: peer {peer_url} cycle failed: {e}");
                }
            });
        }
        while set.join_next().await.is_some() {}

        tokio::select! {
            () = tokio::time::sleep(Duration::from_secs(interval)) => {}
            () = shutdown.notified() => {
                tracing::info!("sync-daemon: shutdown signal received");
                return Ok(());
            }
        }
    }
}

/// Run the curator daemon with a programmable shutdown.
///
/// Mirrors the daemon arm of `cmd_curator()` in `main.rs` (lines 4317-4334
/// of v0.6.3): the inner work is `curator::run_daemon` (a blocking,
/// tight-loop-with-AtomicBool already in lib code), which we drive from a
/// `spawn_blocking`. Tests fire the `Notify` to set the shutdown bool and
/// the blocking task observes it within ~500ms (`run_daemon`'s sleep tick).
pub async fn run_curator_daemon_with_shutdown(
    db_path: PathBuf,
    cfg: crate::curator::CuratorConfig,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_for_signal = shutdown_flag.clone();
    tokio::spawn(async move {
        shutdown.notified().await;
        shutdown_flag_for_signal.store(true, Ordering::Relaxed);
    });

    let llm_arc: Option<Arc<crate::llm::OllamaClient>> = None;
    let db_owned = db_path;
    tokio::task::spawn_blocking(move || {
        crate::curator::run_daemon(db_owned, llm_arc, cfg, shutdown_flag);
    })
    .await
    .map_err(|e| anyhow::anyhow!("curator daemon join: {e}"))?;
    Ok(())
}

/// Curator-daemon loop body, primitive-arg flavour for the binary.
///
/// The production `cmd_curator()` builds its config + (optional) local
/// LLM via `mod curator` / `mod llm` declared inside `main.rs`. Those
/// duplicate-compile to *different* nominal types from `ai_memory::*`,
/// so threading the bin's `CuratorConfig` straight into the lib helper
/// fails at the type level. This variant accepts the four config
/// primitives plus an `Ollama_url`/`Ollama_model` pair (so the helper
/// can construct the lib-side LLM inside the lib crate). Behaviour is
/// 1:1 with `cmd_curator()`'s prior inline body.
///
/// `ollama_model` of `None` disables the LLM (matching the pre-tiered
/// keyword-only path in `build_curator_llm`).
#[allow(clippy::too_many_arguments)]
pub async fn run_curator_daemon_with_primitives(
    db_path: PathBuf,
    interval_secs: u64,
    max_ops_per_cycle: usize,
    dry_run: bool,
    include_namespaces: Vec<String>,
    exclude_namespaces: Vec<String>,
    ollama_model: Option<String>,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let cfg = crate::curator::CuratorConfig {
        interval_secs,
        max_ops_per_cycle,
        dry_run,
        include_namespaces,
        exclude_namespaces,
    };
    let llm: Option<Arc<crate::llm::OllamaClient>> =
        ollama_model.and_then(|m| crate::llm::OllamaClient::new(&m).ok().map(Arc::new));

    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let shutdown_flag_for_signal = shutdown_flag.clone();
    tokio::spawn(async move {
        shutdown.notified().await;
        shutdown_flag_for_signal.store(true, Ordering::Relaxed);
    });

    tokio::task::spawn_blocking(move || {
        crate::curator::run_daemon(db_path, llm, cfg, shutdown_flag);
    })
    .await
    .map_err(|e| anyhow::anyhow!("curator daemon join: {e}"))?;
    Ok(())
}

// -----------------------------------------------------------------------
// helpers
// -----------------------------------------------------------------------

/// Minimal URL-component encoder — only the characters the sync-daemon
/// queries actually emit (RFC3339 timestamps with `:` and `+`, and
/// agent ids with `:`/`@`/`/`). Mirror of `main.rs::urlencoding_minimal`.
fn urlencoding_minimal(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

/// Mirrors `main.rs::SyncSinceResponse` — the fields we deserialize from
/// the peer's `/api/v1/sync/since` body. `count` and `limit` are present
/// in the wire payload but unused on the receive side; allowed to be
/// dead so `clippy::pedantic` doesn't trip.
#[derive(serde::Deserialize)]
struct SyncSinceResponse {
    #[allow(dead_code)]
    count: usize,
    #[allow(dead_code)]
    limit: usize,
    memories: Vec<crate::models::Memory>,
}

/// Re-export the `Instant`/`Duration` types so test crate use sites stay
/// terse.  Kept private — internal to this module.
#[allow(dead_code)]
fn _imports_in_use(_: Instant, _: Duration) {}
