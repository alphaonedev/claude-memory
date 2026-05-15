// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_replay` handler.

use crate::transcripts::replay::{ReplayEntry, replay_transcript_union};
use crate::validate;
use serde_json::{Value, json};

/// v0.7.0 I4 — single-transcript content threshold above which the
/// replay tool omits decompressed text unless the caller opted into
/// `verbose=true`. 100 KB matches the "operators must opt into large
/// dumps" carve-out called out in the I4 prompt; below that, even a
/// long chat fits comfortably in an LLM context window without
/// truncation surprise.
pub(super) const REPLAY_VERBOSE_THRESHOLD_BYTES: i64 = 100 * 1024;

/// v0.7.0 I4 + L2-4 — `memory_replay(memory_id, verbose=false, depth=null)`.
///
/// Walks the I2 `memory_transcript_links` join table for `memory_id`,
/// fetches each linked transcript via I1's [`crate::transcripts::fetch`]
/// (which transparently decompresses the zstd blob), and returns a
/// chronologically-sorted JSON array of transcripts with their span
/// metadata.
///
/// ## L2-4 (issue #669) — reflection union
///
/// When the input memory's `memory_kind` is `Reflection` (L1-1), the
/// replay reads the **union** of every transcript reachable by
/// walking `reflects_on` edges from the input. The walk is BFS over
/// the I2 + reflects_on adjacency; depth-capped at `depth` hops when
/// the caller passes the optional parameter, otherwise unbounded
/// ("full chain", the default per the #669 contract).
///
/// `depth = 0` returns the reflection's own transcripts only —
/// identical shape to the pre-L2-4 I4 read. `depth = N >= 1` returns
/// self plus N hops of ancestors.
///
/// Non-reflection memories ignore the `depth` parameter entirely;
/// their replay shape is unchanged from the pre-L2-4 I4 behaviour
/// (pinned by the #669 acceptance criterion "existing memory_replay
/// for non-reflection memories MUST be unchanged").
///
/// Sort order: ascending `created_at` so the replay reads as the
/// source chain in the order the conversations actually happened.
/// Ties on `created_at` fall back to `transcript_id` for deterministic
/// output even when two transcripts land in the same RFC3339
/// millisecond.
///
/// Truncation rule: when `verbose=false` (default) and a transcript's
/// `original_size` exceeds [`REPLAY_VERBOSE_THRESHOLD_BYTES`], its
/// `content` field is omitted and `truncated` is set to `true`. Forces
/// operators to opt into `verbose=true` for multi-MB dumps so an
/// accidental call from a small-context client doesn't blow the
/// session budget. The metadata block (`compressed_size`,
/// `original_size`, `span_start`, `span_end`, `created_at`,
/// `source_memory_id`) is always returned regardless of truncation so
/// the caller can decide whether to re-issue with `verbose=true`.
///
/// `pub` so the v0.7.0 #628 H6 cross-tenant test in
/// `tests/i4_memory_replay_authz.rs` can drive the handler directly.
/// Other handlers in this module remain private; the dispatcher is
/// their sole caller.

pub fn handle_replay(
    conn: &rusqlite::Connection,
    params: &Value,
    mcp_client: Option<&str>,
) -> Result<Value, String> {
    let memory_id = params["memory_id"]
        .as_str()
        .ok_or("memory_id is required")?;
    validate::validate_id(memory_id).map_err(|e| e.to_string())?;
    let verbose = params
        .get("verbose")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // L2-4 — optional depth cap on the reflection union walk. `null`
    // (or absent) means "full chain"; an integer `>=0` becomes the
    // hop cap on the BFS over `reflects_on` edges. We accept any
    // i64 the JSON layer surfaces, clamp at 0, and cast to u32 (the
    // substrate signature). Negative values are treated as `0`
    // (self-only) rather than rejected so a sloppy client doesn't
    // need to special-case the floor.
    let depth: Option<u32> = match params.get("depth") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_i64() {
            Some(n) if n < 0 => Some(0),
            Some(n) => Some(u32::try_from(n).unwrap_or(u32::MAX)),
            None => return Err("depth must be an integer or null".to_string()),
        },
    };

    // L2-4 substrate read — returns the union for reflections,
    // single-memory transcripts for observations. Ordering and
    // dedup live in the substrate so the handler stays a thin
    // serialisation wrapper.
    let entries: Vec<ReplayEntry> = replay_transcript_union(conn, memory_id, depth)
        .map_err(|e| format!("replay_transcript_union failed: {e}"))?;

    // v0.7.0 #628 H6 — authorise the replay against EACH transcript's
    // namespace before any decompressed content leaves the daemon. K9
    // is the unified surface; calling it per-transcript means an
    // operator's `[[permissions.rules]]` can scope by the transcript's
    // owning namespace rather than the calling memory's namespace
    // (the two diverge when an agent links a transcript stored in
    // namespace A to a memory in namespace B). On Deny we return an
    // MCP error WITHOUT leaking which transcripts existed; on Ask we
    // surface the prompt verbatim so the operator can wire the K10
    // approval pipeline. Allow / Modify let the read proceed.
    let agent_id = crate::identity::resolve_agent_id(params["agent_id"].as_str(), mcp_client)
        .map_err(|e| e.to_string())?;
    for entry in &entries {
        use crate::permissions::{Op, PermissionContext, Permissions};
        let ctx = PermissionContext {
            op: Op::MemoryReplay,
            namespace: entry.meta.namespace.clone(),
            agent_id: agent_id.clone(),
            payload: json!({
                "memory_id": memory_id,
                "transcript_id": entry.meta.id,
                "source_memory_id": entry.memory_id,
            }),
        };
        match Permissions::evaluate(&ctx, &[]) {
            crate::permissions::Decision::Allow | crate::permissions::Decision::Modify(_) => {}
            crate::permissions::Decision::Deny(reason) => {
                return Err(format!("replay denied by permission rule: {reason}"));
            }
            crate::permissions::Decision::Ask(prompt) => {
                return Ok(json!({
                    "status": "ask",
                    "reason": prompt,
                    "action": "replay",
                    "memory_id": memory_id,
                }));
            }
        }
    }

    let mut transcripts_json: Vec<Value> = Vec::with_capacity(entries.len());
    for entry in entries {
        let ReplayEntry {
            memory_id: src_mid,
            link,
            meta,
        } = entry;
        let truncate = !verbose && meta.original_size > REPLAY_VERBOSE_THRESHOLD_BYTES;
        let mut obj = serde_json::Map::new();
        obj.insert("id".into(), Value::String(meta.id.clone()));
        obj.insert("created_at".into(), Value::String(meta.created_at.clone()));
        obj.insert("compressed_size".into(), json!(meta.compressed_size));
        obj.insert("original_size".into(), json!(meta.original_size));
        obj.insert(
            "span_start".into(),
            link.span_start
                .map_or(Value::Null, |v| Value::Number(v.into())),
        );
        obj.insert(
            "span_end".into(),
            link.span_end
                .map_or(Value::Null, |v| Value::Number(v.into())),
        );
        // L2-4 — surface the anchor memory id so callers viewing a
        // reflection union know which ancestor each transcript came
        // from. For a non-reflection replay this is always equal to
        // the input `memory_id`, but emitting it unconditionally
        // keeps the wire shape uniform.
        obj.insert("source_memory_id".into(), Value::String(src_mid));
        if truncate {
            // Honest gate: announce the omission so the caller knows to
            // re-issue with `verbose=true` rather than silently
            // assuming the transcript is empty.
            obj.insert("truncated".into(), Value::Bool(true));
        } else {
            let content = crate::transcripts::fetch(conn, &meta.id)
                .map_err(|e| format!("transcripts::fetch failed: {e}"))?
                .ok_or_else(|| {
                    format!(
                        "transcript {} disappeared between metadata read and content fetch",
                        meta.id
                    )
                })?;
            obj.insert("content".into(), Value::String(content));
        }
        transcripts_json.push(Value::Object(obj));
    }

    Ok(json!({
        "memory_id": memory_id,
        "transcripts": transcripts_json,
        "count": transcripts_json.len(),
    }))
}

#[cfg(test)]
mod tests {
    //! Coverage C-2 — focused tests for `handle_replay`.

    use super::*;
    use crate::models::{Memory, MemoryKind, Tier};
    use crate::storage as db;
    use crate::transcripts;
    use serde_json::json;

    fn fresh_conn() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn seed_observation(conn: &rusqlite::Connection, ns: &str, title: &str) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: format!("body for {title}"),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({"agent_id": "ai:test"}),
            reflection_depth: 0,
            memory_kind: MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        };
        db::insert(conn, &mem).expect("insert")
    }

    // Validation: missing memory_id.
    #[test]
    fn missing_memory_id_errors() {
        let conn = fresh_conn();
        let err = handle_replay(&conn, &json!({}), None).unwrap_err();
        assert!(err.contains("memory_id"), "got: {err}");
    }

    // Validation: invalid memory_id.
    #[test]
    fn invalid_memory_id_rejected() {
        let conn = fresh_conn();
        let err = handle_replay(&conn, &json!({"memory_id": "  "}), None).unwrap_err();
        assert!(!err.is_empty());
    }

    // Validation: depth must be integer or null.
    #[test]
    fn depth_non_integer_rejected() {
        let conn = fresh_conn();
        let mid = seed_observation(&conn, "rp-ns", "obs");
        let err = handle_replay(
            &conn,
            &json!({"memory_id": mid, "depth": "not-a-number"}),
            None,
        )
        .unwrap_err();
        assert!(err.contains("depth must be an integer"), "got: {err}");
    }

    // Validation: negative depth clamps to 0 (no error).
    #[test]
    fn negative_depth_clamped() {
        let conn = fresh_conn();
        let mid = seed_observation(&conn, "rp-clamp", "obs");
        let resp = handle_replay(&conn, &json!({"memory_id": mid, "depth": -5}), None).expect("ok");
        assert_eq!(resp["memory_id"].as_str(), Some(mid.as_str()));
        assert_eq!(resp["count"].as_u64(), Some(0));
    }

    // Happy path with no transcripts — count=0, array empty.
    #[test]
    fn no_transcripts_returns_empty() {
        let conn = fresh_conn();
        let mid = seed_observation(&conn, "rp-empty", "obs");
        let resp = handle_replay(&conn, &json!({"memory_id": mid}), None).expect("ok");
        assert_eq!(resp["count"].as_u64(), Some(0));
        assert!(resp["transcripts"].as_array().unwrap().is_empty());
    }

    // Happy path with a tiny transcript — content surfaced (below threshold).
    #[test]
    fn small_transcript_returns_content() {
        let conn = fresh_conn();
        let mid = seed_observation(&conn, "rp-small", "obs");
        let t =
            transcripts::store(&conn, "rp-small", "short transcript content", None).expect("store");
        transcripts::link_transcript(&conn, &mid, &t.id, None, None).expect("link");
        let resp = handle_replay(&conn, &json!({"memory_id": mid}), None).expect("ok");
        assert_eq!(resp["count"].as_u64(), Some(1));
        let entries = resp["transcripts"].as_array().unwrap();
        assert!(entries[0]["content"].is_string());
        // Below the 100 KB threshold, no truncation marker.
        assert!(entries[0].get("truncated").is_none());
    }

    // Truncation rule — transcript above the verbose threshold is omitted
    // unless `verbose=true`.
    #[test]
    fn large_transcript_truncated_unless_verbose() {
        let conn = fresh_conn();
        let mid = seed_observation(&conn, "rp-large", "obs");
        // 101 KB of content — above the 100 KB threshold.
        let big = "x".repeat(101 * 1024);
        let t = transcripts::store(&conn, "rp-large", &big, None).expect("store");
        transcripts::link_transcript(&conn, &mid, &t.id, None, None).expect("link");
        let resp = handle_replay(&conn, &json!({"memory_id": mid}), None).expect("ok");
        let entries = resp["transcripts"].as_array().unwrap();
        assert_eq!(entries[0]["truncated"], true);
        assert!(entries[0].get("content").is_none());
    }

    // verbose=true forces content even on large transcripts.
    #[test]
    fn verbose_flag_returns_content_for_large() {
        let conn = fresh_conn();
        let mid = seed_observation(&conn, "rp-verbose", "obs");
        let big = "y".repeat(101 * 1024);
        let t = transcripts::store(&conn, "rp-verbose", &big, None).expect("store");
        transcripts::link_transcript(&conn, &mid, &t.id, None, None).expect("link");
        let resp =
            handle_replay(&conn, &json!({"memory_id": mid, "verbose": true}), None).expect("ok");
        let entries = resp["transcripts"].as_array().unwrap();
        assert!(entries[0]["content"].is_string());
        assert!(entries[0].get("truncated").is_none());
    }
}
