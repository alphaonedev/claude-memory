# ai-memory Engineering Standards

> Authoritative reference for all development, testing, security, and release processes.
> Maintained by AlphaOne LLC. All contributors and AI agents must follow these standards.
> In case of conflict with CONTRIBUTING.md, this document takes precedence.

---

## 1. Development Standards

### 1.1 Repository

| Repo | Purpose | Branches |
|------|---------|----------|
| `alphaonedev/ai-memory-mcp` | Single source of truth | `main` (production), `develop` (active development) |

There is no separate development repo. `ai-memory-mcp-dev` is archived.

### 1.2 Branch Strategy

- `main` — production releases, protected branch, requires owner approval
- `develop` — active development, all PRs target this branch
- Feature/fix branches created from `develop` (e.g., `feature/batch-import`, `fix/ttl-overflow`)
- Maintainers merge `develop` into `main` when cutting a release

### 1.3 Branch Protection (main)

| Rule | Enforcement |
|------|-------------|
| Direct pushes | Blocked — PRs required |
| Approving reviews | 1 required from `@alphaonedev` (CODEOWNERS) |
| Stale review dismissal | Enabled — new pushes invalidate approvals |
| CI status checks | `Check (ubuntu-latest)` + `Check (macos-latest)` must pass |
| Branch up-to-date | Required before merge |
| Force pushes | Blocked |
| Branch deletion | Blocked |

No code reaches `main` without the project owner's explicit approval.

PRs to `develop` do not require owner approval but must pass all CI checks (fmt, clippy pedantic, tests).

### 1.4 Code Style

- **Rust 1.75+** minimum supported version.
- **`cargo fmt`** is mandatory. CI enforces via `cargo fmt --check`. Always run before committing.
- **`cargo clippy`** with pedantic:
  ```
  cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic
  ```
  Zero warnings. If a pedantic lint requires `#[allow(clippy::...)]`, it must be justified in the PR description.
- **SPDX headers** required on all source files:
  ```rust
  // Copyright 2026 AlphaOne LLC
  // SPDX-License-Identifier: Apache-2.0
  ```
- No new production `unwrap()` calls. Use `?`, `.map_err()`, `unwrap_or_default()`, or match expressions.
- All SQL queries must use parameterized queries (`params![]`). No string interpolation in SQL.
- FTS5 input sanitized via `sanitize_fts5_query()`.

### 1.5 Commit Messages

Format:
```
<type>: <short summary>

<optional body explaining the change in more detail>
```

Types: `feat`, `fix`, `docs`, `style`, `refactor`, `test`, `chore`, `perf`.

- Reference issues: `Closes #52`
- AI-generated commits include: `Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>`

### 1.6 PR Requirements

Every PR must pass these gates before merge:

| Gate | Requirement |
|------|-------------|
| `cargo fmt --check` | Clean |
| `cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic` | Zero warnings |
| `AI_MEMORY_NO_CONFIG=1 cargo test` | All passing, 0 failures |
| `cargo audit` | 0 vulnerabilities (warnings acceptable if transitive) |
| Functional test | All categories pass |
| Security review | 0 ship-blocking findings |
| Documentation sync | Test counts and tool counts updated in all docs |
| CLA | Signed (see [CLA.md](../CLA.md)) |

---

## 2. Test Standards

### 2.1 cargo test

- **Environment:** `AI_MEMORY_NO_CONFIG=1` to prevent config interference
- **Platforms:** Must pass on both `ubuntu-latest` and `macos-latest`
- **Result:** 0 failures required

```bash
AI_MEMORY_NO_CONFIG=1 cargo test
```

### 2.2 Full Pre-PR Verification

Contributors must run all three before submitting:

```bash
cargo fmt --check
cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic
AI_MEMORY_NO_CONFIG=1 cargo test
```

### 2.3 Full Spectrum Functional Test

Run against the compiled binary via CLI. Covers all 26 commands with edge cases. When adding a new command, add a corresponding row to this table.

| Category | Tests | Scope |
|----------|:-----:|-------|
| Version | 2 | `--version`, `-V` |
| Store | 7 | Multiple tiers, namespaces, tags, priorities, content flag |
| Get | 2 | By ID, non-existent UUID |
| Update | 6 | Content, priority, tags, title, tier downgrade, expires_at |
| List | 5 | All, namespace, tier, limit, tags filters |
| Recall | 5 | Keyword, namespace, limit, empty, cross-namespace |
| Search | 3 | Keyword, namespace, special characters |
| Promote | 3 | Mid-to-long, expires_at cleared, already-long |
| Link | 3 | related_to, derived_from, get links |
| Namespaces | 1 | List with counts |
| Stats | 3 | Total count, db_size, links count |
| Consolidate | 2 | Merge + verify, originals deleted |
| Resolve | 1 | Contradiction resolution |
| Forget | 1 | Bulk delete by namespace |
| GC | 2 | Removes expired, preserves long-tier |
| Export/Import | 2 | JSON roundtrip, count match |
| Delete | 1 | Hard delete + not-found verification |
| Edge cases | 7+ | Unicode, FTS injection, priority/confidence bounds, large content |

### 2.4 Memory & TTL Test Protocol

15-20 tests covering the memory lifecycle:

| Area | Tests |
|------|-------|
| Tier TTL assignment | Short ~6h, mid ~7d, long=none |
| Custom overrides | `--ttl-secs`, `--expires-at` |
| GC behavior | Removes expired, preserves non-expired, archives before deletion |
| TTL refresh on recall | Short: rolling +1h window. Mid: +1d extension |
| Auto-promotion | Mid-to-long at 5+ accesses, expires_at cleared |
| Priority reinforcement | +1 every 10 accesses (max 10) |
| Tier protection | Downgrade silently blocked, upgrades allowed |
| Upsert semantics | Duplicate title preserves higher tier |

### 2.5 Pass/Fail Criteria

- **Pass:** Test produces expected result.
- **Acceptable known behavior:** TTL refresh on recall sets `expires_at = now + extend_secs` (rolling window). When recalled shortly after creation, this can be earlier than the initial TTL. This is by-design, not a bug.
- **Ship-blocking fail:** Any functional test failure not documented as by-design.

### 2.6 Test Count Documentation Locations

When test counts change, update ALL of these:

| File | Instances |
|------|:---------:|
| `README.md` | 1 |
| `CLAUDE.md` | 1 |
| `docs/ADMIN_GUIDE.md` | 2 |
| `docs/DEVELOPER_GUIDE.md` | 1 |
| `docs/index.html` | 2 |

MCP tool count in:

| File | Instances |
|------|:---------:|
| `README.md` | 1 |
| `docs/DEVELOPER_GUIDE.md` | 2 |
| `docs/USER_GUIDE.md` | 1 |

---

## 3. Security Review Standards

### 3.1 Automated (CI)

- **`cargo audit`** runs on every push to `main` and on every PR. Scans `Cargo.lock` for known vulnerabilities.
- 0 vulnerabilities required. Warnings for transitive unmaintained crates are acceptable if not exploitable.

### 3.2 Manual Security Review Checklist

Every patch/release must be reviewed against these 10 areas:

| # | Area | What to Check |
|---|------|---------------|
| 1 | SQL injection | All queries use `params![]` parameterization |
| 2 | `validate_id()` coverage | Every MCP handler and CLI command accepting an ID calls `validate_id()` at entry |
| 3 | Command injection | No `std::process::Command` in production code |
| 4 | Path traversal | File paths use `PathBuf::join()` safely |
| 5 | `unwrap()` calls | Zero new production `unwrap()` — use `?` or `.map_err()` |
| 6 | Error message leakage | Only expose tier names and IDs, not internal state or stack traces |
| 7 | Race conditions | Check embedding regen, HNSW updates, consolidate paths |
| 8 | Auth/authz | HTTP API requires `--auth-token` for non-localhost access |
| 9 | Data in logs | Only UUIDs, error messages, GC counts; no user content |
| 10 | CORS | Strict hostname check with required separator |

### 3.3 Severity Classification

| Severity | Definition | Action |
|----------|-----------|--------|
| **Critical** | Data loss, crash, injection, remote exploit | Ship-blocking. Fix before release. |
| **High** | Exploitable with local access, data exposure | Ship-blocking. Fix before release. |
| **Medium** | Defense-in-depth gap with existing fallback | Should fix. May ship with documented timeline. |
| **Low** | Cosmetic, non-exploitable edge case | Acceptable. Fix if convenient. |

---

## 4. Release Standards

### 4.1 Version Numbering

- Format: `MAJOR.MINOR.PATCH-patch.N` in `Cargo.toml` (e.g., `0.5.4-patch.4`)
- Git tag format: `vMAJOR.MINOR.PATCH.N` (e.g., `v0.5.4.4`)
- Binary reports: `ai-memory MAJOR.MINOR.PATCH-patch.N`

### 4.2 Release Process

1. Merge `develop` into `main` via PR (owner approval required)
2. Ensure all gates pass locally:
   ```bash
   cargo fmt
   cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic
   AI_MEMORY_NO_CONFIG=1 cargo test
   cargo audit
   ```
3. Push tag `v{VERSION}` to `main`
4. CI pipeline triggers automatically:
   - Check phase (fmt, clippy pedantic, test, audit, build) on ubuntu + macos
   - Release phase: 5 platform binaries + `.deb`/`.rpm` packages
   - Docker: push to GHCR (`ghcr.io/alphaonedev/ai-memory:VERSION` + `:latest`)
   - PPA: Ubuntu PPA upload (`ppa:jbridger2021/ppa`)
   - COPR: Fedora COPR upload (`alpha-one-ai/ai-memory`)
5. Verify: `gh release view v{VERSION} --repo alphaonedev/ai-memory-mcp`
6. Update Homebrew tap (manual): `alphaonedev/homebrew-tap` — download platform tarballs, compute SHA256 hashes, update `Formula/ai-memory.rb`

### 4.3 Documentation Sync

Before tagging a release:

- [ ] `CHANGELOG.md` has entry for this version with all fixes
- [ ] Test counts updated in 7 locations (see Section 2.6)
- [ ] MCP tool counts updated in 4 locations (see Section 2.6)
- [ ] `Cargo.toml` version matches the tag

### 4.4 Post-Release

- Install release binary on node from GitHub Releases (not `cargo install`)
- Verify: `ai-memory --version`
- Store release memory in ai-memory for session recall

---

## 5. Legal & Licensing

| Item | Status |
|------|--------|
| License | Apache License, Version 2.0 |
| Patent grant | Apache 2.0 Section 3 (contributor patent grant) |
| Patent retaliation | Apache 2.0 Section 3 (attacker loses license) |
| CLA | Required for all contributors ([CLA.md](../CLA.md)) |
| OIN membership | Active (AlphaOne LLC, 3,900+ member cross-license) |
| Trademark | ai-memory(TM) — USPTO Serial No. 99761257 (pending) |
| SPDX headers | Required on all source files: `// SPDX-License-Identifier: Apache-2.0` |
| NOTICE file | Required per Apache 2.0 Section 4(d) |

---

## 6. Key References

| Reference | Location |
|-----------|----------|
| CI/CD workflow | `.github/workflows/ci.yml` |
| Branch protection | GitHub repo settings + `.github/CODEOWNERS` |
| Contributing guide | `CONTRIBUTING.md` |
| CLA | `CLA.md` |
| CHANGELOG | `CHANGELOG.md` |
| Roadmap | `ROADMAP.md` |
| OIN agreement | `OIN_LICENSE_AGREEMENT.pdf` |
