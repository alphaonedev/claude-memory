# ai-memory Documentation

Navigation hub for the `docs/` directory. Every doc below is
authoritative for its topic; this page is just the map.

## Start here

- **[QUICKSTART.md](QUICKSTART.md)** — first memory stored + recalled
  in under 5 minutes (CLI, MCP, HTTP paths).
- **[GLOSSARY.md](GLOSSARY.md)** — every concept (agent, tier, scope,
  curator, quorum, SAL, …) with one-paragraph definitions and links.

## For end users

- **[USER_GUIDE.md](USER_GUIDE.md)** — MCP tool reference (every
  `memory_*` tool), agent identity, worked examples.
- **[CLI_REFERENCE.md](CLI_REFERENCE.md)** — every subcommand, flag,
  env var. Auto-synced to `src/main.rs` clap defs.
- **[API_REFERENCE.md](API_REFERENCE.md)** — every HTTP endpoint,
  payload, status code, `curl` example.
- **[INSTALL.md](INSTALL.md)** — install recipes per platform +
  every major MCP-capable IDE.
- **[TROUBLESHOOTING.md](TROUBLESHOOTING.md)** — common errors, root
  causes, fixes.

## For admins

- **[ADMIN_GUIDE.md](ADMIN_GUIDE.md)** — deployment, feature tiers,
  clustering, webhooks, governance, schema migration.
- **[SECURITY.md](SECURITY.md)** — threat model, API key, mTLS,
  SQLCipher at rest, attested identity, SSRF hardening.
- **[ARCHITECTURAL_LIMITS.md](ARCHITECTURAL_LIMITS.md)** — performance
  bounds and constraints under the current design.
- **[RUNBOOK-ollama-kv-tuning.md](RUNBOOK-ollama-kv-tuning.md)** —
  `OLLAMA_KV_CACHE_TYPE=q4_0` for 2–4× LLM memory reduction. Zero
  ai-memory code change.
- **[RUNBOOK-chaos-campaign.md](RUNBOOK-chaos-campaign.md)** —
  200-cycle-per-fault-class federation chaos procedure (requires
  real 3-host infra).
- **[RUNBOOK-curator-soak.md](RUNBOOK-curator-soak.md)** — 168-hour
  curator soak procedure against a production corpus. Defines
  reversal rate `R` as the honest autonomy metric.
- **[RUNBOOK-adapter-selection.md](RUNBOOK-adapter-selection.md)** —
  scoped design for the v0.7.1 `serve --store-url postgres://…`
  refactor. NOT shipping in v0.7-alpha.

## For developers

- **[DEVELOPER_GUIDE.md](DEVELOPER_GUIDE.md)** — architecture, module
  roles, recall pipeline, data model, environment variables.
- **[ENGINEERING_STANDARDS.md](ENGINEERING_STANDARDS.md)** — code,
  test, security, and release standards. The four gates every PR
  must pass.
- **[AI_DEVELOPER_WORKFLOW.md](AI_DEVELOPER_WORKFLOW.md)** — the
  eight-phase workflow AI agents must follow (recall → plan →
  branch → implement → gates → self-review → PR → handoff).
- **[AI_DEVELOPER_GOVERNANCE.md](AI_DEVELOPER_GOVERNANCE.md)** —
  authority classes, attribution rules, memory governance, hard
  prohibitions.
- **[CONTRIBUTING.md](../CONTRIBUTING.md)** (repo root) —
  contributor procedures, CLA.

## Design decisions

- **[ADR-0001-quorum-replication.md](ADR-0001-quorum-replication.md)** —
  W-of-N quorum write model + chaos-testing methodology.
- **[PHASE-1.md](PHASE-1.md)** — upcoming memory schema / hierarchy
  changes, governance roadmap.
- **[ROADMAP-ladybug.md](ROADMAP-ladybug.md)** — LadybugDB as a
  v0.7.1+ SAL adapter (deliberately not a 100% transition). Phased
  plan with a benchmark-gated promotion decision.

## SDKs

- **[sdk/typescript/README.md](../sdk/typescript/README.md)** —
  `@alphaone/ai-memory` sync client, all 25 methods + webhook verifier.
- **[sdk/python/README.md](../sdk/python/README.md)** — `ai-memory`
  package, sync + async clients, Pydantic v2 models.

## Release notes

- **[CHANGELOG.md](../CHANGELOG.md)** — Keep-a-Changelog formatted
  release history with mandatory disclosures for every GA.

## Getting help

1. Check the [Troubleshooting guide](TROUBLESHOOTING.md) first.
2. Search existing issues on GitHub.
3. Open a new issue at
   <https://github.com/alphaonedev/ai-memory-mcp/issues> with:
   - `ai-memory --version`
   - Your tier (`ai-memory stats --json`)
   - The last 50 lines of the daemon log (`journalctl -u ai-memory`)
4. For security vulnerabilities: **security@alphaone.dev**. Do not
   open public issues for those.
