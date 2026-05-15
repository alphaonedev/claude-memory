// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-3 — context-offload substrate primitive.
//!
//! Substrate plumbing for the offload+deref pattern absorbed from
//! the Tencent comparison (2026-05-15). The FULL pattern (Mermaid
//! canvas, auto-cadence, node_id integration) targets v0.8.0; this
//! module ships the substrate so v0.8.0 has plumbing to call.
//!
//! # Pipeline
//!
//! - SHA-256 over the original bytes (decompressed) is the integrity
//!   commitment.
//! - `ref_id` format: `ofl_<base32-of-sha256-first-8-bytes>`. 13 chars
//!   of payload after the `ofl_` prefix — short enough to keep in an
//!   agent's working window, long enough that a 40-bit prefix
//!   collision is vanishingly rare for typical fleet scales.
//! - Body compressed with zstd level 3 — matches `memory_transcripts`
//!   (the existing sidechain transcripts pipeline) for cross-codebase
//!   parity.
//! - Ed25519 signature is over the canonical bundle
//!   `{ ref_id, content_sha256, stored_at, namespace }` encoded as
//!   deterministic CBOR (RFC 8949 §4.2.1). Same encoder family as
//!   `identity::sign::canonical_cbor` (the H2 link signer).
//! - A sibling row lands in `signed_events` with `event_type =
//!   context_offloaded` or `context_dereferenced`, binding the
//!   substrate write into the H5 audit chain.
//!
//! # Tamper handling
//!
//! `deref` recomputes the SHA-256 of the freshly-decompressed bytes
//! and refuses with `OffloadError::IntegrityFailed` when it disagrees
//! with the stored `content_sha256`. The signature is verified against
//! the storing agent's public key when that key is provided to the
//! offloader at construction; absent the key, the integrity check
//! alone is the load-bearing tamper guard.
//!
//! # Out of scope (v0.7.0)
//!
//! - Mermaid canvas integration (v0.8.0).
//! - Auto-cadence trigger from the recall pipeline (v0.8.0).
//! - `node_id` cross-link into the `memories` table (v0.8.0).

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, Verifier};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::identity::keypair::AgentKeypair;
use crate::signed_events::{SignedEvent, append_signed_event, payload_hash};

/// Default zstd compression level — matches the sidechain transcripts
/// pipeline (`transcripts::storage::ZSTD_LEVEL`).
const ZSTD_LEVEL: i32 = 3;

/// Hard cap on the decompressed size of a single offloaded blob. Same
/// 16 MiB ceiling the transcripts module enforces — defends against
/// pathological zstd bombs landing through `deref`. v0.8.0 may raise
/// this for the Mermaid-canvas use case after threat-modelling.
pub const MAX_DECOMPRESSED_BYTES: usize = 16 * 1024 * 1024;

/// Default per-blob byte limit when no namespace policy override is set.
/// 1 MiB — Tencent's offload primitive uses ~256 KB chunks; 1 MiB
/// gives headroom for batched tool outputs without crossing the
/// hostile-bomb threshold above.
pub const DEFAULT_MAX_OFFLOAD_BLOB_BYTES: u32 = 1_048_576;

/// RFC 4648 base32 alphabet (without padding). Used to encode the
/// 8-byte prefix of the content's SHA-256 into a 13-char `ref_id`
/// body. Avoids pulling a one-trick crate (no `base32` / `data-
/// encoding` is currently in the dependency tree).
const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// `ref_id` prefix — `ofl_` keeps the offload class identifiable in
/// audit logs, recall queries, and operator dashboards. Pairs the
/// `mem-` / `lnk-` ergonomic convention used elsewhere in the
/// substrate.
const REF_ID_PREFIX: &str = "ofl_";

/// Outcome of [`ContextOffloader::offload`]. Callers persist
/// `ref_id` and discard the content payload — that is the whole
/// point of offload+deref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffloadResult {
    pub ref_id: String,
    pub content_sha256: String,
    pub stored_at: i64,
}

/// Outcome of [`ContextOffloader::deref`]. Returns the original
/// (decompressed) content alongside the metadata that committed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerefResult {
    pub content: String,
    pub stored_at: i64,
    pub sha256: String,
}

/// Domain errors callers may want to discriminate on (size limits,
/// integrity failures, signature mismatches). All other failure modes
/// bubble through `anyhow::Error`. `Display` and `std::error::Error`
/// are implemented by hand to avoid pulling the optional `thiserror`
/// crate into the default feature set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OffloadError {
    SizeLimitExceeded { actual: usize, limit: usize },
    IntegrityFailed { ref_id: String },
    SignatureFailed { ref_id: String },
    NotFound { ref_id: String },
}

impl std::fmt::Display for OffloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SizeLimitExceeded { actual, limit } => {
                write!(f, "offload blob {actual} bytes exceeds policy max {limit}")
            }
            Self::IntegrityFailed { ref_id } => write!(
                f,
                "offloaded blob {ref_id} integrity check failed (content tampered)"
            ),
            Self::SignatureFailed { ref_id } => {
                write!(f, "offloaded blob {ref_id} signature verification failed")
            }
            Self::NotFound { ref_id } => write!(f, "offloaded blob {ref_id} not found"),
        }
    }
}

impl std::error::Error for OffloadError {}

/// Static configuration consumed by [`ContextOffloader`].
#[derive(Debug, Clone)]
pub struct OffloadConfig {
    /// Hard ceiling on the decompressed content length (bytes).
    /// Callers can shrink this below [`DEFAULT_MAX_OFFLOAD_BLOB_BYTES`]
    /// per namespace via the `max_offload_blob_bytes` policy knob in
    /// v0.8.0 (substrate-only in v0.7.0).
    pub max_offload_blob_bytes: u32,
    /// Default TTL applied when the caller passes `ttl_seconds = None`.
    /// `None` (the default) means "permanent until explicit operator
    /// delete".
    pub default_offload_ttl_seconds: Option<u64>,
}

impl Default for OffloadConfig {
    fn default() -> Self {
        Self {
            max_offload_blob_bytes: DEFAULT_MAX_OFFLOAD_BLOB_BYTES,
            default_offload_ttl_seconds: None,
        }
    }
}

/// Substrate-level engine for offload+deref. Composed from the
/// caller's keypair, the existing SQLite connection, and the
/// `OffloadConfig` defaults.
pub struct ContextOffloader<'a> {
    conn: &'a Connection,
    signer: Option<&'a AgentKeypair>,
    config: OffloadConfig,
}

impl<'a> ContextOffloader<'a> {
    /// Construct a new offloader. Pass `signer = None` for read-only
    /// `deref` workflows.
    #[must_use]
    pub fn new(
        conn: &'a Connection,
        signer: Option<&'a AgentKeypair>,
        config: OffloadConfig,
    ) -> Self {
        Self {
            conn,
            signer,
            config,
        }
    }

    /// Offload `content` and return the `ref_id` callers persist in
    /// place of the full payload.
    ///
    /// # Errors
    ///
    /// - [`OffloadError::SizeLimitExceeded`] when `content` is larger
    ///   than the configured per-blob ceiling.
    /// - `anyhow::Error` for zstd / SQLite / signing failures.
    pub fn offload(
        &self,
        content: &str,
        namespace: &str,
        ttl_seconds: Option<u64>,
        agent_id: &str,
    ) -> Result<OffloadResult> {
        let limit = self.config.max_offload_blob_bytes as usize;
        if content.len() > limit {
            return Err(anyhow!(OffloadError::SizeLimitExceeded {
                actual: content.len(),
                limit,
            }));
        }
        // SHA-256 of the original bytes — the integrity commitment.
        let sha = sha256_hex(content.as_bytes());
        let ref_id = ref_id_from_sha(&sha);
        let stored_at = now_unix_seconds();
        let effective_ttl = ttl_seconds.or(self.config.default_offload_ttl_seconds);
        // Compress AFTER the integrity hash is taken — the stored
        // sha256 commits to the ORIGINAL bytes so a future codec
        // upgrade can decode legacy rows without breaking the
        // integrity check.
        let blob = zstd_compress(content.as_bytes()).context("zstd compression failed")?;

        // Canonical signing bundle. `i64::try_from` keeps the encoded
        // bytes byte-stable for the verifier; an `i128`-shaped value
        // would never survive the round-trip through SQLite anyway.
        let stored_at_signed: i64 = stored_at;
        let signature_b64 = if let Some(keypair) = self.signer {
            let payload = canonical_payload(&ref_id, &sha, stored_at_signed, namespace)?;
            let signing = keypair.private.as_ref().with_context(|| {
                format!(
                    "AgentKeypair for {} has no private key — cannot sign offload",
                    keypair.agent_id
                )
            })?;
            URL_SAFE_NO_PAD.encode(signing.sign(&payload).to_bytes())
        } else {
            String::new()
        };

        let ttl_param: Option<i64> = effective_ttl.and_then(|n| i64::try_from(n).ok());
        self.conn
            .execute(
                "INSERT INTO offloaded_blobs (
                    ref_id, namespace, content_zstd, content_sha256,
                    stored_at, ttl_seconds, agent_id, signature_b64
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(ref_id) DO UPDATE SET
                    namespace = excluded.namespace,
                    content_zstd = excluded.content_zstd,
                    content_sha256 = excluded.content_sha256,
                    stored_at = excluded.stored_at,
                    ttl_seconds = excluded.ttl_seconds,
                    agent_id = excluded.agent_id,
                    signature_b64 = excluded.signature_b64",
                params![
                    ref_id,
                    namespace,
                    blob,
                    sha,
                    stored_at,
                    ttl_param,
                    agent_id,
                    signature_b64,
                ],
            )
            .context("INSERT into offloaded_blobs failed")?;

        // Audit: sibling row in signed_events binds this write to the
        // H5 cross-row hash chain so a downstream auditor can replay
        // the exact offload event without diffing the mutable
        // offloaded_blobs table.
        append_audit_row(
            self.conn,
            agent_id,
            "context_offloaded",
            &ref_id,
            &sha,
            namespace,
            stored_at_signed,
            &signature_b64,
        )?;

        Ok(OffloadResult {
            ref_id,
            content_sha256: sha,
            stored_at: stored_at_signed,
        })
    }

    /// Dereference a `ref_id` and return the original content.
    ///
    /// # Errors
    ///
    /// - [`OffloadError::NotFound`] when `ref_id` has no row.
    /// - [`OffloadError::IntegrityFailed`] when the decompressed
    ///   content's SHA-256 disagrees with the stored hash (tamper).
    /// - [`OffloadError::SignatureFailed`] when a signer was provided
    ///   and the stored Ed25519 signature fails to verify.
    pub fn deref(&self, ref_id: &str) -> Result<DerefResult> {
        let row: Option<(Vec<u8>, String, i64, String, String, String)> = self
            .conn
            .query_row(
                "SELECT content_zstd, content_sha256, stored_at, namespace,
                        agent_id, signature_b64
                 FROM offloaded_blobs WHERE ref_id = ?1",
                params![ref_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()
            .context("SELECT offloaded_blobs failed")?;

        let (blob, stored_sha, stored_at, namespace, agent_id, signature_b64) =
            row.ok_or_else(|| {
                anyhow!(OffloadError::NotFound {
                    ref_id: ref_id.to_string(),
                })
            })?;

        // Optional: verify the signature against the supplied key BEFORE
        // decompressing. Catches tampered blobs early without the zstd
        // round-trip cost. Skipped when the offloader has no keypair
        // (read-only workflows).
        if let Some(keypair) = self.signer {
            if !signature_b64.is_empty() {
                let payload = canonical_payload(ref_id, &stored_sha, stored_at, &namespace)?;
                let sig_bytes = URL_SAFE_NO_PAD
                    .decode(signature_b64.as_bytes())
                    .context("decode stored signature_b64")?;
                let sig_arr: [u8; 64] = sig_bytes
                    .as_slice()
                    .try_into()
                    .context("stored signature is not 64 bytes")?;
                let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
                if keypair.public.verify(&payload, &sig).is_err() {
                    return Err(anyhow!(OffloadError::SignatureFailed {
                        ref_id: ref_id.to_string(),
                    }));
                }
            }
        }

        let bytes = zstd_decompress(&blob).context("zstd decompression failed")?;
        // Refuse to surface non-UTF-8 content — the offload API is
        // string-shaped at the input boundary, so a non-UTF-8 stream
        // is by definition a tampered or corrupted row.
        let content = String::from_utf8(bytes).map_err(|_| OffloadError::IntegrityFailed {
            ref_id: ref_id.to_string(),
        })?;
        let recomputed = sha256_hex(content.as_bytes());
        if recomputed != stored_sha {
            return Err(anyhow!(OffloadError::IntegrityFailed {
                ref_id: ref_id.to_string(),
            }));
        }

        append_audit_row(
            self.conn,
            &agent_id,
            "context_dereferenced",
            ref_id,
            &stored_sha,
            &namespace,
            stored_at,
            &signature_b64,
        )?;

        Ok(DerefResult {
            content,
            stored_at,
            sha256: stored_sha,
        })
    }
}

/// SHA-256 hex-string helper.
fn sha256_hex(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    bytes_to_hex(&hasher.finalize())
}

/// Lower-case hex encoding of an arbitrary byte slice. Hand-rolled to
/// avoid a `hex` crate dep.
fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }
    out
}

/// RFC 4648 base32 (no padding) of `bytes`. Matches the alphabet used
/// in standard CLI output for short identifiers.
fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity((bytes.len() * 8 + 4) / 5);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let idx = ((buffer >> bits) & 0x1F) as usize;
            out.push(BASE32_ALPHABET[idx] as char);
        }
    }
    if bits > 0 {
        let idx = ((buffer << (5 - bits)) & 0x1F) as usize;
        out.push(BASE32_ALPHABET[idx] as char);
    }
    out
}

/// `ofl_<base32-of-sha256-first-8-bytes>`. Pure function of the
/// hex-encoded SHA so callers can reconstruct the id offline.
fn ref_id_from_sha(sha_hex: &str) -> String {
    // First 8 bytes = first 16 hex chars.
    let mut first_8 = [0u8; 8];
    for (i, byte) in first_8.iter_mut().enumerate() {
        let hi = hex_nibble(sha_hex.as_bytes()[i * 2]);
        let lo = hex_nibble(sha_hex.as_bytes()[i * 2 + 1]);
        *byte = (hi << 4) | lo;
    }
    format!("{REF_ID_PREFIX}{}", base32_encode(&first_8))
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

/// RFC 8949 §4.2.1 deterministic CBOR over `{ ref_id, content_sha256,
/// stored_at, namespace }`. Same encoder family as the H2 link
/// signer; map keys are sorted lexicographically by the underlying
/// BTreeMap iteration.
fn canonical_payload(
    ref_id: &str,
    content_sha256: &str,
    stored_at: i64,
    namespace: &str,
) -> Result<Vec<u8>> {
    let mut map: BTreeMap<&str, ciborium::Value> = BTreeMap::new();
    map.insert(
        "content_sha256",
        ciborium::Value::Text(content_sha256.to_string()),
    );
    map.insert("namespace", ciborium::Value::Text(namespace.to_string()));
    map.insert("ref_id", ciborium::Value::Text(ref_id.to_string()));
    map.insert("stored_at", ciborium::Value::Integer(stored_at.into()));
    let value = ciborium::Value::Map(
        map.into_iter()
            .map(|(k, v)| (ciborium::Value::Text(k.to_string()), v))
            .collect(),
    );
    let mut buf = Vec::new();
    ciborium::into_writer(&value, &mut buf).context("encode canonical offload payload")?;
    Ok(buf)
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn zstd_compress(input: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut out = Vec::with_capacity(input.len() / 4 + 64);
    {
        let mut encoder = zstd::stream::write::Encoder::new(&mut out, ZSTD_LEVEL)?;
        encoder.write_all(input)?;
        encoder.finish()?;
    }
    Ok(out)
}

fn zstd_decompress(input: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let init_cap = std::cmp::min(input.len() * 4, MAX_DECOMPRESSED_BYTES);
    let mut out = Vec::with_capacity(init_cap);
    let mut decoder = zstd::stream::read::Decoder::new(input)?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = decoder.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if out.len().saturating_add(n) > MAX_DECOMPRESSED_BYTES {
            return Err(anyhow!(
                "offloaded blob decompression exceeded {MAX_DECOMPRESSED_BYTES} byte cap"
            ));
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

/// Audit-row helper. Keeps the offload + deref call sites
/// symmetrical — both event types commit to the same canonical
/// payload bytes, so a downstream verifier can re-derive the hash
/// without branching on event type.
fn append_audit_row(
    conn: &Connection,
    agent_id: &str,
    event_type: &str,
    ref_id: &str,
    content_sha256: &str,
    namespace: &str,
    stored_at: i64,
    signature_b64: &str,
) -> Result<()> {
    let payload = canonical_payload(ref_id, content_sha256, stored_at, namespace)?;
    let hash = payload_hash(&payload);
    let signature_bytes = if signature_b64.is_empty() {
        None
    } else {
        Some(
            URL_SAFE_NO_PAD
                .decode(signature_b64.as_bytes())
                .context("decode signature_b64 for audit row")?,
        )
    };
    let attest_level = if signature_bytes.is_some() {
        "signed"
    } else {
        "unsigned"
    };
    let event = SignedEvent {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        event_type: event_type.to_string(),
        payload_hash: hash,
        signature: signature_bytes,
        attest_level: attest_level.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        prev_hash: Vec::new(),
        sequence: 0,
    };
    append_signed_event(conn, &event)?;
    Ok(())
}

/// Daily TTL sweep. Removes every blob whose `stored_at +
/// ttl_seconds < now`. Bounded to `max_per_run` rows per call so a
/// pathological backlog can't monopolise the connection; callers
/// (the daemon background loop) re-invoke at the configured cadence.
///
/// `sleep_between_deletes` is honoured between row deletions to keep
/// the connection lock window short under contended write traffic.
///
/// # Errors
///
/// Bubbles SQLite errors. A successful run returns the count of
/// deleted rows.
pub fn sweep_expired(
    conn: &Connection,
    now_unix: i64,
    max_per_run: usize,
    sleep_between_deletes: std::time::Duration,
) -> Result<usize> {
    let limit_i64 = i64::try_from(max_per_run).unwrap_or(i64::MAX);
    let mut stmt = conn
        .prepare(
            "SELECT ref_id FROM offloaded_blobs
             WHERE ttl_seconds IS NOT NULL
               AND (stored_at + ttl_seconds) < ?1
             ORDER BY stored_at ASC
             LIMIT ?2",
        )
        .context("prepare TTL sweep select")?;
    let candidates: Vec<String> = stmt
        .query_map(params![now_unix, limit_i64], |r| r.get::<_, String>(0))
        .context("execute TTL sweep select")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("collect TTL sweep candidates")?;
    drop(stmt);

    let mut deleted = 0usize;
    for ref_id in candidates {
        conn.execute(
            "DELETE FROM offloaded_blobs WHERE ref_id = ?1",
            params![ref_id],
        )
        .with_context(|| format!("DELETE offloaded_blob {ref_id}"))?;
        deleted += 1;
        if !sleep_between_deletes.is_zero() {
            std::thread::sleep(sleep_between_deletes);
        }
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage as db;
    use std::path::Path;

    fn fresh_db() -> Connection {
        db::open(Path::new(":memory:")).expect("open in-memory db")
    }

    #[test]
    fn ref_id_is_stable_for_identical_content() {
        let a = ref_id_from_sha(&sha256_hex(b"hello world"));
        let b = ref_id_from_sha(&sha256_hex(b"hello world"));
        assert_eq!(a, b);
        assert!(a.starts_with("ofl_"));
        // 8 bytes = 64 bits = 13 base32 chars (8 * 8 / 5 = 12.8, ceil = 13).
        assert_eq!(a.len(), "ofl_".len() + 13);
    }

    #[test]
    fn ref_id_differs_for_distinct_content() {
        let a = ref_id_from_sha(&sha256_hex(b"alpha"));
        let b = ref_id_from_sha(&sha256_hex(b"beta"));
        assert_ne!(a, b);
    }

    #[test]
    fn canonical_payload_is_deterministic() {
        let p1 = canonical_payload("ofl_X", "deadbeef", 1234, "ns").unwrap();
        let p2 = canonical_payload("ofl_X", "deadbeef", 1234, "ns").unwrap();
        assert_eq!(p1, p2);
    }

    #[test]
    fn offload_deref_round_trip_no_signer() {
        let conn = fresh_db();
        let off = ContextOffloader::new(&conn, None, OffloadConfig::default());
        let content = "the quick brown fox jumps over the lazy dog";
        let r = off
            .offload(content, "ns/test", None, "ai:alice")
            .expect("offload");
        let back = off.deref(&r.ref_id).expect("deref");
        assert_eq!(back.content, content);
        assert_eq!(back.sha256, r.content_sha256);
    }

    #[test]
    fn offload_refuses_oversize_blob() {
        let conn = fresh_db();
        let cfg = OffloadConfig {
            max_offload_blob_bytes: 16,
            ..Default::default()
        };
        let off = ContextOffloader::new(&conn, None, cfg);
        let err = off
            .offload("0123456789ABCDEF_extra", "ns", None, "ai:alice")
            .err()
            .expect("size error");
        let downcast = err
            .downcast_ref::<OffloadError>()
            .expect("OffloadError variant");
        matches!(downcast, OffloadError::SizeLimitExceeded { .. });
    }

    #[test]
    fn deref_refuses_when_content_tampered() {
        let conn = fresh_db();
        let off = ContextOffloader::new(&conn, None, OffloadConfig::default());
        let r = off
            .offload("hello world", "ns", None, "ai:alice")
            .expect("offload");

        // Swap the stored zstd blob for one whose decompressed bytes
        // do NOT match the stored sha256.
        let tampered = zstd_compress(b"GOODBYE WORLD").expect("compress");
        conn.execute(
            "UPDATE offloaded_blobs SET content_zstd = ?1 WHERE ref_id = ?2",
            params![tampered, r.ref_id],
        )
        .expect("tamper");

        let err = off.deref(&r.ref_id).err().expect("deref must reject");
        let downcast = err.downcast_ref::<OffloadError>().expect("OffloadError");
        assert!(matches!(downcast, OffloadError::IntegrityFailed { .. }));
    }

    #[test]
    fn deref_refuses_unknown_ref_id() {
        let conn = fresh_db();
        let off = ContextOffloader::new(&conn, None, OffloadConfig::default());
        let err = off.deref("ofl_DOESNOTEXIST").err().expect("not found");
        let downcast = err.downcast_ref::<OffloadError>().expect("OffloadError");
        assert!(matches!(downcast, OffloadError::NotFound { .. }));
    }

    #[test]
    fn sweep_purges_expired_rows() {
        let conn = fresh_db();
        let off = ContextOffloader::new(&conn, None, OffloadConfig::default());
        // Two TTL'd rows, one permanent row.
        let a = off
            .offload("alpha", "ns", Some(60), "ai:alice")
            .expect("offload a");
        let b = off
            .offload("beta", "ns", Some(60), "ai:alice")
            .expect("offload b");
        let c = off
            .offload("gamma", "ns", None, "ai:alice")
            .expect("offload c");

        // Sweep with `now` well beyond stored_at + 60s.
        let future = a.stored_at + 60 * 60;
        let deleted = sweep_expired(&conn, future, 1000, std::time::Duration::ZERO).expect("sweep");
        assert_eq!(deleted, 2);

        // a + b are gone; c (permanent) remains.
        assert!(off.deref(&a.ref_id).is_err());
        assert!(off.deref(&b.ref_id).is_err());
        assert!(off.deref(&c.ref_id).is_ok());
    }

    #[test]
    fn signed_events_chain_captures_offload_and_deref() {
        let conn = fresh_db();
        let off = ContextOffloader::new(&conn, None, OffloadConfig::default());
        let r = off
            .offload("traced", "ns", None, "ai:alice")
            .expect("offload");
        let _ = off.deref(&r.ref_id).expect("deref");
        let rows = crate::signed_events::list_signed_events(&conn, None, 100, 0).expect("list");
        let kinds: Vec<&str> = rows.iter().map(|r| r.event_type.as_str()).collect();
        assert!(kinds.contains(&"context_offloaded"));
        assert!(kinds.contains(&"context_dereferenced"));
    }
}
