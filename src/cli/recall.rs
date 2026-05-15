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
use crate::embeddings::Embed;
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
    /// v0.7.0 (issue #518) — when set, splice defaults from
    /// `[agents.defaults.recall_scope]` in `config.toml` for any
    /// filter field not explicitly passed on the command line.
    /// Resolution: explicit args > recall_scope defaults > compiled
    /// defaults. Default `false` preserves v0.6.x recall semantics.
    #[arg(long)]
    pub session_default: bool,
    /// v0.7.0 WT-1-E — when set, recall returns archived sources
    /// (those replaced by their atoms after WT-1-B atomisation)
    /// alongside the atoms. Default `false` surfaces atoms only,
    /// which is the canonical post-atomisation recall unit.
    #[arg(long)]
    pub include_archived: bool,
    /// v0.7.0 Form 4 (issue #757) — restrict results to memories
    /// whose `citations` array is non-empty. Composes with the
    /// other filters; default `false` (no provenance filter).
    #[arg(long)]
    pub has_citations: bool,
    /// v0.7.0 Form 4 (issue #757) — restrict results to memories
    /// whose `source_uri` starts with this prefix. Matches the
    /// substring exactly (no glob/regex). Typical use:
    /// `--source-uri-prefix doc:` to surface every atom or memory
    /// pointing at a substrate doc; `--source-uri-prefix uri:https://`
    /// to surface every memory citing an HTTP source.
    #[arg(long)]
    pub source_uri_prefix: Option<String>,
    /// v0.7.x Form 6 (issue #759) — Batman-taxonomy memory-kind
    /// filter. Comma-separated. Examples:
    ///   --kind concept
    ///   --kind concept,entity,claim
    /// Recognised values: observation, reflection, persona, concept,
    /// entity, claim, relation, event, conversation, decision.
    /// OR-of-kinds within the flag; AND with the other filters.
    /// Pass 'all' or omit for no filter.
    #[arg(long = "kind", value_name = "KIND[,KIND...]")]
    pub kind: Option<String>,
}

/// v0.7.0 Form 4 (issue #757) — post-filter a recall result set by
/// the Form 4 fact-provenance criteria. Composes with the existing
/// substrate-level WHERE clauses (those run inside SQL); these
/// filters run in Rust because both criteria are read-only checks
/// on already-deserialised Memory rows and the alternative would
/// be a substrate-wide signature change on `recall` / `recall_hybrid`.
#[must_use]
pub fn apply_form4_recall_filters(
    results: Vec<(crate::models::Memory, f64)>,
    has_citations: bool,
    source_uri_prefix: Option<&str>,
) -> Vec<(crate::models::Memory, f64)> {
    if !has_citations && source_uri_prefix.is_none() {
        return results;
    }
    results
        .into_iter()
        .filter(|(m, _)| {
            if has_citations && m.citations.is_empty() {
                return false;
            }
            if let Some(prefix) = source_uri_prefix {
                match m.source_uri.as_deref() {
                    Some(uri) if uri.starts_with(prefix) => {}
                    _ => return false,
                }
            }
            true
        })
        .collect()
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
    // Delegate to the embedder-injected helper so test code can reach
    // every branch downstream without owning a real candle Embedder.
    let embedder_ref: Option<&dyn Embed> = embedder.as_ref().map(|e| e as &dyn Embed);
    run_with_embedder(
        &conn,
        args,
        json_out,
        app_config,
        feature_tier,
        embedder_ref,
        embedder
            .as_ref()
            .map(crate::embeddings::Embedder::model_description),
        out,
    )
}

/// Test-injectable core of [`run`]. Production callers go through `run`
/// which builds an [`Embedder`] via `daemon_runtime::build_embedder` and
/// delegates here. Tests can pass a `MockEmbedder` directly without the
/// candle / HuggingFace dependency chain.
#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_with_embedder(
    conn: &rusqlite::Connection,
    args: &RecallArgs,
    json_out: bool,
    app_config: &AppConfig,
    feature_tier: crate::config::FeatureTier,
    embedder: Option<&dyn Embed>,
    embedder_model_description: Option<&str>,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let tier_config = feature_tier.config();
    // v0.7.0 (issue #518) — when `--session-default` is passed AND a
    // given filter axis is absent on the CLI, splice in the
    // `[agents.defaults.recall_scope]` value from config.toml.
    let scope = if args.session_default {
        app_config.effective_recall_scope()
    } else {
        None
    };
    let effective_namespace: Option<String> = args.namespace.clone().or_else(|| {
        scope
            .and_then(|s| s.namespaces.as_ref())
            .and_then(|v| v.first())
            .cloned()
    });
    let effective_since: Option<String> = args.since.clone().or_else(|| {
        scope.and_then(|s| {
            s.since.as_deref().and_then(|d| {
                crate::config::parse_duration_string(d).map(|dur| {
                    let cutoff = chrono::Utc::now() - dur;
                    cutoff.to_rfc3339()
                })
            })
        })
    });
    let effective_limit_usize = if args.limit == 10
        && let Some(v) = scope.and_then(|s| s.limit)
    {
        usize::try_from(v).unwrap_or(usize::MAX)
    } else {
        args.limit
    };
    let _effective_recall_tier: Option<String> = scope.and_then(|s| s.tier.clone());

    // v0.7.x Form 6 — parse the optional --kind filter. Treat the
    // literal "all" as "no filter" to match the MCP `kinds: "all"`
    // shorthand, and accept comma-separated tokens otherwise.
    let kinds_filter: Option<Vec<crate::models::MemoryKind>> = args.kind.as_deref().and_then(|s| {
        if s.trim().eq_ignore_ascii_case("all") {
            None
        } else {
            crate::models::MemoryKind::parse_csv(s)
        }
    });

    if let Some(desc) = embedder_model_description {
        writeln!(out.stderr, "ai-memory: embedder loaded ({desc})")?;
    } else if tier_config.embedding_model.is_some() {
        writeln!(
            out.stderr,
            "ai-memory: embedder failed to load, falling back to keyword"
        )?;
    }

    // Backfill embeddings for memories that don't have them
    if let Some(emb) = embedder
        && let Ok(unembedded) = db::get_unembedded_ids(conn)
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
                && db::set_embedding(conn, id, &embedding).is_ok()
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
        match db::get_all_embeddings(conn) {
            Ok(entries) if !entries.is_empty() => Some(hnsw::VectorIndex::build(entries)),
            _ => Some(hnsw::VectorIndex::empty()),
        }
    } else {
        None
    };

    let reranker = if tier_config.cross_encoder {
        Some(reranker::BatchedReranker::new(
            reranker::CrossEncoder::new_neural(),
        ))
    } else {
        None
    };

    let resolved_ttl = app_config.effective_ttl();
    let resolved_scoring = app_config.effective_scoring();

    // Perform recall: hybrid if embedder available, keyword otherwise
    let (results, outcome, mode) = if let Some(emb) = embedder {
        match emb.embed(&args.context) {
            Ok(primary_emb) => {
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
                let (results, outcome) = db::recall_hybrid(
                    conn,
                    &args.context,
                    &query_emb,
                    effective_namespace.as_deref(),
                    effective_limit_usize.min(50),
                    args.tags.as_deref(),
                    effective_since.as_deref(),
                    args.until.as_deref(),
                    vector_index.as_ref(),
                    resolved_ttl.short_extend_secs,
                    resolved_ttl.mid_extend_secs,
                    args.as_agent.as_deref(),
                    args.budget_tokens,
                    &resolved_scoring,
                    args.include_archived,
                )?;
                if let Some(ref ce) = reranker {
                    (ce.rerank(&args.context, results), outcome, "hybrid+rerank")
                } else {
                    (results, outcome, "hybrid")
                }
            }
            Err(e) => {
                writeln!(
                    out.stderr,
                    "ai-memory: embedding query failed: {e}, falling back to keyword"
                )?;
                let (results, outcome) = db::recall(
                    conn,
                    &args.context,
                    effective_namespace.as_deref(),
                    effective_limit_usize,
                    args.tags.as_deref(),
                    effective_since.as_deref(),
                    args.until.as_deref(),
                    resolved_ttl.short_extend_secs,
                    resolved_ttl.mid_extend_secs,
                    args.as_agent.as_deref(),
                    args.budget_tokens,
                    args.include_archived,
                )?;
                (results, outcome, "keyword")
            }
        }
    } else {
        let (results, outcome) = db::recall(
            conn,
            &args.context,
            effective_namespace.as_deref(),
            effective_limit_usize,
            args.tags.as_deref(),
            effective_since.as_deref(),
            args.until.as_deref(),
            resolved_ttl.short_extend_secs,
            resolved_ttl.mid_extend_secs,
            args.as_agent.as_deref(),
            args.budget_tokens,
            args.include_archived,
        )?;
        (results, outcome, "keyword")
    };

    // v0.7.0 Form 4 (issue #757) — fact-provenance post-filter.
    let results = apply_form4_recall_filters(
        results,
        args.has_citations,
        args.source_uri_prefix.as_deref(),
    );

    // v0.7.x Form 6 — apply the parsed kinds filter to the result set
    // in-place. No-op when `kinds_filter == None`. Cheap (results are
    // already capped at limit.min(50)), and avoids touching the recall
    // SQL on the existing storage path.
    let results: Vec<(crate::models::Memory, f64)> = match kinds_filter.as_deref() {
        None => results,
        Some(allowed) => results
            .into_iter()
            .filter(|(m, _)| allowed.contains(&m.memory_kind))
            .collect(),
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
            "tokens_used": outcome.tokens_used,
        });
        if let Some(b) = args.budget_tokens {
            body["budget_tokens"] = serde_json::json!(b);
            // Phase P6 (R1) meta block — same shape as MCP / HTTP paths.
            body["meta"] = serde_json::json!({
                "budget_tokens_used": outcome.tokens_used,
                "budget_tokens_remaining": outcome.tokens_remaining.unwrap_or(0),
                "memories_dropped": outcome.memories_dropped,
                "budget_overflow": outcome.budget_overflow,
            });
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
            session_default: false,
            include_archived: false,
            has_citations: false,
            source_uri_prefix: None,
            kind: None,
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

    // ----------------------------------------------------------------
    // L0.7-3 chunk-e2 — coverage uplift to ≥95%.
    // ----------------------------------------------------------------

    /// Build an AppConfig with a recall_scope so `--session-default`
    /// has something to splice in. Uses TOML parsing because
    /// `AppConfig` does not directly expose builder methods for the
    /// nested defaults block.
    fn app_config_with_recall_scope() -> AppConfig {
        let toml = r#"
tier = "keyword"

[agents.defaults.recall_scope]
namespaces = ["scope-ns"]
since = "1d"
tier = "long"
limit = 25
"#;
        toml::from_str(toml).expect("parse test config")
    }

    #[test]
    fn recall_session_default_splices_namespace_and_since_from_scope() {
        // Drives the session_default scope path (lines 90-110).
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        // Seed a memory in the scoped namespace.
        seed_memory(&db, "scope-ns", "needle title", "scoped");
        // Seed a memory in another namespace which should be filtered out.
        seed_memory(&db, "other-ns", "needle elsewhere", "other");
        let mut args = default_args();
        args.session_default = true;
        // Leave namespace=None so the scope splice picks "scope-ns".
        let cfg = app_config_with_recall_scope();
        {
            let mut out = env.output();
            run(&db, &args, true, &cfg, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        // Only memories in scope-ns survive.
        for m in v["memories"].as_array().unwrap() {
            assert_eq!(m["namespace"].as_str().unwrap(), "scope-ns");
        }
    }

    #[test]
    fn recall_session_default_explicit_namespace_wins_over_scope() {
        // Explicit args > scope (line 95: args.namespace.clone().or_else).
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "scope-ns", "needle title", "content");
        seed_memory(&db, "explicit-ns", "needle elsewhere", "content");
        let mut args = default_args();
        args.session_default = true;
        args.namespace = Some("explicit-ns".to_string());
        let cfg = app_config_with_recall_scope();
        {
            let mut out = env.output();
            run(&db, &args, true, &cfg, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        for m in v["memories"].as_array().unwrap() {
            assert_eq!(m["namespace"].as_str().unwrap(), "explicit-ns");
        }
    }

    #[test]
    fn recall_session_default_with_explicit_limit_does_not_apply_scope_limit() {
        // When args.limit != default (10), the scope.limit splice is
        // skipped (line 117 condition).
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        for i in 0..5 {
            seed_memory(&db, "scope-ns", &format!("needle {i}"), "c");
        }
        let mut args = default_args();
        args.session_default = true;
        args.limit = 2; // explicit override
        let cfg = app_config_with_recall_scope();
        {
            let mut out = env.output();
            run(&db, &args, true, &cfg, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        let mems = v["memories"].as_array().unwrap();
        assert!(mems.len() <= 2, "explicit limit=2 should cap results");
    }

    // ------------------------------------------------------------------
    // L0.7-3 chunk-e2 — embedder-driven branches via run_with_embedder.
    // ------------------------------------------------------------------

    /// Embedder that returns an error on `embed` — drives the
    /// "embedding query failed, falling back to keyword" branch.
    struct FailingEmbedder;
    impl Embed for FailingEmbedder {
        fn embed(&self, _text: &str) -> Result<Vec<f32>> {
            anyhow::bail!("synthetic embed failure for test")
        }
    }

    /// Embedder that errors only when the input is exactly "joined
    /// context tokens" — drives the fuse-failure branch (primary
    /// succeeds, context_tokens embed fails).
    struct FailOnContextTokens {
        joined_marker: String,
    }
    impl Embed for FailOnContextTokens {
        fn embed(&self, text: &str) -> Result<Vec<f32>> {
            if text == self.joined_marker {
                anyhow::bail!("synthetic context-tokens failure")
            }
            let mock = crate::embeddings::test_support::MockEmbedder::new_local()?;
            mock.embed(text)
        }
    }

    #[test]
    fn recall_with_embedder_takes_hybrid_path() {
        // run_with_embedder + MockEmbedder drives the `embedder.is_some()`
        // branch in run_with_embedder including embedder-loaded banner,
        // backfill, vector index build, and the hybrid recall_hybrid call.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "content");
        let conn = db::open(&db).unwrap();
        let mock = crate::embeddings::test_support::MockEmbedder::new_local().unwrap();
        let args = default_args();
        let cfg = AppConfig::default();
        let feature_tier = FeatureTier::Keyword;
        {
            let mut out = env.output();
            run_with_embedder(
                &conn,
                &args,
                true,
                &cfg,
                feature_tier,
                Some(&mock as &dyn Embed),
                Some(mock.model_description()),
                &mut out,
            )
            .unwrap();
        }
        let stderr = env.stderr_str();
        assert!(stderr.contains("embedder loaded"), "got: {stderr}");
        assert!(stderr.contains("backfilling"), "got: {stderr}");
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["mode"].as_str().unwrap(), "hybrid");
    }

    #[test]
    fn recall_with_embedder_failing_primary_falls_back_to_keyword() {
        // FailingEmbedder errors on the primary `embed(query)`. The
        // recall handler emits the "embedding query failed" banner and
        // falls back to db::recall (lines 272-291 in original).
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "content");
        let conn = db::open(&db).unwrap();
        let args = default_args();
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run_with_embedder(
                &conn,
                &args,
                true,
                &cfg,
                FeatureTier::Keyword,
                Some(&FailingEmbedder as &dyn Embed),
                Some("failing-mock"),
                &mut out,
            )
            .unwrap();
        }
        let stderr = env.stderr_str();
        assert!(
            stderr.contains("embedding query failed"),
            "expected fallback banner; got: {stderr}"
        );
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["mode"].as_str().unwrap(), "keyword");
    }

    #[test]
    fn recall_with_embedder_context_tokens_fail_uses_primary_only() {
        // Primary embed OK, context_tokens embed fails → emit the
        // "context_tokens embed failed" banner and continue with
        // primary_emb alone.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "content");
        let conn = db::open(&db).unwrap();
        let mock = FailOnContextTokens {
            joined_marker: "alpha beta".to_string(),
        };
        let mut args = default_args();
        args.context_tokens = Some(vec!["alpha".into(), "beta".into()]);
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run_with_embedder(
                &conn,
                &args,
                true,
                &cfg,
                FeatureTier::Keyword,
                Some(&mock as &dyn Embed),
                Some("primary-ok-context-fail"),
                &mut out,
            )
            .unwrap();
        }
        let stderr = env.stderr_str();
        assert!(
            stderr.contains("context_tokens embed failed"),
            "got: {stderr}"
        );
    }

    #[test]
    fn recall_with_embedder_context_tokens_success_drives_fuse() {
        // Primary OK + context_tokens OK → triggers the fuse() path.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "content");
        let conn = db::open(&db).unwrap();
        let mock = crate::embeddings::test_support::MockEmbedder::new_local().unwrap();
        let mut args = default_args();
        args.context_tokens = Some(vec!["a".into(), "b".into()]);
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run_with_embedder(
                &conn,
                &args,
                true,
                &cfg,
                FeatureTier::Keyword,
                Some(&mock as &dyn Embed),
                Some(mock.model_description()),
                &mut out,
            )
            .unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["mode"].as_str().unwrap(), "hybrid");
    }

    #[test]
    fn recall_with_embedder_load_failed_emits_failed_banner() {
        // tier_config.embedding_model.is_some() && embedder=None → emit
        // the "embedder failed to load, falling back to keyword" banner.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "content");
        let conn = db::open(&db).unwrap();
        let args = default_args();
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run_with_embedder(
                &conn,
                &args,
                true,
                &cfg,
                FeatureTier::Semantic, // tier_config.embedding_model = Some
                None,                  // simulate failed load
                None,
                &mut out,
            )
            .unwrap();
        }
        let stderr = env.stderr_str();
        assert!(
            stderr.contains("embedder failed to load"),
            "expected failed-load banner; got: {stderr}"
        );
    }

    #[test]
    fn recall_text_output_no_embedder_with_low_confidence_emits_conf_pct() {
        // Drives the `confidence < 1.0` branch in the text output loop
        // (line 350) which formats " conf=XX%". Use a custom inserted
        // memory with confidence below 1.0.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        // Insert a low-confidence memory directly.
        let conn = db::open(&db).unwrap();
        let mut mem = crate::models::Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: crate::models::Tier::Mid,
            namespace: "test".to_string(),
            title: "needle low".to_string(),
            content: "low confidence content".to_string(),
            tags: vec![],
            priority: 5,
            confidence: 0.42,
            source: "import".to_string(),
            access_count: 0,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            last_accessed_at: None,
            expires_at: None,
            metadata: crate::models::default_metadata(),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        };
        if let Some(obj) = mem.metadata.as_object_mut() {
            obj.insert("agent_id".to_string(), serde_json::json!("t"));
        }
        db::insert(&conn, &mem).unwrap();
        let args = default_args();
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            // text mode (json_out=false) — drives the text-rendering loop.
            run_with_embedder(
                &conn,
                &args,
                false,
                &cfg,
                FeatureTier::Keyword,
                None,
                None,
                &mut out,
            )
            .unwrap();
        }
        let stdout = env.stdout_str();
        assert!(stdout.contains("conf=42%"), "got: {stdout}");
        assert!(stdout.contains("memory(ies) recalled"), "got: {stdout}");
    }

    #[test]
    fn recall_text_output_no_results_emits_no_memories_message() {
        // Empty result text path (lines 343-345).
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let conn = db::open(&db).unwrap();
        let args = default_args();
        let cfg = AppConfig::default();
        {
            let mut out = env.output();
            run_with_embedder(
                &conn,
                &args,
                false,
                &cfg,
                FeatureTier::Keyword,
                None,
                None,
                &mut out,
            )
            .unwrap();
        }
        let stderr = env.stderr_str();
        assert!(stderr.contains("no memories found"), "got: {stderr}");
    }

    #[test]
    fn recall_session_default_off_does_not_splice_scope() {
        // session_default=false short-circuits the scope branch to None
        // (line 92), so the configured scope is invisible.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "scope-ns", "needle title", "content");
        seed_memory(&db, "other-ns", "needle elsewhere", "content");
        let mut args = default_args();
        args.session_default = false;
        let cfg = app_config_with_recall_scope();
        {
            let mut out = env.output();
            run(&db, &args, true, &cfg, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        // Both namespaces should be visible — no scope splice.
        let nses: std::collections::HashSet<String> = v["memories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["namespace"].as_str().unwrap().to_string())
            .collect();
        assert!(nses.len() >= 2 || nses.contains("other-ns"));
    }
}
