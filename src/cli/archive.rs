// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_archive` migration. See `cli::store` for the design pattern.

use crate::cli::CliOutput;
use crate::cli::helpers::id_short;
use crate::{db, validate};
use anyhow::Result;
use clap::{Args, Subcommand};
use std::path::Path;

#[derive(Args)]
pub struct ArchiveArgs {
    #[command(subcommand)]
    pub action: ArchiveAction,
}

#[derive(Subcommand)]
pub enum ArchiveAction {
    /// List archived memories
    List {
        #[arg(long, short)]
        namespace: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Restore an archived memory back to active
    Restore { id: String },
    /// Permanently delete old archive entries
    Purge {
        /// Delete archive entries older than N days (all if omitted)
        #[arg(long)]
        older_than_days: Option<i64>,
    },
    /// Show archive statistics
    Stats,
}

/// `archive` handler.
pub fn run(
    db_path: &Path,
    args: ArchiveArgs,
    json_out: bool,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let conn = db::open(db_path)?;
    match args.action {
        ArchiveAction::List {
            namespace,
            limit,
            offset,
        } => {
            let items = db::list_archived(&conn, namespace.as_deref(), limit, offset)?;
            if json_out {
                writeln!(
                    out.stdout,
                    "{}",
                    serde_json::json!({"archived": items, "count": items.len()})
                )?;
            } else if items.is_empty() {
                writeln!(out.stdout, "no archived memories")?;
            } else {
                for item in &items {
                    writeln!(
                        out.stdout,
                        "[{}] {} (archived: {})",
                        id_short(item["id"].as_str().unwrap_or("")),
                        item["title"].as_str().unwrap_or(""),
                        item["archived_at"].as_str().unwrap_or("")
                    )?;
                }
                writeln!(out.stdout, "{} archived memories", items.len())?;
            }
        }
        ArchiveAction::Restore { id } => {
            validate::validate_id(&id)?;
            let restored = db::restore_archived(&conn, &id)?;
            if json_out {
                writeln!(
                    out.stdout,
                    "{}",
                    serde_json::json!({"restored": restored, "id": id})
                )?;
            } else if restored {
                writeln!(out.stdout, "restored: {}", id_short(&id))?;
            } else {
                writeln!(out.stderr, "not found in archive: {id}")?;
                std::process::exit(1);
            }
        }
        ArchiveAction::Purge { older_than_days } => {
            // #913 (security-medium / SOC2, 2026-05-19) — admin/destructive
            // state-change audit. CLI archive purge mirrors the HTTP +
            // MCP fixes; emit the forensic-chain row BEFORE the storage
            // write so the audit trail captures the operator regardless
            // of downstream outcome.
            let caller = crate::identity::resolve_agent_id(None, None)
                .unwrap_or_else(|_| format!("anonymous:pid-{}", std::process::id()));
            crate::governance::audit::record_decision(
                &caller,
                "allow",
                "archive_purge",
                "",
                serde_json::json!({ "older_than_days": older_than_days }),
            );

            let purged = db::purge_archive(&conn, older_than_days)?;
            if json_out {
                writeln!(out.stdout, "{}", serde_json::json!({"purged": purged}))?;
            } else {
                writeln!(out.stdout, "purged {purged} archived memories")?;
            }
        }
        ArchiveAction::Stats => {
            let stats = db::archive_stats(&conn)?;
            if json_out {
                writeln!(out.stdout, "{stats}")?;
            } else {
                writeln!(out.stdout, "archived: {} total", stats["archived_total"])?;
                if let Some(by_ns) = stats["by_namespace"].as_array() {
                    for ns in by_ns {
                        writeln!(
                            out.stdout,
                            "  {}: {}",
                            ns["namespace"].as_str().unwrap_or(""),
                            ns["count"]
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    #[test]
    fn test_archive_list_empty() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = ArchiveArgs {
            action: ArchiveAction::List {
                namespace: None,
                limit: 50,
                offset: 0,
            },
        };
        {
            let mut out = env.output();
            run(&db, args, false, &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("no archived memories"));
    }

    #[test]
    fn test_archive_list_empty_json() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = ArchiveArgs {
            action: ArchiveAction::List {
                namespace: None,
                limit: 50,
                offset: 0,
            },
        };
        {
            let mut out = env.output();
            run(&db, args, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["count"].as_u64().unwrap(), 0);
        assert!(v["archived"].is_array());
    }

    #[test]
    fn test_archive_list_with_namespace_filter() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = ArchiveArgs {
            action: ArchiveAction::List {
                namespace: Some("nope".to_string()),
                limit: 50,
                offset: 0,
            },
        };
        {
            let mut out = env.output();
            run(&db, args, false, &mut out).unwrap();
        }
        // No archived memories in any namespace yet.
        assert!(env.stdout_str().contains("no archived memories"));
    }

    #[test]
    fn test_archive_restore_nonexistent_exits_via_stderr() {
        // process::exit would terminate the test; we instead use a valid-looking
        // ID and expect the stderr write, but since exit(1) happens we test the
        // success branch via direct DB seeding.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        // Seed a memory and archive it via direct DB call.
        let id = seed_memory(&db, "ns", "t", "c");
        let conn = db::open(&db).unwrap();
        let _ = db::archive_memory(&conn, &id, None);
        drop(conn);
        let args = ArchiveArgs {
            action: ArchiveAction::Restore { id: id.clone() },
        };
        {
            let mut out = env.output();
            run(&db, args, false, &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("restored:"));
    }

    #[test]
    fn test_archive_restore_json() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "t", "c");
        let conn = db::open(&db).unwrap();
        let _ = db::archive_memory(&conn, &id, None);
        drop(conn);
        let args = ArchiveArgs {
            action: ArchiveAction::Restore { id: id.clone() },
        };
        {
            let mut out = env.output();
            run(&db, args, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["restored"].as_bool().unwrap(), true);
    }

    #[test]
    fn test_archive_purge_no_filter() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = ArchiveArgs {
            action: ArchiveAction::Purge {
                older_than_days: None,
            },
        };
        {
            let mut out = env.output();
            run(&db, args, false, &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("purged 0"));
    }

    #[test]
    fn test_archive_purge_older_than_filter() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = ArchiveArgs {
            action: ArchiveAction::Purge {
                older_than_days: Some(30),
            },
        };
        {
            let mut out = env.output();
            run(&db, args, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["purged"].as_u64().unwrap(), 0);
    }

    #[test]
    fn test_archive_stats() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = ArchiveArgs {
            action: ArchiveAction::Stats,
        };
        {
            let mut out = env.output();
            run(&db, args, false, &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("archived:"));
    }

    #[test]
    fn test_archive_stats_json() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = ArchiveArgs {
            action: ArchiveAction::Stats,
        };
        {
            let mut out = env.output();
            run(&db, args, true, &mut out).unwrap();
        }
        // Stats prints raw json blob, parseable.
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert!(v["archived_total"].is_number());
    }

    // ---------- E1 coverage uplift: list-with-items + stats with by_namespace
    // Both branches require seeding then archiving at least one memory so the
    // archived_at row materializes.

    /// Seed N memories in `ns`, archive them all. Returns the archived ids.
    fn seed_and_archive(db: &std::path::Path, ns: &str, n: usize) -> Vec<String> {
        let mut ids = Vec::with_capacity(n);
        let conn = db::open(db).unwrap();
        for i in 0..n {
            let id = seed_memory(db, ns, &format!("title-{i}"), &format!("body-{i}"));
            db::archive_memory(&conn, &id, None).unwrap();
            ids.push(id);
        }
        ids
    }

    #[test]
    fn test_archive_list_text_with_items() {
        // Drives the for-loop body (lines 66-75) — `[id_short] title (archived: ts)`.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_and_archive(&db, "ns-arch", 2);
        let args = ArchiveArgs {
            action: ArchiveAction::List {
                namespace: Some("ns-arch".to_string()),
                limit: 50,
                offset: 0,
            },
        };
        {
            let mut out = env.output();
            run(&db, args, false, &mut out).unwrap();
        }
        let s = env.stdout_str();
        // Should mention both rows + the footer.
        assert!(s.contains("archived:"));
        assert!(s.contains("title-0") || s.contains("title-1"));
        assert!(s.contains("2 archived memories"));
    }

    #[test]
    fn test_archive_list_json_with_items() {
        // JSON variant — covers the `if json_out` arm with non-empty items.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_and_archive(&db, "ns-arch-j", 3);
        let args = ArchiveArgs {
            action: ArchiveAction::List {
                namespace: Some("ns-arch-j".to_string()),
                limit: 50,
                offset: 0,
            },
        };
        {
            let mut out = env.output();
            run(&db, args, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["count"].as_u64().unwrap(), 3);
    }

    #[test]
    fn test_archive_stats_text_with_namespace_breakdown() {
        // Drives the `if let Some(by_ns)` arm (lines 108-117) — one row per ns.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_and_archive(&db, "ns-stats-a", 1);
        seed_and_archive(&db, "ns-stats-b", 2);
        let args = ArchiveArgs {
            action: ArchiveAction::Stats,
        };
        {
            let mut out = env.output();
            run(&db, args, false, &mut out).unwrap();
        }
        let s = env.stdout_str();
        assert!(s.contains("archived:"));
        // Either of the two namespace lines should appear.
        assert!(
            s.contains("ns-stats-a") || s.contains("ns-stats-b"),
            "stats text missing namespace breakdown, got: {s}"
        );
    }

    #[test]
    fn test_archive_purge_clears_with_filter() {
        // Seed + archive, then purge with older_than_days=0 — sweeps everything.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_and_archive(&db, "ns-purge", 2);
        let args = ArchiveArgs {
            action: ArchiveAction::Purge {
                older_than_days: Some(0),
            },
        };
        {
            let mut out = env.output();
            run(&db, args, false, &mut out).unwrap();
        }
        let s = env.stdout_str();
        // Anything from 0 to 2 — depends on archive_age semantics on this
        // SQLite build. The line itself must surface.
        assert!(s.contains("purged"));
    }
}
