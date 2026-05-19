# PostgreSQL + Apache AGE operator guide (ai-memory v0.7.0)

> **Audience.** Operators running `ai-memory` who want PostgreSQL as the
> live storage backend, with Apache AGE for graph queries and pgvector
> for semantic recall. **As-of v0.7.0**, postgres+AGE is a first-class
> backend — `ai-memory serve --store-url postgres://…` is the supported
> production deployment shape.
>
> If you only want sqlite, you don't need any of this — the default
> `ai-memory serve` continues to work exactly as it did in v0.6.x. The
> postgres path is opt-in.
>
> See also: [`migration-v0.7.0-postgres.md`](migration-v0.7.0-postgres.md)
> for the sqlite → postgres migration runbook, and
> [`RUNBOOK-adapter-selection.md`](RUNBOOK-adapter-selection.md) for
> the adapter-selection design notes.

## Why postgres+AGE

ai-memory's sqlite path is fast, simple, and has zero operational
overhead — for a single workstation or a single daemon, it is the right
choice. Switch to postgres+AGE when one or more of these is true:

- **Multi-tenant scale.** A single sqlite file behind a single
  `Mutex<Connection>` becomes the bottleneck once you cross ~5
  concurrent writers. Postgres' MVCC removes that ceiling.
- **Larger than RAM.** sqlite + HNSW keeps the full vector index in
  memory. pgvector's HNSW lives on disk and pages on demand —
  practical for 10M+ memory corpora.
- **AGE Cypher KG.** ai-memory's KG operations (`kg_query`,
  `kg_timeline`, `kg_invalidate`, `find_paths`) compile to native
  Cypher on AGE, which beats the sqlite recursive-CTE fallback by
  ≥30% at depth=5 on the canonical 1k-entity / 5k-edge corpus
  (S76 perf gate). The deeper the graph, the wider the gap.
- **Multi-daemon A2A.** Two or more `ai-memory serve` processes
  sharing the same store. Postgres is the supported topology;
  sqlite-over-NFS is not.

The two backends have **schema parity at v28** as of v0.7.0 — every
feature that works on sqlite works on postgres.

## Prerequisites

| Component | Version | Notes |
|---|---|---|
| PostgreSQL | 16.x (16.4+ recommended) | 15.x works for the SAL adapter but Apache AGE 1.5.x targets PG 16. |
| Apache AGE | 1.5.0 | Built from source against your PG 16. |
| pgvector | 0.7.x or 0.8.x | 0.8 is preferred (faster HNSW); 0.7 is fine. |
| ai-memory | v0.7.0 with `--features sal-postgres` | The sql-postgres feature is **off by default** to keep the no-postgres build hermetic. |

> **AGE 1.5 + PG 16 cypher-binding compat.** AGE 1.5.0 has a known
> quirk where parameter binding against `cypher()` plus PostgreSQL 16's
> stricter `prepare`-path causes a "could not find parameter $N"
> error in some bind shapes. ai-memory's production code interpolates
> parameters into the Cypher string through the AGE-recommended
> `cypher()` SQL-function form and never hits this — but if you write
> your own SQL probes, prefer the SQL-function form. Wave 1 Stream C
> fixed our equivalence test harness to use the safe form; production
> code never needed the fix.

## Install — Ubuntu 24.04 example

```bash
# 1. PostgreSQL 16 from PGDG.
sudo apt install -y curl ca-certificates gnupg lsb-release
sudo install -d /usr/share/postgresql-common/pgdg
sudo curl -fsSL https://www.postgresql.org/media/keys/ACCC4CF8.asc \
     -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc
echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] \
     https://apt.postgresql.org/pub/repos/apt $(lsb_release -cs)-pgdg main" \
     | sudo tee /etc/apt/sources.list.d/pgdg.list
sudo apt update
sudo apt install -y postgresql-16 postgresql-server-dev-16 \
                    postgresql-contrib-16 build-essential bison flex git

# 2. pgvector 0.8.0 from the upstream release tag.
git clone --depth 1 --branch v0.8.0 https://github.com/pgvector/pgvector.git
cd pgvector
sudo make USE_PGXS=1 PG_CONFIG=/usr/lib/postgresql/16/bin/pg_config install
cd ..

# 3. Apache AGE 1.5.0 from source against PG 16.
git clone --depth 1 --branch PG16/v1.5.0 https://github.com/apache/age.git
cd age
sudo make PG_CONFIG=/usr/lib/postgresql/16/bin/pg_config install
cd ..

# 4. Restart postgres to pick up the shared libraries.
sudo systemctl restart postgresql@16-main
```

For RHEL / Fedora / Amazon Linux: replace the `apt` lines with the
PGDG yum repo equivalents and ensure `postgresql16-devel` /
`postgresql16-contrib` are installed before building AGE.

## Database setup

```bash
sudo -u postgres psql <<'SQL'
CREATE ROLE aimemory WITH LOGIN PASSWORD 'changeme-please';
CREATE DATABASE aimemory OWNER aimemory;
\c aimemory
CREATE EXTENSION IF NOT EXISTS age;
CREATE EXTENSION IF NOT EXISTS vector;
GRANT USAGE ON SCHEMA ag_catalog TO aimemory;
GRANT ALL ON ALL TABLES IN SCHEMA public TO aimemory;
ALTER DATABASE aimemory SET search_path = ag_catalog, "$user", public;
SQL
```

Notes:

- `aimemory` is the role the daemon runs as. Pick a strong password and
  store it in your secret manager — the daemon reads it from the
  `--store-url` URL or the `AI_MEMORY_STORE_URL` env var.
- `ag_catalog` must be on the search path so AGE's `cypher()` SQL
  function resolves without a schema prefix on every call.
- The role only needs `USAGE` on `ag_catalog` (read of the AGE
  function definitions); the AGE projection objects ai-memory creates
  live in the `aimemory` schema by default.

## Bootstrap the schema

`ai-memory schema-init` (Wave 1 Stream B deliverable) is the supported
way to bootstrap a fresh postgres backend:

```bash
ai-memory schema-init --store-url postgres://aimemory:changeme-please@localhost:5432/aimemory
```

What it does:

1. Connects to the `--store-url`.
2. Probes for `age` and `vector` extensions; refuses to proceed with a
   helpful error if either is missing.
3. Runs `src/store/postgres_schema.sql` (idempotent — `CREATE TABLE
   IF NOT EXISTS` throughout) up to the v28 schema marker.
4. If AGE is present, creates the AGE graph (`ai_memory_kg`) and
   primes the projection labels (entity, memory) and edge types
   (related_to, supersedes, contradicts, derived_from).
5. Records `schema_version=28` in the `_ai_memory_schema_version`
   table.

Idempotent on rerun — safe to invoke from a deploy script. Exit code
0 on success, 2 on missing prerequisites, 1 on transient connection
error.

If you'd rather not install the AGE projection (e.g. you don't need
KG queries), run `ai-memory schema-init --skip-age` and the recursive
CTE fallback will be used for `kg_query`/`kg_timeline`/etc.

> **Pre-Wave-1 fallback.** Until Wave 1 Stream B's commit lands, you
> can bootstrap by running the migration tool against a fresh empty
> postgres (which uses the same `INIT_SCHEMA` path internally) — see
> `migration-v0.7.0-postgres.md` for that workflow.

## Daemon configuration

Pass the store URL as a CLI flag on `serve` (this is the supported
shape in v0.7.0; env-var and config-file forms are tracked for a
follow-on release):

```bash
ai-memory serve --store-url postgres://aimemory:PASSWORD@HOST:5432/aimemory
```

URL shapes accepted by `--store-url`:

- `postgres://user:pass@host:port/dbname`
- `postgresql://user:pass@host:port/dbname` (alias)
- `sqlite:///absolute/path/to/file.db` (also valid — same semantics as `--db`)

`--db` and `--store-url` are **mutually exclusive**. Passing both
when `--db` is explicit (set on the command line OR via the
`AI_MEMORY_DB` env var) errors at startup:

```
Error: --db and --store-url are mutually exclusive. Pass exactly one.
       Got --db=/var/lib/ai-memory/db.sqlite and
       --store-url=postgres://aimemory:...@10.20.0.4:5432/aimemory
```

When `--store-url` is **unset**, the daemon falls back to its sqlite
path (`AI_MEMORY_DB` or `--db`'s default). This preserves
byte-for-byte v0.6.x behavior on the default build.

The daemon logs the resolved backend at startup. For postgres:

```
INFO  ai_memory::daemon_runtime: Wave-3: opening Postgres SAL store at postgres://aimemory:...
WARN  ai_memory::daemon_runtime: v0.7.0 Wave-3: postgres-backed daemon — handlers
       that have not yet migrated to the SAL trait surface 501 Not Implemented.
```

A second probe of `/api/v1/capabilities` confirms it:

```bash
curl -s http://HOST:9077/api/v1/capabilities | jq .storage_backend
# "postgres"
```

If AGE is detected, KG queries dispatch through the Cypher path; if
not, the fallback recursive-CTE path runs against the
`memory_links` table on postgres exactly as it does on sqlite.

## HTTPS / mTLS configuration

`ai-memory serve` ships with two-layer transport security:

| Layer | Flag                  | Effect                                         |
|------:|-----------------------|------------------------------------------------|
|     1 | `--tls-cert <path>`   | Server cert (PEM, ECDSA P-256 or RSA)          |
|     1 | `--tls-key <path>`    | Server private key (PKCS#8, RSA, or SEC1 PEM)  |
|     2 | `--mtls-allowlist <path>` | Per-line SHA-256 fingerprint allowlist for client certs |

Layer 1 alone gives you HTTPS — peers verify the server's identity
through the supplied cert chain. Layer 2 layers mTLS on top: the
server requires every client to present a certificate whose SHA-256
DER fingerprint matches an entry in `--mtls-allowlist`. Combined,
they implement the SSH `known_hosts` pin model — the cert chain is
not the trust anchor; the fingerprint pin is.

### Step 1 — generate a CA, server certs, and client certs

The campaign repo ships a one-shot generator at
[`/tmp/a2a-v07-tls/regen.sh`](https://github.com/alphaonedev/ai-memory-a2a-v0.7.0/blob/main/scripts/...);
the underlying openssl invocations look like:

```sh
# 1. test CA (10y, ECDSA P-256). Substitute a stronger key + a real
#    DN for production deployments.
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out ca.key
openssl req -x509 -new -nodes -key ca.key -days 3650 \
    -subj "/CN=my-ai-memory-fleet" -out ca.pem

# 2. server cert. SANs MUST cover every IP/DNS the daemon listens on.
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out server.key
cat > server.cnf <<EOF
[req]
distinguished_name = dn
req_extensions = v3_req
prompt = no
[dn]
CN = ai-memory.internal
[v3_req]
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment
extendedKeyUsage = serverAuth
subjectAltName = @alt_names
[alt_names]
DNS.1 = ai-memory.internal
IP.1 = 10.20.0.2
IP.2 = 127.0.0.1
EOF
openssl req -new -key server.key -out server.csr -config server.cnf
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
    -out server.pem -days 3650 -extfile server.cnf -extensions v3_req

# 3. client cert(s). Repeat per agent identity; the server allowlist
#    pins each client's fingerprint.
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out client-alice.key
# ... same shape as server.cnf with `extendedKeyUsage = clientAuth`.

# 4. compute each client cert's SHA-256 DER fingerprint.
for c in client-*.pem; do
    fp=$(openssl x509 -in "$c" -noout -fingerprint -sha256 \
         | sed 's/.*=//' | tr -d ':' | tr 'A-F' 'a-f')
    echo "$fp"
done > mtls-allowlist.txt
```

Allowlist format (`src/tls.rs::load_fingerprint_allowlist`):

- one fingerprint per line, hex digits, optional `:` separators
- `sha256:` prefix is accepted
- `#` comments (full-line + inline) are stripped before parsing
- empty lines are ignored

### Step 2 — wire the daemon's systemd unit

Edit `/etc/systemd/system/ai-memory.service` (or its drop-in equivalent)
and append the three flags to the `ExecStart=` line:

```ini
[Service]
ExecStart=/usr/local/bin/ai-memory serve \
    --store-url postgres://aimemory:PWD@10.20.0.4:5432/aimemory \
    --tls-cert /etc/ai-memory/tls/server.pem \
    --tls-key  /etc/ai-memory/tls/server.key \
    --mtls-allowlist /etc/ai-memory/tls/mtls-allowlist.txt
```

Reload + restart:

```sh
systemctl daemon-reload
systemctl restart ai-memory
```

The `deploy_wave4.sh` helper in the campaign repo does this
automatically when invoked with `DEPLOY_TLS=1` (and, for mTLS,
`DEPLOY_MTLS=1` which is the default when DEPLOY_TLS=1):

```sh
DEPLOY_TLS=1 DEPLOY_MTLS=1 \
TLS_LOCAL_DIR=/tmp/a2a-v07-tls \
TLS_REMOTE_DIR=/etc/ai-memory-a2a/tls \
scripts/deploy_wave4.sh
```

### Step 3 — verify with curl

```sh
# (from the same VPC; --resolve picks the right SAN)
curl -sS \
    --cacert /etc/ai-memory/tls/ca.pem \
    --cert   /etc/ai-memory/tls/client-alice.pem \
    --key    /etc/ai-memory/tls/client-alice.key \
    https://ai-memory.internal:9077/api/v1/capabilities | jq .storage_backend
# "postgres"
```

A failed handshake (wrong CA, missing client cert, fingerprint not on
allowlist) surfaces as a curl `(35) error:0A000412:SSL routines::sslv3
alert bad certificate` — the server log carries the structured reason.

### Step 4 — wire the campaign harness

The harness picks a per-agent client cert from env vars:

```sh
export TLS_MODE=mtls
export TLS_CA_PEM=/etc/ai-memory-a2a/tls/ca.pem
export TLS_CLIENT_CERT_ALICE=/etc/ai-memory-a2a/tls/client-alice.pem
export TLS_CLIENT_KEY_ALICE=/etc/ai-memory-a2a/tls/client-alice.key
export TLS_CLIENT_CERT_BOB=/etc/ai-memory-a2a/tls/client-bob.pem
export TLS_CLIENT_KEY_BOB=/etc/ai-memory-a2a/tls/client-bob.key
# ... per-agent pairs for every identity the campaign uses.
# Optional process-wide default (used when no per-agent var matches):
export TLS_DEFAULT_CLIENT_CERT=/etc/ai-memory-a2a/tls/client-default.pem
export TLS_DEFAULT_CLIENT_KEY=/etc/ai-memory-a2a/tls/client-default.key
```

The agent suffix is the upper-cased `agent_id` with non-alphanumerics
collapsed to `_` (so `ai:alice@nyc3-droplet-1` becomes
`AI_ALICE_NYC3_DROPLET_1`). See `Harness.client_cert_for(agent_id)` in
`scripts/a2a_harness.py`.

Each scenario report now carries a `tls_handshake` block:

```json
{
    "scenario": "20",
    "tls_mode": "mtls",
    "tls_handshake": {
        "count": 17,
        "min_seconds": 0.013,
        "mean_seconds": 0.022,
        "max_seconds": 0.054,
        "total_seconds": 0.379
    }
}
```

Plain-HTTP runs omit the block. The orchestrator's perf-vs-baseline
diff (Phase 7 of the cert closure) reads `tls_handshake.total_seconds`
to compute the per-scenario TLS overhead.

### Cert closure run reference

The Continuation-6 cert closure run (commit `<orchestrator-fills-in>`)
exercises this configuration end-to-end against the wave-4 droplet
fleet. Compare its `runs/<id>/aggregate.json` against the plain-HTTP
baseline at
[`runs/v0.7.0-a2a-wave4r2-r1-20260509-1858/`](https://github.com/alphaonedev/ai-memory-a2a-v0.7.0/tree/main/runs/v0.7.0-a2a-wave4r2-r1-20260509-1858)
to read the perf overhead.

## Operator surfaces that "just work" identically on both backends

The point of the SAL trait is that no caller needs to know which
backend is mounted. As of v0.7.0 Wave-3 + Wave-3 Continuation the
following **HTTP endpoints** route through the SAL trait
identically on sqlite and postgres:

### Core CRUD (Wave-3 Phase 3 — commit `c049500`)

| HTTP method | Path | Trait method |
|---|---|---|
| `POST` | `/api/v1/memories` | `MemoryStore::store` |
| `GET` | `/api/v1/memories/:id` | `MemoryStore::get` + `list_links` |
| `PUT` | `/api/v1/memories/:id` | `MemoryStore::update` |
| `DELETE` | `/api/v1/memories/:id` | `MemoryStore::delete` |
| `GET` | `/api/v1/memories` | `MemoryStore::list` |
| `GET` | `/api/v1/search` | `MemoryStore::search` |
| `POST` | `/api/v1/links` | `MemoryStore::link_signed` |
| `GET` | `/api/v1/memories/:id/links` | `MemoryStore::list_links` |
| `GET` | `/api/v1/capabilities` | reports `storage_backend` |
| `GET` | `/api/v1/health` | (no storage I/O) |

### Wave-3 Continuation (Phase 4 + 5)

| HTTP method | Path | SAL dispatch |
|---|---|---|
| `POST` | `/api/v1/memories/bulk` | streams each row through `MemoryStore::store` |
| `GET` | `/api/v1/agents` | projects `_agents` namespace via `MemoryStore::list` |
| `POST` | `/api/v1/agents` | `MemoryStore::register_agent` |
| `GET` | `/api/v1/namespaces` | aggregates from `MemoryStore::list` |
| `GET` | `/api/v1/stats` | counts + per-tier histogram via `MemoryStore::list` |
| `GET` | `/api/v1/taxonomy` | flat per-namespace tree via `MemoryStore::list` |
| `GET` | `/api/v1/archive` | projects from `archived_memories` (postgres helper) |
| `GET` | `/api/v1/archive/stats` | aggregates from `archived_memories` (postgres helper) |
| `POST` | `/api/v1/entities` | `MemoryStore::store` with `metadata.kind=entity` |
| `GET` | `/api/v1/entities/by_alias` | walks namespace via `MemoryStore::list` + alias match |
| `GET` | `/api/v1/pending` | empty list with `storage_backend: postgres` note |
| `POST` | `/api/v1/kg/query` | `PostgresStore::kg_query` (AGE Cypher / CTE fallback) |
| `GET` | `/api/v1/kg/timeline` | `PostgresStore::kg_timeline` |
| `POST` | `/api/v1/kg/invalidate` | `PostgresStore::kg_invalidate` |
| `GET` | `/api/v1/inbox` | empty list with structured note |
| `GET` | `/api/v1/subscriptions` | empty list with structured note |
| `POST` | `/api/v1/check_duplicate` | structured no-match envelope (semantic scan is sqlite-only) |

### Wave-3 Continuation 2 (Phase 8 + 9 + 10 + 11)

The four critical surfaces gated for v0.7.0 land here. After
Continuation 2, postgres-backed daemons run as first-class peers in
federation, fire the same audit chain as sqlite, run the full
hybrid recall pipeline, and accept governance write paths.

| HTTP method | Path | SAL dispatch |
|---|---|---|
| `POST` | `/api/v1/sync/push` | per-row `MemoryStore::apply_remote_memory` / `apply_remote_link` / `apply_remote_deletion`. Heterogeneous federation (sqlite ↔ postgres) round-trips. |
| `GET` | `/api/v1/sync/since` | `MemoryStore::list_memories_updated_since` |
| `GET` | `/api/v1/recall` and `POST /api/v1/recall` | `MemoryStore::recall_hybrid` (FTS + pgvector cosine + adaptive blend; mode=hybrid when embedder loaded). Touch ops fire via `MemoryStore::touch_after_recall`. |
| `POST` | `/api/v1/pending/{id}/approve` | (Continuation 3 / Phase 20) full consensus state machine via `MemoryStore::governance_approve_with_consensus` — Human / Agent(required) / Consensus(N) variations + registered-agent gating + threshold transition. |
| `POST` | `/api/v1/pending/{id}/reject` | `MemoryStore::pending_decide(approve=false)` + audit emit. |
| `POST` | `/api/v1/namespaces/{ns}/standard` and `POST /api/v1/namespaces` | auto-seeds placeholder via `MemoryStore::store`, then `MemoryStore::set_namespace_standard` |
| `DELETE` | `/api/v1/namespaces/{ns}/standard` and `DELETE /api/v1/namespaces` | `MemoryStore::clear_namespace_standard` |

### Wave-3 Continuation 3 (Phase 13 + 14 + 15 + 16 + 17 + 18 + 19 + 20)

The eight remaining sqlite-only surfaces land here. After
Continuation 3, **every** HTTP endpoint that works on a sqlite-backed
daemon also works on a postgres-backed daemon — there is no residual
501 envelope on standard endpoints (the route gate keeps the 501
envelope as a safety net for unknown / future endpoints).

| HTTP method | Path | SAL dispatch |
|---|---|---|
| `POST` | `/api/v1/forget` | `MemoryStore::forget` — namespace + ILIKE pattern + tier filters; archive-on-forget moves rows to `archived_memories` with `archive_reason='forget'` before deletion. |
| `POST` | `/api/v1/consolidate` | `MemoryStore::consolidate` — atomic transaction merges sources (tags / metadata / max-priority / sum-access_count), preserves `consolidated_from_agents` provenance, deletes source rows. |
| `GET` | `/api/v1/contradictions` | `MemoryStore::list` + `MemoryStore::list_links` — non-LLM heuristic (pairwise differing-content detection) runs identically on both backends. |
| `POST` | `/api/v1/notify` | `MemoryStore::notify` — lands a memory in `_inbox/<target>` with `metadata.target_agent_id`. |
| `POST` | `/api/v1/gc` | `MemoryStore::run_gc` — deletes (or archives-then-deletes) every row whose `expires_at` is in the past. |
| `POST` | `/api/v1/import` | `MemoryStore::store` per memory + `MemoryStore::link` per link. |
| `GET` | `/api/v1/export` | `MemoryStore::export_memories` + `MemoryStore::export_links`. |
| `POST` | `/api/v1/archive` | `MemoryStore::archive_by_ids` — preserves original tier + expiry + embedding via `original_tier`/`original_expires_at` columns. |
| `DELETE` | `/api/v1/archive` | `MemoryStore::archive_purge`. |
| `POST` | `/api/v1/archive/{id}/restore` | `MemoryStore::archive_restore` — atomic move back to active; rejects (Conflict) when the id already exists in active memories. |
| `POST` | `/api/v1/memories` (writes only) | inheritance-chain walk via `MemoryStore::enforce_governance_action` BEFORE store; Allow/Deny/Pending decisions surface as 201/403/202 respectively. |

### Wave-3 Continuation 6 — F7 closure (S52, S61, S65)

Three new HTTP endpoints close the Wave 4 cert-harness gaps surfaced
post-Continuation-5. Each routes through the SAL trait so postgres-backed
and sqlite-backed daemons project byte-identical wire shapes.

| HTTP method | Path | SAL dispatch |
|---|---|---|
| `POST` | `/api/v1/quota/status` | `MemoryStore::quota_status(agent_id)` (single-agent) or `MemoryStore::quota_status_list()` (operator-facing list). Postgres reads from the `agent_quotas` table directly — no fallthrough to the empty scratch sqlite. Auto-inserts the default row on first call. Body `{agent_id?, namespace?}`; returns the canonical `QuotaStatus` projection (`max_memories_per_day`, `max_storage_bytes`, `max_links_per_day`, `current_*`, `day_started_at`, ...). |
| `POST` | `/api/v1/kg/find_paths` | `MemoryStore::find_paths(source, target, max_depth?, max_results?)`. SQLite uses the recursive CTE in `db::find_paths`; Postgres dispatches AGE Cypher when the extension is installed and falls back to a SQL recursive CTE otherwise. Body `{source_id, target_id, max_depth?, max_results?}`; returns `{paths: [[id, ...], ...], count, source_id, target_id}`. 422 when `max_depth` exceeds the supported ceiling. |
| `POST` | `/api/v1/links/verify` | `MemoryStore::verify_link(VerifyFilter)`. Resolves the `(source, target?, relation?)` triple from the body and re-verifies the canonical-CBOR signature against the enrolled peer key when one is present. Body `{source_id?, target_id?, link_id?}` — at least one of `source_id` or `link_id` is required (`link_id` format is `source_id|target_id|relation`). Returns `{verified, attest_level, signature_present, observed_by, source_id, target_id, relation, findings}`. |

#### Phase 20 — full governance pipeline

Postgres-backed daemons now run the **full** governance pipeline that
sqlite-backed daemons run:

- `MemoryStore::build_namespace_chain` walks `namespace_meta` for
  explicit-parent ancestors (bounded by 8 hops + cycle-safe) +
  `/`-derived hierarchy.
- `MemoryStore::resolve_governance_policy` walks leaf-first and
  returns the most-specific policy with `inherit=true` honored.
- `MemoryStore::enforce_governance_action` evaluates Any /
  Registered / Owner / Approve levels; `Approve` queues a
  `pending_actions` row and returns the canonical pending id.
- `MemoryStore::governance_approve_with_consensus` runs the full
  multi-vote state machine: Human accepts any caller, Agent requires
  the named id, Consensus requires registered-agent voters with
  case-insensitive duplicate-vote dedup; threshold transition stamps
  `decided_by` + `decided_at` and returns Approved.

The post-approval `execute_pending_action` payload-replay path
remains sqlite-only — postgres operators on Approved actions
re-issue the underlying write via the standard CRUD path. This is
the only residual scope difference between sqlite and postgres for
governance.

Federation fanout for governance decisions / archive / restore /
purge stays sqlite-only (the `broadcast_*_quorum` paths use
sqlite-coupled fed-tracker state); postgres operators relying on
multi-node consistency for these subcollections should poll peers
or pin to sqlite for v0.7.0.

The audit module is file-based with no SQLite coupling, so
`ai-memory audit verify --audit-dir <path>` works on a
postgres-backed daemon's log unchanged. The F2 fix
(cross-restart sequence persistence via the chain-tail walk in
`audit::init`) lights up for postgres through the new emit sites
in `create_memory` / `delete_memory` / `create_link` /
`approve_pending` / `reject_pending` / `sync_push`.

The full hybrid recall pipeline mirrors `db::recall_hybrid` (sqlite
path) over pgvector + tsvector + ts_rank: 6-factor FTS sub-score
(priority \* 0.5 + min(access_count, 50) \* 0.1 + confidence \* 2.0
+ tier_bonus + recency_factor), 0.2 cosine gate, adaptive blend
(`semantic_weight = 0.50` for ≤500 chars, lerp to `0.15` at ≥5000
chars), atomic touch ops (++access_count + TTL extension +
mid→long auto-promotion at 5 accesses + ++priority every 10
accesses).

### Postgres route gate

Wave-3 Continuation also installs a route-gate middleware at the
router layer (`handlers::postgres_route_gate`). On a postgres-backed
daemon, any (method, path) tuple **not** in the supported list above
is short-circuited with a structured 501 envelope before reaching
the legacy SQLite handler — closing the silent-corruption gap where
un-migrated handlers would otherwise read from / write to the empty
in-memory scratch SQLite database that `bootstrap_serve` opens
against `--db`. On sqlite-backed daemons the gate is a pure
pass-through.

### What still returns 501 on postgres

After Wave-3 Continuation 3, **no standard HTTP endpoint** returns
501 on a postgres-backed daemon. Every endpoint listed in the v0.7.0
router (**72 `.route(...)` registrations in `src/lib.rs` at v0.7.0**,
surfaced through `/api/v1/capabilities`) dispatches through the SAL
trait or is handled directly by the postgres adapter.

The route gate retains its 501 envelope as a safety net for:

- Unknown / future endpoints not yet in the allow-list.
- Operator-installed handlers (custom plugins) that have not been
  classified.

Two **degraded-mode** behaviors remain documented but do not block
endpoint availability:

- `execute_pending_action` payload-replay (the post-Approved write
  fanout) — sqlite-only. Postgres-backed daemons return 200 +
  `{approved: true, executed: false}` on consensus threshold; the
  approving caller re-issues the underlying write via the standard
  CRUD path. The full state machine runs identically on both
  backends — only the auto-replay step is gated.
- Federation fanout subcollections that ride sqlite-only fed-tracker
  state: archive / restore / purge / pending-decision broadcast.
  Memories / deletions / links / sync_push core round-trip through
  the trait. Postgres operators relying on multi-node consistency
  for these subcollections should poll peers or pin sqlite for v0.7.0.

Schema parity at v28 means a future `migrate sqlite → postgres`
carries every row across cleanly.

The recall **score breakdown** is the same 6-factor formula on both
backends:

```
score = semantic_weight * cosine
      + (1 - semantic_weight) * fts_norm
      + priority * 0.05
      + access_count * 0.01
      + confidence * 0.20
      + tier_bonus
      + recency_factor
```

(Wave 1 Stream A's `tests/recall_scoring_parity.rs` pins this
contract — same query, same top-K, same per-factor breakdown
within FP tolerance.)

## Performance notes

### pgvector HNSW

The postgres adapter creates an HNSW index on `memories.embedding`
during `schema-init`:

```sql
CREATE INDEX IF NOT EXISTS memories_embedding_hnsw
    ON memories USING hnsw (embedding vector_cosine_ops);
```

Default tuning is the pgvector default (`m=16`, `ef_construction=64`).
For corpora >1M memories, raising `ef_construction` to 128 at index
build time and `hnsw.ef_search` to 80 at query time is the standard
recommendation; the v0.7.0 release does not yet expose these as
`ai-memory schema-init` flags — set them via SQL post-bootstrap.

### AGE Cypher vs CTE fallback

The four KG operations dispatch on the `KgBackend` tag the postgres
adapter probes at connect time:

| Op | AGE 1.5 (Cypher) | CTE fallback | Speedup at depth=5 |
|---|---|---|---|
| `kg_query` | `MATCH (a)-[*1..d]->(b) WHERE a.id = $1` | recursive `WITH` join | ≥30% (S76 gate) |
| `kg_timeline` | `MATCH ... WHERE valid_from < $1 AND (valid_until IS NULL OR valid_until > $1)` | recursive temporal join | ≥30% |
| `kg_invalidate` | `MATCH ... SET valid_until = $1` | `UPDATE memory_links` | parity |
| `find_paths` | `MATCH p = shortestPath((a)-[*1..d]->(b))` | recursive CTE with cycle detection | 2-5× at depth=5+ |

The S76 perf gate fires if AGE is reported as engaged but the AGE p95
is **not** at least 30% faster than CTE p95 on the canonical 1k-entity
/ 5k-edge corpus. That gate is honest about the AGE-vs-CTE comparison
on the **same** postgres host — comparing AGE-on-postgres to
CTE-on-sqlite is a different question and not the speedup we claim.

### Connection pool

The postgres adapter uses sqlx's connection pool. The v0.7.0 default
is min=2, max=16, idle-timeout=10min. For high-fanout multi-tenant
deployments, raise `max` to 32-64; for single-daemon deployments the
defaults are appropriate. Pool size is exposed via `AI_MEMORY_PG_POOL_MAX`
and `AI_MEMORY_PG_POOL_MIN` env vars.

## Troubleshooting

### "extension age is not installed"

`ai-memory schema-init` exits 2 if AGE isn't present. Either install
AGE (see "Install" above) or pass `--skip-age` to bootstrap with the
CTE fallback. The `vector` extension is **not** optional — pgvector
is required for embeddings.

### "schema_version=15 detected, expected 28"

You're pointing at a v0.7-alpha postgres database. Run `ai-memory
migrate --from postgres://… --to postgres://… --in-place` to apply
the v15→v28 migrations idempotently. (See
`migration-v0.7.0-postgres.md` for the full migration guide.)

### "could not find parameter $N" against a Cypher query

This is the AGE 1.5 + PG 16 binding quirk mentioned above. ai-memory's
production code never hits it — if you see it, you're running a
custom psql probe. Use the AGE-recommended SQL-function shape:

```sql
SELECT * FROM cypher('ai_memory_kg', $$
  MATCH (a)-[r:RELATED_TO]->(b)
  WHERE a.id = $node_id
  RETURN b
$$, $$ {"node_id": "abc123"} $$) AS (b agtype);
```

### "permission denied for schema ag_catalog"

The `aimemory` role needs `USAGE` on `ag_catalog`. The "Database
setup" section above grants it; if you bootstrapped by hand, run
`GRANT USAGE ON SCHEMA ag_catalog TO aimemory;` as the postgres
superuser.

### Recall scores differ between sqlite and postgres

If you observe this, file an issue and reference
`tests/recall_scoring_parity.rs` (Wave 1 Stream A). The contract is
that the same query returns the same top-K with the same per-factor
score breakdown within FP tolerance. Drift is a regression — the
parity test is the gate that prevents it.

## What's in scope vs out of scope (v0.7.0)

| | sqlite | postgres |
|---|---|---|
| Live daemon | ✓ (default) | ✓ (Wave 3) |
| Schema parity | v28 | v28 (Wave 2) |
| `link()` | ✓ | ✓ (Wave 1 Stream A) |
| `register_agent()` | ✓ | ✓ (Wave 1 Stream A) |
| Recall 6-factor scoring (SAL `search`) | ✓ | ✓ (Wave 1 Stream A) |
| `kg_query` / `kg_timeline` / `kg_invalidate` / `find_paths` | CTE | AGE Cypher (CTE fallback) — sqlite-bound HTTP handlers in v0.7.0; trait routing in v0.7.x |
| HTTP CRUD on SAL trait (Wave-3 subset) | ✓ | ✓ (8 endpoints — see table above) |
| HTTP read paths (agents/stats/namespaces/taxonomy/archive/entities/inbox/subs) | ✓ | ✓ (Wave-3 Continuation — Phase 4 + 5) |
| HTTP KG handlers (kg_query/kg_timeline/kg_invalidate) | ✓ | ✓ (Wave-3 Continuation — Phase 5) |
| HTTP federation push/pull (sync/push, sync/since) | ✓ | ✓ (Wave-3 Continuation 2 — Phase 8) |
| HTTP audit chain emit + cross-restart sequence persistence | ✓ | ✓ (Wave-3 Continuation 2 — Phase 9) |
| HTTP full hybrid recall pipeline (FTS + pgvector + adaptive blend + touch) | ✓ | ✓ (Wave-3 Continuation 2 — Phase 10) |
| HTTP governance write paths (pending decide, namespace standard) | ✓ | ✓ (Wave-3 Continuation 2 — Phase 11) |
| HTTP full governance pipeline (multi-vote consensus + approver_type + inheritance walk on writes) | ✓ | ✓ (Wave-3 Continuation 3 — Phase 20) |
| HTTP forget / consolidate / contradictions / notify / gc / import / export / archive write paths | ✓ | ✓ (Wave-3 Continuation 3 — Phase 13/14/15/16/17/18/19) |
| `execute_pending_action` payload-replay on Approved | ✓ | sqlite-only — postgres returns `{approved: true, executed: false}`; caller re-issues underlying write |
| Federation fanout subcollections (archive / restore / pending-decision broadcast) | ✓ | sqlite-only fed-tracker state |
| Migration tool both directions | ✓ | ✓ |
| `schema-init` CLI | n/a (auto-create) | ✓ (Wave 1 Stream B) |
| `--store-url <URL>` flag on `serve` | ✓ (sqlite://) | ✓ (postgres://, postgresql://) |
| `--db` and `--store-url` mutual exclusion | ✓ (Wave 3) | ✓ (Wave 3) |
| `/api/v1/capabilities.storage_backend` | ✓ → `"sqlite"` | ✓ → `"postgres"` |
| Cross-backend live replication | ✗ | ✗ (v0.7.1+) |

## References

- Apache AGE 1.5 docs: https://age.apache.org/
- pgvector 0.8 docs: https://github.com/pgvector/pgvector
- ai-memory v0.7.0 release notes: [`v0.7.0/release-notes.md`](v0.7.0/release-notes.md)
- A2A campaign Pages: https://alphaonedev.github.io/ai-memory-a2a-v0.7.0/
- Adapter-selection design: [`RUNBOOK-adapter-selection.md`](RUNBOOK-adapter-selection.md)
- Migration runbook: [`migration-v0.7.0-postgres.md`](migration-v0.7.0-postgres.md)
- F6 issue (in-flight Wave 1 closure): https://github.com/alphaonedev/ai-memory-mcp/issues/646
