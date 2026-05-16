-- v0.7.0 QW-3 — Context-offload substrate primitive (Postgres v34).
--
-- Mirror of SQLite migration 0029_v07_offloaded_blobs.sql. See that
-- file for the full design rationale; this header only documents the
-- pg-specific shape choices (BYTEA for the zstd blob, BIGINT for the
-- Unix-seconds timestamp).

CREATE TABLE IF NOT EXISTS offloaded_blobs (
    ref_id           TEXT PRIMARY KEY,
    namespace        TEXT NOT NULL,
    content_zstd     BYTEA NOT NULL,
    content_sha256   TEXT NOT NULL,
    stored_at        BIGINT NOT NULL,
    ttl_seconds      BIGINT,
    agent_id         TEXT NOT NULL,
    signature_b64    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_offloaded_blobs_namespace
    ON offloaded_blobs(namespace);

CREATE INDEX IF NOT EXISTS idx_offloaded_blobs_ttl
    ON offloaded_blobs(stored_at, ttl_seconds)
    WHERE ttl_seconds IS NOT NULL;
