// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_sync` and `cmd_sync_daemon` migrations. The daemon-mode body
//! delegates to `daemon_runtime::run_sync_daemon_with_shutdown_using_client`
//! (W3 work); this module owns only the wrapper + the in-process sync
//! body (pull/push/merge/dry-run).

use crate::cli::CliOutput;
use crate::{db, identity, models, tls, validate};
use anyhow::Result;
use clap::Args;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Args)]
pub struct SyncArgs {
    /// Path to the remote database to sync with
    pub remote_db: PathBuf,
    /// Direction: pull, push, or merge
    #[arg(long, short, default_value = "merge")]
    pub direction: String,
    /// Trust `metadata.agent_id` in remote memories (default: restamp with caller's id).
    /// Only use this when syncing between databases you fully control (e.g., your own backup).
    #[arg(long, default_value_t = false)]
    pub trust_source: bool,
    /// Phase 3 foundation (issue #224): preview what would change without
    /// writing anything. Counts new / updated / unchanged memories and
    /// links in each direction. Uses today's timestamp-aware merge
    /// semantics; CRDT-lite field-level diagnostics land with #224 Task 3a.1.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct SyncDaemonArgs {
    /// Comma-separated list of peer HTTP endpoints to mesh with.
    #[arg(long, value_delimiter = ',')]
    pub peers: Vec<String>,
    /// Seconds between sync cycles. Minimum 1.
    #[arg(long, default_value_t = 2)]
    pub interval: u64,
    /// Optional `X-API-Key` to present to peers that have api-key auth enabled.
    #[arg(long)]
    pub api_key: Option<String>,
    /// Cap on the number of memories transferred per peer per cycle.
    #[arg(long, default_value_t = 500)]
    pub batch_size: usize,
    /// Layer 2 client-cert PEM used when the peer demands mTLS.
    #[arg(long, requires = "client_key")]
    pub client_cert: Option<PathBuf>,
    /// Layer 2 client-key PEM. Must pair with `--client-cert`.
    #[arg(long, requires = "client_cert")]
    pub client_key: Option<PathBuf>,
    /// Disable server-cert verification on outbound HTTPS to peers.
    /// **DANGEROUS** — accepts any server cert without validation.
    #[arg(long, default_value_t = false)]
    pub insecure_skip_server_verify: bool,
}

/// NHI: restamp `metadata.agent_id` to the caller's id, preserving the
/// original as `imported_from_agent_id`. Mirrors `main.rs::restamp_agent_id`
/// (W5 had to extract it because main.rs version is private).
fn restamp_agent_id(mem: &mut models::Memory, caller_id: &str) {
    let original = mem
        .metadata
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    if let Some(obj) = mem.metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String(caller_id.to_string()),
        );
        if let Some(orig) = original
            && orig != caller_id
        {
            obj.insert(
                "imported_from_agent_id".to_string(),
                serde_json::Value::String(orig),
            );
        }
    }
}

#[derive(Default)]
struct SyncPreview {
    would_pull_new: usize,
    would_pull_update: usize,
    would_pull_noop: usize,
    would_push_new: usize,
    would_push_update: usize,
    would_push_noop: usize,
    would_pull_links: usize,
    would_push_links: usize,
}

impl SyncPreview {
    fn classify(local: Option<&models::Memory>, remote: &models::Memory) -> MergeOutcome {
        match local {
            None => MergeOutcome::New,
            Some(existing) => {
                if remote.updated_at > existing.updated_at {
                    MergeOutcome::Update
                } else {
                    MergeOutcome::Noop
                }
            }
        }
    }
}

enum MergeOutcome {
    New,
    Update,
    Noop,
}

/// `sync` handler.
#[allow(clippy::too_many_lines)]
pub fn run(
    db_path: &Path,
    args: &SyncArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let local_conn = db::open(db_path)?;
    let remote_conn = db::open(&args.remote_db)?;
    let caller_id = identity::resolve_agent_id(cli_agent_id, None)?;

    if args.dry_run {
        return cmd_sync_dry_run(&local_conn, &remote_conn, &args.direction, json_out, out);
    }

    match args.direction.as_str() {
        "pull" => {
            let mems = db::export_all(&remote_conn)?;
            let links = db::export_links(&remote_conn)?;
            let mut n = 0;
            for mem in &mems {
                let mut owned = mem.clone();
                if !args.trust_source {
                    restamp_agent_id(&mut owned, &caller_id);
                }
                if let Err(e) = validate::validate_memory(&owned) {
                    tracing::warn!("sync: skipping invalid memory {}: {}", owned.id, e);
                    continue;
                }
                if db::insert(&local_conn, &owned).is_ok() {
                    n += 1;
                }
            }
            for link in &links {
                if validate::validate_link(&link.source_id, &link.target_id, &link.relation)
                    .is_err()
                {
                    continue;
                }
                let _ = db::create_link(
                    &local_conn,
                    &link.source_id,
                    &link.target_id,
                    &link.relation,
                );
            }
            if json_out {
                writeln!(
                    out.stdout,
                    "{}",
                    serde_json::json!({"direction": "pull", "imported": n})
                )?;
            } else {
                writeln!(out.stdout, "pulled {n} memories from remote")?;
            }
        }
        "push" => {
            let mems = db::export_all(&local_conn)?;
            let links = db::export_links(&local_conn)?;
            let mut n = 0;
            for mem in &mems {
                if let Err(e) = validate::validate_memory(mem) {
                    tracing::warn!("sync: skipping invalid memory {}: {}", mem.id, e);
                    continue;
                }
                if db::insert(&remote_conn, mem).is_ok() {
                    n += 1;
                }
            }
            for link in &links {
                if validate::validate_link(&link.source_id, &link.target_id, &link.relation)
                    .is_err()
                {
                    continue;
                }
                let _ = db::create_link(
                    &remote_conn,
                    &link.source_id,
                    &link.target_id,
                    &link.relation,
                );
            }
            if json_out {
                writeln!(
                    out.stdout,
                    "{}",
                    serde_json::json!({"direction": "push", "exported": n})
                )?;
            } else {
                writeln!(out.stdout, "pushed {n} memories to remote")?;
            }
        }
        "merge" => {
            let r_mems = db::export_all(&remote_conn)?;
            let r_links = db::export_links(&remote_conn)?;
            let l_mems = db::export_all(&local_conn)?;
            let l_links = db::export_links(&local_conn)?;
            let (mut pulled, mut pushed) = (0, 0);
            for mem in &r_mems {
                let mut owned = mem.clone();
                if !args.trust_source {
                    restamp_agent_id(&mut owned, &caller_id);
                }
                if validate::validate_memory(&owned).is_err() {
                    continue;
                }
                if db::insert_if_newer(&local_conn, &owned).is_ok() {
                    pulled += 1;
                }
            }
            for link in &r_links {
                if validate::validate_link(&link.source_id, &link.target_id, &link.relation)
                    .is_err()
                {
                    continue;
                }
                let _ = db::create_link(
                    &local_conn,
                    &link.source_id,
                    &link.target_id,
                    &link.relation,
                );
            }
            for mem in &l_mems {
                if validate::validate_memory(mem).is_err() {
                    continue;
                }
                if db::insert_if_newer(&remote_conn, mem).is_ok() {
                    pushed += 1;
                }
            }
            for link in &l_links {
                if validate::validate_link(&link.source_id, &link.target_id, &link.relation)
                    .is_err()
                {
                    continue;
                }
                let _ = db::create_link(
                    &remote_conn,
                    &link.source_id,
                    &link.target_id,
                    &link.relation,
                );
            }
            if json_out {
                writeln!(
                    out.stdout,
                    "{}",
                    serde_json::json!({"direction": "merge", "pulled": pulled, "pushed": pushed})
                )?;
            } else {
                writeln!(out.stdout, "merged: pulled {pulled}, pushed {pushed}")?;
            }
        }
        _ => anyhow::bail!(
            "invalid direction: {} (use pull, push, merge)",
            args.direction
        ),
    }
    Ok(())
}

fn cmd_sync_dry_run(
    local_conn: &rusqlite::Connection,
    remote_conn: &rusqlite::Connection,
    direction: &str,
    json_out: bool,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let l_mems = db::export_all(local_conn)?;
    let r_mems = db::export_all(remote_conn)?;
    let l_links = db::export_links(local_conn)?;
    let r_links = db::export_links(remote_conn)?;

    let local_by_id: std::collections::HashMap<&str, &models::Memory> =
        l_mems.iter().map(|m| (m.id.as_str(), m)).collect();
    let remote_by_id: std::collections::HashMap<&str, &models::Memory> =
        r_mems.iter().map(|m| (m.id.as_str(), m)).collect();

    let mut preview = SyncPreview::default();

    let classify_pull = direction != "push";
    let classify_push = direction != "pull";

    if classify_pull {
        for mem in &r_mems {
            match SyncPreview::classify(local_by_id.get(mem.id.as_str()).copied(), mem) {
                MergeOutcome::New => preview.would_pull_new += 1,
                MergeOutcome::Update => preview.would_pull_update += 1,
                MergeOutcome::Noop => preview.would_pull_noop += 1,
            }
        }
        preview.would_pull_links = r_links.len();
    }

    if classify_push {
        for mem in &l_mems {
            match SyncPreview::classify(remote_by_id.get(mem.id.as_str()).copied(), mem) {
                MergeOutcome::New => preview.would_push_new += 1,
                MergeOutcome::Update => preview.would_push_update += 1,
                MergeOutcome::Noop => preview.would_push_noop += 1,
            }
        }
        preview.would_push_links = l_links.len();
    }

    if json_out {
        writeln!(
            out.stdout,
            "{}",
            serde_json::json!({
                "dry_run": true,
                "direction": direction,
                "pull": {
                    "new": preview.would_pull_new,
                    "update": preview.would_pull_update,
                    "noop": preview.would_pull_noop,
                    "links": preview.would_pull_links,
                },
                "push": {
                    "new": preview.would_push_new,
                    "update": preview.would_push_update,
                    "noop": preview.would_push_noop,
                    "links": preview.would_push_links,
                }
            })
        )?;
    } else {
        writeln!(
            out.stdout,
            "DRY RUN — no changes written. Direction: {direction}"
        )?;
        if classify_pull {
            writeln!(
                out.stdout,
                "  pull: {} new, {} update, {} noop, {} links",
                preview.would_pull_new,
                preview.would_pull_update,
                preview.would_pull_noop,
                preview.would_pull_links
            )?;
        }
        if classify_push {
            writeln!(
                out.stdout,
                "  push: {} new, {} update, {} noop, {} links",
                preview.would_push_new,
                preview.would_push_update,
                preview.would_push_noop,
                preview.would_push_links
            )?;
        }
    }
    Ok(())
}

/// `sync-daemon` handler. Delegates the inner loop to `daemon_runtime`.
pub async fn run_daemon(
    db_path: &Path,
    args: SyncDaemonArgs,
    cli_agent_id: Option<&str>,
) -> Result<()> {
    if args.peers.is_empty() {
        anyhow::bail!("at least one --peers URL is required");
    }
    let interval = args.interval.max(1);
    let batch_size = args.batch_size.max(1);
    let local_agent_id = identity::resolve_agent_id(cli_agent_id, None)?;

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("ai_memory=info".parse()?)
                .add_directive("tower_http=info".parse()?),
        )
        .try_init();

    let _ = rustls::crypto::ring::default_provider().install_default();
    if args.insecure_skip_server_verify && (args.client_cert.is_none() || args.client_key.is_none())
    {
        anyhow::bail!(
            "sync-daemon: --insecure-skip-server-verify requires both --client-cert \
             and --client-key as a compensating mTLS control. Running with neither side \
             of the TLS handshake verified is an open MITM surface and is refused."
        );
    }

    let client = if let (Some(cert_path), Some(key_path)) = (&args.client_cert, &args.client_key) {
        let rustls_config = tls::build_rustls_client_config(cert_path, key_path).await?;
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .use_preconfigured_tls(rustls_config);
        if args.insecure_skip_server_verify {
            tracing::warn!(
                "sync-daemon: --insecure-skip-server-verify set with --client-cert — \
                 peer server certificates will NOT be validated; peer authenticates us \
                 via mTLS allowlist (compensating control). Do NOT use in production."
            );
            builder = builder.danger_accept_invalid_certs(true);
        }
        builder.build()?
    } else {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?
    };

    tracing::info!(
        "sync-daemon: local_agent_id={local_agent_id} peers={peers:?} interval={interval}s",
        peers = args.peers
    );

    let shutdown = Arc::new(tokio::sync::Notify::new());
    let shutdown_for_signal = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown_for_signal.notify_one();
    });

    crate::daemon_runtime::run_sync_daemon_with_shutdown_using_client(
        client,
        db_path.to_path_buf(),
        local_agent_id,
        args.peers,
        args.api_key,
        interval,
        batch_size,
        shutdown,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    fn args_for(remote_db: PathBuf, direction: &str) -> SyncArgs {
        SyncArgs {
            remote_db,
            direction: direction.to_string(),
            trust_source: false,
            dry_run: false,
        }
    }

    #[test]
    fn test_sync_dry_run_merge() {
        let mut env = TestEnv::fresh();
        let local = env.db_path.clone();
        let remote_env = TestEnv::fresh();
        let remote = remote_env.db_path.clone();
        seed_memory(&local, "ns", "local-only", "L");
        seed_memory(&remote, "ns", "remote-only", "R");
        let mut args = args_for(remote, "merge");
        args.dry_run = true;
        {
            let mut out = env.output();
            run(&local, &args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["dry_run"].as_bool().unwrap(), true);
        assert_eq!(v["direction"].as_str().unwrap(), "merge");
    }

    #[test]
    fn test_sync_pull_direction() {
        let mut env = TestEnv::fresh();
        let local = env.db_path.clone();
        let remote_env = TestEnv::fresh();
        let remote = remote_env.db_path.clone();
        seed_memory(&remote, "ns", "from-remote", "data");
        let args = args_for(remote, "pull");
        {
            let mut out = env.output();
            run(&local, &args, false, Some("test-agent"), &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("pulled"));
    }

    #[test]
    fn test_sync_push_direction() {
        let mut env = TestEnv::fresh();
        let local = env.db_path.clone();
        let remote_env = TestEnv::fresh();
        let remote = remote_env.db_path.clone();
        seed_memory(&local, "ns", "to-remote", "data");
        let args = args_for(remote, "push");
        {
            let mut out = env.output();
            run(&local, &args, false, Some("test-agent"), &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("pushed"));
    }

    #[test]
    fn test_sync_merge_direction() {
        let mut env = TestEnv::fresh();
        let local = env.db_path.clone();
        let remote_env = TestEnv::fresh();
        let remote = remote_env.db_path.clone();
        seed_memory(&local, "ns", "L", "L");
        seed_memory(&remote, "ns", "R", "R");
        let args = args_for(remote, "merge");
        {
            let mut out = env.output();
            run(&local, &args, false, Some("test-agent"), &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("merged:"));
    }

    #[test]
    fn test_sync_invalid_direction_errors() {
        let mut env = TestEnv::fresh();
        let local = env.db_path.clone();
        let remote_env = TestEnv::fresh();
        let remote = remote_env.db_path.clone();
        let args = args_for(remote, "sideways");
        let mut out = env.output();
        let res = run(&local, &args, false, Some("test-agent"), &mut out);
        assert!(res.is_err());
    }

    #[test]
    fn test_sync_dry_run_pull_only() {
        let mut env = TestEnv::fresh();
        let local = env.db_path.clone();
        let remote_env = TestEnv::fresh();
        let remote = remote_env.db_path.clone();
        seed_memory(&remote, "ns", "remote", "x");
        let mut args = args_for(remote, "pull");
        args.dry_run = true;
        {
            let mut out = env.output();
            run(&local, &args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert!(v["pull"]["new"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn test_restamp_agent_id_preserves_original() {
        let mut mem = models::Memory {
            id: "m1".to_string(),
            tier: models::Tier::Mid,
            namespace: "ns".to_string(),
            title: "t".to_string(),
            content: "c".to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id": "remote-agent"}),
        };
        restamp_agent_id(&mut mem, "local-agent");
        assert_eq!(mem.metadata["agent_id"].as_str().unwrap(), "local-agent");
        assert_eq!(
            mem.metadata["imported_from_agent_id"].as_str().unwrap(),
            "remote-agent"
        );
    }

    #[test]
    fn test_restamp_same_agent_no_imported_from() {
        let mut mem = models::Memory {
            id: "m1".to_string(),
            tier: models::Tier::Mid,
            namespace: "ns".to_string(),
            title: "t".to_string(),
            content: "c".to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id": "same-agent"}),
        };
        restamp_agent_id(&mut mem, "same-agent");
        assert_eq!(mem.metadata["agent_id"].as_str().unwrap(), "same-agent");
        assert!(mem.metadata.get("imported_from_agent_id").is_none());
    }

    #[tokio::test]
    async fn test_sync_daemon_empty_peers_errors() {
        let env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = SyncDaemonArgs {
            peers: Vec::new(),
            interval: 2,
            api_key: None,
            batch_size: 500,
            client_cert: None,
            client_key: None,
            insecure_skip_server_verify: false,
        };
        let res = run_daemon(&db, args, Some("test-agent")).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("--peers"));
    }

    #[tokio::test]
    async fn test_sync_daemon_insecure_without_mtls_errors() {
        let env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = SyncDaemonArgs {
            peers: vec!["http://example.com:9077".to_string()],
            interval: 2,
            api_key: None,
            batch_size: 500,
            client_cert: None,
            client_key: None,
            insecure_skip_server_verify: true,
        };
        let res = run_daemon(&db, args, Some("test-agent")).await;
        assert!(res.is_err());
        assert!(
            res.unwrap_err()
                .to_string()
                .contains("insecure-skip-server-verify")
        );
    }

    // PR-9i — buffer coverage uplift. Targets previously-uncovered branches
    // in run() / cmd_sync_dry_run: link-sync paths in pull/push/merge,
    // text-mode dry_run output, restamp_agent_id with no original agent_id.

    #[test]
    fn pr9i_pull_propagates_links() {
        let mut env = TestEnv::fresh();
        let local = env.db_path.clone();
        let remote_env = TestEnv::fresh();
        let remote = remote_env.db_path.clone();
        let id1 = seed_memory(&remote, "ns", "src", "src-content");
        let id2 = seed_memory(&remote, "ns", "tgt", "tgt-content");
        {
            let conn = db::open(&remote).unwrap();
            db::create_link(&conn, &id1, &id2, "related_to").unwrap();
        }
        let args = args_for(remote, "pull");
        {
            let mut out = env.output();
            run(&local, &args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["direction"].as_str().unwrap(), "pull");
        let local_conn = db::open(&local).unwrap();
        let local_links = db::export_links(&local_conn).unwrap();
        assert!(
            local_links.iter().any(|l| l.relation == "related_to"),
            "expected pulled link to land in local: {local_links:?}"
        );
    }

    #[test]
    fn pr9i_push_propagates_links() {
        let mut env = TestEnv::fresh();
        let local = env.db_path.clone();
        let remote_env = TestEnv::fresh();
        let remote = remote_env.db_path.clone();
        let id1 = seed_memory(&local, "ns", "a", "a");
        let id2 = seed_memory(&local, "ns", "b", "b");
        {
            let conn = db::open(&local).unwrap();
            db::create_link(&conn, &id1, &id2, "supersedes").unwrap();
        }
        let args = args_for(remote.clone(), "push");
        {
            let mut out = env.output();
            run(&local, &args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["direction"].as_str().unwrap(), "push");
        let remote_conn = db::open(&remote).unwrap();
        let remote_links = db::export_links(&remote_conn).unwrap();
        assert!(remote_links.iter().any(|l| l.relation == "supersedes"));
    }

    #[test]
    fn pr9i_merge_propagates_links_both_directions() {
        let mut env = TestEnv::fresh();
        let local = env.db_path.clone();
        let remote_env = TestEnv::fresh();
        let remote = remote_env.db_path.clone();
        let l1 = seed_memory(&local, "ns", "l1", "l1");
        let l2 = seed_memory(&local, "ns", "l2", "l2");
        {
            let conn = db::open(&local).unwrap();
            db::create_link(&conn, &l1, &l2, "related_to").unwrap();
        }
        let r1 = seed_memory(&remote, "ns", "r1", "r1");
        let r2 = seed_memory(&remote, "ns", "r2", "r2");
        {
            let conn = db::open(&remote).unwrap();
            db::create_link(&conn, &r1, &r2, "derived_from").unwrap();
        }
        let args = args_for(remote.clone(), "merge");
        {
            let mut out = env.output();
            run(&local, &args, false, Some("test-agent"), &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("merged:"));
        let lconn = db::open(&local).unwrap();
        let rconn = db::open(&remote).unwrap();
        let l_relations: Vec<String> = db::export_links(&lconn)
            .unwrap()
            .into_iter()
            .map(|l| l.relation)
            .collect();
        let r_relations: Vec<String> = db::export_links(&rconn)
            .unwrap()
            .into_iter()
            .map(|l| l.relation)
            .collect();
        assert!(l_relations.iter().any(|r| r == "derived_from"));
        assert!(r_relations.iter().any(|r| r == "related_to"));
    }

    #[test]
    fn pr9i_dry_run_text_mode_merge() {
        let mut env = TestEnv::fresh();
        let local = env.db_path.clone();
        let remote_env = TestEnv::fresh();
        let remote = remote_env.db_path.clone();
        seed_memory(&local, "ns", "L", "L");
        seed_memory(&remote, "ns", "R", "R");
        let mut args = args_for(remote, "merge");
        args.dry_run = true;
        {
            let mut out = env.output();
            run(&local, &args, false, Some("test-agent"), &mut out).unwrap();
        }
        let s = env.stdout_str();
        assert!(s.contains("DRY RUN"));
        assert!(s.contains("pull:"));
        assert!(s.contains("push:"));
        assert!(s.contains("noop"));
        assert!(s.contains("links"));
    }

    #[test]
    fn pr9i_dry_run_text_mode_pull_only() {
        let mut env = TestEnv::fresh();
        let local = env.db_path.clone();
        let remote_env = TestEnv::fresh();
        let remote = remote_env.db_path.clone();
        seed_memory(&remote, "ns", "remote-only", "rr");
        let mut args = args_for(remote, "pull");
        args.dry_run = true;
        {
            let mut out = env.output();
            run(&local, &args, false, Some("test-agent"), &mut out).unwrap();
        }
        let s = env.stdout_str();
        assert!(s.contains("DRY RUN"));
        assert!(s.contains("pull:"));
        assert!(!s.contains("push:"));
    }

    #[test]
    fn pr9i_dry_run_text_mode_push_only() {
        let mut env = TestEnv::fresh();
        let local = env.db_path.clone();
        let remote_env = TestEnv::fresh();
        let remote = remote_env.db_path.clone();
        seed_memory(&local, "ns", "local-only", "ll");
        let mut args = args_for(remote, "push");
        args.dry_run = true;
        {
            let mut out = env.output();
            run(&local, &args, false, Some("test-agent"), &mut out).unwrap();
        }
        let s = env.stdout_str();
        assert!(s.contains("DRY RUN"));
        assert!(s.contains("push:"));
        assert!(!s.contains("pull:"));
    }

    #[test]
    fn pr9i_dry_run_classify_update_branch() {
        let mut env = TestEnv::fresh();
        let local = env.db_path.clone();
        let remote_env = TestEnv::fresh();
        let remote = remote_env.db_path.clone();
        let id = seed_memory(&local, "ns", "shared", "old-content");
        let conn = db::open(&remote).unwrap();
        let mem = models::Memory {
            id: id.clone(),
            tier: models::Tier::Mid,
            namespace: "ns".to_string(),
            title: "shared".to_string(),
            content: "newer-content".to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2099-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
        };
        db::insert(&conn, &mem).unwrap();
        drop(conn);
        let mut args = args_for(remote, "merge");
        args.dry_run = true;
        {
            let mut out = env.output();
            run(&local, &args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert!(v["pull"]["update"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn pr9i_restamp_no_original_agent_id() {
        let mut mem = models::Memory {
            id: "m-noid".to_string(),
            tier: models::Tier::Mid,
            namespace: "ns".to_string(),
            title: "t".to_string(),
            content: "c".to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
        };
        restamp_agent_id(&mut mem, "caller-agent");
        assert_eq!(mem.metadata["agent_id"].as_str().unwrap(), "caller-agent");
        assert!(mem.metadata.get("imported_from_agent_id").is_none());
    }

    #[test]
    fn pr9i_pull_skips_invalid_link() {
        let mut env = TestEnv::fresh();
        let local = env.db_path.clone();
        let remote_env = TestEnv::fresh();
        let remote = remote_env.db_path.clone();
        let id1 = seed_memory(&remote, "ns", "src", "src");
        let id2 = seed_memory(&remote, "ns", "tgt", "tgt");
        let conn = db::open(&remote).unwrap();
        conn.execute(
            "INSERT INTO memory_links (source_id, target_id, relation, created_at) VALUES (?, ?, '', datetime('now'))",
            rusqlite::params![id1, id2],
        )
        .unwrap();
        drop(conn);
        let args = args_for(remote, "pull");
        {
            let mut out = env.output();
            run(&local, &args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["direction"].as_str().unwrap(), "pull");
    }
}
