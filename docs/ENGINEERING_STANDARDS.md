# ai-memory Engineering Standards

> Established through Issues #1, #49, #51, #52, #55 and 55+ closed issues across dev and prod repos.
> This document is the authoritative reference for all development, testing, security, and release processes.

---

## 1. Development Standards

### 1.1 Repository Structure

| Repo | Purpose | Default Branch |
|------|---------|----------------|
| `alphaonedev/ai-memory-mcp-dev` | Development | `develop` |
| `alphaonedev/ai-memory-mcp` | Production | `main` |

### 1.2 Branch Strategy

- Feature/patch branches created from `develop` (e.g., `patch/v0.5.4.2`, `fix/phase0-gap-tests`)
- PRs merge to `develop` in dev repo
- Identical diff applied to prod `main` via separate PR
- Dev-only files (e.g., `ROADMAP.md`) are gitignored and never pushed to prod

### 1.3 Dev/Prod Sync Rules

- **Dev must match prod before any new patch work begins.** If codebases have diverged, overwrite dev with prod main first, then apply changes.
- Documentation updates must be applied to both repos simultaneously.
- Test counts and tool counts in docs must match across both repos.

*Reference: Session 2026-04-12 -- 6 core files had diverged (2000+ lines diff). Resolved by full overwrite of dev with prod (commit `fbc7d69`).*

### 1.4 Code Style

- **`cargo fmt`** is mandatory. CI enforces via `cargo fmt --check`. Always run before committing.
- **`cargo clippy`** with these flags:
  ```
  -D warnings -A dead_code -A clippy::too_many_arguments -A clippy::manual_map -A clippy::manual_is_multiple_of
  ```
- No new production `unwrap()` calls. Use `?`, `.map_err()`, `unwrap_or_default()`, or match expressions.
- All SQL queries must use parameterized queries (`params![]`). No string interpolation in SQL.
- FTS5 input sanitized via `sanitize_fts5_query()`.

### 1.5 Commit Messages

- Descriptive of what was changed and why
- Prefix with type: `fix:`, `feat:`, `chore:`, `style:`, `docs:`
- Reference issues: `Closes #52`
- AI-generated commits include: `Co-Authored-By: Claude Opus 4.6 (1M context) <noreply@anthropic.com>`

### 1.6 PR Requirements

Every PR must pass these gates before merge:

| Gate | Requirement |
|------|-------------|
| `cargo fmt --check` | Clean |
| `cargo clippy` | 0 new warnings |
| `cargo test` | All passing, 0 failures |
| `cargo audit` | 0 vulnerabilities (warnings acceptable if transitive) |
| Functional test | All categories pass |
| Security review | 0 ship-blocking findings |
| Documentation sync | Test counts and tool counts updated in all docs |

---

## 2. Test Standards

### 2.1 cargo test

- **Environment:** `AI_MEMORY_NO_CONFIG=1` to prevent config interference
- **Platforms:** Must pass on both `ubuntu-latest` and `macos-latest`
- **Result:** 0 failures required
- **Baseline (v0.5.4.2):** 185 tests (139 unit + 46 integration)

### 2.2 Full Spectrum Functional Test

Run against the compiled binary via CLI. Covers all 26 commands with edge cases.

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
| Patch-specific | varies | Per-patch feature verification |
| Edge cases | 7+ | Unicode, FTS injection, priority/confidence bounds, large content |

*Reference: Issue #51 (functional test protocol), Issue #52 Comment 2 (70-test suite).*

### 2.3 Memory & TTL Test Protocol

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

### 2.4 Pass/Fail Criteria

- **Pass:** Test produces expected result.
- **Acceptable known behavior:** TTL refresh on recall sets `expires_at = now + extend_secs` (rolling window). When recalled shortly after creation, this can be earlier than the initial TTL. This is by-design, not a bug. Document with "T9 Note" if encountered.
- **Ship-blocking fail:** Any functional test failure not documented as by-design.

### 2.5 Test Count Documentation Locations

When test counts change, update ALL of these:

| File | Instances |
|------|:---------:|
| `README.md` | 1 |
| `CLAUDE.md` | 1 |
| `docs/ADMIN_GUIDE.md` | 2 |
| `docs/DEVELOPER_GUIDE.md` | 1 |
| `docs/index.html` | 2 |

MCP tool count (currently 23) in:

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
| 8 | Auth/authz | Local service by design; HTTP API has no auth (documented accepted risk) |
| 9 | Data in logs | Only UUIDs, error messages, GC counts; no user content |
| 10 | CORS | Strict hostname check with required separator |

### 3.3 Severity Classification

| Severity | Definition | Action |
|----------|-----------|--------|
| **Critical** | Data loss, crash, injection, remote exploit | Ship-blocking. Fix before release. |
| **High** | Exploitable with local access, data exposure | Ship-blocking. Fix before release. |
| **Medium** | Defense-in-depth gap with existing fallback | Should fix. May ship with documented timeline. |
| **Low** | Cosmetic, non-exploitable edge case | Acceptable. Fix if convenient. |

*Reference: Issue #52 Comment 2 (10-area security review), Issue #1 Comment 13 (9-finding codebase audit).*

---

## 4. Release Standards

### 4.1 Version Numbering

- Format: `MAJOR.MINOR.PATCH-patch.N` in `Cargo.toml` (e.g., `0.5.4-patch.2`)
- Git tag format: `vMAJOR.MINOR.PATCH.N` (e.g., `v0.5.4.2`)
- Binary reports: `ai-memory MAJOR.MINOR.PATCH-patch.N`

### 4.2 Release Process

1. Ensure all gates pass locally:
   - `cargo fmt` (run it, don't just check)
   - `cargo clippy -- -D warnings -A dead_code -A clippy::too_many_arguments -A clippy::manual_map -A clippy::manual_is_multiple_of`
   - `cargo test` with `AI_MEMORY_NO_CONFIG=1`
   - `cargo audit`
2. Commit, push to prod `main`
3. Push tag `v{VERSION}` to prod `main`
4. CI pipeline triggers automatically:
   - Check phase (fmt, clippy, test, audit, build) on ubuntu + macos
   - Release phase: 5 platform binaries + `.deb`/`.rpm` packages
   - Docker: push to GHCR (`ghcr.io/alphaonedev/ai-memory:VERSION` + `:latest`)
   - PPA: Ubuntu PPA upload
   - COPR: Fedora COPR upload
5. Verify: `gh release view v{VERSION} --repo alphaonedev/ai-memory-mcp`
6. Install on node: download binary from GitHub Releases to `/usr/local/bin/ai-memory`

### 4.3 Documentation Sync

Before tagging a release:

- [ ] `CHANGELOG.md` has entry for this version with all fixes
- [ ] Test counts updated in 7 locations (see Section 2.5)
- [ ] MCP tool counts updated in 4 locations (see Section 2.5)
- [ ] Both dev and prod repos have identical source files
- [ ] `Cargo.toml` version matches the tag

### 4.4 Post-Release

- Install release binary on node from GitHub Releases (not `cargo install`)
- Verify: `ai-memory --version`
- Store release memory in ai-memory for session recall

---

## 5. Key References

| Reference | Location |
|-----------|----------|
| CI/CD workflow | `.github/workflows/ci.yml` |
| Phase 0 tracker | [Issue #1](https://github.com/alphaonedev/ai-memory-mcp-dev/issues/1) |
| Update facility audit | [Issue #49](https://github.com/alphaonedev/ai-memory-mcp-dev/issues/49) |
| Functional test protocol | [Issue #51](https://github.com/alphaonedev/ai-memory-mcp-dev/issues/51) |
| Patch 2 close-out & test standard | [Issue #52](https://github.com/alphaonedev/ai-memory-mcp-dev/issues/52) |
| cargo-audit CI | [Issue #55](https://github.com/alphaonedev/ai-memory-mcp-dev/issues/55) |
| CHANGELOG | `CHANGELOG.md` in both repos |
