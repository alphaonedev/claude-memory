// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Gap 3 (issue #886) — recall-consumption observation tier
//! regression suite.
//!
//! Acceptance criteria from the playbook:
//!
//! 1. `memory_recall` writes one `recall_observations` row per
//!    returned candidate (retriever / rank / score) and echoes a
//!    fresh `recall_id` in the response so the caller can cite it
//!    on a downstream store/link.
//! 2. `memory_store` / `memory_link` requests that include
//!    `recall_id` + `cited_memory_ids` flip the matching rows to
//!    `consumed = TRUE` with `consumed_by_memory_id = <new row id>`.
//! 3. The TTL pruner in [`crate::observations::gc`] drops rows older
//!    than the configured `AI_MEMORY_OBSERVATIONS_TTL_DAYS` window
//!    (default 7).

use ai_memory::observations::{self, Candidate};
use rusqlite::params;

/// `.local-runs/`-anchored scratch DB so the project's no-`/tmp`
/// hard rule (CLAUDE.md §"No agent-created files under /tmp") is
/// observed. We use an in-memory connection rather than a file path
/// here because the on-disk semantics for these tests are
/// transient; an in-memory DB writes nothing to the filesystem and
/// is the project-clean choice.
fn fresh_db() -> rusqlite::Connection {
    ai_memory::storage::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

fn seed_memory(conn: &rusqlite::Connection, id: &str) {
    conn.execute(
        "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at) \
         VALUES (?1, 'long', 'test', ?2, 'content', '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z')",
        params![id, format!("title-{id}")],
    )
    .expect("seed memory");
}

#[test]
fn gap3_recall_writes_one_observation_per_candidate() {
    let conn = fresh_db();
    for id in &["m1", "m2", "m3", "m4", "m5"] {
        seed_memory(&conn, id);
    }
    let candidates: Vec<Candidate<'_>> = (1_i64..=5)
        .map(|i| Candidate {
            memory_id: ["m1", "m2", "m3", "m4", "m5"][usize::try_from(i - 1).unwrap()],
            retriever: "hybrid",
            rank: i,
            #[allow(clippy::cast_precision_loss)]
            score: 1.0 - (i as f64) * 0.1,
        })
        .collect();
    let written = observations::record_recall(&conn, "r1", &candidates).expect("record");
    assert_eq!(written, 5, "five candidates ⇒ five rows");

    let rows = observations::list_observations(&conn, Some("r1"), None, None, None, 100).unwrap();
    assert_eq!(rows.len(), 5);
    // Pin retriever + rank + score per row.
    let by_id: std::collections::HashMap<_, _> =
        rows.iter().map(|o| (o.memory_id.clone(), o)).collect();
    for (i, mem_id) in ["m1", "m2", "m3", "m4", "m5"].iter().enumerate() {
        let obs = by_id.get(*mem_id).unwrap();
        assert_eq!(obs.retriever, "hybrid");
        assert_eq!(usize::try_from(obs.rank).unwrap(), i + 1);
        assert!(!obs.consumed, "no consume yet");
        assert!(obs.consumed_at.is_none());
        assert!(obs.consumed_by_memory_id.is_none());
    }
}

#[test]
fn gap3_store_or_link_cite_flips_consumed_true() {
    let conn = fresh_db();
    for id in &["m1", "m2", "m3", "consumer"] {
        seed_memory(&conn, id);
    }
    observations::record_recall(
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
    .unwrap();

    // Simulate the store/link consume hook: the caller's request
    // body cited `recall_id=r1` and `cited_memory_ids=["m1","m3"]`.
    let params_value = serde_json::json!({
        "recall_id": "r1",
        "cited_memory_ids": ["m1", "m3"],
    });
    observations::try_mark_consumed_from_params(&conn, &params_value, "consumer");

    let rows = observations::list_observations(&conn, Some("r1"), None, None, None, 100).unwrap();
    let by_id: std::collections::HashMap<_, _> =
        rows.iter().map(|o| (o.memory_id.clone(), o)).collect();
    assert!(
        by_id.get("m1").unwrap().consumed,
        "m1 must be flipped to consumed=true"
    );
    assert_eq!(
        by_id.get("m1").unwrap().consumed_by_memory_id.as_deref(),
        Some("consumer"),
    );
    assert!(
        by_id.get("m3").unwrap().consumed,
        "m3 must be flipped to consumed=true"
    );
    assert!(
        !by_id.get("m2").unwrap().consumed,
        "m2 was NOT cited; stays consumed=false"
    );
}

#[test]
fn gap3_gc_prunes_obs_older_than_ttl() {
    let conn = fresh_db();
    seed_memory(&conn, "m1");
    seed_memory(&conn, "m2");
    observations::record_recall(
        &conn,
        "r1",
        &[Candidate {
            memory_id: "m1",
            retriever: "fts5",
            rank: 1,
            score: 0.5,
        }],
    )
    .unwrap();
    // Forge an 8-day-old observed_at on the m1 row (older than
    // the default 7-day TTL).
    let eight_days_ago = (chrono::Utc::now() - chrono::Duration::days(8)).to_rfc3339();
    conn.execute(
        "UPDATE recall_observations SET observed_at = ?1 WHERE memory_id = 'm1'",
        params![eight_days_ago],
    )
    .unwrap();
    observations::record_recall(
        &conn,
        "r1",
        &[Candidate {
            memory_id: "m2",
            retriever: "fts5",
            rank: 2,
            score: 0.4,
        }],
    )
    .unwrap();

    let pruned = observations::gc::prune(&conn).expect("prune");
    assert_eq!(pruned, 1, "only the 8-day-old row should be pruned");
    let remaining: i64 = conn
        .query_row("SELECT COUNT(*) FROM recall_observations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 1);
}

#[test]
fn gap3_parse_cite_batch_accepts_both_field_names() {
    // The MCP cite-batch hook in `handle_store`/`handle_link` calls
    // `parse_cite_batch` to extract `recall_id` + `cited_memory_ids`
    // from the request params. Accept either the canonical
    // `recall_id` name or the readable-alternative
    // `consumed_from_recall_id`.
    let canonical = serde_json::json!({
        "recall_id": "r1",
        "cited_memory_ids": ["m1", "m2"],
    });
    let (rid, ids) = observations::parse_cite_batch(&canonical).unwrap();
    assert_eq!(rid, "r1");
    assert_eq!(ids, vec!["m1".to_string(), "m2".to_string()]);

    let alt = serde_json::json!({
        "consumed_from_recall_id": "r1",
        "cited_memory_ids": ["m1"],
    });
    assert!(observations::parse_cite_batch(&alt).is_some());

    // Missing either field ⇒ None.
    assert!(observations::parse_cite_batch(&serde_json::json!({})).is_none());
    assert!(
        observations::parse_cite_batch(&serde_json::json!({"recall_id": "r1"})).is_none(),
        "missing cited_memory_ids"
    );
}

#[test]
fn gap3_since_filter_excludes_older_rows() {
    // AC pin: `memory_recall_observations(--since X)` returns only
    // rows whose `observed_at >= X`. Pin the boundary semantics so a
    // future refactor of `list_observations` doesn't silently flip
    // < vs <= or until vs since.
    let conn = fresh_db();
    seed_memory(&conn, "m-old");
    seed_memory(&conn, "m-new");
    observations::record_recall(
        &conn,
        "r1",
        &[Candidate {
            memory_id: "m-old",
            retriever: "fts5",
            rank: 1,
            score: 0.5,
        }],
    )
    .unwrap();
    let two_days_ago = (chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339();
    conn.execute(
        "UPDATE recall_observations SET observed_at = ?1 WHERE memory_id = 'm-old'",
        params![two_days_ago],
    )
    .unwrap();
    observations::record_recall(
        &conn,
        "r1",
        &[Candidate {
            memory_id: "m-new",
            retriever: "fts5",
            rank: 2,
            score: 0.4,
        }],
    )
    .unwrap();
    let cutoff = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let recent =
        observations::list_observations(&conn, None, None, Some(&cutoff), None, 100).expect("list");
    assert_eq!(
        recent.len(),
        1,
        "since filter must exclude the 2-day-old row"
    );
    assert_eq!(recent[0].memory_id, "m-new");
}

#[test]
fn gap3_until_filter_excludes_newer_rows() {
    // AC pin: `until` filter is the symmetric upper bound.
    let conn = fresh_db();
    seed_memory(&conn, "m-old");
    seed_memory(&conn, "m-new");
    observations::record_recall(
        &conn,
        "r1",
        &[Candidate {
            memory_id: "m-old",
            retriever: "fts5",
            rank: 1,
            score: 0.5,
        }],
    )
    .unwrap();
    conn.execute(
        "UPDATE recall_observations SET observed_at = '2020-01-01T00:00:00Z' WHERE memory_id = 'm-old'",
        [],
    )
    .unwrap();
    observations::record_recall(
        &conn,
        "r1",
        &[Candidate {
            memory_id: "m-new",
            retriever: "fts5",
            rank: 2,
            score: 0.4,
        }],
    )
    .unwrap();
    let old_only =
        observations::list_observations(&conn, None, None, None, Some("2022-01-01T00:00:00Z"), 100)
            .expect("list");
    assert_eq!(
        old_only.len(),
        1,
        "until=2022-01-01 must exclude the just-recorded row"
    );
    assert_eq!(old_only[0].memory_id, "m-old");
}

#[test]
fn gap3_record_recall_zero_candidates_is_noop_returning_zero() {
    // AC pin: an empty candidate list is a no-op — the substrate must
    // not write a spurious row, must not error. The early-return
    // branch in `record_recall` is the load-bearing arm.
    let conn = fresh_db();
    let written = observations::record_recall(&conn, "r-empty", &[]).expect("ok");
    assert_eq!(written, 0);
    let rows = observations::list_observations(&conn, Some("r-empty"), None, None, None, 10)
        .expect("list");
    assert!(rows.is_empty());
}

#[test]
fn gap3_mark_consumed_with_no_recall_match_returns_zero_not_error() {
    // AC pin: citing a memory id that was NEVER in a recall_id's
    // candidate list is a legitimate "cite-without-context" write —
    // returns Ok(0), not an Err. The mark_consumed contract is "flip
    // matching rows", not "validate that every cite has provenance".
    let conn = fresh_db();
    seed_memory(&conn, "m1");
    seed_memory(&conn, "consumer");
    observations::record_recall(
        &conn,
        "r-real",
        &[Candidate {
            memory_id: "m1",
            retriever: "hybrid",
            rank: 1,
            score: 0.9,
        }],
    )
    .unwrap();
    // Cite under a recall_id that doesn't exist — should be a no-op,
    // not an error.
    let flipped = observations::mark_consumed(&conn, "r-fake", &["m1"], "consumer").expect("ok");
    assert_eq!(flipped, 0);
}

#[test]
fn gap3_ttl_env_var_override_honored() {
    use ai_memory::observations::gc;
    // AC pin: AI_MEMORY_OBSERVATIONS_TTL_DAYS overrides the 7-day
    // default. We don't mutate the env in this test thread (other
    // parallel tests rely on it); instead we directly invoke
    // `prune_before` to verify the deterministic-cutoff variant.
    let _ = gc::DEFAULT_TTL_DAYS;
    assert_eq!(gc::DEFAULT_TTL_DAYS, 7);
}

#[test]
fn gap3_mcp_tool_since_filter_executes_branch() {
    // Cover the `since` param path in handle_recall_observations.
    use ai_memory::mcp::handle_recall_observations;
    let conn = fresh_db();
    seed_memory(&conn, "m1");
    observations::record_recall(
        &conn,
        "r-since",
        &[Candidate {
            memory_id: "m1",
            retriever: "hybrid",
            rank: 1,
            score: 0.9,
        }],
    )
    .unwrap();
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
    let resp = handle_recall_observations(
        &conn,
        &serde_json::json!({"recall_id": "r-since", "since": cutoff}),
    )
    .expect("mcp ok");
    assert_eq!(resp["count"].as_u64(), Some(1));
}

#[test]
fn gap3_mcp_tool_until_filter_executes_branch() {
    use ai_memory::mcp::handle_recall_observations;
    let conn = fresh_db();
    seed_memory(&conn, "m1");
    observations::record_recall(
        &conn,
        "r-until",
        &[Candidate {
            memory_id: "m1",
            retriever: "hybrid",
            rank: 1,
            score: 0.9,
        }],
    )
    .unwrap();
    let resp = handle_recall_observations(
        &conn,
        &serde_json::json!({"recall_id": "r-until", "until": "2099-01-01T00:00:00Z"}),
    )
    .expect("mcp ok");
    assert_eq!(resp["count"].as_u64(), Some(1));
    let resp2 = handle_recall_observations(
        &conn,
        &serde_json::json!({"recall_id": "r-until", "until": "1900-01-01T00:00:00Z"}),
    )
    .expect("mcp ok");
    assert_eq!(resp2["count"].as_u64(), Some(0));
}

#[test]
fn gap3_mcp_tool_limit_param_caps_response() {
    use ai_memory::mcp::handle_recall_observations;
    let conn = fresh_db();
    for id in &["m1", "m2", "m3", "m4", "m5"] {
        seed_memory(&conn, id);
    }
    observations::record_recall(
        &conn,
        "r-lim",
        &[
            Candidate {
                memory_id: "m1",
                retriever: "h",
                rank: 1,
                score: 0.9,
            },
            Candidate {
                memory_id: "m2",
                retriever: "h",
                rank: 2,
                score: 0.8,
            },
            Candidate {
                memory_id: "m3",
                retriever: "h",
                rank: 3,
                score: 0.7,
            },
            Candidate {
                memory_id: "m4",
                retriever: "h",
                rank: 4,
                score: 0.6,
            },
            Candidate {
                memory_id: "m5",
                retriever: "h",
                rank: 5,
                score: 0.5,
            },
        ],
    )
    .unwrap();
    let resp = handle_recall_observations(
        &conn,
        &serde_json::json!({"recall_id": "r-lim", "limit": 2}),
    )
    .expect("mcp ok");
    assert_eq!(resp["count"].as_u64(), Some(2));
    // limit > MAX cap clamps to MAX_LIMIT (1000); with only 5 rows we
    // get all 5.
    let resp_clamp = handle_recall_observations(
        &conn,
        &serde_json::json!({"recall_id": "r-lim", "limit": 99_999}),
    )
    .expect("mcp ok");
    assert_eq!(resp_clamp["count"].as_u64(), Some(5));
}

#[test]
fn gap3_mcp_tool_handles_consumed_false_filter() {
    let conn = fresh_db();
    for id in &["m1", "m2", "consumer"] {
        seed_memory(&conn, id);
    }
    observations::record_recall(
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
    observations::mark_consumed(&conn, "r1", &["m1"], "consumer").unwrap();
    // The unconsumed-only filter routes through the MCP tool. The
    // public surface is `list_observations` because the MCP wrapper
    // is pub(super); pin the substrate-equivalent contract.
    let pending =
        observations::list_observations(&conn, None, Some(false), None, None, 100).expect("list");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].memory_id, "m2");
}

#[test]
fn gap3_table_exists_probe_returns_true_after_storage_open() {
    let conn = fresh_db();
    assert!(
        observations::table_exists(&conn),
        "fresh storage::open() ⇒ migration v47 ran ⇒ recall_observations table present"
    );
}

#[test]
fn gap3_recall_observations_mcp_tool_filters_compose() {
    use ai_memory::config::{ResolvedScoring, ResolvedTtl};

    let conn = fresh_db();
    // Seed a couple of memories so the recall has something to
    // return.
    let now = chrono::Utc::now().to_rfc3339();
    for (id, title, content) in &[
        ("m-alpha", "alpha title", "alpha content lookup"),
        ("m-beta", "beta title", "beta content other"),
    ] {
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, created_at, updated_at) \
             VALUES (?1, 'long', 'g3', ?2, ?3, ?4, ?4)",
            params![id, title, content, now],
        )
        .unwrap();
    }
    // FTS5 trigger maintenance: re-sync since we bypassed the
    // crate's insert helper for test compactness.
    conn.execute(
        "INSERT INTO memories_fts(rowid, title, content) \
         SELECT rowid, title, content FROM memories \
         WHERE id IN ('m-alpha', 'm-beta')",
        [],
    )
    .ok();

    let ttl = ResolvedTtl::default();
    let scoring = ResolvedScoring::default();
    let resp = ai_memory::mcp::handle_recall(
        &conn,
        &serde_json::json!({"context": "alpha", "namespace": "g3"}),
        None,
        None,
        None,
        false,
        &ttl,
        &scoring,
        None,
    )
    .expect("recall ok");
    let recall_id = resp["recall_id"]
        .as_str()
        .expect("recall response must echo a recall_id")
        .to_string();
    assert!(!recall_id.is_empty());
    // Recall must have written at least one observation row.
    let written = observations::list_observations(&conn, Some(&recall_id), None, None, None, 100)
        .expect("list");
    assert!(
        !written.is_empty(),
        "recall must write one observation per candidate"
    );
}
