Name:           ai-memory
Version:        0.5.1
Release:        1%{?dist}
Summary:        AI-agnostic persistent memory system — MCP server, HTTP API, and CLI

License:        MIT
URL:            https://github.com/alphaonedev/ai-memory-mcp
Source0:        https://github.com/alphaonedev/ai-memory-mcp/releases/download/v%{version}/ai-memory-%{_target_platform}.tar.gz

# Pre-built binary — no build dependencies needed
AutoReqProv:    no

%description
Persistent memory for any AI assistant. Zero token cost until recall. 17 MCP
tools, 20 HTTP endpoints, 25 CLI commands. Hybrid recall with FTS5 keyword and
semantic embedding search, TOON compact format (79%% smaller than JSON). 4
feature tiers from keyword to autonomous with local LLMs via Ollama. Works with
Claude, ChatGPT, Grok, Cursor, Windsurf, Continue.dev, OpenClaw, Llama, and
any MCP client. 97.8%% recall accuracy on ICLR 2025 LongMemEval benchmark.

%install
mkdir -p %{buildroot}%{_bindir}
install -m 0755 %{_sourcedir}/ai-memory %{buildroot}%{_bindir}/ai-memory

%files
%{_bindir}/ai-memory

%changelog
* Tue Apr 08 2026 AlphaOne LLC <alphaonedev@users.noreply.github.com> - 0.5.1-1
- Docker image on GHCR, auto-built on tag push
- Official MCP Registry published
- Dockerfile modernized: Rust 1.86, build-essential, MCP registry label
- ARM64 Linux packages fixed
- Documentation red team audit
- OpenClaw added as 9th supported AI platform
- New files: CONTRIBUTING.md, CHANGELOG.md, CODE_OF_CONDUCT.md

* Tue Apr 08 2026 AlphaOne LLC <alphaonedev@users.noreply.github.com> - 0.5.0-1
- Initial release
- MCP server with 17 tools
- HTTP API with 20 endpoints
- CLI with 25 commands
- 4 feature tiers: keyword, semantic, smart, autonomous
- Hybrid recall with FTS5 + semantic embeddings
- TOON compact format
- 97.8%% R@5 on ICLR 2025 LongMemEval benchmark
