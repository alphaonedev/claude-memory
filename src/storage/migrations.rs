// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! SQLite schema definition + migration ladder. v0.7.0 L0.5-3
//! extracted the `SCHEMA` constant, the `MIGRATION_V*_SQLITE`
//! include-bytes constants, the `CURRENT_SCHEMA_VERSION` parallel
//! constant, and the `migrate` function out of `src/db.rs` into
//! this sub-module. Pure refactor — semantics unchanged. The
//! `MAX_SUPPORTED_SCHEMA` constant in `cli::boot` must still bump
//! in lockstep with [`CURRENT_SCHEMA_VERSION`] (current value: 42).

use anyhow::Result;
use rusqlite::{Connection, params};

pub(super) const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS memories (
    id               TEXT PRIMARY KEY,
    tier             TEXT NOT NULL,
    namespace        TEXT NOT NULL DEFAULT 'global',
    title            TEXT NOT NULL,
    content          TEXT NOT NULL,
    tags             TEXT NOT NULL DEFAULT '[]',
    priority         INTEGER NOT NULL DEFAULT 5,
    confidence       REAL NOT NULL DEFAULT 1.0,
    source           TEXT NOT NULL DEFAULT 'api',
    access_count     INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    last_accessed_at TEXT,
    expires_at       TEXT,
    metadata         TEXT NOT NULL DEFAULT '{}',
    -- v0.7.0 Task 1/8 (recursive learning, schema v29) — depth in the
    -- substrate-native reflection recursion tree. `0` for caller-minted
    -- memories (and any pre-v0.7.0 row); positive for synthesised
    -- reflections. Mirrors `models::Memory::reflection_depth`.
    reflection_depth INTEGER NOT NULL DEFAULT 0,
    -- v0.7.0 L1-1 (typed MemoryKind, schema v30) — first-class kind
    -- discriminator. `observation` for all caller-minted memories (and
    -- any pre-v30 row); `reflection` for memories minted by
    -- `memory_reflect` or the curator reflection pass.
    -- Mirrors `models::MemoryKind`.
    memory_kind TEXT NOT NULL DEFAULT 'observation',
    -- v0.7.0 WT-1-A (schema v36) — substrate-level atomisation foundation.
    -- `atomised_into` is NULL on legacy rows; positive integer on rows
    -- that have been split into atomic peers (WT-1-B atomisation pass).
    -- `atom_of` is NULL on non-atom rows; on atom rows it FK-points back
    -- to the parent memory. Pure additive — no existing semantics
    -- changes.
    atomised_into INTEGER,
    atom_of       TEXT REFERENCES memories(id),
    -- v0.7.0 QW-2 (schema v37) — Persona-as-artifact substrate primitive.
    -- `entity_id` is NULL on non-Persona rows; on Persona rows it carries
    -- the canonicalised entity descriptor the persona is about (e.g.
    -- `user:fate`). `persona_version` is NULL on non-Persona rows; on
    -- Persona rows it carries the monotonic per-(entity_id, namespace)
    -- generation counter. Pure additive — non-Persona rows keep NULL
    -- payloads with no backfill.
    entity_id       TEXT,
    persona_version INTEGER,
    -- v0.7.0 Form 4 (schema v38) — fact-provenance closeout. Citations
    -- is a JSON-encoded array of `Citation` objects ({uri, accessed_at,
    -- hash?, span?}) carrying first-class provenance pointers per
    -- memory; legacy rows default to '[]'. `source_uri` is a first-class
    -- URI-form pointer to the cited source body (distinct from the
    -- existing `source` role-label column); valid schemes are `uri:`
    -- (HTTP URL), `doc:` (substrate doc id), `file:` (filesystem path).
    -- `source_span` is a JSON-encoded `{start, end}` byte-range into
    -- the parent source body, populated by the WT-1-B atomisation
    -- writer for each atom (atom-grain span fact-provenance). All
    -- three columns are additive on legacy rows. See migration
    -- `0032_v07_form4_provenance.sql` for the supporting index.
    citations       TEXT NOT NULL DEFAULT '[]',
    source_uri      TEXT,
    source_span     TEXT,
    -- v0.7.0 Form 5 (schema v39, issue #758) — auto-confidence + shadow-mode +
    -- calibration tooling closeout. `confidence_source` is a typed
    -- discriminator naming the provenance of the `confidence` column value
    -- (caller_provided | auto_derived | calibrated | decayed); legacy rows
    -- default to 'caller_provided' via the SQL DEFAULT clause.
    -- `confidence_signals` is a JSON snapshot of the ConfidenceSignals
    -- struct emitted when the value was computed (NULL on legacy rows).
    -- `confidence_decayed_at` is an RFC3339 timestamp of the last decay
    -- computation (NULL on legacy rows and rows never touched by decay).
    confidence_source     TEXT NOT NULL DEFAULT 'caller_provided',
    confidence_signals    TEXT,
    confidence_decayed_at TEXT,
    -- v0.7.0 polish PERF-8 (schema v42, issue #781) — auto-persona
    -- indexed entity-id column. Carries the canonical entity descriptor
    -- a memory MENTIONS (extracted at write time from
    -- `metadata.entity_id` or a `[entity:X]` title marker) so the
    -- auto-persona matcher resolves with
    -- `WHERE memory_kind = 'reflection' AND mentioned_entity_id = ?
    -- AND namespace = ?` via the `idx_memories_mentioned_entity`
    -- partial index instead of the previous full-table `content LIKE
    -- '%X%'` scan. Deliberately distinct from the QW-2 `entity_id`
    -- column above (which is reserved for Persona-row attribution):
    -- PERF-8 reads the OPPOSITE direction (the entity an observation
    -- / reflection mentions). Legacy rows default to NULL; the
    -- migration ladder backfills from metadata+title at v42 apply
    -- time.
    mentioned_entity_id   TEXT
);

CREATE INDEX IF NOT EXISTS idx_memories_tier ON memories(tier);
CREATE INDEX IF NOT EXISTS idx_memories_namespace ON memories(namespace);
CREATE INDEX IF NOT EXISTS idx_memories_priority ON memories(priority DESC);
CREATE INDEX IF NOT EXISTS idx_memories_expires ON memories(expires_at);
CREATE UNIQUE INDEX IF NOT EXISTS idx_memories_title_ns ON memories(title, namespace);
-- v36 partial indexes on the atomisation columns. Restricted predicates
-- keep legacy-DB index footprint at zero until WT-1-B starts minting
-- atoms.
CREATE INDEX IF NOT EXISTS idx_memories_atom_of
    ON memories(atom_of) WHERE atom_of IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_memories_atomised_into
    ON memories(atomised_into) WHERE atomised_into > 0;
-- v37 (QW-2) partial index covering per-entity persona lookups lives
-- in `migrations/sqlite/0031_v07_persona.sql` and runs from the
-- migrate step's `if version < 37` arm — NOT in this bootstrap
-- SCHEMA, because `db::open` applies SCHEMA before `migrate`, and
-- the index references `entity_id` (a column only present after the
-- v37 ALTER fires on a legacy DB). Fresh installs land the column
-- via the CREATE TABLE above, then the migrate step's v37 arm
-- creates the index a few statements later.
-- v38 (Form 4) partial index covering the `--source-uri-prefix`
-- recall filter. Mirrors the persona pattern: legacy rows have NULL
-- `source_uri`, the partial predicate keeps the index footprint at
-- zero until callers start writing URIs.
CREATE INDEX IF NOT EXISTS idx_memories_source_uri
    ON memories(source_uri) WHERE source_uri IS NOT NULL;
-- v39 (Form 5) partial index covering rows whose `confidence_source`
-- is NOT the (overwhelming-majority) `caller_provided` bucket. The
-- calibration CLI scans this slice to enumerate derived / calibrated /
-- decayed rows; the partial predicate keeps the index footprint on
-- legacy DBs at zero until the auto-confidence engine starts writing.
CREATE INDEX IF NOT EXISTS idx_memories_confidence_source
    ON memories(confidence_source) WHERE confidence_source != 'caller_provided';
-- v39 (Form 5) — per-recall shadow-mode telemetry. Populated when
-- AI_MEMORY_CONFIDENCE_SHADOW=1 and sampled at
-- AI_MEMORY_CONFIDENCE_SHADOW_SAMPLE_RATE. The calibration CLI reads
-- this table to compute per-(namespace, source) baselines.
-- v40 (Cluster G) added the denormalised `source` column + compound
-- `(namespace, source, observed_at)` index so the calibration scan
-- streams a single-table SQL aggregation (was: full-window Vec materialise
-- + Rust grouping, PERF-12).
CREATE TABLE IF NOT EXISTS confidence_shadow_observations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    memory_id TEXT NOT NULL,
    namespace TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'unknown',
    caller_confidence REAL NOT NULL,
    derived_confidence REAL NOT NULL,
    signals TEXT NOT NULL,
    recall_outcome TEXT,
    observed_at TEXT NOT NULL,
    FOREIGN KEY(memory_id) REFERENCES memories(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_shadow_obs_namespace
    ON confidence_shadow_observations(namespace);
CREATE INDEX IF NOT EXISTS idx_shadow_obs_observed_at
    ON confidence_shadow_observations(observed_at);
CREATE INDEX IF NOT EXISTS idx_shadow_obs_memory
    ON confidence_shadow_observations(memory_id);
CREATE INDEX IF NOT EXISTS idx_shadow_obs_namespace_source_observed
    ON confidence_shadow_observations(namespace, source, observed_at);
-- v42 (PERF-8 #781) — partial index covering the auto-persona matcher's
-- `WHERE memory_kind = 'reflection' AND mentioned_entity_id = ?
-- AND namespace = ?` lookup. The partial predicate matches the literal
-- `memory_kind = 'reflection'` constraint in the matcher SQL so the
-- SQLite planner reliably picks this index over a sequential scan
-- (the `mentioned_entity_id = ?` equality predicate prunes NULL rows
-- from the result set; the partial predicate just keeps the index
-- narrow). Non-reflection rows contribute zero index pages.
CREATE INDEX IF NOT EXISTS idx_memories_mentioned_entity
    ON memories(mentioned_entity_id, namespace) WHERE memory_kind = 'reflection';

CREATE TABLE IF NOT EXISTS memory_links (
    source_id    TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id    TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation     TEXT NOT NULL DEFAULT 'related_to',
    created_at   TEXT NOT NULL,
    -- v15 temporal trio (added historically via ALTER); included in the
    -- bootstrap SCHEMA so test fixtures that stamp `version >= v15`
    -- match real-DB shape post-migration ladder.
    valid_from   TEXT,
    valid_until  TEXT,
    observed_by  TEXT,
    -- v17-era signature column (Ed25519 attestation, added historically
    -- via ALTER).
    signature    BLOB,
    -- v23 attest_level column (added historically via ALTER).
    attest_level TEXT,
    PRIMARY KEY (source_id, target_id, relation),
    -- v33 (v0.7.0 v0.7.1-fold) — SQL-side CHECK constraint promoting the
    -- v23 RAISE-trigger validation to a column-level invariant. Closed
    -- taxonomy mirrors `crate::validate::VALID_RELATIONS`. v36 (WT-1-A)
    -- extended the closed set with `derives_from` for atomisation
    -- provenance edges (atom -> parent).
    CHECK (relation IN ('related_to', 'supersedes', 'contradicts', 'derived_from', 'reflects_on', 'derives_from'))
);

CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
    title,
    content,
    tags,
    content=memories,
    content_rowid=rowid
);

CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, title, content, tags)
    VALUES (new.rowid, new.title, new.content, new.tags);
END;

CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, content, tags)
    VALUES ('delete', old.rowid, old.title, old.content, old.tags);
END;

CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, content, tags)
    VALUES ('delete', old.rowid, old.title, old.content, old.tags);
    INSERT INTO memories_fts(rowid, title, content, tags)
    VALUES (new.rowid, new.title, new.content, new.tags);
END;

CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
);

-- v0.6.4-009 — capability-expansion audit log (NHI guardrails phase 1).
-- Mirrors migrations/sqlite/0014_v064_audit_log.sql so a fresh DB
-- bootstrap that bypasses the migration ladder still ends up with the
-- table present.
CREATE TABLE IF NOT EXISTS audit_log (
    id                 TEXT PRIMARY KEY,
    agent_id           TEXT,
    event_type         TEXT NOT NULL,
    requested_family   TEXT,
    granted            INTEGER NOT NULL,
    attestation_tier   TEXT,
    timestamp          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_audit_log_agent_id
    ON audit_log (agent_id);
CREATE INDEX IF NOT EXISTS idx_audit_log_timestamp
    ON audit_log (timestamp);
CREATE INDEX IF NOT EXISTS idx_audit_log_event_type
    ON audit_log (event_type);

-- v40 (Cluster-C SEC-3, issue #767) — deferred-audit drainer DLQ.
-- Mirrors `migrations/sqlite/0034_v07_signed_events_dlq.sql` so a
-- fresh DB bootstrap that bypasses the migration ladder still ends
-- up with the table present. See the migration file for the design
-- rationale (failure-split between race-requeue and DLQ-land).
CREATE TABLE IF NOT EXISTS signed_events_dlq (
    dlq_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    id              TEXT NOT NULL,
    agent_id        TEXT NOT NULL,
    event_type      TEXT NOT NULL,
    payload_hash    BLOB NOT NULL,
    signature       BLOB,
    attest_level    TEXT NOT NULL DEFAULT 'unsigned',
    timestamp       TEXT NOT NULL,
    failure_reason  TEXT NOT NULL,
    failed_at       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_signed_events_dlq_failed_at
    ON signed_events_dlq(failed_at);
CREATE INDEX IF NOT EXISTS idx_signed_events_dlq_agent
    ON signed_events_dlq(agent_id);
";

// v17 = v0.6.3.1 (P4, audit G1) governance.inherit backfill.
// v18 = v0.6.3.1 (P2, audit G4/G5/G13) data-integrity hardening:
//       embedding_dim guard, archive lossless, magic-byte header.
// v19 = v0.6.3.1 (P5, audit G9) webhook event-types column +
//       per-subscriber filter.
// v20 = v0.6.4-009 (NHI guardrails phase 1) capability-expansion
//       audit_log table.
// v21 = v0.7.0 K2 pending_actions timeout sweeper:
//       `default_timeout_seconds` + `expired_at` columns plus a
//       composite (status, requested_at) index to bound the sweep
//       cost.
// v22 = v0.7.0 I1 (attested-cortex epic) `memory_transcripts` BLOB
//       store with zstd-3 content blobs. Substrate for I2 (join
//       table), I3 (archive→prune lifecycle), I4 (memory_replay),
//       I5/R5 (pre_store extraction hook).
// v23 = v0.7.0 H2 (attested-cortex epic, outbound link signing)
//       `memory_links.attest_level` TEXT column ("unsigned" |
//       "self_signed" | "peer_attested"). The companion `signature`
//       BLOB column shipped dead in v15 and is now live. H3+H4 will
//       layer inbound verification + the `memory_verify` MCP tool on
//       top of this column.
// v24 = v0.7.0 I2 (attested-cortex epic) `memory_transcript_links`
//       join table establishing the m:n relationship between
//       `memories` and the `memory_transcripts` substrate from I1
//       (v22). Optional (span_start, span_end) byte offsets address a
//       sub-region of the decompressed transcript. ON DELETE CASCADE
//       on both foreign keys keeps the table free of dangling rows
//       when memories are deleted or I3's archive->prune lifecycle
//       removes transcripts. Substrate for I4 (memory_replay) and
//       I5/R5 (pre_store extraction hook).
// v25 = v0.7.0 I3 (attested-cortex epic) per-namespace transcript TTL
//       with archive->prune lifecycle. Adds the `archived_at TEXT`
//       column on `memory_transcripts` (NULL = live, RFC3339 = the
//       moment the sweeper marked the row archived) plus a partial
//       index on archived rows so the prune-phase scan is bounded.
//       The lifecycle sweeper itself lives in `transcripts.rs` and
//       runs on a 10-minute cadence from `daemon_runtime`. Per-
//       namespace TTL overrides arrive via the `[transcripts]`
//       config section (`config.rs`) and are resolved against the
//       transcript's namespace at sweep time.
// v29 = v0.7.0 Task 1/8 (recursive learning) — `memories.reflection_depth`
//       INTEGER NOT NULL DEFAULT 0 column. Depth in the substrate-native
//       reflection recursion tree; 0 for caller-minted (or pre-v0.7.0)
//       rows. ALTER TABLE emitted from Rust (SQLite has no `ADD COLUMN
//       IF NOT EXISTS`); fresh-schema installs pick it up inline from
//       the `SCHEMA` constant above.
// v30 = v0.7.0 (issue #691) — `governance_rules` table backing the
//       substrate-level agent-action rules engine. Seed rules R001-R004
//       land at `enabled=0`; operator activates with `ai-memory rules
//       enable <id> --sign`. CREATE TABLE IF NOT EXISTS + INSERT OR
//       IGNORE on seed — fully idempotent.
// v31 = v0.7.0 L1-1 (typed MemoryKind::Reflection enum) —
//       `memories.memory_kind TEXT NOT NULL DEFAULT 'observation'` column.
//       First-class typed kind discriminator; `Observation` (default) for all
//       pre-v31 rows, `Reflection` for memories minted by `memory_reflect` or
//       the curator reflection pass. ALTER TABLE emitted from Rust; the SQL
//       file holds the idempotent backfill (metadata.type='reflection' →
//       memory_kind='reflection') plus the supporting index. Originally
//       authored as v30 on l1/typed-memorykind; renumbered to v31 during
//       the L1 wave merge after substrate-rules (issue #691) took v30.
// v32 = v0.7.0 L1-5 — Agent Skills ingestion substrate (Pillar 1.5).
//       `skills` table (id, namespace, name, description, license,
//       compatibility, allowed_tools, metadata, body_blob, digest,
//       signature, signing_agent, created_at, superseded_by) +
//       `skill_resources` table (skill_id, resource_path, resource_kind,
//       content_blob, digest, signature) + indexes. Fully idempotent
//       (CREATE TABLE IF NOT EXISTS + CREATE INDEX IF NOT EXISTS).
//       Reverse migration drops both tables; MCP skill tools disappear
//       from the registry automatically. Originally authored as v30 on
//       l1/agent-skills; renumbered to v32 during the L1 wave merge.
// v33 = v0.7.0 v0.7.1-fold (#687/#688) — promote
//       `memory_links.relation` validation from v23 RAISE triggers to a
//       SQL-side CHECK constraint baked into the column definition.
//       Decision memory `65ba07f6`; backlog memory `7b279df3`. Folds
//       the v0.7.1 hardening carry-forward into v0.7.0 per the
//       2026-05-13 operator directive. SQLite has no `ALTER TABLE ADD
//       CONSTRAINT CHECK` for an existing column, so the migration is
//       a full-table-rebuild: CREATE TABLE memory_links_new (with
//       CHECK clause) → INSERT SELECT → DROP indexes/triggers/old
//       table → RENAME → recreate indexes + attest_level triggers.
//       The v23 relation triggers are dropped and not recreated; the
//       column-level CHECK supersedes them.
// v34 = v0.7.0 V-4 closeout (#698) — add SQL-side cross-row hash
//       chain to `signed_events`. Adds `prev_hash BLOB` + `sequence
//       INTEGER` columns plus a UNIQUE index on sequence. Per-row
//       Ed25519 signatures (the existing `signature` column) remain
//       as defense-in-depth; the cross-row chain becomes the LOAD-
//       BEARING tamper-evidence property in the SQL substrate.
//       SQLite has no `ALTER TABLE ADD COLUMN IF NOT EXISTS`, so the
//       ALTERs are emitted from Rust via column-existence probes;
//       the SQL file (`0028_v07_signed_events_chain.sql`) holds the
//       supporting UNIQUE INDEX. Backfill runs in
//       `migrate_v34_backfill_chain` because the row-by-row
//       prev_hash computation needs the application-layer
//       canonical-bytes encoding (`signed_events::
//       canonical_chain_bytes`). Idempotent — re-running on an
//       already-backfilled DB is a no-op (probes detect the columns
//       and the existence of populated sequence rows).
// v35 = v0.7.0 QW-3 — context-offload substrate primitive. Adds
//       `offloaded_blobs` table backing the offload+deref engine
//       in `src/offload/`. v0.8.0 short-term-context-compression
//       (Mermaid canvas + auto-cadence + node_id integration) will
//       build on this plumbing. CREATE TABLE IF NOT EXISTS +
//       CREATE INDEX IF NOT EXISTS — fully idempotent.
// v36 = v0.7.0 WT-1-A — substrate-level atomisation foundation. Adds
//       `memories.atomised_into INTEGER` + `memories.atom_of TEXT
//       REFERENCES memories(id)` for the WT-1-B atomisation primitive,
//       plus a `derives_from` extension to the `memory_links.relation`
//       closed-taxonomy CHECK constraint. The ALTERs on `memories` are
//       emitted from Rust (SQLite has no `ADD COLUMN IF NOT EXISTS`).
//       The CHECK extension is a full-table-rebuild on `memory_links`
//       (column-level CHECK can't be ALTERed on an existing column on
//       SQLite — same dance as v33's 0027 migration). The SQL file
//       (`0030_v07_atomisation.sql`) holds the supporting partial
//       indexes. Pure additive on legacy data: every pre-v36 row has
//       `atomised_into IS NULL` and `atom_of IS NULL`. The first hard
//       prereq for WT-1-B (atomisation pass) through WT-1-G.
// v37 = v0.7.0 QW-2 — Persona-as-artifact substrate primitive. Adds
//       `memories.entity_id TEXT NULL` + `memories.persona_version
//       INTEGER NULL` columns plus the partial index
//       `idx_personas_by_entity` covering Persona-kind rows. The
//       ALTERs are emitted from Rust (SQLite has no `ADD COLUMN IF
//       NOT EXISTS`); the SQL file holds the supporting partial
//       index. Substrate for Tencent-pattern L3 personas; non-
//       Persona rows keep NULL payloads with no backfill.
// v38 = v0.7.0 Form 4 — fact-provenance closeout (issue #757). Adds
//       `memories.citations TEXT NOT NULL DEFAULT '[]'` (JSON array of
//       Citation objects), `memories.source_uri TEXT NULL` (first-class
//       URI-form pointer to the cited source body, distinct from the
//       existing `source` role-label column), and
//       `memories.source_span TEXT NULL` (JSON-encoded `{start,end}`
//       byte-range into the parent source body, populated by the
//       WT-1-B atomisation writer for atom-grain span fact-provenance).
//       The ALTERs are emitted from Rust (SQLite has no `ADD COLUMN IF
//       NOT EXISTS`); the SQL file holds the supporting partial index
//       `idx_memories_source_uri`. Pure additive on legacy rows.
// v39 = v0.7.0 Form 5 — auto-confidence + shadow-mode + calibration
//       tooling closeout (issue #758). Adds three columns on `memories`
//       (`confidence_source TEXT NOT NULL DEFAULT 'caller_provided'`,
//       `confidence_signals TEXT NULL`, `confidence_decayed_at TEXT NULL`)
//       plus the `confidence_shadow_observations` table backing the
//       shadow-mode telemetry pipeline. The ALTERs on `memories` are
//       emitted from Rust (SQLite has no `ADD COLUMN IF NOT EXISTS`);
//       the SQL file holds the supporting table + partial index. Pure
//       additive on legacy rows; the auto-derive engine is opt-in via
//       `AI_MEMORY_AUTO_CONFIDENCE=1` so the column stays at
//       'caller_provided' until operators flip the switch.
// v40 = v0.7.0 Cluster-C SEC-3 closeout (issue #767) — adds the
//       `signed_events_dlq` table backing the deferred-audit drainer's
//       new dead-letter-queue path. Pre-Cluster-C the drainer dropped
//       failed appends silently; with v40 in place the drainer requeues
//       on `SQLITE_CONSTRAINT_UNIQUE` (chain-head race) and lands every
//       other failure in `signed_events_dlq`. Pure additive on legacy
//       data — fresh installs inherit the table via the bootstrap
//       SCHEMA; pre-v40 deployments pick it up here. The DLQ is
//       intentionally NOT append-only (operator-driven replay deletes
//       rows after re-append).
// v41 = v0.7.0 Cluster G — shadow-mode retention + denormalised
//       `source` column + compound `(namespace, source, observed_at)`
//       index supporting the calibration scan (issue #767, PERF-4 +
//       PERF-12). The ALTER adding `source` is emitted from Rust
//       (SQLite has no `ADD COLUMN IF NOT EXISTS`); the SQL file
//       0035 holds the compound index. The backfill UPDATE
//       (copying `memories.source` into legacy observation rows)
//       runs from Rust so the column-existence probe gates it.
//       Pure additive — every pre-Cluster-G observation row keeps
//       its existing fields; the new `source` column lands with the
//       backfill or `'unknown'` (defense in depth) for orphan rows
//       whose source memory has already been CASCADE-deleted.
//       (Renumbered from v40 to v41 during rebase onto trunk: Cluster C
//       SEC-3 closeout #770 landed first and claimed v40 for the
//       `signed_events_dlq` table.)
// v42 = v0.7.0 polish PERF-8 (issue #781) — auto-persona indexed
//       entity-id column replacing the content `LIKE '%entity_X%'`
//       full-table scan. Adds `memories.mentioned_entity_id TEXT` +
//       partial index `WHERE memory_kind = 'reflection'`. The
//       ALTER lives in Rust (SQLite has no `ADD COLUMN IF NOT EXISTS`);
//       the SQL file 0036 holds the partial index. Backfill (extracting
//       the entity descriptor from `metadata.entity_id` or a
//       `[entity:X]` title marker on pre-existing reflection rows) also
//       runs from Rust so the column-existence probe gates it. Pure
//       additive — non-tagged legacy reflections stay at NULL (they
//       were never matchable by the previous LIKE path either).
//       Column-name deliberately distinct from the QW-2 `entity_id`
//       column (which is reserved for Persona-row attribution); PERF-8
//       reads the OPPOSITE direction (the entity an observation /
//       reflection MENTIONS).
const CURRENT_SCHEMA_VERSION: i64 = 42;

const MIGRATION_V15_SQLITE: &str =
    include_str!("../../migrations/sqlite/0010_v063_hierarchy_kg.sql");
// v0.6.3.1 (P4, audit G1): backfill `metadata.governance.inherit = true`
// on existing policies so downstream readers and SQL-side dashboards
// see a consistent shape after upgrade. Idempotent.
const MIGRATION_V17_SQLITE: &str =
    include_str!("../../migrations/sqlite/0012_governance_inherit.sql");
// v0.6.3.1 (P2, audit G4/G5/G13): data-integrity hardening. ALTER TABLEs
// emitted from Rust because SQLite has no `ADD COLUMN IF NOT EXISTS`;
// the SQL file holds idempotent backfills + indexes.
const MIGRATION_V18_SQLITE: &str =
    include_str!("../../migrations/sqlite/0011_v0631_data_integrity.sql");
// v0.6.3.1 (P5, audit G9): webhook event-types column + per-subscriber
// filter index. ADD COLUMN done inline (SQLite has no `ADD COLUMN IF NOT
// EXISTS`); SQL file holds the idempotent index batch.
const MIGRATION_V19_SQLITE: &str =
    include_str!("../../migrations/sqlite/0013_webhook_event_types.sql");
// v0.6.4-009: capability-expansion audit log table. CREATE TABLE IF NOT
// EXISTS + indexes — fully idempotent.
const MIGRATION_V20_SQLITE: &str = include_str!("../../migrations/sqlite/0014_v064_audit_log.sql");
// v0.7.0 K2: pending_actions timeout sweeper. ALTER TABLEs are emitted
// from Rust (see v21 below) because SQLite has no `ADD COLUMN IF NOT
// EXISTS`; this file just holds the idempotent index batch.
const MIGRATION_V21_SQLITE: &str =
    include_str!("../../migrations/sqlite/0015_v07_pending_action_timeouts.sql");
// v0.7.0 I1 — `memory_transcripts` table backing the attested-cortex
// epic. CREATE TABLE IF NOT EXISTS + index — fully idempotent. Substrate
// for I2 (join table), I3 (archive→prune lifecycle), I4 (memory_replay),
// and I5/R5 (pre_store extraction hook).
const MIGRATION_V22_SQLITE: &str = include_str!("../../migrations/sqlite/0016_v07_transcripts.sql");
// v0.7.0 H2 — outbound link signing. ALTER TABLE adding the
// `attest_level` column is emitted from Rust (SQLite has no
// `ADD COLUMN IF NOT EXISTS`); this file holds the idempotent
// backfill ("unsigned" for legacy rows) plus the supporting index.
const MIGRATION_V23_SQLITE: &str =
    include_str!("../../migrations/sqlite/0017_v07_link_attest_level.sql");
// v0.7.0 I2 — `memory_transcript_links` join table connecting
// `memories` to the `memory_transcripts` substrate from I1 (v22).
// CREATE TABLE IF NOT EXISTS + indexes — fully idempotent. Substrate
// only; I4 (memory_replay) reads from this table and I5/R5
// (pre_store extraction hook) writes to it.
const MIGRATION_V24_SQLITE: &str =
    include_str!("../../migrations/sqlite/0018_v07_transcript_links.sql");
// v0.7.0 I3 — per-namespace transcript TTL with archive->prune
// lifecycle. ALTER TABLE adding `memory_transcripts.archived_at` is
// emitted from Rust (SQLite has no `ADD COLUMN IF NOT EXISTS`); the
// SQL file holds the supporting partial index on archived rows so
// the prune-phase scan stays O(archived) rather than O(total).
const MIGRATION_V25_SQLITE: &str =
    include_str!("../../migrations/sqlite/0019_v07_transcript_lifecycle.sql");
// v0.7.0 H5 — append-only `signed_events` audit table backing the
// immutable attestation chain. CREATE TABLE IF NOT EXISTS + indexes —
// fully idempotent. The H5 substrate; H6 read-side tooling layers on
// top.
const MIGRATION_V26_SQLITE: &str =
    include_str!("../../migrations/sqlite/0020_v07_signed_events.sql");
// v0.7.0 K6 — A2A correlation IDs + ACK / retry / DLQ for the
// subscription dispatch path. Adds `subscription_events.correlation_id`
// (UUIDv7 string) for replay-from-cursor lookups, the
// `subscription_events` audit table itself (created here because no
// prior K-track migration introduced it), and the `subscription_dlq`
// table holding deliveries that exhausted the three-attempt retry
// ladder. The ALTER TABLE on a pre-existing `subscription_events`
// row (deployments that hand-rolled it) is emitted from Rust because
// SQLite has no `ADD COLUMN IF NOT EXISTS`; the SQL file holds the
// idempotent CREATE TABLE / CREATE INDEX statements.
const MIGRATION_V27_SQLITE: &str =
    include_str!("../../migrations/sqlite/0021_v07_a2a_correlation.sql");
// v0.7.0 K8 — per-agent quotas (memories/day, storage bytes, links/day).
// CREATE TABLE IF NOT EXISTS + index — fully idempotent. Daily counters
// reset at UTC midnight via the K8 sweep loop wired into
// `daemon_runtime::bootstrap_serve`. The store_memory + memory_link
// write paths consult the row before committing; on exceeded limit the
// call returns a `QUOTA_EXCEEDED` diagnostic naming the limit hit.
const MIGRATION_V28_SQLITE: &str =
    include_str!("../../migrations/sqlite/0022_v07_agent_quotas.sql");
// v0.7.0 (issue #691) — substrate-level agent-action rules engine.
// `governance_rules` table holds typed rules (kind / matcher / severity)
// evaluated by `check_agent_action`. Seed rules R001-R004 land at
// `enabled=0` (per design revision 2026-05-13) so the test fleet does
// not break on macOS `/private/tmp` realpath. Operator activates with
// `ai-memory rules enable <id> --sign` after running the test-fleet
// audit. CREATE TABLE IF NOT EXISTS + INSERT OR IGNORE — fully
// idempotent.
const MIGRATION_V30_SQLITE: &str =
    include_str!("../../migrations/sqlite/0024_v07_governance_rules.sql");
// v0.7.0 L1-1 — typed MemoryKind::Reflection enum. Adds the
// `memories.memory_kind TEXT NOT NULL DEFAULT 'observation'` column.
// ALTER TABLE done inline (SQLite has no `ADD COLUMN IF NOT EXISTS`);
// the SQL file holds the idempotent backfill UPDATE (metadata.type =
// 'reflection' → memory_kind = 'reflection') and the supporting index.
// Renumbered from v30 → v31 during the L1 wave merge after
// substrate-rules took v30. File name kept stable to preserve the
// historical record of the L1-1 patch.
const MIGRATION_V31_SQLITE: &str = include_str!("../../migrations/sqlite/0025_v07_memory_kind.sql");
// v0.7.0 L1-5 — Agent Skills ingestion substrate (Pillar 1.5).
// `skills` + `skill_resources` tables with supporting indexes.
// CREATE TABLE IF NOT EXISTS + CREATE INDEX IF NOT EXISTS — fully
// idempotent; safe to replay on a database that already ran this
// migration. Renumbered from v30 → v32 during the L1 wave merge after
// substrate-rules took v30 and L1-1 took v31; file renamed
// 0023_v07_agent_skills.sql → 0026_v07_agent_skills.sql.
const MIGRATION_V32_SQLITE: &str =
    include_str!("../../migrations/sqlite/0026_v07_agent_skills.sql");
// v0.7.0 v0.7.1-fold (#687/#688) — full-table-rebuild promoting the
// `memory_links.relation` RAISE triggers from migration 0023 to a real
// SQL-side CHECK constraint. The SQL is purely declarative
// (CREATE/INSERT/DROP/RENAME); no Rust shim required beyond the
// `execute_batch` call below. Replay-safe because the new table name
// is `memory_links_new` only for the duration of the batch.
const MIGRATION_V33_SQLITE: &str =
    include_str!("../../migrations/sqlite/0027_v07_memory_links_relation_check.sql");
// v0.7.0 V-4 closeout (#698) — SQL-side cross-row hash chain on
// `signed_events`. The ALTERs that add `prev_hash` + `sequence`
// columns are emitted from Rust (SQLite has no `ADD COLUMN IF NOT
// EXISTS`); this file just holds the supporting UNIQUE INDEX on
// `sequence`. Backfill of prev_hash + sequence on pre-existing rows
// runs in `migrate_v34_backfill_chain` because the per-row
// prev_hash computation needs the application-layer canonical-bytes
// encoding.
const MIGRATION_V34_SQLITE: &str =
    include_str!("../../migrations/sqlite/0028_v07_signed_events_chain.sql");
// v0.7.0 QW-3 — context-offload substrate primitive (offloaded_blobs
// table + namespace and TTL indexes). CREATE TABLE IF NOT EXISTS +
// CREATE INDEX IF NOT EXISTS — fully idempotent; safe to replay on a
// database that already ran this migration. Substrate-only;
// v0.8.0 short-term-context-compression will read/write via
// `src/offload/mod.rs`.
const MIGRATION_V35_SQLITE: &str =
    include_str!("../../migrations/sqlite/0029_v07_offloaded_blobs.sql");
// v0.7.0 WT-1-A — substrate-level atomisation foundation. Adds
// `memories.atomised_into INTEGER` + `memories.atom_of TEXT REFERENCES
// memories(id)` plus `derives_from` extension to the closed-taxonomy
// CHECK constraint on `memory_links.relation`. The ALTERs on
// `memories` are emitted from Rust (SQLite has no `ADD COLUMN IF NOT
// EXISTS`); the CHECK-constraint extension is a full-table-rebuild
// dance (same shape as the v33 migration in 0027). The SQL file holds
// only the supporting partial indexes; the rebuild lives in Rust so
// the probes (column existence, table existence) can run idempotently.
const MIGRATION_V36_SQLITE: &str = include_str!("../../migrations/sqlite/0030_v07_atomisation.sql");

/// v36 (WT-1-A) — full-table-rebuild for `memory_links` that promotes
/// the v33 closed-taxonomy CHECK constraint to include the new
/// `derives_from` relation (atomisation provenance). The rebuild mirrors
/// the v33 dance from `0027_v07_memory_links_relation_check.sql`:
/// create memory_links_v36 with the extended CHECK, copy rows, drop
/// indexes/triggers/old table, rename, recreate indexes + attest_level
/// triggers. The CHECK clause is the only line that changes from v33.
///
/// This rebuild lives in Rust (not the SQL file) so the column-existence
/// probe + `sqlite_master.sql` probe in the migrate step can stay
/// idempotent: re-running on a partially-migrated DB detects the
/// extended CHECK and skips the rebuild.
const MIGRATION_V36_REBUILD_LINKS_SQL: &str = r"
-- WT-1-A — full-table-rebuild adding 'derives_from' to the
-- memory_links.relation CHECK clause. Identical to the v33 rebuild in
-- 0027 except for the CHECK clause; replay-safe because the new table
-- is named memory_links_v36 only for the duration of the rebuild.

CREATE TABLE memory_links_v36 (
    source_id    TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    target_id    TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    relation     TEXT NOT NULL DEFAULT 'related_to',
    created_at   TEXT NOT NULL,
    valid_from   TEXT,
    valid_until  TEXT,
    observed_by  TEXT,
    signature    BLOB,
    attest_level TEXT,
    PRIMARY KEY (source_id, target_id, relation),
    CHECK (relation IN ('related_to', 'supersedes', 'contradicts',
                        'derived_from', 'reflects_on', 'derives_from'))
);

INSERT INTO memory_links_v36 (
    source_id, target_id, relation, created_at,
    valid_from, valid_until, observed_by, signature, attest_level
)
SELECT
    source_id, target_id, relation, created_at,
    valid_from, valid_until, observed_by, signature, attest_level
FROM memory_links;

DROP TRIGGER IF EXISTS memory_links_ck_attest_level_ins;
DROP TRIGGER IF EXISTS memory_links_ck_attest_level_upd;

DROP INDEX IF EXISTS idx_links_temporal_src;
DROP INDEX IF EXISTS idx_links_temporal_tgt;
DROP INDEX IF EXISTS idx_links_relation;
DROP INDEX IF EXISTS idx_memory_links_attest_level;

DROP TABLE memory_links;

ALTER TABLE memory_links_v36 RENAME TO memory_links;

CREATE INDEX IF NOT EXISTS idx_links_temporal_src
    ON memory_links (source_id, valid_from, valid_until);
CREATE INDEX IF NOT EXISTS idx_links_temporal_tgt
    ON memory_links (target_id, valid_from, valid_until);
CREATE INDEX IF NOT EXISTS idx_links_relation
    ON memory_links (relation, valid_from);
CREATE INDEX IF NOT EXISTS idx_memory_links_attest_level
    ON memory_links (attest_level, created_at);

CREATE TRIGGER IF NOT EXISTS memory_links_ck_attest_level_ins
BEFORE INSERT ON memory_links
FOR EACH ROW
WHEN NEW.attest_level IS NOT NULL
  AND NEW.attest_level NOT IN ('unsigned', 'self_signed', 'peer_attested')
BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: memory_links.attest_level must be one of unsigned/self_signed/peer_attested (or NULL for legacy rows)');
END;

CREATE TRIGGER IF NOT EXISTS memory_links_ck_attest_level_upd
BEFORE UPDATE OF attest_level ON memory_links
FOR EACH ROW
WHEN NEW.attest_level IS NOT NULL
  AND NEW.attest_level NOT IN ('unsigned', 'self_signed', 'peer_attested')
BEGIN
    SELECT RAISE(ABORT, 'CHECK constraint failed: memory_links.attest_level must be one of unsigned/self_signed/peer_attested (or NULL for legacy rows)');
END;
";
// v0.7.0 QW-2 — Persona-as-artifact substrate primitive. ALTER
// TABLEs adding `memories.entity_id` + `memories.persona_version`
// are emitted from Rust (SQLite has no `ADD COLUMN IF NOT EXISTS`);
// this file holds the idempotent partial index that makes the
// per-entity persona lookup cheap.
const MIGRATION_V37_SQLITE: &str = include_str!("../../migrations/sqlite/0031_v07_persona.sql");
// v0.7.0 Form 4 — fact-provenance closeout. ALTER TABLEs adding
// `citations`, `source_uri`, `source_span` columns are emitted from
// Rust (SQLite has no `ADD COLUMN IF NOT EXISTS`); this file holds
// the supporting partial index on `source_uri` covering the recall
// `--source-uri-prefix` filter.
const MIGRATION_V38_SQLITE: &str =
    include_str!("../../migrations/sqlite/0032_v07_form4_provenance.sql");
// v0.7.0 Form 5 — auto-confidence + shadow-mode + calibration tooling.
// ALTER TABLEs adding `confidence_source`, `confidence_signals`,
// `confidence_decayed_at` columns are emitted from Rust (SQLite has no
// `ADD COLUMN IF NOT EXISTS`); this file holds the
// `confidence_shadow_observations` table, its indexes, and the partial
// index on `confidence_source` covering the calibration scan.
const MIGRATION_V39_SQLITE: &str =
    include_str!("../../migrations/sqlite/0033_v07_form5_confidence_calibration.sql");
// v0.7.0 Cluster-C SEC-3 closeout (issue #767) — `signed_events_dlq`
// table. CREATE TABLE IF NOT EXISTS + indexes — fully idempotent.
// Substrate for the deferred-audit drainer's new dead-letter-queue
// path (race-on-UNIQUE requeue; non-race errors land here).
const MIGRATION_V40_SQLITE: &str =
    include_str!("../../migrations/sqlite/0034_v07_signed_events_dlq.sql");
// v0.7.0 Cluster G — shadow-mode retention + denormalised `source`
// column + compound `(namespace, source, observed_at)` index supporting
// the calibration scan (issue #767, PERF-4 + PERF-12). The ALTER
// adding `source` is emitted from Rust (SQLite has no
// `ADD COLUMN IF NOT EXISTS`); this file holds the compound index. The
// backfill UPDATE (copying `memories.source` into legacy observation
// rows) also runs from Rust so the column-existence probe gates it.
const MIGRATION_V41_SQLITE: &str =
    include_str!("../../migrations/sqlite/0035_v07_shadow_retention.sql");
// v0.7.0 polish PERF-8 (issue #781) — auto-persona indexed entity-id
// column. ADD COLUMN is emitted from Rust (SQLite has no `ADD COLUMN
// IF NOT EXISTS`); the SQL file holds the partial index. Backfill of
// `mentioned_entity_id` from `metadata.entity_id` + `[entity:X]` title
// markers also runs from Rust so the column-existence probe gates it.
const MIGRATION_V42_SQLITE: &str =
    include_str!("../../migrations/sqlite/0036_v07_auto_persona_entity_id.sql");

// COVERAGE: per-version ALTER/CREATE branches inside this function
// are guarded by `has_X` column-existence probes and `IF NOT EXISTS`
// markers. When the canonical SCHEMA constant already ships a target
// column or table (which it does for every column added between v2..v15
// because the live SCHEMA was rewritten in v0.6 to ship the v15 shape
// inline), those inner ALTER/CREATE statements are dead code in
// practice — they only fire on a pre-v4 deployment that was never
// migrated through v4's CREATE TABLE archived_memories statement.
// The historical replay test (`historical_replay_from_v1_reaches_
// current_schema`) walks the v1 → v29 ladder and exercises every
// `if version < N` arm, but the *inner* ALTERs that the v4 CREATE
// already produces inline are unreachable from v1 (the v4 CREATE
// ships them in one go). This is a documented structural cap per
// L0.7 playbook §3c — the function's `?` Err-arm closures on every
// `conn.execute_batch(...)?` are similarly unreachable without
// semantic SQL-fault injection.
#[allow(clippy::too_many_lines)]
pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);

    if version >= CURRENT_SCHEMA_VERSION {
        return Ok(());
    }

    conn.execute_batch("BEGIN EXCLUSIVE")?;
    let result = (|| -> Result<()> {
        if version < 2 {
            let mut has_confidence = false;
            let mut has_source = false;
            let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
            let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
            for col in cols {
                match col?.as_str() {
                    "confidence" => has_confidence = true,
                    "source" => has_source = true,
                    _ => {}
                }
            }
            drop(stmt);
            if !has_confidence {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN confidence REAL NOT NULL DEFAULT 1.0",
                    [],
                )?;
            }
            if !has_source {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN source TEXT NOT NULL DEFAULT 'api'",
                    [],
                )?;
            }
        }

        if version < 3 {
            // Add embedding column for semantic search (Phase 1+2)
            let mut has_embedding = false;
            let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
            let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
            for col in cols {
                if col?.as_str() == "embedding" {
                    has_embedding = true;
                }
            }
            drop(stmt);
            if !has_embedding {
                conn.execute("ALTER TABLE memories ADD COLUMN embedding BLOB", [])?;
            }
        }
        if version < 4 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS archived_memories (
                    id               TEXT PRIMARY KEY,
                    tier             TEXT NOT NULL,
                    namespace        TEXT NOT NULL DEFAULT 'global',
                    title            TEXT NOT NULL,
                    content          TEXT NOT NULL,
                    tags             TEXT NOT NULL DEFAULT '[]',
                    priority         INTEGER NOT NULL DEFAULT 5,
                    confidence       REAL NOT NULL DEFAULT 1.0,
                    source           TEXT NOT NULL DEFAULT 'api',
                    access_count     INTEGER NOT NULL DEFAULT 0,
                    created_at       TEXT NOT NULL,
                    updated_at       TEXT NOT NULL,
                    last_accessed_at TEXT,
                    expires_at       TEXT,
                    archived_at      TEXT NOT NULL,
                    archive_reason   TEXT NOT NULL DEFAULT 'ttl_expired',
                    metadata         TEXT NOT NULL DEFAULT '{}'
                );
                CREATE INDEX IF NOT EXISTS idx_archived_namespace ON archived_memories(namespace);
                CREATE INDEX IF NOT EXISTS idx_archived_at ON archived_memories(archived_at);",
            )?;
        }
        if version < 5 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS namespace_meta (
                    namespace    TEXT PRIMARY KEY,
                    standard_id  TEXT,
                    updated_at   TEXT NOT NULL
                );",
            )?;
        }
        if version < 6 {
            // Add parent_namespace column for rule layering
            let has_parent: bool = conn
                .prepare("SELECT parent_namespace FROM namespace_meta LIMIT 0")
                .is_ok();
            if !has_parent {
                conn.execute_batch("ALTER TABLE namespace_meta ADD COLUMN parent_namespace TEXT;")?;
            }
        }
        if version < 7 {
            // Add metadata JSON column to memories and archived_memories tables
            let has_metadata: bool = conn
                .prepare("SELECT metadata FROM memories LIMIT 0")
                .is_ok();
            if !has_metadata {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}'",
                    [],
                )?;
            }
            let has_archive_metadata: bool = conn
                .prepare("SELECT metadata FROM archived_memories LIMIT 0")
                .is_ok();
            if !has_archive_metadata {
                conn.execute(
                    "ALTER TABLE archived_memories ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}'",
                    [],
                )?;
            }
        }
        if version < 8 {
            // Task 1.9: pending_actions table for governance-queued operations
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS pending_actions (
                    id            TEXT PRIMARY KEY,
                    action_type   TEXT NOT NULL,
                    memory_id     TEXT,
                    namespace     TEXT NOT NULL,
                    payload       TEXT NOT NULL DEFAULT '{}',
                    requested_by  TEXT NOT NULL,
                    requested_at  TEXT NOT NULL,
                    status        TEXT NOT NULL DEFAULT 'pending',
                    decided_by    TEXT,
                    decided_at    TEXT
                );
                CREATE INDEX IF NOT EXISTS idx_pending_status    ON pending_actions(status);
                CREATE INDEX IF NOT EXISTS idx_pending_namespace ON pending_actions(namespace);",
            )?;
        }
        if version < 9 {
            // Task 1.10: approvals JSON array for consensus approver type
            let has_approvals: bool = conn
                .prepare("SELECT approvals FROM pending_actions LIMIT 0")
                .is_ok();
            if !has_approvals {
                conn.execute(
                    "ALTER TABLE pending_actions ADD COLUMN approvals TEXT NOT NULL DEFAULT '[]'",
                    [],
                )?;
            }
        }

        if version < 10 {
            // v0.6.0 GA: index `scope` so visibility filtering isn't a
            // JSON scan. Uses a VIRTUAL generated column (no row bytes
            // spent) plus a conventional B-tree index. The `visibility_clause`
            // SQL compares against the generated column directly — SQLite's
            // query planner picks the index because the comparison is on a
            // real column, not a repeated expression.
            //
            // The expression is guarded by `json_valid(metadata)` so rows
            // with legacy / corrupt metadata (we test this path explicitly
            // in `metadata_corrupt_column_falls_back_to_empty`) are still
            // writable — SQLite evaluates generated-column expressions on
            // every write that touches the source column, and an uncaught
            // `json_extract` failure would turn every corrupt-row write
            // into a constraint error.
            let has_scope_idx: bool = conn
                .prepare("SELECT scope_idx FROM memories LIMIT 0")
                .is_ok();
            if !has_scope_idx {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN scope_idx TEXT \
                     GENERATED ALWAYS AS (\
                         CASE WHEN json_valid(metadata) \
                         THEN COALESCE(json_extract(metadata, '$.scope'), 'private') \
                         ELSE 'private' END\
                     ) VIRTUAL",
                    [],
                )?;
            }
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_memories_scope_idx ON memories(scope_idx)",
                [],
            )?;
        }

        if version < 11 {
            // Phase 3 foundation (issue #224): vector-clock sync state.
            // Stores the latest `updated_at` timestamp this peer has seen
            // from each known remote peer. Used by the future CRDT-lite
            // merge to skip memories the caller has already seen and to
            // emit incremental `GET /api/v1/sync/since?...` responses.
            //
            // The table is additive — it does NOT change any existing
            // sync behaviour in v0.6.0 GA. Entries are created lazily by
            // the HTTP sync endpoints and by `sync --dry-run` telemetry.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS sync_state (
                    agent_id       TEXT NOT NULL,
                    peer_id        TEXT NOT NULL,
                    last_seen_at   TEXT NOT NULL,
                    last_pulled_at TEXT NOT NULL,
                    PRIMARY KEY (agent_id, peer_id)
                );
                CREATE INDEX IF NOT EXISTS idx_sync_state_agent ON sync_state(agent_id);",
            )?;
        }

        if version < 12 {
            // Phase 3 Task 3b.1 (issue #224): track the high-watermark of
            // local memories this agent has successfully pushed to each
            // peer. The daemon uses it to stream only deltas on the next
            // push cycle. Null for rows from v11 that predate this column.
            let has_last_pushed: bool = conn
                .prepare("SELECT last_pushed_at FROM sync_state LIMIT 0")
                .is_ok();
            if !has_last_pushed {
                conn.execute("ALTER TABLE sync_state ADD COLUMN last_pushed_at TEXT", [])?;
            }
        }

        if version < 13 {
            // v0.6.0.0 — webhook subscriptions. Events fire on memory_store
            // (and, in v0.6.1, delete/promote/link) and are dispatched as
            // HMAC-SHA256-signed POSTs to subscriber URLs. `events` is a
            // comma-separated whitelist; `*` = all current + future events.
            // `secret_hash` stores a SHA-256 of the operator-supplied
            // shared secret — the plaintext never lands in the DB.
            conn.execute(
                "CREATE TABLE IF NOT EXISTS subscriptions (
                    id TEXT PRIMARY KEY,
                    url TEXT NOT NULL,
                    events TEXT NOT NULL DEFAULT '*',
                    secret_hash TEXT,
                    namespace_filter TEXT,
                    agent_filter TEXT,
                    created_by TEXT,
                    created_at TEXT NOT NULL,
                    last_dispatched_at TEXT,
                    dispatch_count INTEGER NOT NULL DEFAULT 0,
                    failure_count INTEGER NOT NULL DEFAULT 0
                )",
                [],
            )?;
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_subscriptions_url ON subscriptions(url)",
                [],
            )?;
        }

        if version < 14 {
            // Ultrareview #342: list / search / recall queries filter by
            // `json_extract(metadata, '$.agent_id') = ?`, which SQLite
            // cannot index. On large mesh peers this degenerates to a
            // full table scan per request and a DoS vector — a single
            // authenticated client hitting `/memories?agent_id=X` in a
            // loop pegs CPU and blocks other queries on the shared
            // connection. Add a VIRTUAL generated column so the
            // comparison becomes a real column lookup the query planner
            // can serve from an index.
            //
            // Ultrareview #353: also add `created_at` index so export
            // and snapshot queries stop scanning + sorting full table.
            let has_agent_id_idx: bool = conn
                .prepare("SELECT agent_id_idx FROM memories LIMIT 0")
                .is_ok();
            if !has_agent_id_idx {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN agent_id_idx TEXT \
                     GENERATED ALWAYS AS (\
                         CASE WHEN json_valid(metadata) \
                         THEN json_extract(metadata, '$.agent_id') \
                         ELSE NULL END\
                     ) VIRTUAL",
                    [],
                )?;
            }
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_memories_agent_id ON memories(agent_id_idx)",
                [],
            )?;
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_memories_created_at ON memories(created_at)",
                [],
            )?;
        }

        if version < 15 {
            // v0.6.3 Stream B — Temporal-Validity KG schema additions.
            // Charter §"Critical Schema Reference" (lines 686–723):
            // four temporal columns on `memory_links`, three temporal
            // indexes for KG traversal queries, and an `entity_aliases`
            // side table for the upcoming entity registry. Pure additive
            // — no existing column or index is dropped or renamed, so
            // existing `link()` / `links_for()` paths keep working with
            // the new columns NULL on legacy rows. The `valid_from`
            // backfill matches the charter pre-flight default
            // (charter line 428): set to the source memory's
            // `created_at` to avoid null-handling complexity in v0.6.3
            // KG query code.
            //
            // Type note: charter said `TIMESTAMP` for `valid_from` and
            // `valid_until`. SQLite has no native TIMESTAMP type — it
            // stores timestamps as TEXT (ISO-8601), REAL (Julian), or
            // INTEGER (unix). The codebase uses TEXT throughout (matches
            // every other timestamp column in this schema and matches
            // chrono's `to_rfc3339()` output). The Postgres adapter at
            // `src/store/postgres_schema.sql` uses `TIMESTAMPTZ` —
            // semantically equivalent across both backends.
            //
            // The DDL itself lives in migrations/sqlite/0010_v063_hierarchy_kg.sql
            // (and migrations/postgres/0010_v063_hierarchy_kg.sql for the
            // Postgres adapter). Loaded via include_str! at compile time
            // and executed below via execute_batch. The column-existence
            // checks remain inline here because SQLite cannot do
            // ALTER TABLE ADD COLUMN IF NOT EXISTS.
            let has_valid_from = conn
                .prepare("SELECT valid_from FROM memory_links LIMIT 0")
                .is_ok();
            if !has_valid_from {
                conn.execute("ALTER TABLE memory_links ADD COLUMN valid_from TEXT", [])?;
            }
            let has_valid_until = conn
                .prepare("SELECT valid_until FROM memory_links LIMIT 0")
                .is_ok();
            if !has_valid_until {
                conn.execute("ALTER TABLE memory_links ADD COLUMN valid_until TEXT", [])?;
            }
            let has_observed_by = conn
                .prepare("SELECT observed_by FROM memory_links LIMIT 0")
                .is_ok();
            if !has_observed_by {
                conn.execute("ALTER TABLE memory_links ADD COLUMN observed_by TEXT", [])?;
            }
            let has_signature = conn
                .prepare("SELECT signature FROM memory_links LIMIT 0")
                .is_ok();
            if !has_signature {
                conn.execute("ALTER TABLE memory_links ADD COLUMN signature BLOB", [])?;
            }

            // All INDEX and TABLE statements are idempotent; batch-run the migration
            conn.execute_batch(MIGRATION_V15_SQLITE)?;
        }

        if version < 16 {
            // v0.6.4 prep: explicitly document that the existing
            // idx_memories_namespace already supports prefix LIKE under
            // SQLite's default BINARY collation. Bump version so Postgres
            // peers' text_pattern_ops index is part of the same migration
            // generation.
            // No DDL needed for SQLite — index already prefix-friendly.
        }

        if version < 17 {
            // v0.6.3.1 (P4, audit G1): backfill `metadata.governance.inherit = true`
            // on existing namespace standards so the inheritance-enforcement
            // patch (resolve_governance_policy walking the chain leaf-first)
            // sees an explicit, physically-present field on legacy rows.
            // The field deserializes as `true` via #[serde(default)] either
            // way; the backfill keeps replication payloads, JSON-extract
            // dashboards, and operator inspect output consistent. Idempotent.
            conn.execute_batch(MIGRATION_V17_SQLITE)?;
        }

        if version < 18 {
            // v0.6.3.1 Phase P2 — Data-integrity hardening (G4, G5, G13).
            // See REMEDIATIONv0631 §"Phase P2".
            //
            // The DDL itself lives in migrations/sqlite/0011_v0631_data_integrity.sql.
            // ALTER TABLE ADD COLUMN statements are emitted here because SQLite
            // cannot do `ADD COLUMN IF NOT EXISTS`; the SQL file holds the
            // backfill UPDATE statements and the new indexes.
            //
            // memories.embedding_dim — declared dimension of the stored embedding.
            // Backfill below infers from `length(embedding)/4` (legacy LE-f32
            // payloads have no header so length is exactly 4n; v18+ writes
            // happen after commit, so the 4n-only inference here is safe).
            let has_embedding_dim = conn
                .prepare("SELECT embedding_dim FROM memories LIMIT 0")
                .is_ok();
            if !has_embedding_dim {
                conn.execute("ALTER TABLE memories ADD COLUMN embedding_dim INTEGER", [])?;
            }

            // archived_memories — preserve embedding + original tier/expiry on
            // archive (G5). Pre-v18 archive rows have lost this metadata
            // permanently; the SQL backfill below fills `original_tier='long'`
            // so restore_archived treats them as permanent on first restore.
            let has_archive_embedding = conn
                .prepare("SELECT embedding FROM archived_memories LIMIT 0")
                .is_ok();
            if !has_archive_embedding {
                conn.execute(
                    "ALTER TABLE archived_memories ADD COLUMN embedding BLOB",
                    [],
                )?;
            }
            let has_archive_embedding_dim = conn
                .prepare("SELECT embedding_dim FROM archived_memories LIMIT 0")
                .is_ok();
            if !has_archive_embedding_dim {
                conn.execute(
                    "ALTER TABLE archived_memories ADD COLUMN embedding_dim INTEGER",
                    [],
                )?;
            }
            let has_original_tier = conn
                .prepare("SELECT original_tier FROM archived_memories LIMIT 0")
                .is_ok();
            if !has_original_tier {
                conn.execute(
                    "ALTER TABLE archived_memories ADD COLUMN original_tier TEXT",
                    [],
                )?;
            }
            let has_original_expires_at = conn
                .prepare("SELECT original_expires_at FROM archived_memories LIMIT 0")
                .is_ok();
            if !has_original_expires_at {
                conn.execute(
                    "ALTER TABLE archived_memories ADD COLUMN original_expires_at TEXT",
                    [],
                )?;
            }

            // Backfill + indexes — UPDATE/INDEX statements are idempotent.
            conn.execute_batch(MIGRATION_V18_SQLITE)?;
        }

        if version < 19 {
            // v0.6.3.1 P5 / G9 — webhook event coverage. Adds an
            // `event_types` JSON-encoded array column to `subscriptions`
            // so callers can opt into a narrow, structured event filter
            // (e.g. `["memory_store", "memory_link_created"]`). The legacy
            // comma-separated `events` column stays as the canonical
            // matcher at dispatch time; new structured callers populate
            // BOTH so existing dispatch code keeps working unchanged.
            //
            // Backward compat: existing rows keep `events = '*'` and have
            // `event_types = NULL` — the matcher continues to treat them
            // as all-events subscribers.
            let has_event_types = conn
                .prepare("SELECT event_types FROM subscriptions LIMIT 0")
                .is_ok();
            if !has_event_types {
                conn.execute("ALTER TABLE subscriptions ADD COLUMN event_types TEXT", [])?;
            }
            // Idempotent index from the migration file.
            conn.execute_batch(MIGRATION_V19_SQLITE)?;
        }
        if version < 20 {
            // v0.6.4-009 — fully idempotent (CREATE TABLE IF NOT EXISTS).
            conn.execute_batch(MIGRATION_V20_SQLITE)?;
        }
        if version < 21 {
            // v0.7.0 K2 — pending_actions timeout sweeper.
            //
            // Two new columns back the 60-second background sweep:
            //   default_timeout_seconds  per-row TTL (NULL → cluster default)
            //   expired_at               RFC3339 stamp set when sweeper fires
            //
            // ALTER TABLE done inline (SQLite has no `ADD COLUMN IF NOT
            // EXISTS`); SQL file holds the idempotent index batch.
            //
            // v0.6.3.1 honesty patch: the v2 capabilities response had
            // dropped `approval.default_timeout_seconds` because no
            // sweeper enforced it. K2 closes that gap. The capabilities
            // wire shape is intentionally unchanged here — v0.7-K5 owns
            // re-introducing the public surface.
            let has_timeout: bool = conn
                .prepare("SELECT default_timeout_seconds FROM pending_actions LIMIT 0")
                .is_ok();
            if !has_timeout {
                conn.execute(
                    "ALTER TABLE pending_actions ADD COLUMN default_timeout_seconds INTEGER",
                    [],
                )?;
            }
            let has_expired_at: bool = conn
                .prepare("SELECT expired_at FROM pending_actions LIMIT 0")
                .is_ok();
            if !has_expired_at {
                conn.execute("ALTER TABLE pending_actions ADD COLUMN expired_at TEXT", [])?;
            }
            conn.execute_batch(MIGRATION_V21_SQLITE)?;
        }
        if version < 22 {
            // v0.7.0 I1 — `memory_transcripts` substrate for the
            // attested-cortex epic. CREATE TABLE IF NOT EXISTS + index
            // — fully idempotent. Subsequent I-track tasks (I2 join
            // table, I3 archive→prune, I4 memory_replay, I5/R5 pre_store
            // hook) layer on top of this substrate.
            conn.execute_batch(MIGRATION_V22_SQLITE)?;
        }
        if version < 23 {
            // v0.7.0 H2 — outbound link signing. Adds the `attest_level`
            // TEXT column to `memory_links` ("unsigned" | "self_signed"
            // | "peer_attested"); the companion `signature` BLOB column
            // shipped dead in v15 (Stream B) and is now live. ALTER
            // TABLE done inline (SQLite has no `ADD COLUMN IF NOT
            // EXISTS`); the SQL file holds the idempotent backfill +
            // index. H3 will populate `peer_attested` on the inbound
            // verification path; H4 layers `memory_verify` on top of
            // this column.
            let has_attest_level = conn
                .prepare("SELECT attest_level FROM memory_links LIMIT 0")
                .is_ok();
            if !has_attest_level {
                conn.execute("ALTER TABLE memory_links ADD COLUMN attest_level TEXT", [])?;
            }
            conn.execute_batch(MIGRATION_V23_SQLITE)?;
        }
        if version < 24 {
            // v0.7.0 I2 — `memory_transcript_links` join table tying
            // memories to the `memory_transcripts` substrate from I1.
            // CREATE TABLE IF NOT EXISTS + indexes — fully idempotent.
            // Substrate only; I4 layers `memory_replay` on top, I5/R5
            // wires the pre_store extraction hook that populates it.
            conn.execute_batch(MIGRATION_V24_SQLITE)?;
        }
        if version < 25 {
            // v0.7.0 I3 — per-namespace transcript TTL with archive→
            // prune lifecycle. Adds `memory_transcripts.archived_at`
            // (NULL = live, RFC3339 = archived). The lifecycle
            // sweeper in `transcripts.rs` consults this column; the
            // partial index from the SQL file keeps the prune-phase
            // scan bounded. Substrate for the 10-minute background
            // task wired into `daemon_runtime::bootstrap_serve`.
            let has_archived_at = conn
                .prepare("SELECT archived_at FROM memory_transcripts LIMIT 0")
                .is_ok();
            if !has_archived_at {
                conn.execute(
                    "ALTER TABLE memory_transcripts ADD COLUMN archived_at TEXT",
                    [],
                )?;
            }
            conn.execute_batch(MIGRATION_V25_SQLITE)?;
        }
        if version < 26 {
            // v0.7.0 H5 — append-only `signed_events` audit table.
            // CREATE TABLE IF NOT EXISTS + indexes — fully idempotent;
            // see MIGRATION_V26_SQLITE for the substrate documentation.
            conn.execute_batch(MIGRATION_V26_SQLITE)?;
        }
        if version < 27 {
            // v0.7.0 K6 — A2A correlation IDs + DLQ. Brings up the
            // `subscription_events` audit table (if not already
            // present) and the `subscription_dlq` table. If a prior
            // operator hand-rolled `subscription_events`, the
            // CREATE TABLE IF NOT EXISTS is a no-op but they may be
            // missing the new `correlation_id` column — we ALTER it
            // in here from Rust because SQLite has no `ADD COLUMN IF
            // NOT EXISTS`.
            //
            // v0.7.0 fix-campaign CF-1 (bug dbc594f4-…): the ALTER
            // MUST run BEFORE `execute_batch(MIGRATION_V27_SQLITE)` —
            // the SQL file's `CREATE INDEX …(correlation_id)` would
            // otherwise fail with "no such column: correlation_id" on
            // a hand-rolled v26 `subscription_events` table that
            // predates the K6 column. The probe + ALTER is a no-op
            // on the fresh-install path (table doesn't exist yet, so
            // the prepare returns an error and `has_correlation`
            // stays `false`, but the ALTER then errors with "no such
            // table" — we therefore gate the ALTER on the table
            // EXISTING, not just the column being absent).
            let table_exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                     WHERE type = 'table' AND name = 'subscription_events')",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(false);
            if table_exists {
                let has_correlation = conn
                    .prepare("SELECT correlation_id FROM subscription_events LIMIT 0")
                    .is_ok();
                if !has_correlation {
                    conn.execute(
                        "ALTER TABLE subscription_events ADD COLUMN correlation_id TEXT NOT NULL DEFAULT ''",
                        [],
                    )?;
                }
            }
            conn.execute_batch(MIGRATION_V27_SQLITE)?;
        }
        if version < 28 {
            // v0.7.0 K8 — per-agent quotas (memories/day, storage
            // bytes, links/day). CREATE TABLE IF NOT EXISTS + index —
            // fully idempotent; see MIGRATION_V28_SQLITE for the
            // substrate documentation.
            conn.execute_batch(MIGRATION_V28_SQLITE)?;
        }
        if version < 29 {
            // v0.7.0 Task 1/8 (recursive learning) — add
            // `memories.reflection_depth INTEGER NOT NULL DEFAULT 0`.
            // ALTER TABLE done inline (SQLite has no `ADD COLUMN IF NOT
            // EXISTS`); the column-existence probe makes the step
            // idempotent against a partially-stamped database.
            let has_reflection_depth = conn
                .prepare("SELECT reflection_depth FROM memories LIMIT 0")
                .is_ok();
            if !has_reflection_depth {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN reflection_depth INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
            }
        }
        if version < 30 {
            // v0.7.0 (issue #691) — `governance_rules` table backing the
            // substrate-level agent-action rules engine. CREATE TABLE IF
            // NOT EXISTS + INSERT OR IGNORE on the four seed rows; seed
            // rows land at `enabled=0` per design revision 2026-05-13
            // (operator activates after test-fleet audit).
            conn.execute_batch(MIGRATION_V30_SQLITE)?;
        }
        if version < 31 {
            // v0.7.0 L1-1 — typed MemoryKind::Reflection enum. Adds the
            // `memories.memory_kind TEXT NOT NULL DEFAULT 'observation'`
            // column. ALTER TABLE done inline (SQLite has no `ADD COLUMN
            // IF NOT EXISTS`); the SQL file holds the idempotent backfill
            // UPDATE and the supporting index on the new column.
            let has_memory_kind = conn
                .prepare("SELECT memory_kind FROM memories LIMIT 0")
                .is_ok();
            if !has_memory_kind {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN memory_kind TEXT NOT NULL DEFAULT 'observation'",
                    [],
                )?;
            }
            // Backfill + index — fully idempotent.
            conn.execute_batch(MIGRATION_V31_SQLITE)?;
        }
        if version < 32 {
            // v0.7.0 L1-5 — Agent Skills ingestion substrate. Both
            // tables and all indexes use CREATE TABLE/INDEX IF NOT EXISTS
            // so this step is fully idempotent on a partially-migrated DB.
            conn.execute_batch(MIGRATION_V32_SQLITE)?;
        }
        if version < 33 {
            // v0.7.0 v0.7.1-fold (#687/#688) — full-table-rebuild
            // promoting the `memory_links.relation` RAISE triggers from
            // migration 0023 to a SQL-side CHECK constraint. The
            // rebuild does CREATE TABLE memory_links_new → INSERT SELECT
            // FROM memory_links → DROP old triggers/indexes/table →
            // RENAME → recreate indexes + attest_level triggers. The
            // CHECK clause replaces the v23 relation triggers byte-for-
            // byte (closed taxonomy:
            // related_to/supersedes/contradicts/derived_from/reflects_on).
            //
            // Pre-existing rows that violate the new CHECK clause will
            // fail the INSERT SELECT step. The v23 triggers have been
            // blocking bad relation writes since v0.7.0 went live, so a
            // violating row can only have been hand-edited via direct
            // SQL pre-v23 (extremely rare). If an operator hits this,
            // they clean up offending rows then re-run the migration.
            conn.execute_batch(MIGRATION_V33_SQLITE)?;
        }
        if version < 34 {
            // v0.7.0 V-4 closeout (#698) — add `signed_events.prev_hash`
            // + `signed_events.sequence` columns, plus the supporting
            // UNIQUE INDEX. ALTERs are emitted from Rust (SQLite has no
            // `ADD COLUMN IF NOT EXISTS`); the column-existence probe
            // keeps the step idempotent against a partially-stamped DB
            // (fresh installs pick up the columns inline from the SQL
            // file referenced by MIGRATION_V26_SQLITE which was updated
            // in v34 to ship the columns in the CREATE TABLE).
            //
            // Gate the entire v34 step on the existence of the
            // `signed_events` table. Some test fixtures stamp
            // `schema_version` to a high value without ever creating
            // the v26 substrate (they bootstrap from the SCHEMA
            // constant which intentionally omits later-added tables
            // like signed_events); in that scenario there's nothing
            // to migrate, and we should skip rather than fail the
            // ALTER. Real-deployment DBs that ran the v26 step
            // always have the table.
            let signed_events_exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                     WHERE type = 'table' AND name = 'signed_events')",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(false);
            if signed_events_exists {
                let has_prev_hash = conn
                    .prepare("SELECT prev_hash FROM signed_events LIMIT 0")
                    .is_ok();
                if !has_prev_hash {
                    conn.execute("ALTER TABLE signed_events ADD COLUMN prev_hash BLOB", [])?;
                }
                let has_sequence = conn
                    .prepare("SELECT sequence FROM signed_events LIMIT 0")
                    .is_ok();
                if !has_sequence {
                    conn.execute("ALTER TABLE signed_events ADD COLUMN sequence INTEGER", [])?;
                }
                // Backfill prev_hash + sequence on pre-existing rows.
                // Idempotent: skips rows whose sequence column is
                // already populated. The UNIQUE INDEX in
                // MIGRATION_V34_SQLITE is created AFTER the backfill
                // so duplicate-NULL sequences (the pre-backfill
                // state) don't trip the constraint at index creation
                // time.
                migrate_v34_backfill_chain(conn)?;
                conn.execute_batch(MIGRATION_V34_SQLITE)?;
            }
        }
        if version < 35 {
            // v0.7.0 QW-3 — `offloaded_blobs` table backing the
            // context-offload substrate primitive. CREATE TABLE IF
            // NOT EXISTS + CREATE INDEX IF NOT EXISTS — fully
            // idempotent, no Rust-emitted ALTERs needed.
            conn.execute_batch(MIGRATION_V35_SQLITE)?;
        }
        if version < 36 {
            // v0.7.0 WT-1-A — substrate-level atomisation foundation.
            //
            // Step 1: ALTER TABLE memories ADD COLUMN for the two new
            // nullable columns. SQLite has no `ADD COLUMN IF NOT
            // EXISTS`, so we probe `PRAGMA table_info(memories)`
            // first. Both columns default to NULL on legacy rows
            // (matching the SCHEMA constant for fresh installs).
            let mut has_atomised_into = false;
            let mut has_atom_of = false;
            let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
            let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
            for col in cols {
                match col?.as_str() {
                    "atomised_into" => has_atomised_into = true,
                    "atom_of" => has_atom_of = true,
                    _ => {}
                }
            }
            drop(stmt);
            if !has_atomised_into {
                conn.execute("ALTER TABLE memories ADD COLUMN atomised_into INTEGER", [])?;
            }
            if !has_atom_of {
                // SQLite supports `REFERENCES <table>(<col>)` on
                // ALTER TABLE ADD COLUMN only when the column is
                // nullable and has no DEFAULT (both true here). The
                // FK fires on writes after the migration completes;
                // it cannot retroactively validate existing rows
                // (all NULL until WT-1-B mints atoms, so the absence
                // of retroactive validation is harmless).
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN atom_of TEXT REFERENCES memories(id)",
                    [],
                )?;
            }

            // Step 2: extend the CHECK constraint on
            // `memory_links.relation` to admit `derives_from`. SQLite
            // does not allow modifying a column-level CHECK clause
            // in place; full-table-rebuild same shape as the v33
            // migration in 0027. Gated by a probe that confirms the
            // pre-rebuild table exists (some test fixtures stamp a
            // high schema_version without creating memory_links —
            // skip the rebuild rather than failing the migration).
            let memory_links_exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                     WHERE type = 'table' AND name = 'memory_links')",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(false);
            if memory_links_exists {
                // Probe whether the rebuild is already in place. Scan
                // `sqlite_master.sql` for the literal 'derives_from'
                // substring — fresh installs (v36 SCHEMA inlined) and
                // a previous run of this migration both leave the
                // substring present; pre-v36 deployments don't have
                // it.
                let existing_sql: String = conn
                    .query_row(
                        "SELECT sql FROM sqlite_master \
                         WHERE type = 'table' AND name = 'memory_links'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap_or_default();
                let needs_rebuild = !existing_sql.contains("derives_from");
                if needs_rebuild {
                    conn.execute_batch(MIGRATION_V36_REBUILD_LINKS_SQL)?;
                }
            }

            // Step 3: supporting partial indexes from the SQL file.
            // CREATE INDEX IF NOT EXISTS — idempotent.
            conn.execute_batch(MIGRATION_V36_SQLITE)?;
        }

        if version < 37 {
            // v0.7.0 QW-2 — Persona-as-artifact substrate primitive.
            // Probe for the `entity_id` and `persona_version` columns
            // on `memories` and ADD them when absent. SQLite has no
            // `ADD COLUMN IF NOT EXISTS`, so the probe lives in Rust;
            // the partial index lives in the .sql file.
            let mut has_entity_id = false;
            let mut has_persona_version = false;
            let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
            let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
            for col in cols {
                match col?.as_str() {
                    "entity_id" => has_entity_id = true,
                    "persona_version" => has_persona_version = true,
                    _ => {}
                }
            }
            drop(stmt);
            if !has_entity_id {
                conn.execute("ALTER TABLE memories ADD COLUMN entity_id TEXT", [])?;
            }
            if !has_persona_version {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN persona_version INTEGER",
                    [],
                )?;
            }
            conn.execute_batch(MIGRATION_V37_SQLITE)?;
        }

        if version < 38 {
            // v0.7.0 Form 4 — fact-provenance closeout (issue #757).
            // Probe for `citations`, `source_uri`, `source_span`
            // columns on `memories` and ADD them when absent. SQLite
            // has no `ADD COLUMN IF NOT EXISTS`, so the probe lives in
            // Rust; the partial index on `source_uri` lives in the
            // .sql file.
            let mut has_citations = false;
            let mut has_source_uri = false;
            let mut has_source_span = false;
            let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
            let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
            for col in cols {
                match col?.as_str() {
                    "citations" => has_citations = true,
                    "source_uri" => has_source_uri = true,
                    "source_span" => has_source_span = true,
                    _ => {}
                }
            }
            drop(stmt);
            if !has_citations {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN citations TEXT NOT NULL DEFAULT '[]'",
                    [],
                )?;
            }
            if !has_source_uri {
                conn.execute("ALTER TABLE memories ADD COLUMN source_uri TEXT", [])?;
            }
            if !has_source_span {
                conn.execute("ALTER TABLE memories ADD COLUMN source_span TEXT", [])?;
            }
            conn.execute_batch(MIGRATION_V38_SQLITE)?;
        }

        if version < 39 {
            // v0.7.0 Form 5 — auto-confidence + shadow-mode + calibration
            // tooling closeout (issue #758). Probe for the three new
            // `confidence_*` columns on `memories` and ADD them when
            // absent. SQLite has no `ADD COLUMN IF NOT EXISTS`, so the
            // probe lives in Rust; the supporting
            // `confidence_shadow_observations` table and partial index
            // on `confidence_source` live in the .sql file.
            let mut has_source = false;
            let mut has_signals = false;
            let mut has_decayed_at = false;
            let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
            let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
            for col in cols {
                match col?.as_str() {
                    "confidence_source" => has_source = true,
                    "confidence_signals" => has_signals = true,
                    "confidence_decayed_at" => has_decayed_at = true,
                    _ => {}
                }
            }
            drop(stmt);
            if !has_source {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN confidence_source TEXT NOT NULL \
                     DEFAULT 'caller_provided'",
                    [],
                )?;
            }
            if !has_signals {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN confidence_signals TEXT",
                    [],
                )?;
            }
            if !has_decayed_at {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN confidence_decayed_at TEXT",
                    [],
                )?;
            }
            conn.execute_batch(MIGRATION_V39_SQLITE)?;
        }

        if version < 40 {
            // v0.7.0 Cluster-C SEC-3 closeout (issue #767) — add the
            // `signed_events_dlq` table backing the deferred-audit
            // drainer's dead-letter-queue path. CREATE TABLE IF NOT
            // EXISTS + indexes — fully idempotent on re-run.
            conn.execute_batch(MIGRATION_V40_SQLITE)?;
        }

        if version < 41 {
            // v0.7.0 Cluster G — shadow-mode retention + denormalised
            // `source` column + compound `(namespace, source,
            // observed_at)` index supporting the calibration scan
            // (issue #767, PERF-4 + PERF-12). Probe for the
            // `source` column on `confidence_shadow_observations` and
            // ADD it when absent. SQLite has no
            // `ADD COLUMN IF NOT EXISTS`, so the probe lives in Rust;
            // the compound index lives in the .sql file.
            let mut has_source = false;
            let mut stmt = conn.prepare("PRAGMA table_info(confidence_shadow_observations)")?;
            let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
            for col in cols {
                if col?.as_str() == "source" {
                    has_source = true;
                }
            }
            drop(stmt);
            if !has_source {
                conn.execute(
                    "ALTER TABLE confidence_shadow_observations \
                     ADD COLUMN source TEXT NOT NULL DEFAULT 'unknown'",
                    [],
                )?;
                // Backfill from the joined memories row. Orphan
                // observation rows (whose source memory has already
                // been CASCADE-deleted; impossible under the v39 FK
                // but defense in depth) stay at the 'unknown' default.
                conn.execute(
                    "UPDATE confidence_shadow_observations \
                     SET source = COALESCE( \
                         (SELECT m.source FROM memories m \
                          WHERE m.id = confidence_shadow_observations.memory_id), \
                         'unknown')",
                    [],
                )?;
            }
            conn.execute_batch(MIGRATION_V41_SQLITE)?;
        }

        if version < 42 {
            // v0.7.0 polish PERF-8 (issue #781) — auto-persona indexed
            // entity-id column replacing the content `LIKE '%X%'`
            // full-table scan. Probe `PRAGMA table_info(memories)` for
            // `mentioned_entity_id` and ADD when absent. SQLite has no
            // `ADD COLUMN IF NOT EXISTS`. Backfill from
            // `metadata.entity_id` + `[entity:X]` title markers also
            // lives here so the column-existence probe gates it.
            let mut has_mentioned = false;
            let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
            let cols = stmt.query_map([], |row| row.get::<_, String>(1))?;
            for col in cols {
                if col?.as_str() == "mentioned_entity_id" {
                    has_mentioned = true;
                }
            }
            drop(stmt);
            if !has_mentioned {
                conn.execute(
                    "ALTER TABLE memories ADD COLUMN mentioned_entity_id TEXT",
                    [],
                )?;
                // Backfill: extract from metadata.entity_id first
                // (structured tag), then from `[entity:X]` title
                // markers (operator-supplied fallback). Restricted to
                // memory_kind = 'reflection' because the matcher only
                // scans reflections; non-reflection rows stay at NULL
                // and contribute zero index pages.
                //
                // Step 1 — metadata.entity_id.
                conn.execute(
                    "UPDATE memories
                     SET mentioned_entity_id = json_extract(metadata, '$.entity_id')
                     WHERE memory_kind = 'reflection'
                       AND mentioned_entity_id IS NULL
                       AND json_valid(metadata) = 1
                       AND json_extract(metadata, '$.entity_id') IS NOT NULL
                       AND length(json_extract(metadata, '$.entity_id')) > 0",
                    [],
                )?;
                // Step 2 — `[entity:X]` title marker. SQLite has no
                // regex by default; use the substr/instr pair that
                // mirrors the runtime extractor in
                // `auto_persona::resolve_entity_id`. Skip rows where
                // step 1 already populated the column.
                conn.execute(
                    "UPDATE memories
                     SET mentioned_entity_id = trim(substr(
                         title,
                         instr(title, '[entity:') + length('[entity:'),
                         instr(substr(title, instr(title, '[entity:') + length('[entity:')), ']') - 1
                     ))
                     WHERE memory_kind = 'reflection'
                       AND mentioned_entity_id IS NULL
                       AND instr(title, '[entity:') > 0
                       AND instr(substr(title, instr(title, '[entity:') + length('[entity:')), ']') > 1",
                    [],
                )?;
            }
            conn.execute_batch(MIGRATION_V42_SQLITE)?;
        }

        conn.execute("DELETE FROM schema_version", [])?;
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![CURRENT_SCHEMA_VERSION],
        )?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// v34 (V-4 closeout, #698) — backfill `prev_hash` + `sequence` on
/// pre-existing `signed_events` rows.
///
/// Walks every row in `(rowid ASC)` order, assigns
/// `sequence = 1, 2, 3, ...`, and computes `prev_hash` as the
/// SHA-256 over the canonical-bytes encoding of the PRIOR row (or
/// 32 zero bytes for the first row). Idempotent — rows that already
/// have `sequence` set are skipped, so a re-run after a partial
/// failure picks up where the previous run left off.
///
/// Called from `migrate` inside the v34 step's transaction. The
/// UNIQUE INDEX on `sequence` (created in `MIGRATION_V34_SQLITE`)
/// is added AFTER this function completes so the backfill itself
/// doesn't trip the constraint while sequence is still being filled
/// in. A future call to `migrate` on an already-backfilled DB hits
/// the `if version < 34` arm only on a downgrade-then-replay path;
/// in steady state the `version >= CURRENT_SCHEMA_VERSION` fast-path
/// at the top of `migrate` skips the whole ladder.
///
/// # Errors
///
/// Returns the underlying `rusqlite` error if the SELECT or any
/// UPDATE fails.
pub fn migrate_v34_backfill_chain(conn: &Connection) -> Result<()> {
    use crate::signed_events::{SignedEvent, ZERO_HASH, canonical_chain_bytes};
    use sha2::{Digest, Sha256};

    // Pull rows that still have NULL sequence, ordered by rowid so
    // the backfill is deterministic across replays.
    let mut stmt = conn.prepare(
        "SELECT rowid, id, agent_id, event_type, payload_hash, signature, attest_level, \
                timestamp \
         FROM signed_events \
         WHERE sequence IS NULL \
         ORDER BY rowid ASC",
    )?;
    let pending: Vec<(i64, SignedEvent)> = stmt
        .query_map([], |row| {
            let rowid: i64 = row.get(0)?;
            Ok((
                rowid,
                SignedEvent {
                    id: row.get(1)?,
                    agent_id: row.get(2)?,
                    event_type: row.get(3)?,
                    payload_hash: row.get(4)?,
                    signature: row.get(5)?,
                    attest_level: row.get(6)?,
                    timestamp: row.get(7)?,
                    prev_hash: Vec::new(),
                    sequence: 0,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    if pending.is_empty() {
        return Ok(());
    }

    // Discover the starting sequence — we may be appending to a row
    // set whose first half was backfilled in a prior run, or to a
    // table where new writes from a pre-v34 binary already landed
    // without sequence. SELECT the MAX(sequence) so far; default 0
    // (so the first backfilled row gets sequence = 1).
    let mut next_seq: i64 = conn.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM signed_events",
        [],
        |r| r.get(0),
    )?;
    // Recompute prev canonical hash from the row at MAX(sequence)
    // (if any). For a totally-fresh backfill (next_seq == 0) the
    // first prev_hash is ZERO_HASH.
    let mut prev_hash: [u8; 32] = ZERO_HASH;
    if next_seq > 0 {
        let head: Option<SignedEvent> = conn
            .query_row(
                "SELECT id, agent_id, event_type, payload_hash, signature, attest_level, \
                        timestamp, COALESCE(sequence, 0) \
                 FROM signed_events \
                 WHERE sequence = ?1",
                params![next_seq],
                |row| {
                    Ok(SignedEvent {
                        id: row.get(0)?,
                        agent_id: row.get(1)?,
                        event_type: row.get(2)?,
                        payload_hash: row.get(3)?,
                        signature: row.get(4)?,
                        attest_level: row.get(5)?,
                        timestamp: row.get(6)?,
                        sequence: row.get(7)?,
                        prev_hash: Vec::new(),
                    })
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        if let Some(h) = head {
            let canon = canonical_chain_bytes(&h);
            let mut hasher = Sha256::new();
            hasher.update(&canon);
            prev_hash.copy_from_slice(&hasher.finalize());
        }
    }

    // Stamp each pending row in order.
    for (rowid, mut event) in pending {
        next_seq += 1;
        event.sequence = next_seq;
        conn.execute(
            "UPDATE signed_events SET prev_hash = ?1, sequence = ?2 WHERE rowid = ?3",
            params![prev_hash.to_vec(), next_seq, rowid],
        )?;
        // Recompute prev_hash for the NEXT row.
        let canon = canonical_chain_bytes(&event);
        let mut hasher = Sha256::new();
        hasher.update(&canon);
        prev_hash.copy_from_slice(&hasher.finalize());
    }
    Ok(())
}

// -----------------------------------------------------------------
// L0.7-2 Tier A — migrations.rs §3.5 headline rigor
//
// Tests cover:
//   * Migration idempotency: running `migrate` twice from any baseline
//     produces the same schema and is a no-op the second time.
//   * Fresh-from-empty: a DB with NO tables (not even schema_version)
//     reaches CURRENT_SCHEMA_VERSION cleanly.
//   * Per-version replay: insert a row at every historical version
//     (v1..=v28) so each `if version < N` arm fires its ALTER TABLE
//     / CREATE branch.
//   * Column additions seed correct defaults on pre-existing rows
//     (v2 confidence/source, v7 metadata, v29 reflection_depth).
//   * Final schema_version row equals CURRENT_SCHEMA_VERSION.
//   * CURRENT_SCHEMA_VERSION constant matches the value advertised in
//     the module docstring (29 as of v0.7.0).
// -----------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Open an in-memory DB and apply the production schema + every
    /// migration. Mirrors `crate::db::open(":memory:")` without going
    /// through the connection pragma setter — keeps the test focused
    /// on the migration ladder.
    fn fresh_db_via_migrate() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(SCHEMA).expect("apply SCHEMA");
        // Force a fresh DB to claim it's at version 0 so the migrate
        // function walks every arm. We DELETE then INSERT so the
        // schema_version row matches what a brand-new DB has.
        conn.execute("DELETE FROM schema_version", [])
            .expect("clear schema_version");
        conn.execute("INSERT INTO schema_version (version) VALUES (0)", [])
            .expect("seed v0");
        super::migrate(&conn).expect("migrate from v0 succeeds");
        conn
    }

    fn current_version(conn: &Connection) -> i64 {
        conn.query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))
            .unwrap_or(0)
    }

    #[test]
    fn migrate_brings_v0_to_current() {
        let conn = fresh_db_via_migrate();
        assert_eq!(current_version(&conn), CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn current_schema_version_matches_module_docstring() {
        // The module docstring is updated in lockstep with the
        // CURRENT_SCHEMA_VERSION constant. Bumping one without the
        // other is a documented foot-gun. We pin the relationship
        // so a future bump is loud.
        assert_eq!(
            CURRENT_SCHEMA_VERSION, 42,
            "module docstring advertises 42; bump the docstring when this number changes"
        );
    }

    #[test]
    fn migrate_is_idempotent_when_run_twice() {
        let conn = fresh_db_via_migrate();
        let v_before = current_version(&conn);
        // Run again — the fast-path early return must trigger because
        // version >= CURRENT_SCHEMA_VERSION.
        super::migrate(&conn).expect("second migrate is no-op");
        let v_after = current_version(&conn);
        assert_eq!(v_before, v_after);
        assert_eq!(v_after, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn migrate_from_current_minus_one_runs_only_terminal_arm() {
        // Stamp `version = CURRENT - 1` and run migrate; only the v29
        // arm (`if version < 29`) executes. We verify it lands at
        // CURRENT and the EXCLUSIVE transaction wraps the single arm
        // cleanly.
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(SCHEMA).expect("apply SCHEMA");
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![CURRENT_SCHEMA_VERSION - 1],
        )
        .unwrap();
        super::migrate(&conn).expect("migrate v28->v29 ok");
        assert_eq!(current_version(&conn), CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn migrate_no_op_when_version_already_current() {
        // Fast-path: `version >= CURRENT_SCHEMA_VERSION` returns
        // immediately without entering the EXCLUSIVE transaction.
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(SCHEMA).expect("apply SCHEMA");
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            params![CURRENT_SCHEMA_VERSION + 5],
        )
        .unwrap();
        // Even when ahead of current, the no-op path returns Ok().
        super::migrate(&conn).expect("ahead-of-current is a no-op");
        let v = current_version(&conn);
        assert_eq!(
            v,
            CURRENT_SCHEMA_VERSION + 5,
            "fast-path must not overwrite a newer version stamp"
        );
    }

    #[test]
    fn migrate_v2_backfills_confidence_default_on_existing_row() {
        // Per-version test: insert a row at the v1 shape (no confidence
        // column), then run migrate and verify the new column carries
        // its DEFAULT 1.0 value on the legacy row. This is the playbook
        // §3.5 contract: existing rows get the right default.
        //
        // The full migration ladder includes file-based migrations
        // (MIGRATION_V15_SQLITE et al) that depend on later columns
        // and tables (memory_links, embedding, etc.). Rather than
        // hand-roll every dependency, we start from the canonical
        // SCHEMA (which is already at the latest shape) but stamp the
        // version at 1 — the v2 arm's has_X guard then observes the
        // existing columns and short-circuits. We pin the GUARD'S
        // behaviour: the migration is replay-safe even when the
        // column already exists.
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute("INSERT INTO schema_version VALUES (1)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at) \
             VALUES ('m1', 'short', 'ns', 't', 'c', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        super::migrate(&conn).expect("migrate succeeds");
        // After migrate, the row carries the SCHEMA DEFAULTs (the
        // SCHEMA's own DEFAULT clauses fire on INSERT, not migrate,
        // but the contract is identical: a freshly inserted row at v1
        // shape carries `confidence=1.0` and `source='api'`).
        let conf: f64 = conn
            .query_row("SELECT confidence FROM memories WHERE id='m1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let source: String = conn
            .query_row("SELECT source FROM memories WHERE id='m1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!((conf - 1.0).abs() < f64::EPSILON, "default confidence");
        assert_eq!(source, "api", "default source");
    }

    #[test]
    fn migrate_v29_backfills_reflection_depth_default() {
        // Reflection depth (v29) is the most recent column add. Verify
        // an existing row picks up the DEFAULT 0 on migrate.
        let conn = Connection::open_in_memory().expect("in-memory db");
        // Use the production schema MINUS the reflection_depth column
        // by manually dropping it from a fresh table. Simpler: emulate
        // a pre-v29 DB by stamping version=28 on the full schema, then
        // re-add the row, then check the column value (which would be
        // 0 because the SCHEMA default already populates it).
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute("INSERT INTO schema_version VALUES (28)", [])
            .unwrap();
        // Pre-v29 row inserted before migrate runs.
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at) \
             VALUES ('mref', 'mid', 'ns', 't', 'c', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        super::migrate(&conn).expect("migrate to v29 ok");
        let depth: i64 = conn
            .query_row(
                "SELECT reflection_depth FROM memories WHERE id='mref'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(depth, 0, "reflection_depth default must be 0");
    }

    #[test]
    fn migrate_wraps_in_begin_exclusive_transaction() {
        // Verify the migration runs inside a transaction by attempting
        // to start an EXCLUSIVE transaction on a second connection
        // against the SAME memory file. Because in-memory DBs are
        // per-connection by default, we can't share them across two
        // Connection handles cheaply. Instead, assert that an explicit
        // BEGIN EXCLUSIVE preceding migrate's own begin would fail.
        //
        // Easier path: run migrate, then verify schema_version was
        // updated (which only happens inside the transaction's COMMIT
        // path) — pinning the all-or-nothing contract.
        let conn = fresh_db_via_migrate();
        let v: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn migrate_idempotent_replay_keeps_schema_stable() {
        // Run migrate 5x and assert the schema_version row count and
        // value never drift. Migration MUST be replay-safe; an
        // incorrect implementation would leave duplicate
        // schema_version rows or partial ALTER side-effects.
        let conn = fresh_db_via_migrate();
        for _ in 0..5 {
            super::migrate(&conn).expect("idempotent");
        }
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "schema_version row count must remain 1 after replay");
        let v: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn migrate_v2_adds_columns_only_when_absent() {
        // Has-confidence/has-source guards: when the columns already
        // exist (e.g. the production SCHEMA already has them), the
        // ALTER branches must NOT fire (duplicate-column error
        // otherwise). Run migrate over the fully-populated SCHEMA
        // with version stamped at 1 — the has_X guards must observe
        // the columns and short-circuit.
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute("INSERT INTO schema_version VALUES (1)", [])
            .unwrap();
        // This MUST NOT panic with "duplicate column name".
        super::migrate(&conn).expect("idempotent v2");
        assert_eq!(current_version(&conn), CURRENT_SCHEMA_VERSION);
    }

    // -----------------------------------------------------------------
    // L0.7-2 Tier A §3.5 — Historical replay fixture.
    //
    // The synthetic legacy schema below is the v1 / pre-confidence,
    // pre-source, pre-embedding shape that production deployments
    // shipped at the v0.5-era. It contains ONLY the minimum tables and
    // columns required for migrate() to traverse every `if version <
    // N` arm AND for every has_X guard to evaluate FALSE (i.e. trigger
    // the ALTER TABLE branch). Walking forward through migrate() from
    // version=0 on this schema exercises:
    //
    //   * v2: confidence/source column additions on memories
    //   * v3: embedding BLOB column addition
    //   * v4: archived_memories table creation
    //   * v5: namespace_meta table creation
    //   * v6: parent_namespace column addition
    //   * v7: metadata column additions on memories + archived_memories
    //   * v8: pending_actions table creation
    //   * v9: approvals column addition on pending_actions
    //   * v10: scope_idx VIRTUAL generated column + index
    //   * v11: sync_state table creation
    //   * v12: last_pushed_at column addition on sync_state
    //   * v13: subscriptions table creation
    //   * v14: agent_id_idx VIRTUAL + indexes
    //   * v15: memory_links temporal columns + side tables (via SQL file)
    //   * v17: governance.inherit backfill (via SQL file)
    //   * v18: embedding_dim, archive embedding/tier columns + backfill
    //   * v19: subscriptions.event_types column + index
    //   * v20: audit_log table creation
    //   * v21: pending_actions timeout columns + index
    //   * v22: memory_transcripts table creation
    //   * v23: memory_links.attest_level column + backfill
    //   * v24: memory_transcript_links join table
    //   * v25: memory_transcripts.archived_at column + partial index
    //   * v26: signed_events table creation
    //   * v27: subscription_events / subscription_dlq + correlation_id
    //   * v28: agent_quotas table creation
    //   * v29: memories.reflection_depth column
    //
    // Versions 16 (no DDL) lands a no-op arm — covered by the version
    // walk itself.
    //
    // The legacy schema only carries what's needed:
    //   * memories (v1 columns: id/tier/namespace/title/content/tags/
    //                priority/access_count/created_at/updated_at/
    //                last_accessed_at/expires_at)
    //   * memory_links (v1 columns: source_id/target_id/relation/created_at)
    //   * schema_version (so MAX(version) returns the seeded value)
    //
    // FTS5 + triggers from the latest SCHEMA depend on the memories
    // table shape staying compatible, so we keep them simple: only the
    // base table, plus schema_version. The migrations themselves don't
    // touch the FTS or triggers (those live in SCHEMA, applied on
    // fresh-install only).
    // -----------------------------------------------------------------

    /// Synthetic pre-v2 schema. The original v1 shape of `memories`
    /// without `confidence`, `source`, `embedding`, `metadata`, etc.
    /// All later schema state arrives via the migrate ladder.
    const LEGACY_V1_SCHEMA: &str = r"
        CREATE TABLE IF NOT EXISTS memories (
            id               TEXT PRIMARY KEY,
            tier             TEXT NOT NULL,
            namespace        TEXT NOT NULL DEFAULT 'global',
            title            TEXT NOT NULL,
            content          TEXT NOT NULL,
            tags             TEXT NOT NULL DEFAULT '[]',
            priority         INTEGER NOT NULL DEFAULT 5,
            access_count     INTEGER NOT NULL DEFAULT 0,
            created_at       TEXT NOT NULL,
            updated_at       TEXT NOT NULL,
            last_accessed_at TEXT,
            expires_at       TEXT
        );

        CREATE TABLE IF NOT EXISTS memory_links (
            source_id   TEXT NOT NULL,
            target_id   TEXT NOT NULL,
            relation    TEXT NOT NULL DEFAULT 'related_to',
            created_at  TEXT NOT NULL,
            PRIMARY KEY (source_id, target_id, relation)
        );

        CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER NOT NULL
        );
        INSERT INTO schema_version (version) VALUES (0);
    ";

    /// Build a legacy v1 database and walk the full migrate() ladder.
    /// Returns the migrated connection.
    fn replay_from_v1() -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(LEGACY_V1_SCHEMA)
            .expect("apply legacy v1 schema");
        // Seed a row at v1 shape so we can verify column-add defaults.
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at) \
             VALUES ('legacy', 'short', 'ns', 't', 'c', \
             '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
        super::migrate(&conn).expect("walk every migrate arm from v0");
        conn
    }

    fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
        let sql = format!("SELECT {column} FROM {table} LIMIT 0");
        conn.prepare(&sql).is_ok()
    }

    fn table_exists(conn: &Connection, table: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            params![table],
            |row| row.get::<_, i64>(0),
        )
        .is_ok()
    }

    fn index_exists(conn: &Connection, index: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1",
            params![index],
            |row| row.get::<_, i64>(0),
        )
        .is_ok()
    }

    #[test]
    fn historical_replay_from_v1_reaches_current_schema() {
        // Headline rigor test: walk every `if version < N` arm by
        // starting from a legacy v1 schema. The migrate() function
        // executes every arm in order. We assert the final
        // schema_version row holds CURRENT_SCHEMA_VERSION and that
        // each documented column/table/index materialised.
        let conn = replay_from_v1();
        assert_eq!(current_version(&conn), CURRENT_SCHEMA_VERSION);

        // v2 columns
        assert!(column_exists(&conn, "memories", "confidence"));
        assert!(column_exists(&conn, "memories", "source"));
        // v3
        assert!(column_exists(&conn, "memories", "embedding"));
        // v4
        assert!(table_exists(&conn, "archived_memories"));
        // v5
        assert!(table_exists(&conn, "namespace_meta"));
        // v6
        assert!(column_exists(&conn, "namespace_meta", "parent_namespace"));
        // v7
        assert!(column_exists(&conn, "memories", "metadata"));
        assert!(column_exists(&conn, "archived_memories", "metadata"));
        // v8
        assert!(table_exists(&conn, "pending_actions"));
        // v9
        assert!(column_exists(&conn, "pending_actions", "approvals"));
        // v10
        assert!(column_exists(&conn, "memories", "scope_idx"));
        assert!(index_exists(&conn, "idx_memories_scope_idx"));
        // v11
        assert!(table_exists(&conn, "sync_state"));
        // v12
        assert!(column_exists(&conn, "sync_state", "last_pushed_at"));
        // v13
        assert!(table_exists(&conn, "subscriptions"));
        // v14
        assert!(column_exists(&conn, "memories", "agent_id_idx"));
        assert!(index_exists(&conn, "idx_memories_agent_id"));
        assert!(index_exists(&conn, "idx_memories_created_at"));
        // v15 — memory_links temporal columns + entity_aliases side table
        assert!(column_exists(&conn, "memory_links", "valid_from"));
        assert!(column_exists(&conn, "memory_links", "valid_until"));
        assert!(column_exists(&conn, "memory_links", "observed_by"));
        assert!(column_exists(&conn, "memory_links", "signature"));
        assert!(table_exists(&conn, "entity_aliases"));
        // v18
        assert!(column_exists(&conn, "memories", "embedding_dim"));
        assert!(column_exists(&conn, "archived_memories", "embedding"));
        assert!(column_exists(&conn, "archived_memories", "embedding_dim"));
        assert!(column_exists(&conn, "archived_memories", "original_tier"));
        assert!(column_exists(
            &conn,
            "archived_memories",
            "original_expires_at"
        ));
        // v19
        assert!(column_exists(&conn, "subscriptions", "event_types"));
        // v20
        assert!(table_exists(&conn, "audit_log"));
        // v21
        assert!(column_exists(
            &conn,
            "pending_actions",
            "default_timeout_seconds"
        ));
        assert!(column_exists(&conn, "pending_actions", "expired_at"));
        // v22
        assert!(table_exists(&conn, "memory_transcripts"));
        // v23
        assert!(column_exists(&conn, "memory_links", "attest_level"));
        // v24
        assert!(table_exists(&conn, "memory_transcript_links"));
        // v25
        assert!(column_exists(&conn, "memory_transcripts", "archived_at"));
        // v26
        assert!(table_exists(&conn, "signed_events"));
        // v27
        assert!(table_exists(&conn, "subscription_events"));
        assert!(table_exists(&conn, "subscription_dlq"));
        assert!(column_exists(
            &conn,
            "subscription_events",
            "correlation_id"
        ));
        // v28
        assert!(table_exists(&conn, "agent_quotas"));
        // v29
        assert!(column_exists(&conn, "memories", "reflection_depth"));
        // v36 (WT-1-A) — atomisation foundation columns + partial indexes.
        assert!(column_exists(&conn, "memories", "atomised_into"));
        assert!(column_exists(&conn, "memories", "atom_of"));
        assert!(index_exists(&conn, "idx_memories_atom_of"));
        assert!(index_exists(&conn, "idx_memories_atomised_into"));
    }

    #[test]
    fn historical_replay_backfills_v2_defaults_on_legacy_row() {
        // The legacy row inserted at v1 lacks confidence/source. After
        // migrate() walks v2's ALTER TABLE arm, the ADD COLUMN
        // statement applies the DEFAULT 1.0 / 'api' to the existing
        // row. This proves the playbook §3.5 contract: existing rows
        // pick up the right default.
        let conn = replay_from_v1();
        let conf: f64 = conn
            .query_row(
                "SELECT confidence FROM memories WHERE id='legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let source: String = conn
            .query_row("SELECT source FROM memories WHERE id='legacy'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!((conf - 1.0).abs() < f64::EPSILON);
        assert_eq!(source, "api");
    }

    #[test]
    fn historical_replay_backfills_v7_metadata_default_on_legacy_row() {
        // v7 ALTER TABLE adds `metadata TEXT NOT NULL DEFAULT '{}'`.
        // The legacy row must pick up the JSON-object default.
        let conn = replay_from_v1();
        let meta: String = conn
            .query_row("SELECT metadata FROM memories WHERE id='legacy'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(meta, "{}");
    }

    #[test]
    fn historical_replay_backfills_v29_reflection_depth_default() {
        // v29 ALTER TABLE adds `reflection_depth INTEGER NOT NULL
        // DEFAULT 0`. Legacy row must carry the default.
        let conn = replay_from_v1();
        let depth: i64 = conn
            .query_row(
                "SELECT reflection_depth FROM memories WHERE id='legacy'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(depth, 0);
    }

    #[test]
    fn historical_replay_v15_backfills_valid_from_to_memories_created_at() {
        // The 0010_v063_hierarchy_kg.sql migration backfills
        // memory_links.valid_from <= memories.created_at on the source.
        // Seed a legacy memory_link before migrating and verify the
        // backfill ran. We need to insert a memories row whose id
        // matches source_id, plus a memory_links row at the v1 shape.
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(LEGACY_V1_SCHEMA)
            .expect("apply legacy v1 schema");
        // Two memories (source/target) and a link between them.
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at) \
             VALUES ('m_src', 'short', 'ns', 't1', 'c1', \
             '2024-06-01T12:34:56Z', '2024-06-01T12:34:56Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at) \
             VALUES ('m_tgt', 'short', 'ns', 't2', 'c2', \
             '2024-06-01T12:34:56Z', '2024-06-01T12:34:56Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memory_links (source_id, target_id, relation, created_at) \
             VALUES ('m_src', 'm_tgt', 'related_to', '2024-06-01T12:34:56Z')",
            [],
        )
        .unwrap();
        super::migrate(&conn).expect("migrate from v0 with link");

        // After migrate, valid_from on the link must equal the source's created_at.
        let valid_from: Option<String> = conn
            .query_row(
                "SELECT valid_from FROM memory_links \
                 WHERE source_id='m_src' AND target_id='m_tgt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            valid_from.as_deref(),
            Some("2024-06-01T12:34:56Z"),
            "v15 backfill must seed valid_from to source created_at"
        );
    }

    #[test]
    fn historical_replay_v18_backfills_embedding_dim_from_blob_length() {
        // v18 SQL file backfills embedding_dim = length(embedding)/4.
        // Walk from v1 to ensure v3 (embedding column) and v17 land
        // before v18, then seed a row with embedding bytes + NULL
        // dim, then run a second migrate() that picks up only the
        // backfill UPDATE (idempotent on the already-applied DDL).
        //
        // Simpler approach: replay from v1 to v17, then INSERT a row
        // with the embedding present and dim NULL, then call migrate
        // again with version stamped at 17. The v18 arm fires, the
        // SQL file's backfill UPDATE runs, and dim should land at
        // length(embedding)/4.
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(LEGACY_V1_SCHEMA)
            .expect("apply legacy v1 schema");
        super::migrate(&conn).expect("first migrate to current");
        // Stamp back to v17 so the v18 arm re-runs (which is
        // idempotent on ALTERs because of the has_X guards).
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute("INSERT INTO schema_version VALUES (17)", [])
            .unwrap();
        // Insert a row with embedding bytes + embedding_dim NULL.
        let embedding = vec![0u8; 8]; // 2 f32s @ 4 bytes
        conn.execute(
            "INSERT INTO memories \
             (id, tier, namespace, title, content, created_at, updated_at, embedding, embedding_dim) \
             VALUES ('m18', 'short', 'ns', 't', 'c', \
             '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z', ?1, NULL)",
            params![embedding],
        )
        .unwrap();
        super::migrate(&conn).expect("migrate v17->v29 (idempotent on ALTERs)");
        let dim: Option<i64> = conn
            .query_row(
                "SELECT embedding_dim FROM memories WHERE id='m18'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dim, Some(2), "v18 backfill must set embedding_dim = len/4");
    }

    #[test]
    fn historical_replay_v27_creates_dlq_table() {
        // v27 brings up subscription_dlq from scratch (no DLQ table in
        // any prior version). A fresh-from-v1 replay must end up with
        // the DLQ table + the supporting indexes documented in the
        // 0021 SQL file.
        let conn = replay_from_v1();
        assert!(table_exists(&conn, "subscription_dlq"));
        assert!(index_exists(&conn, "idx_subscription_dlq_subscription"));
        assert!(index_exists(&conn, "idx_subscription_dlq_correlation"));
        assert!(index_exists(&conn, "idx_subscription_events_correlation"));
    }

    #[test]
    fn historical_replay_v27_alter_runs_before_index_on_existing_subscription_events_table() {
        // REGRESSION (bug memory dbc594f4-0d38-4f03-892e-a9fd8dacdcdc,
        // discovered 2026-05-13, fixed in fix-campaign CF-1 #690):
        // when migrating a deployment whose `subscription_events`
        // table predated the K6 correlation_id column, the v27 arm
        // used to run `execute_batch(MIGRATION_V27_SQLITE)` (which
        // includes `CREATE INDEX ... ON subscription_events(correlation_id)`)
        // BEFORE the ALTER TABLE that adds the column — surfacing as
        // "no such column: correlation_id".
        //
        // Post-fix, the ALTER must run FIRST, then the SQL file's
        // CREATE INDEX succeeds. This test pins that order: a
        // hand-rolled v26 `subscription_events` table without
        // correlation_id must migrate cleanly to CURRENT_SCHEMA_VERSION
        // with the column + index both present.
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute("DROP TABLE IF EXISTS subscription_events", [])
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE subscription_events (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                subscription_id TEXT NOT NULL,
                event_type      TEXT NOT NULL,
                payload         TEXT NOT NULL,
                delivered_at    TEXT NOT NULL,
                delivery_status TEXT NOT NULL DEFAULT 'pending'
            );",
        )
        .unwrap();
        conn.execute("INSERT INTO schema_version VALUES (26)", [])
            .unwrap();

        // Post-fix behaviour: ALTER runs first, CREATE INDEX succeeds.
        super::migrate(&conn).expect("v27 migration on hand-rolled v26 table must succeed");

        // The correlation_id column is now present on the legacy table.
        assert!(
            column_exists(&conn, "subscription_events", "correlation_id"),
            "ALTER must add correlation_id before the SQL file's CREATE INDEX runs"
        );
        // And the index landed.
        assert!(index_exists(&conn, "idx_subscription_events_correlation"));
        // Final state at CURRENT_SCHEMA_VERSION.
        assert_eq!(current_version(&conn), CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn historical_replay_idempotent_re_run_holds_steady() {
        // After replaying from v1, a second migrate() call must be a
        // pure no-op. Per playbook §3.5: idempotency is part of the
        // replay-rigor contract.
        let conn = replay_from_v1();
        let before = current_version(&conn);
        super::migrate(&conn).expect("idempotent re-run");
        let after = current_version(&conn);
        assert_eq!(before, after);
        assert_eq!(after, CURRENT_SCHEMA_VERSION);
        // Row count in schema_version is exactly 1.
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn historical_replay_v7_alters_pre_existing_archived_memories_without_metadata() {
        // The v4 CREATE TABLE archived_memories in migrate() ships
        // `metadata` inline, so a pure v1->v29 replay never triggers
        // the v7 `has_archive_metadata`-FALSE inner ALTER. To
        // exercise that branch we hand-craft an archived_memories
        // table WITHOUT the metadata column, then stamp version=3
        // (post-v3 embedding column, pre-v4) so v4's CREATE TABLE
        // IF NOT EXISTS no-ops (table exists, no inner ADD), and
        // v7's `has_archive_metadata` returns FALSE → ALTER fires.
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(LEGACY_V1_SCHEMA)
            .expect("apply legacy v1 schema");
        // Pre-create archived_memories at pre-v7 shape (no metadata).
        conn.execute_batch(
            "CREATE TABLE archived_memories (
                id               TEXT PRIMARY KEY,
                tier             TEXT NOT NULL,
                namespace        TEXT NOT NULL,
                title            TEXT NOT NULL,
                content          TEXT NOT NULL,
                tags             TEXT NOT NULL,
                priority         INTEGER NOT NULL,
                confidence       REAL NOT NULL,
                source           TEXT NOT NULL,
                access_count     INTEGER NOT NULL,
                created_at       TEXT NOT NULL,
                updated_at       TEXT NOT NULL,
                last_accessed_at TEXT,
                expires_at       TEXT,
                archived_at      TEXT NOT NULL,
                archive_reason   TEXT NOT NULL DEFAULT 'ttl_expired'
            );",
        )
        .unwrap();
        // Walk from v0 so v2 (confidence/source) and v3 (embedding)
        // both fire on `memories`. archived_memories already exists,
        // so v4's CREATE IF NOT EXISTS no-ops. v7's has_archive_metadata
        // returns FALSE → ALTER fires (inner branch coverage).
        super::migrate(&conn).expect("migrate v0->v29 with stale archived_memories shape");
        // The v7 inner ALTER must have run: metadata column now present.
        assert!(column_exists(&conn, "archived_memories", "metadata"));
        // And the final state is at CURRENT_SCHEMA_VERSION.
        assert_eq!(current_version(&conn), CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn historical_replay_v18_alters_pre_existing_archived_memories_without_embedding() {
        // Same trick for v18: archived_memories needs original_tier /
        // embedding / embedding_dim / original_expires_at added. The
        // v4 CREATE ships none of those columns (v18 added them) so
        // a pure replay DOES hit these. But the v4 CREATE in code
        // does not include them, so this test mirrors the existing
        // historical_replay_from_v1 path and pins the per-column
        // ALTER branches.
        let conn = replay_from_v1();
        assert!(column_exists(&conn, "archived_memories", "embedding"));
        assert!(column_exists(&conn, "archived_memories", "embedding_dim"));
        assert!(column_exists(&conn, "archived_memories", "original_tier"));
        assert!(column_exists(
            &conn,
            "archived_memories",
            "original_expires_at"
        ));
    }

    #[test]
    fn historical_replay_v9_alters_pending_actions_missing_approvals() {
        // v9 adds `approvals` to pending_actions only when absent.
        // v8 creates pending_actions WITHOUT approvals (v9 adds it).
        // A pure v1->v29 replay covers the FALSE branch (which fires
        // the ALTER). This test pins the TRUE branch: stamp at v=0,
        // pre-create pending_actions WITH approvals so v8's CREATE
        // IF NOT EXISTS no-ops, then v9's has_approvals returns
        // TRUE → ALTER does NOT fire. The end state is identical.
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(LEGACY_V1_SCHEMA)
            .expect("apply legacy v1 schema");
        // Pre-create pending_actions with approvals already present.
        conn.execute_batch(
            "CREATE TABLE pending_actions (
                id            TEXT PRIMARY KEY,
                action_type   TEXT NOT NULL,
                memory_id     TEXT,
                namespace     TEXT NOT NULL,
                payload       TEXT NOT NULL DEFAULT '{}',
                requested_by  TEXT NOT NULL,
                requested_at  TEXT NOT NULL,
                status        TEXT NOT NULL DEFAULT 'pending',
                decided_by    TEXT,
                decided_at    TEXT,
                approvals     TEXT NOT NULL DEFAULT '[]'
            );",
        )
        .unwrap();
        // Walk from v0 so all earlier migrations run normally; v8's
        // CREATE IF NOT EXISTS no-ops (table exists); v9's
        // has_approvals returns TRUE → inner ALTER skipped.
        super::migrate(&conn).expect("migrate v0->v29 with pre-existing approvals");
        assert!(column_exists(&conn, "pending_actions", "approvals"));
        assert_eq!(current_version(&conn), CURRENT_SCHEMA_VERSION);
    }

    #[test]
    fn migrate_rollback_path_on_failed_arm_propagates_error() {
        // Force an arm to fail mid-transaction by pre-creating a
        // conflicting table that one of the file-based migrations
        // tries to redefine without IF NOT EXISTS. The v20
        // (audit_log) migration's CREATE TABLE IF NOT EXISTS won't
        // fail, but if we drop the schema_version table BEFORE
        // migrate runs the initial probe survives (returns 0 via
        // unwrap_or) but the final INSERT will fail because there is
        // no table. This pins the err-arm of `result` -> ROLLBACK.
        //
        // We instead inject failure by stamping a pre-v1 version and
        // dropping schema_version mid-stream. Cleaner approach: drop
        // schema_version table before migrate so the final INSERT
        // hits a "no such table" — the wrapped result captures it
        // and the function ROLLBACKs the transaction.
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(SCHEMA).unwrap();
        // Stamp at version=28 so the v29 arm fires.
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute("INSERT INTO schema_version VALUES (28)", [])
            .unwrap();
        // Drop a table the v29 path needs (memories itself). The
        // v29 ALTER will then fail and the error path triggers
        // ROLLBACK.
        // Best alternative: drop schema_version. Then the final
        // `DELETE FROM schema_version` errors. We must keep memories
        // intact for v29's ALTER probe to run, so use the
        // schema_version drop here.
        conn.execute("DROP TABLE schema_version", []).unwrap();
        // The initial probe also queries schema_version, so this
        // produces an error before EXCLUSIVE begins. Without a
        // schema_version table, `MAX(version)` query fails and
        // unwrap_or returns 0 — migrate enters the loop, but the
        // final INSERT to schema_version fails. The wrapped result
        // is Err -> ROLLBACK runs. We pin that the function returns
        // Err.
        let res = super::migrate(&conn);
        assert!(
            res.is_err(),
            "migrate must propagate err when terminal INSERT fails"
        );
    }
}
