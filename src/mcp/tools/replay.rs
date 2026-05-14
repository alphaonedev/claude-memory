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
