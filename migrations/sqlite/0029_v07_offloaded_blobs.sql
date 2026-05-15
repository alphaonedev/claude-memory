-- v0.7.0 QW-3 — Context-offload substrate primitive (schema v35).
--
-- Substrate-level offload+deref store. v0.8.0 short-term context
-- compression (Mermaid canvas, auto-cadence, node_id integration)
-- will build on this primitive: agents drop a large tool-call result
-- here, keep the short `ref_id` in their working window, and `deref`
-- when the content is needed again.
--
-- Pipeline (shared with v0.7.0 transcripts module):
--   - content compressed with zstd level 3 (matches `memory_transcripts`).
--   - `content_sha256` is the digest of the ORIGINAL (decompressed) bytes
--     — recomputed on `deref` and compared to refuse tampered blobs.
--   - `signature_b64` is the agent's Ed25519 signature over the canonical
--     bundle `{ ref_id, content_sha256, stored_at, namespace }` (URL-safe
--     base64, no padding).
--   - `signed_events` carries a sibling row with `event_type =
--     context_offloaded` so the H5 audit chain captures provenance.
--
-- Notes:
--   * `stored_at` is a Unix epoch seconds (INTEGER) — RFC3339 strings in
--     the canonical signed payload are unnecessary here; the substrate
--     reads timestamps numerically for the TTL sweep, and the signed
--     payload uses the same integer so verification stays bit-exact.
--   * `ttl_seconds` is the duration from `stored_at` after which the
--     row becomes eligible for the daily TTL sweep. NULL means
--     "permanent until explicit operator delete".
--   * The partial index `idx_offloaded_blobs_ttl` is the access pattern
--     for the daily sweep — covers `(stored_at, ttl_seconds)` only on
--     rows that have a TTL, so the index never grows for the permanent
--     class.

CREATE TABLE IF NOT EXISTS offloaded_blobs (
    ref_id           TEXT PRIMARY KEY,
    namespace        TEXT NOT NULL,
    content_zstd     BLOB NOT NULL,
    content_sha256   TEXT NOT NULL,
    stored_at        INTEGER NOT NULL,
    ttl_seconds      INTEGER,
    agent_id         TEXT NOT NULL,
    signature_b64    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_offloaded_blobs_namespace
    ON offloaded_blobs(namespace);

CREATE INDEX IF NOT EXISTS idx_offloaded_blobs_ttl
    ON offloaded_blobs(stored_at, ttl_seconds)
    WHERE ttl_seconds IS NOT NULL;
