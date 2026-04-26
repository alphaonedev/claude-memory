// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_update` migration. See `cli::store` for the design pattern.

use crate::cli::CliOutput;
use crate::{db, validate};
use anyhow::Result;
use clap::Args;
use std::path::Path;

#[derive(Args)]
pub struct UpdateArgs {
    pub id: String,
    #[arg(long, short = 'T', allow_hyphen_values = true)]
    pub title: Option<String>,
    #[arg(long, short, allow_hyphen_values = true)]
    pub content: Option<String>,
    #[arg(long, short)]
    pub tier: Option<String>,
    #[arg(long, short)]
    pub namespace: Option<String>,
    #[arg(long)]
    pub tags: Option<String>,
    #[arg(long, short)]
    pub priority: Option<i32>,
    #[arg(long)]
    pub confidence: Option<f64>,
    /// Expiry timestamp (RFC3339), or empty string to clear
    #[arg(long)]
    pub expires_at: Option<String>,
}

/// `update` handler.
pub fn run(
    db_path: &Path,
    args: &UpdateArgs,
    json_out: bool,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    use crate::models::Tier;
    validate::validate_id(&args.id)?;
    let conn = db::open(db_path)?;
    let resolved_id = if db::get(&conn, &args.id)?.is_some() {
        args.id.clone()
    } else if let Some(mem) = db::get_by_prefix(&conn, &args.id)? {
        mem.id
    } else {
        writeln!(out.stderr, "not found: {}", args.id)?;
        std::process::exit(1);
    };
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let tags: Option<Vec<String>> = args.tags.as_ref().map(|t| {
        t.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    });
    if let Some(ref t) = args.title {
        validate::validate_title(t)?;
    }
    if let Some(ref c) = args.content {
        validate::validate_content(c)?;
    }
    if let Some(ref ns) = args.namespace {
        validate::validate_namespace(ns)?;
    }
    if let Some(ref tags) = tags {
        validate::validate_tags(tags)?;
    }
    if let Some(p) = args.priority {
        validate::validate_priority(p)?;
    }
    if let Some(c) = args.confidence {
        validate::validate_confidence(c)?;
    }
    if let Some(ref ts) = args.expires_at
        && !ts.is_empty()
    {
        validate::validate_expires_at_format(ts)?;
    }
    let (found, _content_changed) = db::update(
        &conn,
        &resolved_id,
        args.title.as_deref(),
        args.content.as_deref(),
        tier.as_ref(),
        args.namespace.as_deref(),
        tags.as_ref(),
        args.priority,
        args.confidence,
        args.expires_at.as_deref(),
        None,
    )?;
    if !found {
        writeln!(out.stderr, "not found: {}", args.id)?;
        std::process::exit(1);
    }
    if let Some(mem) = db::get(&conn, &resolved_id)? {
        if json_out {
            writeln!(out.stdout, "{}", serde_json::to_string(&mem)?)?;
        } else {
            writeln!(out.stdout, "updated: {} [{}]", mem.id, mem.title)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    fn empty_args(id: &str) -> UpdateArgs {
        UpdateArgs {
            id: id.to_string(),
            title: None,
            content: None,
            tier: None,
            namespace: None,
            tags: None,
            priority: None,
            confidence: None,
            expires_at: None,
        }
    }

    #[test]
    fn test_update_happy_path() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "old-title", "old content");
        let mut args = empty_args(&id);
        args.title = Some("new-title".to_string());
        args.content = Some("new content".to_string());
        {
            let mut out = env.output();
            run(&db, &args, false, &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("updated:"));
        assert!(env.stdout_str().contains("new-title"));
    }

    #[test]
    fn test_update_by_prefix_id() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "title-a", "content-a");
        // Use an 8-char prefix (UUIDs are 36 chars).
        let prefix = &id[..8];
        let mut args = empty_args(prefix);
        args.title = Some("renamed".to_string());
        {
            let mut out = env.output();
            run(&db, &args, false, &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("renamed"));
    }

    // Skip nonexistent-id-exits-nonzero test directly: process::exit
    // tears down the test runner. Exit-path coverage handled in the
    // integration suite that spawns the binary.

    #[test]
    fn test_update_partial_only_title() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "orig-title", "orig content");
        let mut args = empty_args(&id);
        args.title = Some("title-only-change".to_string());
        {
            let mut out = env.output();
            run(&db, &args, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["title"].as_str().unwrap(), "title-only-change");
        assert_eq!(v["content"].as_str().unwrap(), "orig content");
    }

    #[test]
    fn test_update_partial_only_content() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "kept-title", "old-content");
        let mut args = empty_args(&id);
        args.content = Some("new content body".to_string());
        {
            let mut out = env.output();
            run(&db, &args, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["title"].as_str().unwrap(), "kept-title");
        assert_eq!(v["content"].as_str().unwrap(), "new content body");
    }

    #[test]
    fn test_update_clear_expires_at_with_empty_string() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "tt", "cc");
        let mut args = empty_args(&id);
        args.expires_at = Some(String::new());
        {
            let mut out = env.output();
            // Empty-string skips the format-validate branch and is
            // forwarded as a clear-expiry directive to db::update.
            run(&db, &args, false, &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("updated:"));
    }

    #[test]
    fn test_update_invalid_priority_validation_error() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "tt", "cc");
        let mut args = empty_args(&id);
        args.priority = Some(99);
        let mut out = env.output();
        let res = run(&db, &args, false, &mut out);
        assert!(res.is_err());
    }
}
