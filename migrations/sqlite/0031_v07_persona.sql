-- Copyright 2026 AlphaOne LLC
-- SPDX-License-Identifier: Apache-2.0
--
-- v0.7.0 QW-2 — Persona-as-artifact substrate primitive (schema v37).
--
-- Adds the storage for Tencent-pattern L3 personas: a Persona is a
-- first-class MemoryKind variant minted by the reflection-pass curator
-- from a cluster of reflections about a single entity. Personas are
-- stored as memories so they ride the normal recall, signing, and
-- federation rails — this migration just adds the discriminator
-- columns + the partial index that makes "fetch the persona for
-- entity X in namespace Y" an indexed lookup rather than a JSON
-- metadata scan.
--
-- Columns:
--   * `entity_id`        — populated only when memory_kind = 'persona'.
--                          Identifies the subject of the persona.
--   * `persona_version`  — monotonic counter per (entity_id, namespace).
--                          Each regeneration writes a new row with
--                          version+1; older rows stay queryable for
--                          audit / rollback.
--
-- Both columns are nullable on the parent table so non-Persona rows
-- (every observation, every reflection) keep their existing NULL
-- payload — no backfill needed.
--
-- The partial index keeps the namespace + entity_id lookup cheap
-- (Persona rows are a small minority of the table) and prevents the
-- index from bloating on the dominant observation/reflection workload.

CREATE INDEX IF NOT EXISTS idx_personas_by_entity
    ON memories(entity_id, namespace)
    WHERE memory_kind = 'persona';
