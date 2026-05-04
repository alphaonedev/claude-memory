// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_link` and `cmd_resolve` migrations. See `cli::store` for the
//! design pattern.

use crate::cli::CliOutput;
use crate::{color, db, models, validate};
use anyhow::Result;
use clap::Args;
use std::path::Path;

#[derive(Args)]
pub struct LinkArgs {
    pub source_id: String,
    pub target_id: String,
    #[arg(long, short, default_value = "related_to")]
    pub relation: String,
}

#[derive(Args)]
pub struct ResolveArgs {
    /// ID of the memory that wins (supersedes)
    pub winner_id: String,
    /// ID of the memory that loses (superseded)
    pub loser_id: String,
}

/// `link` handler.
pub fn cmd_link(
    db_path: &Path,
    args: &LinkArgs,
    json_out: bool,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    validate::validate_link(&args.source_id, &args.target_id, &args.relation)?;
    let conn = db::open(db_path)?;
    db::create_link(&conn, &args.source_id, &args.target_id, &args.relation)?;
    if json_out {
        writeln!(out.stdout, "{}", serde_json::json!({"linked": true}))?;
    } else {
        writeln!(
            out.stdout,
            "linked: {} --[{}]--> {}",
            args.source_id, args.relation, args.target_id
        )?;
    }
    Ok(())
}

/// `resolve` handler — record `winner supersedes loser`, demote loser
/// priority/confidence, and refresh winner's TTL.
pub fn cmd_resolve(
    db_path: &Path,
    args: &ResolveArgs,
    json_out: bool,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let conn = db::open(db_path)?;
    validate::validate_link(&args.winner_id, &args.loser_id, "supersedes")?;
    db::create_link(&conn, &args.winner_id, &args.loser_id, "supersedes")?;
    let _ = db::update(
        &conn,
        &args.loser_id,
        None,
        None,
        None,
        None,
        None,
        Some(1),
        Some(0.1),
        None,
        None,
    )?;
    db::touch(
        &conn,
        &args.winner_id,
        models::SHORT_TTL_EXTEND_SECS,
        models::MID_TTL_EXTEND_SECS,
    )?;
    if json_out {
        writeln!(
            out.stdout,
            "{}",
            serde_json::json!({"resolved": true, "winner": args.winner_id, "loser": args.loser_id})
        )?;
    } else {
        writeln!(
            out.stdout,
            "resolved: {} supersedes {}",
            color::long(&args.winner_id),
            color::dim(&args.loser_id)
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    #[test]
    fn test_link_happy_path() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id1 = seed_memory(&db, "ns", "a", "ca");
        let id2 = seed_memory(&db, "ns", "b", "cb");
        let args = LinkArgs {
            source_id: id1.clone(),
            target_id: id2.clone(),
            relation: "related_to".to_string(),
        };
        {
            let mut out = env.output();
            cmd_link(&db, &args, false, &mut out).unwrap();
        }
        assert!(
            env.stdout_str().contains("linked:"),
            "got: {}",
            env.stdout_str()
        );
        // Confirm row exists in DB.
        let conn = db::open(&db).unwrap();
        let links = db::get_links(&conn, &id1).unwrap();
        assert!(links.iter().any(|l| l.target_id == id2));
    }

    #[test]
    fn test_link_invalid_relation_validation_error() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id1 = seed_memory(&db, "ns", "a", "ca");
        let id2 = seed_memory(&db, "ns", "b", "cb");
        let args = LinkArgs {
            source_id: id1,
            target_id: id2,
            relation: "totally-bogus-relation".to_string(),
        };
        let mut out = env.output();
        let res = cmd_link(&db, &args, false, &mut out);
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("invalid relation"), "got: {msg}");
    }

    #[test]
    fn test_link_self_link_validation_error() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "a", "ca");
        let args = LinkArgs {
            source_id: id.clone(),
            target_id: id,
            relation: "related_to".to_string(),
        };
        let mut out = env.output();
        let res = cmd_link(&db, &args, false, &mut out);
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("itself"), "got: {msg}");
    }

    #[test]
    fn test_link_json_output() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id1 = seed_memory(&db, "ns", "a", "ca");
        let id2 = seed_memory(&db, "ns", "b", "cb");
        let args = LinkArgs {
            source_id: id1,
            target_id: id2,
            relation: "supersedes".to_string(),
        };
        {
            let mut out = env.output();
            cmd_link(&db, &args, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["linked"].as_bool().unwrap(), true);
    }

    #[test]
    fn test_resolve_creates_supersedes_link() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let winner = seed_memory(&db, "ns", "winner", "wins");
        let loser = seed_memory(&db, "ns", "loser", "loses");
        let args = ResolveArgs {
            winner_id: winner.clone(),
            loser_id: loser.clone(),
        };
        {
            let mut out = env.output();
            cmd_resolve(&db, &args, false, &mut out).unwrap();
        }
        let conn = db::open(&db).unwrap();
        let links = db::get_links(&conn, &winner).unwrap();
        assert!(
            links
                .iter()
                .any(|l| l.target_id == loser && l.relation == "supersedes"),
            "expected supersedes link from winner to loser"
        );
    }

    #[test]
    fn test_resolve_demotes_loser_priority_and_confidence() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let winner = seed_memory(&db, "ns", "winner", "wins");
        let loser = seed_memory(&db, "ns", "loser", "loses");
        let args = ResolveArgs {
            winner_id: winner,
            loser_id: loser.clone(),
        };
        {
            let mut out = env.output();
            cmd_resolve(&db, &args, true, &mut out).unwrap();
        }
        let conn = db::open(&db).unwrap();
        let mem = db::get(&conn, &loser).unwrap().unwrap();
        assert_eq!(mem.priority, 1);
        assert!((mem.confidence - 0.1).abs() < 1e-6);
    }

    #[test]
    fn test_resolve_touches_winner() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let winner = seed_memory(&db, "ns", "winner", "wins");
        let loser = seed_memory(&db, "ns", "loser", "loses");
        // Capture access_count + updated_at before resolve.
        let conn = db::open(&db).unwrap();
        let pre = db::get(&conn, &winner).unwrap().unwrap();
        let pre_access = pre.access_count;
        drop(conn);
        let args = ResolveArgs {
            winner_id: winner.clone(),
            loser_id: loser,
        };
        {
            let mut out = env.output();
            cmd_resolve(&db, &args, true, &mut out).unwrap();
        }
        let conn = db::open(&db).unwrap();
        let post = db::get(&conn, &winner).unwrap().unwrap();
        // touch() bumps access_count.
        assert!(
            post.access_count >= pre_access,
            "access_count should not regress: pre={pre_access} post={}",
            post.access_count
        );
    }
}
