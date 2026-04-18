---
sidebar_position: 1
title: Install
description: 8 install methods across macOS, Linux, and Windows.
---

# Install ai-memory

ai-memory ships as a single static binary across 8 distribution channels. Pick whichever you already use.

## macOS / Linux (recommended)

### Homebrew
```bash
brew install alphaonedev/tap/ai-memory
```

### One-line install script
```bash
curl -fsSL https://raw.githubusercontent.com/alphaonedev/ai-memory-mcp/main/install.sh | sh
```

## Linux package managers

### Ubuntu / Debian (APT)
```bash
sudo add-apt-repository ppa:jbridger2021/ppa
sudo apt update
sudo apt install ai-memory
```

### Fedora / RHEL (DNF)
```bash
sudo dnf copr enable alpha-one-ai/ai-memory
sudo dnf install ai-memory
```

## Windows

```powershell
irm https://raw.githubusercontent.com/alphaonedev/ai-memory-mcp/main/install.ps1 | iex
```

## Cargo (build from source)

```bash
cargo install ai-memory
# Or with cargo-binstall (no compile):
cargo binstall ai-memory
```

Requires Rust 1.87+ and a C++ toolchain.

## Docker / GHCR

```bash
docker pull ghcr.io/alphaonedev/ai-memory-mcp:latest
docker run --rm -v ~/.local/share/ai-memory:/data \
  ghcr.io/alphaonedev/ai-memory-mcp:latest \
  ai-memory --db /data/memories.db serve --host 0.0.0.0
```

## GitHub Releases

Pre-built binaries (Linux x86_64/arm64, macOS x86_64/arm64, Windows), `.deb`, and `.rpm` packages live at:
[github.com/alphaonedev/ai-memory-mcp/releases](https://github.com/alphaonedev/ai-memory-mcp/releases)

## Verify the install

```bash
ai-memory --version
# ai-memory 0.6.0
```

## Next

→ [Quickstart](./quickstart) — store and recall your first memory in 60 seconds
→ [Tiers](./tiers) — pick the right feature tier for your use case
