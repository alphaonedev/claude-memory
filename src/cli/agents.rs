// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_agents` and `cmd_pending` migrations. See `cli::store` for the
//! design pattern.

use crate::cli::CliOutput;
use crate::cli::helpers::id_short;
use crate::{db, identity, validate};
use anyhow::Result;
use clap::{Args, Subcommand};
use std::path::Path;

#[derive(Args)]
pub struct AgentsArgs {
    #[command(subcommand)]
    pub action: Option<AgentsAction>,
}

#[derive(Subcommand)]
pub enum AgentsAction {
    /// List registered agents (default)
    List,
    /// Register or refresh an agent
    Register {
        /// Agent identifier
        #[arg(long)]
        agent_id: String,
        /// Agent type. Curated values: human, system, ai:claude-opus-4.6,
        /// ai:claude-opus-4.7, ai:codex-5.4, ai:grok-4.2. Any `ai:<name>`
        /// form is also accepted (e.g. `ai:gpt-5`, `ai:gemini-2.5`) —
        /// red-team #235.
        #[arg(long)]
        agent_type: String,
        /// Comma-separated capability tags
        #[arg(long, default_value = "")]
        capabilities: String,
    },
}

#[derive(Args)]
pub struct PendingArgs {
    #[command(subcommand)]
    pub action: PendingAction,
}

#[derive(Subcommand)]
pub enum PendingAction {
    /// List pending actions (optionally filter by status).
    List {
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Approve a pending action by id.
    Approve { id: String },
    /// Reject a pending action by id.
    Reject { id: String },
}

/// `agents` handler.
pub fn run_agents(
    db_path: &Path,
    args: AgentsArgs,
    json_out: bool,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let conn = db::open(db_path)?;
    match args.action.unwrap_or(AgentsAction::List) {
        AgentsAction::List => {
            let agents = db::list_agents(&conn)?;
            if json_out {
                writeln!(
                    out.stdout,
                    "{}",
                    serde_json::json!({"count": agents.len(), "agents": agents})
                )?;
            } else if agents.is_empty() {
                writeln!(out.stdout, "no registered agents")?;
            } else {
                for a in &agents {
                    let caps = if a.capabilities.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", a.capabilities.join(","))
                    };
                    writeln!(
                        out.stdout,
                        "{}  type={}  registered={}  last_seen={}{}",
                        a.agent_id, a.agent_type, a.registered_at, a.last_seen_at, caps
                    )?;
                }
                writeln!(out.stdout, "{} registered agents", agents.len())?;
            }
        }
        AgentsAction::Register {
            agent_id,
            agent_type,
            capabilities,
        } => {
            validate::validate_agent_id(&agent_id)?;
            validate::validate_agent_type(&agent_type)?;
            let caps: Vec<String> = capabilities
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            validate::validate_capabilities(&caps)?;
            let id = db::register_agent(&conn, &agent_id, &agent_type, &caps)?;
            if json_out {
                writeln!(
                    out.stdout,
                    "{}",
                    serde_json::json!({
                        "registered": true,
                        "id": id,
                        "agent_id": agent_id,
                        "agent_type": agent_type,
                        "capabilities": caps,
                    })
                )?;
            } else {
                writeln!(
                    out.stdout,
                    "registered {agent_id} (type={agent_type}, capabilities={})",
                    if caps.is_empty() {
                        "-".to_string()
                    } else {
                        caps.join(",")
                    }
                )?;
            }
        }
    }
    Ok(())
}

/// `pending` handler.
pub fn run_pending(
    db_path: &Path,
    args: PendingArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let conn = db::open(db_path)?;
    match args.action {
        PendingAction::List { status, limit } => {
            let items = db::list_pending_actions(&conn, status.as_deref(), limit)?;
            if json_out {
                writeln!(
                    out.stdout,
                    "{}",
                    serde_json::json!({"count": items.len(), "pending": items})
                )?;
            } else if items.is_empty() {
                writeln!(out.stdout, "no pending actions")?;
            } else {
                for item in &items {
                    writeln!(
                        out.stdout,
                        "[{}] {} ns={} action={} by={} ({})",
                        id_short(&item.id),
                        item.status,
                        item.namespace,
                        item.action_type,
                        item.requested_by,
                        item.requested_at
                    )?;
                }
                writeln!(out.stdout, "{} pending action(s)", items.len())?;
            }
        }
        PendingAction::Approve { id } => {
            use db::ApproveOutcome;
            validate::validate_id(&id)?;
            let agent = identity::resolve_agent_id(cli_agent_id, None)?;
            match db::approve_with_approver_type(&conn, &id, &agent)? {
                ApproveOutcome::Approved => {
                    let executed = db::execute_pending_action(&conn, &id)?;
                    if json_out {
                        writeln!(
                            out.stdout,
                            "{}",
                            serde_json::json!({
                                "approved": true,
                                "id": id,
                                "decided_by": agent,
                                "executed": true,
                                "memory_id": executed,
                            })
                        )?;
                    } else {
                        writeln!(out.stdout, "approved + executed: {id} (by {agent})")?;
                    }
                }
                ApproveOutcome::Pending { votes, quorum } => {
                    if json_out {
                        writeln!(
                            out.stdout,
                            "{}",
                            serde_json::json!({
                                "approved": false,
                                "status": "pending",
                                "id": id,
                                "votes": votes,
                                "quorum": quorum,
                                "reason": "consensus threshold not yet reached",
                            })
                        )?;
                    } else {
                        writeln!(
                            out.stdout,
                            "approval recorded: {id} ({votes}/{quorum} consensus, not yet met)"
                        )?;
                    }
                }
                ApproveOutcome::Rejected(reason) => {
                    writeln!(out.stderr, "approve rejected: {reason}")?;
                    std::process::exit(1);
                }
            }
        }
        PendingAction::Reject { id } => {
            validate::validate_id(&id)?;
            let agent = identity::resolve_agent_id(cli_agent_id, None)?;
            let ok = db::decide_pending_action(&conn, &id, false, &agent)?;
            if !ok {
                writeln!(
                    out.stderr,
                    "pending action not found or already decided: {id}"
                )?;
                std::process::exit(1);
            }
            if json_out {
                writeln!(
                    out.stdout,
                    "{}",
                    serde_json::json!({"rejected": true, "id": id, "decided_by": agent})
                )?;
            } else {
                writeln!(out.stdout, "rejected: {id} (by {agent})")?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::TestEnv;

    #[test]
    fn test_agents_list_empty() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = AgentsArgs {
            action: Some(AgentsAction::List),
        };
        {
            let mut out = env.output();
            run_agents(&db, args, false, &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("no registered agents"));
    }

    #[test]
    fn test_agents_list_empty_json() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = AgentsArgs {
            action: Some(AgentsAction::List),
        };
        {
            let mut out = env.output();
            run_agents(&db, args, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["count"].as_u64().unwrap(), 0);
    }

    #[test]
    fn test_agents_register_happy_path() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = AgentsArgs {
            action: Some(AgentsAction::Register {
                agent_id: "agent-1".to_string(),
                agent_type: "human".to_string(),
                capabilities: "alpha,beta".to_string(),
            }),
        };
        {
            let mut out = env.output();
            run_agents(&db, args, false, &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("registered agent-1"));
    }

    #[test]
    fn test_agents_register_then_list() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let reg = AgentsArgs {
            action: Some(AgentsAction::Register {
                agent_id: "agent-2".to_string(),
                agent_type: "system".to_string(),
                capabilities: String::new(),
            }),
        };
        {
            let mut out = env.output();
            run_agents(&db, reg, false, &mut out).unwrap();
        }
        env.stdout.clear();
        env.stderr.clear();
        let list = AgentsArgs {
            action: Some(AgentsAction::List),
        };
        {
            let mut out = env.output();
            run_agents(&db, list, false, &mut out).unwrap();
        }
        let s = env.stdout_str();
        assert!(s.contains("agent-2"));
        assert!(s.contains("type=system"));
    }

    #[test]
    fn test_agents_register_invalid_agent_id() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = AgentsArgs {
            action: Some(AgentsAction::Register {
                agent_id: String::new(), // empty -> validation error
                agent_type: "human".to_string(),
                capabilities: String::new(),
            }),
        };
        let mut out = env.output();
        let res = run_agents(&db, args, false, &mut out);
        assert!(res.is_err());
    }

    #[test]
    fn test_agents_default_action_is_list() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = AgentsArgs { action: None };
        {
            let mut out = env.output();
            run_agents(&db, args, false, &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("no registered agents"));
    }

    #[test]
    fn test_pending_list_empty() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = PendingArgs {
            action: PendingAction::List {
                status: None,
                limit: 100,
            },
        };
        {
            let mut out = env.output();
            run_pending(&db, args, false, Some("test-agent"), &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("no pending actions"));
    }

    #[test]
    fn test_pending_list_empty_json() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = PendingArgs {
            action: PendingAction::List {
                status: Some("pending".to_string()),
                limit: 100,
            },
        };
        {
            let mut out = env.output();
            run_pending(&db, args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["count"].as_u64().unwrap(), 0);
    }
}
