# ai-memory Engineering Standards

> Authoritative reference for all development, testing, security, and release processes.
> Maintained by AlphaOne LLC. All contributors and AI agents must follow these standards.
> In case of conflict with CONTRIBUTING.md, this document takes precedence.
>
> **AI agents** must additionally follow [`AI_DEVELOPER_WORKFLOW.md`](AI_DEVELOPER_WORKFLOW.md)
> (operational steps) and [`AI_DEVELOPER_GOVERNANCE.md`](AI_DEVELOPER_GOVERNANCE.md)
> (policy boundaries). The Governance standard takes precedence over this document
> only on matters of AI participation; this document remains authoritative for
> code, test, security, and release.

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

- **Rust 1.87+** minimum supported version (MSRV). Required for `is_multiple_of()` stabilization.
- **`cargo fmt`** is mandatory. CI enforces via `cargo fmt --check`. Always run before committing.
- **`cargo clippy`** with pedantic:
  ```
  cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic
  ```
  Zero warnings. If a pedantic lint requires `#[allow(clippy::...)]`, it must be justified in the PR description.
- **SPDX headers** required on all source files. Use the year of file creation:
  ```rust
  // Copyright <YEAR> AlphaOne LLC
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
| Functional test | All categories pass (maintainer performs during review) |
| Security review | 0 ship-blocking findings (maintainer performs during review) |
| Documentation sync | Test counts and tool counts updated in all docs |
| CLA | Signed (see [CLA.md](../CLA.md)) |

Contributors are responsible for the first four gates. Maintainers perform the functional test, security review, and documentation sync verification during PR review.

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

Contributors must run all four before submitting:

```bash
cargo fmt --check
cargo clippy -- -D warnings -D clippy::all -D clippy::pedantic
AI_MEMORY_NO_CONFIG=1 cargo test
cargo audit
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
- [ ] All new source files have SPDX headers
- [ ] Homebrew tap updated (`alphaonedev/homebrew-tap`) with new SHA256 hashes

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
| AI Developer Workflow | `docs/AI_DEVELOPER_WORKFLOW.md` |
| AI Developer Governance Standard | `docs/AI_DEVELOPER_GOVERNANCE.md` |
| CLA | `CLA.md` |
| LICENSE | `LICENSE` |
| NOTICE | `NOTICE` |
| CHANGELOG | `CHANGELOG.md` |
| Roadmap | `ROADMAP.md` |
| Install guide | `docs/INSTALL.md` |
| User guide | `docs/USER_GUIDE.md` |
| Developer guide | `docs/DEVELOPER_GUIDE.md` |
| Admin guide | `docs/ADMIN_GUIDE.md` |
| OIN agreement | `OIN_LICENSE_AGREEMENT.pdf` |
| Homebrew tap | `alphaonedev/homebrew-tap` (separate repo) |

---

## 7. Autonomous Campaign Workflow

When development is driven by an autonomous Claude Code campaign (the
`campaign` Python harness at
`alphaonedev/agentic-mem-labs/tools/campaign/`, Apache 2.0 © AlphaOne
LLC), every standard in §1–§6 still applies. The campaign agent runs
with `--dangerously-skip-permissions` and merge authority on a single
release branch only; the constraints below are baked into every
iteration's prompt and survive `bypassPermissions`.

### 7.1 Hard rules under campaign

| Rule | Enforcement |
|------|-------------|
| Push/merge into `main` or `develop` | Forbidden — agent uses a designated `release/vX.Y.Z` branch only |
| Push/merge into any other `release/*` branch | Forbidden |
| `git push --force` / `--force-with-lease` | Forbidden |
| `v*` tags | Forbidden — releases are the human's job |
| `gh release create/edit` | Forbidden |
| `gh workflow run` | Forbidden — no manual CI triggers from the agent |
| Edits to `.github/workflows/release*.yml` | Forbidden |
| `cargo publish` / `npm publish` | Forbidden |
| Edits to charter doc or any `strategy/`, `the-standard/`, `relocated-from-public*/` paths | Forbidden |
| `git config --global` / `~/.gitconfig` mutation | Forbidden |
| `gh auth` reconfigure / token rotation | Forbidden |
| Writes to `/etc`, `/usr`, `/System`, `/Library`, `~/.aws`, `~/.ssh`, `~/.config/gh`, `.credentials.json` | Forbidden |
| Rewriting or deleting past iteration audit reports on the `campaign-log` branch | Forbidden — append-only |

### 7.2 Per-iteration loop (8 steps)

The agent's prompt requires every iteration to execute, in order:

1. **Load memories** — recall 3 targeted queries + list recent in the
   campaign's namespace (see §7.4)
2. **Read** charter + last iteration report
3. **Inspect** repo state (`git fetch` / `status` / `log`)
4. **Pick** exactly one unblocked charter item
5. **Implement** on `campaign/<slug>` feature branch → cargo fmt →
   `cargo clippy --all-targets -- -D warnings` → `cargo test --all` →
   signed commit → push → `gh pr create` → `gh pr merge --squash --delete-branch`
6. **Record** decisions to memory throughout (not only at the end)
7. **Write** iteration report markdown using the fixed template
8. **Commit + push** the report on the dedicated `campaign-log/vX.Y.Z`
   branch of the charter repo (append-only audit)

### 7.3 Quality gates (identical to non-campaign work)

- Tests must pass before push (`AI_MEMORY_NO_CONFIG=1 cargo test --all`)
- `cargo fmt --check` clean
- `cargo clippy --all-targets -- -D warnings -D clippy::all -D clippy::pedantic` clean
- No `unwrap()` in non-test code without an explicit invariant comment
- Conventional commit messages
- One logical change per dev-repo commit
- All commits SSH-signed (`commit.gpgsign=true`, ed25519)
- SPDX headers on new source files

### 7.4 Memory namespace convention (load-bearing)

**Each campaign owns one ai-memory namespace named after the campaign**
(e.g. `campaign-v063` for the v0.6.3 grand-slam campaign). The
namespace is the campaign's **complete operating context** AND its
**historical audit artifact**:

| Memory category | Tier | Tags | Lifetime |
|---|---|---|---|
| Campaign overview / scope / hard rules | `long` | `campaign,scope,charter` | Permanent |
| Approvals — what is allowed/forbidden | `long` | `campaign,approvals` | Permanent |
| Code quality standards | `long` | `campaign,quality,standards` | Permanent |
| Engineering Standards alignment | `long` | `campaign,standards,alignment` | Permanent |
| Open-issues snapshot at campaign start | `long` | `campaign,issues,snapshot` | Permanent (historical) |
| Per-iteration summary (one entry per iter) | `long` | `campaign,iteration` | Permanent (audit) |
| Decisions / rationale | `long` | `campaign,decision` | Permanent |
| Blockers + how resolved | `long` | `campaign,blocker` | Permanent |
| Out-of-charter ideas (deferred) | `mid` | `future,deferred` | 7 days unless promoted |

**Rule:** every campaign memory is namespace-scoped to that campaign's
name. After the campaign ends, the namespace is the canonical historical
reference for "what got built, why, and in what order." Future campaigns
do not share namespaces.

Discoverable via:

```bash
ai-memory --db ~/.claude/ai-memory.db list   --namespace campaign-v063
ai-memory --db ~/.claude/ai-memory.db recall --namespace campaign-v063 "what task was last shipped"
```

### 7.5 Living snapshot — open issues & PRs (charter context)

The campaign agent reads this section at iter 1 of every new campaign
to seed scope. Maintainers refresh it when a campaign is created or
when issue/PR triage is done. **Most recent snapshot — 2026-04-25:**

```
Open issues (alphaonedev/ai-memory-mcp)
  #355  chore(deps): rustls-pemfile 2.2.0 unmaintained (RUSTSEC-2025-0134) — transitive via axum-server [low]
  #331  v0.7 red-team P3 polish punchlist (rollup) [enhancement,low]
  #318  MCP stdio tool dispatch writes bypass federation fanout coordinator [bug,high]
  #311  feat(v0.6.0.1): targeted memory share — CLI + MCP tool for point-to-point sync by ID/namespace/last-N [enhancement]
  #239  [P2] /sync/since allows full DB dump for any valid mTLS peer (red-team #230) [docs,med,security]
  #238  [P2] Body-claimed sender_agent_id not attested to mTLS cert (red-team #230) [enh,med,security]
  #228  v0.8: end-to-end memory encryption (X25519 + ChaCha20-Poly1305) — Layer 3 peer-mesh crypto [enhancement]
  #224  Phase 3: Memory Sharing & Sync — design + decomposition (v0.8.0; foundation lands v0.6.0 GA) [enhancement]

Open PRs (against develop)
  #330  feat(msrv,ppa): Ubuntu 26.04 Resolute Raccoon as sole PPA target; rustc 1.91 MSRV
  #285  feat: Layer 2b attested sender_agent_id primitives (v0.7)
  #253  docs: scaffold Docusaurus documentation site (#252)
```

The campaign's mandate is the charter, not the open-issues backlog —
but the agent must consult this list when picking tasks to avoid
duplicating in-flight work.

### 7.6 Operator surface (campaign harness CLI)

```
campaign preflight        # 8 health checks
campaign install          # 24x7 launchd daemon (renders plist from live Config)
campaign install --force  # bootout existing + re-install (use after env changes)
campaign service-status   # human-readable status with drift detection
campaign watch            # live TUI of newest iter log (auto-rotates)
campaign status           # pid / branch / iter count
campaign stop             # kill-switch + drain + SIGTERM/SIGKILL
campaign uninstall        # bootout (optionally --remove-plist)
```

Pause without uninstalling: `touch <state-dir>/kill-switch`. Resume:
`rm <state-dir>/kill-switch`. The `KeepAlive PathState` watcher in the
LaunchAgent plist toggles the daemon's run-state automatically.

Full operator guide:
[`alphaonedev/agentic-mem-labs/tools/campaign/README.md`](https://github.com/alphaonedev/agentic-mem-labs/blob/main/tools/campaign/README.md)
(Apache 2.0 © AlphaOne LLC).
