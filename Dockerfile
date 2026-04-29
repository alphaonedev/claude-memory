# syntax=docker/dockerfile:1

# ---- Build stage ----
# Pin to bookworm so the produced binary's glibc matches the runtime
# stage (debian:bookworm-slim, glibc 2.36). Without the explicit
# bookworm tag, rust:1.94-slim resolves to a trixie-based image
# (glibc 2.41) and the binary fails at startup with
# `version GLIBC_2.39 not found` — caught by the dockerfile-validate
# CI job (PR #465 retrospective; v0.6.5 bake).
FROM rust:1.94-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY benches/ benches/
# v0.6.3 added include_str! references to migration SQL files
# (Streams A-C schema v15: migrations/sqlite/0010_v063_hierarchy_kg.sql).
# Without the migrations/ directory in the build context, cargo build
# fails at compile time. Pre-existing Dockerfile gap that v0.6.2 did
# not surface (no new migrations).
COPY migrations/ migrations/

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
