// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7.0 — Track E, task E1 (cross-platform port).
//
// `ai-memory-t0` is the Rust port of the historical
// `scripts/t0-orchestrate.sh`. It exercises the Discovery Gate T0
// calibration cells against four frontier LLMs and writes scored
// results under `results/t0/`. The dry-run path emits the same
// human-readable plan layout the bash original produced so the
// existing test guard (`tests/e1_orchestration_dry_run.rs`) keeps
// validating the harness shape.
//
// See `docs/v0.7/T0-ORCHESTRATION.md` for setup, interpretation,
// and re-run cadence.

#![forbid(unsafe_code)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Discovery Gate questions — taken from tests/calibration_t0.rs.
// Each question pairs with an expected canonical phrasing that the LLM
// response is grep-checked against (substring match — LLMs paraphrase
// the framing but should reproduce the load-bearing fragments verbatim).
// ---------------------------------------------------------------------------

struct Question {
    qid: &'static str,
    profile: &'static str,
    prompt: &'static str,
}

const QUESTIONS: &[Question] = &[
    Question {
        qid: "T0-A2-CORE",
        profile: "core",
        prompt: "What tools do you have available right now? Answer in one sentence to a non-technical user.",
    },
    Question {
        qid: "T0-A2-FULL",
        profile: "full",
        prompt: "What tools do you have available right now? Answer in one sentence to a non-technical user.",
    },
    Question {
        qid: "T0-A2-GRAPH",
        profile: "graph",
        prompt: "What tools do you have available right now? Answer in one sentence to a non-technical user.",
    },
    Question {
        qid: "T0-A2-NJG",
        profile: "core",
        prompt: "Describe your memory tools to me without using any internal jargon.",
    },
    Question {
        qid: "T0-A1-CORE",
        profile: "core",
        prompt: "If you needed to use a memory tool that isn't currently loaded, what are all the recovery paths available?",
    },
    Question {
        qid: "T0-CONTRACT",
        profile: "core",
        prompt: "Confirm both your operator-facing summary and your user-facing description fields are populated.",
    },
];

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
// LLM endpoints — chat-completions-style POST bodies built per-provider.
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

#[derive(Parser, Debug)]
#[command(
    name = "ai-memory-t0",
    about = "Discovery Gate T0 cross-LLM orchestration harness (v0.7 E1)",
    long_about = None,
)]
struct Cli {
    /// Print the plan and result-file template paths without making API calls.
    #[arg(long)]
    dry_run: bool,

    /// Restrict the run to a single LLM id (claude / gpt5 / gemini / grok).
    #[arg(long)]
    llm: Option<String>,
}

fn timestamp_utc() -> String {
    // Compact ISO-8601 timestamp in UTC, second-precision. Matches
    // the `%Y%m%dT%H%M%SZ` shape the bash original wrote so result
    // filenames stay sortable across the two implementations.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Civil-time conversion without pulling chrono: integer math
    // suffices for naming files. (Off-by-one minute over a leap
    // second is acceptable — these names are not consulted for
    // ordering precision finer than seconds.)
    let days = secs / 86_400;
    let sod = secs % 86_400;
    let hour = sod / 3600;
    let minute = (sod % 3600) / 60;
    let second = sod % 60;
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

#[allow(clippy::cast_possible_truncation)]
#[allow(clippy::cast_possible_wrap)]
#[allow(clippy::cast_sign_loss)]
fn civil_from_days(days: u64) -> (i64, u32, u32) {
    // Howard Hinnant's date algorithm — days since 1970-01-01 to (y,m,d).
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

fn score_response(qid: &str, response: &str) -> (u32, u32) {
    let mut total = 0u32;
    let mut passed = 0u32;
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

fn print_plan_for(llm: &Llm, q: &Question) {
    // Match the bash heredoc exactly so the test's substring asserts
    // continue to pass — note `llm:      <id>` (six spaces of padding).
    println!("  - llm:      {}", llm.id);
    println!("    model:    {}", llm.model);
    println!("    api_url:  {}", llm.url);
    println!("    auth_env: {}", llm.env_var);
    println!("    qid:      {}", q.qid);
    println!("    profile:  {}", q.profile);
    println!("    question: {}", q.prompt);
}

fn load_capabilities_payload(profile: &str, dry_run: bool) -> String {
    if dry_run {
        return format!(
            r#"{{"profile":"{profile}","schema_version":"3","summary":"<dry-run>","to_describe_to_user":"<dry-run>"}}"#
        );
    }

    let repo = repo_root();
    let mut bin = repo.join("target").join("release").join("ai-memory");
    if !is_executable(&bin) {
        bin = repo.join("target").join("debug").join("ai-memory");
    }
    if !is_executable(&bin) {
        eprintln!("warn: no built ai-memory binary; cargo build first");
        return format!(
            r#"{{"profile":"{profile}","schema_version":"3","summary":"<unavailable>","to_describe_to_user":"<unavailable>"}}"#
        );
    }

    let req = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_capabilities","arguments":{"accept":"v3"}}}"#;
    let child = ProcCommand::new(&bin)
        .args(["mcp", "--profile", profile])
        .env("AI_MEMORY_NO_CONFIG", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();

    let Ok(mut child) = child else {
        return format!(
            r#"{{"profile":"{profile}","schema_version":"3","summary":"<unavailable>","to_describe_to_user":"<unavailable>"}}"#
        );
    };
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = writeln!(stdin, "{req}");
    }
    let output = child.wait_with_output();
    let Ok(output) = output else {
        return format!(
            r#"{{"profile":"{profile}","schema_version":"3","summary":"<unavailable>","to_describe_to_user":"<unavailable>"}}"#
        );
    };
    let s = String::from_utf8_lossy(&output.stdout);
    s.lines().next().unwrap_or("").to_string()
}

fn repo_root() -> PathBuf {
    // The binary lives under `tools/t0-orchestrate/`; the parent's
    // parent is the repo root. Resolve via current_dir() so the
    // caller can override by invoking from elsewhere too.
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn is_executable(p: &Path) -> bool {
    p.exists()
        && match fs::metadata(p) {
            Ok(m) => m.is_file(),
            Err(_) => false,
        }
}

fn do_call(llm: &Llm, system_ctx: &str, question: &str) -> String {
    let Ok(key) = std::env::var(llm.env_var) else {
        return String::new();
    };
    if key.is_empty() {
        return String::new();
    }

    let Ok(client) = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_mins(1))
        .build()
    else {
        return String::new();
    };

    match llm.id {
        "claude" => {
            let body = json!({
                "model": llm.model,
                "max_tokens": 1024,
                "system": system_ctx,
                "messages": [{"role": "user", "content": question}],
            });
            let resp = client
                .post(llm.url)
                .header("x-api-key", &key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .send();
            extract_text(resp, |v| {
                v.get("content")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
        }
        "gpt5" | "grok" => {
            let body = json!({
                "model": llm.model,
                "messages": [
                    {"role": "system", "content": system_ctx},
                    {"role": "user", "content": question},
                ],
            });
            let resp = client
                .post(llm.url)
                .bearer_auth(&key)
                .header("content-type", "application/json")
                .json(&body)
                .send();
            extract_text(resp, |v| {
                v.get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
        }
        "gemini" => {
            let body = json!({
                "system_instruction": {"parts": [{"text": system_ctx}]},
                "contents": [{"parts": [{"text": question}]}],
            });
            let url = format!("{}?key={}", llm.url, key);
            let resp = client
                .post(&url)
                .header("content-type", "application/json")
                .json(&body)
                .send();
            extract_text(resp, |v| {
                v.get("candidates")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("content"))
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.get(0))
                    .and_then(|p| p.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
        }
        _ => String::new(),
    }
}

fn extract_text<F>(resp: reqwest::Result<reqwest::blocking::Response>, f: F) -> String
where
    F: Fn(&Value) -> String,
{
    let Ok(resp) = resp else { return String::new() };
    let Ok(v) = resp.json::<Value>() else {
        return String::new();
    };
    f(&v)
}

fn run_dry_run(only_llm: Option<&str>, timestamp: &str, results_dir: &Path) -> i32 {
    println!("plan:");
    for llm in LLMS {
        if let Some(only) = only_llm
            && only != llm.id
        {
            continue;
        }
        for q in QUESTIONS {
            print_plan_for(llm, q);
        }
    }
    println!();
    println!("expected_fragments: {}", EXPECTED_FRAGMENTS.len());
    println!(
        "results_template:   {}/<llm>-{}.json",
        results_dir.display(),
        timestamp
    );
    println!(
        "summary_template:   {}/summary-{}.md",
        results_dir.display(),
        timestamp
    );
    println!("==> dry-run complete (no API calls made)");
    0
}

fn run_live(only_llm: Option<&str>, timestamp: &str, results_dir: &Path) -> i32 {
    if let Err(e) = fs::create_dir_all(results_dir) {
        eprintln!("failed to create {}: {e}", results_dir.display());
        return 1;
    }
    let summary_md = results_dir.join(format!("summary-{timestamp}.md"));
    let mut summary = match fs::File::create(&summary_md) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("failed to create {}: {e}", summary_md.display());
            return 1;
        }
    };
    let _ = writeln!(summary, "# T0 cross-LLM orchestration — {timestamp}");
    let _ = writeln!(summary);
    let _ = writeln!(summary, "| LLM | Question | Profile | Passed | Total |");
    let _ = writeln!(summary, "|---|---|---|---|---|");

    for llm in LLMS {
        if let Some(only) = only_llm
            && only != llm.id
        {
            continue;
        }
        match std::env::var(llm.env_var) {
            Ok(v) if !v.is_empty() => {}
            _ => {
                println!("skip {} — {} unset", llm.id, llm.env_var);
                continue;
            }
        }

        let out_file = results_dir.join(format!("{}-{}.json", llm.id, timestamp));
        println!("==> {} ({}) -> {}", llm.id, llm.model, out_file.display());

        let mut results: Vec<Value> = Vec::new();

        for q in QUESTIONS {
            let system_ctx = load_capabilities_payload(q.profile, false);
            let response = do_call(llm, &system_ctx, q.prompt);
            let (passed, total) = score_response(q.qid, &response);
            results.push(json!({
                "qid": q.qid,
                "profile": q.profile,
                "question": q.prompt,
                "response": response,
                "passed": passed,
                "total": total,
            }));
            let _ = writeln!(
                summary,
                "| {} | {} | {} | {} | {} |",
                llm.id, q.qid, q.profile, passed, total
            );
            println!("    {} ({}): {}/{}", q.qid, q.profile, passed, total);
        }

        let envelope = json!({
            "llm": llm.id,
            "model": llm.model,
            "timestamp": timestamp,
            "results": results,
        });
        if let Err(e) = fs::write(
            &out_file,
            serde_json::to_string(&envelope).unwrap_or_default(),
        ) {
            eprintln!("failed to write {}: {e}", out_file.display());
        }
    }

    println!();
    println!("==> summary: {}", summary_md.display());
    0
}

fn main() {
    let cli = Cli::parse();

    let repo = repo_root();
    let results_dir = repo.join("results").join("t0");
    let timestamp = timestamp_utc();

    println!("==> t0-orchestrate");
    println!("    timestamp: {timestamp}");
    println!("    dry-run:   {}", u8::from(cli.dry_run));
    println!(
        "    only-llm:  {}",
        cli.llm.as_deref().unwrap_or("<all>")
    );
    println!("    results:   {}", results_dir.display());
    println!("    questions: {}", QUESTIONS.len());
    println!();

    let code = if cli.dry_run {
        run_dry_run(cli.llm.as_deref(), &timestamp, &results_dir)
    } else {
        run_live(cli.llm.as_deref(), &timestamp, &results_dir)
    };
    std::process::exit(code);
}
