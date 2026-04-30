// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_export`, `cmd_import`, `cmd_mine` migrations.

use crate::cli::CliOutput;
use crate::{config, db, identity, mine, models, validate};
use anyhow::Result;
use chrono::{Duration, Utc};
use clap::Args;
use models::Tier;
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct ImportArgs {
    /// Trust `metadata.agent_id` in imported JSON (default: restamp with caller's id).
    /// Only use this when importing a JSON export you fully trust (e.g., your own backup).
    #[arg(long, default_value_t = false)]
    pub trust_source: bool,
}

#[derive(Args)]
pub struct MineArgs {
    /// Path to the export file or directory
    pub path: PathBuf,
    /// Export format: claude, chatgpt, slack
    #[arg(long, short)]
    pub format: String,
    /// Namespace for imported memories (auto-detected if omitted)
    #[arg(long, short)]
    pub namespace: Option<String>,
    /// Memory tier for imported memories
    #[arg(long, short, default_value = "mid")]
    pub tier: String,
    /// Minimum message count to import a conversation
    #[arg(long, default_value_t = 3)]
    pub min_messages: usize,
    /// Dry run — show what would be imported without writing
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

/// `export` handler. Dumps every memory + link as pretty JSON.
pub fn export(db_path: &Path, out: &mut CliOutput<'_>) -> Result<()> {
    let conn = db::open(db_path)?;
    let memories = db::export_all(&conn)?;
    let links = db::export_links(&conn)?;
    writeln!(
        out.stdout,
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "memories": memories, "links": links, "count": memories.len(),
            "exported_at": Utc::now().to_rfc3339(),
        }))?
    )?;
    Ok(())
}

/// `import` handler. Reads JSON from `import_reader` (defaulting to
/// stdin in production) and inserts into the DB.
pub fn import(
    db_path: &Path,
    args: &ImportArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let mut buf = String::new();
    use std::io::Read as _;
    std::io::stdin().read_to_string(&mut buf)?;
    import_from_str(&buf, db_path, args, json_out, cli_agent_id, out)
}

/// Stdin-decoupled half of `import`. Tests call this directly with a
/// literal payload instead of redirecting the process's stdin.
pub(crate) fn import_from_str(
    payload: &str,
    db_path: &Path,
    args: &ImportArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let data: serde_json::Value = serde_json::from_str(payload)?;
    let memories: Vec<models::Memory> =
        serde_json::from_value(data.get("memories").cloned().unwrap_or_default())?;
    let links: Vec<models::MemoryLink> =
        serde_json::from_value(data.get("links").cloned().unwrap_or_default()).unwrap_or_default();

    let caller_id = identity::resolve_agent_id(cli_agent_id, None)?;

    let conn = db::open(db_path)?;
    let mut imported = 0usize;
    let mut restamped = 0usize;
    let mut errors = Vec::new();
    for mut mem in memories {
        if !args.trust_source {
            let original = mem
                .metadata
                .get("agent_id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string);
            if let Some(obj) = mem.metadata.as_object_mut() {
                obj.insert(
                    "agent_id".to_string(),
                    serde_json::Value::String(caller_id.clone()),
                );
                if let Some(orig) = original.as_ref()
                    && orig.as_str() != caller_id
                {
                    obj.insert(
                        "imported_from_agent_id".to_string(),
                        serde_json::Value::String(orig.clone()),
                    );
                    restamped += 1;
                }
            }
        }
        if let Err(e) = validate::validate_memory(&mem) {
            errors.push(format!("{}: {}", mem.id, e));
            continue;
        }
        match db::insert(&conn, &mem) {
            Ok(_) => imported += 1,
            Err(e) => errors.push(format!("{}: {}", mem.id, e)),
        }
    }
    for link in links {
        if validate::validate_link(&link.source_id, &link.target_id, &link.relation).is_err() {
            continue;
        }
        let _ = db::create_link(&conn, &link.source_id, &link.target_id, &link.relation);
    }
    if json_out {
        writeln!(
            out.stdout,
            "{}",
            serde_json::json!({
                "imported": imported,
                "restamped": restamped,
                "trusted_source": args.trust_source,
                "errors": errors
            })
        )?;
    } else {
        writeln!(
            out.stdout,
            "imported: {imported} (restamped agent_id on {restamped})"
        )?;
        if args.trust_source {
            writeln!(
                out.stderr,
                "warning: --trust-source: agent_id from imported JSON was preserved as-is"
            )?;
        }
        if !errors.is_empty() {
            for e in &errors {
                writeln!(out.stderr, "  {e}")?;
            }
        }
    }
    Ok(())
}

/// `mine` handler.
#[allow(clippy::too_many_lines)]
pub fn mine(
    db_path: &Path,
    args: MineArgs,
    json_out: bool,
    app_config: &config::AppConfig,
    cli_agent_id: Option<&str>,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let miner_agent_id = identity::resolve_agent_id(cli_agent_id, None)?;
    let format = mine::Format::from_str(&args.format).ok_or_else(|| {
        anyhow::anyhow!(
            "invalid format: {} (use claude, chatgpt, slack)",
            args.format
        )
    })?;
    let tier = Tier::from_str(&args.tier)
        .ok_or_else(|| anyhow::anyhow!("invalid tier: {} (use short, mid, long)", args.tier))?;
    let namespace = args.namespace.unwrap_or_else(|| match format {
        mine::Format::Claude => "claude-export".to_string(),
        mine::Format::ChatGpt => "chatgpt-export".to_string(),
        mine::Format::Slack => "slack-export".to_string(),
    });

    let path = std::path::Path::new(&args.path);

    let conversations = match format {
        mine::Format::Claude => mine::parse_claude(path)?,
        mine::Format::ChatGpt => mine::parse_chatgpt(path)?,
        mine::Format::Slack => mine::parse_slack(path)?,
    };

    let filtered: Vec<_> = conversations
        .iter()
        .filter(|c| c.messages.len() >= args.min_messages)
        .collect();

    if args.dry_run {
        if json_out {
            let items: Vec<serde_json::Value> = filtered
                .iter()
                .filter_map(|c| {
                    mine::conversation_to_memory(c, format).map(|m| {
                        serde_json::json!({
                            "title": m.title,
                            "content_length": m.content.len(),
                            "messages": c.messages.len(),
                            "source": m.source_format,
                        })
                    })
                })
                .collect();
            writeln!(
                out.stdout,
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "dry_run": true,
                    "total_conversations": conversations.len(),
                    "filtered": filtered.len(),
                    "would_import": items.len(),
                    "namespace": namespace,
                    "tier": tier.as_str(),
                    "memories": items,
                }))?
            )?;
        } else {
            writeln!(out.stdout, "Dry run — no memories will be stored\n")?;
            writeln!(
                out.stdout,
                "Total conversations found: {}",
                conversations.len()
            )?;
            writeln!(
                out.stdout,
                "After filter (>={} messages): {}",
                args.min_messages,
                filtered.len()
            )?;
            writeln!(out.stdout, "Namespace: {namespace}")?;
            writeln!(out.stdout, "Tier: {tier}\n")?;
            for c in &filtered {
                if let Some(m) = mine::conversation_to_memory(c, format) {
                    writeln!(
                        out.stdout,
                        "  {} ({} msgs, {} bytes)",
                        m.title,
                        c.messages.len(),
                        m.content.len()
                    )?;
                }
            }
        }
        return Ok(());
    }

    let conn = db::open(db_path)?;
    let _ = db::gc_if_needed(&conn, app_config.effective_archive_on_gc());
    let now = Utc::now();

    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut errors = 0usize;

    conn.execute_batch("BEGIN")?;

    for conv in &filtered {
        let Some(mined) = mine::conversation_to_memory(conv, format) else {
            skipped += 1;
            continue;
        };

        let expires_at = app_config
            .effective_ttl()
            .ttl_for_tier(&tier)
            .map(|s| (now + Duration::seconds(s)).to_rfc3339());

        let mut metadata = models::default_metadata();
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert(
                "agent_id".to_string(),
                serde_json::Value::String(miner_agent_id.clone()),
            );
            obj.insert(
                "mined_from".to_string(),
                serde_json::Value::String(format.source_tag().to_string()),
            );
        }
        let mem = models::Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: tier.clone(),
            namespace: namespace.clone(),
            title: mined.title,
            content: mined.content,
            tags: vec![format.source_tag().to_string()],
            priority: 5,
            confidence: 0.8,
            source: mined.source_format,
            access_count: 0,
            created_at: mined.created_at.unwrap_or_else(|| now.to_rfc3339()),
            updated_at: now.to_rfc3339(),
            last_accessed_at: None,
            expires_at,
            metadata,
        };

        match db::insert(&conn, &mem) {
            Ok(_) => imported += 1,
            Err(e) => {
                errors += 1;
                writeln!(
                    out.stderr,
                    "warning: failed to store '{}': {}",
                    mem.title, e
                )?;
            }
        }

        if imported.is_multiple_of(100) && imported > 0 {
            conn.execute_batch("COMMIT")?;
            conn.execute_batch("BEGIN")?;
        }
    }

    conn.execute_batch("COMMIT")?;

    if json_out {
        writeln!(
            out.stdout,
            "{}",
            serde_json::to_string(&serde_json::json!({
                "imported": imported,
                "skipped": skipped,
                "errors": errors,
                "total_conversations": conversations.len(),
                "namespace": namespace,
                "tier": tier.as_str(),
            }))?
        )?;
    } else {
        writeln!(
            out.stdout,
            "Imported {} memories from {} conversations (skipped: {}, errors: {})",
            imported,
            conversations.len(),
            skipped,
            errors
        )?;
        writeln!(out.stdout, "Namespace: {namespace}, Tier: {tier}")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    // ---------------- export ------------------------------------------

    #[test]
    fn test_export_empty_db() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        // Touch db path so db::open() materialises an empty schema
        let _ = seed_memory(&db, "ns-init", "init", "init");
        {
            let mut out = env.output();
            export(&db, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert!(v["memories"].is_array());
        assert!(v["links"].is_array());
        assert!(v["count"].is_u64());
        assert!(v["exported_at"].is_string());
    }

    #[test]
    fn test_export_with_memories_includes_links() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id1 = seed_memory(&db, "ns", "a", "content-a");
        let id2 = seed_memory(&db, "ns", "b", "content-b");
        let conn = db::open(&db).unwrap();
        db::create_link(&conn, &id1, &id2, "relates").unwrap();
        drop(conn);
        {
            let mut out = env.output();
            export(&db, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        let mems = v["memories"].as_array().unwrap();
        assert_eq!(mems.len(), 2);
        let links = v["links"].as_array().unwrap();
        assert_eq!(links.len(), 1);
    }

    #[test]
    fn test_export_pretty_printed_json() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "ns", "x", "y");
        {
            let mut out = env.output();
            export(&db, &mut out).unwrap();
        }
        // Pretty-printed JSON has at least one newline + 2-space indent.
        let s = env.stdout_str();
        assert!(s.contains('\n'));
        assert!(s.contains("  \"memories\""));
    }

    // ---------------- import ------------------------------------------

    fn export_payload_at(db_path: &Path) -> String {
        let mut buf = Vec::<u8>::new();
        let mut errbuf = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut buf, &mut errbuf);
        export(db_path, &mut out).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn test_import_default_restamps_agent_id() {
        // Source: a payload whose memories carry agent_id="other-agent"
        let src = TestEnv::fresh();
        let src_db = src.db_path.clone();
        let id = seed_memory(&src_db, "ns", "src-title", "src-content");
        {
            let conn = db::open(&src_db).unwrap();
            conn.execute(
                "UPDATE memories SET metadata = json_set(metadata, '$.agent_id', 'other-agent') WHERE id = ?1",
                rusqlite::params![id],
            )
            .unwrap();
        }
        let payload = export_payload_at(&src_db);

        let mut dst = TestEnv::fresh();
        let dst_db = dst.db_path.clone();
        let args = ImportArgs {
            trust_source: false,
        };
        {
            let mut out = dst.output();
            import_from_str(
                &payload,
                &dst_db,
                &args,
                true,
                Some("caller-agent"),
                &mut out,
            )
            .unwrap();
        }
        let conn = db::open(&dst_db).unwrap();
        let mem = db::get(&conn, &id).unwrap().unwrap();
        assert_eq!(
            mem.metadata.get("agent_id").and_then(|v| v.as_str()),
            Some("caller-agent")
        );
        assert_eq!(
            mem.metadata
                .get("imported_from_agent_id")
                .and_then(|v| v.as_str()),
            Some("other-agent")
        );
    }

    #[test]
    fn test_import_trust_source_preserves_agent_id() {
        let src = TestEnv::fresh();
        let src_db = src.db_path.clone();
        let id = seed_memory(&src_db, "ns", "tt", "cc");
        {
            let conn = db::open(&src_db).unwrap();
            conn.execute(
                "UPDATE memories SET metadata = json_set(metadata, '$.agent_id', 'preserved-agent') WHERE id = ?1",
                rusqlite::params![id],
            )
            .unwrap();
        }
        let payload = export_payload_at(&src_db);

        let mut dst = TestEnv::fresh();
        let dst_db = dst.db_path.clone();
        let args = ImportArgs { trust_source: true };
        {
            let mut out = dst.output();
            import_from_str(&payload, &dst_db, &args, false, Some("caller"), &mut out).unwrap();
        }
        let conn = db::open(&dst_db).unwrap();
        let mem = db::get(&conn, &id).unwrap().unwrap();
        assert_eq!(
            mem.metadata.get("agent_id").and_then(|v| v.as_str()),
            Some("preserved-agent")
        );
        assert!(dst.stderr_str().contains("trust-source"));
    }

    #[test]
    fn test_import_invalid_memory_skipped_with_error() {
        let mut dst = TestEnv::fresh();
        let dst_db = dst.db_path.clone();
        // Craft a payload with one valid + one invalid (empty title).
        let payload = serde_json::json!({
            "memories": [
                {
                    "id": "11111111-1111-1111-1111-111111111111",
                    "tier": "mid",
                    "namespace": "ns",
                    "title": "",  // invalid: empty title
                    "content": "c",
                    "tags": [],
                    "priority": 5,
                    "confidence": 1.0,
                    "source": "import",
                    "access_count": 0,
                    "created_at": "2026-01-01T00:00:00+00:00",
                    "updated_at": "2026-01-01T00:00:00+00:00",
                    "last_accessed_at": null,
                    "expires_at": null,
                    "metadata": {"agent_id": "x"}
                },
                {
                    "id": "22222222-2222-2222-2222-222222222222",
                    "tier": "mid",
                    "namespace": "ns",
                    "title": "valid-row",
                    "content": "c",
                    "tags": [],
                    "priority": 5,
                    "confidence": 1.0,
                    "source": "import",
                    "access_count": 0,
                    "created_at": "2026-01-01T00:00:00+00:00",
                    "updated_at": "2026-01-01T00:00:00+00:00",
                    "last_accessed_at": null,
                    "expires_at": null,
                    "metadata": {"agent_id": "x"}
                }
            ],
            "links": [],
            "count": 2,
            "exported_at": "2026-01-01T00:00:00+00:00"
        })
        .to_string();
        let args = ImportArgs { trust_source: true };
        {
            let mut out = dst.output();
            import_from_str(&payload, &dst_db, &args, true, Some("caller"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(dst.stdout_str().trim()).unwrap();
        assert_eq!(v["imported"].as_u64().unwrap(), 1);
        let errs = v["errors"].as_array().unwrap();
        assert!(!errs.is_empty(), "expected at least one error");
    }

    #[test]
    fn test_import_invalid_link_skipped() {
        let mut dst = TestEnv::fresh();
        let dst_db = dst.db_path.clone();
        // Seed two valid memories so the link-target exists, then attach
        // a syntactically-invalid link entry.
        let id1 = seed_memory(&dst_db, "ns", "a", "ca");
        let id2 = seed_memory(&dst_db, "ns", "b", "cb");
        let payload = serde_json::json!({
            "memories": [],
            "links": [
                { "source_id": id1, "target_id": id2, "relation": "" },
                { "source_id": id1, "target_id": id2, "relation": "supersedes" }
            ],
            "count": 0,
            "exported_at": "2026-01-01T00:00:00+00:00"
        })
        .to_string();
        let args = ImportArgs { trust_source: true };
        {
            let mut out = dst.output();
            import_from_str(&payload, &dst_db, &args, true, Some("caller"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(dst.stdout_str().trim()).unwrap();
        assert_eq!(v["imported"].as_u64().unwrap(), 0);
    }

    #[test]
    fn test_import_roundtrip_export_import_preserves_data() {
        let src = TestEnv::fresh();
        let src_db = src.db_path.clone();
        let _id = seed_memory(&src_db, "rt-ns", "rt-title", "rt-content");
        let payload = export_payload_at(&src_db);

        let mut dst = TestEnv::fresh();
        let dst_db = dst.db_path.clone();
        let args = ImportArgs { trust_source: true };
        {
            let mut out = dst.output();
            import_from_str(&payload, &dst_db, &args, true, Some("caller"), &mut out).unwrap();
        }
        let conn = db::open(&dst_db).unwrap();
        let all = db::export_all(&conn).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "rt-title");
        assert_eq!(all[0].content, "rt-content");
        assert_eq!(all[0].namespace, "rt-ns");
    }

    // ---------------- mine --------------------------------------------

    fn write_minimal_claude_export(dir: &Path) -> PathBuf {
        // Claude export shape: JSONL — one conversation per line.
        let conv1 = serde_json::json!({
            "uuid": "conv-1",
            "name": "Conv with 5 messages",
            "created_at": "2026-01-01T00:00:00.000Z",
            "updated_at": "2026-01-01T00:00:00.000Z",
            "chat_messages": [
                { "uuid": "m1", "text": "hello", "sender": "human", "created_at": "2026-01-01T00:00:00.000Z" },
                { "uuid": "m2", "text": "hi there", "sender": "assistant", "created_at": "2026-01-01T00:00:00.000Z" },
                { "uuid": "m3", "text": "how are you", "sender": "human", "created_at": "2026-01-01T00:00:00.000Z" },
                { "uuid": "m4", "text": "fine thanks", "sender": "assistant", "created_at": "2026-01-01T00:00:00.000Z" },
                { "uuid": "m5", "text": "ok bye", "sender": "human", "created_at": "2026-01-01T00:00:00.000Z" }
            ]
        });
        let conv2 = serde_json::json!({
            "uuid": "conv-2",
            "name": "Short Conv",
            "created_at": "2026-01-01T00:00:00.000Z",
            "updated_at": "2026-01-01T00:00:00.000Z",
            "chat_messages": [
                { "uuid": "m6", "text": "ping", "sender": "human", "created_at": "2026-01-01T00:00:00.000Z" }
            ]
        });
        let p = dir.join("claude.jsonl");
        let body = format!("{}\n{}\n", conv1, conv2);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn test_mine_dry_run_writes_nothing() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        let claude_path = write_minimal_claude_export(tmp.path());
        let args = MineArgs {
            path: claude_path,
            format: "claude".to_string(),
            namespace: Some("mined-ns".to_string()),
            tier: "mid".to_string(),
            min_messages: 3,
            dry_run: true,
        };
        {
            let mut out = env.output();
            mine(&db, args, true, &cfg, Some("miner"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["dry_run"].as_bool().unwrap(), true);
        // No memory was written. Only attempt to open if the file exists
        // (dry-run never touches the DB at all).
        if db.exists() {
            let conn = db::open(&db).unwrap();
            let all = db::export_all(&conn).unwrap();
            assert_eq!(all.len(), 0);
        }
    }

    #[test]
    fn test_mine_filters_by_min_messages() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        let claude_path = write_minimal_claude_export(tmp.path());
        // min_messages=3 keeps Conv-1 (5 msgs) and drops Conv-2 (1 msg).
        let args = MineArgs {
            path: claude_path,
            format: "claude".to_string(),
            namespace: Some("mined-ns".to_string()),
            tier: "mid".to_string(),
            min_messages: 3,
            dry_run: true,
        };
        {
            let mut out = env.output();
            mine(&db, args, true, &cfg, Some("miner"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["total_conversations"].as_u64().unwrap(), 2);
        assert_eq!(v["filtered"].as_u64().unwrap(), 1);
    }

    // PR-9i — buffer coverage uplift. Targets the actual mine() write path
    // (lines 261-356) and invalid format/tier error paths.

    #[test]
    fn pr9i_mine_actual_write_path_text() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        let claude_path = write_minimal_claude_export(tmp.path());
        let args = MineArgs {
            path: claude_path,
            format: "claude".to_string(),
            namespace: Some("mined-real".to_string()),
            tier: "long".to_string(),
            min_messages: 3,
            dry_run: false,
        };
        {
            let mut out = env.output();
            mine(&db, args, false, &cfg, Some("miner-id"), &mut out).unwrap();
        }
        let s = env.stdout_str();
        assert!(s.contains("Imported"));
        assert!(s.contains("mined-real"));
        // The conversation with >=3 messages was actually written.
        let conn = db::open(&db).unwrap();
        let all = db::export_all(&conn).unwrap();
        let in_ns: Vec<&_> = all.iter().filter(|m| m.namespace == "mined-real").collect();
        assert_eq!(
            in_ns.len(),
            1,
            "expected exactly one mined memory in mined-real ns: {all:?}"
        );
        // agent_id is the miner; mined_from is the source format.
        assert_eq!(
            in_ns[0].metadata.get("agent_id").and_then(|v| v.as_str()),
            Some("miner-id")
        );
        assert_eq!(
            in_ns[0].metadata.get("mined_from").and_then(|v| v.as_str()),
            Some("mine-claude")
        );
    }

    #[test]
    fn pr9i_mine_actual_write_path_json() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        let claude_path = write_minimal_claude_export(tmp.path());
        let args = MineArgs {
            path: claude_path,
            format: "claude".to_string(),
            namespace: Some("mined-json".to_string()),
            tier: "mid".to_string(),
            min_messages: 3,
            dry_run: false,
        };
        {
            let mut out = env.output();
            mine(&db, args, true, &cfg, Some("miner-x"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["namespace"].as_str().unwrap(), "mined-json");
        assert_eq!(v["tier"].as_str().unwrap(), "mid");
        assert!(v["imported"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn pr9i_mine_default_namespace_per_format() {
        // Omit --namespace; defaults to "<format>-export".
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        let claude_path = write_minimal_claude_export(tmp.path());
        let args = MineArgs {
            path: claude_path,
            format: "claude".to_string(),
            namespace: None,
            tier: "mid".to_string(),
            min_messages: 3,
            dry_run: true,
        };
        {
            let mut out = env.output();
            mine(&db, args, true, &cfg, Some("miner"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["namespace"].as_str().unwrap(), "claude-export");
    }

    #[test]
    fn pr9i_mine_invalid_format_errors() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("anything.jsonl");
        std::fs::write(&p, "{}").unwrap();
        let args = MineArgs {
            path: p,
            format: "myspace".to_string(), // not claude/chatgpt/slack
            namespace: None,
            tier: "mid".to_string(),
            min_messages: 3,
            dry_run: true,
        };
        let mut out = env.output();
        let res = mine(&db, args, false, &cfg, Some("miner"), &mut out);
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("invalid format"));
    }

    #[test]
    fn pr9i_mine_invalid_tier_errors() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("c.jsonl");
        std::fs::write(&p, "{}").unwrap();
        let args = MineArgs {
            path: p,
            format: "claude".to_string(),
            namespace: None,
            tier: "permanent".to_string(), // not short/mid/long
            min_messages: 3,
            dry_run: true,
        };
        let mut out = env.output();
        let res = mine(&db, args, false, &cfg, Some("miner"), &mut out);
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("invalid tier"));
    }

    #[test]
    fn pr9i_mine_text_dry_run_lists_filtered_titles() {
        // Text-mode dry_run prints conversation titles (lines 232-256).
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let tmp = tempfile::tempdir().unwrap();
        let claude_path = write_minimal_claude_export(tmp.path());
        let args = MineArgs {
            path: claude_path,
            format: "claude".to_string(),
            namespace: Some("dry-text".to_string()),
            tier: "short".to_string(),
            min_messages: 3,
            dry_run: true,
        };
        {
            let mut out = env.output();
            mine(&db, args, false, &cfg, Some("miner"), &mut out).unwrap();
        }
        let s = env.stdout_str();
        assert!(s.contains("Dry run"));
        assert!(s.contains("Total conversations found"));
        assert!(s.contains("After filter"));
        assert!(s.contains("dry-text"));
        // The Claude conversation with name "Conv with 5 messages" must be
        // listed in the filtered preview.
        assert!(
            s.contains("5 msgs") || s.contains("Conv with 5 messages"),
            "expected conversation listing in dry-run text output: {s}"
        );
    }
}
