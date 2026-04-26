// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![recursion_limit = "256"]

// Library interface for ai-memory. Exposes public modules for testing and external use.

pub mod autonomy;
pub mod bench;
pub mod color;
pub mod config;
pub mod curator;
pub mod db;
pub mod embeddings;
pub mod errors;
pub mod federation;
pub mod handlers;
pub mod hnsw;
pub mod identity;
pub mod llm;
pub mod mcp;
pub mod metrics;
pub mod mine;
pub mod models;
pub mod replication;
pub mod reranker;
pub mod subscriptions;
pub mod toon;
pub mod validate;

#[cfg(feature = "sal")]
pub mod migrate;

#[cfg(feature = "sal")]
pub mod store;
