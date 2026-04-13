# Contributing to ai-memory-mcp

Thank you for considering contributing to ai-memory-mcp. This document outlines the process for contributing to this project.

## Reporting Bugs

Open an issue at [GitHub Issues](https://github.com/alphaonedev/ai-memory-mcp/issues) with:

- A clear, descriptive title
- Steps to reproduce the problem
- Expected vs actual behavior
- Your environment (OS, Rust version, build configuration)
- Relevant logs or error output

## Suggesting Features

Open a feature request at [GitHub Issues](https://github.com/alphaonedev/ai-memory-mcp/issues) with:

- A description of the problem the feature would solve
- Your proposed solution
- Any alternatives you have considered

## Development Setup

### Prerequisites

- **Rust 1.75+** (install via [rustup](https://rustup.rs/))
- **C compiler** (gcc, clang, or MSVC)
- Git

### Getting Started

```bash
git clone https://github.com/alphaonedev/ai-memory-mcp.git
cd ai-memory-mcp
cargo build
cargo test
```

## Code Style

- Run `cargo fmt` before committing. All code must be formatted with rustfmt.
- Run `cargo clippy` and resolve all warnings. CI will reject code with clippy warnings.
- Follow standard Rust naming conventions and idioms.

## Testing Requirements

- All existing tests must pass (`cargo test`).
- New code must include tests. Bug fixes should include a regression test.
- If you add a new MCP tool, HTTP endpoint, or CLI command, include integration tests covering the primary usage path.

## Pull Request Process

1. Fork the repository.
2. Create a feature branch from `main` (`git checkout -b feature/my-change`).
3. Make your changes, following the code style and testing guidelines above.
4. Ensure `cargo fmt`, `cargo clippy`, and `cargo test` all pass.
5. Push your branch and open a pull request against `main`.
6. Fill out the PR description with what changed and why.
7. Address any review feedback.

See the 8-step feature checklist in [DEVELOPER_GUIDE.md](docs/DEVELOPER_GUIDE.md) for end-to-end guidance on adding new features.

PRs require at least one approval before merging.

## Commit Message Conventions

Use the following format:

```
<type>: <short summary>

<optional body explaining the change in more detail>
```

Types: `feat`, `fix`, `docs`, `style`, `refactor`, `test`, `chore`, `perf`.

Examples:

- `feat: add batch memory import from JSONL`
- `fix: prevent duplicate entries during sync`
- `docs: update CLI usage examples`

## Release Process

1. Version bumps are coordinated by maintainers.
2. A changelog entry is added for every release (see `CHANGELOG.md`).
3. Releases are tagged as `vX.Y.Z` and published from the `main` branch.
4. Crate publishing and binary builds are handled by CI.

## Contributor License Agreement (CLA)

All contributors must agree to the project's [Contributor License Agreement](CLA.md) before their contributions can be accepted. The CLA ensures that you grant AlphaOne LLC the necessary rights to use your contributions under the Apache License, Version 2.0, while you retain ownership of your work.

- **Individual contributors:** Include your signed CLA information (as described in [CLA.md](CLA.md)) in your first pull request.
- **Corporate contributors:** Have your authorized representative submit the Entity CLA before any employees or contractors submit pull requests.

If you have questions about the CLA, open an issue or contact the project maintainers.

## License

By contributing, you agree that your contributions will be licensed under the [Apache License, Version 2.0](LICENSE).
