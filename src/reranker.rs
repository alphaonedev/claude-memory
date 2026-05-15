// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Cross-encoder reranking for search results.
//!
//! A cross-encoder takes a (query, document) pair and produces a relevance
//! score. This is more accurate than cosine similarity of independent
//! embeddings but slower since it must run for each candidate.
//!
//! **Two implementations:**
//! - `CrossEncoder::Lexical` — lightweight term-overlap scorer (default).
//! - `CrossEncoder::Neural` — BERT-based cross-encoder loaded via candle
//!   from `cross-encoder/ms-marco-MiniLM-L-6-v2` (~80 MB, ONNX-free).

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Sender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use hf_hub::{Repo, RepoType, api::sync::Api};
use tokenizers::Tokenizer;

use crate::models::Memory;

/// Blend weight applied to the original (embedding/FTS) score.
const ORIGINAL_WEIGHT: f64 = 0.6;
/// Blend weight applied to the cross-encoder score.
const CROSS_ENCODER_WEIGHT: f64 = 0.4;

const CROSS_ENCODER_MODEL_ID: &str = "cross-encoder/ms-marco-MiniLM-L-6-v2";
const CROSS_ENCODER_MAX_SEQ: usize = 512;
const CROSS_ENCODER_HIDDEN_DIM: usize = 384;

/// v0.7.0 L2-8 — default multiplicative boost applied to `Reflection`-kind
/// memories AFTER cross-encoder reranking. Reflections summarise multiple
/// observations, so abstraction-shaped queries ("what patterns...",
/// "what are recurring themes...") should preferentially surface them.
/// Default value `1.2` sits in the band where a reflection with a base
/// score equal to its source observations consistently lifts into the
/// top-5 without dragging mediocre reflections above well-matched
/// observations.
pub const DEFAULT_REFLECTION_BOOST: f32 = 1.2;

/// v0.7.0 L2-8 — default per-depth additional multiplier increment.
/// `per_depth_factor = 1.0 + per_depth_increment * reflection_depth`.
/// Deeper reflections (reflections-on-reflections) compress more
/// observations, so a small per-depth bump is justified.
pub const DEFAULT_REFLECTION_PER_DEPTH_INCREMENT: f32 = 0.05;

/// v0.7.0 L2-8 — default depth cap mirrored from
/// [`GovernancePolicy::effective_max_reflection_depth`]. Past this depth
/// the per-depth multiplier stops growing; reflections deeper than the
/// cap still receive the cap-evaluated boost (operator policy may refuse
/// the write entirely, but the reranker side never produces an unbounded
/// multiplier).
pub const DEFAULT_REFLECTION_MAX_DEPTH_CAP: u32 = 3;

/// v0.7.0 L2-8 — configuration for the reflection-aware reranker boost.
///
/// The boost is applied AFTER the cross-encoder blend (i.e. it does NOT
/// participate in the `0.6 * original + 0.4 * cross_encoder` scoring
/// formula). Boost shape:
///
/// ```text
/// per_depth_factor = 1.0 + per_depth_increment * min(reflection_depth, max_depth_cap)
/// final_score      = base_score * (kind == Reflection ? boost * per_depth_factor : 1.0)
/// ```
///
/// Default factor = `1.2` (see [`DEFAULT_REFLECTION_BOOST`]). Setting
/// `boost = 1.0` makes the reranker reproduce its pre-L2-8 behavior
/// exactly — a deliberate kill-switch for the recall regression suite.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReflectionBoostConfig {
    /// Multiplicative boost applied to `Reflection`-kind memories.
    /// Default `1.2`. `1.0` disables the boost.
    pub boost: f32,
    /// Per-depth additional multiplier increment. Default `0.05`.
    pub per_depth_increment: f32,
    /// Depth cap for the per-depth multiplier. Default `3` (mirrors
    /// the compiled-in default of
    /// `GovernancePolicy::effective_max_reflection_depth`). Larger
    /// `reflection_depth` values are clamped to this cap so the
    /// reranker never produces an unbounded multiplier.
    pub max_depth_cap: u32,
}

impl Default for ReflectionBoostConfig {
    fn default() -> Self {
        Self {
            boost: DEFAULT_REFLECTION_BOOST,
            per_depth_increment: DEFAULT_REFLECTION_PER_DEPTH_INCREMENT,
            max_depth_cap: DEFAULT_REFLECTION_MAX_DEPTH_CAP,
        }
    }
}

impl ReflectionBoostConfig {
    /// Pin to pre-L2-8 behavior: `boost = 1.0` ⇒ multiplier is always
    /// `1.0` regardless of memory kind or depth. Used by the regression
    /// test that proves the new pathway is a *pure addition* over the RC
    /// behavior.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            boost: 1.0,
            per_depth_increment: 0.0,
            max_depth_cap: 0,
        }
    }

    /// Compute the multiplicative factor for a given memory. Returns
    /// `1.0` for non-reflections; `boost * per_depth_factor` for
    /// reflections (with `reflection_depth` clamped to `max_depth_cap`).
    ///
    /// Pulled out so the same arithmetic is shared by both the per-query
    /// `rerank` and the G9 batched `rerank_batch` codepaths — there is
    /// exactly one place to audit the multiplier shape.
    #[must_use]
    pub fn factor_for(&self, mem: &Memory) -> f64 {
        if !matches!(mem.memory_kind, crate::models::MemoryKind::Reflection) {
            return 1.0;
        }
        // `reflection_depth` is stored as i32 (SQL signed) but the
        // governance accessor returns u32; the column DEFAULT is 0 and
        // negative values would already have been rejected by the
        // `memory_reflect` write path. Clamp to non-negative defensively
        // so a bad write upstream can't produce a negative multiplier.
        let depth = u32::try_from(mem.reflection_depth.max(0)).unwrap_or(0);
        let depth_clamped = depth.min(self.max_depth_cap);
        let per_depth_factor =
            f64::from(self.per_depth_increment).mul_add(f64::from(depth_clamped), 1.0);
        f64::from(self.boost) * per_depth_factor
    }
}

/// Cross-encoder for (query, document) relevance scoring.
pub enum CrossEncoder {
    /// Lightweight lexical cross-encoder using term overlap signals.
    ///
    /// `degraded` is `true` when this variant exists because a
    /// configured neural cross-encoder failed to initialise (HF Hub
    /// unreachable, model checksum mismatch, etc.) and the runtime
    /// fell back. `false` is the originally-configured lexical tier
    /// (operator opted in to keyword-tier or smart-tier without
    /// cross-encoder reranking).
    ///
    /// v0.7.0 R3-S2 — the distinction surfaces in the recall
    /// response's `meta.reranker_used` field as
    /// `"degraded_lexical"` vs `"lexical"`, so an in-band signal
    /// tells clients (MCP + HTTP) when their reranker downgraded.
    /// The original G8 fix landed `tracing::warn!` only; G8 closure
    /// per the playbook required an in-response field, which the
    /// prior implementation overstated.
    Lexical { degraded: bool },
    /// Neural BERT-based cross-encoder (ms-marco-MiniLM-L-6-v2).
    Neural {
        model: Arc<Mutex<BertModel>>,
        tokenizer: Arc<Tokenizer>,
        classifier_weight: Tensor,
        classifier_bias: Tensor,
        device: Device,
    },
}

impl CrossEncoder {
    /// Create a new lexical cross-encoder (no model download required).
    ///
    /// This is the "originally lexical" path — the operator either
    /// chose keyword-/semantic-tier (no cross-encoder reranking) or
    /// explicitly opted into the lexical variant. Use
    /// [`Self::new_neural`] to attempt the neural path with
    /// fall-back-to-lexical semantics.
    pub fn new() -> Self {
        Self::Lexical { degraded: false }
    }

    /// Create a neural cross-encoder by downloading ms-marco-MiniLM-L-6-v2.
    ///
    /// Falls back to lexical if download or loading fails. The
    /// fallback is marked `degraded: true` so the recall response
    /// surfaces `reranker_used = "degraded_lexical"` per R3-S2 — an
    /// in-band signal that v0.7.0 promises but pre-R3 only emitted
    /// as a `tracing::warn!` (a tracing-event-only fallback is not
    /// the same as a per-response field operators can branch on).
    ///
    /// v0.6.3.1 (P3, G8): when the neural path fails (e.g. HF Hub
    /// unreachable, model checksum mismatch), emit a structured tracing
    /// event `reranker.fallback` so operators see the silent
    /// neural→lexical degrade. The eprintln remains for backward-compat
    /// startup logs.
    pub fn new_neural() -> Self {
        match Self::load_neural() {
            Ok(ce) => ce,
            Err(e) => {
                tracing::warn!(
                    target: "reranker.fallback",
                    from = "neural",
                    to = "lexical",
                    reason = %e,
                    "cross-encoder fell back to lexical: neural init failed"
                );
                eprintln!("ai-memory: neural cross-encoder failed ({e}), using lexical fallback");
                Self::Lexical { degraded: true }
            }
        }
    }

    fn load_neural() -> Result<Self> {
        let device = Device::Cpu;

        let api = Api::new().context("failed to init HuggingFace Hub API")?;
        let repo = api.repo(Repo::new(
            CROSS_ENCODER_MODEL_ID.to_string(),
            RepoType::Model,
        ));

        let config_path = repo
            .get("config.json")
            .context("failed to download config.json")?;
        let tokenizer_path = repo
            .get("tokenizer.json")
            .context("failed to download tokenizer.json")?;
        let weights_path = repo
            .get("model.safetensors")
            .context("failed to download model.safetensors")?;

        // Load BERT config
        let config_data = std::fs::read_to_string(&config_path)
            .context("failed to read cross-encoder config.json")?;
        let config: BertConfig = serde_json::from_str(&config_data)
            .context("failed to parse cross-encoder config.json")?;

        // Load tokenizer
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("failed to load cross-encoder tokenizer: {e}"))?;
        let truncation = tokenizers::TruncationParams {
            max_length: CROSS_ENCODER_MAX_SEQ,
            ..Default::default()
        };
        tokenizer
            .with_truncation(Some(truncation))
            .map_err(|e| anyhow::anyhow!("failed to set truncation: {e}"))?;
        tokenizer.with_padding(None);

        // Load model weights
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights_path], candle_core::DType::F32, &device)
                .context("failed to load cross-encoder weights")?
        };

        let model = BertModel::load(vb.clone(), &config)
            .context("failed to build cross-encoder BertModel")?;

        // Load the classification head: classifier.weight [1, hidden_dim] and classifier.bias [1]
        let classifier_weight = vb
            .get((1, CROSS_ENCODER_HIDDEN_DIM), "classifier.weight")
            .context("failed to load classifier.weight")?;
        let classifier_bias = vb
            .get(1, "classifier.bias")
            .context("failed to load classifier.bias")?;

        Ok(Self::Neural {
            model: Arc::new(Mutex::new(model)),
            tokenizer: Arc::new(tokenizer),
            classifier_weight,
            classifier_bias,
            device,
        })
    }

    /// Score a single (query, document) pair.
    ///
    /// Returns a relevance score in `0.0..=1.0`.
    pub fn score(&self, query: &str, title: &str, content: &str) -> f32 {
        match self {
            Self::Lexical { .. } => lexical_score(query, title, content),
            Self::Neural {
                model,
                tokenizer,
                classifier_weight,
                classifier_bias,
                device,
            } => {
                let model_guard = match model.lock() {
                    Ok(g) => g,
                    Err(e) => {
                        tracing::warn!("cross-encoder model lock poisoned: {e}");
                        return lexical_score(query, title, content);
                    }
                };
                match Self::neural_score(
                    &model_guard,
                    tokenizer,
                    classifier_weight,
                    classifier_bias,
                    device,
                    query,
                    title,
                    content,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(
                            "neural cross-encoder score failed: {e}, using lexical fallback"
                        );
                        lexical_score(query, title, content)
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn neural_score(
        model: &BertModel,
        tokenizer: &Tokenizer,
        classifier_weight: &Tensor,
        classifier_bias: &Tensor,
        device: &Device,
        query: &str,
        title: &str,
        content: &str,
    ) -> Result<f32> {
        // Cross-encoder input: "[CLS] query [SEP] title content [SEP]"
        let document = format!("{title} {content}");

        let encoding = tokenizer
            .encode((query, document.as_str()), true)
            .map_err(|e| anyhow::anyhow!("cross-encoder tokenization failed: {e}"))?;

        let input_ids = encoding.get_ids();
        let attention_mask = encoding.get_attention_mask();
        let token_type_ids = encoding.get_type_ids();
        let seq_len = input_ids.len();

        let input_ids = Tensor::new(input_ids, device)?.reshape((1, seq_len))?;
        let attention_mask = Tensor::new(attention_mask, device)?.reshape((1, seq_len))?;
        let token_type_ids = Tensor::new(token_type_ids, device)?.reshape((1, seq_len))?;

        // Forward pass through BERT → [1, seq_len, 384]
        let hidden = model.forward(&input_ids, &token_type_ids, Some(&attention_mask))?;

        // Take [CLS] token (first token) → [1, 384]
        let cls = hidden.narrow(1, 0, 1)?.squeeze(1)?;

        // Classification head: logit = cls @ weight^T + bias → [1, 1]
        let logit = cls
            .matmul(&classifier_weight.t()?)?
            .broadcast_add(classifier_bias)?;

        // Extract scalar logit and apply sigmoid to get [0, 1] score
        let logit_val: f32 = logit.squeeze(0)?.squeeze(0)?.to_scalar()?;
        let score = 1.0 / (1.0 + (-logit_val).exp());

        Ok(score)
    }

    /// Whether this is a neural cross-encoder.
    pub fn is_neural(&self) -> bool {
        matches!(self, Self::Neural { .. })
    }

    /// v0.7.0 R3-S2 — whether this cross-encoder is a *degraded*
    /// lexical fallback (i.e., a neural variant was attempted at
    /// startup or mid-flight and the runtime fell back). `false` for
    /// `Neural` and for the originally-configured `Lexical` (operator
    /// opted into keyword-/semantic-tier without cross-encoder
    /// reranking). The recall response surfaces this distinction as
    /// `meta.reranker_used = "degraded_lexical"` so clients can
    /// detect the silent downgrade in-band — closing the G8 closure
    /// claim that tracing-event-only signalling had overstated.
    #[must_use]
    pub fn is_degraded_lexical(&self) -> bool {
        matches!(self, Self::Lexical { degraded: true })
    }

    /// Rerank a set of candidates by blending their original scores with
    /// cross-encoder scores.
    ///
    /// **Blend formula:** `final = 0.6 * original + 0.4 * cross_encoder`
    ///
    /// Results are returned sorted by `final_score` descending.
    ///
    /// **v0.7.0 L2-8 contract:** the bare `rerank` is the *pre-L2-8*
    /// behavior — no reflection boost is applied. Daemons that want
    /// the reflection-aware boost must call
    /// [`Self::rerank_with_reflection_boost`] (which is what
    /// [`BatchedReranker`] does by default with
    /// [`ReflectionBoostConfig::default`]). Keeping the bare method
    /// boost-free is a deliberate regression-pin discipline: the L2-8
    /// recall test for `boost = 1.0` uses
    /// `rerank_with_reflection_boost(.., &ReflectionBoostConfig::disabled())`
    /// and asserts byte-identical output to `rerank(..)`.
    pub fn rerank(&self, query: &str, mut candidates: Vec<(Memory, f64)>) -> Vec<(Memory, f64)> {
        let mut scored: Vec<(Memory, f64)> = candidates
            .drain(..)
            .map(|(mem, original_score)| {
                let ce_score = f64::from(self.score(query, &mem.title, &mem.content));
                let final_score =
                    ORIGINAL_WEIGHT * original_score + CROSS_ENCODER_WEIGHT * ce_score;
                (mem, final_score)
            })
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored
    }

    /// v0.7.0 L2-8 — rerank with a post-step reflection-aware boost.
    ///
    /// 1. Same blend as [`Self::rerank`] (`0.6 * original + 0.4 * ce`).
    /// 2. **After** the blend, multiply each candidate's `final_score`
    ///    by [`ReflectionBoostConfig::factor_for`]. Observations get a
    ///    multiplier of `1.0` (unchanged); reflections get
    ///    `boost * (1.0 + per_depth_increment * clamp(depth, 0..=cap))`.
    /// 3. Sort descending after the boost so the output ordering
    ///    reflects the post-boost ranking.
    ///
    /// Operationally this means: a reflection that the cross-encoder
    /// scored at parity with its source observations *moves up*; the
    /// movement is bounded (capped per-depth multiplier, single global
    /// `boost` factor) so a mediocre reflection cannot leapfrog a
    /// well-matched observation — the boost is a thumb-on-the-scale,
    /// not a free pass.
    pub fn rerank_with_reflection_boost(
        &self,
        query: &str,
        mut candidates: Vec<(Memory, f64)>,
        boost_config: &ReflectionBoostConfig,
    ) -> Vec<(Memory, f64)> {
        let mut scored: Vec<(Memory, f64)> = candidates
            .drain(..)
            .map(|(mem, original_score)| {
                let ce_score = f64::from(self.score(query, &mem.title, &mem.content));
                let blended = ORIGINAL_WEIGHT * original_score + CROSS_ENCODER_WEIGHT * ce_score;
                let factor = boost_config.factor_for(&mem);
                (mem, blended * factor)
            })
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored
    }

    /// v0.7 G9 — batched rerank for concurrent recall.
    ///
    /// Process all `(query, candidates)` jobs in a single tokenize + single
    /// forward pass on the Neural variant, holding the BERT mutex once for
    /// the whole batch instead of once per (query, candidate) pair.
    ///
    /// **Throughput target**: ~3× for parallel recall vs. per-query
    /// `rerank()` calls.
    ///
    /// Output ordering: `result[i]` corresponds to `queries[i]`. Each
    /// inner vector is sorted by descending blended score, identical to
    /// `rerank()`. Lexical variant delegates per-query (no batching win
    /// since lexical scoring is already CPU-trivial).
    pub fn rerank_batch(
        &self,
        queries: Vec<(String, Vec<(Memory, f64)>)>,
    ) -> Vec<Vec<(Memory, f64)>> {
        // Boost-free legacy entry point — preserves the pre-L2-8 wire
        // shape for callers that haven't migrated to the boost-aware
        // variant.  See `rerank_batch_with_reflection_boost` for the
        // L2-8 path; here we delegate to it with the `disabled()`
        // config so the implementation lives in one place.
        self.rerank_batch_with_reflection_boost(queries, &ReflectionBoostConfig::disabled())
    }

    /// v0.7.0 L2-8 — batched rerank with a post-step reflection-aware
    /// boost applied per candidate. Same boost arithmetic as
    /// [`Self::rerank_with_reflection_boost`], factored so the boost
    /// shape lives in a single helper.
    pub fn rerank_batch_with_reflection_boost(
        &self,
        queries: Vec<(String, Vec<(Memory, f64)>)>,
        boost_config: &ReflectionBoostConfig,
    ) -> Vec<Vec<(Memory, f64)>> {
        // Single-query short-circuit: avoid any batching overhead.
        if queries.len() == 1 {
            let mut iter = queries.into_iter();
            let (q, cands) = iter.next().expect("len == 1");
            return vec![self.rerank_with_reflection_boost(&q, cands, boost_config)];
        }

        match self {
            Self::Lexical { .. } => queries
                .into_iter()
                .map(|(q, cands)| self.rerank_with_reflection_boost(&q, cands, boost_config))
                .collect(),
            Self::Neural {
                model,
                tokenizer,
                classifier_weight,
                classifier_bias,
                device,
            } => {
                let model_guard = match model.lock() {
                    Ok(g) => g,
                    Err(e) => {
                        tracing::warn!(
                            "cross-encoder model lock poisoned in rerank_batch: {e}, falling back to lexical per-query"
                        );
                        return queries
                            .into_iter()
                            .map(|(q, cands)| {
                                // The fall-back here is a *runtime* degrade
                                // (the model lock poisoned mid-flight), so
                                // surface it as degraded lexical to mirror
                                // R3-S2 reranker_mode semantics.
                                let lex = Self::Lexical { degraded: true };
                                lex.rerank_with_reflection_boost(&q, cands, boost_config)
                            })
                            .collect();
                    }
                };

                match Self::neural_rerank_batch(
                    &model_guard,
                    tokenizer,
                    classifier_weight,
                    classifier_bias,
                    device,
                    &queries,
                ) {
                    Ok(scores) => {
                        // scores is a flat Vec<f32>, one per (query_idx,
                        // candidate_idx) in row-major order matching
                        // queries.iter().flat_map(|(_, cs)| cs).
                        let mut out = Vec::with_capacity(queries.len());
                        let mut cursor = 0usize;
                        for (_query, cands) in queries {
                            let n = cands.len();
                            let mut scored: Vec<(Memory, f64)> = cands
                                .into_iter()
                                .enumerate()
                                .map(|(i, (mem, original))| {
                                    let ce = f64::from(scores[cursor + i]);
                                    let blended =
                                        ORIGINAL_WEIGHT * original + CROSS_ENCODER_WEIGHT * ce;
                                    let factor = boost_config.factor_for(&mem);
                                    (mem, blended * factor)
                                })
                                .collect();
                            cursor += n;
                            scored.sort_by(|a, b| {
                                b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
                            });
                            out.push(scored);
                        }
                        out
                    }
                    Err(e) => {
                        tracing::warn!(
                            "neural rerank_batch failed: {e}, falling back to lexical per-query"
                        );
                        drop(model_guard);
                        queries
                            .into_iter()
                            .map(|(q, cands)| {
                                // Runtime degrade (forward-pass failure) —
                                // mark the variant degraded so the recall
                                // response can surface `degraded_lexical`.
                                let lex = Self::Lexical { degraded: true };
                                lex.rerank_with_reflection_boost(&q, cands, boost_config)
                            })
                            .collect()
                    }
                }
            }
        }
    }

    /// One tokenize + one forward pass over a flat batch of (query, doc)
    /// pairs. Returns a flat `Vec<f32>` of sigmoided logits in the same
    /// row-major order the candidates appear in `queries`.
    fn neural_rerank_batch(
        model: &BertModel,
        tokenizer: &Tokenizer,
        classifier_weight: &Tensor,
        classifier_bias: &Tensor,
        device: &Device,
        queries: &[(String, Vec<(Memory, f64)>)],
    ) -> Result<Vec<f32>> {
        // Build the flat (query, document) pair list.
        let mut pairs: Vec<(&str, String)> = Vec::new();
        for (q, cands) in queries {
            for (mem, _) in cands {
                let document = format!("{} {}", mem.title, mem.content);
                pairs.push((q.as_str(), document));
            }
        }
        if pairs.is_empty() {
            return Ok(Vec::new());
        }

        // Variable-length pairs require padding for a single forward pass.
        // Clone the tokenizer so we can mutate padding settings without
        // racing other threads on the shared `Arc<Tokenizer>`.
        let mut batch_tokenizer = tokenizer.clone();
        let padding = tokenizers::PaddingParams {
            strategy: tokenizers::PaddingStrategy::BatchLongest,
            direction: tokenizers::PaddingDirection::Right,
            pad_id: 0,
            pad_type_id: 0,
            pad_token: "[PAD]".to_string(),
            ..Default::default()
        };
        batch_tokenizer.with_padding(Some(padding));

        let encodings = batch_tokenizer
            .encode_batch(
                pairs
                    .into_iter()
                    .map(|(q, d)| tokenizers::EncodeInput::Dual(q.into(), d.into()))
                    .collect::<Vec<_>>(),
                true,
            )
            .map_err(|e| anyhow::anyhow!("cross-encoder batch tokenization failed: {e}"))?;

        let batch_size = encodings.len();
        let seq_len = encodings.first().map(|e| e.get_ids().len()).unwrap_or(0);

        let mut input_ids: Vec<u32> = Vec::with_capacity(batch_size * seq_len);
        let mut attn_mask: Vec<u32> = Vec::with_capacity(batch_size * seq_len);
        let mut token_types: Vec<u32> = Vec::with_capacity(batch_size * seq_len);
        for enc in &encodings {
            input_ids.extend_from_slice(enc.get_ids());
            attn_mask.extend_from_slice(enc.get_attention_mask());
            token_types.extend_from_slice(enc.get_type_ids());
        }

        let input_ids = Tensor::from_vec(input_ids, (batch_size, seq_len), device)?;
        let attention_mask = Tensor::from_vec(attn_mask, (batch_size, seq_len), device)?;
        let token_type_ids = Tensor::from_vec(token_types, (batch_size, seq_len), device)?;

        // Forward pass → [batch, seq, 384]
        let hidden = model.forward(&input_ids, &token_type_ids, Some(&attention_mask))?;

        // [CLS] token per row → [batch, 384]
        let cls = hidden.narrow(1, 0, 1)?.squeeze(1)?;

        // Classification head per row → [batch, 1]
        let logits = cls
            .matmul(&classifier_weight.t()?)?
            .broadcast_add(classifier_bias)?;

        let logits_vec: Vec<f32> = logits.squeeze(1)?.to_vec1()?;
        Ok(logits_vec
            .into_iter()
            .map(|l| 1.0 / (1.0 + (-l).exp()))
            .collect())
    }
}

impl Default for CrossEncoder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Lexical cross-encoder (original implementation)
// ---------------------------------------------------------------------------

fn lexical_score(query: &str, title: &str, content: &str) -> f32 {
    let query_terms = tokenize(query);
    if query_terms.is_empty() {
        return 0.0;
    }

    let title_terms = tokenize(title);
    let content_terms = tokenize(content);

    let doc_terms: HashSet<&str> = title_terms
        .iter()
        .chain(content_terms.iter())
        .copied()
        .collect();
    let query_set: HashSet<&str> = query_terms.iter().copied().collect();

    // 1. Jaccard term overlap
    #[allow(clippy::cast_precision_loss)]
    let intersection = query_set.intersection(&doc_terms).count() as f32;
    #[allow(clippy::cast_precision_loss)]
    let union = query_set.union(&doc_terms).count() as f32;
    let jaccard = if union > 0.0 {
        intersection / union
    } else {
        0.0
    };

    // 2. TF-IDF-like term weighting
    let doc_all: Vec<&str> = title_terms
        .iter()
        .chain(content_terms.iter())
        .copied()
        .collect();
    let tf_idf = tfidf_score(&query_terms, &doc_all);

    // 3. Bigram overlap bonus
    let query_bigrams = bigrams(&query_terms);
    let doc_bigrams = bigrams(&doc_all);
    let bigram_overlap = if query_bigrams.is_empty() {
        0.0
    } else {
        let doc_bigram_set: HashSet<(&str, &str)> = doc_bigrams.into_iter().collect();
        #[allow(clippy::cast_precision_loss)]
        let hits = query_bigrams
            .iter()
            .filter(|b| doc_bigram_set.contains(b))
            .count() as f32;
        #[allow(clippy::cast_precision_loss)]
        let query_bigrams_len = query_bigrams.len() as f32;
        hits / query_bigrams_len
    };

    // 4. Title match bonus
    let title_set: HashSet<&str> = title_terms.iter().copied().collect();
    #[allow(clippy::cast_precision_loss)]
    let title_hits = query_set.intersection(&title_set).count() as f32;
    #[allow(clippy::cast_precision_loss)]
    let title_bonus = if query_set.is_empty() {
        0.0
    } else {
        title_hits / query_set.len() as f32
    };

    let raw = 0.30 * jaccard + 0.30 * tf_idf + 0.20 * bigram_overlap + 0.20 * title_bonus;
    raw.clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn tokenize(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric() && c != '\'')
        .filter(|w| !w.is_empty())
        .collect()
}

fn tfidf_score(query_terms: &[&str], doc_tokens: &[&str]) -> f32 {
    if doc_tokens.is_empty() || query_terms.is_empty() {
        return 0.0;
    }

    let mut tf_map: HashMap<&str, usize> = HashMap::new();
    for &tok in doc_tokens {
        *tf_map.entry(tok).or_insert(0) += 1;
    }

    #[allow(clippy::cast_precision_loss)]
    let total = doc_tokens.len() as f32;
    #[allow(clippy::cast_precision_loss)]
    let unique = tf_map.len() as f32;

    let mut score_sum: f32 = 0.0;
    let query_lower: Vec<String> = query_terms.iter().map(|t| t.to_lowercase()).collect();

    for qt in &query_lower {
        #[allow(clippy::cast_precision_loss)]
        let tf = tf_map
            .iter()
            .filter(|(k, _)| k.to_lowercase() == *qt)
            .map(|(_, &v)| v)
            .sum::<usize>() as f32;

        if tf == 0.0 {
            continue;
        }

        let tf_norm = tf / total;
        #[allow(clippy::cast_precision_loss)]
        let doc_freq = tf_map.keys().filter(|k| k.to_lowercase() == *qt).count() as f32;
        let idf = (unique / (1.0 + doc_freq)).ln() + 1.0;

        score_sum += tf_norm * idf;
    }

    #[allow(clippy::cast_precision_loss)]
    let max_possible = query_lower.len() as f32;
    (score_sum / max_possible).clamp(0.0, 1.0)
}

fn bigrams<'a>(tokens: &'a [&str]) -> Vec<(&'a str, &'a str)> {
    tokens.windows(2).map(|w| (w[0], w[1])).collect()
}

// ---------------------------------------------------------------------------
// v0.7 G9 — concurrent rerank coalescer
// ---------------------------------------------------------------------------

/// Default upper bound on how many requests we coalesce per BERT call.
pub const DEFAULT_MAX_BATCH: usize = 32;

/// Default flush latency (ms) — how long the worker waits for more requests
/// before processing a non-full batch. 5ms keeps single-request latency
/// negligible while still benefiting parallel callers.
pub const DEFAULT_MAX_WAIT_MS: u64 = 5;

/// Job submitted to the coalescer worker.
struct RerankJob {
    query: String,
    candidates: Vec<(Memory, f64)>,
    reply: std::sync::mpsc::SyncSender<Vec<(Memory, f64)>>,
}

/// Concurrent rerank coalescer.
///
/// Wraps a `CrossEncoder` and serializes concurrent recall reranks through
/// a single worker thread. The worker buffers up to `max_batch` requests
/// or waits up to `max_wait_ms` (whichever first), then issues one
/// `rerank_batch` call. The Mutex around the BERT model is held for the
/// whole batch instead of once per (query, candidate) — the throughput
/// fix mandated by G9.
///
/// **Single-request latency**: the worker flushes immediately when the
/// queue is empty after pulling the first job, so a lone request only
/// pays one `recv_timeout(0)` round-trip — no artificial waiting.
pub struct BatchedReranker {
    sender: Option<Sender<RerankJob>>,
    /// H2 (v0.7.0 round-2) — explicit one-shot shutdown signal. The
    /// worker thread selects on BOTH the work channel and this
    /// shutdown channel; receiving on the shutdown channel makes the
    /// worker exit its loop deterministically, even if a holder of
    /// `sender` happens to outlive `Drop` (e.g. the test harness
    /// stashed a `Sender` clone). `Drop` triggers this BEFORE dropping
    /// `sender`, so a worker that is currently blocked in
    /// `rx.recv()` wakes up via the shutdown channel without waiting
    /// for the work-channel disconnect.
    shutdown: Option<std::sync::mpsc::Sender<()>>,
    worker: Option<JoinHandle<()>>,
    /// Direct handle to the underlying encoder, used for the single-query
    /// short-circuit and for callers that explicitly want non-batched
    /// behavior (tests, benchmarks).
    encoder: Arc<CrossEncoder>,
    /// v0.7.0 L2-8 — reflection-aware boost config the worker hands
    /// down to every batched `rerank` call.  Defaults to
    /// [`ReflectionBoostConfig::default`] (boost = 1.2) so the daemon
    /// flow ships the boost; explicit configuration goes through
    /// [`Self::with_reflection_boost`] before the worker starts taking
    /// jobs.
    reflection_boost: ReflectionBoostConfig,
}

impl BatchedReranker {
    /// Wrap an existing `CrossEncoder` with the default batching parameters
    /// (`max_batch = 32`, `max_wait_ms = 5`).
    pub fn new(encoder: CrossEncoder) -> Self {
        Self::with_params(encoder, DEFAULT_MAX_BATCH, DEFAULT_MAX_WAIT_MS)
    }

    /// Wrap an existing `CrossEncoder` with custom batching parameters.
    pub fn with_params(encoder: CrossEncoder, max_batch: usize, max_wait_ms: u64) -> Self {
        Self::with_full_params(
            encoder,
            max_batch,
            max_wait_ms,
            ReflectionBoostConfig::default(),
        )
    }

    /// v0.7.0 L2-8 — wrap an existing `CrossEncoder` with a custom
    /// reflection-boost config alongside default batching parameters.
    /// Used by the recall integration tests to pin specific boost shapes
    /// (e.g. `disabled()` for the regression test).
    pub fn with_reflection_boost(encoder: CrossEncoder, boost: ReflectionBoostConfig) -> Self {
        Self::with_full_params(encoder, DEFAULT_MAX_BATCH, DEFAULT_MAX_WAIT_MS, boost)
    }

    /// Internal constructor — all knobs visible.
    fn with_full_params(
        encoder: CrossEncoder,
        max_batch: usize,
        max_wait_ms: u64,
        reflection_boost: ReflectionBoostConfig,
    ) -> Self {
        let encoder = Arc::new(encoder);
        let (tx, rx) = std::sync::mpsc::channel::<RerankJob>();
        // H2 (v0.7.0 round-2) — one-shot shutdown channel. The std
        // mpsc channel is used as a "oneshot": we never send more
        // than one value, and the worker exits on the first
        // `try_recv()` success OR on disconnect (Drop of the holder
        // closes the sender side, which also surfaces as a recv
        // outcome the worker can branch on).
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
        let worker_encoder = Arc::clone(&encoder);
        let worker_boost = reflection_boost;
        let max_wait = Duration::from_millis(max_wait_ms);

        let worker = thread::Builder::new()
            .name("ai-memory-reranker-batcher".into())
            .spawn(move || {
                // H2 polling cadence: when waiting for the first job
                // of a batch, fall back to `recv_timeout` so the worker
                // wakes up periodically to check the shutdown signal.
                // 100ms keeps the test in `test_drop_terminates_worker`
                // comfortably inside its 500ms budget while staying
                // well below the 5ms intra-batch coalescing window
                // (no cost to the hot path).
                const SHUTDOWN_POLL: Duration = Duration::from_millis(100);
                'outer: loop {
                    // Block until the first job arrives OR the
                    // shutdown signal fires OR the sender drops.
                    let first = loop {
                        // Cheap non-blocking shutdown check first so a
                        // signal that arrived between iterations is
                        // observed even if the work channel had a job
                        // queued before the signal landed.
                        match shutdown_rx.try_recv() {
                            Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                break 'outer;
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => {}
                        }
                        match rx.recv_timeout(SHUTDOWN_POLL) {
                            Ok(job) => break job,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                break 'outer;
                            }
                        }
                    };

                    let mut batch: Vec<RerankJob> = Vec::with_capacity(max_batch);
                    batch.push(first);

                    // Coalesce additional jobs that arrive within the
                    // window, up to the batch cap.
                    let deadline = Instant::now() + max_wait;
                    while batch.len() < max_batch {
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        match rx.recv_timeout(deadline - now) {
                            Ok(j) => batch.push(j),
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                // Drain the current batch then exit.
                                process_batch(&worker_encoder, batch, &worker_boost);
                                break 'outer;
                            }
                        }
                    }

                    process_batch(&worker_encoder, batch, &worker_boost);
                }
            })
            .expect("failed to spawn rerank batcher worker");

        Self {
            sender: Some(tx),
            shutdown: Some(shutdown_tx),
            worker: Some(worker),
            encoder,
            reflection_boost,
        }
    }

    /// Submit a single rerank request. Blocks until the batcher returns
    /// the result. Concurrent callers are coalesced into a single
    /// `rerank_batch` call inside the worker thread.
    ///
    /// If the worker is unavailable for any reason (channel closed),
    /// falls back to a direct `rerank` call on the underlying encoder
    /// (with the wrapper's configured reflection boost applied).
    pub fn rerank(&self, query: &str, candidates: Vec<(Memory, f64)>) -> Vec<(Memory, f64)> {
        let Some(sender) = self.sender.as_ref() else {
            return self.encoder.rerank_with_reflection_boost(
                query,
                candidates,
                &self.reflection_boost,
            );
        };
        let (reply_tx, reply_rx) = sync_channel::<Vec<(Memory, f64)>>(1);
        let job = RerankJob {
            query: query.to_string(),
            candidates,
            reply: reply_tx,
        };
        if sender.send(job).is_err() {
            return self.encoder.rerank_with_reflection_boost(
                query,
                Vec::new(),
                &self.reflection_boost,
            );
        }
        reply_rx.recv().unwrap_or_else(|_| {
            self.encoder
                .rerank_with_reflection_boost(query, Vec::new(), &self.reflection_boost)
        })
    }

    /// v0.7.0 L2-8 — expose the configured boost for the
    /// `memory_capabilities` reporter.
    #[must_use]
    pub fn reflection_boost(&self) -> &ReflectionBoostConfig {
        &self.reflection_boost
    }

    /// Direct access to the wrapped encoder. Useful for callers that
    /// want to bypass the coalescer (tests, benchmarks).
    pub fn encoder(&self) -> &CrossEncoder {
        &self.encoder
    }

    /// Convenience shortcut for `self.encoder().is_neural()`. Most
    /// callers in the recall pipeline only need to check the variant
    /// for capability reporting.
    pub fn is_neural(&self) -> bool {
        self.encoder.is_neural()
    }

    /// v0.7.0 R3-S2 — shortcut for `self.encoder().is_degraded_lexical()`.
    /// The recall path reads this to drive the in-band `reranker_used`
    /// signal exposed via `RecallMeta`.
    #[must_use]
    pub fn is_degraded_lexical(&self) -> bool {
        self.encoder.is_degraded_lexical()
    }
}

impl Drop for BatchedReranker {
    fn drop(&mut self) {
        // H2 (v0.7.0 round-2): two-step termination.
        //
        //   1. Fire the explicit shutdown signal FIRST so the worker
        //      observes it even when another holder of `Sender`
        //      (e.g. a test that cloned the work channel) would
        //      otherwise keep the work channel alive.
        //   2. Then drop the work-channel sender — a worker that was
        //      blocked in `rx.recv_timeout(...)` wakes up either via
        //      the shutdown poll OR the disconnect, whichever
        //      happens first.
        //
        // Joining the worker after BOTH signals fire bounds shutdown
        // by the SHUTDOWN_POLL cadence (100ms) in the absolute worst
        // case, well inside the 500ms budget exercised by
        // `test_drop_terminates_worker`.
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.sender.take();
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

fn process_batch(
    encoder: &CrossEncoder,
    batch: Vec<RerankJob>,
    boost_config: &ReflectionBoostConfig,
) {
    if batch.is_empty() {
        return;
    }

    // Single-request fast path: bypass the batched API to avoid the
    // padding overhead and any latency regression on lone callers.
    if batch.len() == 1 {
        let mut iter = batch.into_iter();
        let job = iter.next().expect("len == 1");
        let result = encoder.rerank_with_reflection_boost(&job.query, job.candidates, boost_config);
        let _ = job.reply.send(result);
        return;
    }

    // Build the input vector for the batched call. Use placeholder
    // `Memory` clones via `take` to avoid copying — we move out.
    let mut queries: Vec<(String, Vec<(Memory, f64)>)> = Vec::with_capacity(batch.len());
    let mut replies: Vec<std::sync::mpsc::SyncSender<Vec<(Memory, f64)>>> =
        Vec::with_capacity(batch.len());
    for job in batch {
        queries.push((job.query, job.candidates));
        replies.push(job.reply);
    }

    let outputs = encoder.rerank_batch_with_reflection_boost(queries, boost_config);
    for (out, reply) in outputs.into_iter().zip(replies.into_iter()) {
        let _ = reply.send(out);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Memory, Tier};

    fn make_memory(title: &str, content: &str) -> Memory {
        Memory {
            id: "test-id".to_string(),
            tier: Tier::Mid,
            namespace: "test".to_string(),
            title: title.to_string(),
            content: content.to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        }
    }

    #[test]
    fn lexical_score_returns_zero_for_empty_query() {
        assert_eq!(lexical_score("", "some title", "some content"), 0.0);
    }

    #[test]
    fn lexical_score_returns_zero_for_no_overlap() {
        let s = lexical_score("quantum physics", "grocery list", "milk eggs bread butter");
        assert!(s < 0.05, "expected near-zero, got {s}");
    }

    #[test]
    fn lexical_score_rewards_title_match() {
        let content = "This document discusses network configuration for LAN setups.";
        let s_title_match = lexical_score(
            "network configuration",
            "Network Configuration Guide",
            content,
        );
        let s_no_title = lexical_score("network configuration", "Unrelated Title", content);
        assert!(
            s_title_match > s_no_title,
            "title match ({s_title_match}) should beat no title match ({s_no_title})"
        );
    }

    #[test]
    fn lexical_score_is_bounded_zero_one() {
        let s = lexical_score(
            "the quick brown fox jumps over the lazy dog",
            "the quick brown fox",
            "the quick brown fox jumps over the lazy dog and more words",
        );
        assert!((0.0..=1.0).contains(&s), "score {s} out of bounds");
    }

    #[test]
    fn rerank_reorders_candidates() {
        let ce = CrossEncoder::new();
        let a = make_memory("Rust cross-encoder", "cross-encoder reranking for search");
        let b = make_memory("Grocery list", "milk eggs bread butter cheese");
        let candidates = vec![(b.clone(), 0.55), (a.clone(), 0.45)];
        let reranked = ce.rerank("cross-encoder reranking", candidates);
        assert_eq!(reranked[0].0.title, "Rust cross-encoder");
    }

    #[test]
    fn rerank_preserves_candidate_count() {
        let ce = CrossEncoder::new();
        let candidates = vec![
            (make_memory("A", "alpha"), 0.5),
            (make_memory("B", "beta"), 0.6),
            (make_memory("C", "gamma"), 0.7),
        ];
        let reranked = ce.rerank("alpha", candidates);
        assert_eq!(reranked.len(), 3);
    }

    #[test]
    fn bigram_overlap_boosts_phrase_match() {
        let s_phrase = lexical_score(
            "network adapter",
            "title",
            "the network adapter is connected to the LAN",
        );
        let s_scattered = lexical_score(
            "network adapter",
            "title",
            "the adapter handles the network traffic independently",
        );
        assert!(
            s_phrase > s_scattered,
            "phrase match ({s_phrase}) should beat scattered ({s_scattered})"
        );
    }

    // -----------------------------------------------------------------
    // W11/S11b — input-count invariants for the rerank() API
    // -----------------------------------------------------------------

    #[test]
    fn test_rerank_preserves_input_count_heuristic() {
        let ce = CrossEncoder::new();
        // Build 5 distinct candidates with varied original scores.
        let candidates: Vec<(Memory, f64)> = (0..5)
            .map(|i| {
                (
                    make_memory(
                        &format!("title {i}"),
                        &format!("content body number {i} with some words"),
                    ),
                    f64::from(i) * 0.1,
                )
            })
            .collect();
        let query = "title content body";
        let reranked = ce.rerank(query, candidates);
        assert_eq!(
            reranked.len(),
            5,
            "heuristic rerank must preserve candidate count, got {} = {:?}",
            reranked.len(),
            reranked
                .iter()
                .map(|(m, s)| (&m.title, *s))
                .collect::<Vec<_>>()
        );
        // Sorted descending by final score (rerank contract).
        for w in reranked.windows(2) {
            assert!(
                w[0].1 >= w[1].1,
                "rerank output must be descending by score: {} < {}",
                w[0].1,
                w[1].1
            );
        }
    }

    #[test]
    fn test_rerank_zero_candidates_returns_empty_heuristic() {
        let ce = CrossEncoder::new();
        let reranked = ce.rerank("query", Vec::new());
        assert!(reranked.is_empty());
    }

    // Neural variant: gated to avoid pulling 80MB BERT weights at test time.
    // Run with `--features test-with-models` once the cross-encoder feature
    // exists upstream.
    #[cfg(feature = "test-with-models")]
    #[test]
    fn test_rerank_preserves_input_count_neural_if_available() {
        let ce = CrossEncoder::new_neural();
        let candidates: Vec<(Memory, f64)> = (0..5)
            .map(|i| (make_memory(&format!("t{i}"), &format!("body {i}")), 0.5))
            .collect();
        let reranked = ce.rerank("body", candidates);
        assert_eq!(reranked.len(), 5);
    }

    // -----------------------------------------------------------------
    // W12-E — heuristic-path branch coverage for reranker.rs
    //
    // Targets the Lexical variant only. The Neural variant requires
    // downloading 80+ MB of BERT weights from HuggingFace Hub and is
    // gated behind `feature = "test-with-models"`.
    // -----------------------------------------------------------------

    #[test]
    fn w12e_default_is_lexical() {
        let ce = CrossEncoder::default();
        assert!(!ce.is_neural(), "Default::default() must return Lexical");
    }

    #[test]
    fn w12e_new_returns_lexical() {
        let ce = CrossEncoder::new();
        assert!(!ce.is_neural());
    }

    #[test]
    fn w12e_score_dispatch_lexical_matches_helper() {
        // The CrossEncoder::score() dispatcher must delegate to lexical_score()
        // for the Lexical variant. Compute both and assert exact equality.
        let ce = CrossEncoder::new();
        let q = "rust async runtime";
        let title = "Tokio: Rust async runtime";
        let content = "Tokio is an async runtime for the Rust programming language.";
        let via_dispatcher = ce.score(q, title, content);
        let direct = lexical_score(q, title, content);
        assert!((via_dispatcher - direct).abs() < f32::EPSILON);
    }

    #[test]
    fn w12e_score_empty_inputs_safe() {
        let ce = CrossEncoder::new();
        // Empty query → 0.0 by short-circuit in lexical_score
        assert_eq!(ce.score("", "title", "content"), 0.0);
        // Empty title and content with non-empty query — must not panic
        let s = ce.score("query", "", "");
        assert!((0.0..=1.0).contains(&s));
        // Whitespace-only query treated as empty after tokenization
        let s_ws = ce.score("   \t\n", "title", "content");
        assert_eq!(s_ws, 0.0);
        // Punctuation-only query also yields no tokens
        let s_punct = ce.score("!?.,;:", "title", "content");
        assert_eq!(s_punct, 0.0);
    }

    #[test]
    fn w12e_lexical_score_is_bounded_for_unicode_and_long() {
        // Mixed Unicode tokens with apostrophes, accents, emoji boundaries.
        let s_unicode = lexical_score(
            "café résumé d'oeuvre",
            "Le Café d'Oeuvre",
            "résumé du café avec d'oeuvre noté",
        );
        assert!(
            (0.0..=1.0).contains(&s_unicode),
            "unicode score {s_unicode} out of bounds"
        );

        // Very long content stresses the length-normalization branches.
        let huge = "alpha beta gamma delta ".repeat(2_500);
        let s_long = lexical_score("alpha gamma", "headline", &huge);
        assert!(
            (0.0..=1.0).contains(&s_long),
            "long score {s_long} out of bounds"
        );
    }

    #[test]
    fn w12e_lexical_score_perfect_overlap_high() {
        // 100% query overlap with title and content should produce a high
        // (but bounded) score.
        let s = lexical_score(
            "alpha beta gamma",
            "alpha beta gamma",
            "alpha beta gamma alpha beta gamma",
        );
        assert!(s > 0.5, "expected high score for perfect overlap, got {s}");
        assert!(s <= 1.0);
    }

    #[test]
    fn w12e_tfidf_score_empty_doc_returns_zero() {
        // Branch: doc_tokens.is_empty() → 0.0 short-circuit.
        let q = vec!["alpha", "beta"];
        let doc: Vec<&str> = Vec::new();
        assert_eq!(tfidf_score(&q, &doc), 0.0);
    }

    #[test]
    fn w12e_tfidf_score_empty_query_returns_zero() {
        // Branch: query_terms.is_empty() → 0.0 short-circuit.
        let q: Vec<&str> = Vec::new();
        let doc = vec!["alpha", "beta", "gamma"];
        assert_eq!(tfidf_score(&q, &doc), 0.0);
    }

    #[test]
    fn w12e_tfidf_score_no_matching_terms() {
        // Query terms entirely absent from doc → tf == 0 continue branch.
        let q = vec!["xenon", "kryptonite"];
        let doc = vec!["alpha", "beta", "gamma"];
        let s = tfidf_score(&q, &doc);
        assert_eq!(s, 0.0);
    }

    #[test]
    fn w12e_tfidf_score_partial_match_bounded() {
        // Mixed presence/absence; clamp branch reachable.
        let q = vec!["alpha", "missing"];
        let doc = vec!["alpha", "alpha", "beta", "gamma"];
        let s = tfidf_score(&q, &doc);
        assert!((0.0..=1.0).contains(&s));
        assert!(s > 0.0);
    }

    #[test]
    fn w12e_bigrams_empty_and_single_and_multi() {
        // Empty input → empty bigram list.
        let empty: Vec<&str> = Vec::new();
        assert!(bigrams(&empty).is_empty());

        // Single token → no bigrams (windows(2) yields nothing).
        let one = vec!["solo"];
        assert!(bigrams(&one).is_empty());

        // Multi-token → N-1 bigrams.
        let three = vec!["a", "b", "c"];
        let bg = bigrams(&three);
        assert_eq!(bg, vec![("a", "b"), ("b", "c")]);
    }

    #[test]
    fn w12e_tokenize_handles_apostrophe_and_unicode() {
        // Apostrophes are preserved (e.g., "don't"), other punctuation splits.
        let toks = tokenize("don't stop, I won't!");
        assert!(toks.contains(&"don't"));
        assert!(toks.contains(&"won't"));
        assert!(toks.contains(&"stop"));
        assert!(toks.contains(&"I"));

        // Pure-punctuation yields no tokens.
        let none = tokenize("!!!,,,;;;");
        assert!(none.is_empty());

        // Empty string yields no tokens.
        let empty = tokenize("");
        assert!(empty.is_empty());

        // Unicode alphanumerics survive (café = 4 alphanumeric chars).
        let unicode = tokenize("café résumé");
        assert_eq!(unicode.len(), 2);
    }

    #[test]
    fn w12e_rerank_single_candidate_keeps_it() {
        let ce = CrossEncoder::new();
        let only = make_memory("solo title", "solo content body");
        let out = ce.rerank("solo", vec![(only.clone(), 0.42)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0.title, "solo title");
        // Final score is a blend of original and CE score, both nonneg.
        assert!(out[0].1 >= 0.0);
    }

    #[test]
    fn w12e_rerank_identical_originals_stable_under_score() {
        // When original scores are identical, ordering is determined by the
        // CE score. The candidate whose title/content overlaps the query
        // should rank first.
        let ce = CrossEncoder::new();
        let on_topic = make_memory("rust async runtime", "rust async runtime tokio");
        let off_topic = make_memory("grocery", "milk eggs bread");
        let out = ce.rerank(
            "rust async",
            vec![(off_topic.clone(), 0.5), (on_topic.clone(), 0.5)],
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0.title, "rust async runtime");
    }

    #[test]
    fn w12e_rerank_descending_invariant_holds_across_shapes() {
        // Property-style: irrespective of input shape, output is sorted desc.
        let ce = CrossEncoder::new();
        let cands: Vec<(Memory, f64)> = vec![
            (make_memory("a", "alpha words"), 0.10),
            (make_memory("b", "beta words"), 0.95),
            (make_memory("c", "gamma alpha"), 0.55),
            (make_memory("d", ""), 0.0),
            (make_memory("", "empty title doc"), 0.30),
        ];
        let out = ce.rerank("alpha", cands);
        assert_eq!(out.len(), 5);
        for w in out.windows(2) {
            assert!(
                w[0].1 >= w[1].1,
                "non-descending pair: {} then {}",
                w[0].1,
                w[1].1
            );
        }
    }

    #[test]
    fn w12e_lexical_score_no_title_branch_via_empty_title() {
        // Empty title means title_set is empty; title_bonus == 0.0.
        // query_set non-empty so the else branch (title_hits / |Q|) runs.
        let s_empty_title = lexical_score("alpha beta", "", "alpha beta gamma");
        let s_with_title = lexical_score("alpha beta", "alpha beta", "alpha beta gamma");
        assert!(s_with_title >= s_empty_title);
        assert!((0.0..=1.0).contains(&s_empty_title));
    }

    #[test]
    fn w12e_lexical_score_query_terms_only_in_title() {
        // Title contains all query terms; content has none.
        let s = lexical_score("rust crate", "Rust Crate Index", "unrelated body text");
        assert!(s > 0.0);
        assert!(s <= 1.0);
    }

    // PR-9i — buffer coverage uplift.

    #[test]
    fn pr9i_new_neural_dual_outcome() {
        // Exercises CrossEncoder::new_neural() (lines 65-79). Behavior is
        // environment-dependent: with an HF cache or network the call
        // succeeds and returns Self::Neural; without either it falls back
        // to Self::Lexical via the documented eprintln + tracing warn
        // pathway. Both outcomes are acceptable — what matters is the
        // dispatch is hit. Functionally, both variants score within
        // [0.0, 1.0].
        let ce = CrossEncoder::new_neural();
        let s = ce.score("query", "title", "content");
        assert!((0.0..=1.0).contains(&s), "score {s} out of bounds");
    }

    // -----------------------------------------------------------------
    // v0.7 G9 — batched rerank parity + coalescer smoke tests
    // -----------------------------------------------------------------

    #[test]
    fn g9_rerank_batch_matches_per_query_rerank_lexical() {
        // Spec: 3 queries × 5 candidates. Batched output must match
        // per-query rerank() output exactly for the deterministic Lexical
        // path. (Neural parity is gated behind `test-with-models`; the
        // implementation is symmetric — same blend, same sort.)
        let ce = CrossEncoder::new();
        let queries = vec!["alpha gamma", "beta words", "rust async"];
        let mut jobs: Vec<(String, Vec<(Memory, f64)>)> = Vec::new();
        let mut expected: Vec<Vec<(Memory, f64)>> = Vec::new();
        for q in &queries {
            let cands: Vec<(Memory, f64)> = (0..5)
                .map(|i| {
                    (
                        make_memory(
                            &format!("title-{i}-{q}"),
                            &format!("alpha beta gamma rust async body {i} {q}"),
                        ),
                        f64::from(i) * 0.1,
                    )
                })
                .collect();
            expected.push(ce.rerank(q, cands.clone()));
            jobs.push(((*q).to_string(), cands));
        }

        let batched = ce.rerank_batch(jobs);
        assert_eq!(batched.len(), expected.len());
        for (b, e) in batched.iter().zip(expected.iter()) {
            assert_eq!(b.len(), e.len());
            for (bi, ei) in b.iter().zip(e.iter()) {
                assert_eq!(bi.0.id, ei.0.id);
                assert_eq!(bi.0.title, ei.0.title);
                assert!(
                    (bi.1 - ei.1).abs() < 1e-12,
                    "blended score mismatch: batched={} per-query={}",
                    bi.1,
                    ei.1
                );
            }
        }
    }

    #[test]
    fn g9_rerank_batch_single_query_short_circuits() {
        // Single-query batches must not regress vs rerank() — use the
        // single-query short-circuit path.
        let ce = CrossEncoder::new();
        let cands: Vec<(Memory, f64)> = (0..5)
            .map(|i| (make_memory(&format!("t{i}"), &format!("body {i}")), 0.5))
            .collect();
        let direct = ce.rerank("body", cands.clone());
        let batched = ce.rerank_batch(vec![("body".to_string(), cands)]);
        assert_eq!(batched.len(), 1);
        assert_eq!(batched[0].len(), direct.len());
        for (a, b) in batched[0].iter().zip(direct.iter()) {
            assert_eq!(a.0.id, b.0.id);
            assert!((a.1 - b.1).abs() < 1e-12);
        }
    }

    #[test]
    fn g9_rerank_batch_empty_inputs() {
        let ce = CrossEncoder::new();
        let out = ce.rerank_batch(Vec::new());
        assert!(out.is_empty());

        // Multi-query but each has zero candidates.
        let out2 = ce.rerank_batch(vec![
            ("q1".to_string(), Vec::new()),
            ("q2".to_string(), Vec::new()),
        ]);
        assert_eq!(out2.len(), 2);
        assert!(out2.iter().all(std::vec::Vec::is_empty));
    }

    #[test]
    fn g9_batched_reranker_serial_calls_match_rerank() {
        use super::BatchedReranker;
        let batched = BatchedReranker::new(CrossEncoder::new());
        let cands: Vec<(Memory, f64)> = (0..4)
            .map(|i| {
                (
                    make_memory(
                        &format!("t{i}"),
                        &format!("alpha gamma body {i} content words"),
                    ),
                    f64::from(i) * 0.1,
                )
            })
            .collect();
        let direct = CrossEncoder::new().rerank("alpha", cands.clone());
        let via_batcher = batched.rerank("alpha", cands);
        assert_eq!(via_batcher.len(), direct.len());
        for (a, b) in via_batcher.iter().zip(direct.iter()) {
            assert_eq!(a.0.id, b.0.id);
            assert!((a.1 - b.1).abs() < 1e-12);
        }
    }

    #[test]
    fn g9_batched_reranker_concurrent_calls_all_succeed() {
        use super::BatchedReranker;
        use std::sync::Arc;
        let batched = Arc::new(BatchedReranker::new(CrossEncoder::new()));
        let mut handles = Vec::new();
        for i in 0..8 {
            let b = Arc::clone(&batched);
            handles.push(std::thread::spawn(move || {
                let cands: Vec<(Memory, f64)> = (0..5)
                    .map(|j| {
                        (
                            make_memory(
                                &format!("t{i}-{j}"),
                                &format!("body {j} alpha gamma rust"),
                            ),
                            0.5,
                        )
                    })
                    .collect();
                let q = format!("alpha {i}");
                let out = b.rerank(&q, cands);
                assert_eq!(out.len(), 5);
                // Output is sorted descending.
                for w in out.windows(2) {
                    assert!(w[0].1 >= w[1].1);
                }
            }));
        }
        for h in handles {
            h.join().expect("worker thread panicked");
        }
    }

    #[test]
    fn pr9i_rerank_via_score_returns_blend() {
        // Even when new_neural() falls back to lexical, rerank() must
        // still produce a deterministic [0..1] blend. Pins the contract
        // for both branches of CrossEncoder::score().
        let ce = CrossEncoder::new_neural();
        let cands = vec![
            (
                Memory {
                    id: "a".to_string(),
                    tier: Tier::Mid,
                    namespace: "ns".to_string(),
                    title: "rust async runtime".to_string(),
                    content: "tokio rust async".to_string(),
                    tags: vec![],
                    priority: 5,
                    confidence: 1.0,
                    source: "test".to_string(),
                    access_count: 0,
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    updated_at: "2026-01-01T00:00:00Z".to_string(),
                    last_accessed_at: None,
                    expires_at: None,
                    metadata: serde_json::json!({}),
                    reflection_depth: 0,
                    memory_kind: crate::models::MemoryKind::Observation,
                    entity_id: None,
                    persona_version: None,
                    citations: Vec::new(),
                    source_uri: None,
                    source_span: None,
                },
                0.6,
            ),
            (
                Memory {
                    id: "b".to_string(),
                    tier: Tier::Mid,
                    namespace: "ns".to_string(),
                    title: "grocery list".to_string(),
                    content: "milk eggs".to_string(),
                    tags: vec![],
                    priority: 5,
                    confidence: 1.0,
                    source: "test".to_string(),
                    access_count: 0,
                    created_at: "2026-01-01T00:00:00Z".to_string(),
                    updated_at: "2026-01-01T00:00:00Z".to_string(),
                    last_accessed_at: None,
                    expires_at: None,
                    metadata: serde_json::json!({}),
                    reflection_depth: 0,
                    memory_kind: crate::models::MemoryKind::Observation,
                    entity_id: None,
                    persona_version: None,
                    citations: Vec::new(),
                    source_uri: None,
                    source_span: None,
                },
                0.4,
            ),
        ];
        let out = ce.rerank("rust async", cands);
        assert_eq!(out.len(), 2);
        for (_, score) in &out {
            assert!(score.is_finite());
        }
        // First entry's blended score >= second by sort contract.
        assert!(out[0].1 >= out[1].1);
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

    /// Mock neural cross-encoder for testing. Returns deterministic scores
    /// based on (query, title, content) without loading BERT.
    pub struct MockCrossEncoder {
        pub use_neural: bool,
    }

    impl MockCrossEncoder {
        /// Create a mock lexical encoder (like CrossEncoder::new()).
        pub fn new() -> Self {
            Self { use_neural: false }
        }

        /// Create a mock neural encoder (like CrossEncoder::new_neural()).
        pub fn new_neural() -> Self {
            Self { use_neural: true }
        }

        /// Mock score: deterministic hash-based score in [0, 1].
        /// Neural path uses a different formula than lexical for testing.
        pub fn score(&self, query: &str, title: &str, content: &str) -> f32 {
            if self.use_neural {
                // Neural mock: combine query+title hash
                let combined = format!("{}{}", query, title);
                let hash = combined.bytes().fold(0u32, |acc, b| {
                    acc.wrapping_mul(31).wrapping_add(u32::from(b))
                });
                let base = ((hash % 1000) as f32) / 1000.0;
                // Boost for exact title matches
                if title.contains(query) {
                    (base * 0.5 + 0.5).min(1.0)
                } else {
                    base
                }
            } else {
                // Lexical path uses the real lexical_score
                lexical_score(query, title, content)
            }
        }

        /// Whether this is a neural mock.
        pub fn is_neural(&self) -> bool {
            self.use_neural
        }

        /// Rerank candidates (same blending formula as real CrossEncoder).
        pub fn rerank(
            &self,
            query: &str,
            mut candidates: Vec<(Memory, f64)>,
        ) -> Vec<(Memory, f64)> {
            let mut scored: Vec<(Memory, f64)> = candidates
                .drain(..)
                .map(|(mem, original_score)| {
                    let ce_score = f64::from(self.score(query, &mem.title, &mem.content));
                    let final_score =
                        ORIGINAL_WEIGHT * original_score + CROSS_ENCODER_WEIGHT * ce_score;
                    (mem, final_score)
                })
                .collect();

            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            scored
        }
    }

    impl Default for MockCrossEncoder {
        fn default() -> Self {
            Self::new()
        }
    }
}

#[cfg(test)]
mod mock_tests {
    use super::test_support::*;
    use super::{BatchedReranker, CrossEncoder};
    use crate::models::{Memory, Tier};
    use std::time::Duration;

    fn make_memory(title: &str, content: &str) -> Memory {
        Memory {
            id: "test-id".to_string(),
            tier: Tier::Mid,
            namespace: "test".to_string(),
            title: title.to_string(),
            content: content.to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        }
    }

    #[test]
    fn mock_lexical_new() {
        let ce = MockCrossEncoder::new();
        assert!(!ce.is_neural());
    }

    #[test]
    fn mock_neural_new() {
        let ce = MockCrossEncoder::new_neural();
        assert!(ce.is_neural());
    }

    #[test]
    fn mock_neural_score_deterministic() {
        let ce = MockCrossEncoder::new_neural();
        let s1 = ce.score("query", "title", "content");
        let s2 = ce.score("query", "title", "content");
        assert_eq!(s1, s2);
    }

    #[test]
    fn mock_neural_score_title_match_boost() {
        let ce = MockCrossEncoder::new_neural();
        let s_title_contains = ce.score("apple", "apple pie recipe", "delicious dessert");
        let s_no_match = ce.score("apple", "unrelated", "delicious dessert");
        assert!(
            s_title_contains > s_no_match,
            "title match ({s_title_contains}) should beat no match ({s_no_match})"
        );
    }

    #[test]
    fn mock_neural_score_bounded() {
        let ce = MockCrossEncoder::new_neural();
        for query in &["test", "neural", "reranker", "machine learning"] {
            for title in &["a", "b", "the quick brown"] {
                let s = ce.score(query, title, "content");
                assert!((0.0..=1.0).contains(&s), "score {s} out of bounds");
            }
        }
    }

    #[test]
    fn mock_neural_rerank_reorders() {
        let ce = MockCrossEncoder::new_neural();
        let a = make_memory("neural network", "deep learning with transformers");
        let b = make_memory("grocery list", "milk eggs bread butter");
        let candidates = vec![(b.clone(), 0.3), (a.clone(), 0.2)];
        let reranked = ce.rerank("neural network", candidates);
        // Neural encoder should boost the neural-network-titled memory
        assert_eq!(reranked[0].0.title, "neural network");
    }

    #[test]
    fn mock_neural_rerank_preserves_count() {
        let ce = MockCrossEncoder::new_neural();
        let candidates = vec![
            (make_memory("A", "content a"), 0.5),
            (make_memory("B", "content b"), 0.4),
            (make_memory("C", "content c"), 0.6),
        ];
        let reranked = ce.rerank("test", candidates);
        assert_eq!(reranked.len(), 3);
    }

    #[test]
    fn mock_lexical_path_via_mock() {
        let ce = MockCrossEncoder::new();
        let s = ce.score(
            "network adapter",
            "Network Configuration",
            "the network adapter is connected",
        );
        assert!((0.0..=1.0).contains(&s));
    }

    #[test]
    fn mock_neural_different_from_lexical() {
        let lexical = MockCrossEncoder::new();
        let neural = MockCrossEncoder::new_neural();
        let s_lex = lexical.score("machine learning", "ML title", "neural networks");
        let s_neu = neural.score("machine learning", "ML title", "neural networks");
        // They should use different scoring formulas
        assert_ne!(s_lex, s_neu);
    }

    // -----------------------------------------------------------------
    // H2 (v0.7.0 round-2) — worker-thread shutdown discipline.
    //
    // Contract: spawning a `BatchedReranker` and dropping it
    // immediately must terminate the worker thread within a bounded
    // wall-clock window. Without an explicit shutdown channel, a
    // worker that was blocked in `rx.recv()` would only exit on
    // sender disconnect; the explicit signal closes the worst-case
    // (e.g. a stashed `Sender` clone) and bounds the shutdown
    // latency by the worker's SHUTDOWN_POLL cadence.
    // -----------------------------------------------------------------
    #[test]
    fn h2_drop_terminates_worker_within_500ms() {
        use std::time::Instant;
        let reranker = BatchedReranker::new(CrossEncoder::new());
        // Capture the JoinHandle by exfiltrating it BEFORE drop so we
        // can observe thread termination from the outside. We
        // re-implement the Drop body inline for the assertion: fire
        // shutdown, drop sender, join with a wall-clock budget.
        let mut r = reranker;
        let shutdown = r.shutdown.take().expect("shutdown sender present");
        let worker = r.worker.take().expect("worker handle present");
        // Drop the work-channel sender first to mimic the same
        // disconnect semantics the production Drop sequence
        // produces.
        r.sender.take();
        let start = Instant::now();
        let _ = shutdown.send(());
        // Spawn the join on a side thread so we can apply a hard
        // wall-clock budget. `JoinHandle::join` does not take a
        // timeout, so the side-thread + park-with-deadline form is
        // the idiomatic Rust pattern.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _ = worker.join();
            let _ = done_tx.send(());
        });
        let observed = done_rx
            .recv_timeout(Duration::from_millis(500))
            .map(|()| Instant::now().duration_since(start));
        assert!(
            observed.is_ok(),
            "BatchedReranker worker did not terminate within 500ms after \
             explicit shutdown — observed: {observed:?}"
        );
    }
}

#[test]
fn score_handles_empty_query_string() {
    let s = lexical_score("", "Document Title", "This is document content");
    assert_eq!(s, 0.0, "empty query must return 0.0");
}

#[test]
fn score_handles_unicode_normalization() {
    // Query with accented characters, document with decomposed/composed variants
    let s1 = lexical_score("café", "café", "the café is open");
    let s2 = lexical_score("cafe", "cafe", "the cafe is open");
    // Both should score positively; exact equality not required due to normalization
    assert!(s1 > 0.0);
    assert!(s2 > 0.0);
}

#[test]
fn score_handles_very_long_content_truncation() {
    // Query and document with extreme length (lexical tokenizer should handle it)
    let long_content = "word ".repeat(10000); // 50k+ chars
    let s = lexical_score("word", "title", &long_content);
    assert!((0.0..=1.0).contains(&s), "score must be bounded [0, 1]");
}

#[test]
fn bigram_score_with_single_token_query() {
    // Query with only one token — bigrams should be empty, no crash
    let s = lexical_score("query", "Single Token Title", "single token content");
    assert!((0.0..=1.0).contains(&s));
}
