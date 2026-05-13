-- v0.7.0 L1-5 — Agent Skills ingestion substrate (Pillar 1.5).
--
-- Two tables: `skills` (the registered skill rows) and
-- `skill_resources` (binary attachments: scripts, reference docs,
-- assets). Both are `CREATE TABLE IF NOT EXISTS` + supporting
-- `CREATE INDEX IF NOT EXISTS` so the migration is fully idempotent
-- on a database that already ran this file.
--
-- # `skills` columns
--
-- * `id`           — TEXT PRIMARY KEY, UUIDv4 minted by the register
--                    handler.
-- * `namespace`    — TEXT NOT NULL, caller-supplied namespace (mirrors
--                    the `memories.namespace` convention).
-- * `name`         — TEXT NOT NULL, the agentskills.io name token:
--                    1-64 chars, lowercase alphanumeric + hyphen,
--                    no leading / trailing / consecutive hyphens.
--                    Validated in the Rust layer (see
--                    `src/parsing/skill_md.rs`).
-- * `description`  — TEXT NOT NULL, 1-1024 chars.
-- * `license`      — TEXT, SPDX expression or free-form.  Optional.
-- * `compatibility`— TEXT, 1-500 chars when present.  Optional.
-- * `allowed_tools`— TEXT, JSON array of MCP tool names this skill
--                    is expected to use.  Optional.
-- * `metadata`     — TEXT NOT NULL DEFAULT '{}', extra JSON KVs.
-- * `body_blob`    — BLOB NOT NULL, zstd-3-compressed SKILL.md body
--                    (everything after the YAML frontmatter).
-- * `digest`       — BLOB NOT NULL, SHA-256 over the canonical
--                    signing surface (frontmatter JSON || body bytes
--                    || sorted resource digests).
-- * `signature`    — BLOB, Ed25519 signature over `digest` with the
--                    registering agent's keypair.  NULL when the
--                    daemon has no active keypair.
-- * `signing_agent`— TEXT, `agent_id` of the keypair used to sign.
--                    NULL when unsigned.
-- * `created_at`   — INTEGER NOT NULL, Unix epoch seconds (UTC).
-- * `superseded_by`— TEXT, self-FK: when a new version of
--                    (namespace, name) is registered the previous
--                    row's `superseded_by` is set to the new row's
--                    `id`.  NULL means "current".
--
-- # `skill_resources` columns
--
-- * `skill_id`     — TEXT NOT NULL REFERENCES skills(id) ON DELETE
--                    CASCADE.
-- * `resource_path`— TEXT NOT NULL, relative path as declared in the
--                    SKILL.md `resources:` section (e.g.
--                    `scripts/run.sh`).
-- * `resource_kind`— TEXT NOT NULL, one of: `script` | `reference`
--                    | `asset`.
-- * `content_blob` — BLOB, zstd-3-compressed resource content.
--                    NULL for reference-kind entries that carry only
--                    a URL / path, not inline bytes.
-- * `digest`       — BLOB, SHA-256 over the decompressed content.
--                    NULL when `content_blob` is NULL.
-- * `signature`    — BLOB, optional per-resource Ed25519 signature.
--
-- # Indexes
--
-- * `skills_namespace_name`  — (namespace, name): discovery list
--   query + uniqueness check for the version-chain upsert.
-- * `skills_supersedes`      — (superseded_by): finding all skills
--   that point at a given version without a full table scan.
--
-- # Rollback
--
-- The reverse migration (schema downgrade) drops both tables.  All
-- MCP skill tools then disappear from the registry automatically;
-- no other code change is needed to revert.
--
-- This file is included as `MIGRATION_V30_SQLITE` in
-- `src/storage/migrations.rs`.

CREATE TABLE IF NOT EXISTS skills (
    id              TEXT PRIMARY KEY,
    namespace       TEXT NOT NULL,
    name            TEXT NOT NULL,
    description     TEXT NOT NULL,
    license         TEXT,
    compatibility   TEXT,
    allowed_tools   TEXT,
    metadata        TEXT NOT NULL DEFAULT '{}',
    body_blob       BLOB NOT NULL,
    digest          BLOB NOT NULL,
    signature       BLOB,
    signing_agent   TEXT,
    created_at      INTEGER NOT NULL,
    superseded_by   TEXT REFERENCES skills(id)
);

CREATE INDEX IF NOT EXISTS skills_namespace_name
    ON skills(namespace, name);

CREATE INDEX IF NOT EXISTS skills_supersedes
    ON skills(superseded_by);

CREATE TABLE IF NOT EXISTS skill_resources (
    skill_id        TEXT NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
    resource_path   TEXT NOT NULL,
    resource_kind   TEXT NOT NULL,
    content_blob    BLOB,
    digest          BLOB,
    signature       BLOB,
    PRIMARY KEY (skill_id, resource_path)
);
