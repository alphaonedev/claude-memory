// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Context, Result};
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use hf_hub::{api::sync::Api, Repo, RepoType};
use std::sync::Arc;
use tokenizers::Tokenizer;

use crate::config::EmbeddingModel;

const MINILM_MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";
#[allow(dead_code)]
const MINILM_DIM: usize = 384;
const MAX_SEQ_LEN: usize = 256;
/// Fallback subdirectory under $HOME for pre-downloaded MiniLM model files
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
        model: Arc<BertModel>,
        tokenizer: Arc<Tokenizer>,
        device: Device,
    },
    /// Ollama-based embedding (nomic-embed-text-v1.5, 768-dim)
    Ollama {
        client: Arc<crate::llm::OllamaClient>,
        model_name: String,
    },
}

// BertModel does not implement Send/Sync by default but the CPU-backed
// tensors are safe to share across threads.
unsafe impl Send for Embedder {}
unsafe impl Sync for Embedder {}

impl Embedder {
    /// Create a local candle embedder (MiniLM-L6-v2, 384-dim).
    pub fn new_local() -> Result<Self> {
        let device = Device::Cpu;

        let (config_path, tokenizer_path, weights_path) = match Self::download_via_hf_hub() {
            Ok(paths) => paths,
            Err(e) => {
                eprintln!(
                    "ai-memory: hf-hub download failed ({}), trying fallback dir",
                    e
                );
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
            model: Arc::new(model),
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
            } => Self::embed_local(model, tokenizer, device, text),
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
        if denom < 1e-12 {
            0.0
        } else {
            dot / denom
        }
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
}
