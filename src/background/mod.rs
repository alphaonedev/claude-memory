// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-3 — daemon-side background tasks landed for the
//! context-offload substrate primitive.
//!
//! Today this carries just the daily TTL sweep for `offloaded_blobs`.
//! Future v0.8.0 substrate work (Mermaid-canvas projection refresh,
//! short-term-context auto-cadence) layers on without churning the
//! daemon_runtime spawn surface.

pub mod offload_ttl_sweep;
