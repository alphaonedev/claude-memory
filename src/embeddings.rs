// Copyright (c) 2026 AlphaOne LLC. All rights reserved.

use anyhow::{Context, Result};
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use hf_hub::{api::sync::Api, Repo, RepoType};
use std::sync::Arc;
use tokenizers::Tokenizer;

const MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";
pub const EMBEDDING_DIM: usize = 384;
const MAX_SEQ_LEN: usize = 256;
/// Fallback subdirectory under $HOME for pre-downloaded model files
const FALLBACK_MODEL_SUBDIR: &str =
    ".cache/huggingface/hub/models--sentence-transformers--all-MiniLM-L6-v2/snapshots/main";

/// Semantic embedding engine backed by all-MiniLM-L6-v2.
///
/// The model is downloaded on first use from HuggingFace Hub and cached
/// in `~/.cache/huggingface/`. The struct is `Send + Sync` and can be
/// shared across threads via `Arc`.
#[derive(Clone)]
pub struct Embedder {
    model: Arc<BertModel>,
    tokenizer: Arc<Tokenizer>,
    device: Device,
}

// BertModel does not implement Send/Sync by default but the CPU-backed
// tensors are safe to share across threads.
unsafe impl Send for Embedder {}
unsafe impl Sync for Embedder {}

impl Embedder {
    /// Create a new embedder, downloading the model if it is not already cached.
    /// Falls back to a pre-downloaded directory if hf-hub fails.
    pub fn new() -> Result<Self> {
        let device = Device::Cpu;

        // Try hf-hub first, fall back to pre-downloaded files
        let (config_path, tokenizer_path, weights_path) = match Self::download_via_hf_hub() {
            Ok(paths) => paths,
            Err(e) => {
                eprintln!("ai-memory: hf-hub download failed ({}), trying fallback dir", e);
                Self::load_from_fallback()?
            }
        };

        // Load config.
        let config_data =
            std::fs::read_to_string(&config_path).context("failed to read config.json")?;
        let config: Config =
            serde_json::from_str(&config_data).context("failed to parse config.json")?;

        // Load tokenizer.
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;

        // Enforce max sequence length via truncation.
        let truncation = tokenizers::TruncationParams {
            max_length: MAX_SEQ_LEN,
            ..Default::default()
        };
        tokenizer
            .with_truncation(Some(truncation))
            .map_err(|e| anyhow::anyhow!("failed to set truncation: {e}"))?;
        tokenizer.with_padding(None);

        // Load model weights from safetensors.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], candle_core::DType::F32, &device)
                .context("failed to load model weights")?
        };
        let model = BertModel::load(vb, &config).context("failed to build BertModel")?;

        Ok(Self {
            model: Arc::new(model),
            tokenizer: Arc::new(tokenizer),
            device,
        })
    }

    fn download_via_hf_hub() -> Result<(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf)> {
        let api = Api::new().context("failed to initialise HuggingFace Hub API")?;
        let repo = api.repo(Repo::new(MODEL_ID.to_string(), RepoType::Model));
        let config_path = repo.get("config.json").context("failed to download config.json")?;
        let tokenizer_path = repo.get("tokenizer.json").context("failed to download tokenizer.json")?;
        let weights_path = repo.get("model.safetensors").context("failed to download model.safetensors")?;
        Ok((config_path, tokenizer_path, weights_path))
    }

    fn load_from_fallback() -> Result<(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf)> {
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

    /// Generate a 384-dimensional embedding for a single text input.
    ///
    /// The text is tokenised, passed through the model, mean-pooled over
    /// non-padding tokens, and L2-normalised.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("tokenisation failed: {e}"))?;

        let input_ids = encoding.get_ids();
        let attention_mask = encoding.get_attention_mask();
        let token_type_ids = encoding.get_type_ids();
        let seq_len = input_ids.len();

        let input_ids =
            Tensor::new(input_ids, &self.device)?.reshape((1, seq_len))?;
        let attention_mask_tensor =
            Tensor::new(attention_mask, &self.device)?.reshape((1, seq_len))?;
        let token_type_ids =
            Tensor::new(token_type_ids, &self.device)?.reshape((1, seq_len))?;

        // Forward pass — returns the last hidden state [1, seq_len, 384].
        let hidden = self
            .model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask_tensor))
            .context("model forward pass failed")?;

        // Mean pooling: sum hidden states weighted by attention mask, divide
        // by the number of real (non-padding) tokens.
        let mask = attention_mask_tensor
            .unsqueeze(2)?
            .to_dtype(candle_core::DType::F32)?
            .broadcast_as(hidden.shape())?;
        let masked = hidden.mul(&mask)?;
        let summed = masked.sum(1)?;                       // [1, 384]
        let count = mask.sum(1)?.clamp(1e-9, f64::MAX)?;  // [1, 384]
        let pooled = summed.div(&count)?;                  // [1, 384]

        // L2 normalise.
        let norm = pooled
            .sqr()?
            .sum_keepdim(1)?
            .sqrt()?
            .clamp(1e-12, f64::MAX)?;
        let normalised = pooled.broadcast_div(&norm)?;

        let embedding: Vec<f32> = normalised.squeeze(0)?.to_vec1()?;
        debug_assert_eq!(embedding.len(), EMBEDDING_DIM);
        Ok(embedding)
    }

    /// Generate embeddings for multiple texts in one call.
    ///
    /// Each text is processed independently (no cross-padding) so varying
    /// lengths do not waste compute.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    /// Compute cosine similarity between two embedding vectors.
    ///
    /// Returns a value in \[-1, 1\]. Both vectors must be the same length.
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        debug_assert_eq!(a.len(), b.len(), "embedding dimensions must match");

        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        let denom = norm_a * norm_b;
        if denom < 1e-12 {
            0.0
        } else {
            dot / denom
        }
    }
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
}
