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
    — base `signed_events` table (schema v25 origin; the v34 chain
    columns landed in the same file as inline comments).
  - [`migrations/sqlite/0028_v07_signed_events_chain.sql`](../migrations/sqlite/0028_v07_signed_events_chain.sql)
    — V-4 cross-row chain columns (schema v34).
  - [`migrations/postgres/0015_v07_signed_events_chain.sql`](../migrations/postgres/0015_v07_signed_events_chain.sql)
    — postgres mirror (postgres ladder ran one step behind).
- **CLI verb:** [`src/cli/verify_signed_events.rs`](../src/cli/verify_signed_events.rs)
  — `ai-memory verify-signed-events-chain [--since N] [--format text|json]`.

## Row shape (v34)

```sql
CREATE TABLE signed_events (
  id            TEXT PRIMARY KEY,             -- UUIDv4 minted by writer
  agent_id      TEXT NOT NULL,                -- writer identity
  event_type    TEXT NOT NULL,                -- e.g. "memory_link.created"
  payload_hash  BLOB NOT NULL,                -- SHA-256 over canonical-CBOR payload
  signature     BLOB,                         -- Ed25519 over the source row (NULL when unsigned)
  attest_level  TEXT NOT NULL,                -- "unsigned" | "signed" | …
  timestamp     TEXT NOT NULL,                -- RFC3339 UTC
  prev_hash     BLOB NOT NULL,                -- 32 bytes; zero for first row (v34)
  sequence      INTEGER NOT NULL              -- monotonic per-DB; UNIQUE (v34)
);

CREATE UNIQUE INDEX signed_events_sequence_uniq ON signed_events(sequence);
```

The canonical bytes that the chain hash covers are
([`src/signed_events.rs:150`](../src/signed_events.rs)):

```
id || 0x1F || agent_id || 0x1F || event_type || 0x1F ||
payload_hash || 0x1F || signature_or_empty || 0x1F ||
attest_level || 0x1F || timestamp || 0x1F || sequence_be_8_bytes
```

The separator `0x1F` (ASCII Unit Separator) is present in neither
RFC3339 timestamps nor UUIDs nor the hex/base64 payloads — so
concatenation is unambiguous without escaping.

`prev_hash` for row `N` is `SHA-256(canonical_chain_bytes(row N-1))`.
For row 1, `prev_hash` is 32 zero bytes (`ZERO_HASH` at
[`src/signed_events.rs:129`](../src/signed_events.rs)). Tampering
with any prior row's canonical fields invalidates every subsequent
row's `prev_hash`.

`prev_hash` and `sequence` are populated by `append_signed_event`
([`src/signed_events.rs:468`](../src/signed_events.rs)) at insert
time. **Callers MUST NOT pre-populate them** — any value set by the
caller is ignored. Use `..SignedEvent::default()` at the
struct-literal tail to leave them empty.

## Backfill (v33 → v34)

A fresh v0.7.0 install starts at v34 and writes the chain from row 1.
A v0.7-alpha install at v33 has rows without `prev_hash` /
`sequence`; the `migrate_v34_backfill_chain` function in
[`src/storage/migrations.rs`](../src/storage/migrations.rs) walks the
existing rows in `rowid` order, assigns sequential `sequence`
numbers, and computes `prev_hash` from the prior row's canonical
bytes. The backfill is **idempotent** — re-running on an
already-backfilled table is a no-op.

A partially-backfilled state (some rows have `sequence IS NULL`,
others don't) is **load-bearing-bad**. `read_chain_head`
([`src/signed_events.rs:207`](../src/signed_events.rs)) hard-fails
with a clear diagnostic in this case (the COR-9 fix; cluster-C
issue #767) and refuses to append further rows until the operator
re-runs `ai-memory migrate`. Silently treating `NULL` as 0 would
collide with the legitimately-backfilled first row on the UNIQUE
index, masking a real migration-needed state behind a misleading
SQLITE_CONSTRAINT_UNIQUE.

Pinned by [`tests/signed_events_chain_v34.rs`](../tests/signed_events_chain_v34.rs):

- First-row `prev_hash` is zero.
- Multi-row chaining (each row's `prev_hash` = SHA-256 of prior
  canonical bytes).
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
  row's `sequence` and the report's
  `signature_failures` list contains any rows whose Ed25519
  signature did not verify against the supplied key set.

JSON shape ([`src/cli/verify_signed_events.rs:57`](../src/cli/verify_signed_events.rs)):

```jsonc
{
  "rows_checked": 142387,
  "chain_break": null,           // i64 sequence of first break, or null
  "signature_failures": [],      // list of sequences that failed sig verify
  "chain_holds": true
}
```

A chain break is structurally worse than a signature failure — the
chain itself is the load-bearing tamper-evidence property, and a
broken chain implicates every row past the break. Per-row signature
failures are surfaced separately because they are a disjoint
property: the chain may still be intact even if individual
signatures fail.

Pinned by [`tests/cli_verify_chain.rs`](../tests/cli_verify_chain.rs).

## Three complementary verifiers

The substrate ships three verifier surfaces, each pinning a distinct
property ([`src/cli/verify_signed_events.rs:11-19`](../src/cli/verify_signed_events.rs)):

| Verifier | Property | Source of truth |
|---|---|---|
| `verify-signed-events-chain` | Cross-row hash chain on `signed_events` (this doc) | SQL substrate |
| `audit verify` | JSONL audit log under `<audit_dir>/audit.log` — own `prev_hash` chain, restart-stable monotonic sequence (F2) | On-disk file |
| `verify-reflection-chain` | Per-edge Ed25519 signatures on `reflects_on` links | `memory_links` SQL |

Audit defense-in-depth: a successful attack must tamper with **all
three** without leaving evidence. Cross-host portability lives in
the JSONL log; daemon-local tamper-evidence lives in the SQL chain;
reflection ancestry attestation lives in the per-edge signatures.

## Concurrent-writer guarantee (PE-3)

`signed_events` writes happen on the substrate `storage::insert` path
under a transactional wrap that the PE-3 wire-point hook elevates so
the chain stays monotonic under concurrent writers. The
[`tests/deferred_audit_soak.rs`](../tests/deferred_audit_soak.rs)
soak fires 5,000 concurrent inserts and asserts the chain walk passes
afterwards. `append_signed_event`
([`src/signed_events.rs:468`](../src/signed_events.rs)) wraps in a
transaction; `append_signed_event_no_tx`
([`src/signed_events.rs:518`](../src/signed_events.rs)) is the
in-an-existing-transaction variant for callers that compose with a
larger write.

## Append-only invariant

The application layer exposes ONE writer (`append_signed_event`) and
ZERO mutators — there are no `UPDATE signed_events` or
`DELETE FROM signed_events` statements anywhere in the production
code path. Operators that need to prune (compliance retention, disk
pressure) MUST do so via direct SQL with explicit awareness that
they are breaking the audit chain.

The substrate intentionally does NOT add SQLite triggers enforcing
append-only — trigger-based enforcement would also fire against
operator-driven pruning, defeating the escape hatch. The contract is
enforced at the Rust API surface; the H5 test suite asserts no
`UPDATE signed_events` / `DELETE FROM signed_events` strings appear
in `src/` outside doc comments.

## Operator workflow

1. **Generate an operator keypair** (`ai-memory identity generate
   --agent-id "$(ai-memory identity suggest-id)"`).
2. **Restart the daemon.** The v34 schema migration runs
   automatically on first start and backfills the chain from existing
   `signed_events` rows. The backfill is idempotent; no manual step
   required for a healthy v0.6.x → v0.7.0 upgrade.
3. **Run the verifier daily** (cron / systemd timer):
   ```bash
   ai-memory verify-signed-events-chain --format json | \
     jq -e '.chain_holds' >/dev/null || \
     pager "signed-events chain broken on $(hostname)"
   ```
   A non-zero exit indicates either tampering or a v0.7.0-alpha row
   that didn't make it through the backfill — investigate
   immediately.
4. **Pair with the forensic bundle** (L2-5,
   [`docs/forensic-export.md`](forensic-export.md)) — the signed
   events table ships inside the bundle by default. Offline reviewers
   can re-verify the chain without DB access.

## Chain verification recipe (end-to-end)

For a routine integrity check (post-migration, post-incident, or
scheduled audit):

```bash
# 1. Read current chain tail
TAIL=$(sqlite3 "$AI_MEMORY_DB" "SELECT MAX(sequence) FROM signed_events;")
echo "Current chain tail: $TAIL"

# 2. Full walk (cold start; expensive for >1M rows)
ai-memory verify-signed-events-chain --format json > /tmp/verify-report.json
jq '{rows_checked, chain_holds, chain_break, signature_failures}' /tmp/verify-report.json

# 3. Incremental walk (verify only what's new since last cron tick)
LAST_VERIFIED=$(cat /var/lib/ai-memory/last-verified-seq 2>/dev/null || echo 0)
ai-memory verify-signed-events-chain --since "$LAST_VERIFIED" --format json \
  | jq -r 'if .chain_holds then "OK at \(.rows_checked) rows" else "BROKEN at \(.chain_break)" end'

# 4. On success, advance the watermark
if jq -e '.chain_holds' /tmp/verify-report.json > /dev/null; then
  echo "$TAIL" > /var/lib/ai-memory/last-verified-seq
fi
```

For a forensic walk after suspected tamper (the chain returns
`chain_break: <N>` and the operator needs to know what changed):

```bash
# 1. Identify the broken row
SUSPECT=$(jq -r .chain_break /tmp/verify-report.json)

# 2. Dump that row and its neighbours
sqlite3 -header "$AI_MEMORY_DB" "
  SELECT sequence, id, agent_id, event_type, hex(payload_hash) AS payload_hex,
         hex(prev_hash) AS prev_hex, timestamp
  FROM signed_events
  WHERE sequence BETWEEN $SUSPECT - 2 AND $SUSPECT + 2
  ORDER BY sequence;
"

# 3. Cross-check with the JSONL audit log
grep -F "\"sequence\":${SUSPECT}" /var/log/ai-memory/audit.log
```

The JSONL log carries an independent prev_hash chain (per F2), so a
tamper that hits both must have happened at write-time, not
post-hoc — a strong signal for incident response.

## Key rotation procedure

`signed_events.signature` is Ed25519 over the source row's
canonical-CBOR bytes; the key material is the agent's identity
keypair (`~/.config/ai-memory/identities/<agent-id>.key`). Rotation:

1. **Generate the new keypair**:
   ```bash
   ai-memory identity generate --agent-id "<id>" --rotate
   ```
   The `--rotate` flag preserves the old key under
   `<id>.key.rotated-<timestamp>` for re-verification of historical
   rows.
2. **Restart the daemon** so the new key is the active signer.
3. **Verify historical chain still holds** under the rotated key set:
   ```bash
   ai-memory verify-signed-events-chain --format json | jq .signature_failures
   ```
   An empty `signature_failures` array means the verifier knows about
   both keys and every row's signature checks out.
4. **Audit the rotation transition** by querying the row range that
   straddles the rotation timestamp; both signatures should be
   present (old for pre-rotation rows, new for post-).
5. **Destroy the old key material** ONLY after the rotation window
   has soaked and the chain walk has confirmed `chain_holds == true`
   on multiple post-rotation runs. The old key is needed to verify
   pre-rotation signatures.

**Important**: rotation does NOT change `prev_hash` or `sequence`
values. The chain integrity is independent of the signing key — a
key rotation is invisible to the chain walk except via the per-row
signature verification.

## Audit-trail forensic analysis

When the auditor needs to reconstruct "what happened in the
window 2026-05-15T08:00Z to 2026-05-15T09:00Z":

```sql
-- 1. Pull every signed event in the window
SELECT sequence, id, agent_id, event_type, attest_level, timestamp,
       LENGTH(signature) > 0 AS signed,
       hex(substr(prev_hash, 1, 8)) AS prev_hash_prefix
FROM signed_events
WHERE timestamp BETWEEN '2026-05-15T08:00:00Z' AND '2026-05-15T09:00:00Z'
ORDER BY sequence;
```

```bash
# 2. Verify just that window via --since
START_SEQ=$(sqlite3 "$AI_MEMORY_DB" \
  "SELECT MIN(sequence) FROM signed_events WHERE timestamp >= '2026-05-15T08:00:00Z';")
ai-memory verify-signed-events-chain --since "$((START_SEQ - 1))" --format json

# 3. Cross-reference with the on-disk audit log for the same window
jq -c 'select(.timestamp >= "2026-05-15T08:00:00Z" and .timestamp < "2026-05-15T09:00:00Z")' \
  /var/log/ai-memory/audit.log

# 4. Diff the SQL count against the JSONL count — they MUST match
SQL_COUNT=$(sqlite3 "$AI_MEMORY_DB" \
  "SELECT COUNT(*) FROM signed_events WHERE timestamp BETWEEN '2026-05-15T08:00:00Z' AND '2026-05-15T09:00:00Z';")
JSONL_COUNT=$(jq -c 'select(.timestamp >= "2026-05-15T08:00:00Z" and .timestamp < "2026-05-15T09:00:00Z")' \
  /var/log/ai-memory/audit.log | wc -l)
echo "SQL: $SQL_COUNT, JSONL: $JSONL_COUNT"
```

A divergence between the two counts is high-signal: either the
substrate dropped a JSONL flush (rare; F2 hardened this) or
post-write tampering removed an entry from one of the two surfaces.

## Tuning guidance

The signed-events substrate has very few operator knobs by design —
the chain integrity property is binary and should not be tunable.
The operationally-relevant choices:

| Choice | Recommended value | Rationale |
|---|---|---|
| `verify-signed-events-chain` cadence | Daily for healthy substrate; on every restart for regulated tenant | Daily is enough to catch hostile tamper before it accumulates; per-restart catches a hostile boot. |
| `--since` watermark | Persist last successful chain tail to a file | Incremental walks are O(rows since last verify); cold walks are O(total rows). |
| Retention | Indefinite (default); operator-pruned by SQL when disk pressure mandates | Each row is ~200-300 bytes; 1M rows ≈ 250 MB. A pruning event is a chain break — log it. |
| Postgres mirror | Enable when running multi-host with a shared substrate | Postgres ladder ran one schema-step behind; check `migrations/postgres/` for the matched migration before flipping. |

## Troubleshooting

| Symptom | Likely cause | Diagnostic recipe |
|---|---|---|
| `verify-signed-events-chain FAIL: chain break at sequence=N` | Tamper, partial backfill, or operator-issued DELETE | Inspect rows N-2..N+2 (recipe above); check operator change-log for SQL writes. |
| `read_chain_head: signed_events row(s) have sequence IS NULL` | v34 backfill incomplete | Re-run `ai-memory migrate`; the backfill is idempotent. Daemon refuses to append until repaired. |
| New rows fail with `SQLITE_CONSTRAINT_UNIQUE` on sequence | Two writers raced past the chain head | Confirms PE-3 hook isn't wired; check `tests/deferred_audit_soak.rs` for the correct boot pattern. |
| `signature_failures` non-empty after key rotation | Old key not retained for historical verification | Restore old key under `<id>.key.rotated-<timestamp>`; re-verify. |
| Verifier slow on large substrates | Cold walk over millions of rows | Use `--since <last-verified>` to skip pre-verified prefix. |
| Postgres deployment: `prev_hash` column missing | Postgres migration ladder is one step behind sqlite | Check `migrations/postgres/0015_v07_signed_events_chain.sql` is applied. |
| JSONL audit log count diverges from SQL count | Either dropped JSONL flush or post-write tamper | Investigate per the forensic recipe; F2 hardened JSONL durability but a divergence is high-signal. |

## Operator runbook (3am procedures)

**Chain break detected at runtime.** First, stop appending —
suspected tamper is worse than a brief audit-write outage. Set the
daemon to read-only via the runtime flag if available, otherwise
stop accepting writes at the load balancer. Then:

1. Pull the broken row + neighbours via the SQL recipe.
2. Cross-reference with the JSONL log for the same `sequence`. If
   the JSONL still has the row but SQL is broken, the tamper happened
   in SQL after the JSONL flush — strong signal.
3. Pull the forensic bundle (`docs/forensic-export.md`) for the
   incident window. The bundle is independently re-verifiable.
4. Decision: roll back to the most recent verified chain tail (lose
   N rows of audit history), or fork the chain into a "post-tamper"
   substrate and reconcile manually. Both are operator-policy calls.
5. After remediation, run a full `--since 0` walk to confirm
   `chain_holds == true` end-to-end before re-opening writes.

**Key rotation went sideways — chain still walks but signatures
fail on old rows.** The pre-rotation key was destroyed before
verification. Recover from the off-host key backup (every operator
runbook should have one). If no backup exists, the affected rows are
not signature-verifiable but the chain itself is still tamper-
evident — the operator notes the rotation-debt in the incident log
and moves on.

**Partial backfill stuck in a v0.7-alpha → v0.7.0 upgrade.** The
COR-9 diagnostic fires and the daemon refuses appends. Run
`ai-memory migrate` explicitly:
```bash
ai-memory migrate --target 34
```
The `migrate_v34_backfill_chain` function is idempotent — re-running
is safe. After it completes, `verify-signed-events-chain --format
json` should report `chain_holds: true` over the full row count.

**Audit log + SQL chain divergence.** Treat as high-signal. Pull
both logs for the divergence window, file an incident, and roll out
the forensic-export bundle to an offline reviewer. The substrate's
defense-in-depth design assumes any single surface can be tampered
— it's the *agreement* between SQL chain + JSONL log + per-edge
signatures that pins the audit story.

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
[`docs/internal/v070-feature-inventory.md` §"Feature: Signed events V-4 closeout"](internal/v070-feature-inventory.md),
the federation hardening layer that produces peer-write events on
this chain at [`docs/federation.md`](federation.md), the K10 approvals
path whose decisions are recorded as `signed_events` rows at
[`docs/k10-sse-approvals.md`](k10-sse-approvals.md), the
forensic-bundle exporter at [`docs/forensic-export.md`](forensic-export.md),
the hook pipeline whose gated writes generate signed-event rows at
[`docs/hook-pipeline.md`](hook-pipeline.md), the K8 quotas substrate
whose refusals are also audit events at
[`docs/k8-quotas.md`](k8-quotas.md), and the sidechain transcripts
whose store-events appear in the chain at
[`docs/sidechain-transcripts.md`](sidechain-transcripts.md).
