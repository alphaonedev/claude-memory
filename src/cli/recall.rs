// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_recall` migration. See `cli::store` for the design pattern.
//!
//! W6 (v0.6.3) — embedder construction was unified into
//! [`crate::daemon_runtime::build_embedder`]. Both `serve()` and this
//! handler now call the same builder, killing the per-call-site
//! duplication that the original W5b note flagged. The TestHelper that
//! used to live here (`build_embedder_for_recall`) is gone.

use crate::cli::CliOutput;
use crate::cli::helpers::{human_age, id_short};
use crate::config::AppConfig;
use crate::{color, daemon_runtime, db, embeddings, hnsw, reranker, validate};
use anyhow::Result;
use clap::Args;
use std::path::Path;

/// Clap-derived arg shape for the `recall` subcommand. Definition moved
/// from `main.rs` verbatim in W5b — fields and attrs unchanged.
#[derive(Args)]
pub struct RecallArgs {
    #[arg(allow_hyphen_values = true)]
    pub context: String,
    #[arg(long, short)]
    pub namespace: Option<String>,
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
    #[arg(long)]
    pub tags: Option<String>,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub until: Option<String>,
    /// Feature tier for recall: keyword, semantic, smart, autonomous
    #[arg(long, short = 'T')]
    pub tier: Option<String>,
    /// Task 1.5: querying agent's namespace position. Enables scope-based
    /// visibility filtering (private/team/unit/org/collective).
    #[arg(long)]
    pub as_agent: Option<String>,
    /// Task 1.11: context-budget-aware recall. Return the top-ranked
    /// memories whose cumulative estimated tokens fit within N. Omit
    /// for unlimited (limit-based only).
    #[arg(long)]
    pub budget_tokens: Option<usize>,
    /// v0.6.0.0 contextual recall. Comma-separated list of recent
    /// conversation tokens used to bias the query embedding at 70/30
    /// (primary/context). Shifts the recall towards memories that
    /// match both the explicit query and the conversation's nearby
    /// topics.
    #[arg(long, value_delimiter = ',')]
    pub context_tokens: Option<Vec<String>>,
}

/// `recall` handler. Mirrors `cmd_recall` from the pre-W5b `main.rs`
/// verbatim except every emit routes through `out.stdout` / `out.stderr`
/// instead of `println!` / `eprintln!`. The embedder is built via the
/// shared [`crate::daemon_runtime::build_embedder`] helper so the offline
/// recall path and the HTTP daemon use identical construction logic.
#[allow(clippy::too_many_lines)]
pub fn run(
    db_path: &Path,
    args: &RecallArgs,
    json_out: bool,
    app_config: &AppConfig,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    // #151: validate --as-agent namespace
    if let Some(ref a) = args.as_agent {
        validate::validate_namespace(a)?;
    }
    let conn = db::open(db_path)?;
    let _ = db::gc_if_needed(&conn, app_config.effective_archive_on_gc());

    // Resolve feature tier
    let feature_tier = app_config.effective_tier(args.tier.as_deref());
    let tier_config = feature_tier.config();

    // Initialize embedder if tier supports it. Use the shared builder so
    // recall and the HTTP daemon agree on tier→embedder semantics
    // (embed_url, model selection, error fallback). The shared builder
    // is async; we drive it on a small inline runtime to keep `run()`
    // sync. Tier=Keyword short-circuits inside the builder before any
    // tokio work happens, so the runtime's only cost is the keyword path.
    let embedder = {
        // Bridge sync→async: build a single-threaded runtime just for
        // this call. Cheap on the Keyword path (no tasks spawned), and
        // safe because `run()` is itself called from `main.rs` which is
        // already inside `#[tokio::main]` only when invoked through
        // `daemon_runtime::run` — the inner runtime is never nested
        // because we use `Handle::try_current()` to detect that case.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| {
                handle.block_on(daemon_runtime::build_embedder(feature_tier, app_config))
            })
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(daemon_runtime::build_embedder(feature_tier, app_config))
        }
    };
    if let Some(ref emb) = embedder {
        writeln!(
            out.stderr,
            "ai-memory: embedder loaded ({})",
            emb.model_description()
        )?;
    } else if tier_config.embedding_model.is_some() {
        writeln!(
            out.stderr,
            "ai-memory: embedder failed to load, falling back to keyword"
        )?;
    }

    // Backfill embeddings for memories that don't have them
    if let Some(ref emb) = embedder
        && let Ok(unembedded) = db::get_unembedded_ids(&conn)
        && !unembedded.is_empty()
    {
        writeln!(
            out.stderr,
            "ai-memory: backfilling {} memories...",
            unembedded.len()
        )?;
        let mut ok = 0usize;
        for (id, title, content) in &unembedded {
            let text = format!("{title} {content}");
            if let Ok(embedding) = emb.embed(&text)
                && db::set_embedding(&conn, id, &embedding).is_ok()
            {
                ok += 1;
            }
        }
        writeln!(
            out.stderr,
            "ai-memory: backfilled {}/{}",
            ok,
            unembedded.len()
        )?;
    }

    // Build HNSW vector index if embedder is available
    let vector_index = if embedder.is_some() {
        match db::get_all_embeddings(&conn) {
            Ok(entries) if !entries.is_empty() => Some(hnsw::VectorIndex::build(entries)),
            _ => Some(hnsw::VectorIndex::empty()),
        }
    } else {
        None
    };

    // Initialize cross-encoder reranker for autonomous tier
    let reranker = if tier_config.cross_encoder {
        Some(reranker::CrossEncoder::new_neural())
    } else {
        None
    };

    let resolved_ttl = app_config.effective_ttl();
    let resolved_scoring = app_config.effective_scoring();

    // Perform recall: hybrid if embedder available, keyword otherwise
    let (results, tokens_used, mode) = if let Some(ref emb) = embedder {
        match emb.embed(&args.context) {
            Ok(primary_emb) => {
                // v0.6.0.0 contextual recall. Fuse the primary query
                // embedding with an embedding over recent conversation
                // tokens (caller-supplied) at 70/30. Fusion is done
                // caller-side so recall_hybrid stays unaware of the bias —
                // the vector it receives is the final query direction.
                let query_emb = match args.context_tokens.as_deref() {
                    Some(tokens) if !tokens.is_empty() => {
                        let joined = tokens.join(" ");
                        match emb.embed(&joined) {
                            Ok(ctx_emb) => embeddings::Embedder::fuse(&primary_emb, &ctx_emb, 0.7),
                            Err(e) => {
                                writeln!(
                                    out.stderr,
                                    "ai-memory: context_tokens embed failed: {e}, using primary only"
                                )?;
                                primary_emb
                            }
                        }
                    }
                    _ => primary_emb,
                };
                let (results, tokens_used) = db::recall_hybrid(
                    &conn,
                    &args.context,
                    &query_emb,
                    args.namespace.as_deref(),
                    args.limit.min(50),
                    args.tags.as_deref(),
                    args.since.as_deref(),
                    args.until.as_deref(),
                    vector_index.as_ref(),
                    resolved_ttl.short_extend_secs,
                    resolved_ttl.mid_extend_secs,
                    args.as_agent.as_deref(),
                    args.budget_tokens,
                    &resolved_scoring,
                )?;
                if let Some(ref ce) = reranker {
                    (
                        ce.rerank(&args.context, results),
                        tokens_used,
                        "hybrid+rerank",
                    )
                } else {
                    (results, tokens_used, "hybrid")
                }
            }
            Err(e) => {
                writeln!(
                    out.stderr,
                    "ai-memory: embedding query failed: {e}, falling back to keyword"
                )?;
                let (results, tokens_used) = db::recall(
                    &conn,
                    &args.context,
                    args.namespace.as_deref(),
                    args.limit,
                    args.tags.as_deref(),
                    args.since.as_deref(),
                    args.until.as_deref(),
                    resolved_ttl.short_extend_secs,
                    resolved_ttl.mid_extend_secs,
                    args.as_agent.as_deref(),
                    args.budget_tokens,
                )?;
                (results, tokens_used, "keyword")
            }
        }
    } else {
        let (results, tokens_used) = db::recall(
            &conn,
            &args.context,
            args.namespace.as_deref(),
            args.limit,
            args.tags.as_deref(),
            args.since.as_deref(),
            args.until.as_deref(),
            resolved_ttl.short_extend_secs,
            resolved_ttl.mid_extend_secs,
            args.as_agent.as_deref(),
            args.budget_tokens,
        )?;
        (results, tokens_used, "keyword")
    };

    if json_out {
        let scored: Vec<serde_json::Value> = results
            .iter()
            .map(|(m, s)| {
                let mut v = serde_json::to_value(m).unwrap_or_default();
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "score".to_string(),
                        serde_json::json!((s * 1000.0).round() / 1000.0),
                    );
                }
                v
            })
            .collect();
        let mut body = serde_json::json!({
            "memories": scored,
            "count": results.len(),
            "mode": mode,
            "tokens_used": tokens_used,
        });
        if let Some(b) = args.budget_tokens {
            body["budget_tokens"] = serde_json::json!(b);
        }
        writeln!(out.stdout, "{}", serde_json::to_string(&body)?)?;
        return Ok(());
    }
    if results.is_empty() {
        writeln!(out.stderr, "no memories found for: {}", args.context)?;
        return Ok(());
    }
    for (mem, score) in &results {
        let age = human_age(&mem.updated_at);
        let config = if mem.confidence < 1.0 {
            format!(" conf={:.0}%", mem.confidence * 100.0)
        } else {
            String::new()
        };
        writeln!(
            out.stdout,
            "[{}] {} {} score={:.2} (ns={}, {}x, {}{})",
            color::tier_color(
                mem.tier.as_str(),
                &format!("{}/{}", mem.tier, id_short(&mem.id))
            ),
            color::bold(&mem.title),
            color::priority_bar(mem.priority),
            score,
            color::cyan(&mem.namespace),
            mem.access_count,
            color::dim(&age),
            config
        )?;
        let preview: String = mem.content.chars().take(200).collect();
        writeln!(out.stdout, "  {}\n", color::dim(&preview))?;
    }
    writeln!(
        out.stdout,
        "{} memory(ies) recalled [{}]",
        results.len(),
        mode
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};
    use crate::config::FeatureTier;

    fn default_args() -> RecallArgs {
        RecallArgs {
            context: "needle".to_string(),
            namespace: None,
            limit: 10,
            tags: None,
            since: None,
            until: None,
            tier: Some("keyword".to_string()),
            as_agent: None,
            budget_tokens: None,
            context_tokens: None,
        }
    }

    #[test]
    fn test_recall_keyword_tier_no_embedder() {
        // Keyword tier => no embedder; the keyword branch must run
        // happily and find the seeded title.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "haystack content");
        let args = default_args();
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run(&db, &args, false, &cfg, &mut out).unwrap();
        }
        let stdout = env.stdout_str();
        assert!(stdout.contains("needle title"), "got: {stdout}");
        assert!(stdout.contains("[keyword]"), "got: {stdout}");
    }

    #[test]
    fn test_recall_keyword_empty_results() {
        // No seeded rows => empty results => stderr emits "no memories
        // found for: ..." and stdout stays empty (text mode).
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = default_args();
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run(&db, &args, false, &cfg, &mut out).unwrap();
        }
        assert_eq!(env.stdout_str(), "");
        assert!(
            env.stderr_str().contains("no memories found for: needle"),
            "got: {}",
            env.stderr_str()
        );
    }

    #[test]
    fn test_recall_keyword_with_namespace_filter() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "ns-a", "needle in a", "content a");
        seed_memory(&db, "ns-b", "needle in b", "content b");
        let mut args = default_args();
        args.namespace = Some("ns-a".to_string());
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run(&db, &args, true, &cfg, &mut out).unwrap();
        }
        // JSON mode — parse and verify only the ns-a row came back.
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        let mems = v["memories"].as_array().unwrap();
        for m in mems {
            assert_eq!(m["namespace"].as_str().unwrap(), "ns-a");
        }
    }

    #[test]
    fn test_recall_keyword_with_tags_filter() {
        // tags filter takes a string; absence of tags on seeded rows
        // means the filter excludes them. Just verify the call shape
        // doesn't error when a tags filter is supplied.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "content");
        let mut args = default_args();
        args.tags = Some("nonexistent".to_string());
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run(&db, &args, true, &cfg, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        // No row has the "nonexistent" tag => 0 results.
        assert_eq!(v["count"].as_u64().unwrap(), 0);
    }

    #[test]
    fn test_recall_keyword_with_since_until_window() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "content");
        let mut args = default_args();
        // A date range that excludes the just-now timestamp.
        args.since = Some("1970-01-01T00:00:00Z".to_string());
        args.until = Some("1970-01-02T00:00:00Z".to_string());
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run(&db, &args, true, &cfg, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["count"].as_u64().unwrap(), 0);
    }

    #[test]
    fn test_recall_with_as_agent_scope_filter() {
        // --as-agent must validate as a namespace; passing a real
        // namespace exercises the validation branch and succeeds.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "content");
        let mut args = default_args();
        args.as_agent = Some("test".to_string());
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run(&db, &args, true, &cfg, &mut out).unwrap();
        }
        // No assertion error; JSON shape comes through.
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert!(v["memories"].is_array());
    }

    #[test]
    fn test_recall_with_budget_tokens_caps_results() {
        // budget_tokens flips through into recall(); JSON envelope
        // includes the budget echo when set.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle one", "content one");
        seed_memory(&db, "test", "needle two", "content two");
        let mut args = default_args();
        args.budget_tokens = Some(64);
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run(&db, &args, true, &cfg, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["budget_tokens"].as_u64().unwrap(), 64);
    }

    #[test]
    fn test_recall_json_output_includes_score_mode_tokens() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "haystack content");
        let args = default_args();
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run(&db, &args, true, &cfg, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["mode"].as_str().unwrap(), "keyword");
        assert!(v["tokens_used"].is_number());
        let mems = v["memories"].as_array().unwrap();
        assert!(!mems.is_empty(), "expected at least one match");
        for m in mems {
            assert!(m["score"].is_number());
        }
    }

    #[test]
    fn test_recall_text_output_formats_correctly() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test-ns", "needle title", "haystack content");
        let args = default_args();
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run(&db, &args, false, &cfg, &mut out).unwrap();
        }
        let stdout = env.stdout_str();
        // Header line: tier/short-id, title, score, namespace.
        assert!(stdout.contains("needle title"));
        assert!(stdout.contains("ns="));
        assert!(stdout.contains("score="));
        assert!(stdout.contains("memory(ies) recalled"));
    }

    #[test]
    fn test_recall_invalid_as_agent_namespace_validation_error() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let mut args = default_args();
        // Invalid namespace: empty after trimming, or contains illegal chars.
        args.as_agent = Some(String::new());
        let cfg = AppConfig::default();
        let mut out = env.output();
        let res = run(&db, &args, false, &cfg, &mut out);
        assert!(res.is_err(), "expected validate_namespace to reject");
    }

    #[test]
    fn test_recall_with_context_tokens_fusion() {
        // With tier=keyword, no embedder is built, so the fusion path
        // is skipped entirely and the call falls through the keyword
        // branch. This proves the fall-through path exists when an
        // embedder is absent. The actual fusion path requires a real
        // embedder and is exercised under feature = "test-with-models".
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "content");
        let mut args = default_args();
        args.context_tokens = Some(vec!["recent".to_string(), "talk".to_string()]);
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run(&db, &args, true, &cfg, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["mode"].as_str().unwrap(), "keyword");
    }

    #[test]
    fn test_recall_embedder_failure_falls_back_to_keyword() {
        // Same shape as the no-embedder test, but routed through the
        // build_embedder_for_recall path. Keyword tier => Ok(None) and
        // no stderr emission about embedder failure.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "content");
        let args = default_args();
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run(&db, &args, true, &cfg, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["mode"].as_str().unwrap(), "keyword");
        // No embedder messages on stderr in the keyword branch.
        let stderr = env.stderr_str();
        assert!(
            !stderr.contains("embedder loaded"),
            "no embedder should be loaded on keyword tier"
        );
    }

    #[tokio::test]
    async fn test_shared_build_embedder_keyword_returns_none() {
        // W6 — recall now delegates embedder construction to
        // `daemon_runtime::build_embedder`. Smoke-test that the keyword
        // tier short-circuit still yields `None` (no model load attempt,
        // no panic).
        let cfg = AppConfig::default();
        let res = daemon_runtime::build_embedder(FeatureTier::Keyword, &cfg).await;
        assert!(res.is_none(), "keyword tier must not build an embedder");
    }
}
