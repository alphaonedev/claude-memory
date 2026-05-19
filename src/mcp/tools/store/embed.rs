// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Source-embed pipeline + post-insert HNSW warm-up.
//!
//! #881 (PR-4 extraction): split out of the monolithic
//! `src/mcp/tools/store.rs` so the embed-before-store branch lives in
//! its own ~70-LOC module. Wire-compat preserved verbatim: every
//! tracing warn label is byte-identical to the pre-#881 inline code
//! path.
//!
//! The embed pass runs AFTER `db::insert` lands so the row id is
//! known. The embedder + HNSW writes are best-effort — a failure
//! degrades recall on this row but does NOT roll back the store.
//!
//! Form 2 synchronous atomisation SKIPs the source embed entirely;
//! the atomiser archives the parent with `atomised_into > 0` and
//! atoms get their own embed-on-insert path. The store handler
//! resolves the skip flag and gates this helper accordingly.

use crate::embeddings::Embed;
use crate::hnsw::VectorIndex;
use crate::models::Memory;
use crate::{db, models::AutoAtomiseMode};

use super::AUTONOMY_MIN_CONTENT_LEN;

/// Decide whether to skip the source embed because Form 2 synchronous
/// atomisation will run the per-atom embed path instead. Mirrors the
/// pre-#881 inline guard in `handle_store`.
pub(super) fn skip_source_embed_for_synchronous_atomise(
    atomise_mode: AutoAtomiseMode,
    content_len: usize,
) -> bool {
    atomise_mode == AutoAtomiseMode::Synchronous && content_len >= AUTONOMY_MIN_CONTENT_LEN
}

/// Generate the embedding for the just-stored memory and persist it
/// to the `embeddings` table + the HNSW vector index. Best-effort —
/// any failure logs at WARN and lets the store response proceed
/// without the embedding (degrades recall on this row, never blocks
/// the write).
pub(super) fn store_source_embedding(
    conn: &rusqlite::Connection,
    embedder: &dyn Embed,
    mem: &Memory,
    actual_id: &str,
    vector_index: Option<&VectorIndex>,
) {
    let text = format!("{} {}", mem.title, mem.content);
    match embedder.embed(&text) {
        Ok(embedding) => {
            if let Err(e) = db::set_embedding(conn, actual_id, &embedding) {
                tracing::warn!("failed to store embedding for {}: {}", actual_id, e);
            }
            // Add to HNSW index for fast ANN search.
            if let Some(idx) = vector_index {
                idx.insert(actual_id.to_string(), embedding);
            }
        }
        Err(e) => {
            tracing::warn!("failed to generate embedding for {}: {}", actual_id, e);
        }
    }
}
