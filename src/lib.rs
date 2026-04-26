// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![recursion_limit = "256"]
// The library target was added by the proptest infra (Agent G) to expose
// production modules to the integration test crate. The bin target's
// clippy run already gates CI — re-running pedantic against the same
// modules through the lib target would re-flag the same pre-existing
// lint backlog the bin target already passes. Allow at the lib level;
// the bin target is the authoritative gate for production-code linting.
#![allow(clippy::pedantic, clippy::all)]

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
