// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L2-2 — cross-peer `reflection_depth` bookkeeping.
//!
//! The recursive-learning primitive (Task 4/8) caps `reflection_depth`
//! on the LOCAL host via `GovernancePolicy.effective_max_reflection_depth`.
//! Without cross-peer bookkeeping a peer can sync a depth-N reflection
//! into the substrate that would have been refused locally — and the
//! local curator can then derive further reflections from it,
//! laundering depth through federation.
//!
//! L2-2 closes that with three guarantees on the receive path:
//!
//! 1. **Origin recording.** When a reflection memory (a row whose
//!    `reflection_depth > 0` or whose `metadata.reflection_metadata`
//!    is present) lands via `sync_push`, the receiver stamps two
//!    fields under `metadata.reflection_origin`:
//!     - `peer_origin` — the `sender_agent_id` claim from the push
//!       envelope (the substrate-identity of the peer that pushed
//!       the row to us; not the original author, which is captured
//!       by `metadata.agent_id`).
//!     - `original_depth` — the depth the row carried in transit, so
//!       a later auditor can see what the source peer claimed.
//!     - `local_depth_at_arrival` — the local
//!       `effective_max_reflection_depth` at the moment the row
//!       arrived, so an after-the-fact tightening of the cap is
//!       visible on every imported row.
//!
//!    The original `reflection_depth` column itself is **preserved**
//!    — federation never silently rewrites depth. The local cap is
//!    enforced on **derived** writes, not on the inbound import.
//!
//! 2. **Derived-write enforcement.** [`enforce_local_cap_on_derived`]
//!    is invoked by `storage::reflect` (Task 4/8) before any NEW
//!    reflection lands. It computes the proposed `new_depth` against
//!    the LOCAL cap regardless of whether one or more sources are
//!    imported rows — the cap is local territorial sovereignty, not
//!    a remote peer's permission. Already enforced by the existing
//!    `reflect` path; the function here is the named hook so the
//!    audit emitter can surface the cross-peer context.
//!
//! 3. **Inspection surface.** [`reflection_origin`] returns the
//!    structured record for any memory id so the MCP
//!    `memory_reflection_origin` tool (and operators) can answer
//!    "where did this reflection come from?".
//!
//! This module is a substrate-layer helper. The HTTP receive path
//! (`handlers::federation_receive::sync_push`) calls
//! [`stamp_reflection_origin`] on every applied reflection memory;
//! the MCP `memory_reflection_origin` handler calls
//! [`reflection_origin`] for read-side queries.

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::models::Memory;

/// v0.7.0 L2-2 — metadata sub-object key holding the imported-from-peer
/// provenance. Lives under `Memory.metadata.reflection_origin` so the
/// reflection's own `metadata.reflection_metadata` (Task 4/8 substrate
/// stamp) stays untouched.
pub const REFLECTION_ORIGIN_KEY: &str = "reflection_origin";

/// v0.7.0 L2-2 — structured record returned by [`reflection_origin`].
/// Mirrors the wire shape of the `memory_reflection_origin` MCP tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReflectionOrigin {
    /// The id of the memory the record describes.
    pub memory_id: String,
    /// `sender_agent_id` from the push envelope that delivered the row.
    /// `None` for locally authored reflections (no peer_origin stamp).
    pub peer_origin: Option<String>,
    /// `metadata.agent_id` — the original signer (NHI). May differ from
    /// `peer_origin` when an intermediate peer re-broadcasts a row it
    /// itself received from upstream.
    pub signing_agent: Option<String>,
    /// `reflection_depth` the row carried in transit. Always populated
    /// when the row is a reflection (depth > 0).
    pub original_depth: i32,
    /// Snapshot of the local `effective_max_reflection_depth` at the
    /// moment this row was first imported. `None` when the row was
    /// authored locally (no import event to anchor against).
    pub local_depth_at_arrival: Option<u32>,
    /// `true` if the row is a reflection (depth > 0) regardless of
    /// import path; lets callers handle a non-reflection lookup with
    /// a clean response shape rather than a 404.
    pub is_reflection: bool,
}

/// v0.7.0 L2-2 — non-destructive metadata patch: stamps
/// `metadata.reflection_origin = { peer_origin, original_depth,
/// local_depth_at_arrival }` onto an inbound reflection memory's
/// metadata BEFORE it gets persisted via `insert_if_newer`.
///
/// Returns a mutated [`Memory`] with the patched metadata. The original
/// `reflection_depth` column is untouched — federation never silently
/// rewrites depth. The local cap enforcement happens later on derived
/// writes (see [`enforce_local_cap_on_derived`]).
///
/// Idempotent: if the row already carries a `reflection_origin` block
/// (e.g., we are processing the same push twice on a retry), the
/// existing stamp is preserved. The first peer to deliver the row wins
/// the origin record; downstream re-fans don't overwrite it.
///
/// Only reflection rows (`reflection_depth > 0`) get the stamp. Plain
/// memory rows pass through unchanged so the metadata bloat is bounded
/// to the reflection subgraph.
#[must_use]
pub fn stamp_reflection_origin(mem: &Memory, sender_agent_id: &str, local_cap: u32) -> Memory {
    // Non-reflections: untouched.
    if mem.reflection_depth <= 0 {
        return mem.clone();
    }
    let mut out = mem.clone();
    // Coerce metadata to an object (the canonical shape for memories);
    // if a peer sent us something weird (array / scalar / null), replace
    // it with a fresh object so the stamp lands cleanly.
    let mut meta_map: Map<String, Value> = match out.metadata.take() {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    // Idempotency: existing stamp wins. First delivery records the
    // peer; later re-fans never overwrite the substrate-of-record.
    if !meta_map.contains_key(REFLECTION_ORIGIN_KEY) {
        let stamp = serde_json::json!({
            "peer_origin": sender_agent_id,
            "original_depth": mem.reflection_depth,
            "local_depth_at_arrival": local_cap,
        });
        meta_map.insert(REFLECTION_ORIGIN_KEY.to_string(), stamp);
    }
    out.metadata = Value::Object(meta_map);
    out
}

/// v0.7.0 L2-2 — read-side lookup for the [`ReflectionOrigin`] record
/// of a memory by id. Backs the MCP `memory_reflection_origin` tool.
///
/// Returns `Ok(None)` when the id does not exist; returns a populated
/// record (with `is_reflection = false`) when the id exists but is not
/// a reflection — callers can then surface a clean "this memory is not
/// a reflection" response rather than a 404, which keeps the MCP tool's
/// shape uniform across reflection / non-reflection inputs.
///
/// # Errors
///
/// Wrapped rusqlite/SQL errors from the underlying `db::get` call.
pub fn reflection_origin(conn: &Connection, id: &str) -> Result<Option<ReflectionOrigin>> {
    let mem = match crate::storage::get(conn, id).context("storage::get for reflection_origin")? {
        Some(m) => m,
        None => return Ok(None),
    };
    let is_reflection = mem.reflection_depth > 0;
    let signing_agent = mem
        .metadata
        .get("agent_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    let origin_obj = mem.metadata.get(REFLECTION_ORIGIN_KEY);
    let peer_origin = origin_obj
        .and_then(|v| v.get("peer_origin"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let local_depth_at_arrival = origin_obj
        .and_then(|v| v.get("local_depth_at_arrival"))
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok());
    Ok(Some(ReflectionOrigin {
        memory_id: mem.id,
        peer_origin,
        signing_agent,
        original_depth: mem.reflection_depth,
        local_depth_at_arrival,
        is_reflection,
    }))
}

/// v0.7.0 L2-2 — enforcement hook for the LOCAL `max_reflection_depth`
/// cap on derived reflections. Called from the storage `reflect` path
/// before the new row commits, BUT the actual cap check already lives
/// in `storage::reflect::reflect`; this function is the named explainer
/// so the cross-peer context can be surfaced in audit + refusal text.
///
/// Given the source memories (including any imported ones) and the
/// proposed new depth, returns:
/// - `Ok(())` when the local cap is satisfied,
/// - `Err(LocalCapRefusal { ... })` with a refusal reason that names
///   the imported source's `peer_origin` when at least one source has
///   a `reflection_origin` stamp (so the operator sees the cross-peer
///   provenance in the refusal message).
///
/// # Errors
///
/// Returns [`LocalCapRefusal`] when `new_depth > local_cap`. Callers
/// map this back to `MemoryError::ReflectionDepthExceeded` for the
/// HTTP wire envelope.
pub fn enforce_local_cap_on_derived(
    new_depth: u32,
    local_cap: u32,
    sources: &[Memory],
) -> std::result::Result<(), LocalCapRefusal> {
    if new_depth <= local_cap {
        return Ok(());
    }
    // Identify any source whose `reflection_origin.peer_origin` is set
    // — those are the imported sources that drove the depth over the
    // local cap. Surface the first such peer in the refusal reason so
    // operators can see WHERE the depth came from at a glance.
    let imported_peer = sources.iter().find_map(|m| {
        m.metadata
            .get(REFLECTION_ORIGIN_KEY)
            .and_then(|v| v.get("peer_origin"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    let max_source_depth = sources
        .iter()
        .map(|m| m.reflection_depth)
        .max()
        .unwrap_or(0)
        .max(0);
    Err(LocalCapRefusal {
        attempted: new_depth,
        local_cap,
        max_source_depth: u32::try_from(max_source_depth).unwrap_or(u32::MAX),
        imported_peer,
    })
}

/// v0.7.0 L2-2 — refusal record returned by [`enforce_local_cap_on_derived`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalCapRefusal {
    /// Depth the curator attempted to write.
    pub attempted: u32,
    /// Local namespace cap that gated the write.
    pub local_cap: u32,
    /// Max `reflection_depth` across the supplied sources.
    pub max_source_depth: u32,
    /// First imported source's `peer_origin`, if any. `None` when no
    /// source has an import stamp (purely local subgraph).
    pub imported_peer: Option<String>,
}

impl std::fmt::Display for LocalCapRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.imported_peer.as_deref() {
            Some(peer) => write!(
                f,
                "remote reflection at depth {} (from peer {}), local depth limit {}",
                self.max_source_depth, peer, self.local_cap,
            ),
            None => write!(
                f,
                "reflection depth {} would exceed local cap {}",
                self.attempted, self.local_cap,
            ),
        }
    }
}

impl std::error::Error for LocalCapRefusal {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Tier;
    use chrono::Utc;

    fn reflection_memory(id: &str, depth: i32) -> Memory {
        let now = Utc::now().to_rfc3339();
        Memory {
            id: id.to_string(),
            tier: Tier::Mid,
            namespace: "test".to_string(),
            title: format!("reflection-{id}"),
            content: "body".to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id": "ai:test"}),
            reflection_depth: depth,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
            confidence_source: crate::models::ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
            version: 1,
        }
    }

    #[test]
    fn stamp_skips_non_reflection() {
        let mut mem = reflection_memory("m1", 0);
        let before = mem.metadata.clone();
        let stamped = stamp_reflection_origin(&mem, "peer-A", 3);
        assert_eq!(stamped.metadata, before);
        mem.reflection_depth = 0;
        assert!(stamped.metadata.get(REFLECTION_ORIGIN_KEY).is_none());
    }

    #[test]
    fn stamp_records_peer_and_local_cap() {
        let mem = reflection_memory("m1", 2);
        let stamped = stamp_reflection_origin(&mem, "peer-A", 3);
        let origin = stamped
            .metadata
            .get(REFLECTION_ORIGIN_KEY)
            .expect("origin stamped");
        assert_eq!(origin["peer_origin"].as_str(), Some("peer-A"));
        assert_eq!(origin["original_depth"].as_i64(), Some(2));
        assert_eq!(origin["local_depth_at_arrival"].as_u64(), Some(3));
    }

    #[test]
    fn stamp_is_idempotent_first_writer_wins() {
        let mem = reflection_memory("m1", 2);
        let first = stamp_reflection_origin(&mem, "peer-A", 3);
        let second = stamp_reflection_origin(&first, "peer-B", 5);
        let origin = second
            .metadata
            .get(REFLECTION_ORIGIN_KEY)
            .expect("origin preserved");
        // peer-A wins; peer-B's re-fan didn't overwrite.
        assert_eq!(origin["peer_origin"].as_str(), Some("peer-A"));
        assert_eq!(origin["local_depth_at_arrival"].as_u64(), Some(3));
    }

    #[test]
    fn enforce_local_cap_allows_when_under_limit() {
        let sources = vec![reflection_memory("s1", 1)];
        assert!(enforce_local_cap_on_derived(2, 3, &sources).is_ok());
    }

    #[test]
    fn enforce_local_cap_refuses_with_imported_peer_named() {
        let mut imported = reflection_memory("s1", 2);
        imported.metadata = serde_json::json!({
            "agent_id": "ai:test",
            REFLECTION_ORIGIN_KEY: {
                "peer_origin": "peer-A",
                "original_depth": 2,
                "local_depth_at_arrival": 3,
            },
        });
        let refusal = enforce_local_cap_on_derived(3, 2, &[imported]).unwrap_err();
        let msg = refusal.to_string();
        assert!(
            msg.contains("peer-A") && msg.contains("local depth limit 2"),
            "refusal msg should name peer + local cap: {msg}"
        );
    }
}
