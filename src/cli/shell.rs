// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_shell` REPL migration. The line-handling logic is extracted into
//! `handle_command(parts, conn, out)` so unit tests can drive command
//! parsing/dispatch without spawning a subprocess. The outer stdin loop
//! is intentionally minimal and is **not** covered by unit tests — its
//! `read_line` blocking call would deadlock a buffer-driven test fixture.

use crate::cli::CliOutput;
use crate::cli::helpers::human_age;
use crate::{color, db, models, validate};
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

/// Returned by `handle_command` to signal whether the REPL should keep
/// reading more lines.
#[derive(Debug, PartialEq, Eq)]
pub enum ShellAction {
    /// Continue reading the next prompt line.
    Continue,
    /// Exit the REPL cleanly.
    Quit,
}

/// REPL command dispatcher. Splits its input into a command + tail and
/// emits all output through `out`. Returns `Quit` on `quit/exit/q`,
/// `Continue` otherwise.
#[allow(clippy::too_many_lines)]
pub fn handle_command(parts: &[&str], conn: &Connection, out: &mut CliOutput<'_>) -> ShellAction {
    if parts.is_empty() {
        return ShellAction::Continue;
    }
    match parts[0] {
        "quit" | "exit" | "q" => return ShellAction::Quit,
        "help" | "h" => {
            let _ = writeln!(out.stdout, "  recall <context>    — fuzzy recall");
            let _ = writeln!(out.stdout, "  search <query>      — keyword search");
            let _ = writeln!(out.stdout, "  list [namespace]    — list memories");
            let _ = writeln!(out.stdout, "  get <id>            — show memory details");
            let _ = writeln!(out.stdout, "  update <id> <field>=<value> [field=value]…");
            let _ = writeln!(
                out.stdout,
                "                       — mutate one or more fields (issue #653: full-profile parity)"
            );
            let _ = writeln!(out.stdout, "  stats               — show statistics");
            let _ = writeln!(out.stdout, "  namespaces          — list namespaces");
            let _ = writeln!(out.stdout, "  delete <id>         — delete a memory");
            let _ = writeln!(out.stdout, "  quit                — exit shell");
        }
        "recall" | "r" => {
            let ctx = parts[1..].join(" ");
            if ctx.is_empty() {
                let _ = writeln!(out.stderr, "usage: recall <context>");
                return ShellAction::Continue;
            }
            match db::recall(
                conn,
                &ctx,
                None,
                10,
                None,
                None,
                None,
                models::SHORT_TTL_EXTEND_SECS,
                models::MID_TTL_EXTEND_SECS,
                None,
                None,
                false,
                None,
            ) {
                Ok((results, _outcome)) => {
                    for (mem, score) in &results {
                        let _ = writeln!(
                            out.stdout,
                            "  [{}] {} {} score={:.2}",
                            color::tier_color(mem.tier.as_str(), mem.tier.as_str()),
                            color::bold(&mem.title),
                            color::priority_bar(mem.priority),
                            score
                        );
                        let preview: String = mem.content.chars().take(100).collect();
                        let _ = writeln!(out.stdout, "    {}", color::dim(&preview));
                    }
                    let _ = writeln!(out.stdout, "  {} result(s)", results.len());
                }
                Err(e) => {
                    let _ = writeln!(out.stderr, "error: {e}");
                }
            }
        }
        "search" | "s" => {
            let q = parts[1..].join(" ");
            if q.is_empty() {
                let _ = writeln!(out.stderr, "usage: search <query>");
                return ShellAction::Continue;
            }
            match db::search(
                conn, &q, None, None, 20, None, None, None, None, None, None, false,
            ) {
                Ok(results) => {
                    for mem in &results {
                        let _ = writeln!(
                            out.stdout,
                            "  [{}] {} (p={})",
                            color::tier_color(mem.tier.as_str(), mem.tier.as_str()),
                            mem.title,
                            mem.priority
                        );
                    }
                    let _ = writeln!(out.stdout, "  {} result(s)", results.len());
                }
                Err(e) => {
                    let _ = writeln!(out.stderr, "error: {e}");
                }
            }
        }
        "list" | "ls" => {
            let ns = parts.get(1).copied();
            match db::list(conn, ns, None, 20, 0, None, None, None, None, None) {
                Ok(results) => {
                    for mem in &results {
                        let age = human_age(&mem.updated_at);
                        let _ = writeln!(
                            out.stdout,
                            "  [{}] {} (ns={}, {})",
                            color::tier_color(mem.tier.as_str(), mem.tier.as_str()),
                            mem.title,
                            mem.namespace,
                            color::dim(&age)
                        );
                    }
                    let _ = writeln!(out.stdout, "  {} memory(ies)", results.len());
                }
                Err(e) => {
                    let _ = writeln!(out.stderr, "error: {e}");
                }
            }
        }
        "get" => {
            let id = parts.get(1).copied().unwrap_or("");
            if id.is_empty() {
                let _ = writeln!(out.stderr, "usage: get <id>");
                return ShellAction::Continue;
            }
            if let Err(e) = validate::validate_id(id) {
                let _ = writeln!(out.stderr, "invalid id: {e}");
                return ShellAction::Continue;
            }
            match db::get(conn, id) {
                Ok(Some(mem)) => {
                    let _ = writeln!(
                        out.stdout,
                        "{}",
                        serde_json::to_string_pretty(&mem).unwrap_or_default()
                    );
                }
                Ok(None) => {
                    let _ = writeln!(out.stderr, "not found");
                }
                Err(e) => {
                    let _ = writeln!(out.stderr, "error: {e}");
                }
            }
        }
        "update" | "u" => {
            // Issue #653 — REPL parity with the `--profile full` MCP
            // `memory_update` surface. Parses `update <id> field=value
            // [field=value]…` where field ∈ {title, content, tier,
            // namespace, tags, priority, confidence, expires_at}.
            // Honors the same validators the CLI `update` subcommand
            // uses (`crate::validate::*`). Empty `expires_at=` clears
            // the expiry; comma-separated `tags=` splits + trims.
            if parts.len() < 3 {
                let _ = writeln!(
                    out.stderr,
                    "usage: update <id> field=value [field=value]…  (fields: title, content, tier, namespace, tags, priority, confidence, expires_at)"
                );
                return ShellAction::Continue;
            }
            let raw_id = parts[1];
            if let Err(e) = validate::validate_id(raw_id) {
                let _ = writeln!(out.stderr, "invalid id: {e}");
                return ShellAction::Continue;
            }
            let resolved_id = match db::get(conn, raw_id) {
                Ok(Some(_)) => raw_id.to_string(),
                Ok(None) => match db::get_by_prefix(conn, raw_id) {
                    Ok(Some(mem)) => mem.id,
                    Ok(None) => {
                        let _ = writeln!(out.stderr, "not found: {raw_id}");
                        return ShellAction::Continue;
                    }
                    Err(e) => {
                        let _ = writeln!(out.stderr, "error: {e}");
                        return ShellAction::Continue;
                    }
                },
                Err(e) => {
                    let _ = writeln!(out.stderr, "error: {e}");
                    return ShellAction::Continue;
                }
            };
            let mut title: Option<String> = None;
            let mut content: Option<String> = None;
            let mut tier: Option<models::Tier> = None;
            let mut namespace: Option<String> = None;
            let mut tags: Option<Vec<String>> = None;
            let mut priority: Option<i32> = None;
            let mut confidence: Option<f64> = None;
            let mut expires_at: Option<String> = None;
            let mut parse_err: Option<String> = None;
            for kv in &parts[2..] {
                let Some((k, v)) = kv.split_once('=') else {
                    parse_err = Some(format!(
                        "expected key=value, got '{kv}' (e.g. namespace=work)"
                    ));
                    break;
                };
                match k {
                    "title" => title = Some(v.to_string()),
                    "content" => content = Some(v.to_string()),
                    "tier" => match models::Tier::from_str(v) {
                        Some(t) => tier = Some(t),
                        None => {
                            parse_err =
                                Some(format!("invalid tier '{v}' (expected short/mid/long)"));
                            break;
                        }
                    },
                    "namespace" | "ns" => namespace = Some(v.to_string()),
                    "tags" => {
                        tags = Some(
                            v.split(',')
                                .map(|s| s.trim().to_string())
                                .filter(|s| !s.is_empty())
                                .collect(),
                        );
                    }
                    "priority" => match v.parse::<i32>() {
                        Ok(p) => priority = Some(p),
                        Err(_) => {
                            parse_err = Some(format!("invalid priority '{v}' (i32 expected)"));
                            break;
                        }
                    },
                    "confidence" => match v.parse::<f64>() {
                        Ok(c) => confidence = Some(c),
                        Err(_) => {
                            parse_err = Some(format!("invalid confidence '{v}' (0.0..=1.0)"));
                            break;
                        }
                    },
                    "expires_at" => expires_at = Some(v.to_string()),
                    unknown => {
                        parse_err = Some(format!(
                            "unknown field '{unknown}' (one of: title, content, tier, namespace, tags, priority, confidence, expires_at)"
                        ));
                        break;
                    }
                }
            }
            if let Some(e) = parse_err {
                let _ = writeln!(out.stderr, "{e}");
                return ShellAction::Continue;
            }
            if let Some(ref t) = title
                && let Err(e) = validate::validate_title(t)
            {
                let _ = writeln!(out.stderr, "invalid title: {e}");
                return ShellAction::Continue;
            }
            if let Some(ref c) = content
                && let Err(e) = validate::validate_content(c)
            {
                let _ = writeln!(out.stderr, "invalid content: {e}");
                return ShellAction::Continue;
            }
            if let Some(ref ns) = namespace
                && let Err(e) = validate::validate_namespace(ns)
            {
                let _ = writeln!(out.stderr, "invalid namespace: {e}");
                return ShellAction::Continue;
            }
            if let Some(ref tg) = tags
                && let Err(e) = validate::validate_tags(tg)
            {
                let _ = writeln!(out.stderr, "invalid tags: {e}");
                return ShellAction::Continue;
            }
            if let Some(p) = priority
                && let Err(e) = validate::validate_priority(p)
            {
                let _ = writeln!(out.stderr, "invalid priority: {e}");
                return ShellAction::Continue;
            }
            if let Some(c) = confidence
                && let Err(e) = validate::validate_confidence(c)
            {
                let _ = writeln!(out.stderr, "invalid confidence: {e}");
                return ShellAction::Continue;
            }
            if let Some(ref ts) = expires_at
                && !ts.is_empty()
                && let Err(e) = validate::validate_expires_at_format(ts)
            {
                let _ = writeln!(out.stderr, "invalid expires_at: {e}");
                return ShellAction::Continue;
            }
            match db::update(
                conn,
                &resolved_id,
                title.as_deref(),
                content.as_deref(),
                tier.as_ref(),
                namespace.as_deref(),
                tags.as_ref(),
                priority,
                confidence,
                expires_at.as_deref(),
                None,
            ) {
                Ok((true, _)) => {
                    let _ = writeln!(out.stdout, "  updated: {}", color::cyan(&resolved_id));
                }
                Ok((false, _)) => {
                    let _ = writeln!(out.stderr, "  not found");
                }
                Err(e) => {
                    let _ = writeln!(out.stderr, "error: {e}");
                }
            }
        }
        "stats" => match db::stats(conn, Path::new(":memory:")) {
            Ok(s) => {
                let _ = writeln!(out.stdout, "  total: {}, links: {}", s.total, s.links_count);
                for t in &s.by_tier {
                    let _ = writeln!(
                        out.stdout,
                        "    {}: {}",
                        color::tier_color(&t.tier, &t.tier),
                        t.count
                    );
                }
            }
            Err(e) => {
                let _ = writeln!(out.stderr, "error: {e}");
            }
        },
        "namespaces" | "ns" => match db::list_namespaces(conn) {
            Ok(ns) => {
                for n in &ns {
                    let _ = writeln!(out.stdout, "  {}: {}", color::cyan(&n.namespace), n.count);
                }
            }
            Err(e) => {
                let _ = writeln!(out.stderr, "error: {e}");
            }
        },
        "delete" | "del" | "rm" => {
            let id = parts.get(1).copied().unwrap_or("");
            if id.is_empty() {
                let _ = writeln!(out.stderr, "usage: delete <id>");
                return ShellAction::Continue;
            }
            if let Err(e) = validate::validate_id(id) {
                let _ = writeln!(out.stderr, "invalid id: {e}");
                return ShellAction::Continue;
            }
            match db::delete(conn, id) {
                Ok(true) => {
                    let _ = writeln!(out.stdout, "  deleted");
                }
                Ok(false) => {
                    let _ = writeln!(out.stderr, "  not found");
                }
                Err(e) => {
                    let _ = writeln!(out.stderr, "error: {e}");
                }
            }
        }
        unknown => {
            let _ = writeln!(
                out.stderr,
                "unknown command: {unknown}. Type 'help' for commands."
            );
        }
    }
    ShellAction::Continue
}

/// `shell` handler. Outer stdin loop. Not unit-tested — the blocking
/// `read_line` call would deadlock a `Vec<u8>` test fixture; the line
/// handler logic lives in `handle_command`, which is exhaustively tested.
pub fn run(db_path: &Path) -> Result<()> {
    let conn = db::open(db_path)?;
    println!(
        "{}",
        color::bold("ai-memory shell — type 'help' for commands, 'quit' to exit")
    );
    let stdin = std::io::stdin();
    let stdout_handle = std::io::stdout();
    let stderr_handle = std::io::stderr();
    loop {
        eprint!("{} ", color::cyan("memory>"));
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        let mut so = stdout_handle.lock();
        let mut se = stderr_handle.lock();
        let mut out = CliOutput::from_std(&mut so, &mut se);
        let action = handle_command(&parts, &conn, &mut out);
        drop(out);
        if action == ShellAction::Quit {
            break;
        }
    }
    println!("goodbye");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    fn fresh_conn(env: &TestEnv) -> Connection {
        // Seed at least once so the schema is materialised, then reopen.
        seed_memory(&env.db_path, "shell-ns", "seed", "seed-content");
        db::open(&env.db_path).unwrap()
    }

    #[test]
    fn test_shell_quit_command_returns_quit() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(&["quit"], &conn, &mut out);
        assert_eq!(action, ShellAction::Quit);
        let action = handle_command(&["exit"], &conn, &mut out);
        assert_eq!(action, ShellAction::Quit);
        let action = handle_command(&["q"], &conn, &mut out);
        assert_eq!(action, ShellAction::Quit);
    }

    #[test]
    fn test_shell_recall_runs_recall() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(&["recall", "seed"], &conn, &mut out);
        assert_eq!(action, ShellAction::Continue);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("result(s)"));
    }

    #[test]
    fn test_shell_recall_empty_args_writes_usage() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["recall"], &conn, &mut out);
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("usage: recall"));
    }

    #[test]
    fn test_shell_search_runs_search() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(&["search", "seed"], &conn, &mut out);
        assert_eq!(action, ShellAction::Continue);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("result(s)"));
    }

    #[test]
    fn test_shell_help_writes_help_text() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["help"], &conn, &mut out);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("recall"));
        assert!(stdout_str.contains("search"));
        assert!(stdout_str.contains("quit"));
    }

    #[test]
    fn test_shell_unknown_command_writes_error() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(&["frobnicate"], &conn, &mut out);
        assert_eq!(action, ShellAction::Continue);
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("unknown command"));
    }

    #[test]
    fn test_shell_empty_parts_continues() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(&[], &conn, &mut out);
        assert_eq!(action, ShellAction::Continue);
    }

    #[test]
    fn test_shell_list_runs_list() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(&["list"], &conn, &mut out);
        assert_eq!(action, ShellAction::Continue);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("memory(ies)"));
    }

    #[test]
    fn test_shell_namespaces_runs() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(&["namespaces"], &conn, &mut out);
        assert_eq!(action, ShellAction::Continue);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("shell-ns"));
    }

    #[test]
    fn test_shell_get_invalid_id_writes_error() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        // Trigger "id contains invalid characters" via a control character.
        handle_command(&["get", "bad\x07id"], &conn, &mut out);
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("invalid id"), "stderr: {stderr_str}");
    }

    #[test]
    fn test_shell_get_missing_arg_writes_usage() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["get"], &conn, &mut out);
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("usage: get"));
    }

    #[test]
    fn test_shell_delete_missing_arg() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["delete"], &conn, &mut out);
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("usage: delete"));
    }

    // ----------------------------------------------------------------
    // L0.7-3 chunk-e2 — coverage uplift to ≥95%.
    // ----------------------------------------------------------------

    /// Look up a seeded memory id directly so we can drive the
    /// `get`/`delete` happy paths without guessing UUIDs.
    fn lookup_seeded_id(env: &TestEnv) -> String {
        let conn = db::open(&env.db_path).unwrap();
        let all = db::export_all(&conn).unwrap();
        all.first()
            .expect("seed should have inserted one row")
            .id
            .clone()
    }

    #[test]
    fn shell_recall_emits_result_row_with_score() {
        // Drives the recall result-printing branch (lines 67-79). The
        // seed memory's title matches "seed" so we get a hit.
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(&["recall", "seed"], &conn, &mut out);
        assert_eq!(action, ShellAction::Continue);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("score="), "got: {stdout_str}");
        // Result count line at the end.
        assert!(stdout_str.contains("result(s)"));
    }

    #[test]
    fn shell_recall_r_alias_works() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(&["r", "seed"], &conn, &mut out);
        assert_eq!(action, ShellAction::Continue);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("result(s)"));
    }

    #[test]
    fn shell_search_emits_result_row() {
        // Drives the search result-printing branch (lines 94-103).
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(&["search", "seed"], &conn, &mut out);
        assert_eq!(action, ShellAction::Continue);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("p="), "got: {stdout_str}");
        assert!(stdout_str.contains("result(s)"));
    }

    #[test]
    fn shell_search_empty_args_writes_usage() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(&["search"], &conn, &mut out);
        assert_eq!(action, ShellAction::Continue);
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("usage: search"));
    }

    #[test]
    fn shell_list_emits_result_row() {
        // Drives the list result-printing branch (lines 114-125).
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(&["list"], &conn, &mut out);
        assert_eq!(action, ShellAction::Continue);
        let stdout_str = String::from_utf8(stdout).unwrap();
        // Each row carries "ns=" and the trailing count line.
        assert!(stdout_str.contains("ns="), "got: {stdout_str}");
        assert!(stdout_str.contains("memory(ies)"));
    }

    #[test]
    fn shell_list_namespace_filter() {
        // Drives the `parts.get(1)` namespace argument path.
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["list", "shell-ns"], &conn, &mut out);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("shell-ns"));
    }

    #[test]
    fn shell_list_ls_alias_works() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["ls"], &conn, &mut out);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("memory(ies)"));
    }

    #[test]
    fn shell_get_returns_memory_details() {
        // Drives the get(success) JSON-pretty-print branch (lines 143-148).
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let id = lookup_seeded_id(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["get", &id], &conn, &mut out);
        let stdout_str = String::from_utf8(stdout).unwrap();
        // JSON pretty includes the title field literal.
        assert!(stdout_str.contains("\"title\""), "got: {stdout_str}");
        assert!(stdout_str.contains("seed"), "got: {stdout_str}");
    }

    #[test]
    fn shell_get_not_found_writes_stderr() {
        // Drives the get(Ok(None)) branch (line 151).
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        // A syntactically-valid id that does not exist.
        handle_command(
            &["get", "00000000-0000-0000-0000-000000000000"],
            &conn,
            &mut out,
        );
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("not found"));
    }

    #[test]
    fn shell_stats_runs() {
        // Drives the stats success branch (lines 159-168).
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(&["stats"], &conn, &mut out);
        assert_eq!(action, ShellAction::Continue);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("total:"));
    }

    #[test]
    fn shell_delete_success() {
        // Drives the delete(Ok(true)) branch (line 195-197).
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let id = lookup_seeded_id(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["delete", &id], &conn, &mut out);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("deleted"));
    }

    #[test]
    fn shell_delete_not_found_writes_stderr() {
        // Drives the delete(Ok(false)) branch (line 198-200).
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(
            &["delete", "00000000-0000-0000-0000-000000000000"],
            &conn,
            &mut out,
        );
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("not found"));
    }

    #[test]
    fn shell_delete_invalid_id() {
        // Drives the validate_id-error branch on delete (line 191-192).
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["delete", "bad\x07id"], &conn, &mut out);
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("invalid id"));
    }

    #[test]
    fn shell_help_h_alias() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["h"], &conn, &mut out);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("recall"));
    }

    #[test]
    fn shell_namespaces_ns_alias() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["ns"], &conn, &mut out);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("shell-ns"));
    }

    /// Pipe-driven stdin redirect for unit-testing `run()`. Writes
    /// `lines` to a pipe, dup2s the pipe over fd 0 for the duration
    /// of `f`, then restores the original stdin. Unix-only (the
    /// playbook explicitly forbids modifying the CLI surface, so we
    /// stretch the test harness instead).
    #[cfg(unix)]
    fn with_stdin_lines<R>(lines: &str, f: impl FnOnce() -> R) -> R {
        use std::os::unix::io::AsRawFd;
        use std::sync::Mutex;
        static STDIN_LOCK: Mutex<()> = Mutex::new(());
        let _g = STDIN_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // Build a pipe; write the lines into the write end then close it
        // so the read end yields EOF after the buffered content is drained.
        let mut fds: [libc::c_int; 2] = [0; 2];
        unsafe {
            assert_eq!(libc::pipe(fds.as_mut_ptr()), 0, "pipe()");
        }
        let read_fd = fds[0];
        let write_fd = fds[1];
        unsafe {
            let bytes = lines.as_bytes();
            let written = libc::write(write_fd, bytes.as_ptr().cast(), bytes.len());
            assert_eq!(written, bytes.len() as isize, "write to pipe");
            libc::close(write_fd);
        }

        // Snapshot stdin's current fd and dup2 the read end over fd 0.
        let stdin = std::io::stdin();
        let stdin_fd = stdin.as_raw_fd();
        let saved = unsafe { libc::dup(stdin_fd) };
        assert!(saved >= 0, "save stdin fd");
        unsafe {
            assert_eq!(libc::dup2(read_fd, stdin_fd), stdin_fd, "dup2");
            libc::close(read_fd);
        }

        let r = f();

        // Restore stdin.
        unsafe {
            libc::dup2(saved, stdin_fd);
            libc::close(saved);
        }
        r
    }

    #[cfg(unix)]
    #[test]
    fn shell_run_with_quit_line_returns_cleanly() {
        // Feeds a single "quit\n" line through stdin, then EOF. The
        // REPL must call handle_command which returns ShellAction::Quit
        // and break.
        let env = TestEnv::fresh();
        seed_memory(&env.db_path, "shell-run-ns", "seed", "content");
        let db = env.db_path.clone();
        let r = with_stdin_lines("quit\n", || run(&db));
        assert!(r.is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn shell_run_with_help_then_quit() {
        // Two-line input drives both `read_line` (twice) and
        // handle_command (twice).
        let env = TestEnv::fresh();
        seed_memory(&env.db_path, "shell-run-ns", "seed", "content");
        let db = env.db_path.clone();
        let r = with_stdin_lines("help\nquit\n", || run(&db));
        assert!(r.is_ok());
    }

    #[test]
    fn shell_run_with_eof_stdin_returns_cleanly() {
        // The outer REPL `run()` reads from process stdin. Under
        // `cargo test`, stdin is connected to `/dev/null` (which yields
        // EOF on first read) on every host this codebase is tested on
        // (macOS, Linux CI), so the read_line loop short-circuits to
        // `Ok(())` without ever blocking.
        //
        // This is the only viable unit-test path for `run()`: the
        // function's I/O contract is hard-wired to `std::io::stdin()`
        // / `std::io::stdout()` / `std::io::stderr()` and we are not
        // allowed to refactor it for testability (see playbook §1
        // "do not modify CLI clap definitions").
        //
        // If a future CI introduces a stdin that does not yield EOF
        // (e.g. an interactive harness) this test will hang. The fix
        // is to gate it behind `#[cfg(target_family = "unix")]` and
        // pipe `/dev/null` to stdin explicitly via `nix::dup2`.
        let env = TestEnv::fresh();
        // Seed first so db::open finds an existing schema.
        seed_memory(&env.db_path, "shell-run-ns", "seed", "content");
        // We can't capture stdout/stderr from `println!`/`eprint!` here,
        // and `run()` blocks if stdin doesn't EOF. Under cargo test,
        // stdin is /dev/null which yields EOF immediately. If this
        // test hangs in CI, mark it with `#[ignore]` and exercise the
        // REPL via an integration test that spawns the binary.
        let r = run(&env.db_path);
        assert!(r.is_ok());
    }

    #[test]
    fn shell_delete_aliases() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let id = lookup_seeded_id(&env);
        // `del` alias.
        {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
            handle_command(&["del", &id], &conn, &mut out);
            assert!(String::from_utf8(stdout).unwrap().contains("deleted"));
        }
        // Re-seed for the second alias.
        seed_memory(&env.db_path, "shell-ns", "seed2", "seed-content-2");
        let conn2 = db::open(&env.db_path).unwrap();
        let id2 = {
            let all = db::export_all(&conn2).unwrap();
            all.iter().find(|m| m.title == "seed2").unwrap().id.clone()
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["rm", &id2], &conn2, &mut out);
        assert!(String::from_utf8(stdout).unwrap().contains("deleted"));
    }

    // ----------------------------------------------------------------
    // Issue #653 — REPL `update` parity with `--profile full`
    // `memory_update`. The CLI subcommand always existed; the REPL
    // didn't, forcing operators into raw MCP JSON-RPC. These tests
    // pin the parsing surface + the dispatch path against db::update.
    // ----------------------------------------------------------------

    #[test]
    fn shell_update_changes_namespace() {
        // Headline use case from the issue: "switch a memory's
        // namespace … via the REPL or the CLI".
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let id = lookup_seeded_id(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(&["update", &id, "namespace=migrated"], &conn, &mut out);
        assert_eq!(action, ShellAction::Continue);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("updated:"), "stdout: {stdout_str}");
        let mem = db::get(&conn, &id).unwrap().unwrap();
        assert_eq!(mem.namespace, "migrated");
    }

    #[test]
    fn shell_update_multiple_fields_one_call() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let id = lookup_seeded_id(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let action = handle_command(
            &[
                "update",
                &id,
                "title=renamed",
                "priority=9",
                "confidence=0.9",
            ],
            &conn,
            &mut out,
        );
        assert_eq!(action, ShellAction::Continue);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("updated:"), "stdout: {stdout_str}");
        let mem = db::get(&conn, &id).unwrap().unwrap();
        assert_eq!(mem.title, "renamed");
        assert_eq!(mem.priority, 9);
        assert!((mem.confidence - 0.9).abs() < f64::EPSILON);
    }

    #[test]
    fn shell_update_short_alias_u_works() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let id = lookup_seeded_id(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["u", &id, "namespace=via-alias"], &conn, &mut out);
        let mem = db::get(&conn, &id).unwrap().unwrap();
        assert_eq!(mem.namespace, "via-alias");
    }

    #[test]
    fn shell_update_missing_args_writes_usage() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["update"], &conn, &mut out);
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("usage: update"));
    }

    #[test]
    fn shell_update_missing_kv_writes_usage() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let id = lookup_seeded_id(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["update", &id], &conn, &mut out);
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("usage: update"));
    }

    #[test]
    fn shell_update_unknown_field_errors() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let id = lookup_seeded_id(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["update", &id, "frobnitz=value"], &conn, &mut out);
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("unknown field"), "stderr: {stderr_str}");
    }

    #[test]
    fn shell_update_malformed_kv_errors() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let id = lookup_seeded_id(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["update", &id, "no-equals-sign"], &conn, &mut out);
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(
            stderr_str.contains("expected key=value"),
            "stderr: {stderr_str}"
        );
    }

    #[test]
    fn shell_update_invalid_tier_errors() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let id = lookup_seeded_id(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["update", &id, "tier=archived"], &conn, &mut out);
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("invalid tier"), "stderr: {stderr_str}");
    }

    #[test]
    fn shell_update_invalid_id_errors() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["update", "bad\x07id", "namespace=foo"], &conn, &mut out);
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("invalid id"), "stderr: {stderr_str}");
    }

    #[test]
    fn shell_update_nonexistent_id_writes_not_found() {
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        // Plausible UUID format that won't resolve.
        let fake = "deadbeef-dead-beef-dead-beefdeadbeef";
        handle_command(&["update", fake, "namespace=foo"], &conn, &mut out);
        let stderr_str = String::from_utf8(stderr).unwrap();
        assert!(stderr_str.contains("not found"), "stderr: {stderr_str}");
    }

    #[test]
    fn shell_help_lists_update_command() {
        // Pin the help text for issue #653 so future help-text edits
        // don't silently drop the update entry.
        let env = TestEnv::fresh();
        let conn = fresh_conn(&env);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        handle_command(&["help"], &conn, &mut out);
        let stdout_str = String::from_utf8(stdout).unwrap();
        assert!(stdout_str.contains("update <id>"), "help: {stdout_str}");
        assert!(stdout_str.contains("#653"), "help: {stdout_str}");
    }
}
