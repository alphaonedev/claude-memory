---
sidebar_position: 7
title: Building from source
description: Cargo build, test, and quality gates.
---

# Building from source

## Requirements

- Rust **1.87+**
- C++ toolchain (for `candle-core`)
- Optional: Ollama (for smart/autonomous tier development)

## Build

```bash
git clone https://github.com/alphaonedev/ai-memory-mcp
cd ai-memory-mcp

cargo build              # debug
cargo build --release    # release (thin LTO, stripped)
```

## Quality gates

All four MUST pass before PR submission:

```bash
cargo fmt --check
cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic
AI_MEMORY_NO_CONFIG=1 cargo test
cargo audit
```

`AI_MEMORY_NO_CONFIG=1` prevents loading user config which may trigger embedder/LLM init during tests.

## Run a single test

```bash
AI_MEMORY_NO_CONFIG=1 cargo test test_sync_daemon_mesh_propagates_memory_between_peers
```

## Benchmarks

```bash
cargo bench --bench recall
```

## Docker build

```bash
docker build -t ai-memory:dev .
```

## CI

Every PR runs the four gates on Linux + macOS + Windows. Tag-pushes also build:

- 5-platform binaries (Linux x86_64/arm64, macOS x86_64/arm64, Windows)
- Docker image to GHCR
- Homebrew formula update (auto SHA256)
- crates.io publish
- Ubuntu PPA + Fedora COPR triggers
