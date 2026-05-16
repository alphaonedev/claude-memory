# Signed events V-4 closeout — cross-row hash chain

v0.7.0 flips the V-4 validation from YELLOW to GREEN by adding a
cross-row hash chain to the `signed_events` audit table. Each row
carries `prev_hash` + `sequence` so the chain itself is tamper-evident,
not just each individual signed payload. Verify end-to-end with
`ai-memory verify-signed-events-chain`.

- **Issue trail:** [#698](https://github.com/alphaonedev/ai-memory-mcp/issues/698).
- **Code paths:** [`src/signed_events.rs`](../src/signed_events.rs),
  the `migrate_v34_backfill_chain` function in
  [`src/storage/migrations.rs`](../src/storage/migrations.rs).
- **Schema:**
  - [`migrations/sqlite/0020_v07_signed_events.sql`](../migrations/sqlite/0020_v07_signed_events.sql)
    — base `signed_events` table (v33).
  - [`migrations/sqlite/0028_v07_signed_events_chain.sql`](../migrations/sqlite/0028_v07_signed_events_chain.sql)
    — V-4 cross-row chain columns (v34).
  - [`migrations/postgres/0015_v07_signed_events_chain.sql`](../migrations/postgres/0015_v07_signed_events_chain.sql)
    — postgres mirror at v33 (postgres ladder ran one step behind).
- **CLI verb:** [`src/cli/verify_signed_events.rs`](../src/cli/verify_signed_events.rs)
  — `ai-memory verify-signed-events-chain [--since N] [--format text|json]`.

## Row shape (v34)

```
CREATE TABLE signed_events (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  sequence      INTEGER NOT NULL,             -- monotonic per-DB
  prev_hash     BLOB NOT NULL,                -- 32 bytes; zero for first row
  event_type    TEXT NOT NULL,
  payload_cbor  BLOB NOT NULL,                -- canonical CBOR
  payload_sha256 BLOB NOT NULL,               -- SHA-256 of payload_cbor
  signed_by     TEXT NOT NULL,                -- agent_id
  signature     BLOB NOT NULL,                -- Ed25519 over the canonical preimage
  created_at    TEXT NOT NULL
);

CREATE UNIQUE INDEX signed_events_sequence_uniq ON signed_events(sequence);
```

The canonical preimage that the Ed25519 signature covers is:

```
sequence || prev_hash || event_type || payload_sha256 || signed_by || created_at
```

`prev_hash` for row `N` is the SHA-256 of row `N-1`'s `payload_cbor`.
For row 1, `prev_hash` is 32 zero bytes. Tampering with any prior
row's `payload_cbor` invalidates every subsequent row's `prev_hash`.

## Backfill (v33 → v34)

A fresh v0.7.0 install starts at v34 and writes the chain from row 1.
A v0.7-alpha install at v33 has rows without `prev_hash` /
`sequence`; the `migrate_v34_backfill_chain` function in
[`src/storage/migrations.rs`](../src/storage/migrations.rs) walks the
existing rows in `id` order, assigns sequential `sequence` numbers,
and computes `prev_hash` from the prior row's `payload_cbor`. The
backfill is **idempotent** — re-running on an already-backfilled
table is a no-op.

Pinned by [`tests/signed_events_chain_v34.rs`](../tests/signed_events_chain_v34.rs):

- First-row `prev_hash` is zero.
- Multi-row chaining (each row's `prev_hash` = SHA-256 of prior payload).
- Payload tamper (the signature still verifies in isolation, but the
  chain walk catches it on the next row).
- Sequence tamper (gaps fail the chain check).
- Concurrent drainer inserts via PE-3 (the wire-point pre-write hook
  guarantees the chain stays monotonic under concurrent writers).
- Backfill idempotency.
- Backfill correctness (re-walked chain matches a fresh-write chain).

## CLI verifier

```bash
# Text output (operator readable)
ai-memory verify-signed-events-chain

# JSON output (operator scriptable)
ai-memory verify-signed-events-chain --format json | jq

# Skip the first N rows (incremental verification)
ai-memory verify-signed-events-chain --since 100000
```

Exit codes:

- `0` — chain valid from start (or `--since`) to current tail.
- `1` — chain invalid; the JSON output identifies the first offending
  row's `sequence` and reason (`prev_hash_mismatch` /
  `signature_invalid` / `sequence_gap` / `payload_hash_mismatch`).

Pinned by [`tests/cli_verify_chain.rs`](../tests/cli_verify_chain.rs).

## Concurrent-writer guarantee (PE-3)

`signed_events` writes happen on the substrate `storage::insert` path
under a transactional wrap that the PE-3 wire-point hook elevates so
the chain stays monotonic under concurrent writers. The
[`tests/deferred_audit_soak.rs`](../tests/deferred_audit_soak.rs)
soak fires 5,000 concurrent inserts and asserts the chain walk passes
afterwards.

## Operator workflow

1. **Generate an operator keypair** (`ai-memory identity generate
   --agent-id "$(ai-memory identity suggest-id)"`).
2. **Restart the daemon.** The v34 schema migration runs
   automatically on first start and backfills the chain from existing
   `signed_events` rows.
3. **Run the verifier daily** (cron / systemd timer). A non-zero exit
   indicates either tampering or a v0.7.0-alpha row that didn't make
   it through the backfill — investigate immediately.
4. **Pair with the forensic bundle** (L2-5,
   [`docs/forensic-export.md`](forensic-export.md)) — the signed
   events table ships inside the bundle by default. Offline reviewers
   can re-verify the chain without DB access.

## Mapping to the V-4 audit

- **V-4 (cross-row hash chain) PASS** — pinned by
  `tests/signed_events_chain_v34.rs`.
- **V-4 (per-row signature) PASS** — unchanged from v0.6.3.
- **V-4 (append-only enforcement) PASS** — no UPDATE / DELETE path
  through the application layer; the substrate validates against
  `SignedEventsAppendOnlyViolation` on any write that would non-append.

See also: [`docs/MIGRATION_v0.7.md` §"Ed25519 attestation"](MIGRATION_v0.7.md#ed25519-attestation-opt-in),
[`docs/v0.7.0/release-notes.md` §"Signed events V-4 closeout"](v0.7.0/release-notes.md),
the canonical inventory in
[`docs/internal/v070-feature-inventory.md` §"Feature: Signed events V-4 closeout"](internal/v070-feature-inventory.md).
