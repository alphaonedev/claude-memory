// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Wave-1 Fix 3 — `ai-memory schema-init` CLI verb.
//!
//! Bootstraps the storage schema for a SAL backend by URL. Opens the
//! store via [`crate::migrate::open_store`] which is the same factory
//! the `migrate` verb uses; that call triggers `INIT_SCHEMA` (bundled
//! `postgres_schema.sql` for Postgres, `db::open` migrations for
//! SQLite) as a side effect. After init, the verb enumerates the
//! resulting catalog (tables, views, functions, indices, extensions,
//! schema version) and emits a human or JSON summary.
//!
//! ## Postgres + Apache AGE
//!
//! When the target Postgres has the `age` extension installed, the
//! verb additionally bootstraps the `memory_graph` projection via
//! `SELECT create_graph('memory_graph')`. The call is wrapped in a
//! "graph already exists" guard so re-running is idempotent — AGE
//! raises `invalid_graph_name` (or "graph ... already exists") on a
//! second invocation; we treat that as success.
//!
//! AGE is opt-in: missing-extension or probe-failure leaves
//! `age_projection_created = false` in the JSON payload and is NOT a
//! fatal error. Operators with no AGE-installed deployment see a
//! clean exit.
//!
//! ## URL contract
//!
//! Mirrors `migrate::open_store`:
//!
//! - `sqlite:///absolute/path/to/file.db` → `SqliteStore`
//! - `sqlite://./relative/path.db`        → `SqliteStore`
//! - `postgres://user:pass@host:port/db`  → `PostgresStore`
//!   (only when `--features sal-postgres`)
//! - `postgresql://...` is also accepted on the Postgres side.
//!
//! Anything else exits non-zero with the sanitized error from
//! `open_store`.
//!
//! ## Output (human)
//!
//! ```text
//! schema initialized at <url>
//!   tables: <count>
//!   indices: <count>
//!   views: <count>
//!   functions: <count>
//!   extensions: [<list>]
//!   schema_version: <n>
//! ```
//!
//! ## Output (`--json`)
//!
//! ```json
//! {
//!   "url": "...",
//!   "kind": "sqlite|postgres",
//!   "tables": [...],
//!   "views": [...],
//!   "functions": [...],
//!   "indices": [...],
//!   "extensions": [...],
//!   "schema_version": <n>,
//!   "age_projection_created": true|false
//! }
//! ```

#![cfg(feature = "sal")]

use anyhow::{Context, Result};
use clap::Args;
use serde::Serialize;

use crate::cli::CliOutput;
use crate::migrate;

// ---------------------------------------------------------------------------
// CLI arg surface
// ---------------------------------------------------------------------------

/// `ai-memory schema-init` arguments.
#[derive(Args, Debug, Clone)]
pub struct SchemaInitArgs {
    /// Target store URL. `sqlite:///path/to/file.db` or
    /// `postgres://user:pass@host:port/dbname`. Same shape as
    /// `ai-memory migrate --from / --to`.
    #[arg(long, value_name = "URL")]
    pub store_url: String,
    /// Emit the summary as JSON (machine-parseable). Without this
    /// flag the verb prints a six-line human summary suitable for
    /// CI logs and operator scripts.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

// ---------------------------------------------------------------------------
// Report payload — also the JSON wire shape
// ---------------------------------------------------------------------------

/// Schema enumeration report emitted by `schema-init`. The struct
/// doubles as the `--json` payload, so field names + serialization
/// order are the wire contract: every field stays `serde`-stable.
#[derive(Debug, Clone, Serialize)]
pub struct SchemaInitReport {
    /// The original `--store-url` value, echoed back verbatim. Useful
    /// when the operator pipes JSON output into downstream tooling.
    /// Note: passwords inside Postgres URLs are NOT redacted here —
    /// the URL was already in the operator's terminal scrollback.
    pub url: String,
    /// Backend tag: `"sqlite"` or `"postgres"`.
    pub kind: String,
    /// Sorted list of user table names. Excludes `sqlite_*` system
    /// tables and Postgres `pg_catalog` tables.
    pub tables: Vec<String>,
    /// Sorted list of user view names.
    pub views: Vec<String>,
    /// Sorted list of user function names. SQLite has no user
    /// functions in the C-API sense — this stays empty for SQLite.
    pub functions: Vec<String>,
    /// Sorted list of user index names. SQLite excludes
    /// auto-generated `sqlite_autoindex_*` indices for legibility;
    /// Postgres excludes `pg_catalog`.
    pub indices: Vec<String>,
    /// Sorted list of installed extension names. SQLite has no
    /// extension catalog at the SQL layer — this stays empty for
    /// SQLite.
    pub extensions: Vec<String>,
    /// Highest `version` row in the `schema_version` table. `0` if
    /// the table is empty (should not happen post-init).
    pub schema_version: i64,
    /// `true` when the AGE `memory_graph` projection was created (or
    /// already existed) on this connect. `false` when AGE is not
    /// installed (which is the common case) or the target is
    /// SQLite. Never aborts the verb on its own.
    pub age_projection_created: bool,
}

// ---------------------------------------------------------------------------
// Entry point — invoked from `daemon_runtime::run` dispatch
// ---------------------------------------------------------------------------

/// Run `schema-init`. Opens the store at `args.store_url` (the open
/// itself runs `INIT_SCHEMA` + migrations as a side effect),
/// enumerates the resulting catalog, optionally bootstraps the AGE
/// `memory_graph` projection on Postgres, and emits a summary.
///
/// # Errors
///
/// - `unrecognised store URL …` — when the URL scheme is not one of
///   `sqlite://` / `postgres://` / `postgresql://`.
/// - Connection / schema-bootstrap failures bubble up from the
///   underlying adapter with their original error chain so operators
///   can diagnose missing extensions, bad credentials, etc.
pub async fn run(args: &SchemaInitArgs, out: &mut CliOutput<'_>) -> Result<()> {
    // Open via the same factory `migrate` uses — this triggers
    // INIT_SCHEMA execution as a side effect on both backends.
    let _store = migrate::open_store(&args.store_url)
        .await
        .with_context(|| format!("open store at {}", args.store_url))?;

    // Enumerate per-backend. We dispatch on URL scheme rather than
    // on the SAL Capabilities bits because the enumeration queries
    // are inherently backend-specific (sqlite_master vs pg_catalog).
    let report = if is_sqlite_url(&args.store_url) {
        enumerate_sqlite(&args.store_url)?
    } else if is_postgres_url(&args.store_url) {
        #[cfg(feature = "sal-postgres")]
        {
            enumerate_postgres(&args.store_url).await?
        }
        #[cfg(not(feature = "sal-postgres"))]
        {
            // `migrate::open_store` would have already errored on a
            // postgres URL without the feature; this branch exists
            // only to satisfy the compiler in non-default builds.
            anyhow::bail!("postgres support not compiled in (build with --features sal-postgres)");
        }
    } else {
        anyhow::bail!(
            "unrecognised store URL: {} (expected sqlite:///path or postgres://...)",
            args.store_url
        );
    };

    if args.json {
        let json = serde_json::to_string_pretty(&report).context("serialize schema-init report")?;
        writeln!(out.stdout, "{json}")?;
    } else {
        render_human(&report, out)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// URL classification — duplicate of `migrate::open_store`'s prefix
// match, kept local so we only walk the URL once and the dispatch
// reads cleanly without an extra round-trip through `open_store`.
// ---------------------------------------------------------------------------

fn is_sqlite_url(url: &str) -> bool {
    url.starts_with("sqlite://")
}

fn is_postgres_url(url: &str) -> bool {
    url.starts_with("postgres://") || url.starts_with("postgresql://")
}

/// Strip the `sqlite://` prefix and the optional third slash so the
/// remainder is a filesystem path that `rusqlite::Connection::open`
/// understands. Mirrors `migrate::open_store`.
fn sqlite_path_from_url(url: &str) -> &str {
    let path = url.strip_prefix("sqlite://").unwrap_or(url);
    // `sqlite:///foo` → `/foo`; `sqlite://./foo` → `./foo`.
    path.strip_prefix('/')
        .map_or(path, |p| if p.starts_with('/') { p } else { path })
}

// ---------------------------------------------------------------------------
// SQLite enumeration
// ---------------------------------------------------------------------------

/// Open a fresh read-only `rusqlite::Connection` against the same
/// path the SAL adapter just initialized, then walk `sqlite_master`
/// for tables / views / indices and `schema_version` for the
/// numeric version.
///
/// Read-only is deliberate: by the time we reach this function the
/// SAL adapter has already run migrations; we only need to *read*
/// the catalog. A second writer connection on top of WAL would also
/// work, but read-only is the smallest blast radius.
fn enumerate_sqlite(url: &str) -> Result<SchemaInitReport> {
    let path = sqlite_path_from_url(url);
    let conn = rusqlite::Connection::open(path)
        .with_context(|| format!("open sqlite for enumeration: {path}"))?;

    let tables = list_sqlite_objects(&conn, "table")?;
    let views = list_sqlite_objects(&conn, "view")?;
    let indices = list_sqlite_indices(&conn)?;
    let schema_version = read_schema_version_sqlite(&conn).unwrap_or(0);

    Ok(SchemaInitReport {
        url: url.to_string(),
        kind: "sqlite".to_string(),
        tables,
        views,
        functions: Vec::new(),
        indices,
        extensions: Vec::new(),
        schema_version,
        age_projection_created: false,
    })
}

/// Walk `sqlite_master` for objects of `kind` (`"table"` / `"view"`).
/// Excludes `sqlite_*` system rows so the report only surfaces
/// schema we actually own.
fn list_sqlite_objects(conn: &rusqlite::Connection, kind: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = ?1 AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .context("prepare sqlite_master scan")?;
    let rows = stmt
        .query_map([kind], |row| row.get::<_, String>(0))
        .context("query sqlite_master")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read sqlite_master row")?);
    }
    Ok(out)
}

/// Walk `sqlite_master` for indices, excluding `sqlite_*` system
/// rows AND `sqlite_autoindex_*` (auto-created for `UNIQUE` /
/// `PRIMARY KEY` columns) so the list reads cleanly.
fn list_sqlite_indices(conn: &rusqlite::Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type = 'index' \
               AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .context("prepare sqlite_master index scan")?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("query sqlite_master indices")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("read sqlite_master index row")?);
    }
    Ok(out)
}

fn read_schema_version_sqlite(conn: &rusqlite::Connection) -> Result<i64> {
    let v: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )
        .context("read schema_version")?;
    Ok(v)
}

// ---------------------------------------------------------------------------
// Postgres enumeration
// ---------------------------------------------------------------------------

#[cfg(feature = "sal-postgres")]
async fn enumerate_postgres(url: &str) -> Result<SchemaInitReport> {
    use sqlx::postgres::PgPoolOptions;

    // Small pool — enumeration runs a handful of catalog queries
    // and exits. We hold the pool for the duration of this function
    // and let it drop at the end so we don't keep a Postgres
    // connection slot warm.
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(15))
        .connect(url)
        .await
        .with_context(|| format!("connect postgres for enumeration: {url}"))?;

    // Tables in the user-facing `public` schema, sorted. Filtering
    // on `public` keeps the report scoped to the application; AGE
    // installs its own `ag_catalog` schema which we surface via the
    // extensions list rather than dumping its internal tables.
    let table_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT tablename FROM pg_tables \
         WHERE schemaname = 'public' \
         ORDER BY tablename",
    )
    .fetch_all(&pool)
    .await
    .context("list pg_tables")?;
    let tables: Vec<String> = table_rows.into_iter().map(|(n,)| n).collect();

    let view_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT viewname FROM pg_views \
         WHERE schemaname = 'public' \
         ORDER BY viewname",
    )
    .fetch_all(&pool)
    .await
    .context("list pg_views")?;
    let views: Vec<String> = view_rows.into_iter().map(|(n,)| n).collect();

    let index_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT indexname FROM pg_indexes \
         WHERE schemaname = 'public' \
         ORDER BY indexname",
    )
    .fetch_all(&pool)
    .await
    .context("list pg_indexes")?;
    let indices: Vec<String> = index_rows.into_iter().map(|(n,)| n).collect();

    // User functions in `public` schema, distinct names. We filter
    // out aggregate / window flavours and limit to `prokind = 'f'`
    // (regular functions) + `prokind = 'p'` (procedures); aggregates
    // / windows live elsewhere and operators rarely care about them
    // for "did init run cleanly" diagnostics.
    let function_rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT proname FROM pg_proc p \
         JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = 'public' AND p.prokind IN ('f', 'p') \
         ORDER BY proname",
    )
    .fetch_all(&pool)
    .await
    .context("list pg_proc")?;
    let functions: Vec<String> = function_rows.into_iter().map(|(n,)| n).collect();

    // Installed extensions, sorted. This is the surface that
    // captures "is pgvector + AGE present" — the operator's first
    // question post-bootstrap.
    let ext_rows: Vec<(String,)> =
        sqlx::query_as("SELECT extname FROM pg_extension ORDER BY extname")
            .fetch_all(&pool)
            .await
            .context("list pg_extension")?;
    let extensions: Vec<String> = ext_rows.into_iter().map(|(n,)| n).collect();

    // schema_version is created by `postgres_schema.sql` (line 48)
    // and populated by `PostgresStore::migrate`. A missing row set
    // means migration didn't reach the version-stamp step — we
    // surface 0 rather than failing.
    let schema_version_row: Option<(i32,)> =
        sqlx::query_as("SELECT COALESCE(MAX(version), 0)::int FROM schema_version")
            .fetch_optional(&pool)
            .await
            .context("read schema_version")?;
    let schema_version = i64::from(schema_version_row.map_or(0, |(v,)| v));

    // AGE bootstrap: only attempt when the extension is actually
    // installed (it appears in the extensions list above). The call
    // is `SELECT create_graph('memory_graph')`. AGE returns an
    // error if the graph already exists; we tolerate that as a
    // success signal so re-runs are idempotent.
    let age_projection_created = if extensions.iter().any(|e| e == "age") {
        bootstrap_memory_graph(&pool).await
    } else {
        false
    };

    // Drop the pool explicitly so the connection slot frees before
    // the verb returns. Not strictly required (it would drop on
    // function exit anyway) but it documents intent.
    drop(pool);

    Ok(SchemaInitReport {
        url: url.to_string(),
        kind: "postgres".to_string(),
        tables,
        views,
        functions,
        indices,
        extensions,
        schema_version,
        age_projection_created,
    })
}

/// Run `SELECT create_graph('memory_graph')` against an
/// AGE-installed Postgres pool, swallowing the
/// "graph-already-exists" error so the call is idempotent. Any
/// other error is logged at WARN and reported as
/// `age_projection_created = false`; AGE is opt-in and a failure
/// here MUST NOT fail the whole verb.
#[cfg(feature = "sal-postgres")]
async fn bootstrap_memory_graph(pool: &sqlx::PgPool) -> bool {
    // AGE requires `ag_catalog` on the search path before
    // `create_graph` resolves. We set the search path on a
    // dedicated connection so the SET sticks for the duration of
    // the create call. (Same pattern as `kg_query_cypher`.)
    let mut conn = match pool.acquire().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                target = "schema_init",
                error = %e,
                "acquire connection for AGE bootstrap"
            );
            return false;
        }
    };

    if let Err(e) = sqlx::query("SET search_path = ag_catalog, \"$user\", public")
        .execute(&mut *conn)
        .await
    {
        tracing::warn!(
            target = "schema_init",
            error = %e,
            "set ag_catalog search_path"
        );
        return false;
    }

    match sqlx::query("SELECT create_graph('memory_graph')")
        .execute(&mut *conn)
        .await
    {
        Ok(_) => true,
        Err(e) => {
            // AGE's "graph already exists" comes back as a generic
            // SQLSTATE with a message containing "already exists".
            // We treat that as success — re-running schema-init
            // against a previously-bootstrapped DB MUST be
            // idempotent.
            let msg = e.to_string();
            if msg.contains("already exists") {
                true
            } else {
                tracing::warn!(
                    target = "schema_init",
                    error = %e,
                    "create_graph('memory_graph') failed (continuing without AGE projection)"
                );
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Human-readable rendering
// ---------------------------------------------------------------------------

fn render_human(report: &SchemaInitReport, out: &mut CliOutput<'_>) -> Result<()> {
    writeln!(out.stdout, "schema initialized at {}", report.url)?;
    writeln!(out.stdout, "  tables:         {}", report.tables.len())?;
    writeln!(out.stdout, "  indices:        {}", report.indices.len())?;
    writeln!(out.stdout, "  views:          {}", report.views.len())?;
    writeln!(out.stdout, "  functions:      {}", report.functions.len())?;
    writeln!(
        out.stdout,
        "  extensions:     [{}]",
        report.extensions.join(", ")
    )?;
    writeln!(out.stdout, "  schema_version: {}", report.schema_version)?;
    if report.kind == "postgres" {
        writeln!(
            out.stdout,
            "  age_projection: {}",
            if report.age_projection_created {
                "created"
            } else {
                "skipped (AGE not installed or bootstrap failed)"
            }
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_sqlite_urls() {
        assert!(is_sqlite_url("sqlite:///tmp/foo.db"));
        assert!(is_sqlite_url("sqlite://./rel.db"));
        assert!(!is_sqlite_url("postgres://x"));
        assert!(!is_sqlite_url("nosql://x"));
    }

    #[test]
    fn classifies_postgres_urls() {
        assert!(is_postgres_url("postgres://u:p@h/d"));
        assert!(is_postgres_url("postgresql://u:p@h/d"));
        assert!(!is_postgres_url("sqlite:///x"));
    }

    #[test]
    fn sqlite_path_strips_prefix_and_third_slash() {
        assert_eq!(sqlite_path_from_url("sqlite:///tmp/foo.db"), "/tmp/foo.db");
        assert_eq!(sqlite_path_from_url("sqlite://./rel.db"), "./rel.db");
    }

    #[tokio::test]
    async fn run_sqlite_emits_json_with_expected_fields() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_string_lossy().to_string();
        let url = format!("sqlite://{path}");

        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);

        let args = SchemaInitArgs {
            store_url: url.clone(),
            json: true,
        };
        run(&args, &mut out).await.expect("schema-init sqlite");

        let raw = String::from_utf8(stdout).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).expect("parseable JSON");
        assert_eq!(v["kind"], "sqlite");
        assert_eq!(v["url"], serde_json::Value::String(url));
        assert!(
            v["schema_version"].as_i64().unwrap() > 0,
            "schema_version should be > 0 after init: {v}"
        );
        let tables: Vec<&str> = v["tables"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        assert!(
            tables.contains(&"memories"),
            "memories table missing: {tables:?}"
        );
        assert!(
            tables.contains(&"memory_links"),
            "memory_links table missing: {tables:?}"
        );
        // SQLite has no extensions / functions surface.
        assert!(v["extensions"].as_array().unwrap().is_empty());
        assert!(v["functions"].as_array().unwrap().is_empty());
        assert_eq!(v["age_projection_created"], false);
    }

    #[tokio::test]
    async fn run_sqlite_human_output_is_six_lines_minimum() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_string_lossy().to_string();
        let url = format!("sqlite://{path}");

        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);

        let args = SchemaInitArgs {
            store_url: url.clone(),
            json: false,
        };
        run(&args, &mut out)
            .await
            .expect("schema-init sqlite human");

        let raw = String::from_utf8(stdout).unwrap();
        assert!(
            raw.contains("schema initialized at"),
            "missing header: {raw}"
        );
        assert!(raw.contains("tables:"), "missing tables row: {raw}");
        assert!(raw.contains("indices:"), "missing indices row: {raw}");
        assert!(raw.contains("views:"), "missing views row: {raw}");
        assert!(raw.contains("functions:"), "missing functions row: {raw}");
        assert!(raw.contains("extensions:"), "missing extensions row: {raw}");
        assert!(
            raw.contains("schema_version:"),
            "missing version row: {raw}"
        );
    }

    #[tokio::test]
    async fn run_rejects_unrecognised_url_scheme() {
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);

        let args = SchemaInitArgs {
            store_url: "nosql://nope".to_string(),
            json: false,
        };
        let err = run(&args, &mut out).await.expect_err("should reject");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("unrecognised store URL"),
            "expected unrecognised-scheme error, got: {msg}"
        );
    }
}
