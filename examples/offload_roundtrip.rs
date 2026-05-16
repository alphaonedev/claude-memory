// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Cookbook harness for the v0.7.0 QW-3 context-offload substrate.
//!
//! Drives the engine directly (no MCP, no daemon) so the cookbook
//! recipe at `cookbook/context-offload/01-offload-large-tool-output.sh`
//! is reproducible in <2 minutes from a clean checkout without an
//! Ollama dependency.
//!
//! Flags:
//!   --db <path>         SQLite path; created if missing.
//!   --input <path>      File to offload.
//!   --output <path>     Where deref writes the round-tripped bytes.
//!   --report <path>     JSON report (round-trip + tamper outcomes).

use std::path::PathBuf;

use ai_memory::offload::{ContextOffloader, OffloadConfig, OffloadError};
use ai_memory::storage as db;
use anyhow::{Context, Result, anyhow};
use rusqlite::params;
use sha2::{Digest, Sha256};

struct Args {
    db: PathBuf,
    input: PathBuf,
    output: PathBuf,
    report: PathBuf,
}

fn parse_args() -> Result<Args> {
    let mut db = None;
    let mut input = None;
    let mut output = None;
    let mut report = None;
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or_else(|| anyhow!("flag {flag} needs a value"))?;
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(value)),
            "--input" => input = Some(PathBuf::from(value)),
            "--output" => output = Some(PathBuf::from(value)),
            "--report" => report = Some(PathBuf::from(value)),
            other => return Err(anyhow!("unknown flag {other}")),
        }
    }
    Ok(Args {
        db: db.ok_or_else(|| anyhow!("--db required"))?,
        input: input.ok_or_else(|| anyhow!("--input required"))?,
        output: output.ok_or_else(|| anyhow!("--output required"))?,
        report: report.ok_or_else(|| anyhow!("--report required"))?,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn main() -> Result<()> {
    let args = parse_args()?;
    let content = std::fs::read_to_string(&args.input)
        .with_context(|| format!("read input {}", args.input.display()))?;
    let conn = db::open(&args.db).context("open db")?;
    let off = ContextOffloader::new(&conn, None, OffloadConfig::default());
    let result = off
        .offload(&content, "cookbook/offload", None, "ai:cookbook")
        .context("offload step")?;
    let deref = off.deref(&result.ref_id).context("deref step")?;
    std::fs::write(&args.output, deref.content.as_bytes())
        .with_context(|| format!("write output {}", args.output.display()))?;
    let round_trip_ok = deref.content == content && deref.sha256 == result.content_sha256;

    // Tamper sub-test: mutate the stored blob, expect deref to refuse.
    let tampered = {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut encoder = zstd::stream::write::Encoder::new(&mut buf, 3)?;
            encoder.write_all(b"REPLACED-TAMPERED-CONTENT")?;
            encoder.finish()?;
        }
        buf
    };
    conn.execute(
        "UPDATE offloaded_blobs SET content_zstd = ?1 WHERE ref_id = ?2",
        params![tampered, result.ref_id],
    )?;
    let tamper_refused = match off.deref(&result.ref_id) {
        Err(e) => e
            .downcast_ref::<OffloadError>()
            .map(|err| matches!(err, OffloadError::IntegrityFailed { .. }))
            .unwrap_or(false),
        Ok(_) => false,
    };

    let report_json = format!(
        "{{\n  \"ref_id\": \"{}\",\n  \"content_sha256\": \"{}\",\n  \"input_sha256\": \"{}\",\n  \"round_trip\": {},\n  \"tamper_refused\": {}\n}}\n",
        result.ref_id,
        result.content_sha256,
        sha256_hex(content.as_bytes()),
        round_trip_ok,
        tamper_refused,
    );
    std::fs::write(&args.report, report_json)?;
    if !round_trip_ok {
        std::process::exit(2);
    }
    if !tamper_refused {
        std::process::exit(3);
    }
    Ok(())
}
