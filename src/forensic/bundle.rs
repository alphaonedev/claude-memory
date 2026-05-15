// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L2-5 (issue #670) — forensic evidence bundle.
//!
//! This module assembles and verifies the procurement-grade evidence
//! tarball produced by `ai-memory export-forensic-bundle`. The bundle
//! is the OSS surface for the `AgenticMem Attest` tier — a single
//! tar file an external auditor can re-verify with no network and no
//! daemon state, just the public keys of the signing agents.
//!
//! ## Bundle layout
//!
//! ```text
//!     <bundle>.tar
//!       manifest.json                 — bundle metadata + SHA-256s + sig
//!       verification.json             — L1-3 `verify-reflection-chain` JSON
//!       memories/<id>.json            — target memory + sources
//!       edges/<src>__<rel>__<dst>.json — reflects_on / supersedes /
//!                                       derived_from edges + signatures
//!       signed_events/<event_id>.json  — append-only audit rows for
//!                                       the chain
//!       transcripts/<id>.json          — transcript metadata
//!       transcripts/<id>.content       — raw decompressed UTF-8 body
//! ```
//!
//! ## Determinism + reproducibility
//!
//! Acceptance criterion from #670 is "byte-identical mod timestamp".
//! We enforce that by:
//!
//! - Writing a minimal POSIX ustar archive in-process (no `tar` crate
//!   dep — keeps the dep surface flat per repo convention).
//! - Sorting every file name lexicographically before emission so two
//!   builds over the same DB produce identical bytes regardless of
//!   SQLite row order.
//! - Pinning every per-file ustar header field (uid, gid, mtime, mode,
//!   uname, gname) to a constant — there is no caller-supplied
//!   filesystem metadata in the archive.
//! - Pinning the manifest field order via a struct definition rather
//!   than a `serde_json::Map` (which is `BTreeMap`-backed but the
//!   default `to_string` writer is still order-preserving for the
//!   struct path) and emitting via `serde_json::to_vec_pretty` which is
//!   deterministic for `#[derive(Serialize)]` structs.
//!
//! The only legitimate non-determinism is `manifest.generated_at` —
//! the RFC3339 instant the bundle was assembled. That field is
//! explicitly documented as "expected to vary across rebuilds" and
//! lives in a stable position so a downstream diff tool can ignore it
//! exactly.
//!
//! ## Signature
//!
//! The bundle's `manifest.json` includes a SHA-256 over every file in
//! the archive AND, when an AlphaOne operator keypair is on disk, an
//! Ed25519 signature over a canonical concatenation of those hashes.
//! An auditor verifies the bundle by:
//!
//! 1. Re-hashing every file in the tar.
//! 2. Comparing each hash to `manifest.files[path].sha256`.
//! 3. (If `manifest.signature` is present) re-deriving the same
//!    canonical concat and verifying the Ed25519 signature against the
//!    operator's public key.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ed25519_dalek::Signer;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::CliOutput;
use crate::identity::keypair as kp_mod;
use crate::identity::sign::SignableLink;

// ─────────────────────────────────────────────────────────────────────
// Public arguments (consumed by daemon_runtime dispatch)
// ─────────────────────────────────────────────────────────────────────

/// Arguments for `ai-memory export-forensic-bundle`.
#[derive(clap::Args, Debug)]
pub struct ExportForensicBundleArgs {
    /// Memory id whose reflection chain to bundle.
    #[arg(long, value_name = "ID")]
    pub memory_id: String,

    /// Include the target memory + every reachable source memory.
    #[arg(long, default_value_t = false)]
    pub include_reflections: bool,

    /// Include the transcript union (per L2-4 `replay_transcript_union`).
    #[arg(long, default_value_t = false)]
    pub include_transcripts: bool,

    /// Output path for the tarball. Defaults to
    /// `forensic-bundle-<short-id>-<rfc3339>.tar` in the working
    /// directory.
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// v0.7.0 WT-1-E — when true (default), include the full
    /// atomisation chain whenever the target memory is an archived
    /// source or an atom: the source row, every atom (atom_of =
    /// source_id), every `derives_from` edge, and the
    /// `atomisation_complete` signed event. When false the bundle
    /// emits only the atoms (the source chain is skipped), useful
    /// when an auditor only needs the canonical post-atomisation
    /// surface and not the historical record.
    #[arg(long, default_value_t = true)]
    pub include_atomisation_chain: bool,
}

/// Arguments for `ai-memory verify-forensic-bundle <path>`.
#[derive(clap::Args, Debug)]
pub struct VerifyForensicBundleArgs {
    /// Path to the `.tar` bundle to verify.
    pub bundle_path: PathBuf,
}

// ─────────────────────────────────────────────────────────────────────
// Manifest types
// ─────────────────────────────────────────────────────────────────────

/// One entry in the manifest's per-file index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestFile {
    /// Path inside the tarball (e.g. `memories/abc.json`).
    pub path: String,
    /// File size in bytes.
    pub size: u64,
    /// SHA-256 hex digest over the file contents.
    pub sha256: String,
}

/// Manifest metadata + integrity index for the bundle.
///
/// Serialised to `manifest.json` inside the tar. The `signature` and
/// `signer_agent_id` fields are filled when an AlphaOne operator
/// keypair is available; auditors verify the signature with the
/// operator's public key (out-of-band distribution — same model the
/// rest of the H-track uses).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Bundle schema version. Bumped on a wire-incompatible change.
    pub schema_version: u32,
    /// Target memory id passed on the command line.
    pub memory_id: String,
    /// RFC3339 instant the bundle was assembled.
    ///
    /// Only field that legitimately varies across rebuilds (every
    /// other byte in the bundle is deterministic — the reproducibility
    /// acceptance criterion in #670 is "byte-identical mod timestamp"
    /// and this is the timestamp).
    pub generated_at: String,
    /// `true` when `--include-reflections` was passed.
    pub include_reflections: bool,
    /// `true` when `--include-transcripts` was passed.
    pub include_transcripts: bool,
    /// Sorted-by-path SHA-256 manifest over every other file in the
    /// archive (excludes `manifest.json` itself).
    pub files: Vec<ManifestFile>,
    /// Operator agent_id whose key signed the manifest, or `None` when
    /// the bundle is unsigned (no operator key on disk).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signer_agent_id: Option<String>,
    /// Ed25519 signature (base64) over `canonical_signed_bytes` of the
    /// rest of the manifest, or `None` when unsigned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Bundle schema version pin. Bumped on any change that breaks the
/// auditor's deserialisation contract (new mandatory field, removed
/// field, reshuffled enum, etc.).
pub const BUNDLE_SCHEMA_VERSION: u32 = 1;

// ─────────────────────────────────────────────────────────────────────
// Per-entity envelope types
// ─────────────────────────────────────────────────────────────────────

/// One stored memory inside the bundle. We re-emit a stable subset of
/// the [`crate::models::Memory`] shape so a future struct refactor
/// doesn't silently break the on-disk format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEnvelope {
    pub id: String,
    pub namespace: String,
    pub title: String,
    pub content: String,
    pub tier: String,
    pub memory_kind: String,
    pub reflection_depth: i32,
    pub created_at: String,
    pub updated_at: String,
    pub metadata: serde_json::Value,
    /// v0.7.0 WT-1-E — atomisation-chain enrichment. Present only
    /// when this memory is involved in atomisation (either an
    /// archived source with `atomised_into > 0` or an atom with
    /// `atom_of` set). Provides the full chain locally to the
    /// auditor without forcing them to cross-reference between
    /// envelopes. Skipped (None) on rows untouched by atomisation
    /// and on every bundle built with
    /// `--include-atomisation-chain=false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atomisation: Option<AtomisationEnvelope>,
    /// v0.7.0 Form 4 (issue #757) — fact-provenance citations.
    /// Always emitted (defaults to an empty array) so auditors can
    /// rely on the field's presence regardless of row vintage.
    /// Mirrors [`crate::models::Memory::citations`].
    #[serde(default)]
    pub citations: Vec<crate::models::Citation>,
    /// v0.7.0 Form 4 — first-class URI-form pointer to the cited
    /// source body. Omitted when NULL on the underlying row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// v0.7.0 Form 4 — byte-range into the parent source body.
    /// Omitted when NULL on the underlying row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_span: Option<crate::models::SourceSpan>,
}

/// v0.7.0 WT-1-E — per-memory atomisation enrichment block. Carries
/// the substrate-visible signals (`atomised_into`, `archived_at`,
/// `atom_ids`, `atom_of`) directly so an auditor can reconstruct the
/// chain from a single envelope.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AtomisationEnvelope {
    /// Count of atoms emitted from this source (mirror of
    /// `memories.atomised_into`). `None` on atom rows and on rows
    /// untouched by atomisation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atomised_into: Option<i64>,
    /// RFC3339 stamp from `metadata.atomisation_archived_at`,
    /// populated by the WT-1-B `archive_source` step. `None` on
    /// rows untouched by atomisation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
    /// Ordered list of atom ids whose `atom_of` points back at this
    /// source. Empty on atom rows and on rows untouched by
    /// atomisation.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub atom_ids: Vec<String>,
    /// Parent source id when this memory is an atom. `None` on
    /// archived-source rows and on rows untouched by atomisation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atom_of: Option<String>,
}

/// One signed link inside the bundle. Carries the canonical
/// [`SignableLink`] field set plus the raw signature so an auditor can
/// re-derive the canonical-CBOR bytes and re-verify the Ed25519
/// signature without joining back to a substrate row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeEnvelope {
    pub source_id: String,
    pub target_id: String,
    pub relation: String,
    pub created_at: String,
    pub observed_by: Option<String>,
    pub valid_from: Option<String>,
    pub valid_until: Option<String>,
    pub attest_level: String,
    /// Hex-encoded Ed25519 signature, or `None` for unsigned edges.
    pub signature_hex: Option<String>,
}

/// One `signed_events` audit row inside the bundle. Mirrors the column
/// shape of [`crate::signed_events::SignedEvent`] but emits
/// `payload_hash` and `signature` as hex strings so the on-wire format
/// is JSON-safe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedEventEnvelope {
    pub id: String,
    pub agent_id: String,
    pub event_type: String,
    pub payload_hash_hex: String,
    pub signature_hex: Option<String>,
    pub attest_level: String,
    pub timestamp: String,
}

/// One transcript inside the bundle. We split metadata from content so
/// callers can deserialise the metadata without holding the body in
/// memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEnvelope {
    pub id: String,
    pub namespace: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub compressed_size: i64,
    pub original_size: i64,
    /// Memory ids that linked to this transcript inside the chain.
    pub linked_memory_ids: Vec<String>,
}

// ─────────────────────────────────────────────────────────────────────
// Bundle builder
// ─────────────────────────────────────────────────────────────────────

/// In-memory representation of the bundle before it's emitted as a
/// tar. Path → file bytes. Sorted iteration is guaranteed by the
/// `BTreeMap` so the on-wire archive is deterministic.
type BundleFiles = BTreeMap<String, Vec<u8>>;

/// Build the bundle for the given memory id, writing the tarball to
/// `output_path`.
///
/// `generated_at` overrides the RFC3339 timestamp written into the
/// manifest. The CLI always passes `None` (which fills in
/// `chrono::Utc::now()`); the test suite passes a fixed string to make
/// the byte-identical reproducibility assertion provable.
///
/// # Errors
///
/// Propagates I/O errors writing the tarball, signing errors when an
/// operator key is on disk but corrupted, or substrate read errors.
pub fn build(
    conn: &Connection,
    args: &ExportForensicBundleArgs,
    output_path: &Path,
    generated_at: Option<&str>,
) -> Result<()> {
    let files = build_files(conn, args, generated_at)?;
    write_ustar(output_path, &files).context("write forensic bundle tar")
}

/// In-memory variant of [`build`]. Returns the
/// path-keyed `BundleFiles` map ready to be serialised by either
/// [`write_ustar`] (production) or `pack_to_vec` (tests). Public so
/// the integration test suite can rebuild the same bundle twice and
/// diff the bytes without going through the filesystem.
pub fn build_files(
    conn: &Connection,
    args: &ExportForensicBundleArgs,
    generated_at: Option<&str>,
) -> Result<BundleFiles> {
    let generated_at: String = generated_at
        .map(ToString::to_string)
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

    // 1) Walk the reflects_on graph backward from memory_id to assemble
    //    the set of in-scope memory ids.
    let mut chain_ids = walk_reflection_chain(conn, &args.memory_id)?;

    // v0.7.0 WT-1-E — atomisation-chain expansion. When the target
    // memory (or any ancestor) is an archived source, fold its atoms
    // in. When the target is itself an atom, fold its parent source
    // in. Both directions are needed so an auditor sees the full
    // chain regardless of which "end" of it they queried by id.
    // The expansion is purely additive — reflections + atomisation
    // can coexist on the same memory.
    if args.include_atomisation_chain {
        let mut expanded = chain_ids.clone();
        for mid in &chain_ids {
            // Source → atoms (when this id is an archived source)
            for atom_id in atom_ids_of_source(conn, mid)? {
                if !expanded.contains(&atom_id) {
                    expanded.push(atom_id);
                }
            }
            // Atom → source (when this id is an atom)
            if let Some(parent_id) = atom_of_for(conn, mid)? {
                if !expanded.contains(&parent_id) {
                    expanded.push(parent_id.clone());
                    // Recursively pick up sibling atoms of that
                    // parent so the auditor sees the whole sibling
                    // cohort, not just the one atom that was the
                    // entry point.
                    for atom_id in atom_ids_of_source(conn, &parent_id)? {
                        if !expanded.contains(&atom_id) {
                            expanded.push(atom_id);
                        }
                    }
                }
            }
        }
        expanded.sort();
        chain_ids = expanded;
    }

    let mut files: BundleFiles = BTreeMap::new();

    // 2) Memory envelopes (target + ancestors when --include-reflections).
    //    The atomisation expansion above is preserved verbatim when
    //    --include-reflections=true; when --include-reflections=false
    //    the original reflects_on logic emits only the target row,
    //    but the atomisation enrichment is still attached to it.
    let memory_ids_to_emit: Vec<String> = if args.include_reflections {
        chain_ids.clone()
    } else if args.include_atomisation_chain {
        // Even without --include-reflections, emit the target's
        // atomisation cohort (source + sibling atoms) so the bundle
        // is self-contained for the substrate-visible chain.
        let mut ids = vec![args.memory_id.clone()];
        for atom_id in atom_ids_of_source(conn, &args.memory_id)? {
            if !ids.contains(&atom_id) {
                ids.push(atom_id);
            }
        }
        if let Some(parent) = atom_of_for(conn, &args.memory_id)? {
            if !ids.contains(&parent) {
                ids.push(parent.clone());
            }
            for atom_id in atom_ids_of_source(conn, &parent)? {
                if !ids.contains(&atom_id) {
                    ids.push(atom_id);
                }
            }
        }
        ids.sort();
        ids
    } else {
        vec![args.memory_id.clone()]
    };
    for mid in &memory_ids_to_emit {
        if let Some(mem) = crate::db::get(conn, mid).context("db::get for bundle")? {
            let atomisation = if args.include_atomisation_chain {
                build_atomisation_envelope(conn, &mem)?
            } else {
                None
            };
            let env = MemoryEnvelope {
                id: mem.id.clone(),
                namespace: mem.namespace.clone(),
                title: mem.title.clone(),
                content: mem.content.clone(),
                tier: mem.tier.as_str().to_string(),
                memory_kind: format!("{:?}", mem.memory_kind).to_ascii_lowercase(),
                reflection_depth: mem.reflection_depth,
                created_at: mem.created_at.clone(),
                updated_at: mem.updated_at.clone(),
                metadata: mem.metadata.clone(),
                atomisation,
                // v0.7.0 Form 4 (issue #757) — fact-provenance fields
                // ride alongside the existing envelope shape. Citations
                // always lands (defaults to empty); source_uri /
                // source_span emit only when populated.
                citations: mem.citations.clone(),
                source_uri: mem.source_uri.clone(),
                source_span: mem.source_span,
            };
            let bytes = serde_json::to_vec_pretty(&env).context("serialise MemoryEnvelope")?;
            files.insert(format!("memories/{}.json", mem.id), bytes);
        }
    }

    // 3) Edge envelopes — every reflects_on / supersedes / derived_from
    //    edge whose source is in `chain_ids`. WT-1-E folds in
    //    `derives_from` (atom → parent) alongside the existing
    //    relations — see [`fetch_edges_for`]. When the
    //    `include_atomisation_chain` flag is unset, drop
    //    `derives_from` edges from the output so the auditor
    //    sees only the atom rows (the historical record stays
    //    in the substrate; the bundle just doesn't carry it).
    let edges_raw = fetch_edges_for(conn, &chain_ids)?;
    let edges: Vec<_> = if args.include_atomisation_chain {
        edges_raw
    } else {
        edges_raw
            .into_iter()
            .filter(|e| e.relation != "derives_from")
            .collect()
    };
    for edge in &edges {
        let bytes = serde_json::to_vec_pretty(edge).context("serialise EdgeEnvelope")?;
        // Lexicographic path so determinism survives row-order shuffling.
        // Path components are safe ASCII (uuid + relation name); no
        // sanitisation needed.
        let path = format!(
            "edges/{}__{}__{}.json",
            edge.source_id, edge.relation, edge.target_id
        );
        files.insert(path, bytes);
    }

    // 4) signed_events slice — every audit row whose agent_id matches
    //    a memory in the chain (the H5 convention is to use agent_id =
    //    actor's id; the memory_id is embedded in the payload).
    let mut event_ids_emitted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let events = fetch_signed_events_for(conn, &chain_ids)?;
    for ev in &events {
        let bytes = serde_json::to_vec_pretty(ev).context("serialise SignedEventEnvelope")?;
        files.insert(format!("signed_events/{}.json", ev.id), bytes);
        event_ids_emitted.insert(ev.id.clone());
    }

    // v0.7.0 WT-1-E — atomisation-chain signed_events. Two event
    // shapes need to land in the bundle even when their agent_id is
    // not itself a memory id in the chain:
    //
    //   * `atomisation_complete` — the summary event the WT-1-B
    //     atomiser emits per source. Its `agent_id` is the calling
    //     agent's id (e.g. `ai:claude@host:pid-…`), not a memory
    //     id, so the H5-agent-id-match query above misses it.
    //   * `memory_link.created` for each `derives_from` atom→parent
    //     edge. Again the agent_id is the writer, not the memory.
    //
    // We fetch these explicitly by joining the memory_links table
    // (for the per-atom edge events) and by event_type +
    // payload_hash cross-reference (for the summary event). Both
    // sets are unioned with the existing agent-id-matched events,
    // de-duped, then emitted under the same path scheme.
    if args.include_atomisation_chain {
        let extra = fetch_atomisation_signed_events_for(conn, &chain_ids)?;
        for ev in &extra {
            if event_ids_emitted.contains(&ev.id) {
                continue;
            }
            let bytes = serde_json::to_vec_pretty(ev).context("serialise SignedEventEnvelope")?;
            files.insert(format!("signed_events/{}.json", ev.id), bytes);
            event_ids_emitted.insert(ev.id.clone());
        }
    }

    // 5) Transcript union (per L2-4) when --include-transcripts.
    if args.include_transcripts {
        let entries =
            crate::transcripts::replay::replay_transcript_union(conn, &args.memory_id, None)
                .context("replay_transcript_union for bundle")?;

        // Dedup by transcript id (replay_transcript_union already
        // dedups, but defensive coding here keeps the manifest stable
        // if the upstream contract loosens).
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for entry in &entries {
            if !seen.insert(entry.meta.id.clone()) {
                continue;
            }
            // Gather every memory_id that linked to this transcript
            // (deterministic order via sort).
            let mut linked: Vec<String> = entries
                .iter()
                .filter(|e| e.meta.id == entry.meta.id)
                .map(|e| e.memory_id.clone())
                .collect();
            linked.sort();
            linked.dedup();

            let env = TranscriptEnvelope {
                id: entry.meta.id.clone(),
                namespace: entry.meta.namespace.clone(),
                created_at: entry.meta.created_at.clone(),
                expires_at: entry.meta.expires_at.clone(),
                compressed_size: entry.meta.compressed_size,
                original_size: entry.meta.original_size,
                linked_memory_ids: linked,
            };
            let meta_bytes =
                serde_json::to_vec_pretty(&env).context("serialise TranscriptEnvelope")?;
            files.insert(format!("transcripts/{}.json", entry.meta.id), meta_bytes);

            if let Some(content) = crate::transcripts::storage::fetch(conn, &entry.meta.id)
                .context("fetch transcript content for bundle")?
            {
                files.insert(
                    format!("transcripts/{}.content", entry.meta.id),
                    content.into_bytes(),
                );
            }
        }
    }

    // 6) Embed the L1-3 verify-reflection-chain JSON as verification.json.
    //    Pass `generated_at` through so the embedded report's
    //    timestamp matches the manifest's — keeps the bundle
    //    reproducible per #670's "byte-identical mod timestamp"
    //    acceptance criterion (the manifest's `generated_at` is the
    //    one legitimate non-deterministic field).
    let report =
        crate::cli::verify::build_chain_report_at(conn, &args.memory_id, true, Some(&generated_at))
            .context("build_chain_report for bundle")?;
    let verification_bytes =
        serde_json::to_vec_pretty(&report).context("serialise chain report")?;
    files.insert("verification.json".to_string(), verification_bytes);

    // 7) Build the manifest (every file EXCEPT manifest.json itself
    //    contributes to the SHA-256 index) and sign it.
    let mut manifest = Manifest {
        schema_version: BUNDLE_SCHEMA_VERSION,
        memory_id: args.memory_id.clone(),
        generated_at,
        include_reflections: args.include_reflections,
        include_transcripts: args.include_transcripts,
        files: files
            .iter()
            .map(|(p, body)| ManifestFile {
                path: p.clone(),
                size: body.len() as u64,
                sha256: hex_sha256(body),
            })
            .collect(),
        signer_agent_id: None,
        signature: None,
    };

    // 8) Sign the manifest with the operator's keypair, if one is on
    //    disk. The signature commits to a canonical concatenation of
    //    every file's path + size + sha256 (the rest of the manifest
    //    fields are reconstructible from the tarball at verify time).
    if let Some((agent_id, sig_b64)) = sign_manifest_if_keyed(&manifest)? {
        manifest.signer_agent_id = Some(agent_id);
        manifest.signature = Some(sig_b64);
    }

    let manifest_bytes = serde_json::to_vec_pretty(&manifest).context("serialise Manifest")?;
    files.insert("manifest.json".to_string(), manifest_bytes);

    Ok(files)
}

/// Canonical signing input: `path:size:sha256` per file, joined by
/// `\n`, then the bundle's schema version + memory id appended. The
/// ordering of the `manifest.files` vec is already deterministic (it
/// reflects `BundleFiles`'s BTreeMap iteration order), so the same
/// bundle always produces the same signing input.
pub fn canonical_signed_bytes(m: &Manifest) -> Vec<u8> {
    let mut out = String::new();
    for f in &m.files {
        out.push_str(&f.path);
        out.push(':');
        out.push_str(&f.size.to_string());
        out.push(':');
        out.push_str(&f.sha256);
        out.push('\n');
    }
    out.push_str("schema_version:");
    out.push_str(&m.schema_version.to_string());
    out.push('\n');
    out.push_str("memory_id:");
    out.push_str(&m.memory_id);
    out.push('\n');
    out.into_bytes()
}

/// Look for an operator keypair on disk and, if found, sign the
/// manifest's canonical bytes. Returns `(agent_id, base64_signature)`.
/// Returns `Ok(None)` when no key is available — that path is the
/// "unsigned bundle" mode, which still verifies for integrity (every
/// file's SHA-256 is recomputed and compared) but lacks operator
/// attestation.
fn sign_manifest_if_keyed(manifest: &Manifest) -> Result<Option<(String, String)>> {
    let key_dir = match kp_mod::default_key_dir() {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    if !key_dir.exists() {
        return Ok(None);
    }
    let entries = match kp_mod::list(&key_dir) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    // Find the first keypair with a private signing key on disk
    // (operator-managed; deterministic by `agent_id` sort order).
    let mut candidates: Vec<String> = entries.into_iter().map(|kp| kp.agent_id).collect();
    candidates.sort();
    for agent_id in candidates {
        if let Ok(kp) = kp_mod::load(&agent_id, &key_dir) {
            if let Some(signing) = kp.private.as_ref() {
                let bytes = canonical_signed_bytes(manifest);
                let sig = signing.sign(&bytes);
                let sig_b64 = STANDARD_NO_PAD.encode(sig.to_bytes());
                return Ok(Some((agent_id, sig_b64)));
            }
        }
    }
    Ok(None)
}

// ─────────────────────────────────────────────────────────────────────
// Substrate readers
// ─────────────────────────────────────────────────────────────────────

/// Walk `reflects_on` edges backward from `root` and return the
/// visited memory ids in BFS order. Mirrors the walk in
/// [`crate::cli::verify::build_chain_report`] but returns only the id
/// set (no per-edge verification).
fn walk_reflection_chain(conn: &Connection, root: &str) -> Result<Vec<String>> {
    use std::collections::{HashSet, VecDeque};
    let mut visited: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    queue.push_back(root.to_string());
    while let Some(cur) = queue.pop_front() {
        if !visited.insert(cur.clone()) {
            continue;
        }
        order.push(cur.clone());
        let mut stmt = conn.prepare(
            "SELECT target_id FROM memory_links \
             WHERE source_id = ?1 AND relation = 'reflects_on' \
             ORDER BY target_id",
        )?;
        let rows = stmt.query_map(params![cur], |r| r.get::<_, String>(0))?;
        for r in rows {
            let tgt = r?;
            if !visited.contains(&tgt) {
                queue.push_back(tgt);
            }
        }
    }
    // Stable sort so the on-wire ordering is independent of BFS
    // expansion order (BFS depends on row insertion order which can
    // differ across DBs even when the set is identical).
    order.sort();
    Ok(order)
}

/// Fetch every `reflects_on` / `supersedes` / `derived_from` edge
/// whose `source_id` is in `chain_ids`. Returns rows sorted by
/// (source_id, relation, target_id) so the on-wire ordering is
/// deterministic.
fn fetch_edges_for(conn: &Connection, chain_ids: &[String]) -> Result<Vec<EdgeEnvelope>> {
    let mut out = Vec::new();
    if chain_ids.is_empty() {
        return Ok(out);
    }
    let placeholders: String = chain_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT source_id, target_id, relation, created_at, observed_by, \
                valid_from, valid_until, signature, attest_level \
         FROM memory_links \
         WHERE source_id IN ({placeholders}) \
           AND relation IN ('reflects_on', 'supersedes', 'derived_from', 'derives_from') \
         ORDER BY source_id, relation, target_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = chain_ids
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(param_refs.as_slice(), |r| {
        Ok(EdgeEnvelope {
            source_id: r.get::<_, String>(0)?,
            target_id: r.get::<_, String>(1)?,
            relation: r.get::<_, String>(2)?,
            created_at: r.get::<_, String>(3)?,
            observed_by: r.get::<_, Option<String>>(4)?,
            valid_from: r.get::<_, Option<String>>(5)?,
            valid_until: r.get::<_, Option<String>>(6)?,
            signature_hex: r.get::<_, Option<Vec<u8>>>(7)?.map(|b| bytes_to_hex(&b)),
            attest_level: r
                .get::<_, Option<String>>(8)?
                .unwrap_or_else(|| "unsigned".to_string()),
        })
    })?;
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// v0.7.0 WT-1-E — return the atom ids whose `atom_of` column FK
/// points back to `source_id`. Empty when `source_id` is not an
/// archived source. Ordering matches the WT-1-B emission order
/// (created_at ASC, id ASC).
fn atom_ids_of_source(conn: &Connection, source_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM memories \
         WHERE atom_of = ?1 \
         ORDER BY created_at ASC, id ASC",
    )?;
    let rows = stmt.query_map(params![source_id], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// v0.7.0 WT-1-E — return the parent source id when `id` is an atom
/// (i.e. `memories.atom_of` is set). `None` for non-atom rows or
/// when the id is unknown.
fn atom_of_for(conn: &Connection, id: &str) -> Result<Option<String>> {
    let res: rusqlite::Result<Option<String>> = conn.query_row(
        "SELECT atom_of FROM memories WHERE id = ?1",
        params![id],
        |r| r.get::<_, Option<String>>(0),
    );
    match res {
        Ok(v) => Ok(v),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// v0.7.0 WT-1-E — build the `AtomisationEnvelope` enrichment block
/// for `mem`. Returns `None` when the memory is untouched by
/// atomisation (neither an archived source nor an atom), so the
/// outer envelope's `Option<AtomisationEnvelope>` field round-trips
/// `serde(skip_serializing_if = "Option::is_none")` cleanly.
fn build_atomisation_envelope(
    conn: &Connection,
    mem: &crate::models::Memory,
) -> Result<Option<AtomisationEnvelope>> {
    // Read the two source-side columns. These are not on the Memory
    // struct (yet), so query directly.
    let (atomised_into, atom_of_col): (Option<i64>, Option<String>) = conn
        .query_row(
            "SELECT atomised_into, atom_of FROM memories WHERE id = ?1",
            params![mem.id],
            |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .unwrap_or((None, None));

    let archived_at = mem
        .metadata
        .get("atomisation_archived_at")
        .and_then(|v| v.as_str())
        .map(ToString::to_string);

    let is_archived_source = atomised_into.unwrap_or(0) > 0 || archived_at.is_some();
    let is_atom = atom_of_col.is_some();
    if !is_archived_source && !is_atom {
        return Ok(None);
    }
    let atom_ids = if is_archived_source {
        atom_ids_of_source(conn, &mem.id)?
    } else {
        Vec::new()
    };
    Ok(Some(AtomisationEnvelope {
        atomised_into: atomised_into.filter(|n| *n > 0),
        archived_at,
        atom_ids,
        atom_of: atom_of_col,
    }))
}

/// v0.7.0 WT-1-E — fetch every atomisation-related signed event for
/// the chain. Two queries:
///
///   1. Every `memory_link.created` row whose payload describes a
///      `derives_from` edge from one of the chain's memory ids. We
///      approximate this by joining on `memory_links` (the row that
///      generated the audit event) — the WT-1-B atomiser writes the
///      link via `create_link_signed` which appends a matching
///      audit row at the same instant. Match heuristic: same
///      agent_id and timestamp >= the link's created_at on the
///      same source/target row.
///   2. Every `atomisation_complete` event whose timestamp lies at
///      or after the earliest `derives_from` edge's `created_at`
///      for the chain (i.e. the summary event for any atomisation
///      that involves these memories). Because the payload itself
///      is only stored as a hash we can't filter on `source_id`
///      directly; we instead fetch all events of that type that
///      could plausibly have been emitted by the same calling
///      agent and let the auditor cross-reference at verify time.
///      The over-fetch is bounded by the agent_id set of the
///      chain's `derives_from` writers, so unrelated atomisations
///      from other agents are excluded.
fn fetch_atomisation_signed_events_for(
    conn: &Connection,
    chain_ids: &[String],
) -> Result<Vec<SignedEventEnvelope>> {
    if chain_ids.is_empty() {
        return Ok(Vec::new());
    }
    // Collect the set of `observed_by` agent ids on the chain's
    // derives_from edges. The atomisation_complete event's
    // `agent_id` matches the same `calling_agent_id` used by the
    // per-atom create_link_signed call.
    //
    // Two disjoint placeholder ranges so the source_id and
    // target_id INs each get their own bound slot — using the same
    // placeholder name twice in rusqlite collapses the second bind,
    // which leaves the OR branch unbound and rusqlite errors with
    // "Wrong number of parameters."
    let src_placeholders: String = (1..=chain_ids.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let tgt_placeholders: String = (chain_ids.len() + 1..=chain_ids.len() * 2)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let agent_sql = format!(
        "SELECT DISTINCT observed_by FROM memory_links \
         WHERE relation = 'derives_from' \
           AND (source_id IN ({src_placeholders}) OR target_id IN ({tgt_placeholders})) \
           AND observed_by IS NOT NULL"
    );
    let mut agent_stmt = conn.prepare(&agent_sql)?;
    let bind_pairs: Vec<&dyn rusqlite::ToSql> = chain_ids
        .iter()
        .chain(chain_ids.iter())
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();
    let agent_rows = agent_stmt.query_map(bind_pairs.as_slice(), |r| r.get::<_, String>(0))?;
    let mut writer_agents: Vec<String> = Vec::new();
    for r in agent_rows {
        let id = r?;
        if !writer_agents.contains(&id) {
            writer_agents.push(id);
        }
    }

    // Without a writer agent (i.e. unsigned `derives_from` edge) we
    // still fall back to fetching atomisation_complete events of
    // any agent so the bundle preserves the audit row. Auditors can
    // distinguish via the `attest_level` column.
    let mut out: Vec<SignedEventEnvelope> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    if writer_agents.is_empty() {
        // Fallback: fetch ALL atomisation_complete + memory_link.created
        // events. The chain has unsigned derives_from edges, so we
        // cannot scope by agent — better to over-include in the
        // bundle (auditor sees the relevant subset) than to silently
        // drop the chain's audit trail.
        let sql = "SELECT id, agent_id, event_type, payload_hash, signature, \
                          attest_level, timestamp \
                   FROM signed_events \
                   WHERE event_type IN ('atomisation_complete', 'memory_link.created') \
                   ORDER BY timestamp ASC, id ASC";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], row_to_signed_event_envelope)?;
        for r in rows {
            let ev = r?;
            if seen.insert(ev.id.clone()) {
                out.push(ev);
            }
        }
        return Ok(out);
    }

    let agent_placeholders: String = writer_agents
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT id, agent_id, event_type, payload_hash, signature, \
                attest_level, timestamp \
         FROM signed_events \
         WHERE event_type IN ('atomisation_complete', 'memory_link.created') \
           AND agent_id IN ({agent_placeholders}) \
         ORDER BY timestamp ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = writer_agents
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(param_refs.as_slice(), row_to_signed_event_envelope)?;
    for r in rows {
        let ev = r?;
        if seen.insert(ev.id.clone()) {
            out.push(ev);
        }
    }
    Ok(out)
}

/// v0.7.0 WT-1-E — shared row→envelope decoder. Replicates the
/// inline closure in [`fetch_signed_events_for`] so the WT-1-E
/// fetcher does not duplicate the column-index pattern (and so a
/// future column-set extension only needs to be applied in one
/// place).
fn row_to_signed_event_envelope(r: &rusqlite::Row<'_>) -> rusqlite::Result<SignedEventEnvelope> {
    Ok(SignedEventEnvelope {
        id: r.get::<_, String>(0)?,
        agent_id: r.get::<_, String>(1)?,
        event_type: r.get::<_, String>(2)?,
        payload_hash_hex: bytes_to_hex(&r.get::<_, Vec<u8>>(3)?),
        signature_hex: r.get::<_, Option<Vec<u8>>>(4)?.map(|b| bytes_to_hex(&b)),
        attest_level: r.get::<_, String>(5)?,
        timestamp: r.get::<_, String>(6)?,
    })
}

/// Fetch every `signed_events` row whose `agent_id` matches a memory
/// id in `chain_ids` (the H5 convention puts the actor's agent_id in
/// the `agent_id` column; the memory_id is embedded in the payload —
/// the LIKE is intentional best-effort: signed_events join to the
/// chain via the agent identity of whichever caller minted the link).
fn fetch_signed_events_for(
    conn: &Connection,
    chain_ids: &[String],
) -> Result<Vec<SignedEventEnvelope>> {
    if chain_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders: String = chain_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT id, agent_id, event_type, payload_hash, signature, \
                attest_level, timestamp \
         FROM signed_events \
         WHERE agent_id IN ({placeholders}) \
         ORDER BY timestamp ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = chain_ids
        .iter()
        .map(|s| s as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(param_refs.as_slice(), |r| {
        Ok(SignedEventEnvelope {
            id: r.get::<_, String>(0)?,
            agent_id: r.get::<_, String>(1)?,
            event_type: r.get::<_, String>(2)?,
            payload_hash_hex: bytes_to_hex(&r.get::<_, Vec<u8>>(3)?),
            signature_hex: r.get::<_, Option<Vec<u8>>>(4)?.map(|b| bytes_to_hex(&b)),
            attest_level: r.get::<_, String>(5)?,
            timestamp: r.get::<_, String>(6)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────
// Verification
// ─────────────────────────────────────────────────────────────────────

/// Result of [`verify`]. One row per discrepancy plus an `ok` flag.
#[derive(Debug, Clone, Serialize)]
pub struct VerificationReport {
    pub ok: bool,
    pub bundle_path: String,
    pub manifest_present: bool,
    pub schema_version: u32,
    pub memory_id: String,
    pub signer_agent_id: Option<String>,
    pub signature_status: SignatureStatus,
    /// Files whose recomputed SHA-256 disagreed with the manifest.
    pub tampered_files: Vec<String>,
    /// Files present in the manifest but missing from the tarball.
    pub missing_files: Vec<String>,
    /// Files present in the tarball but absent from the manifest.
    pub extra_files: Vec<String>,
    /// Reflection-chain edges whose embedded signature failed to
    /// re-verify against the bundled `observed_by` public key.
    /// Auditors typically expect this to be empty.
    pub chain_edges_failed: Vec<String>,
}

/// Manifest-signature outcome.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SignatureStatus {
    /// Manifest carried a signature and it verified against the
    /// signer's enrolled public key.
    Verified,
    /// Manifest carried a signature but verification failed.
    Failed,
    /// Manifest carried no signature (unsigned bundle).
    Absent,
    /// Manifest carried a signature but the signer's public key is
    /// not enrolled locally — we can't decide either way.
    UnknownSigner,
}

/// Verify a forensic bundle on disk.
///
/// Re-reads the tarball, recomputes every file's SHA-256, cross-
/// checks the manifest's signature (when present), and re-verifies
/// every edge envelope's Ed25519 signature.
///
/// # Errors
///
/// Propagates I/O errors reading the tarball or parse errors for the
/// embedded manifest. A successful return with `ok = false` means the
/// bundle was structurally valid but failed integrity checks; an
/// `Err` means we couldn't even unpack the archive.
pub fn verify(bundle_path: &Path) -> Result<VerificationReport> {
    let bytes = fs::read(bundle_path)
        .with_context(|| format!("read bundle from {}", bundle_path.display()))?;
    let files = read_ustar(&bytes).context("parse forensic bundle tar")?;

    let manifest_bytes = files
        .get("manifest.json")
        .ok_or_else(|| anyhow!("bundle is missing manifest.json"))?
        .clone();
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("parse manifest.json")?;

    let mut report = VerificationReport {
        ok: true,
        bundle_path: bundle_path.display().to_string(),
        manifest_present: true,
        schema_version: manifest.schema_version,
        memory_id: manifest.memory_id.clone(),
        signer_agent_id: manifest.signer_agent_id.clone(),
        signature_status: SignatureStatus::Absent,
        tampered_files: Vec::new(),
        missing_files: Vec::new(),
        extra_files: Vec::new(),
        chain_edges_failed: Vec::new(),
    };

    // 1) Compare per-file SHA-256s + presence.
    let manifest_index: BTreeMap<&str, &ManifestFile> = manifest
        .files
        .iter()
        .map(|m| (m.path.as_str(), m))
        .collect();
    for (path, body) in &files {
        if path == "manifest.json" {
            continue;
        }
        match manifest_index.get(path.as_str()) {
            Some(mf) => {
                let actual = hex_sha256(body);
                if actual != mf.sha256 || u64::try_from(body.len()).unwrap_or(0) != mf.size {
                    report.tampered_files.push(path.clone());
                }
            }
            None => report.extra_files.push(path.clone()),
        }
    }
    for (path, _) in manifest_index.iter() {
        if !files.contains_key(*path) {
            report.missing_files.push((*path).to_string());
        }
    }

    // 2) Manifest signature.
    if let (Some(signer), Some(sig_b64)) = (
        manifest.signer_agent_id.as_ref(),
        manifest.signature.as_ref(),
    ) {
        let pubkey_opt = crate::identity::verify::lookup_peer_public_key(signer);
        match pubkey_opt {
            Some(pubkey) => {
                let signed_bytes = canonical_signed_bytes(&Manifest {
                    signer_agent_id: None,
                    signature: None,
                    ..manifest.clone()
                });
                let sig_bytes = STANDARD_NO_PAD
                    .decode(sig_b64)
                    .context("decode manifest signature")?;
                let sig_arr: [u8; ed25519_dalek::SIGNATURE_LENGTH] = sig_bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("manifest signature has wrong length"))?;
                let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
                report.signature_status = match pubkey.verify_strict(&signed_bytes, &sig) {
                    Ok(()) => SignatureStatus::Verified,
                    Err(_) => SignatureStatus::Failed,
                };
            }
            None => {
                report.signature_status = SignatureStatus::UnknownSigner;
            }
        }
    }

    // 3) Re-verify every edge envelope's signature.
    for (path, body) in &files {
        if !path.starts_with("edges/") || !path.ends_with(".json") {
            continue;
        }
        let edge: EdgeEnvelope = match serde_json::from_slice(body) {
            Ok(e) => e,
            Err(_) => {
                report.chain_edges_failed.push(path.clone());
                continue;
            }
        };
        if !verify_edge_envelope(&edge) {
            report.chain_edges_failed.push(path.clone());
        }
    }

    // 4) Roll the per-section failures into the top-level ok flag.
    report.ok = report.tampered_files.is_empty()
        && report.missing_files.is_empty()
        && report.chain_edges_failed.is_empty()
        && !matches!(report.signature_status, SignatureStatus::Failed);

    Ok(report)
}

/// Re-derive the canonical CBOR bytes from an [`EdgeEnvelope`] and
/// verify the embedded Ed25519 signature. Returns `true` for unsigned
/// edges (no signature to falsify) and signed edges that verify
/// cleanly. Returns `false` only when a present signature fails to
/// verify against the bundled `observed_by` public key.
fn verify_edge_envelope(edge: &EdgeEnvelope) -> bool {
    let Some(sig_hex) = edge.signature_hex.as_ref() else {
        return true; // unsigned — nothing to verify
    };
    let Some(observed_by) = edge.observed_by.as_ref() else {
        return false; // signed but no agent_id — broken envelope
    };
    let Some(pubkey) = crate::identity::verify::lookup_peer_public_key(observed_by) else {
        // Signer key not enrolled locally — auditor can't decide, but
        // we treat this as a verification failure on the conservative
        // side (the auditor running `verify-forensic-bundle` is
        // expected to have the chain's signers in their key dir).
        return false;
    };
    let Ok(sig_bytes) = hex_to_bytes(sig_hex) else {
        return false;
    };
    let link = SignableLink {
        src_id: &edge.source_id,
        dst_id: &edge.target_id,
        relation: &edge.relation,
        observed_by: Some(observed_by),
        valid_from: edge.valid_from.as_deref(),
        valid_until: edge.valid_until.as_deref(),
    };
    crate::identity::verify::verify(&pubkey, &link, &sig_bytes).is_ok()
}

// ─────────────────────────────────────────────────────────────────────
// CLI entry points (called by daemon_runtime dispatch)
// ─────────────────────────────────────────────────────────────────────

/// Run `ai-memory export-forensic-bundle`.
///
/// # Errors
///
/// Propagates DB / I/O / signing errors.
pub fn run_export(
    db_path: &Path,
    args: &ExportForensicBundleArgs,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    let conn = crate::db::open(db_path).context("open db")?;
    let output = match args.output.as_ref() {
        Some(p) => p.clone(),
        None => {
            let short = args.memory_id.chars().take(8).collect::<String>();
            let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
            PathBuf::from(format!("forensic-bundle-{short}-{ts}.tar"))
        }
    };
    build(&conn, args, &output, None)?;
    writeln!(out.stdout, "forensic bundle written: {}", output.display())?;
    Ok(0)
}

/// Run `ai-memory verify-forensic-bundle`.
///
/// # Errors
///
/// Propagates I/O / parse errors. Verification *failure* (the bundle
/// was parseable but didn't pass integrity checks) returns
/// `Ok(non-zero exit code)` rather than an error.
///
/// v0.7.0 G-PHASE-E-4 (#709) — raised the failure exit code from `1`
/// to `2`. `1` was indistinguishable from CLI argument errors / unwrap
/// panics under shell error trapping; `2` is the conventional
/// "verification failed" code (matches the new convention on
/// `verify-reflection-chain`).
pub fn run_verify(args: &VerifyForensicBundleArgs, out: &mut CliOutput<'_>) -> Result<i32> {
    let report = verify(&args.bundle_path)?;
    let payload = serde_json::to_string_pretty(&report).context("serialise VerificationReport")?;
    writeln!(out.stdout, "{payload}")?;
    if report.ok {
        writeln!(out.stdout, "verification OK")?;
        Ok(0)
    } else {
        writeln!(out.stdout, "verification FAILED")?;
        Ok(2)
    }
}

// ─────────────────────────────────────────────────────────────────────
// Hex helpers
// ─────────────────────────────────────────────────────────────────────

fn bytes_to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hex_to_bytes(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        bail!("hex string has odd length");
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let pair = &s[i..i + 2];
        let byte =
            u8::from_str_radix(pair, 16).with_context(|| format!("invalid hex pair '{pair}'"))?;
        out.push(byte);
    }
    Ok(out)
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    bytes_to_hex(&hasher.finalize())
}

// ─────────────────────────────────────────────────────────────────────
// Minimal deterministic POSIX ustar writer + reader
// ─────────────────────────────────────────────────────────────────────
//
// We could pull in the `tar` crate, but that adds a transitive dep (it
// is not currently in the lockfile). The bundle format we need is a
// tiny subset of ustar — every file is a regular file, every name
// fits in 100 bytes, no symlinks, no hardlinks, no PAX extensions. A
// 80-line writer + reader keeps the dep surface flat per repo
// convention and makes the format trivially auditable.
//
// All header fields are pinned to constants so two builds over the
// same `BundleFiles` produce byte-identical archives:
//
//   - uid / gid: 0
//   - mode: 0o644
//   - mtime: 0 (Unix epoch)
//   - uname / gname: empty
//
// The only operator-visible field is the file name (BTreeMap key);
// the rest of the header derives from the file body.

const USTAR_BLOCK_SIZE: usize = 512;

/// Serialise `files` as a deterministic POSIX ustar archive, writing
/// to `path`. The on-wire bytes are identical for identical inputs
/// regardless of the host's filesystem, locale, or clock.
fn write_ustar(path: &Path, files: &BundleFiles) -> Result<()> {
    let mut out: Vec<u8> = Vec::new();
    for (name, body) in files {
        write_ustar_entry(&mut out, name, body)?;
    }
    // Two zero blocks = end-of-archive marker (POSIX requirement).
    out.extend(std::iter::repeat(0u8).take(USTAR_BLOCK_SIZE * 2));
    fs::write(path, &out).with_context(|| format!("write tarball to {}", path.display()))?;
    Ok(())
}

/// Serialise `files` to an in-memory `Vec<u8>` — used by the
/// reproducibility tests so two builds can be byte-diffed without
/// hitting the disk.
pub fn pack_to_vec(files: &BundleFiles) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    for (name, body) in files {
        write_ustar_entry(&mut out, name, body)?;
    }
    out.extend(std::iter::repeat(0u8).take(USTAR_BLOCK_SIZE * 2));
    Ok(out)
}

fn write_ustar_entry(out: &mut Vec<u8>, name: &str, body: &[u8]) -> Result<()> {
    if name.len() > 100 {
        bail!(
            "bundle path '{name}' exceeds 100-byte ustar name limit; the bundle layout is \
             documented to keep every path under 100 bytes"
        );
    }
    let mut header = [0u8; USTAR_BLOCK_SIZE];

    // name: bytes 0..100
    header[..name.len()].copy_from_slice(name.as_bytes());
    // mode: bytes 100..108 — 7-byte octal + NUL. "0000644"
    write_octal(&mut header[100..108], 0o644, 7);
    // uid: bytes 108..116 — "0000000"
    write_octal(&mut header[108..116], 0, 7);
    // gid: bytes 116..124 — "0000000"
    write_octal(&mut header[116..124], 0, 7);
    // size: bytes 124..136 — 11-byte octal + NUL
    write_octal(&mut header[124..136], body.len() as u64, 11);
    // mtime: bytes 136..148 — pinned to 0 for determinism
    write_octal(&mut header[136..148], 0, 11);
    // checksum: bytes 148..156 — filled with spaces first, then recomputed
    for b in &mut header[148..156] {
        *b = b' ';
    }
    // typeflag: bytes 156..157 — '0' = regular file
    header[156] = b'0';
    // linkname: bytes 157..257 — empty
    // magic: bytes 257..263 — "ustar\0"
    header[257..263].copy_from_slice(b"ustar\0");
    // version: bytes 263..265 — "00"
    header[263..265].copy_from_slice(b"00");
    // uname / gname: 265..297 + 297..329 — empty
    // devmajor / devminor: 329..337 + 337..345 — "0000000\0" each
    write_octal(&mut header[329..337], 0, 7);
    write_octal(&mut header[337..345], 0, 7);
    // prefix: 345..500 — empty (we require name <= 100)

    // Compute the unsigned checksum over the entire header with the
    // checksum field treated as 8 spaces (already done above), then
    // write it back as 6-octal-digit + NUL + space (POSIX-mandated
    // termination).
    let chksum: u32 = header.iter().map(|b| u32::from(*b)).sum();
    let s = format!("{chksum:06o}\0 ");
    header[148..156].copy_from_slice(s.as_bytes());

    out.extend_from_slice(&header);
    out.extend_from_slice(body);
    let pad = (USTAR_BLOCK_SIZE - (body.len() % USTAR_BLOCK_SIZE)) % USTAR_BLOCK_SIZE;
    out.extend(std::iter::repeat(0u8).take(pad));
    Ok(())
}

fn write_octal(field: &mut [u8], value: u64, width: usize) {
    // Octal digits, zero-padded to `width`, followed by NUL.
    let s = format!("{value:0width$o}", width = width);
    for (i, b) in s.bytes().enumerate() {
        field[i] = b;
    }
    field[width] = 0;
}

/// Parse a POSIX ustar archive emitted by [`write_ustar`] back into a
/// path-keyed `BundleFiles` map. We deliberately keep the parser
/// strict — only the field set we ourselves emit is accepted, so a
/// downstream auditor running this code path is auditing the same
/// minimal grammar the build path emits.
pub fn read_ustar(bytes: &[u8]) -> Result<BundleFiles> {
    let mut files: BundleFiles = BTreeMap::new();
    let mut pos = 0;
    while pos + USTAR_BLOCK_SIZE <= bytes.len() {
        let header = &bytes[pos..pos + USTAR_BLOCK_SIZE];
        // End-of-archive: first byte zero (per POSIX, two zero blocks
        // terminate; we accept one and bail).
        if header[0] == 0 {
            break;
        }
        let name = read_cstr(&header[..100]);
        let size = read_octal_size(&header[124..136])?;
        pos += USTAR_BLOCK_SIZE;
        if pos + size > bytes.len() {
            bail!("tar entry '{name}' size {size} extends beyond archive bytes");
        }
        let body = bytes[pos..pos + size].to_vec();
        files.insert(name, body);
        let pad = (USTAR_BLOCK_SIZE - (size % USTAR_BLOCK_SIZE)) % USTAR_BLOCK_SIZE;
        pos += size + pad;
    }
    Ok(files)
}

fn read_cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn read_octal_size(bytes: &[u8]) -> Result<usize> {
    let s = read_cstr(bytes);
    let trimmed = s.trim().trim_matches(|c: char| !c.is_ascii_digit());
    if trimmed.is_empty() {
        return Ok(0);
    }
    usize::from_str_radix(trimmed, 8).with_context(|| format!("invalid octal size field '{s}'"))
}

// ─────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::models::{Memory, MemoryKind, Tier};
    use chrono::Utc;
    use rusqlite::params;
    use tempfile::TempDir;

    fn open_tmp_db(tmp: &TempDir) -> (rusqlite::Connection, PathBuf) {
        let p = tmp.path().join("ai-memory.db");
        let conn = db::open(&p).expect("db::open");
        (conn, p)
    }

    fn insert_mem(conn: &rusqlite::Connection, ns: &str, depth: i32, kind: MemoryKind) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let mem = Memory {
            id: id.clone(),
            tier: Tier::Mid,
            namespace: ns.to_string(),
            title: format!("t-{depth}"),
            content: format!("c-{depth}"),
            reflection_depth: depth,
            created_at: now.clone(),
            updated_at: now,
            memory_kind: kind,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
            ..Default::default()
        };
        db::insert(conn, &mem).expect("insert");
        id
    }

    fn link_unsigned(conn: &rusqlite::Connection, src: &str, tgt: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO memory_links \
             (source_id, target_id, relation, created_at, attest_level) \
             VALUES (?1, ?2, 'reflects_on', ?3, 'unsigned')",
            params![src, tgt, Utc::now().to_rfc3339()],
        )
        .expect("link_unsigned");
    }

    #[test]
    fn write_and_read_ustar_round_trips() {
        let mut files = BTreeMap::new();
        files.insert("a.json".to_string(), b"{\"a\":1}".to_vec());
        files.insert("nested/b.txt".to_string(), b"hello world".to_vec());
        let bytes = pack_to_vec(&files).expect("pack");
        let parsed = read_ustar(&bytes).expect("parse");
        assert_eq!(parsed, files);
    }

    #[test]
    fn ustar_is_byte_deterministic() {
        let mut files = BTreeMap::new();
        files.insert("z.txt".to_string(), b"last".to_vec());
        files.insert("a.txt".to_string(), b"first".to_vec());
        let a = pack_to_vec(&files).expect("pack a");
        let b = pack_to_vec(&files).expect("pack b");
        assert_eq!(a, b, "same input must produce byte-identical output");
    }

    #[test]
    fn build_files_emits_manifest_with_pinned_schema_version() {
        let tmp = TempDir::new().unwrap();
        let (conn, _) = open_tmp_db(&tmp);
        let id = insert_mem(&conn, "fb-ns", 0, MemoryKind::Observation);
        let args = ExportForensicBundleArgs {
            memory_id: id.clone(),
            include_reflections: true,
            include_transcripts: false,
            include_atomisation_chain: true,
            output: None,
        };
        let files = build_files(&conn, &args, Some("2026-01-01T00:00:00Z")).expect("build");
        let manifest_bytes = files.get("manifest.json").expect("manifest present");
        let manifest: Manifest = serde_json::from_slice(manifest_bytes).expect("parse manifest");
        assert_eq!(manifest.schema_version, BUNDLE_SCHEMA_VERSION);
        assert_eq!(manifest.memory_id, id);
        assert_eq!(manifest.generated_at, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn build_files_reproducible_modulo_timestamp() {
        let tmp = TempDir::new().unwrap();
        let (conn, _) = open_tmp_db(&tmp);
        let d0 = insert_mem(&conn, "ns", 0, MemoryKind::Observation);
        let d1 = insert_mem(&conn, "ns", 1, MemoryKind::Reflection);
        link_unsigned(&conn, &d1, &d0);
        let args = ExportForensicBundleArgs {
            memory_id: d1.clone(),
            include_reflections: true,
            include_transcripts: false,
            include_atomisation_chain: true,
            output: None,
        };
        let files_a = build_files(&conn, &args, Some("2026-01-01T00:00:00Z")).expect("build a");
        let files_b = build_files(&conn, &args, Some("2026-01-01T00:00:00Z")).expect("build b");
        let bytes_a = pack_to_vec(&files_a).expect("pack a");
        let bytes_b = pack_to_vec(&files_b).expect("pack b");
        assert_eq!(
            bytes_a, bytes_b,
            "byte-identical mod timestamp is the L2-5 acceptance criterion"
        );
    }

    #[test]
    fn verify_clean_bundle_reports_ok() {
        let tmp = TempDir::new().unwrap();
        let (conn, _) = open_tmp_db(&tmp);
        let d0 = insert_mem(&conn, "ns", 0, MemoryKind::Observation);
        let d1 = insert_mem(&conn, "ns", 1, MemoryKind::Reflection);
        link_unsigned(&conn, &d1, &d0);
        let args = ExportForensicBundleArgs {
            memory_id: d1.clone(),
            include_reflections: true,
            include_transcripts: false,
            include_atomisation_chain: true,
            output: None,
        };
        let bundle_path = tmp.path().join("bundle.tar");
        build(&conn, &args, &bundle_path, Some("2026-01-01T00:00:00Z")).expect("build");
        let report = verify(&bundle_path).expect("verify");
        assert!(report.ok, "clean bundle must verify: {report:#?}");
        assert!(report.tampered_files.is_empty());
        assert!(report.missing_files.is_empty());
    }

    #[test]
    fn verify_detects_tampered_file_in_bundle() {
        let tmp = TempDir::new().unwrap();
        let (conn, _) = open_tmp_db(&tmp);
        let d0 = insert_mem(&conn, "ns", 0, MemoryKind::Observation);
        let d1 = insert_mem(&conn, "ns", 1, MemoryKind::Reflection);
        link_unsigned(&conn, &d1, &d0);
        let args = ExportForensicBundleArgs {
            memory_id: d1.clone(),
            include_reflections: true,
            include_transcripts: false,
            include_atomisation_chain: true,
            output: None,
        };
        let bundle_path = tmp.path().join("bundle.tar");
        build(&conn, &args, &bundle_path, Some("2026-01-01T00:00:00Z")).expect("build");

        // Tamper: rewrite the file body without updating the manifest.
        // The verifier should flag the affected entry.
        let bytes = fs::read(&bundle_path).expect("read");
        let mut files = read_ustar(&bytes).expect("parse");
        let target_key = files
            .keys()
            .find(|k| k.starts_with("memories/"))
            .expect("at least one memory entry")
            .clone();
        files.insert(target_key.clone(), b"tampered".to_vec());
        let new_bytes = pack_to_vec(&files).expect("repack");
        fs::write(&bundle_path, &new_bytes).expect("write");

        let report = verify(&bundle_path).expect("verify");
        assert!(!report.ok, "tampered bundle must fail verification");
        assert!(
            report.tampered_files.contains(&target_key),
            "verifier must name the tampered file; got {:?}",
            report.tampered_files
        );
    }

    #[test]
    fn canonical_signed_bytes_is_stable() {
        let m = Manifest {
            schema_version: 1,
            memory_id: "abc".into(),
            generated_at: "2026-01-01T00:00:00Z".into(),
            include_reflections: true,
            include_transcripts: false,
            files: vec![
                ManifestFile {
                    path: "a.json".into(),
                    size: 5,
                    sha256: "ff".into(),
                },
                ManifestFile {
                    path: "b.json".into(),
                    size: 10,
                    sha256: "ee".into(),
                },
            ],
            signer_agent_id: None,
            signature: None,
        };
        let a = canonical_signed_bytes(&m);
        let b = canonical_signed_bytes(&m);
        assert_eq!(a, b);
        let s = String::from_utf8(a).unwrap();
        assert!(s.contains("a.json:5:ff"));
        assert!(s.contains("b.json:10:ee"));
        assert!(s.contains("memory_id:abc"));
    }

    #[test]
    fn build_chain_includes_ancestors_when_reflections_requested() {
        let tmp = TempDir::new().unwrap();
        let (conn, _) = open_tmp_db(&tmp);
        let d0 = insert_mem(&conn, "ns", 0, MemoryKind::Observation);
        let d1 = insert_mem(&conn, "ns", 1, MemoryKind::Reflection);
        let d2 = insert_mem(&conn, "ns", 2, MemoryKind::Reflection);
        link_unsigned(&conn, &d2, &d1);
        link_unsigned(&conn, &d1, &d0);
        let args = ExportForensicBundleArgs {
            memory_id: d2.clone(),
            include_reflections: true,
            include_transcripts: false,
            include_atomisation_chain: true,
            output: None,
        };
        let files = build_files(&conn, &args, Some("2026-01-01T00:00:00Z")).expect("build");
        for id in [&d0, &d1, &d2] {
            let key = format!("memories/{id}.json");
            assert!(
                files.contains_key(&key),
                "depth-2 chain must include all ancestors; missing {key}"
            );
        }
        // Two reflects_on edges in the chain → two edge files.
        let edge_count = files.keys().filter(|k| k.starts_with("edges/")).count();
        assert_eq!(edge_count, 2, "expected 2 reflects_on edges");
    }

    #[test]
    fn build_chain_excludes_ancestors_without_reflections_flag() {
        let tmp = TempDir::new().unwrap();
        let (conn, _) = open_tmp_db(&tmp);
        let d0 = insert_mem(&conn, "ns", 0, MemoryKind::Observation);
        let d1 = insert_mem(&conn, "ns", 1, MemoryKind::Reflection);
        link_unsigned(&conn, &d1, &d0);
        let args = ExportForensicBundleArgs {
            memory_id: d1.clone(),
            include_reflections: false,
            include_transcripts: false,
            include_atomisation_chain: true,
            output: None,
        };
        let files = build_files(&conn, &args, Some("2026-01-01T00:00:00Z")).expect("build");
        assert!(files.contains_key(&format!("memories/{d1}.json")));
        assert!(
            !files.contains_key(&format!("memories/{d0}.json")),
            "ancestor must be excluded when --include-reflections is unset"
        );
    }

    #[test]
    fn verify_detects_missing_file_from_bundle() {
        let tmp = TempDir::new().unwrap();
        let (conn, _) = open_tmp_db(&tmp);
        let d0 = insert_mem(&conn, "ns", 0, MemoryKind::Observation);
        let d1 = insert_mem(&conn, "ns", 1, MemoryKind::Reflection);
        link_unsigned(&conn, &d1, &d0);
        let args = ExportForensicBundleArgs {
            memory_id: d1.clone(),
            include_reflections: true,
            include_transcripts: false,
            include_atomisation_chain: true,
            output: None,
        };
        let bundle_path = tmp.path().join("bundle.tar");
        build(&conn, &args, &bundle_path, Some("2026-01-01T00:00:00Z")).expect("build");

        let bytes = fs::read(&bundle_path).expect("read");
        let mut files = read_ustar(&bytes).expect("parse");
        let memory_key = files
            .keys()
            .find(|k| k.starts_with("memories/") && k.contains(&d0))
            .expect("ancestor entry present")
            .clone();
        files.remove(&memory_key);
        let new_bytes = pack_to_vec(&files).expect("repack");
        fs::write(&bundle_path, &new_bytes).expect("write");

        let report = verify(&bundle_path).expect("verify");
        assert!(!report.ok, "missing file must fail verification");
        assert!(report.missing_files.contains(&memory_key));
    }

    #[test]
    fn hex_round_trip() {
        let bytes = vec![0u8, 0x0f, 0xa1, 0xff];
        let hex = bytes_to_hex(&bytes);
        assert_eq!(hex, "000fa1ff");
        assert_eq!(hex_to_bytes(&hex).unwrap(), bytes);
    }

    #[test]
    fn hex_to_bytes_rejects_odd_length() {
        assert!(hex_to_bytes("abc").is_err());
    }

    #[test]
    fn ustar_rejects_long_paths() {
        let mut files = BTreeMap::new();
        // 101-char name — must error.
        files.insert("a".repeat(101), b"x".to_vec());
        assert!(pack_to_vec(&files).is_err());
    }

    #[test]
    fn hex_to_bytes_rejects_invalid_pair() {
        let err = hex_to_bytes("zz").unwrap_err();
        assert!(format!("{err:#}").contains("invalid hex pair"));
    }

    #[test]
    fn hex_sha256_stable_for_same_input() {
        let a = hex_sha256(b"hello world");
        let b = hex_sha256(b"hello world");
        assert_eq!(a, b);
        // Known fixed property: 64 hex chars
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn read_octal_size_parses_padded_field() {
        let mut field = [0u8; 12];
        write_octal(&mut field, 256, 11);
        let parsed = read_octal_size(&field).unwrap();
        assert_eq!(parsed, 256);
    }

    #[test]
    fn read_octal_size_empty_returns_zero() {
        let field = [0u8; 12];
        // All zeros after octal write of 0 produces "00000000000\0".
        let parsed = read_octal_size(&field).unwrap();
        assert_eq!(parsed, 0);
    }

    #[test]
    fn read_octal_size_garbage_returns_error_or_zero() {
        // Field starts with non-digit garbage — trim removes it, empty -> 0.
        let field = b"  \0\0\0\0\0\0\0\0\0\0";
        let parsed = read_octal_size(field).unwrap();
        assert_eq!(parsed, 0);
    }

    #[test]
    fn ustar_pack_unpack_empty_files_map() {
        let files: BundleFiles = BTreeMap::new();
        let bytes = pack_to_vec(&files).unwrap();
        let parsed = read_ustar(&bytes).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn ustar_pack_unpack_handles_block_aligned_body() {
        let mut files = BundleFiles::new();
        // Exactly 512 bytes — no padding inside record.
        files.insert("aligned.bin".to_string(), vec![b'A'; 512]);
        let bytes = pack_to_vec(&files).unwrap();
        let parsed = read_ustar(&bytes).unwrap();
        assert_eq!(parsed.get("aligned.bin").unwrap().len(), 512);
    }

    #[test]
    fn read_ustar_stops_on_zero_block() {
        // Empty zero block at the start -> empty map.
        let bytes = vec![0u8; 1024];
        let parsed = read_ustar(&bytes).unwrap();
        assert!(parsed.is_empty());
    }

    #[test]
    fn canonical_signed_bytes_excludes_signature_fields() {
        // canonical_signed_bytes must not include `signer_agent_id` or
        // `signature` so re-signing produces the same canonical input.
        let mut m1 = Manifest {
            schema_version: 1,
            memory_id: "abc".into(),
            generated_at: "2026-01-01T00:00:00Z".into(),
            include_reflections: true,
            include_transcripts: false,
            files: vec![ManifestFile {
                path: "a.json".into(),
                size: 5,
                sha256: "ff".into(),
            }],
            signer_agent_id: None,
            signature: None,
        };
        let bytes_unsigned = canonical_signed_bytes(&m1);
        m1.signer_agent_id = Some("alice".into());
        m1.signature = Some("0xdead".into());
        let bytes_signed = canonical_signed_bytes(&m1);
        assert_eq!(
            bytes_unsigned, bytes_signed,
            "signer fields must not affect canonical signed bytes"
        );
    }

    #[test]
    fn bytes_to_hex_empty_returns_empty_string() {
        assert_eq!(bytes_to_hex(&[]), "");
    }

    #[test]
    fn hex_to_bytes_empty_returns_empty_vec() {
        let v = hex_to_bytes("").unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn write_octal_zero_value_is_padded() {
        let mut field = [0u8; 8];
        write_octal(&mut field, 0, 7);
        assert_eq!(&field[..7], b"0000000");
        assert_eq!(field[7], 0);
    }

    #[test]
    fn read_ustar_truncated_body_rejected() {
        // Build a single-file archive and truncate it mid-body.
        let mut files = BundleFiles::new();
        files.insert("x.txt".to_string(), b"hello".to_vec());
        let bytes = pack_to_vec(&files).unwrap();
        // Truncate at 520 bytes (just past header, body is incomplete).
        let truncated = &bytes[..516];
        let err = read_ustar(truncated).unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("extends beyond"));
    }

    #[test]
    fn verify_returns_error_for_missing_bundle_path() {
        let p = std::path::Path::new("/this/does/not/exist/bundle.tar");
        assert!(verify(p).is_err());
    }
}
