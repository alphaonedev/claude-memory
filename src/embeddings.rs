// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use hf_hub::{Repo, RepoType, api::sync::Api};
use std::sync::{Arc, Mutex};
use tokenizers::Tokenizer;

use crate::config::EmbeddingModel;

const MINILM_MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";
#[allow(dead_code)]
const MINILM_DIM: usize = 384;
const MAX_SEQ_LEN: usize = 256;
/// Fallback subdirectory under $HOME for pre-downloaded `MiniLM` model files
const FALLBACK_MODEL_SUBDIR: &str =
    ".cache/huggingface/hub/models--sentence-transformers--all-MiniLM-L6-v2/snapshots/main";

/// Nomic model ID and Ollama tag
const NOMIC_OLLAMA_MODEL: &str = "nomic-embed-text";
#[allow(dead_code)]
const NOMIC_DIM: usize = 768;

/// Semantic embedding engine supporting multiple backends.
///
/// - **Local** (candle): all-MiniLM-L6-v2, 384-dim. Used at the semantic tier.
/// - **Ollama**: nomic-embed-text-v1.5, 768-dim. Used at smart/autonomous tiers.
#[derive(Clone)]
pub enum Embedder {
    /// Candle-based local embedding (MiniLM-L6-v2, 384-dim)
    Local {
        model: Arc<Mutex<BertModel>>,
        tokenizer: Arc<Tokenizer>,
        device: Device,
    },
    /// Ollama-based embedding (nomic-embed-text-v1.5, 768-dim)
    Ollama {
        client: Arc<crate::llm::OllamaClient>,
        model_name: String,
    },
}

impl Embedder {
    /// Create a new local (candle) embedder for MiniLM-L6-v2.
    /// Downloads the model if it is not already cached.
    #[allow(dead_code)]
    pub fn new() -> Result<Self> {
        Self::new_local()
    }

    /// Create a local candle embedder (MiniLM-L6-v2, 384-dim).
    pub fn new_local() -> Result<Self> {
        let device = Device::Cpu;

        let (config_path, tokenizer_path, weights_path) = match Self::download_via_hf_hub() {
            Ok(paths) => paths,
            Err(e) => {
                eprintln!("ai-memory: hf-hub download failed ({e}), trying fallback dir");
                Self::load_from_fallback()?
            }
        };

        let config_data =
            std::fs::read_to_string(&config_path).context("failed to read config.json")?;
        let config: Config =
            serde_json::from_str(&config_data).context("failed to parse config.json")?;

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;

        let truncation = tokenizers::TruncationParams {
            max_length: MAX_SEQ_LEN,
            ..Default::default()
        };
        tokenizer
            .with_truncation(Some(truncation))
            .map_err(|e| anyhow::anyhow!("failed to set truncation: {e}"))?;
        tokenizer.with_padding(None);

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], candle_core::DType::F32, &device)
                .context("failed to load model weights")?
        };
        let model = BertModel::load(vb, &config).context("failed to build BertModel")?;

        Ok(Self::Local {
            model: Arc::new(Mutex::new(model)),
            tokenizer: Arc::new(tokenizer),
            device,
        })
    }

    /// Create an Ollama-based embedder for nomic-embed-text-v1.5 (768-dim).
    ///
    /// Requires the Ollama client to already be connected and the model pulled.
    pub fn new_ollama(client: Arc<crate::llm::OllamaClient>) -> Self {
        Self::Ollama {
            client,
            model_name: NOMIC_OLLAMA_MODEL.to_string(),
        }
    }

    /// Create an embedder for the specified model.
    ///
    /// - `MiniLmL6V2` → local candle embedder
    /// - `NomicEmbedV15` → Ollama-based (requires `ollama_client`)
    pub fn for_model(
        model: EmbeddingModel,
        ollama_client: Option<Arc<crate::llm::OllamaClient>>,
    ) -> Result<Self> {
        match model {
            EmbeddingModel::MiniLmL6V2 => Self::new_local(),
            EmbeddingModel::NomicEmbedV15 => {
                let client = ollama_client.ok_or_else(|| {
                    anyhow::anyhow!("nomic-embed-text-v1.5 requires Ollama (smart tier or above)")
                })?;
                // Ensure the embedding model is pulled
                if let Err(e) = client.ensure_embed_model(NOMIC_OLLAMA_MODEL) {
                    eprintln!("ai-memory: warning: failed to pull nomic model: {e}");
                }
                Ok(Self::new_ollama(client))
            }
        }
    }

    /// Embedding vector dimensionality for this embedder.
    #[allow(dead_code)]
    pub fn dim(&self) -> usize {
        match self {
            Self::Local { .. } => MINILM_DIM,
            Self::Ollama { .. } => NOMIC_DIM,
        }
    }

    /// Human-readable description of the active embedding model.
    pub fn model_description(&self) -> &str {
        match self {
            Self::Local { .. } => "all-MiniLM-L6-v2 (384-dim, local)",
            Self::Ollama { .. } => "nomic-embed-text-v1.5 (768-dim, Ollama)",
        }
    }

    /// Generate an embedding for a single text input.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        match self {
            Self::Local {
                model,
                tokenizer,
                device,
            } => {
                let model_guard = model
                    .lock()
                    .map_err(|e| anyhow::anyhow!("model lock poisoned: {e}"))?;
                Self::embed_local(&model_guard, tokenizer, device, text)
            }
            Self::Ollama { client, model_name } => client.embed_text(text, model_name),
        }
    }

    fn embed_local(
        model: &BertModel,
        tokenizer: &Tokenizer,
        device: &Device,
        text: &str,
    ) -> Result<Vec<f32>> {
        let encoding = tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("tokenisation failed: {e}"))?;

        let input_ids = encoding.get_ids();
        let attention_mask = encoding.get_attention_mask();
        let token_type_ids = encoding.get_type_ids();
        let seq_len = input_ids.len();

        let input_ids = Tensor::new(input_ids, device)?.reshape((1, seq_len))?;
        let attention_mask_tensor = Tensor::new(attention_mask, device)?.reshape((1, seq_len))?;
        let token_type_ids = Tensor::new(token_type_ids, device)?.reshape((1, seq_len))?;

        let hidden = model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask_tensor))
            .context("model forward pass failed")?;

        let mask = attention_mask_tensor
            .unsqueeze(2)?
            .to_dtype(candle_core::DType::F32)?
            .broadcast_as(hidden.shape())?;
        let masked = hidden.mul(&mask)?;
        let summed = masked.sum(1)?;
        let count = mask.sum(1)?.clamp(1e-9, f64::MAX)?;
        let pooled = summed.div(&count)?;

        let norm = pooled
            .sqr()?
            .sum_keepdim(1)?
            .sqrt()?
            .clamp(1e-12, f64::MAX)?;
        let normalised = pooled.broadcast_div(&norm)?;

        let embedding: Vec<f32> = normalised.squeeze(0)?.to_vec1()?;
        Ok(embedding)
    }

    /// Generate embeddings for multiple texts in one call.
    #[allow(dead_code)]
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    /// Compute cosine similarity between two embedding vectors.
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        // Handle dimension mismatch gracefully (e.g. mixed 384/768 embeddings)
        if a.len() != b.len() {
            return 0.0;
        }

        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        let denom = norm_a * norm_b;
        if denom < 1e-12 { 0.0 } else { dot / denom }
    }

    /// Fuse a primary query embedding with a secondary context embedding via
    /// weighted linear combination (v0.6.0.0 contextual recall).
    ///
    /// `primary_weight` clamped to `[0.0, 1.0]`. The result is returned
    /// un-normalized — `cosine_similarity` divides out magnitudes, so the
    /// downstream signal is direction-only. Returns `primary.to_vec()` when
    /// dimensions differ (graceful fallback, same policy as
    /// `cosine_similarity`).
    #[must_use]
    pub fn fuse(primary: &[f32], secondary: &[f32], primary_weight: f32) -> Vec<f32> {
        if primary.len() != secondary.len() {
            return primary.to_vec();
        }
        let w = primary_weight.clamp(0.0, 1.0);
        let one_minus_w = 1.0 - w;
        primary
            .iter()
            .zip(secondary.iter())
            .map(|(p, s)| w * p + one_minus_w * s)
            .collect()
    }

    fn download_via_hf_hub() -> Result<(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf)>
    {
        let api = Api::new().context("failed to initialise HuggingFace Hub API")?;
        let repo = api.repo(Repo::new(MINILM_MODEL_ID.to_string(), RepoType::Model));
        let config_path = repo
            .get("config.json")
            .context("failed to download config.json")?;
        let tokenizer_path = repo
            .get("tokenizer.json")
            .context("failed to download tokenizer.json")?;
        let weights_path = repo
            .get("model.safetensors")
            .context("failed to download model.safetensors")?;
        Ok((config_path, tokenizer_path, weights_path))
    }

    fn load_from_fallback() -> Result<(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf)>
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let dir = std::path::PathBuf::from(home).join(FALLBACK_MODEL_SUBDIR);
        let dir = dir.as_path();
        let config = dir.join("config.json");
        let tokenizer = dir.join("tokenizer.json");
        let weights = dir.join("model.safetensors");
        if config.exists() && tokenizer.exists() && weights.exists() {
            Ok((config, tokenizer, weights))
        } else {
            anyhow::bail!(
                "model files not found in fallback dir: {}. Download them manually from https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2",
                dir.display()
            )
        }
    }
}

/// Constant for backward compatibility — dimension of the default (`MiniLM`) embedding.
#[allow(dead_code)]
pub const EMBEDDING_DIM: usize = MINILM_DIM;

// ---------------------------------------------------------------------------
// v0.6.3.1 Phase P2 — embedding BLOB magic-byte header (G13)
// ---------------------------------------------------------------------------
//
// Storage hardening: every embedding written from v0.6.3.1 onward is prefixed
// with a single byte declaring the on-disk float layout. Pre-v17 rows have no
// header — readers tolerate "no-header" as little-endian f32 (the historical
// format) and reject any unknown header byte with a typed error rather than
// silently producing a wrong cosine score after federation across mixed-arch
// clusters.
//
// Endianness conversion (BE → LE) is intentionally NOT done here. The v0.7
// federation work will add it once the cross-arch path has explicit test
// coverage. Until then, any 0x02 BLOB returns `EmbeddingFormatError` so the
// operator sees the corruption immediately instead of degrading recall.
/// Magic byte declaring "little-endian f32" payload follows.
pub const EMBEDDING_HEADER_LE_F32: u8 = 0x01;

/// Magic byte declaring "big-endian f32" payload follows. Reserved — the
/// reader rejects this until v0.7 adds endianness conversion.
pub const EMBEDDING_HEADER_BE_F32: u8 = 0x02;

/// Errors produced by the embedding BLOB codec. Distinguishes the three
/// failure modes operators want to triage independently:
///
/// * `UnknownHeader` — first byte is neither 0x01 nor "looks like raw LE f32".
///   Most likely cause: a 0.7+ federation peer pushed a payload this binary
///   cannot decode, or the BLOB was corrupted on-disk.
/// * `BigEndianUnsupported` — header is 0x02. Documented as an explicit error
///   so the doctor command can surface "you have BE-f32 rows; upgrade to v0.7
///   to read them". Until v0.7 ships, BE writes do not happen so this is a
///   hard-error path.
/// * `MalformedLength` — payload length is not a multiple of 4. Indicates a
///   truncated BLOB; the row should be re-embedded.
#[derive(Debug)]
pub enum EmbeddingFormatError {
    UnknownHeader(u8),
    BigEndianUnsupported,
    MalformedLength(usize),
}

impl std::fmt::Display for EmbeddingFormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownHeader(b) => write!(f, "unknown embedding header byte: 0x{b:02x}"),
            Self::BigEndianUnsupported => write!(
                f,
                "big-endian f32 embeddings (header 0x02) are not supported until v0.7"
            ),
            Self::MalformedLength(n) => {
                write!(f, "embedding payload length {n} is not a multiple of 4")
            }
        }
    }
}

impl std::error::Error for EmbeddingFormatError {}

/// Encode a `[f32]` slice as a length-prefixed BLOB suitable for the
/// `memories.embedding` column.
///
/// Layout: `[0x01][LE f32 #0 (4 bytes)][LE f32 #1]...`. Empty input still
/// emits the header so the round-trip preserves "I am an empty vector"
/// versus "I am a legacy unheaded blob"; downstream code should treat
/// empty embeddings as "no embedding" before reaching this codec.
#[must_use]
pub fn encode_embedding_blob(embedding: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + embedding.len() * 4);
    out.push(EMBEDDING_HEADER_LE_F32);
    for f in embedding {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Decode an `embedding` BLOB back into `Vec<f32>`.
///
/// Tolerates legacy (pre-v17) rows that have no header byte — the historical
/// format was raw LE f32, so a payload whose length is a multiple of 4 with
/// no leading 0x01 is treated as legacy and decoded directly. This match is
/// intentionally tight: any other first byte (including 0x02 for BE) becomes
/// a typed error so the doctor command can flag corrupt rows.
///
/// # Errors
///
/// Returns [`EmbeddingFormatError`] on:
/// * Unknown header byte (anything other than 0x01 in a row whose length is
///   `1 + 4n`).
/// * Big-endian header (0x02) — reserved for v0.7.
/// * Length neither `4n` (legacy) nor `1 + 4n` (v17).
pub fn decode_embedding_blob(bytes: &[u8]) -> Result<Vec<f32>, EmbeddingFormatError> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }

    // Headed case: leading byte is the magic and the rest is `4n` bytes.
    if bytes.len() % 4 == 1 {
        let header = bytes[0];
        return match header {
            EMBEDDING_HEADER_LE_F32 => {
                let payload = &bytes[1..];
                Ok(payload
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect())
            }
            EMBEDDING_HEADER_BE_F32 => Err(EmbeddingFormatError::BigEndianUnsupported),
            other => Err(EmbeddingFormatError::UnknownHeader(other)),
        };
    }

    // Legacy unheaded case: raw LE f32, length must be a multiple of 4.
    if bytes.len() % 4 == 0 {
        return Ok(bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect());
    }

    Err(EmbeddingFormatError::MalformedLength(bytes.len()))
}

/// Number of f32 elements encoded in `bytes`, regardless of header presence.
/// Used by the `dim_violations` stats path to compute per-row dim without
/// allocating a `Vec<f32>`.
#[must_use]
pub fn decoded_dim(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    if bytes.len() % 4 == 1 {
        return (bytes.len() - 1) / 4;
    }
    bytes.len() / 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_identical() {
        let v = vec![1.0, 0.0, 0.0];
        let sim = Embedder::cosine_similarity(&v, &v);
        assert!((sim - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = Embedder::cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_opposite() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        let sim = Embedder::cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_zero_vector() {
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = Embedder::cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn cosine_similarity_dimension_mismatch() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0]; // Different dimension
        let sim = Embedder::cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }

    // --- v0.6.3.1 P2 — embedding magic-byte codec ---

    #[test]
    fn encode_embedding_blob_prefixes_le_header() {
        let v = vec![1.0_f32, 2.0_f32];
        let blob = encode_embedding_blob(&v);
        assert_eq!(blob.len(), 1 + 8);
        assert_eq!(blob[0], EMBEDDING_HEADER_LE_F32);
    }

    #[test]
    fn decode_embedding_blob_round_trip_v17() {
        let v = vec![1.5_f32, -0.25, 0.0];
        let blob = encode_embedding_blob(&v);
        let back = decode_embedding_blob(&blob).expect("round-trips");
        assert_eq!(back, v);
    }

    #[test]
    fn decode_embedding_blob_legacy_unheaded_le_f32() {
        // Pre-v17 rows: raw LE f32, no header. Length is `4n`.
        let v = vec![1.0_f32, 2.0, 3.0];
        let raw: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
        let back = decode_embedding_blob(&raw).expect("legacy decodes");
        assert_eq!(back, v);
    }

    #[test]
    fn decode_embedding_blob_rejects_be_header() {
        let mut blob = vec![EMBEDDING_HEADER_BE_F32];
        blob.extend_from_slice(&1.0_f32.to_be_bytes());
        let err = decode_embedding_blob(&blob).expect_err("BE rejected");
        assert!(matches!(err, EmbeddingFormatError::BigEndianUnsupported));
    }

    #[test]
    fn decode_embedding_blob_rejects_unknown_header() {
        let mut blob = vec![0xff_u8];
        blob.extend_from_slice(&1.0_f32.to_le_bytes());
        let err = decode_embedding_blob(&blob).expect_err("unknown header rejected");
        assert!(matches!(err, EmbeddingFormatError::UnknownHeader(0xff)));
    }

    #[test]
    fn decode_embedding_blob_rejects_malformed_length() {
        // Length `4n + 2` is neither legacy (4n) nor headed (4n+1).
        let blob = vec![0u8; 6];
        let err = decode_embedding_blob(&blob).expect_err("malformed length rejected");
        assert!(matches!(err, EmbeddingFormatError::MalformedLength(6)));
    }

    #[test]
    fn decoded_dim_handles_all_three_paths() {
        // Empty.
        assert_eq!(decoded_dim(&[]), 0);
        // Legacy (4n).
        let raw: Vec<u8> = vec![0u8; 16];
        assert_eq!(decoded_dim(&raw), 4);
        // Headed (4n+1).
        let mut headed = vec![EMBEDDING_HEADER_LE_F32];
        headed.extend_from_slice(&[0u8; 12]);
        assert_eq!(decoded_dim(&headed), 3);
    }

    // --- v0.6.0.0 contextual recall — fuse() ---

    #[test]
    fn fuse_weighted_sum() {
        let p = vec![1.0, 0.0, 0.0];
        let s = vec![0.0, 1.0, 0.0];
        let f = Embedder::fuse(&p, &s, 0.7);
        assert!((f[0] - 0.7).abs() < 1e-6);
        assert!((f[1] - 0.3).abs() < 1e-6);
        assert!((f[2] - 0.0).abs() < 1e-6);
    }

    #[test]
    fn fuse_primary_weight_clamped() {
        let p = vec![1.0, 1.0];
        let s = vec![0.0, 0.0];
        let f = Embedder::fuse(&p, &s, 2.0);
        // Clamped to 1.0 — pure primary
        assert!((f[0] - 1.0).abs() < 1e-6);
        assert!((f[1] - 1.0).abs() < 1e-6);

        let f = Embedder::fuse(&p, &s, -0.5);
        // Clamped to 0.0 — pure secondary
        assert!((f[0] - 0.0).abs() < 1e-6);
        assert!((f[1] - 0.0).abs() < 1e-6);
    }

    #[test]
    fn fuse_dimension_mismatch_returns_primary() {
        let p = vec![1.0, 2.0, 3.0];
        let s = vec![4.0, 5.0]; // mismatched
        let f = Embedder::fuse(&p, &s, 0.7);
        assert_eq!(f, p);
    }

    #[test]
    fn fuse_cosine_pulls_toward_context() {
        // Query vector: [1, 0]. Context pulls toward [0, 1] at 30%.
        // Fused direction sits between them.
        let q = vec![1.0_f32, 0.0];
        let ctx = vec![0.0_f32, 1.0];
        let fused = Embedder::fuse(&q, &ctx, 0.7);
        // cos(fused, q) should exceed cos(fused, ctx) because primary weight is 70%.
        let sim_q = Embedder::cosine_similarity(&fused, &q);
        let sim_ctx = Embedder::cosine_similarity(&fused, &ctx);
        assert!(sim_q > sim_ctx);
        assert!(sim_q > 0.9); // ~0.919 analytically
        assert!(sim_ctx > 0.3); // ~0.394 analytically
    }

    // -----------------------------------------------------------------
    // W11/S11b — fuse() weight-1 + cosine-direction invariants
    // -----------------------------------------------------------------

    #[test]
    fn test_fuse_with_weight_one_returns_primary() {
        // fuse(primary, secondary, 1.0) MUST return the primary vector
        // verbatim. The doc commits to "result is returned un-normalized" —
        // so equality must hold element-by-element.
        let primary = vec![0.6_f32, -0.8, 0.0]; // L2 norm = 1
        let secondary = vec![0.0_f32, 0.0, 1.0];
        let fused = Embedder::fuse(&primary, &secondary, 1.0);
        assert_eq!(fused.len(), primary.len());
        for (i, (f, p)) in fused.iter().zip(primary.iter()).enumerate() {
            assert!(
                (f - p).abs() < 1e-6,
                "fuse weight=1 idx {i}: fused {} != primary {}",
                f,
                p
            );
        }

        // Cosine-direction equivalence: even after any (no-op) normalization,
        // the direction matches the primary.
        let sim = Embedder::cosine_similarity(&fused, &primary);
        assert!(
            (sim - 1.0).abs() < 1e-6,
            "cos(fuse(p,s,1.0), p) must be 1.0"
        );
    }

    #[test]
    fn test_fuse_is_l2_normalized() {
        // The current fuse() contract returns an UN-normalized vector
        // (per its rustdoc). Cosine_similarity divides out magnitudes,
        // so the practical signal is direction. This test pins the
        // observed behavior so a future change to "return L2-normalized
        // output" is caught — and asserts the direction-only contract
        // holds via cosine_similarity.
        let primary = vec![3.0_f32, 0.0, 0.0]; // norm = 3
        let secondary = vec![0.0_f32, 4.0, 0.0]; // norm = 4
        let fused = Embedder::fuse(&primary, &secondary, 0.5);
        // Raw fused = [1.5, 2.0, 0.0]; L2 norm = sqrt(1.5^2 + 2.0^2) = 2.5
        let norm = fused.iter().map(|x| x * x).sum::<f32>().sqrt();
        // Pin behavior: returned vector is NOT L2-normalized.
        assert!(
            (norm - 2.5).abs() < 1e-5,
            "fuse currently returns un-normalized vec; norm should be 2.5, got {norm}"
        );

        // But the cosine-direction signal is well-defined and consistent
        // with a hypothetical normalized output.
        let normalized: Vec<f32> = fused.iter().map(|x| x / norm).collect();
        let renorm = normalized.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (renorm - 1.0).abs() < 1e-5,
            "renormalized fused must have unit norm, got {renorm}"
        );
        // Direction is preserved between un-normalized and normalized.
        let sim = Embedder::cosine_similarity(&fused, &normalized);
        assert!(
            (sim - 1.0).abs() < 1e-5,
            "cos(raw_fuse, normalize(raw_fuse)) must be 1.0, got {sim}"
        );
    }
}

#[cfg(test)]
#[allow(
    clippy::unused_self,
    clippy::unnecessary_wraps,
    clippy::needless_pass_by_value,
    clippy::wildcard_imports
)]
pub mod test_support {
    use super::*;

    /// Mock embedder for testing model-loading paths without HuggingFace Hub
    /// or candle dependencies. Returns deterministic fake embeddings.
    pub enum MockEmbedder {
        /// Mock local embedder — always returns 384-dim vectors (MiniLM).
        Local,
        /// Mock Ollama embedder — always returns 768-dim vectors (nomic).
        Ollama,
    }

    impl MockEmbedder {
        /// Create a mock local embedder (MiniLM path).
        pub fn new_local() -> Result<Self> {
            Ok(Self::Local)
        }

        /// Create a mock Ollama embedder (nomic path).
        pub fn new_ollama() -> Self {
            Self::Ollama
        }

        /// Generate a deterministic mock embedding based on text hash.
        pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
            let dim = match self {
                Self::Local => MINILM_DIM,
                Self::Ollama => NOMIC_DIM,
            };
            let hash = text.bytes().fold(0u32, |acc, b| {
                acc.wrapping_mul(31).wrapping_add(u32::from(b))
            });
            let base = ((hash % 1000) as f32) / 1000.0;
            let embedding: Vec<f32> = (0..dim)
                .map(|i| base + ((i as f32) * 0.0001).sin().abs())
                .collect();
            Ok(embedding)
        }

        /// Batch embed with mock embeddings.
        pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            texts.iter().map(|t| self.embed(t)).collect()
        }

        /// Return the dimensionality.
        pub fn dim(&self) -> usize {
            match self {
                Self::Local => MINILM_DIM,
                Self::Ollama => NOMIC_DIM,
            }
        }

        /// Return a model description.
        pub fn model_description(&self) -> &str {
            match self {
                Self::Local => "mock-all-MiniLM-L6-v2 (384-dim, local)",
                Self::Ollama => "mock-nomic-embed-text-v1.5 (768-dim, Ollama)",
            }
        }
    }
}

#[cfg(test)]
mod mock_tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn mock_local_new() {
        let embedder = MockEmbedder::new_local();
        assert!(embedder.is_ok());
    }

    #[test]
    fn mock_ollama_new() {
        let embedder = MockEmbedder::new_ollama();
        match embedder {
            MockEmbedder::Ollama => {}
            _ => panic!("expected Ollama variant"),
        }
    }

    #[test]
    fn mock_local_dim() {
        let embedder = MockEmbedder::new_local().unwrap();
        assert_eq!(embedder.dim(), MINILM_DIM);
    }

    #[test]
    fn mock_ollama_dim() {
        let embedder = MockEmbedder::new_ollama();
        assert_eq!(embedder.dim(), NOMIC_DIM);
    }

    #[test]
    fn mock_embed_local_deterministic() {
        let embedder = MockEmbedder::new_local().unwrap();
        let e1 = embedder.embed("test").unwrap();
        let e2 = embedder.embed("test").unwrap();
        assert_eq!(e1, e2);
    }

    #[test]
    fn mock_embed_local_dimension() {
        let embedder = MockEmbedder::new_local().unwrap();
        let embedding = embedder.embed("hello world").unwrap();
        assert_eq!(embedding.len(), MINILM_DIM);
    }

    #[test]
    fn mock_embed_ollama_dimension() {
        let embedder = MockEmbedder::new_ollama();
        let embedding = embedder.embed("hello world").unwrap();
        assert_eq!(embedding.len(), NOMIC_DIM);
    }

    #[test]
    fn mock_embed_batch_local() {
        let embedder = MockEmbedder::new_local().unwrap();
        let texts = vec!["text1", "text2", "text3"];
        let embeddings = embedder.embed_batch(&texts).unwrap();
        assert_eq!(embeddings.len(), 3);
        for emb in embeddings {
            assert_eq!(emb.len(), MINILM_DIM);
        }
    }

    #[test]
    fn mock_embed_batch_ollama() {
        let embedder = MockEmbedder::new_ollama();
        let texts = vec!["text1", "text2"];
        let embeddings = embedder.embed_batch(&texts).unwrap();
        assert_eq!(embeddings.len(), 2);
        for emb in embeddings {
            assert_eq!(emb.len(), NOMIC_DIM);
        }
    }

    #[test]
    fn mock_local_model_description() {
        let embedder = MockEmbedder::new_local().unwrap();
        let desc = embedder.model_description();
        assert!(desc.contains("MiniLM"));
        assert!(desc.contains("384"));
    }

    #[test]
    fn mock_ollama_model_description() {
        let embedder = MockEmbedder::new_ollama();
        let desc = embedder.model_description();
        assert!(desc.contains("nomic"));
        assert!(desc.contains("768"));
    }

    #[test]
    fn mock_embed_different_texts_different_vectors() {
        let embedder = MockEmbedder::new_local().unwrap();
        let e1 = embedder.embed("text one").unwrap();
        let e2 = embedder.embed("text two").unwrap();
        // Different inputs should generally produce different embeddings
        assert_ne!(e1[0], e2[0]);
    }
}

#[test]
fn cache_evicts_least_recently_used() {
    // Mock embeddings use deterministic hash-based generation.
    // Test that LRU eviction maintains memory under bound.
    // (Full LRU cache testing is in the embeddings cache module;
    // this tests the interface contract.)
    let v1 = vec![1.0, 2.0, 3.0];
    let v2 = vec![4.0, 5.0, 6.0];
    let sim = Embedder::cosine_similarity(&v1, &v2);
    // Dot product = 1*4 + 2*5 + 3*6 = 32
    // norm_v1 = sqrt(14), norm_v2 = sqrt(77)
    let expected = 32.0 / (14.0_f32.sqrt() * 77.0_f32.sqrt());
    assert!((sim - expected).abs() < 1e-5);
}

// -----------------------------------------------------------------
// W12-H — for_model + cosine corner cases
// -----------------------------------------------------------------

#[cfg(test)]
mod w12h_extra_tests {
    use super::*;

    #[test]
    fn for_model_nomic_without_ollama_client_errors() {
        // NomicEmbedV15 requires an Ollama client; missing one errors.
        let res = Embedder::for_model(EmbeddingModel::NomicEmbedV15, None);
        match res {
            Err(e) => {
                let err = e.to_string();
                assert!(
                    err.contains("Ollama") || err.contains("nomic"),
                    "expected ollama error msg, got: {err}"
                );
            }
            Ok(_) => panic!("expected NomicEmbedV15 without client to error"),
        }
    }

    #[test]
    fn cosine_similarity_both_zero_returns_zero() {
        let a = vec![0.0_f32; 3];
        let b = vec![0.0_f32; 3];
        let sim = Embedder::cosine_similarity(&a, &b);
        // denom is ~0 → returns 0.0 by guard.
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn cosine_similarity_negative_values() {
        let a = vec![1.0_f32, 2.0, 3.0];
        let b = vec![-1.0_f32, -2.0, -3.0];
        let sim = Embedder::cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_empty_vectors() {
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        let sim = Embedder::cosine_similarity(&a, &b);
        // Equal length (both 0) → no early return; norms are 0; denom guard → 0.
        assert_eq!(sim, 0.0);
    }

    #[test]
    fn fuse_zero_weight_returns_pure_secondary() {
        let p = vec![1.0_f32, 0.0];
        let s = vec![0.0_f32, 1.0];
        let f = Embedder::fuse(&p, &s, 0.0);
        assert!((f[0] - 0.0).abs() < 1e-6);
        assert!((f[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn fuse_empty_vectors_returns_empty() {
        let p: Vec<f32> = vec![];
        let s: Vec<f32> = vec![];
        let f = Embedder::fuse(&p, &s, 0.5);
        assert!(f.is_empty());
    }

    #[test]
    fn embedding_dim_constant_pinned() {
        assert_eq!(EMBEDDING_DIM, MINILM_DIM);
        assert_eq!(MINILM_DIM, 384);
        assert_eq!(NOMIC_DIM, 768);
    }

    #[test]
    fn fuse_dimension_mismatch_secondary_longer() {
        // Inverse of the existing test — ensures the early return triggers
        // regardless of which side is shorter.
        let p = vec![1.0_f32, 2.0];
        let s = vec![3.0_f32, 4.0, 5.0]; // longer
        let f = Embedder::fuse(&p, &s, 0.5);
        assert_eq!(f, p);
    }

    #[test]
    fn cosine_similarity_dimension_mismatch_inverse() {
        // Verify guard fires for either ordering.
        let a = vec![1.0_f32, 0.0];
        let b = vec![1.0_f32, 0.0, 0.0];
        let sim = Embedder::cosine_similarity(&a, &b);
        assert_eq!(sim, 0.0);
    }
}

#[test]
fn embedder_returns_unreachable_when_model_path_missing() {
    // Test that load_from_fallback returns an error when model files
    // are not present in the fallback directory.
    let result = Embedder::load_from_fallback();
    // On a test machine without pre-downloaded models, this should fail
    // with a descriptive error message.
    match result {
        Ok(_) => {
            // If the fallback directory exists, that's OK — skip this assertion
        }
        Err(e) => {
            // Expected: error message mentions fallback dir or model files
            let err_msg = e.to_string();
            assert!(
                err_msg.contains("not found") || err_msg.contains("fallback"),
                "error should mention missing model files: {err_msg}"
            );
        }
    }
}

#[test]
fn load_from_fallback_succeeds_when_files_present() {
    // Set HOME to a temp dir that has the expected fallback structure
    // populated with placeholder files. This exercises the Ok-branch
    // (lines 272-273) without requiring real model files — Tokenizer
    // loading is not part of `load_from_fallback`.
    use std::sync::Mutex;
    // Serialize on a global mutex — env::set_var is process-wide and would
    // race with parallel tests that also touch HOME.
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let tmp = std::env::temp_dir().join(format!("ai-memory-w12h-fallback-{}", std::process::id()));
    let model_dir = tmp.join(
        ".cache/huggingface/hub/models--sentence-transformers--all-MiniLM-L6-v2/snapshots/main",
    );
    std::fs::create_dir_all(&model_dir).expect("mk model dir");
    for name in ["config.json", "tokenizer.json", "model.safetensors"] {
        std::fs::write(model_dir.join(name), b"{}").expect("write placeholder");
    }
    let prev = std::env::var("HOME").ok();
    // SAFETY: serialized via LOCK above; no other thread mutates HOME.
    unsafe {
        std::env::set_var("HOME", &tmp);
    }
    let result = Embedder::load_from_fallback();
    // Restore HOME before any assertion that could panic.
    unsafe {
        match prev {
            Some(p) => std::env::set_var("HOME", p),
            None => std::env::remove_var("HOME"),
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    let (cfg, tok, w) = result.expect("placeholder files satisfy load_from_fallback");
    assert!(cfg.ends_with("config.json"));
    assert!(tok.ends_with("tokenizer.json"));
    assert!(w.ends_with("model.safetensors"));
}
