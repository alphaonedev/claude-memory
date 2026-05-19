// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 #697 — Ed25519-signed forensic audit log.
//!
//! Every governance decision (allow / refuse / warn) emitted by the
//! agent-action engine OR the deferred-audit pipeline lands in an
//! append-only forensic log:
//!
//! ```text
//! <forensic_dir>/forensic-<YYYY-MM-DD>.jsonl
//! ```
//!
//! Each line is a JSON object:
//!
//! ```json
//! {
//!   "ts": "2026-05-18T12:34:56.000Z",
//!   "actor": "<agent_id>",
//!   "decision": "allow|refuse|warn",
//!   "kind": "<rule_kind>",
//!   "rule_id": "R001",
//!   "payload": { ... },
//!   "prev_hash": "<sha256-hex-of-prior-line-canonical-bytes>",
//!   "sig": "<base64-ed25519-over-canonical-bytes>"
//! }
//! ```
//!
//! Canonical bytes for hashing AND signing = the JSON serialisation
//! of the same object with `sig` cleared. Files are rotated by UTC
//! date; the chain `prev_hash` carries across file boundaries.
//! `verify_since` walks every file at or after `<ISO_DATE>` in
//! lexicographic order.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Datelike, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Sentinel `prev_hash` for the first line of a fresh chain.
pub const CHAIN_HEAD_PREV_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// File-name prefix for the daily-rotated forensic log files.
pub const FORENSIC_FILE_PREFIX: &str = "forensic-";

/// File-name suffix for the daily-rotated forensic log files.
pub const FORENSIC_FILE_SUFFIX: &str = ".jsonl";

/// A single signed forensic decision record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForensicDecision {
    pub ts: String,
    pub actor: String,
    pub decision: String,
    pub kind: String,
    pub rule_id: String,
    pub payload: serde_json::Value,
    pub prev_hash: String,
    pub sig: String,
}

impl ForensicDecision {
    /// Canonical bytes for hashing AND signing — `sig` zeroed.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut clone = self.clone();
        clone.sig.clear();
        serde_json::to_vec(&clone).expect("ForensicDecision always serialises")
    }

    /// Hex-encoded sha256 of the canonical bytes.
    #[must_use]
    pub fn self_hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.canonical_bytes());
        hex_encode(&h.finalize())
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    static HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

// ---------------------------------------------------------------------------
// Sink — process-wide writer + chain head
// ---------------------------------------------------------------------------

static SINK: OnceLock<Mutex<Option<ForensicSink>>> = OnceLock::new();

fn sink() -> &'static Mutex<Option<ForensicSink>> {
    SINK.get_or_init(|| Mutex::new(None))
}

struct ForensicSink {
    dir: PathBuf,
    last_hash: String,
    signing_key: Option<SigningKey>,
}

/// Initialise the forensic audit sink.
///
/// # Errors
/// - The directory cannot be created.
pub fn init(dir: &Path, signing_key: Option<SigningKey>) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating forensic audit dir {}", dir.display()))?;
    let last_hash = read_chain_tail(dir).unwrap_or_else(|| CHAIN_HEAD_PREV_HASH.to_string());
    let new_sink = ForensicSink {
        dir: dir.to_path_buf(),
        last_hash,
        signing_key,
    };
    let mut guard = sink()
        .lock()
        .map_err(|_| anyhow!("forensic sink mutex poisoned"))?;
    *guard = Some(new_sink);
    Ok(())
}

/// Tear down the sink (test-only convenience).
pub fn shutdown() {
    if let Ok(mut guard) = sink().lock() {
        *guard = None;
    }
}

/// `true` when [`init`] has been called and the sink is active.
#[must_use]
pub fn is_enabled() -> bool {
    sink().lock().map(|g| g.is_some()).unwrap_or(false)
}

/// Record a governance decision to the forensic log.
///
/// # Errors
/// - The current-day file cannot be opened for append.
/// - Serialisation fails.
/// - The mutex protecting the sink is poisoned.
pub fn try_record_decision(
    actor: &str,
    decision: &str,
    kind: &str,
    rule_id: &str,
    payload: serde_json::Value,
) -> Result<()> {
    let mut guard = sink()
        .lock()
        .map_err(|_| anyhow!("forensic sink mutex poisoned"))?;
    let Some(s) = guard.as_mut() else {
        return Ok(());
    };

    let now = Utc::now();
    let ts = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let prev_hash = s.last_hash.clone();

    let mut row = ForensicDecision {
        ts,
        actor: actor.to_string(),
        decision: decision.to_string(),
        kind: kind.to_string(),
        rule_id: rule_id.to_string(),
        payload,
        prev_hash,
        sig: String::new(),
    };

    if let Some(key) = &s.signing_key {
        let canonical = row.canonical_bytes();
        let sig: Signature = key.sign(&canonical);
        row.sig = B64.encode(sig.to_bytes());
    }

    let self_hash = row.self_hash();
    let line = serde_json::to_string(&row).context("serialising forensic row")?;

    let file_path = daily_path(&s.dir, &now);
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file_path)
        .with_context(|| format!("opening forensic log {}", file_path.display()))?;
    writeln!(f, "{line}")
        .with_context(|| format!("appending forensic line to {}", file_path.display()))?;
    f.flush().ok();

    s.last_hash = self_hash;
    Ok(())
}

/// Fire-and-forget wrapper. Errors logged + swallowed.
pub fn record_decision(
    actor: &str,
    decision: &str,
    kind: &str,
    rule_id: &str,
    payload: serde_json::Value,
) {
    if let Err(e) = try_record_decision(actor, decision, kind, rule_id, payload) {
        tracing::error!(
            target: "ai_memory::governance::audit",
            "forensic: emission failed: {e}"
        );
    }
}

fn daily_path(dir: &Path, when: &DateTime<Utc>) -> PathBuf {
    let date = when.format("%Y-%m-%d").to_string();
    dir.join(format!(
        "{FORENSIC_FILE_PREFIX}{date}{FORENSIC_FILE_SUFFIX}"
    ))
}

fn read_chain_tail(dir: &Path) -> Option<String> {
    let files = list_forensic_files(dir).ok()?;
    let last_file = files.last()?;
    let f = File::open(last_file).ok()?;
    let mut last_hash: Option<String> = None;
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(row) = serde_json::from_str::<ForensicDecision>(&line) {
            last_hash = Some(row.self_hash());
        }
    }
    last_hash
}

fn list_forensic_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("reading forensic dir {}", dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if name_str.starts_with(FORENSIC_FILE_PREFIX) && name_str.ends_with(FORENSIC_FILE_SUFFIX) {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

// ---------------------------------------------------------------------------
// Verification
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VerifyReport {
    pub total_lines: u64,
    pub unsigned_lines: u64,
    pub first_failure: Option<VerifyFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyFailure {
    pub line_number: u64,
    pub file: PathBuf,
    pub kind: VerifyFailureKind,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyFailureKind {
    Parse,
    ChainBreak,
    Signature,
}

/// Walk every forensic file under `dir` whose date is `>= since` and
/// verify the hash chain + every signature against `public_key`.
///
/// # Errors
/// - The directory cannot be enumerated.
/// - A file cannot be opened.
pub fn verify_since(
    dir: &Path,
    since: &str,
    public_key: Option<&VerifyingKey>,
) -> Result<VerifyReport> {
    let cutoff = parse_iso_date(since)?;
    let files = list_forensic_files(dir)?;
    let mut prev_hash = CHAIN_HEAD_PREV_HASH.to_string();
    let mut total: u64 = 0;
    let mut unsigned: u64 = 0;

    for file in &files {
        let date = file_date(file)?;
        if date >= cutoff {
            break;
        }
        let f = File::open(file).with_context(|| format!("opening {}", file.display()))?;
        for line in BufReader::new(f).lines() {
            let Ok(line) = line else { continue };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(row) = serde_json::from_str::<ForensicDecision>(&line) {
                prev_hash = row.self_hash();
            }
        }
    }

    for file in &files {
        let date = file_date(file)?;
        if date < cutoff {
            continue;
        }
        let f = File::open(file).with_context(|| format!("opening {}", file.display()))?;
        for (idx, line) in BufReader::new(f).lines().enumerate() {
            let line_no = (idx as u64) + 1;
            let line = line.with_context(|| format!("reading {}:{line_no}", file.display()))?;
            if line.trim().is_empty() {
                continue;
            }
            let row: ForensicDecision = match serde_json::from_str(&line) {
                Ok(r) => r,
                Err(e) => {
                    return Ok(VerifyReport {
                        total_lines: total,
                        unsigned_lines: unsigned,
                        first_failure: Some(VerifyFailure {
                            line_number: line_no,
                            file: file.clone(),
                            kind: VerifyFailureKind::Parse,
                            detail: format!("malformed JSON: {e}"),
                        }),
                    });
                }
            };

            total += 1;

            if row.prev_hash != prev_hash {
                return Ok(VerifyReport {
                    total_lines: total,
                    unsigned_lines: unsigned,
                    first_failure: Some(VerifyFailure {
                        line_number: line_no,
                        file: file.clone(),
                        kind: VerifyFailureKind::ChainBreak,
                        detail: format!(
                            "prev_hash mismatch: expected {prev_hash}, got {}",
                            row.prev_hash
                        ),
                    }),
                });
            }

            if row.sig.is_empty() {
                unsigned += 1;
            } else if let Some(pk) = public_key {
                let canonical = row.canonical_bytes();
                let sig_bytes = match B64.decode(row.sig.as_bytes()) {
                    Ok(b) => b,
                    Err(e) => {
                        return Ok(VerifyReport {
                            total_lines: total,
                            unsigned_lines: unsigned,
                            first_failure: Some(VerifyFailure {
                                line_number: line_no,
                                file: file.clone(),
                                kind: VerifyFailureKind::Signature,
                                detail: format!("base64 decode failed: {e}"),
                            }),
                        });
                    }
                };
                if sig_bytes.len() != 64 {
                    return Ok(VerifyReport {
                        total_lines: total,
                        unsigned_lines: unsigned,
                        first_failure: Some(VerifyFailure {
                            line_number: line_no,
                            file: file.clone(),
                            kind: VerifyFailureKind::Signature,
                            detail: format!("signature has {} bytes, expected 64", sig_bytes.len()),
                        }),
                    });
                }
                let mut sig_arr = [0u8; 64];
                sig_arr.copy_from_slice(&sig_bytes);
                let sig = Signature::from_bytes(&sig_arr);
                if let Err(e) = pk.verify(&canonical, &sig) {
                    return Ok(VerifyReport {
                        total_lines: total,
                        unsigned_lines: unsigned,
                        first_failure: Some(VerifyFailure {
                            line_number: line_no,
                            file: file.clone(),
                            kind: VerifyFailureKind::Signature,
                            detail: format!("signature verify failed: {e}"),
                        }),
                    });
                }
            }

            prev_hash = row.self_hash();
        }
    }

    Ok(VerifyReport {
        total_lines: total,
        unsigned_lines: unsigned,
        first_failure: None,
    })
}

fn parse_iso_date(s: &str) -> Result<i64> {
    let dt = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .with_context(|| format!("parsing --since {s} as YYYY-MM-DD"))?;
    Ok(i64::from(dt.year_ce().1 as i32) * 10000
        + i64::from(dt.month() as i32) * 100
        + i64::from(dt.day() as i32))
}

fn file_date(path: &Path) -> Result<i64> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("forensic file has non-UTF8 name: {}", path.display()))?;
    let stem = name
        .strip_prefix(FORENSIC_FILE_PREFIX)
        .and_then(|s| s.strip_suffix(FORENSIC_FILE_SUFFIX))
        .ok_or_else(|| {
            anyhow!("forensic file name not in forensic-YYYY-MM-DD.jsonl shape: {name}")
        })?;
    parse_iso_date(stem)
}

/// Load the daemon's signing key by agent_id. Returns `Ok(None)`
/// when no key is enrolled.
///
/// # Errors
/// - The key dir cannot be resolved.
pub fn load_daemon_signing_key(agent_id: &str) -> Result<Option<SigningKey>> {
    let dir = crate::identity::keypair::default_key_dir()?;
    if !dir.exists() {
        return Ok(None);
    }
    let kp = match crate::identity::keypair::load(agent_id, &dir) {
        Ok(k) => k,
        Err(_) => return Ok(None),
    };
    Ok(kp.private)
}

/// Load the daemon's verifying key by agent_id. Returns `Ok(None)`
/// when no key is enrolled.
///
/// # Errors
/// - The key dir cannot be resolved.
pub fn load_daemon_verifying_key(agent_id: &str) -> Result<Option<VerifyingKey>> {
    let dir = crate::identity::keypair::default_key_dir()?;
    if !dir.exists() {
        return Ok(None);
    }
    match crate::identity::keypair::load(agent_id, &dir) {
        Ok(kp) => Ok(Some(kp.public)),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;
    use std::sync::{Mutex, OnceLock};
    use tempfile::TempDir;

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn fresh_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn fresh_init(dir: &Path, key: Option<SigningKey>) {
        shutdown();
        // Defensive cleanup: Windows-only test flake (#899) where
        // `record_then_verify_signed_chain` counted 5 records instead
        // of 3, suggesting cross-test forensic-file bleed into the
        // tempdir. Clearing the dir before init guarantees the test
        // body starts from a known-empty state regardless of which
        // sibling test ran prior or what global-sink state lingered.
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        init(dir, key).expect("forensic init");
    }

    #[test]
    fn record_then_verify_signed_chain() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let key = fresh_key();
        let pubkey = key.verifying_key();
        fresh_init(tmp.path(), Some(key));
        for i in 0..3 {
            record_decision(
                "ai:test",
                "allow",
                "bash",
                &format!("R00{i}"),
                serde_json::json!({"command": format!("ls -la /{i}")}),
            );
        }
        shutdown();
        let since = Utc::now().format("%Y-%m-%d").to_string();
        let report = verify_since(tmp.path(), &since, Some(&pubkey)).expect("verify");
        assert!(report.first_failure.is_none(), "{:?}", report.first_failure);
        assert_eq!(report.total_lines, 3);
        assert_eq!(report.unsigned_lines, 0);
    }

    #[test]
    fn tampering_detected_by_verify() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        let key = fresh_key();
        let pubkey = key.verifying_key();
        fresh_init(tmp.path(), Some(key));
        record_decision(
            "ai:t",
            "refuse",
            "bash",
            "R001",
            serde_json::json!({"r":"no"}),
        );
        record_decision("ai:t", "allow", "bash", "R002", serde_json::json!({}));
        shutdown();
        let date = Utc::now().format("%Y-%m-%d").to_string();
        let path = tmp.path().join(format!("forensic-{date}.jsonl"));
        let body = std::fs::read_to_string(&path).unwrap();
        let tampered = body.replacen("\"ai:t\"", "\"evil\"", 1);
        std::fs::write(&path, tampered).unwrap();
        let report = verify_since(tmp.path(), &date, Some(&pubkey)).expect("verify");
        let failure = report.first_failure.expect("tamper must be flagged");
        assert!(matches!(
            failure.kind,
            VerifyFailureKind::Signature | VerifyFailureKind::ChainBreak
        ));
    }

    #[test]
    fn unsigned_rows_counted_not_failed() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        let tmp = TempDir::new().unwrap();
        fresh_init(tmp.path(), None);
        record_decision("ai:t", "allow", "bash", "R001", serde_json::json!({}));
        record_decision("ai:t", "allow", "bash", "R002", serde_json::json!({}));
        shutdown();
        let since = Utc::now().format("%Y-%m-%d").to_string();
        let report = verify_since(tmp.path(), &since, None).expect("verify");
        assert!(report.first_failure.is_none());
        assert_eq!(report.total_lines, 2);
        assert_eq!(report.unsigned_lines, 2);
    }

    #[test]
    fn parse_iso_date_basic() {
        assert!(parse_iso_date("2026-05-18").is_ok());
        assert!(parse_iso_date("not-a-date").is_err());
    }

    #[test]
    fn record_when_disabled_is_noop() {
        let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        shutdown();
        record_decision("ai:t", "allow", "bash", "R001", serde_json::json!({}));
        assert!(!is_enabled());
    }
}
