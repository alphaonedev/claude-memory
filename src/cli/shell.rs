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
            ) {
                Ok((results, _tokens_used)) => {
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
            match db::search(conn, &q, None, None, 20, None, None, None, None, None, None) {
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
}
