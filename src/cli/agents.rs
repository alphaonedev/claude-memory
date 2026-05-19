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

    // ---------- E1 coverage uplift: register-json + pending-with-items
    // + approve happy + reject happy + consensus pending. The
    // `process::exit` branches (Approve::Rejected, Reject not-found) stay
    // uncovered intentionally — they call `std::process::exit(1)` which
    // would terminate the test process.

    #[test]
    fn test_agents_register_json_output() {
        // Covers the `if json_out` arm inside Register (lines 112-123)
        // which is not exercised by `test_agents_register_happy_path`.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = AgentsArgs {
            action: Some(AgentsAction::Register {
                agent_id: "agent-json".to_string(),
                agent_type: "human".to_string(),
                capabilities: "x,y,z".to_string(),
            }),
        };
        {
            let mut out = env.output();
            run_agents(&db, args, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["registered"].as_bool().unwrap(), true);
        assert_eq!(v["agent_id"].as_str().unwrap(), "agent-json");
        assert_eq!(v["agent_type"].as_str().unwrap(), "human");
        // Capabilities round-trip as a JSON array of length 3.
        assert_eq!(v["capabilities"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn test_agents_register_empty_caps_human_text_dash() {
        // Hits the `if caps.is_empty()` true branch in the text-output
        // path (line 128 → "-").
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = AgentsArgs {
            action: Some(AgentsAction::Register {
                agent_id: "agent-no-caps".to_string(),
                agent_type: "system".to_string(),
                capabilities: String::new(),
            }),
        };
        {
            let mut out = env.output();
            run_agents(&db, args, false, &mut out).unwrap();
        }
        // The "-" sentinel appears when capabilities is empty.
        assert!(env.stdout_str().contains("capabilities=-"));
    }

    #[test]
    fn test_agents_list_with_registered_agent_text_includes_caps() {
        // Drives the for-loop body (lines 82-94) — including the
        // `caps.is_empty() == false` branch where capabilities are
        // printed `[a,b]`.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let reg = AgentsArgs {
            action: Some(AgentsAction::Register {
                agent_id: "agent-with-caps".to_string(),
                agent_type: "ai:claude-opus-4.7".to_string(),
                capabilities: "alpha,beta".to_string(),
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
        assert!(s.contains("agent-with-caps"));
        assert!(s.contains("type=ai:claude-opus-4.7"));
        assert!(s.contains("[alpha,beta]"));
        assert!(s.contains("1 registered agents"));
    }

    #[test]
    fn test_agents_list_json_with_items() {
        // Drives the JSON branch of list when there *are* agents
        // (lines 73-78) with a non-empty agents array.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let reg = AgentsArgs {
            action: Some(AgentsAction::Register {
                agent_id: "agent-jsonlist".to_string(),
                agent_type: "human".to_string(),
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
            run_agents(&db, list, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["count"].as_u64().unwrap(), 1);
        assert_eq!(
            v["agents"][0]["agent_id"].as_str().unwrap(),
            "agent-jsonlist"
        );
    }

    // ---- Pending list-with-items + decision paths -----------------

    /// Seed one `pending_actions` row directly via SQL. The CLI's
    /// `Approve` arm reads & writes through `db::*` helpers which
    /// validate this shape.
    fn seed_pending_action(
        db_path: &std::path::Path,
        id: &str,
        ns: &str,
        action_type: &str,
        requested_by: &str,
    ) {
        use rusqlite::params;
        let conn = db::open(db_path).expect("db::open");
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO pending_actions \
             (id, action_type, namespace, payload, requested_by, requested_at, status) \
             VALUES (?1, ?2, ?3, '{}', ?4, ?5, 'pending')",
            params![id, action_type, ns, requested_by, now],
        )
        .expect("insert pending_actions");
    }

    #[test]
    fn test_pending_list_text_with_items() {
        // Hits the for-loop body (lines 161-171) + count footer (line 173).
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_pending_action(&db, "pa-1", "ns-x", "store", "test-agent");
        seed_pending_action(&db, "pa-2", "ns-y", "delete", "test-agent");
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
        let s = env.stdout_str();
        assert!(s.contains("pa-1") || s.contains("pa-2"));
        assert!(s.contains("pending action"));
    }

    #[test]
    fn test_pending_list_json_with_items() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_pending_action(&db, "pa-json-1", "ns-x", "store", "test-agent");
        let args = PendingArgs {
            action: PendingAction::List {
                status: None,
                limit: 100,
            },
        };
        {
            let mut out = env.output();
            run_pending(&db, args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["count"].as_u64().unwrap(), 1);
        assert!(v["pending"].is_array());
    }

    /// Seed a `delete`-shaped pending action whose memory_id is a real,
    /// existing memory. `execute_pending_action`'s delete arm reads
    /// `pa.memory_id` (the dedicated column, not the payload) and calls
    /// `db::delete`. With a valid target row, execution succeeds and the
    /// CLI's Approved arm reaches the "approved + executed" branch.
    fn seed_delete_pending(db_path: &std::path::Path, pa_id: &str, ns: &str) -> String {
        use rusqlite::params;
        let target = seed_memory_local(db_path, ns, &format!("t-{pa_id}"), "c");
        let conn = db::open(db_path).expect("db::open");
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO pending_actions \
             (id, action_type, memory_id, namespace, payload, requested_by, requested_at, status) \
             VALUES (?1, 'delete', ?2, ?3, '{}', 'test-agent', ?4, 'pending')",
            params![pa_id, target, ns, now],
        )
        .expect("seed pending");
        target
    }

    #[test]
    fn test_pending_approve_happy_text() {
        // Default namespace policy (no governance row) → approver = Human →
        // `approve_with_approver_type` writes `Approved` and the CLI's
        // Approved arm calls `execute_pending_action`. With action_type=delete
        // and a valid memory_id, the delete arm succeeds and we hit the
        // "approved + executed" line.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_delete_pending(&db, "pa-approve-1", "ns-app");
        let args = PendingArgs {
            action: PendingAction::Approve {
                id: "pa-approve-1".to_string(),
            },
        };
        {
            let mut out = env.output();
            run_pending(&db, args, false, Some("test-agent"), &mut out).unwrap();
        }
        let s = env.stdout_str();
        assert!(
            s.contains("approved + executed: pa-approve-1"),
            "expected approved+executed line, got: {s}"
        );
    }

    #[test]
    fn test_pending_approve_happy_json() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_delete_pending(&db, "pa-approve-json", "ns-app2");
        let args = PendingArgs {
            action: PendingAction::Approve {
                id: "pa-approve-json".to_string(),
            },
        };
        {
            let mut out = env.output();
            run_pending(&db, args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["approved"].as_bool().unwrap(), true);
        assert_eq!(v["id"].as_str().unwrap(), "pa-approve-json");
        assert_eq!(v["decided_by"].as_str().unwrap(), "test-agent");
    }

    #[test]
    fn test_pending_reject_happy_text() {
        // Happy `Reject` text path (lines 226-245).
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_pending_action(&db, "pa-reject-1", "ns-r", "store", "test-agent");
        let args = PendingArgs {
            action: PendingAction::Reject {
                id: "pa-reject-1".to_string(),
            },
        };
        {
            let mut out = env.output();
            run_pending(&db, args, false, Some("test-agent"), &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("rejected: pa-reject-1"));
    }

    #[test]
    fn test_pending_reject_happy_json() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_pending_action(&db, "pa-reject-j", "ns-r", "store", "test-agent");
        let args = PendingArgs {
            action: PendingAction::Reject {
                id: "pa-reject-j".to_string(),
            },
        };
        {
            let mut out = env.output();
            run_pending(&db, args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["rejected"].as_bool().unwrap(), true);
        assert_eq!(v["id"].as_str().unwrap(), "pa-reject-j");
        assert_eq!(v["decided_by"].as_str().unwrap(), "test-agent");
    }

    /// Install a Consensus(2) governance policy on `namespace`. The
    /// policy lives inside a "standard" memory's metadata; we seed the
    /// memory then point `namespace_meta` at it.
    fn install_consensus_policy(db_path: &std::path::Path, namespace: &str, quorum: u32) {
        let conn = db::open(db_path).expect("db::open");
        let policy = serde_json::json!({
            "write": "approve",
            "promote": "any",
            "delete": "owner",
            "approver": {"consensus": quorum},
            "inherit": true,
        });
        let now = chrono::Utc::now().to_rfc3339();
        let mut metadata = crate::models::default_metadata();
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert(
                "agent_id".to_string(),
                serde_json::Value::String("test-agent".to_string()),
            );
            obj.insert("governance".to_string(), policy);
        }
        let mem = crate::models::Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: crate::models::Tier::Long,
            namespace: namespace.to_string(),
            title: format!("standard:{namespace}"),
            content: "policy standard".to_string(),
            tags: vec![],
            priority: 9,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata,
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
            confidence_source: crate::models::ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
            version: 1,
        };
        let id = db::insert(&conn, &mem).expect("db::insert standard");
        db::set_namespace_standard(&conn, namespace, &id, None).expect("set_namespace_standard");
    }

    #[test]
    fn test_pending_approve_consensus_pending_branch() {
        // Drives the `ApproveOutcome::Pending { votes, quorum }` arm
        // (lines 199-219). Path:
        //   1. Register two agents so they qualify as consensus voters.
        //   2. Set a namespace standard whose policy demands Consensus(2).
        //   3. Seed a pending action under that namespace.
        //   4. Have agent A approve — quorum not met → Pending response.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();

        // Step 1: register voters.
        for who in ["voter-a", "voter-b"] {
            let reg = AgentsArgs {
                action: Some(AgentsAction::Register {
                    agent_id: who.to_string(),
                    agent_type: "human".to_string(),
                    capabilities: String::new(),
                }),
            };
            let mut out = env.output();
            run_agents(&db, reg, false, &mut out).expect("register voter");
        }
        env.stdout.clear();

        // Step 2: install a Consensus(2) policy via the standard
        // memory + namespace_meta path.
        install_consensus_policy(&db, "ns-cons", 2);

        // Step 3: seed a pending action.
        seed_pending_action(&db, "pa-cons-1", "ns-cons", "store", "voter-a");

        // Step 4: voter-a approves.
        let args = PendingArgs {
            action: PendingAction::Approve {
                id: "pa-cons-1".to_string(),
            },
        };
        {
            let mut out = env.output();
            run_pending(&db, args, false, Some("voter-a"), &mut out).expect("approve voter-a");
        }
        // Text branch — "approval recorded".
        assert!(
            env.stdout_str().contains("approval recorded: pa-cons-1"),
            "expected `approval recorded` text, got: {}",
            env.stdout_str()
        );
    }

    #[test]
    fn test_pending_approve_consensus_pending_json() {
        // JSON variant of the same path (lines 200-212).
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        for who in ["voter-a", "voter-b"] {
            let reg = AgentsArgs {
                action: Some(AgentsAction::Register {
                    agent_id: who.to_string(),
                    agent_type: "human".to_string(),
                    capabilities: String::new(),
                }),
            };
            let mut out = env.output();
            run_agents(&db, reg, false, &mut out).expect("register voter");
        }
        env.stdout.clear();
        install_consensus_policy(&db, "ns-cons-j", 2);
        seed_pending_action(&db, "pa-cons-j", "ns-cons-j", "store", "voter-a");
        let args = PendingArgs {
            action: PendingAction::Approve {
                id: "pa-cons-j".to_string(),
            },
        };
        {
            let mut out = env.output();
            run_pending(&db, args, true, Some("voter-a"), &mut out).expect("approve voter-a");
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["approved"].as_bool().unwrap(), false);
        assert_eq!(v["status"].as_str().unwrap(), "pending");
        assert_eq!(v["quorum"].as_u64().unwrap(), 2);
    }

    #[test]
    fn test_pending_reject_invalid_id_validation_error() {
        // validate_id rejects an obviously-invalid id (empty / contains
        // disallowed chars). The CLI returns the error via `?`.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = PendingArgs {
            action: PendingAction::Reject { id: String::new() },
        };
        let mut out = env.output();
        let res = run_pending(&db, args, false, Some("test-agent"), &mut out);
        assert!(res.is_err());
    }

    #[test]
    fn test_pending_approve_invalid_id_validation_error() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = PendingArgs {
            action: PendingAction::Approve { id: String::new() },
        };
        let mut out = env.output();
        let res = run_pending(&db, args, false, Some("test-agent"), &mut out);
        assert!(res.is_err());
    }

    // Local seed helper — duplicated from cli::test_utils so we can
    // bind a specific id without changing the shared signature.
    fn seed_memory_local(
        db_path: &std::path::Path,
        ns: &str,
        title: &str,
        content: &str,
    ) -> String {
        crate::cli::test_utils::seed_memory(db_path, ns, title, content)
    }
}
