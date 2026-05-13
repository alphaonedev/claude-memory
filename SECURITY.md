# Security Policy

ai-memory is a substrate for persistent AI/agent memory. Customers and AgenticMem prospects deploy it in regulated environments. Security disclosures are taken seriously.

## Supported versions

| Version    | Supported |
|------------|-----------|
| v0.7.x     | ✅ Active  |
| v0.6.4     | ✅ Active (LTS through v1.0 ship) |
| v0.6.3.1   | ⚠️  Security fixes only |
| v0.6.3 and earlier | ❌ End of life |

When v1.0 ships (Q2 2027), only the two most recent minor versions receive security fixes.

## Reporting a vulnerability

**Do NOT open a public GitHub issue for a vulnerability report.** Report privately via one of:

1. **GitHub Security Advisory** (preferred):
   [github.com/alphaonedev/ai-memory-mcp/security/advisories/new](https://github.com/alphaonedev/ai-memory-mcp/security/advisories/new)
2. **Email**: `security@alpha-one.mobi`
   - GPG key fingerprint: published at [alpha-one.mobi/.well-known/security.asc](https://alpha-one.mobi/.well-known/security.asc) (when available)
   - Otherwise email plaintext with subject line `[ai-memory security]`

Include:
- Affected version (output of `ai-memory --version`)
- Reproduction steps or proof-of-concept
- Impact assessment (data exposure, denial of service, integrity compromise, etc.)
- Suggested severity (see rubric below)
- Whether you intend to disclose publicly and on what timeline

## Response SLA

| Step | Target |
|------|--------|
| Acknowledge receipt | ≤ 48 hours |
| Initial severity assessment | ≤ 5 business days |
| Coordinated fix in code | severity-dependent (see rubric) |
| Public disclosure + CVE | ≤ 90 days from acknowledgment (coordinated) |

If 48-hour acknowledgment is missed, escalate by replying to your original report — we monitor that thread.

## Severity rubric + fix windows

| Severity | Definition | Fix window |
|---|---|---|
| **CRITICAL** | Remote code execution, audit-chain forgery, unauthenticated data exfiltration, cross-organization federation bypass | ≤ 7 days |
| **HIGH** | Auth bypass, signature verification bypass, substrate boundary bypass (§16 violations) | ≤ 30 days |
| **MEDIUM** | Information disclosure with limited blast radius, denial-of-service requiring authenticated access | ≤ 60 days |
| **LOW** | Style, code-quality, hardening opportunities with no exploitable impact | next release |

Severity is finalized by AlphaOne after triage; reporters may appeal via the security email thread.

## Disclosure timeline

1. **T-0**: vulnerability reported privately
2. **T+48h**: acknowledgment
3. **T+5d**: severity assessment + fix-eta
4. **T+fix-window**: coordinated patch released (versions per support table)
5. **T+90d max**: public disclosure with CVE assignment

If AlphaOne cannot ship a fix within the window, reporters may publicly disclose at T+90d with prior written notice. We will coordinate on the disclosure date and assist with CVE assignment.

## Out of scope

- Vulnerabilities in dependencies (file with upstream; we will coordinate on patch release once upstream fixes)
- Theoretical attacks requiring physical access or pre-existing root access
- Self-DoS via misconfiguration (operator-level error)
- Findings on releases past their EOL (see support table)

## Hall of fame

Reporters of CRITICAL or HIGH severity vulnerabilities, with their consent, are recognized in:
- The relevant CVE advisory
- The release notes for the fix release
- [`docs/security/hall-of-fame.md`](docs/security/hall-of-fame.md) (when populated)

No monetary bounty at present; recognition only.

## Audit attestation

ai-memory ships substrate-attested forensic surfaces:
- Ed25519-signed `memory_links.signature` column on every link write (G12 closure, v0.7.0)
- Hash-chained `signed_events` row-level append-only audit table
- `audit.rs` JSONL emitter with monotonic sequence across daemon restart (F2 closure, Round-2)
- `ai-memory verify-reflection-chain <id>` — procurement-grade evidence packet generator (v0.7.0 L1-3)
- `ai-memory export-forensic-bundle --memory-id <id>` — tamper-detection bundle (v0.7.0 L2-5)

Vulnerability reports involving the audit chain are CRITICAL severity by default.

The v1.0 release (Q2 2027) will be audited by a named third-party firm. Audit firm selection criteria and dispute-resolution process are documented in [`ROADMAP2.md`](ROADMAP2.md) §7.7.

## Cryptographic implementations

ai-memory uses:
- **Ed25519** for agent identity + signature verification (via the `ed25519-dalek` crate)
- **SHA-256** for payload hash chains (via the `sha2` crate)
- **TLS 1.3** for federation transport (rustls)
- **HMAC-SHA256** for subscription + approval API auth

Cryptographic protocol issues are HIGH severity at minimum. Implementation issues (timing leaks, side-channel, weak randomness) are also HIGH or CRITICAL severity.

## OSS commitment

Per [`ROADMAP2.md`](ROADMAP2.md) §15: ai-memory is Apache 2.0 forever. Security fixes ship under the same license; no commercial-only patches. AgenticMem's commercial offerings build on the OSS substrate but do not paywall security fixes.

---

Last updated: 2026-05-13.
