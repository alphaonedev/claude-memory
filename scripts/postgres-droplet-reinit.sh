#!/usr/bin/env bash
# postgres-droplet-reinit.sh — reset the v0.7.0 postgres droplet's
# `aimemory` schema state and stand up clean disposable databases for
# integration / live A2A scenarios.
#
# WHEN TO RUN
# -----------
#   * The droplet's primary `aimemory` database was bootstrapped against
#     an older `postgres_schema.sql` and is missing columns / indices
#     that the current build's `INIT_SCHEMA` expects (the symptom
#     surfaced in v0.7.0 Wave-3 Continuation 2: `agent_id_idx` generated
#     column was added after the droplet was first provisioned, so
#     `CREATE INDEX idx_memories_agent_id ON memories (agent_id_idx)`
#     failed before any migration could run).
#   * You need a clean roster of per-scenario disposable databases for
#     a Wave-4-style live A2A re-validation pass and want them all
#     bootstrapped to the same schema version.
#   * A previous run left half-applied schema state and you want to
#     blow it away from a known-good baseline.
#
# WHEN *NOT* TO RUN
# -----------------
#   * On a postgres host that is actively serving production traffic
#     for any tenant. This script destroys data. The pg_dump backup is
#     defense in depth, not a license to skip a maintenance window.
#   * Without first verifying you have a current `ai-memory schema-init`
#     binary on the orchestrator host. Bootstrapping with a stale
#     binary just reproduces the original drift.
#
# WHAT THIS DOES (defense-in-depth ordering)
# ------------------------------------------
#   1. Take a pg_dump custom-format backup of the existing `aimemory`
#      database into /var/backups/. Backups are timestamped and never
#      overwritten.
#   2. DROP / CREATE the primary `aimemory` database and reinstall the
#      `age` + `vector` extensions (extension installs into the new
#      database; they are per-database in PostgreSQL).
#   3. Run `ai-memory schema-init --json` against the fresh database to
#      bootstrap the bundled `postgres_schema.sql`, run pending
#      migrations, and create the `memory_graph` AGE projection. The
#      JSON report is captured for audit + verification.
#   4. CREATE a roster of disposable per-scenario databases
#      (`aimemory_w4_<subset>`) and schema-init each one. These exist
#      so parallel scenario subsets do not pollute each other's state.
#   5. Print a verification summary (table counts, schema_version,
#      extensions, AGE projection state) to stdout for the operator's
#      runbook.
#
# WHAT THIS DOES NOT DO
# ---------------------
#   * Touch `template1`, `postgres`, or any pre-existing operator
#     database (`aimemory_perf*`, `aimemory_kg*`, `aimemory_smoke`, …).
#     Those are left alone so prior testing artifacts survive.
#   * Run the daemon. After this script completes, redeploy / restart
#     `ai-memory.service` on each daemon host so the new schema is
#     picked up by the running process.
#   * Push or commit anything. This is purely an operator runbook
#     helper.
#
# REQUIREMENTS
# ------------
#   * Run on the postgres droplet (or a host with `psql` + `pg_dump`
#     and network access to the postgres host).
#   * `AI_MEMORY_BIN` must point to a v0.7.0 build that supports the
#     `schema-init` subcommand (Wave-1 Fix 3, commit 90b4144 onwards).
#     If the binary is on a different host, set `AI_MEMORY_SSH_HOST`
#     and the script will run schema-init via SSH.
#   * `PG_PASSWORD_FILE` must contain the postgres role password (mode
#     0600). Default: /root/aimemory-pg-password.txt.
#
# USAGE
# -----
#   sudo ./postgres-droplet-reinit.sh                 # default: full reinit
#   sudo ./postgres-droplet-reinit.sh --dry-run       # show plan, take no action
#   sudo ./postgres-droplet-reinit.sh --skip-disposable
#                                                     # only re-init `aimemory`
#
# POST-RUN VERIFICATION
# ---------------------
#   * /tmp/aimemory-schema-init.json should show
#       tables   > 0
#       schema_version == 28   (v0.7.0 expected)
#       extensions includes "age" and "vector"
#       age_projection_created == true
#   * `psql -c "\\dt" aimemory` lists the v0.7.0 table set (memories,
#     memory_links, archived_memories, pending_actions, sync_state,
#     subscriptions, namespace_meta, entity_aliases, schema_version,
#     plus any v0.7.0 additions: agent_registry, audit_log, transcripts,
#     transcript_links, signed_events, agent_quotas, …).
#   * Each disposable database should have an identical schema_version
#     (compare via `SELECT version FROM schema_version` across the
#     roster).
#
# Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration (override via env)
# ---------------------------------------------------------------------------

PG_HOST="${PG_HOST:-10.20.0.4}"
PG_PORT="${PG_PORT:-5432}"
PG_USER="${PG_USER:-aimemory}"
PG_PRIMARY_DB="${PG_PRIMARY_DB:-aimemory}"
PG_PASSWORD_FILE="${PG_PASSWORD_FILE:-/root/aimemory-pg-password.txt}"

BACKUP_DIR="${BACKUP_DIR:-/var/backups}"
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
BACKUP_FILE="${BACKUP_DIR}/aimemory-pre-reinit-${TIMESTAMP}.dump"
SCHEMA_INIT_JSON="${SCHEMA_INIT_JSON:-/tmp/aimemory-schema-init-${TIMESTAMP}.json}"

AI_MEMORY_BIN="${AI_MEMORY_BIN:-/opt/ai-memory-src/target/release/ai-memory}"
AI_MEMORY_SSH_HOST="${AI_MEMORY_SSH_HOST:-}"   # empty => run binary locally

DISPOSABLE_DBS=(
    aimemory_w4_core
    aimemory_w4_federation
    aimemory_w4_kg
    aimemory_w4_audit
    aimemory_w4_governance
    aimemory_w4_recall
    aimemory_w4_subscriptions
    aimemory_w4_smoke
)

DRY_RUN=0
SKIP_DISPOSABLE=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        --skip-disposable) SKIP_DISPOSABLE=1 ;;
        --help|-h)
            sed -n '1,/^set -euo pipefail$/p' "$0" | sed -e 's/^# \?//' -e '$d'
            exit 0
            ;;
        *)
            echo "unknown arg: $1" >&2
            exit 2
            ;;
    esac
    shift
done

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

log() { printf '[%s] %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*"; }

run() {
    if [[ "$DRY_RUN" -eq 1 ]]; then
        log "DRY-RUN: $*"
    else
        log "RUN: $*"
        "$@"
    fi
}

require_password() {
    if [[ ! -r "$PG_PASSWORD_FILE" ]]; then
        echo "FATAL: cannot read postgres password file: $PG_PASSWORD_FILE" >&2
        exit 3
    fi
    PG_PWD="$(cat "$PG_PASSWORD_FILE")"
    export PGPASSWORD="$PG_PWD"
}

psql_postgres() {
    # Run psql as the OS `postgres` superuser via local socket — no
    # password required. Use this for DROP / CREATE DATABASE and
    # CREATE EXTENSION since `aimemory` role is not a superuser.
    if [[ "$DRY_RUN" -eq 1 ]]; then
        log "DRY-RUN: sudo -u postgres psql $*"
    else
        sudo -u postgres psql "$@"
    fi
}

run_schema_init() {
    local db="$1"
    local url="postgres://${PG_USER}:${PG_PWD}@${PG_HOST}:${PG_PORT}/${db}"
    local out="${SCHEMA_INIT_JSON%.json}-${db}.json"
    local cmd

    if [[ -n "$AI_MEMORY_SSH_HOST" ]]; then
        cmd=(ssh "$AI_MEMORY_SSH_HOST" "$AI_MEMORY_BIN" schema-init --store-url "$url" --json)
    else
        cmd=("$AI_MEMORY_BIN" schema-init --store-url "$url" --json)
    fi

    log "schema-init -> ${db} (output: ${out})"
    if [[ "$DRY_RUN" -eq 1 ]]; then
        log "DRY-RUN: ${cmd[*]} | tee ${out}"
        return 0
    fi
    "${cmd[@]}" | tee "$out"
    echo
    # Quick sanity check
    if command -v jq >/dev/null 2>&1; then
        local tables version age_ok
        tables="$(jq -r '.tables | length' "$out" 2>/dev/null || echo 0)"
        version="$(jq -r '.schema_version' "$out" 2>/dev/null || echo unknown)"
        age_ok="$(jq -r '.age_projection_created' "$out" 2>/dev/null || echo unknown)"
        log "  -> tables=${tables} schema_version=${version} age_projection_created=${age_ok}"
    fi
}

# ---------------------------------------------------------------------------
# Step 0 — preflight
# ---------------------------------------------------------------------------

log "postgres-droplet-reinit.sh starting (dry_run=${DRY_RUN}, skip_disposable=${SKIP_DISPOSABLE})"
require_password

if [[ -z "$AI_MEMORY_SSH_HOST" && ! -x "$AI_MEMORY_BIN" ]]; then
    echo "FATAL: ai-memory binary not found at $AI_MEMORY_BIN — set AI_MEMORY_BIN or AI_MEMORY_SSH_HOST" >&2
    exit 4
fi

# ---------------------------------------------------------------------------
# Step 1 — backup
# ---------------------------------------------------------------------------

run mkdir -p "$BACKUP_DIR"
log "step 1: pg_dump ${PG_PRIMARY_DB} -> ${BACKUP_FILE}"
if [[ "$DRY_RUN" -eq 1 ]]; then
    log "DRY-RUN: pg_dump -h ${PG_HOST} -U ${PG_USER} -d ${PG_PRIMARY_DB} -F c -f ${BACKUP_FILE}"
else
    pg_dump -h "$PG_HOST" -U "$PG_USER" -d "$PG_PRIMARY_DB" -F c -f "$BACKUP_FILE"
    if [[ ! -s "$BACKUP_FILE" ]]; then
        echo "FATAL: backup file is empty — aborting before destructive step" >&2
        exit 5
    fi
    log "  -> $(ls -lh "$BACKUP_FILE" | awk '{print $5, $9}')"
fi

# ---------------------------------------------------------------------------
# Step 2 — drop + recreate primary
# ---------------------------------------------------------------------------

log "step 2: drop + recreate ${PG_PRIMARY_DB}"
psql_postgres -c "DROP DATABASE IF EXISTS ${PG_PRIMARY_DB};"
psql_postgres -c "CREATE DATABASE ${PG_PRIMARY_DB} OWNER ${PG_USER};"
psql_postgres -d "$PG_PRIMARY_DB" -c "CREATE EXTENSION IF NOT EXISTS age;"
psql_postgres -d "$PG_PRIMARY_DB" -c "CREATE EXTENSION IF NOT EXISTS vector;"

# ---------------------------------------------------------------------------
# Step 3 — schema-init primary
# ---------------------------------------------------------------------------

log "step 3: schema-init ${PG_PRIMARY_DB}"
run_schema_init "$PG_PRIMARY_DB"

# ---------------------------------------------------------------------------
# Step 4 — disposable scenario databases
# ---------------------------------------------------------------------------

if [[ "$SKIP_DISPOSABLE" -eq 1 ]]; then
    log "step 4: SKIPPED (--skip-disposable)"
else
    log "step 4: disposable scenario databases"
    for db in "${DISPOSABLE_DBS[@]}"; do
        log "  - ${db}: drop + create + extensions"
        psql_postgres -c "DROP DATABASE IF EXISTS ${db};"
        psql_postgres -c "CREATE DATABASE ${db} OWNER ${PG_USER};"
        psql_postgres -d "$db" -c "CREATE EXTENSION IF NOT EXISTS age;"
        psql_postgres -d "$db" -c "CREATE EXTENSION IF NOT EXISTS vector;"
        run_schema_init "$db"
    done
fi

# ---------------------------------------------------------------------------
# Step 5 — verification
# ---------------------------------------------------------------------------

log "step 5: verification"
if [[ "$DRY_RUN" -eq 0 ]]; then
    log "  primary db (${PG_PRIMARY_DB}) tables:"
    psql_postgres -d "$PG_PRIMARY_DB" -c "\dt" || true
    log "  primary db schema_version row:"
    psql_postgres -d "$PG_PRIMARY_DB" -c "SELECT version FROM schema_version;" || true
    log "  primary db extensions:"
    psql_postgres -d "$PG_PRIMARY_DB" -c "SELECT extname, extversion FROM pg_extension ORDER BY extname;" || true
fi

log "DONE — backup at ${BACKUP_FILE}"
log "DONE — schema-init reports under ${SCHEMA_INIT_JSON%.json}-*.json"
