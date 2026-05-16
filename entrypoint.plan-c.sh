#!/usr/bin/env bash
# Plan C — ai-memory daemon entrypoint.
# Generates daemon keypair on first start, writes config, then exec's serve.
set -euo pipefail

# Required env (validated):
: "${AI_MEMORY_AGENT_ID:?missing}"
: "${AI_MEMORY_STORE_URL:?missing}"
: "${AI_MEMORY_LISTEN_HOST:?missing}"
: "${AI_MEMORY_LISTEN_PORT:?missing}"
: "${OLLAMA_BASE_URL:?missing}"

# Optional env
TIER="${AI_MEMORY_TIER:-autonomous}"
LLM_MODEL="${AI_MEMORY_LLM_MODEL:-gemma4:e4b}"
# v0.7.0 L15 — auto_tag goes through a fast, non-thinking model to keep
# the autonomy hook bounded by the H8 30s per-LLM-call timeout. Gemma 4
# e4b in thinking mode generates 396-564 tokens for a 5-tag prompt
# (Plan C R4 cert observed `H8: LLM call (auto_tag) exceeded 30s`).
# Gemma 3 4b runs the same prompt in ~0.7s. Override via env if the
# operator has a different fast model loaded.
AUTO_TAG_MODEL="${AI_MEMORY_AUTO_TAG_MODEL:-gemma3:4b}"
EMBED_MODEL="${AI_MEMORY_EMBED_MODEL:-nomic-embed-text}"
PEER_URLS="${AI_MEMORY_PEER_URLS:-}"
TLS_DIR="${AI_MEMORY_TLS_DIR:-/etc/ai-memory-a2a/tls}"

mkdir -p /etc/ai-memory /root/.config/ai-memory

# Daemon keypair (refuse-by-default per Round-4 fix)
if [ ! -f /root/.config/ai-memory/keys/daemon.priv ]; then
  /usr/local/bin/ai-memory identity generate --agent-id daemon --json
fi

# config.toml — top-level fields per AppConfig (see src/config.rs line 1433).
# Sections [memory], [autonomous], [governance], [federation] are NOT valid
# AppConfig keys — serde silently ignores them, falling through to defaults.
# Federation comes from CLI flags below. Plan C uses autonomous tier:
# Gemma 4 LLM + nomic-embed-text 768-dim embedder via Ollama + cross-encoder.
cat >/root/.config/ai-memory/config.toml <<TOML
tier = "${TIER}"
ollama_url = "${OLLAMA_BASE_URL}"
embed_url = "${OLLAMA_BASE_URL}"
embedding_model = "nomic_embed_v15"
llm_model = "${LLM_MODEL}"
auto_tag_model = "${AUTO_TAG_MODEL}"
cross_encoder = true

[audit]
enabled = true
path = "/var/log/ai-memory/audit"
redact_content = true
hash_chain = true

[permissions]
mode = "enforce"
TOML
mkdir -p /etc/ai-memory
cp /root/.config/ai-memory/config.toml /etc/ai-memory/config.toml

# Server-side TLS flags (HTTPS + mTLS via fingerprint allowlist).
# `serve` accepts only --tls-cert, --tls-key, --mtls-allowlist on the
# server path. CA cert is only used on the outbound quorum-client path
# (--quorum-ca-cert below).
TLS_FLAGS=""
if [ -f "$TLS_DIR/server.pem" ] && [ -f "$TLS_DIR/server.key" ]; then
  TLS_FLAGS="--tls-cert $TLS_DIR/server.pem --tls-key $TLS_DIR/server.key"
  if [ -f "$TLS_DIR/mtls-allowlist.txt" ]; then
    TLS_FLAGS="$TLS_FLAGS --mtls-allowlist $TLS_DIR/mtls-allowlist.txt"
  fi
fi

# Quorum mTLS (when peer set + client cert)
QUORUM_FLAGS=""
if [ -n "$PEER_URLS" ]; then
  QUORUM_FLAGS="--quorum-writes 2 --quorum-peers $PEER_URLS"
  if [ -f "$TLS_DIR/client.pem" ] && [ -f "$TLS_DIR/client.key" ]; then
    QUORUM_FLAGS="$QUORUM_FLAGS --quorum-client-cert $TLS_DIR/client.pem --quorum-client-key $TLS_DIR/client.key"
    [ -f "$TLS_DIR/ca.pem" ] && QUORUM_FLAGS="$QUORUM_FLAGS --quorum-ca-cert $TLS_DIR/ca.pem"
  fi
fi

echo "[entrypoint] starting ai-memory:"
echo "  agent_id=$AI_MEMORY_AGENT_ID tier=$TIER listen=$AI_MEMORY_LISTEN_HOST:$AI_MEMORY_LISTEN_PORT"
echo "  store_url=$(echo $AI_MEMORY_STORE_URL | sed -E 's|//[^@]+@|//<redacted>@|')"
echo "  ollama=$OLLAMA_BASE_URL llm=$LLM_MODEL embed=$EMBED_MODEL auto_tag=$AUTO_TAG_MODEL"
echo "  peers=${PEER_URLS:-(none)}"
echo "  tls_flags='$TLS_FLAGS'"
echo "  quorum_flags='$QUORUM_FLAGS'"

unset AI_MEMORY_DB
export AI_MEMORY_AGENT_ID=daemon
export RUST_LOG="${RUST_LOG:-ai_memory=info}"

exec /usr/local/bin/ai-memory serve \
  --host "$AI_MEMORY_LISTEN_HOST" --port "$AI_MEMORY_LISTEN_PORT" \
  --store-url "$AI_MEMORY_STORE_URL" \
  $TLS_FLAGS \
  $QUORUM_FLAGS
