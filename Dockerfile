# syntax=docker/dockerfile:1

# ---- Build stage ----
FROM rust:1.94-slim AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY benches/ benches/

RUN cargo build --release && strip target/release/ai-memory

# ---- Runtime stage ----
FROM debian:bookworm-slim

LABEL org.opencontainers.image.title="ai-memory" \
      org.opencontainers.image.description="AI-agnostic persistent memory system — MCP server, HTTP API, and CLI" \
      org.opencontainers.image.version="0.6.0-alpha.1" \
      org.opencontainers.image.source="https://github.com/alphaonedev/ai-memory-mcp" \
      org.opencontainers.image.licenses="Apache-2.0" \
      org.opencontainers.image.vendor="AlphaOne LLC" \
      io.modelcontextprotocol.server.name="io.github.alphaonedev/ai-memory"

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system aimem \
    && useradd --system --gid aimem --create-home aimem \
    && mkdir -p /data && chown aimem:aimem /data

COPY --from=builder /build/target/release/ai-memory /usr/local/bin/ai-memory

ENV AI_MEMORY_DB=/data/ai-memory.db

VOLUME /data
EXPOSE 9077

USER aimem

ENTRYPOINT ["ai-memory"]
CMD ["serve", "--host", "0.0.0.0"]
