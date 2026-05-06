// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7.0.1 — closes #625.
//
// Cross-platform Rust port of the original `scripts/t0-orchestrate.sh`
// (E1, PR #621). The original bash script ran fine on macOS/Linux but
// CI on Windows had to skip its dry-run integration test
// (`tests/e1_orchestration_dry_run.rs` was `#![cfg(unix)]`-gated)
// because Windows runners don't ship `bash` by default. This binary
// drops `bash`, `curl`, and `jq` as runtime deps in favour of
// pure-Rust `clap` + `reqwest` + `serde_json`, so Windows CI now
// validates the same harness shape macOS/Linux do.
//
// The behaviour mirrors the original bash one-for-one:
//
//   - Same 4 LLMs (claude / gpt5 / gemini / grok) with the same
//     model ids, env-var names, and chat-completions endpoints.
//   - Same 6 Discovery Gate question ids
//     (T0-A2-CORE/FULL/GRAPH/NJG, T0-A1-CORE, T0-CONTRACT) with
//     the same expected canonical fragments grep-checked against
//     each LLM's response.
//   - Same dry-run text plan (the existing test asserts on the
//     `llm:      <id>` lines and the question ids — that contract
//     is preserved verbatim so the rewritten Rust test reads the
//     same shape the bash test did).
//   - Same `results/t0/<llm>-<ts>.json` + `summary-<ts>.md` output
//     layout for live runs.
//
// The orchestrator is still an out-of-band tool: live runs require
// API keys and burn vendor credits. CI exercises it in `--dry-run`
// mode only.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// LLM registry — (id, model, env-var, api-url) tuples. Same shape
// the original bash `LLMS=(...)` array carried.
// ---------------------------------------------------------------------------
struct Llm {
    id: &'static str,
    model: &'static str,
    env_var: &'static str,
    url: &'static str,
}

const LLMS: &[Llm] = &[
    Llm {
        id: "claude",
        model: "claude-sonnet-4-6",
        env_var: "ANTHROPIC_API_KEY",
        url: "https://api.anthropic.com/v1/messages",
    },
    Llm {
        id: "gpt5",
        model: "gpt-5",
        env_var: "OPENAI_API_KEY",
        url: "https://api.openai.com/v1/chat/completions",
    },
    Llm {
        id: "gemini",
        model: "gemini-3",
        env_var: "GOOGLE_API_KEY",
        url: "https://generativelanguage.googleapis.com/v1/models/gemini-3:generateContent",
    },
    Llm {
        id: "grok",
        model: "grok-4-3",
        env_var: "XAI_API_KEY",
        url: "https://api.x.ai/v1/chat/completions",
    },
];

// ---------------------------------------------------------------------------
// Discovery Gate questions — taken from `tests/calibration_t0.rs`.
// Each question is paired with its source profile (the
// capabilities-v3 profile to fetch system context from) and the
// natural-language prompt the LLM sees.
// ---------------------------------------------------------------------------
struct Question {
    qid: &'static str,
    profile: &'static str,
    text: &'static str,
}

const QUESTIONS: &[Question] = &[
    Question {
        qid: "T0-A2-CORE",
        profile: "core",
        text: "What tools do you have available right now? Answer in one sentence to a non-technical user.",
    },
    Question {
        qid: "T0-A2-FULL",
        profile: "full",
        text: "What tools do you have available right now? Answer in one sentence to a non-technical user.",
    },
    Question {
        qid: "T0-A2-GRAPH",
        profile: "graph",
        text: "What tools do you have available right now? Answer in one sentence to a non-technical user.",
    },
    Question {
        qid: "T0-A2-NJG",
        profile: "core",
        text: "Describe your memory tools to me without using any internal jargon.",
    },
    Question {
        qid: "T0-A1-CORE",
        profile: "core",
        text: "If you needed to use a memory tool that isn't currently loaded, what are all the recovery paths available?",
    },
    Question {
        qid: "T0-CONTRACT",
        profile: "core",
        text: "Confirm both your operator-facing summary and your user-facing description fields are populated.",
    },
];

// ---------------------------------------------------------------------------
// Expected canonical fragments — same list the bash
// `EXPECTED_FRAGMENTS=(...)` array pinned. Substring match: LLMs
// paraphrase the framing but should reproduce the load-bearing
// fragments verbatim.
// ---------------------------------------------------------------------------
const EXPECTED_FRAGMENTS: &[(&str, &str)] = &[
    ("T0-A2-CORE", "7 memory tools right now"),
    ("T0-A2-CORE", "43 more"),
    ("T0-A2-CORE", "available on demand"),
    ("T0-A2-FULL", "all 50 memory tools right now"),
    ("T0-A2-FULL", "Nothing more to load"),
    ("T0-A2-GRAPH", "18 memory tools right now"),
    ("T0-A2-GRAPH", "32 more"),
    ("T0-A2-NJG", "memory tools"),
    ("T0-A1-CORE", "--profile <family>"),
    ("T0-A1-CORE", "memory_load_family"),
    ("T0-A1-CORE", "memory_smart_load"),
    ("T0-A1-CORE", "JSON-RPC -32601"),
    ("T0-CONTRACT", "summary"),
    ("T0-CONTRACT", "to_describe_to_user"),
];

// ---------------------------------------------------------------------------
// CLI surface.
// ---------------------------------------------------------------------------
#[derive(Parser, Debug)]
#[command(
    name = "t0-orchestrate",
    about = "v0.7 E1 cross-LLM Discovery Gate orchestrator",
    long_about = "Fans the Discovery Gate questions out to the four covered \
                  frontier LLMs (Claude / GPT-5 / Gemini / Grok) and scores \
                  each response against canonical capabilities-v3 fragments."
)]
struct Cli {
    /// Print the orchestration plan + JSON envelope without
    /// making any API calls.
    #[arg(long)]
    dry_run: bool,

    /// Restrict the run to a single LLM by id (claude / gpt5 /
    /// gemini / grok). Default: run all four.
    #[arg(long)]
    llm: Option<String>,

    /// Override the env-var name to read the API key from. By
    /// default each LLM uses its registered env var. Most useful
    /// in combination with `--llm` to point a single provider at
    /// a non-default credential.
    #[arg(long, value_name = "NAME")]
    api_key_env: Option<String>,

    /// Path to write the JSON envelope to. In `--dry-run` this is
    /// the orchestration plan; in live mode it's the per-LLM
    /// results bundle. When omitted in live mode, falls back to
    /// `results/t0/<llm>-<timestamp>.json` per LLM (matching the
    /// original bash layout).
    #[arg(long, value_name = "PATH")]
    out: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// JSON envelope shape — emitted to `--out` (when set) and a copy
// printed to stdout under `--dry-run` for the integration test to
// parse. Field names are stable: tests/e1_orchestration_dry_run.rs
// asserts on them.
// ---------------------------------------------------------------------------
#[derive(Serialize, Deserialize, Debug)]
struct PlanEntry {
    llm: String,
    model: String,
    api_url: String,
    auth_env: String,
    qid: String,
    profile: String,
    question: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct DryRunEnvelope {
    mode: String,
    timestamp: String,
    only_llm: Option<String>,
    questions: usize,
    expected_fragments: usize,
    results_template: String,
    summary_template: String,
    plan: Vec<PlanEntry>,
}

// ---------------------------------------------------------------------------
// Plan construction — the (LLM × question) cartesian product the
// orchestrator will execute.
// ---------------------------------------------------------------------------
fn build_plan(only: Option<&str>, override_env: Option<&str>) -> Vec<PlanEntry> {
    let mut plan = Vec::with_capacity(LLMS.len() * QUESTIONS.len());
    for llm in LLMS {
        if let Some(filter) = only
            && filter != llm.id
        {
            continue;
        }
        let env_var = override_env.unwrap_or(llm.env_var);
        for q in QUESTIONS {
            plan.push(PlanEntry {
                llm: llm.id.to_string(),
                model: llm.model.to_string(),
                api_url: llm.url.to_string(),
                auth_env: env_var.to_string(),
                qid: q.qid.to_string(),
                profile: q.profile.to_string(),
                question: q.text.to_string(),
            });
        }
    }
    plan
}

fn timestamp_utc() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

// ---------------------------------------------------------------------------
// Dry-run output — preserves the original bash text format
// (`llm:      <id>` etc.) so the integration test's substring
// assertions stay structurally identical to the pre-port test, AND
// emits the JSON envelope (either to stdout when `--out` is
// omitted, or to the `--out` path when set).
// ---------------------------------------------------------------------------
fn run_dry(cli: &Cli) -> std::io::Result<()> {
    let timestamp = timestamp_utc();
    let plan = build_plan(cli.llm.as_deref(), cli.api_key_env.as_deref());

    let results_template = format!("results/t0/<llm>-{timestamp}.json");
    let summary_template = format!("results/t0/summary-{timestamp}.md");

    println!("==> t0-orchestrate");
    println!("    timestamp: {timestamp}");
    println!("    dry-run:   1");
    println!(
        "    only-llm:  {}",
        cli.llm.as_deref().unwrap_or("<all>")
    );
    println!("    results:   results/t0");
    println!("    questions: {}", QUESTIONS.len());
    println!();
    println!("plan:");
    for entry in &plan {
        // Preserve exact column widths the bash script printed —
        // the integration test substring-matches `llm:      <id>`.
        println!("  - llm:      {}", entry.llm);
        println!("    model:    {}", entry.model);
        println!("    api_url:  {}", entry.api_url);
        println!("    auth_env: {}", entry.auth_env);
        println!("    qid:      {}", entry.qid);
        println!("    profile:  {}", entry.profile);
        println!("    question: {}", entry.question);
    }
    println!();
    println!("expected_fragments: {}", EXPECTED_FRAGMENTS.len());
    println!("results_template:   {results_template}");
    println!("summary_template:   {summary_template}");

    let envelope = DryRunEnvelope {
        mode: "dry-run".to_string(),
        timestamp: timestamp.clone(),
        only_llm: cli.llm.clone(),
        questions: QUESTIONS.len(),
        expected_fragments: EXPECTED_FRAGMENTS.len(),
        results_template,
        summary_template,
        plan,
    };

    if let Some(path) = cli.out.as_ref() {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(path)?;
        serde_json::to_writer_pretty(&mut f, &envelope)?;
        f.write_all(b"\n")?;
        println!("wrote: {}", path.display());
    }

    println!("==> dry-run complete (no API calls made)");
    Ok(())
}

// ---------------------------------------------------------------------------
// Scoring — same substring contract the bash `score_response()`
// implemented. Returns (passed, total).
// ---------------------------------------------------------------------------
fn score_response(qid: &str, response: &str) -> (usize, usize) {
    let mut passed = 0_usize;
    let mut total = 0_usize;
    for (eid, frag) in EXPECTED_FRAGMENTS {
        if *eid != qid {
            continue;
        }
        total += 1;
        if response.contains(frag) {
            passed += 1;
        }
    }
    (passed, total)
}

// ---------------------------------------------------------------------------
// Live HTTP — provider-specific request bodies + response
// extraction. Same wire shapes the bash `do_call()` constructed
// with `jq` + `curl`.
// ---------------------------------------------------------------------------
fn live_call(
    client: &reqwest::blocking::Client,
    llm_id: &str,
    model: &str,
    url: &str,
    api_key: &str,
    system_ctx: &str,
    question: &str,
) -> String {
    let req = match llm_id {
        "claude" => client
            .post(url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&json!({
                "model": model,
                "max_tokens": 1024,
                "system": system_ctx,
                "messages": [{"role": "user", "content": question}],
            })),
        "gemini" => client
            .post(format!("{url}?key={api_key}"))
            .header("content-type", "application/json")
            .json(&json!({
                "system_instruction": {"parts": [{"text": system_ctx}]},
                "contents": [{"parts": [{"text": question}]}],
            })),
        // OpenAI- and xAI-compatible chat-completions surface.
        _ => client
            .post(url)
            .bearer_auth(api_key)
            .header("content-type", "application/json")
            .json(&json!({
                "model": model,
                "messages": [
                    {"role": "system", "content": system_ctx},
                    {"role": "user",   "content": question},
                ],
            })),
    };

    let Ok(resp) = req.send() else { return String::new() };
    let Ok(body) = resp.json::<Value>() else {
        return String::new();
    };

    let extracted = match llm_id {
        "claude" => body
            .get("content")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "gemini" => body
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("content"))
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.get(0))
            .and_then(|p| p.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => body
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    };

    extracted
}

fn run_live(cli: &Cli) -> std::io::Result<()> {
    let timestamp = timestamp_utc();
    let results_dir = PathBuf::from("results").join("t0");
    fs::create_dir_all(&results_dir)?;

    let plan = build_plan(cli.llm.as_deref(), cli.api_key_env.as_deref());

    println!("==> t0-orchestrate");
    println!("    timestamp: {timestamp}");
    println!("    dry-run:   0");
    println!(
        "    only-llm:  {}",
        cli.llm.as_deref().unwrap_or("<all>")
    );
    println!("    results:   {}", results_dir.display());
    println!("    questions: {}", QUESTIONS.len());
    println!();

    // Group plan by LLM so we write one results-file per provider.
    let mut by_llm: BTreeMap<&str, Vec<&PlanEntry>> = BTreeMap::new();
    for entry in &plan {
        by_llm.entry(entry.llm.as_str()).or_default().push(entry);
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("build reqwest client");

    let summary_path = results_dir.join(format!("summary-{timestamp}.md"));
    let mut summary = fs::File::create(&summary_path)?;
    writeln!(
        summary,
        "# T0 cross-LLM orchestration — {timestamp}\n\n| LLM | Question | Profile | Passed | Total |\n|---|---|---|---|---|"
    )?;

    for (llm_id, entries) in &by_llm {
        // Pull the API key by env-var name (per-entry — `--api-key-env`
        // overrides on the whole run, otherwise each LLM uses its
        // registered name). Skip silently if unset, matching bash.
        let env_var = entries[0].auth_env.as_str();
        let Ok(api_key) = std::env::var(env_var) else {
            println!("skip {llm_id} — {env_var} unset");
            continue;
        };
        if api_key.is_empty() {
            println!("skip {llm_id} — {env_var} empty");
            continue;
        }

        let out_path = cli.out.clone().unwrap_or_else(|| {
            results_dir.join(format!("{llm_id}-{timestamp}.json"))
        });
        let model = entries[0].model.as_str();
        let url = entries[0].api_url.as_str();
        println!("==> {llm_id} ({model}) -> {}", out_path.display());

        let mut results: Vec<Value> = Vec::with_capacity(entries.len());
        for entry in entries {
            let system_ctx = format!(
                "{{\"profile\":\"{}\",\"schema_version\":\"3\",\"summary\":\"<live-context-unavailable>\",\"to_describe_to_user\":\"<live-context-unavailable>\"}}",
                entry.profile
            );
            let response = live_call(
                &client,
                llm_id,
                model,
                url,
                &api_key,
                &system_ctx,
                &entry.question,
            );
            let (passed, total) = score_response(&entry.qid, &response);
            println!("    {} ({}): {passed}/{total}", entry.qid, entry.profile);
            writeln!(
                summary,
                "| {llm_id} | {} | {} | {passed} | {total} |",
                entry.qid, entry.profile
            )?;
            results.push(json!({
                "qid": entry.qid,
                "profile": entry.profile,
                "question": entry.question,
                "response": response,
                "passed": passed,
                "total": total,
            }));
        }

        let bundle = json!({
            "llm": llm_id,
            "model": model,
            "timestamp": timestamp,
            "results": results,
        });
        let mut f = fs::File::create(&out_path)?;
        serde_json::to_writer_pretty(&mut f, &bundle)?;
        f.write_all(b"\n")?;
    }

    println!();
    println!("==> summary: {}", summary_path.display());
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = if cli.dry_run {
        run_dry(&cli)
    } else {
        run_live(&cli)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("t0-orchestrate: {e}");
            ExitCode::FAILURE
        }
    }
}
