// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7.0 — Track E, task E2 (cross-platform port).
//
// `ai-memory-post-ship-converge` is the Rust port of the historical
// `scripts/post-ship-converge.sh`. It installs the published
// `ai-memory` crate via cargo / brew / GitHub-release-binary and
// replays the 6 canonical Discovery Gate questions against it.
//
// The dry-run path (used by `tests/e2_post_ship_dry_run.rs`) skips
// install + spawn and emits the same structured JSON envelope shape
// the bash original produced — `verdict`, `results[]` array, per-
// question IDs — so the post-mortem playbook in
// `docs/v0.7/POST-SHIP-CONVERGENCE.md` keeps a stable contract.

#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcCommand, Stdio};

use clap::Parser;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// The 6 canonical Discovery Gate questions (sister to E1).
//
// Q1..Q3 are the user-facing T0-A2 calibration cells (one per
// representative profile: core/graph/full). Q4 is the operator-facing
// T0-A1 cell on --profile core. Q5 is the T0-NO-JARGON tone gate
// applied to --profile full. Q6 is the structural T0-CONTRACT cell on
// --profile core.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum ExpectKind {
    Exact,
    Contains,
    Absent,
    Schema,
}

impl ExpectKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Contains => "contains",
            Self::Absent => "absent",
            Self::Schema => "schema",
        }
    }
}

struct Question {
    id: &'static str,
    profile: &'static str,
    field: &'static str,
    kind: ExpectKind,
    expect_exact: Option<&'static str>,
    expect_list: &'static [&'static str],
}

const Q1_EXACT: &str = "I can directly use 7 memory tools right now (store, recall, list, get, search, ...). 43 more (update, delete, forget, gc, etc.) are available on demand — I can load them if you ask for something that needs them, or you can restart the server with a different profile.";
const Q2_EXACT: &str = "I can directly use 18 memory tools right now (store, recall, list, get, search, ...). 32 more (update, delete, forget, gc, etc.) are available on demand — I can load them if you ask for something that needs them, or you can restart the server with a different profile.";
const Q3_EXACT: &str = "I can directly use all 50 memory tools right now (store, recall, list, get, search, ...). Nothing more to load — the full memory surface is already active.";

const Q4_PATHS: &[&str] = &[
    "(a) restart the server with --profile <family>",
    "(b) call memory_load_family(family=<name>) — preferred",
    "(c) call memory_smart_load(intent='<plain language>') — easiest",
    "(d) call the tool by name and recover from JSON-RPC -32601",
];

const Q5_FORBIDDEN: &[&str] = &[
    "--profile <family>",
    "memory_load_family",
    "memory_smart_load",
    "JSON-RPC",
    "-32601",
    "tools/list",
    "memory_",
];

const QUESTIONS: &[Question] = &[
    Question {
        id: "Q1-T0-A2-CORE",
        profile: "core",
        field: "to_describe_to_user",
        kind: ExpectKind::Exact,
        expect_exact: Some(Q1_EXACT),
        expect_list: &[],
    },
    Question {
        id: "Q2-T0-A2-GRAPH",
        profile: "graph",
        field: "to_describe_to_user",
        kind: ExpectKind::Exact,
        expect_exact: Some(Q2_EXACT),
        expect_list: &[],
    },
    Question {
        id: "Q3-T0-A2-FULL",
        profile: "full",
        field: "to_describe_to_user",
        kind: ExpectKind::Exact,
        expect_exact: Some(Q3_EXACT),
        expect_list: &[],
    },
    Question {
        id: "Q4-T0-A1-CORE-RECOVERY-PATHS",
        profile: "core",
        field: "summary",
        kind: ExpectKind::Contains,
        expect_exact: None,
        expect_list: Q4_PATHS,
    },
    Question {
        id: "Q5-T0-NO-JARGON-FULL",
        profile: "full",
        field: "to_describe_to_user",
        kind: ExpectKind::Absent,
        expect_exact: None,
        expect_list: Q5_FORBIDDEN,
    },
    Question {
        id: "Q6-T0-CONTRACT-CORE",
        profile: "core",
        field: "(envelope)",
        kind: ExpectKind::Schema,
        expect_exact: None,
        expect_list: &[],
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstallMethod {
    Cargo,
    Brew,
    Binary,
}

impl InstallMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Brew => "brew",
            Self::Binary => "binary",
        }
    }
}

impl std::str::FromStr for InstallMethod {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "cargo" => Ok(Self::Cargo),
            "brew" => Ok(Self::Brew),
            "binary" => Ok(Self::Binary),
            other => Err(format!(
                "--method must be one of: cargo, brew, binary (got {other})"
            )),
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "ai-memory-post-ship-converge",
    about = "v0.7 E2 post-ship convergence verifier",
    long_about = None,
    disable_help_flag = false,
)]
struct Cli {
    /// Published ai-memory crate / tag version to verify (X.Y.Z).
    #[arg(long)]
    version: Option<String>,

    /// Skip install + spawn; emit the question set and the JSON
    /// verdict envelope with `dry_run: true`.
    #[arg(long)]
    dry_run: bool,

    /// Install method: cargo (default), brew, binary.
    #[arg(long, default_value = "cargo")]
    method: String,
}

const EXIT_GREEN: i32 = 0;
const EXIT_RED: i32 = 2;
const EXIT_USAGE: i32 = 3;
const EXIT_INSTALL: i32 = 4;

fn usage_err(msg: &str) -> ! {
    eprintln!("post-ship-converge: {msg}");
    eprintln!(
        "Usage: ai-memory-post-ship-converge --version <X.Y.Z> [--dry-run] [--method cargo|brew|binary]"
    );
    std::process::exit(EXIT_USAGE);
}

fn install_published_binary(method: InstallMethod, version: &str) -> Result<PathBuf, String> {
    let install_dir = mktemp_dir("ai-memory-e2-")?;

    match method {
        InstallMethod::Cargo => {
            let status = ProcCommand::new("cargo")
                .args([
                    "install",
                    "ai-memory",
                    "--version",
                    version,
                    "--root",
                    install_dir.to_str().ok_or("install dir not utf-8")?,
                ])
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .map_err(|e| format!("spawn cargo install failed: {e}"))?;
            if !status.success() {
                return Err("cargo install failed".to_string());
            }
            let bin = install_dir.join("bin").join("ai-memory");
            if !bin.exists() {
                return Err(format!(
                    "installed binary missing at {}",
                    bin.display()
                ));
            }
            Ok(bin)
        }
        InstallMethod::Brew => {
            let formula = format!("alphaonedev/tap/ai-memory@{version}");
            let status = ProcCommand::new("brew")
                .args(["install", &formula])
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .status()
                .map_err(|e| format!("spawn brew install failed: {e}"))?;
            if !status.success() {
                return Err("brew install failed".to_string());
            }
            let prefix = ProcCommand::new("brew")
                .arg("--prefix")
                .output()
                .map_err(|e| format!("brew --prefix failed: {e}"))?;
            let prefix = String::from_utf8_lossy(&prefix.stdout).trim().to_string();
            Ok(PathBuf::from(prefix).join("bin").join("ai-memory"))
        }
        InstallMethod::Binary => {
            let os = match env::consts::OS {
                "macos" => "darwin",
                other => other,
            };
            let arch = env::consts::ARCH;
            let url = format!(
                "https://github.com/alphaonedev/ai-memory-mcp/releases/download/v{version}/ai-memory-{os}-{arch}.tar.gz"
            );
            let tarball = install_dir.join("ai-memory.tar.gz");

            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_mins(2))
                .build()
                .map_err(|e| format!("http client build failed: {e}"))?;
            let resp = client
                .get(&url)
                .send()
                .map_err(|e| format!("download {url} failed: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("binary download HTTP {} for {url}", resp.status()));
            }
            let bytes = resp
                .bytes()
                .map_err(|e| format!("download {url} read failed: {e}"))?;
            fs::write(&tarball, &bytes)
                .map_err(|e| format!("write {} failed: {e}", tarball.display()))?;

            // Delegate untar to the host's `tar` tool; it ships on
            // every supported platform (macOS, Linux, Windows≥10).
            let status = ProcCommand::new("tar")
                .args(["-xzf"])
                .arg(&tarball)
                .arg("-C")
                .arg(&install_dir)
                .status()
                .map_err(|e| format!("spawn tar failed: {e}"))?;
            if !status.success() {
                return Err("tar -xzf failed".to_string());
            }

            #[cfg(windows)]
            let bin = install_dir.join("ai-memory.exe");
            #[cfg(not(windows))]
            let bin = install_dir.join("ai-memory");
            if !bin.exists() {
                return Err(format!("installed binary missing at {}", bin.display()));
            }
            Ok(bin)
        }
    }
}

fn mktemp_dir(prefix: &str) -> Result<PathBuf, String> {
    let base = env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = base.join(format!("{prefix}{pid}-{nanos}"));
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {} failed: {e}", dir.display()))?;
    Ok(dir)
}

/// Send a `memory_capabilities` request via the installed binary's
/// MCP stdio interface and return the parsed JSON response.
fn mcp_capabilities(bin: &Path, profile: &str) -> Option<Value> {
    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "memory_capabilities",
            "arguments": {"accept": "v3", "profile": profile},
        }
    });
    let mut child = ProcCommand::new(bin)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = writeln!(stdin, "{req}");
    }
    let output = child.wait_with_output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().rev().find(|l| !l.trim().is_empty())?;
    serde_json::from_str(line).ok()
}

/// Walk `value` recursively to find the first object containing
/// `field` and return that field's value.
fn find_field<'a>(value: &'a Value, field: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            if let Some(v) = map.get(field) {
                return Some(v);
            }
            for v in map.values() {
                if let Some(found) = find_field(v, field) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => arr.iter().find_map(|v| find_field(v, field)),
        _ => None,
    }
}

fn match_question(q: &Question, response: &Value) -> bool {
    if matches!(q.kind, ExpectKind::Schema) {
        let Some(schema_v) = find_field(response, "schema_version").and_then(Value::as_str) else {
            return false;
        };
        if schema_v != "3" {
            return false;
        }
        for fname in ["summary", "to_describe_to_user"] {
            match find_field(response, fname).and_then(Value::as_str) {
                Some(v) if !v.is_empty() => {}
                _ => return false,
            }
        }
        return true;
    }

    let actual = find_field(response, q.field)
        .and_then(Value::as_str)
        .unwrap_or("");
    match q.kind {
        ExpectKind::Exact => Some(actual) == q.expect_exact,
        ExpectKind::Contains => q.expect_list.iter().all(|needle| actual.contains(needle)),
        ExpectKind::Absent => q.expect_list.iter().all(|needle| !actual.contains(needle)),
        ExpectKind::Schema => unreachable!(),
    }
}

fn ask_one(q: &Question, bin: Option<&Path>, dry_run: bool) -> Value {
    if dry_run {
        return json!({
            "id": q.id,
            "profile": q.profile,
            "kind": q.kind.as_str(),
            "status": "SKIPPED_DRY_RUN",
        });
    }

    let Some(bin) = bin else {
        return json!({
            "id": q.id,
            "profile": q.profile,
            "kind": q.kind.as_str(),
            "status": "FAIL",
            "response": Value::Null,
        });
    };

    let response = mcp_capabilities(bin, q.profile).unwrap_or(Value::Null);
    if match_question(q, &response) {
        json!({
            "id": q.id,
            "profile": q.profile,
            "kind": q.kind.as_str(),
            "status": "PASS",
        })
    } else {
        json!({
            "id": q.id,
            "profile": q.profile,
            "kind": q.kind.as_str(),
            "status": "FAIL",
            "response": response,
        })
    }
}

fn main() {
    let cli = Cli::parse();

    let Some(version) = cli.version.clone() else {
        usage_err("--version <X.Y.Z> is required");
    };

    let method: InstallMethod = match cli.method.parse() {
        Ok(m) => m,
        Err(e) => usage_err(&e),
    };

    let bin_path: Option<PathBuf> = if cli.dry_run {
        None
    } else {
        match install_published_binary(method, &version) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("post-ship-converge: {e}");
                std::process::exit(EXIT_INSTALL);
            }
        }
    };

    let mut results: Vec<Value> = Vec::with_capacity(QUESTIONS.len());
    let mut pass_count = 0u32;
    let mut fail_count = 0u32;
    for q in QUESTIONS {
        let result = ask_one(q, bin_path.as_deref(), cli.dry_run);
        match result.get("status").and_then(Value::as_str) {
            Some("PASS") => pass_count += 1,
            Some("FAIL") => fail_count += 1,
            _ => {}
        }
        results.push(result);
    }

    let verdict = if cli.dry_run {
        "DRY_RUN"
    } else if fail_count == 0 {
        "GREEN"
    } else {
        "RED"
    };

    let envelope = json!({
        "task": "v0.7-E2",
        "version": version,
        "install_method": method.as_str(),
        "dry_run": cli.dry_run,
        "verdict": verdict,
        "pass_count": pass_count,
        "fail_count": fail_count,
        "question_count": QUESTIONS.len(),
        "results": results,
    });

    // Pretty-print so the envelope reads like the bash heredoc output.
    println!(
        "{}",
        serde_json::to_string_pretty(&envelope).unwrap_or_else(|_| envelope.to_string())
    );

    eprintln!(
        "post-ship-converge: verdict={verdict} version={version} pass={pass_count}/6 fail={fail_count}/6 dry_run={}",
        cli.dry_run
    );

    let code = if verdict == "RED" { EXIT_RED } else { EXIT_GREEN };
    std::process::exit(code);
}
