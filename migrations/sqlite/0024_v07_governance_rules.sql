-- v0.7.0 (issue #691) — substrate-level agent-action rules engine.
--
-- The K9 governance pipeline (`src/governance/mod.rs`) gates only six
-- substrate-INTERNAL ops (memory_store, memory_link, memory_delete,
-- memory_archive, memory_consolidate, memory_replay). It has no
-- insertion point for agent-EXTERNAL actions — Bash command execution,
-- filesystem writes outside the substrate, network requests, process
-- spawns. Issue #691 RCA: every operator hard rule that has ever been
-- violated in the v0.7.0 campaign (5-6 occurrences of /tmp writes, low-
-- disk cargo runs) lived OUTSIDE the K9 surface. The fix is to add a
-- second engine — `check_agent_action` — that evaluates a declarative
-- table of rules at every external-action entry point.
--
-- This table holds those rules as typed data, not text. Each row is one
-- rule. The kind column is the AgentAction enum tag (Bash /
-- FilesystemWrite / NetworkRequest / ProcessSpawn / Custom). The
-- matcher column is per-kind JSON (glob for paths, regex for command
-- text, threshold for disk_free). The severity column drives the
-- pipeline outcome (refuse / warn / log). Every check emits a row to
-- `signed_events` so the audit chain captures both passes and refusals.
--
-- # Design-revision decisions (issue #691 comment 2026-05-13)
--
-- 1. Seed rules R001-R004 land at `enabled = 0`. Operator activates
--    them with `ai-memory rules enable <id> --sign` after auditing the
--    test fleet for `/tmp` usage (macOS `/private/tmp` is realpath of
--    `/tmp`; any test using `/tmp/*` would refuse if seed rules
--    landed enabled).
--
-- 2. Language is honest:
--    - For substrate-INTERNAL ops: "substrate-authoritative" (gate is
--      mechanical at write path).
--    - For agent-EXTERNAL ops: "substrate-rule-bound, harness-mediated"
--      (rule lives in substrate; harness PreToolUse hook calls
--      `memory_check_agent_action` and honors the decision).
--
-- 3. Rule mutation requires the operator's Ed25519 keypair on disk
--    (mode 0600, default `~/.config/ai-memory/keys/operator.priv`).
--    MCP stdio cannot mutate rules — `rule_add` / `rule_remove` /
--    `rule_enable` / `rule_disable` over MCP return
--    `governance.not_available_over_mcp` error. MCP can READ rules
--    and INVOKE check_agent_action.
--
-- # Columns
--
-- * `id`            — TEXT PRIMARY KEY, operator-chosen short id
--                     (R001 / R002 / etc) or a fresh UUIDv4.
-- * `kind`          — TEXT, the AgentAction enum tag in lower_snake
--                     (`bash` / `filesystem_write` / `network_request`
--                     / `process_spawn` / `custom`).
-- * `matcher`       — TEXT, JSON object whose shape depends on `kind`:
--                       Bash:           {"command_regex": "..."}
--                       FilesystemWrite: {"glob": "/tmp/**"}
--                       NetworkRequest: {"host": "evil.example.com"}
--                       ProcessSpawn:   {"binary": "cargo",
--                                        "disk_free_min_gib": 20}
--                       Custom:         {"kind": "foo", ...}
-- * `severity`      — TEXT, one of `refuse` / `warn` / `log`. The
--                     `refuse` outcome stops the action; `warn`
--                     proceeds with a logged warning; `log` is silent
--                     trace-level.
-- * `reason`        — TEXT, human-readable why-it's-refused string
--                     surfaced to the agent.
-- * `namespace`     — TEXT, defaults to `_global`. Rules can be
--                     namespace-scoped so a per-tenant rule does not
--                     bleed into another tenant.
-- * `created_by`    — TEXT, the operator's agent_id at rule creation
--                     time (NHI provenance).
-- * `created_at`    — INTEGER, UNIX epoch seconds.
-- * `enabled`       — INTEGER, 0 or 1. Defaults to 1 for runtime-
--                     created rules; seeded rules below land at 0
--                     awaiting operator activation.
-- * `signature`     — BLOB, optional Ed25519 signature over the
--                     canonical rule encoding. Present when the rule
--                     was added via `ai-memory rules add --sign`.
-- * `attest_level`  — TEXT, defaults to `unsigned`; future
--                     `operator_signed` / `peer_attested` extensions.
--
-- # Indexes
--
-- A composite index on (kind, enabled) covers the dominant query
-- shape `SELECT * FROM governance_rules WHERE kind = ? AND enabled = 1`
-- that `check_agent_action` runs on every call.

CREATE TABLE IF NOT EXISTS governance_rules (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    matcher       TEXT NOT NULL,
    severity      TEXT NOT NULL CHECK (severity IN ('refuse', 'warn', 'log')),
    reason        TEXT NOT NULL,
    namespace     TEXT NOT NULL DEFAULT '_global',
    created_by    TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    enabled       INTEGER NOT NULL DEFAULT 1,
    signature     BLOB,
    attest_level  TEXT NOT NULL DEFAULT 'unsigned'
);

CREATE INDEX IF NOT EXISTS idx_governance_rules_kind_enabled
    ON governance_rules (kind, enabled);
CREATE INDEX IF NOT EXISTS idx_governance_rules_namespace
    ON governance_rules (namespace);

-- # Seed rules R001-R004 — INERT (enabled = 0) at migration time
--
-- These are the operator hard rules that drove issue #691. They land
-- in the table so an operator can see and audit them, but they are
-- DISABLED. Operator activates with `ai-memory rules enable R001
-- --sign` (etc) after running the test-fleet audit:
--
--   grep -rn "/tmp/" tests/ scripts/  # find scripts using /tmp
--   grep -rn "/private/tmp/" tests/   # find macOS realpath uses
--
-- The seeded rows are `attest_level = 'unsigned'` because there is no
-- operator signature available at migration time. When the operator
-- enables, the CLI re-signs the row and bumps `attest_level` to
-- `operator_signed`.
--
-- INSERT OR IGNORE — re-running the migration is a no-op. An
-- operator that has already enabled R001 will not have it silently
-- re-disabled by a migration replay.

INSERT OR IGNORE INTO governance_rules
    (id, kind, matcher, severity, reason, namespace, created_by, created_at, enabled, signature, attest_level)
VALUES
    ('R001',
     'filesystem_write',
     '{"glob":"/tmp/**"}',
     'refuse',
     'Operator hard rule (#691): no /tmp writes. Use $TMPDIR or .local-runs/.',
     '_global',
     'system:seed',
     0,
     0,
     NULL,
     'unsigned'),
    ('R002',
     'filesystem_write',
     '{"glob":"/var/tmp/**"}',
     'refuse',
     'Operator hard rule (#691): no /var/tmp writes.',
     '_global',
     'system:seed',
     0,
     0,
     NULL,
     'unsigned'),
    ('R003',
     'filesystem_write',
     '{"glob":"/private/tmp/**"}',
     'refuse',
     'Operator hard rule (#691): no /private/tmp writes (macOS realpath of /tmp).',
     '_global',
     'system:seed',
     0,
     0,
     NULL,
     'unsigned'),
    ('R004',
     'process_spawn',
     '{"binary":"cargo","disk_free_min_gib":20}',
     'refuse',
     'Operator hard rule (#691): cargo refused on low-disk system (<20 GiB free).',
     '_global',
     'system:seed',
     0,
     0,
     NULL,
     'unsigned');
