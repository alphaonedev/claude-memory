// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Pluggable inference backend trait — issue #651 (RFC pulled forward
//! from v0.8 per operator directive `28860423-d12c-4959-bc8b-8fa9a94a33d9`,
//! 2026-05-18).
//!
//! ## Goal
//!
//! Provide a single trait surface that unifies the substrate's two
//! inference paths today (`embeddings::Embedder` for vector embedding,
//! `llm::OllamaClient` for chat / auto-tag / detect-contradiction)
//! AND provides a forward-compatible hook for the v0.8 GPU / MTP
//! distilled hot-path backend (issues #651 / #654 / Gap #10 of #846).
//!
//! ## Surface
//!
//! ```ignore
//! pub trait InferenceBackend: Send + Sync {
//!     fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;
//!     fn chat(&self, prompt: &str) -> anyhow::Result<String>;
//!     fn attested_weights(&self) -> Option<AttestedWeights>;
//! }
//! ```
//!
//! ## Backends shipped at v0.7.0
//!
//! - [`CpuBackend`] — wraps the existing CPU pipeline
//!   (`embeddings::Embedder` + `llm::OllamaClient`). This is what
//!   v0.7.0 actually uses on the recall hot-path.
//! - [`GpuBackend`] — stub returning `not implemented`. Lands as a
//!   trait-conformant placeholder so the v0.8 work (issue #651 Phase 1
//!   — mistralrs or candle in-process GPU backend) can drop in without
//!   any caller-side refactor.
//!
//! ## Attested weights (issue #654)
//!
//! `attested_weights()` returns the loaded model's SHA-256 + an
//! optional Ed25519 signature over the weight bytes. The CPU backend
//! implements MVP supply-chain attestation by hashing the on-disk
//! model file at load time; the GPU backend stub returns `None`.
//! Documentation for the full v0.8 attested weight chain lives at
//! `docs/v0.7.0/inference-attestation.md`.
//!
//! ## Regression test
//!
//! `cpu_backend_round_trips_embed` (in this module) and
//! `gpu_backend_returns_not_implemented` pin the contract.

use anyhow::{Result, anyhow};
use std::sync::Arc;

/// Attested model-weight provenance returned by
/// [`InferenceBackend::attested_weights`]. MVP supply-chain attestation
/// per issue #654 — SHA-256 of the on-disk weight file, plus an
/// optional Ed25519 signature attested by the operator key.
///
/// v0.8 will extend this with a full Sigstore-style chain (cosign
/// bundle, transparency log entry, key-rotation reference). Today the
/// MVP shape is enough to refuse to serve from a tampered weight file
/// at load time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestedWeights {
    /// Hex-encoded SHA-256 of the model weight bytes.
    pub sha256: String,
    /// Optional base64-encoded Ed25519 signature over `sha256`.
    /// `None` for backends that have not been signed yet.
    pub signature: Option<String>,
    /// Operator-readable label identifying the model
    /// (e.g. `"all-MiniLM-L6-v2"` or `"distilled-hot-path-v0.8"`).
    pub label: String,
}

/// The unified inference surface. v0.8 callers will hold an
/// `Arc<dyn InferenceBackend>` instead of separate embedder + llm
/// handles. At v0.7.0 the recall hot-path still uses the legacy
/// types directly (no callsite churn during the v0.7.0 ship window);
/// the trait is the seam through which the v0.8 GPU/MTP backend will
/// be threaded.
pub trait InferenceBackend: Send + Sync {
    /// Produce a single embedding vector for `text`.
    ///
    /// # Errors
    ///
    /// Implementor-specific (model load failure, tokenisation error,
    /// device OOM, etc.). The GPU stub backend returns a
    /// `not implemented` error.
    fn embed(&self, text: &str) -> Result<Vec<f32>>;

    /// Generate a chat completion for `prompt`. Default system prompt
    /// is `None` (implementor decides); use a concrete backend's API
    /// for system-prompt support.
    ///
    /// # Errors
    ///
    /// Implementor-specific (transport error, model unavailable,
    /// safety refusal, etc.).
    fn chat(&self, prompt: &str) -> Result<String>;

    /// Return the loaded model's SHA-256 + optional signature for
    /// issue #654 supply-chain attestation. `None` if the backend
    /// has no on-disk weights to attest (e.g. a network-only client).
    fn attested_weights(&self) -> Option<AttestedWeights> {
        None
    }
}

/// CPU backend — wraps the existing v0.7.0 inference path
/// (`embeddings::Embedder` + `llm::OllamaClient`). This is a thin
/// adapter; the underlying types are unchanged.
pub struct CpuBackend {
    embedder: Arc<dyn crate::embeddings::Embed>,
    llm: Option<Arc<crate::llm::OllamaClient>>,
    /// Optional pre-computed attested-weights record. Construct via
    /// [`CpuBackend::with_attested_weights`] when the operator has
    /// pinned the model file's SHA-256.
    attested: Option<AttestedWeights>,
}

impl CpuBackend {
    /// Construct a CPU backend from existing handles.
    #[must_use]
    pub fn new(
        embedder: Arc<dyn crate::embeddings::Embed>,
        llm: Option<Arc<crate::llm::OllamaClient>>,
    ) -> Self {
        Self {
            embedder,
            llm,
            attested: None,
        }
    }

    /// Pin an attested-weights record (issue #654). Returns a new
    /// backend wrapping the same handles. The hash is NOT recomputed
    /// here — the caller pre-computes it via
    /// [`compute_attested_weights`] at model-load time so the
    /// `verify_attested_weights` gate can refuse to serve from a
    /// tampered file.
    #[must_use]
    pub fn with_attested_weights(mut self, attested: AttestedWeights) -> Self {
        self.attested = Some(attested);
        self
    }
}

impl InferenceBackend for CpuBackend {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.embedder.embed(text)
    }

    fn chat(&self, prompt: &str) -> Result<String> {
        let llm = self
            .llm
            .as_ref()
            .ok_or_else(|| anyhow!("CpuBackend: chat unavailable (no OllamaClient configured)"))?;
        llm.generate(prompt, None)
    }

    fn attested_weights(&self) -> Option<AttestedWeights> {
        self.attested.clone()
    }
}

/// GPU backend stub — issue #651 Phase 1 placeholder. Returns
/// `not implemented` from every call. Lands as a trait-conformant
/// type so the v0.8 GPU/MTP backend (mistralrs or candle in-process)
/// can drop in without a single caller-side refactor.
#[derive(Default)]
pub struct GpuBackend {
    /// Operator-readable label (e.g. `"distilled-hot-path-v0.8"`).
    /// Stored even on the stub so attestation plumbing can be
    /// exercised end-to-end during the v0.8 work.
    pub label: String,
}

impl GpuBackend {
    /// Construct a GPU backend stub with the given operator-readable
    /// label.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

impl InferenceBackend for GpuBackend {
    fn embed(&self, _text: &str) -> Result<Vec<f32>> {
        Err(anyhow!(
            "GpuBackend::embed not implemented (v0.8 work — issue #651 Phase 1; \
             see docs/v0.7.0/inference-attestation.md for the rollout plan)"
        ))
    }

    fn chat(&self, _prompt: &str) -> Result<String> {
        Err(anyhow!(
            "GpuBackend::chat not implemented (v0.8 work — issue #651 Phase 1)"
        ))
    }
}

/// Compute the SHA-256 of a model-weight file on disk and assemble an
/// [`AttestedWeights`] record. Issue #654 MVP supply-chain attestation.
///
/// # Errors
///
/// Returns an error if the file cannot be read.
pub fn compute_attested_weights(
    path: &std::path::Path,
    label: impl Into<String>,
    signature: Option<String>,
) -> Result<AttestedWeights> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)
        .map_err(|e| anyhow!("compute_attested_weights: read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    Ok(AttestedWeights {
        sha256: hex::encode(digest),
        signature,
        label: label.into(),
    })
}

/// Verify an in-flight [`AttestedWeights`] record against the file at
/// `path`. Issue #654 MVP gate — call before binding the backend if
/// the operator has pinned a known-good hash.
///
/// # Errors
///
/// Returns an error if the file cannot be read or the recomputed hash
/// does not match `expected.sha256`.
pub fn verify_attested_weights(path: &std::path::Path, expected: &AttestedWeights) -> Result<()> {
    let recomputed = compute_attested_weights(path, &expected.label, None)?;
    if recomputed.sha256 != expected.sha256 {
        return Err(anyhow!(
            "verify_attested_weights: hash mismatch for {} (expected {}, got {}) — \
             refusing to serve from a tampered weight file (issue #654)",
            path.display(),
            expected.sha256,
            recomputed.sha256,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct MockEmbedder;
    impl crate::embeddings::Embed for MockEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>> {
            Ok(vec![text.len() as f32; 4])
        }
    }

    #[test]
    fn cpu_backend_round_trips_embed() {
        let be: Arc<dyn InferenceBackend> = Arc::new(CpuBackend::new(Arc::new(MockEmbedder), None));
        let v = be.embed("hello").expect("embed ok");
        assert_eq!(v, vec![5.0_f32; 4]);
    }

    #[test]
    fn cpu_backend_chat_without_llm_errors() {
        let be = CpuBackend::new(Arc::new(MockEmbedder), None);
        let err = be.chat("anything").expect_err("must err");
        assert!(err.to_string().contains("chat unavailable"));
    }

    #[test]
    fn gpu_backend_returns_not_implemented() {
        let be: Arc<dyn InferenceBackend> = Arc::new(GpuBackend::new("test-gpu"));
        let err = be.embed("x").expect_err("gpu embed must err");
        assert!(err.to_string().contains("not implemented"));
        let err = be.chat("x").expect_err("gpu chat must err");
        assert!(err.to_string().contains("not implemented"));
        assert!(be.attested_weights().is_none());
    }

    #[test]
    fn compute_and_verify_attested_weights_round_trip() {
        // Write a tiny fixture file to .local-runs/ so we honor the
        // no-/tmp HARD RULE in CLAUDE.md.
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".local-runs");
        std::fs::create_dir_all(&dir).expect("mkdir .local-runs");
        let path = dir.join(format!(
            "inference-attest-fixture-{}.bin",
            uuid::Uuid::new_v4()
        ));
        let mut f = std::fs::File::create(&path).expect("create fixture");
        f.write_all(b"a tiny attested model weight blob")
            .expect("write fixture");
        f.sync_all().expect("sync fixture");
        drop(f);

        let attested =
            compute_attested_weights(&path, "fixture", None).expect("compute_attested_weights ok");
        assert_eq!(attested.sha256.len(), 64, "sha256 hex must be 64 chars");

        verify_attested_weights(&path, &attested).expect("verify ok");

        // Tamper the file; verify must now refuse.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open append");
        f.write_all(b"--tampered--").expect("tamper write");
        f.sync_all().expect("sync tamper");
        drop(f);
        let err = verify_attested_weights(&path, &attested)
            .expect_err("verify must refuse tampered file");
        assert!(err.to_string().contains("hash mismatch"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cpu_backend_with_attested_weights_round_trip() {
        let attested = AttestedWeights {
            sha256: "0".repeat(64),
            signature: None,
            label: "test".into(),
        };
        let be =
            CpuBackend::new(Arc::new(MockEmbedder), None).with_attested_weights(attested.clone());
        assert_eq!(be.attested_weights(), Some(attested));
    }
}
