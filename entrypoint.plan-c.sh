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

# #845: optional api_key when AI_MEMORY_API_KEY is set. NOTE the
# substrate error message says "set [api] api_key" but the actual
# AppConfig schema (src/config.rs:2283) has `api_key` as a TOP-LEVEL
# Option<String> field — NOT inside an [api] section. Serde silently
# ignores unknown top-level subsections like [api], so the daemon
# sees api_key=None and the S5-C1 guard refuses to bind 0.0.0.0.
# Error-message drift filed as a separate sub-defect.
API_KEY_TOML=""
if [ -n "${AI_MEMORY_API_KEY:-}" ]; then
  API_KEY_TOML="api_key = \"${AI_MEMORY_API_KEY}\""
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
${API_KEY_TOML}

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

# Issue #878 — peer-mesh reach preflight.
#
# Plan-C re-test surfaced that `host.docker.internal:<port>` URLs in the
# peer mesh silently shadow when another host process owns those ports
# (SSH forwards, python -m http.server, stale docker proxy after
# `colima delete`, etc.). The published port-bind "succeeds" but quorum
# POSTs land on the unrelated process and 404 / hang forever.
#
# Long-term fix: user-defined bridge + container-DNS in
# `infra/plan-c/docker-compose.yml` (peer URLs become `http://ic-bob:19077`
# routed through the docker network, never touching the host). For
# operators still on the `host.docker.internal` recipe, the preflight
# below probes every declared peer URL before exec'ing `serve` and
# aborts with EX_CONFIG (78) if any peer is unreachable.
#
# Logic lives in `infra/plan-c/peer-preflight.sh` so it can be unit-
# tested via `tests/plan_c_preflight.sh` without bringing the rest of
# the entrypoint side-effects (mkdir /etc, /root/.config) into scope.
# Opt-out: `AI_MEMORY_SKIP_PEER_PREFLIGHT=1` disables the check.
PREFLIGHT="${AI_MEMORY_PEER_PREFLIGHT_SCRIPT:-/usr/local/lib/ai-memory/peer-preflight.sh}"
if [ ! -f "$PREFLIGHT" ]; then
  for candidate in \
    "$(dirname "$0")/infra/plan-c/peer-preflight.sh" \
    "/usr/local/lib/ai-memory/peer-preflight.sh"; do
    if [ -f "$candidate" ]; then
      PREFLIGHT="$candidate"
      break
    fi
  done
fi
if [ -f "$PREFLIGHT" ]; then
  PEER_URLS="$PEER_URLS" \
  AI_MEMORY_SKIP_PEER_PREFLIGHT="${AI_MEMORY_SKIP_PEER_PREFLIGHT:-0}" \
    bash "$PREFLIGHT" || exit $?
else
  echo "[entrypoint] #878 preflight script not found at any candidate path — skipping" >&2
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
