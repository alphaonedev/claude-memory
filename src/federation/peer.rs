// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Peer construction and `FederationConfig::build`.

use std::time::Duration;

use crate::replication::QuorumPolicy;

use super::{FederationConfig, PeerEndpoint};

impl FederationConfig {
    /// Build a `FederationConfig` from the serve-time CLI flags. Returns
    /// `None` when federation is disabled (`quorum_writes == 0` or the
    /// peer list is empty).
    ///
    /// `api_key` carries the local daemon's configured `[api] api_key`
    /// (issue #702, v0.7.0 fold-A2A1.4). When `Some`, every outbound
    /// federation POST attaches an `x-api-key` header so peers that
    /// themselves run with api-key auth accept the request. `None`
    /// preserves the backwards-compatible header set used by mTLS-only
    /// deployments — outbound POSTs stay unauthenticated at the
    /// application layer and rely on the TLS layer for trust.
    ///
    /// # Errors
    ///
    /// Returns an error if the reqwest client cannot be constructed
    /// with the supplied certificate material.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        quorum_writes: usize,
        peer_urls: &[String],
        timeout: Duration,
        client_cert_path: Option<&std::path::Path>,
        client_key_path: Option<&std::path::Path>,
        ca_cert_path: Option<&std::path::Path>,
        sender_agent_id: String,
        api_key: Option<String>,
    ) -> anyhow::Result<Option<Self>> {
        if quorum_writes == 0 || peer_urls.is_empty() {
            return Ok(None);
        }
        // Ultrareview #341: reject duplicate peer URLs at build time.
        // If the same peer URL appears twice under different indices,
        // both would count as distinct ack sources and the quorum
        // guarantee is violated. Normalize (trim trailing slash,
        // lowercase scheme+host) before comparing.
        let mut seen_urls: std::collections::HashSet<String> = std::collections::HashSet::new();
        for raw in peer_urls {
            let normalized = raw.trim_end_matches('/').to_ascii_lowercase();
            if !seen_urls.insert(normalized.clone()) {
                return Err(anyhow::anyhow!(
                    "duplicate peer URL in --quorum-peers: {raw} (normalized: {normalized}) \
                     — duplicates would let a single peer contribute to quorum more than once"
                ));
            }
        }
        let n = 1 + peer_urls.len(); // local node + remotes
        let policy = QuorumPolicy::new(n, quorum_writes, timeout, Duration::from_secs(30))
            .map_err(|e| anyhow::anyhow!("invalid quorum policy: {e}"))?;
        let peers: Vec<PeerEndpoint> = peer_urls
            .iter()
            .enumerate()
            .map(|(i, raw)| {
                // `id` is used as a Prometheus metric label; keep it
                // low-cardinality. The full URL is logged separately.
                // (#304 nit — prior form `peer-{i}:{url}` blew up the
                // label space as deployment size grew.)
                let trimmed = raw.trim_end_matches('/');
                tracing::debug!(
                    target = "federation",
                    peer_index = i,
                    url = trimmed,
                    "registered peer"
                );
                PeerEndpoint {
                    id: format!("peer-{i}"),
                    sync_push_url: format!("{trimmed}/api/v1/sync/push"),
                }
            })
            .collect();

        // Federation client tuning.
        //
        // An earlier PR #314 attempted tight `tcp_keepalive(1s)` +
        // `pool_idle_timeout(5s)` on this builder to close the Phase
        // 4 partition_minority convergence gap. Ship-gate run 21
        // showed that combination caused Phase 4 to hang for 40+
        // minutes — suspected cause was connection-pool churn on the
        // chaos-client's local 3-process mesh exhausting ephemeral
        // ports under continuous close+reopen cycles with the tight
        // keepalive generating probe traffic on every idle socket.
        //
        // Reverted to the conservative-default client here. Partition-
        // recovery under chaos is moved out of the required ship-gate
        // and into an opt-in campaign shape. Real partition resilience
        // is a v0.6.0.1+ investigation with instrumented cycle data
        // (cycles_by_fault now landed in ship-gate, giving us per-cycle
        // visibility the next time we attempt this).
        let mut client_builder = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(Duration::from_secs(2))
            .use_rustls_tls();
        // --quorum-ca-cert: trust a caller-supplied root CA for outbound
        // federation POSTs. Required whenever peers present a cert NOT
        // rooted in webpki-roots (Mozilla CA bundle) — e.g. a self-
        // signed / ephemeral CA generated for an isolated test fleet.
        // Without this, reqwest's rustls-tls feature (webpki-roots
        // only) rejects the peer cert and every quorum write times
        // out as quorum_not_met. See alphaonedev/ai-memory-mcp#333.
        if let Some(ca_path) = ca_cert_path {
            let ca_pem = std::fs::read(ca_path)
                .map_err(|e| anyhow::anyhow!("read --quorum-ca-cert: {e}"))?;
            let ca = reqwest::Certificate::from_pem(&ca_pem)
                .map_err(|e| anyhow::anyhow!("parse --quorum-ca-cert: {e}"))?;
            client_builder = client_builder.add_root_certificate(ca);
        }
        if let (Some(cert), Some(key)) = (client_cert_path, client_key_path) {
            let cert_pem =
                std::fs::read(cert).map_err(|e| anyhow::anyhow!("read --client-cert: {e}"))?;
            let key_pem =
                std::fs::read(key).map_err(|e| anyhow::anyhow!("read --client-key: {e}"))?;
            let mut pem = cert_pem;
            pem.extend_from_slice(b"\n");
            pem.extend_from_slice(&key_pem);
            let identity = reqwest::Identity::from_pem(&pem)
                .map_err(|e| anyhow::anyhow!("build mTLS identity: {e}"))?;
            client_builder = client_builder.identity(identity);
        }
        let client = client_builder
            .build()
            .map_err(|e| anyhow::anyhow!("build federation client: {e}"))?;

        // v0.7.0 #791 — load the daemon's Ed25519 signing key (no-op
        // stub here; the full #697 audit/identity module loads from
        // disk). NON-FATAL: peers running `AI_MEMORY_FED_REQUIRE_SIG=0`
        // accept unsigned posts even when the signing key is missing.
        let signing_key = crate::governance::audit::load_daemon_signing_key(&sender_agent_id)
            .ok()
            .flatten()
            .map(std::sync::Arc::new);
        Ok(Some(Self {
            policy,
            peers,
            client,
            sender_agent_id,
            api_key,
            signing_key,
        }))
    }

    /// Count of peers in the mesh (excludes the local node). Useful for
    /// metrics labels.
    #[must_use]
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }
}
