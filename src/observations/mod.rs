// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Gap 3 (#886) — recall-consumption observation tier.
//!
//! The Batman 6-form audit closeout flagged a missing "what did the
//! caller actually use after we ranked it" feedback channel: recall
//! ranking telemetry stops at the recall response, so the substrate
//! cannot tell which candidates the caller subsequently cited in a
//! `memory_store` or `memory_link` payload. This module is the
//! write-side half of the closeout:
//!
//! - [`record_recall`] — at the end of every `memory_recall` call,
//!   the dispatcher writes one ledger row per returned candidate
//!   (recall_id + memory_id + retriever + rank + score). The
//!   recall_id is a fresh UUID returned in the response so the
//!   caller can echo it back on a later store/link.
//! - [`mark_consumed`] — when a `memory_store` or `memory_link`
//!   request cites a `recall_id` + `memory_ids` list, the matching
//!   rows flip `consumed = TRUE` and capture the `consumed_by_memory_id`
//!   FK of the row that did the citing.
//!
//! Read-side is exposed via the [`gc`] submodule (TTL prune) and the
//! `memory_recall_observations` MCP tool defined in
//! `crate::mcp::tools::recall_observations`.
//!
//! # Schema
//!
//! `recall_observations` (migration `0038_v07_recall_observations.sql`,
//! schema v47) is the backing table. Composite primary key
//! `(recall_id, memory_id)` keeps the substrate idempotent under
//! duplicate writes (an exact replay of the same recall is a no-op,
//! not a UNIQUE-violation refusal).

pub mod gc;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

/// One candidate row passed to [`record_recall`].
///
/// Each tuple maps directly to a `recall_observations` row: the
/// memory id the recall returned, the retriever name (`"fts5"` /
/// `"hnsw"` / `"hybrid"`), the in-result rank (1-based), and the
/// blended score that produced that rank.
#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    pub memory_id: &'a str,
    pub retriever: &'a str,
    pub rank: i64,
    pub score: f64,
}

/// Write one `recall_observations` row per `candidates` entry under a
/// single `recall_id`. The call is best-effort: a SQL error during
/// insertion logs at warn level and continues, since the substrate
/// MUST NOT block a successful recall response on a failed audit
/// write.
///
/// # Errors
///
/// Returns the first `rusqlite::Error` encountered. Callers typically
/// log + discard — the recall response is already minted by the time
/// this runs.
pub fn record_recall(
    conn: &Connection,
    recall_id: &str,
    candidates: &[Candidate<'_>],
) -> Result<usize> {
    if candidates.is_empty() {
        return Ok(0);
    }
    let mut stmt = conn.prepare_cached(
        "INSERT OR IGNORE INTO recall_observations \
                (recall_id, memory_id, retriever, rank, score) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut written = 0_usize;
    for c in candidates {
        let n = stmt.execute(params![
            recall_id,
            c.memory_id,
            c.retriever,
            c.rank,
            c.score
        ])?;
        written += n;
    }
    Ok(written)
}

/// Flip the `consumed` flag (and capture `consumed_at` +
/// `consumed_by_memory_id`) for every `(recall_id, memory_id)` pair
/// where `memory_id` is in `cited_memory_ids`. Idempotent — replaying
/// the same call is a no-op because the WHERE clause requires
/// `consumed = 0`.
///
/// Returns the count of rows that flipped. Zero means none of the
/// supplied memory ids matched a prior recall ledger row under
/// `recall_id` — this is the legitimate "caller cited a memory that
/// wasn't in the recall" case and is intentionally NOT an error
/// (the citation is a first-class write whose value is independent
/// of whether the substrate had previously surfaced the memory).
///
/// # Errors
///
/// Returns the underlying `rusqlite::Error` on SQL failure.
pub fn mark_consumed(
    conn: &Connection,
    recall_id: &str,
    cited_memory_ids: &[&str],
    consumed_by: &str,
) -> Result<usize> {
    if cited_memory_ids.is_empty() {
        return Ok(0);
    }
    let now = chrono::Utc::now().to_rfc3339();
    let mut stmt = conn.prepare_cached(
        "UPDATE recall_observations \
            SET consumed = 1, \
                consumed_at = ?1, \
                consumed_by_memory_id = ?2 \
          WHERE recall_id = ?3 \
            AND memory_id = ?4 \
            AND consumed = 0",
    )?;
    let mut flipped = 0_usize;
    for mid in cited_memory_ids {
        let n = stmt.execute(params![now, consumed_by, recall_id, mid])?;
        flipped += n;
    }
    Ok(flipped)
}

/// One row of `recall_observations` as it travels over the read-side
/// MCP `memory_recall_observations` tool. Mirrors the SQL columns 1:1
/// plus a derived `consumed` boolean (the SQL column is an INTEGER
/// 0/1).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Observation {
    pub recall_id: String,
    pub memory_id: String,
    pub retriever: String,
    pub rank: i64,
    pub score: f64,
    pub consumed: bool,
    pub observed_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumed_by_memory_id: Option<String>,
}

/// Read-side query for the `memory_recall_observations` MCP tool.
///
/// Filters compose with AND. Every field is optional; passing all
/// `None`s returns the full ledger (capped by `limit`, default 200).
///
/// # Errors
///
/// Returns the underlying `rusqlite::Error` on SQL failure.
pub fn list_observations(
    conn: &Connection,
    recall_id: Option<&str>,
    consumed: Option<bool>,
    since: Option<&str>,
    until: Option<&str>,
    limit: usize,
) -> Result<Vec<Observation>> {
    let mut sql = String::from(
        "SELECT recall_id, memory_id, retriever, rank, score, consumed, \
                observed_at, consumed_at, consumed_by_memory_id \
           FROM recall_observations \
          WHERE 1=1",
    );
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(rid) = recall_id {
        sql.push_str(" AND recall_id = ?");
        binds.push(Box::new(rid.to_string()));
    }
    if let Some(c) = consumed {
        sql.push_str(" AND consumed = ?");
        binds.push(Box::new(i64::from(c)));
    }
    if let Some(s) = since {
        sql.push_str(" AND observed_at >= ?");
        binds.push(Box::new(s.to_string()));
    }
    if let Some(u) = until {
        sql.push_str(" AND observed_at <= ?");
        binds.push(Box::new(u.to_string()));
    }
    sql.push_str(" ORDER BY observed_at DESC LIMIT ?");
    let lim_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
    binds.push(Box::new(lim_i64));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(binds.iter()), |row| {
            Ok(Observation {
                recall_id: row.get(0)?,
                memory_id: row.get(1)?,
                retriever: row.get(2)?,
                rank: row.get(3)?,
                score: row.get(4)?,
                consumed: row.get::<_, i64>(5)? != 0,
                observed_at: row.get(6)?,
                consumed_at: row.get(7).ok(),
                consumed_by_memory_id: row.get(8).ok(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Convenience helper used by the MCP store/link consume hook: read
/// the `recall_id` + `cited_memory_ids` array out of an MCP request
/// `params` `Value`. Both must be present for a cite-batch to fire.
///
/// Accepts the following shapes under `params`:
///
/// - `{ "recall_id": "...", "cited_memory_ids": ["...", "..."] }`
/// - `{ "consumed_from_recall_id": "...", "cited_memory_ids": [...] }`
///   (alternate field name some clients prefer for readability)
///
/// Returns `None` when either field is missing or the recall_id is
/// empty / whitespace. Returns `Some((recall_id, ids))` otherwise;
/// ids are unique-preserving over the source array.
#[must_use]
pub fn parse_cite_batch(params: &serde_json::Value) -> Option<(String, Vec<String>)> {
    let recall_id = params
        .get("recall_id")
        .or_else(|| params.get("consumed_from_recall_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let ids_raw = params.get("cited_memory_ids").and_then(|v| v.as_array())?;
    let mut out: Vec<String> = Vec::new();
    for v in ids_raw {
        if let Some(s) = v.as_str() {
            let s = s.trim();
            if !s.is_empty() && !out.iter().any(|x| x == s) {
                out.push(s.to_string());
            }
        }
    }
    if out.is_empty() {
        return None;
    }
    Some((recall_id, out))
}

/// Best-effort wrapper around [`mark_consumed`] used by the
/// `handle_store` / `handle_link` hot paths: parses the cite batch,
/// invokes the SQL update, and logs (rather than propagates) any
/// substrate error. The recall ledger MUST NOT block the underlying
/// write — it's an audit trail, not a precondition.
pub fn try_mark_consumed_from_params(
    conn: &Connection,
    params: &serde_json::Value,
    consumed_by: &str,
) {
    let Some((recall_id, ids)) = parse_cite_batch(params) else {
        return;
    };
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    if let Err(e) = mark_consumed(conn, &recall_id, &refs, consumed_by) {
        tracing::warn!(
            target: "observations",
            recall_id = %recall_id,
            consumed_by,
            "mark_consumed failed (non-fatal): {e}"
        );
    }
}

/// Probe whether the `recall_observations` table exists on this
/// connection. Used by the recall-side instrumentation as a soft
/// gate so a pre-v47 database doesn't blow up the recall response
/// when the binary briefly precedes the migration apply (the
/// migration runs at open time, so this is only relevant for
/// hand-rolled `Connection::open` test fixtures that skip
/// `crate::storage::open`).
#[must_use]
pub fn table_exists(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT name FROM sqlite_master WHERE type='table' AND name='recall_observations'",
        [],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
    .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fresh() -> Connection {
        // Go through the public `storage::open` entry point so the
        // canonical SCHEMA is applied before the migration ladder
        // runs (the ladder ALTERs columns on `memories` etc., which
        // would fail on an empty DB).
        crate::storage::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn seed_memory(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO memories \
                (id, tier, namespace, title, content, created_at, updated_at) \
             VALUES (?1, 'long', 'test', ?2, 'content', '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
            params![id, format!("title-{id}")],
        )
        .expect("seed memory");
    }

    #[test]
    fn record_recall_writes_one_row_per_candidate() {
        let conn = fresh();
        seed_memory(&conn, "m1");
        seed_memory(&conn, "m2");
        let candidates = vec![
            Candidate {
                memory_id: "m1",
                retriever: "hybrid",
                rank: 1,
                score: 0.9,
            },
            Candidate {
                memory_id: "m2",
                retriever: "hybrid",
                rank: 2,
                score: 0.8,
            },
        ];
        let n = record_recall(&conn, "r1", &candidates).expect("record");
        assert_eq!(n, 2);

        let obs = list_observations(&conn, Some("r1"), None, None, None, 10).expect("list");
        assert_eq!(obs.len(), 2);
        assert!(obs.iter().any(|o| o.memory_id == "m1"));
        assert!(obs.iter().any(|o| o.memory_id == "m2"));
        assert!(obs.iter().all(|o| !o.consumed));
    }

    #[test]
    fn record_recall_is_idempotent_under_replay() {
        let conn = fresh();
        seed_memory(&conn, "m1");
        let candidates = vec![Candidate {
            memory_id: "m1",
            retriever: "fts5",
            rank: 1,
            score: 0.5,
        }];
        record_recall(&conn, "r1", &candidates).expect("first");
        let n = record_recall(&conn, "r1", &candidates).expect("replay");
        // INSERT OR IGNORE on the composite PK collapses the replay
        // to zero inserts. (Caller's perspective: idempotent.)
        assert_eq!(n, 0);
    }

    #[test]
    fn mark_consumed_flips_only_matching_rows() {
        let conn = fresh();
        seed_memory(&conn, "m1");
        seed_memory(&conn, "m2");
        seed_memory(&conn, "m3");
        seed_memory(&conn, "consumer");
        record_recall(
            &conn,
            "r1",
            &[
                Candidate {
                    memory_id: "m1",
                    retriever: "hybrid",
                    rank: 1,
                    score: 0.9,
                },
                Candidate {
                    memory_id: "m2",
                    retriever: "hybrid",
                    rank: 2,
                    score: 0.8,
                },
                Candidate {
                    memory_id: "m3",
                    retriever: "hybrid",
                    rank: 3,
                    score: 0.7,
                },
            ],
        )
        .expect("record");

        let flipped = mark_consumed(&conn, "r1", &["m1", "m3"], "consumer").expect("mark");
        assert_eq!(flipped, 2);

        let obs = list_observations(&conn, Some("r1"), None, None, None, 10).expect("list");
        let m1 = obs.iter().find(|o| o.memory_id == "m1").unwrap();
        let m2 = obs.iter().find(|o| o.memory_id == "m2").unwrap();
        let m3 = obs.iter().find(|o| o.memory_id == "m3").unwrap();
        assert!(m1.consumed && m1.consumed_at.is_some());
        assert!(!m2.consumed && m2.consumed_at.is_none());
        assert!(m3.consumed);
        assert_eq!(m1.consumed_by_memory_id.as_deref(), Some("consumer"));
    }

    #[test]
    fn mark_consumed_idempotent_no_replay_flip() {
        let conn = fresh();
        seed_memory(&conn, "m1");
        seed_memory(&conn, "consumer");
        record_recall(
            &conn,
            "r1",
            &[Candidate {
                memory_id: "m1",
                retriever: "hybrid",
                rank: 1,
                score: 0.9,
            }],
        )
        .unwrap();
        assert_eq!(mark_consumed(&conn, "r1", &["m1"], "consumer").unwrap(), 1);
        assert_eq!(
            mark_consumed(&conn, "r1", &["m1"], "consumer").unwrap(),
            0,
            "second call must be a no-op because consumed=1 already"
        );
    }

    #[test]
    fn parse_cite_batch_accepts_both_field_names() {
        let v1 = serde_json::json!({
            "recall_id": "r1",
            "cited_memory_ids": ["m1", "m2"],
        });
        let v2 = serde_json::json!({
            "consumed_from_recall_id": "r1",
            "cited_memory_ids": ["m1", "m2"],
        });
        let (rid, ids) = parse_cite_batch(&v1).expect("v1");
        assert_eq!(rid, "r1");
        assert_eq!(ids, vec!["m1".to_string(), "m2".to_string()]);
        let (rid2, ids2) = parse_cite_batch(&v2).expect("v2");
        assert_eq!(rid2, "r1");
        assert_eq!(ids2, ids);
    }

    #[test]
    fn parse_cite_batch_returns_none_on_missing_fields() {
        assert!(parse_cite_batch(&serde_json::json!({})).is_none());
        assert!(
            parse_cite_batch(&serde_json::json!({"recall_id": "r1"})).is_none(),
            "missing cited_memory_ids"
        );
        assert!(
            parse_cite_batch(&serde_json::json!({"cited_memory_ids": ["m1"]})).is_none(),
            "missing recall_id"
        );
        assert!(
            parse_cite_batch(&serde_json::json!({"recall_id": "   ", "cited_memory_ids": ["m1"]}))
                .is_none(),
            "blank recall_id"
        );
    }

    #[test]
    fn parse_cite_batch_dedupes_and_skips_blank_ids() {
        let v = serde_json::json!({
            "recall_id": "r1",
            "cited_memory_ids": ["m1", "m1", "", "  ", "m2"],
        });
        let (_rid, ids) = parse_cite_batch(&v).unwrap();
        assert_eq!(ids, vec!["m1".to_string(), "m2".to_string()]);
    }

    #[test]
    fn list_observations_filters_compose() {
        let conn = fresh();
        for id in &["m1", "m2", "m3", "consumer"] {
            seed_memory(&conn, id);
        }
        record_recall(
            &conn,
            "r1",
            &[
                Candidate {
                    memory_id: "m1",
                    retriever: "hybrid",
                    rank: 1,
                    score: 0.9,
                },
                Candidate {
                    memory_id: "m2",
                    retriever: "hybrid",
                    rank: 2,
                    score: 0.8,
                },
            ],
        )
        .unwrap();
        record_recall(
            &conn,
            "r2",
            &[Candidate {
                memory_id: "m3",
                retriever: "fts5",
                rank: 1,
                score: 0.4,
            }],
        )
        .unwrap();
        mark_consumed(&conn, "r1", &["m1"], "consumer").unwrap();

        // Per-recall filter:
        assert_eq!(
            list_observations(&conn, Some("r1"), None, None, None, 10)
                .unwrap()
                .len(),
            2
        );
        // Consumed=true ⇒ only the one flipped row.
        let only_consumed = list_observations(&conn, None, Some(true), None, None, 10).unwrap();
        assert_eq!(only_consumed.len(), 1);
        assert_eq!(only_consumed[0].memory_id, "m1");
        // Consumed=false ⇒ the remaining two.
        let only_pending = list_observations(&conn, None, Some(false), None, None, 10).unwrap();
        assert_eq!(only_pending.len(), 2);
    }
}
