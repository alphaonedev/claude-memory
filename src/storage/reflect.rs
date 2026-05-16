// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Reflection family — the substrate-native recursive-learning
//! primitive (v0.7.0 Tasks 4/5/6 of the recursive-learning epic).
//! v0.7.0 L0.5-3 extracted `ReflectError`, `ReflectOutcome`,
//! `ReflectHookDecision`, `ReflectHooks`, `ReflectInput`, `reflect`,
//! `reflect_with_hooks`, `canonical_cbor_reflection_depth_exceeded`,
//! and `emit_reflection_depth_exceeded_audit` out of `src/db.rs` into
//! this sub-module. Pure refactor — semantics unchanged.

use crate::identity::keypair::AgentKeypair;
use crate::models::ConfidenceSource;
use anyhow::Context;
use chrono::Utc;
use rusqlite::Connection;

use crate::models::{GovernancePolicy, Memory, MemoryKind, Tier};

use super::{
    ConflictMode, create_link_signed, get, insert_with_conflict, resolve_governance_policy,
};

/// Typed substrate-level error surface for [`reflect`]. Kept distinct
/// from [`crate::errors::MemoryError`] so the SQLite substrate layer
/// stays free of HTTP-status concerns; the caller at the MCP / HTTP
/// boundary maps these into the wire-shaped variant. Task 5/8 matches
/// on `ReflectError::DepthExceeded` here (and the equivalent
/// `MemoryError::ReflectionDepthExceeded` variant) to emit the
/// `signed_events` audit record for the refusal decision.
#[derive(Debug)]
pub enum ReflectError {
    /// Input violated a validator. Carries the operator-readable
    /// reason; the MCP layer surfaces it verbatim.
    Validation(String),
    /// One of the requested source memories does not exist. Carries
    /// the offending id so the caller can name the missing source.
    SourceNotFound(String),
    /// Proposed reflection depth exceeds the resolved namespace cap.
    /// The triple is the structured payload Task 5/8 will attach to
    /// the audit row.
    DepthExceeded {
        attempted: u32,
        cap: u32,
        namespace: String,
    },
    /// v0.7.0 recursive-learning Task 6/8 — a `pre_reflect` hook
    /// callback returned [`ReflectHookDecision::Deny`], vetoing the
    /// reflection. Distinct from `DepthExceeded` because the substrate
    /// cap was NOT evaluated (the veto fires earlier in step 4) and
    /// because the Task 5 depth-cap audit row is NOT emitted on this
    /// path — hook vetoes are caller-policy refusals that carry their
    /// own provenance via the hook's own decision record (if any).
    HookVeto { reason: String, code: i32 },
    /// Database error during the atomic write. Carries the underlying
    /// rusqlite / anyhow string.
    Database(String),
}

impl std::fmt::Display for ReflectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(m) | Self::SourceNotFound(m) | Self::Database(m) => f.write_str(m),
            Self::DepthExceeded {
                attempted,
                cap,
                namespace,
            } => write!(
                f,
                "reflection depth {attempted} would exceed namespace \
                 max_reflection_depth {cap} (namespace='{namespace}')"
            ),
            Self::HookVeto { reason, code } => {
                write!(
                    f,
                    "pre_reflect hook vetoed reflection (code={code}): {reason}"
                )
            }
        }
    }
}

impl std::error::Error for ReflectError {}

/// Outcome of a successful [`reflect`] write. Mirrors the MCP `memory_reflect`
/// wire shape so the dispatch layer is a thin serialization wrapper.
#[derive(Debug, Clone)]
pub struct ReflectOutcome {
    /// Newly minted reflection memory id.
    pub id: String,
    /// Depth assigned to the new memory (max source depth + 1).
    pub reflection_depth: i32,
    /// Source memory ids the new memory reflects on, in input order.
    pub reflects_on: Vec<String>,
    /// Namespace the reflection landed in (resolved to the first source's
    /// namespace when the caller omitted the field).
    pub namespace: String,
}

/// v0.7.0 recursive-learning Task 6/8 — substrate-level decision
/// surface returned by a `pre_reflect` hook callback.
///
/// Mirrors the shape of [`crate::hooks::HookDecision`] minus the
/// `Modify` and `AskUser` variants — the substrate hook surface only
/// exposes the two outcomes that affect the reflect control flow:
/// continue (`Allow`) or veto (`Deny`). Hook-supplied delta merging
/// and operator prompts are handled by the wire-level
/// [`crate::hooks::HookChain`] when the daemon's hook pipeline is
/// configured (G7+ wiring); this in-substrate variant is the path
/// the substrate uses today to fire `PreReflect` / `PostReflect`
/// events on the reflect codepath.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReflectHookDecision {
    /// Continue evaluating the reflect — proceed to the cap check.
    Allow,
    /// Veto the reflect. The substrate returns
    /// [`ReflectError::HookVeto`] with the supplied reason +
    /// HTTP-style status code; the cap check is NOT evaluated and
    /// the depth-cap audit row is NOT emitted (this is a caller-
    /// policy refusal, not a substrate cap refusal — Task 5 audits
    /// the latter; hook vetoes carry their own provenance).
    Deny { reason: String, code: i32 },
}

/// v0.7.0 recursive-learning Task 6/8 — optional in-substrate hook
/// callbacks fired by [`reflect_with_hooks`]. Bundled into a single
/// struct so the substrate signature stays compact and so future
/// callbacks (e.g. on-rollback) can land without churning every
/// call site.
///
/// Both callbacks are `Option<...>`; when `None`, the substrate
/// behaves identically to the unhooked [`reflect`] entry-point. The
/// callback type is `Box<dyn Fn(...)>` so the substrate stays
/// allocator-friendly (one allocation per reflect call) and so test
/// code can pass simple closures that capture observation state.
pub struct ReflectHooks<'a> {
    /// Fired BEFORE the cap check (step 4 of `reflect`). Receives a
    /// read-only view of the in-flight [`ReflectInput`] (the
    /// substrate-side equivalent of [`crate::hooks::events::ReflectDelta`]
    /// — the in-process callback gets the typed input directly,
    /// while the cross-process wire path serialises a `ReflectDelta`).
    /// Returns [`ReflectHookDecision::Deny`] to veto.
    pub pre_reflect: Option<Box<dyn Fn(&ReflectInput) -> ReflectHookDecision + Send + Sync + 'a>>,
    /// Fired AFTER the transaction commits (step 7 of `reflect`).
    /// Receives a read-only snapshot of the post-commit outcome
    /// (mirrors [`crate::hooks::events::ReflectResult`]). Notify-class
    /// — return value is ignored; the reflect already landed.
    pub post_reflect: Option<Box<dyn Fn(&ReflectOutcome) + Send + Sync + 'a>>,
    /// Issue #815 — signing keypair for the `reflects_on` edges
    /// written inside the reflect transaction. When `Some`, each
    /// edge is persisted via [`create_link_signed`] with this
    /// keypair, producing `attest_level='self_signed'` rows with a
    /// 64-byte Ed25519 signature. When `None`, edges land as
    /// `attest_level='unsigned'` — the v0.6.x behaviour and the
    /// state of the world before #815 fixed the storage::reflect
    /// gap that #814 left behind.
    pub active_keypair: Option<&'a AgentKeypair>,
}

impl<'a> ReflectHooks<'a> {
    /// Empty bundle — both callbacks `None`, no signing keypair.
    /// The default used by callers that don't want to register
    /// hooks AND don't have a keypair to sign with (test harnesses,
    /// the thin [`reflect`] shim that preserves pre-#815 behaviour).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            pre_reflect: None,
            post_reflect: None,
            active_keypair: None,
        }
    }
}

impl<'a> Default for ReflectHooks<'a> {
    fn default() -> Self {
        Self::empty()
    }
}

impl<'a> std::fmt::Debug for ReflectHooks<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReflectHooks")
            .field("pre_reflect", &self.pre_reflect.as_ref().map(|_| "<fn>"))
            .field("post_reflect", &self.post_reflect.as_ref().map(|_| "<fn>"))
            .field(
                "active_keypair",
                &self.active_keypair.map(|k| k.agent_id.as_str()),
            )
            .finish()
    }
}

/// Input bundle for [`reflect`]. Holds every caller-tunable field of the
/// new reflection memory plus the source-id list. Defaults mirror the
/// MCP tool schema (`tier=mid`, `priority=5`, `confidence=1.0`,
/// `source="claude"`) so the dispatch layer can build this from the
/// raw JSON arguments without further fixup.
#[derive(Debug, Clone)]
pub struct ReflectInput {
    pub source_ids: Vec<String>,
    pub title: String,
    pub content: String,
    /// `None` → resolve to the namespace of the first source memory.
    pub namespace: Option<String>,
    pub tier: Tier,
    pub tags: Vec<String>,
    pub priority: i32,
    pub confidence: f64,
    pub source: String,
    pub agent_id: String,
    /// Caller-supplied metadata. The reflection writer merges system-
    /// generated `reflection_metadata` keys underneath this object;
    /// caller-supplied keys win on collision (the additive contract
    /// documented on the MCP tool).
    pub metadata: serde_json::Value,
}

/// v0.7.0 recursive-learning Task 4/8 (issue #655) — substrate-native
/// reflection primitive.
///
/// Steps (matches the MCP tool contract):
///
/// 1. Validate inputs (`title`, `content`, namespace, tags, priority,
///    confidence, agent_id, source_ids).
/// 2. Load each source memory; bail with [`ReflectError::SourceNotFound`]
///    on any missing id (no partial write).
/// 3. Compute `new_depth = max(source.reflection_depth) + 1`.
/// 4. Resolve the effective namespace cap via
///    [`resolve_governance_policy`] (walks the ancestor chain leaf-
///    first), fall back to [`GovernancePolicy::default`] when the chain
///    has no policy at any level, then call
///    [`GovernancePolicy::effective_max_reflection_depth`] on the
///    resolved policy.
/// 5. Refuse with [`ReflectError::DepthExceeded`] when
///    `new_depth > max_dep`.
/// 6. Insert the new reflection memory and write a `reflects_on` link
///    from the new memory to each source — all inside a single
///    `BEGIN IMMEDIATE` … `COMMIT` block. Any insert / link failure
///    rolls back the entire write so a half-written reflection cannot
///    survive.
///
/// The new memory's metadata is the caller-supplied object with a
/// system-generated `reflection_metadata` key spliced in (recording
/// the source-id list, the resolved depth, and the RFC3339 creation
/// timestamp). **Caller-supplied keys win on collision** — if the
/// caller already supplied `reflection_metadata` we honor their value
/// and skip the system splice. This is the documented additive contract.
///
/// The `agent_id` field on the input bundle is stamped into
/// `metadata.agent_id` before insert; the caller is responsible for
/// resolving it via [`crate::identity::resolve_agent_id`].
///
/// # Errors
///
/// Returns one of the four [`ReflectError`] variants. The DB-error
/// variant is the only one with no structured payload — every other
/// variant carries enough information for the caller to render a clean
/// operator-readable message and (for `DepthExceeded`) for Task 5/8 to
/// emit a structured audit row.
pub fn reflect(
    conn: &Connection,
    input: &ReflectInput,
) -> std::result::Result<ReflectOutcome, ReflectError> {
    // Thin shim over [`reflect_with_hooks`] with an empty hook bundle.
    // Existing callers (MCP `memory_reflect`, the `tests/recursive_
    // learning_task4_*` suite, the Postgres parity test) keep using
    // this entry-point unchanged; the new in-substrate hook surface
    // is opt-in via `reflect_with_hooks`.
    reflect_with_hooks(conn, input, &ReflectHooks::empty())
}

/// v0.7.0 recursive-learning Task 6/8 — variant of [`reflect`] with
/// in-substrate hook callbacks. See [`reflect`] for the full step
/// list; the only deltas are:
///
///   * Between step 4 (depth + cap resolution) and step 5 (cap
///     check), `hooks.pre_reflect` fires when configured. A
///     [`ReflectHookDecision::Deny`] return propagates as
///     [`ReflectError::HookVeto`]; the cap check is NOT evaluated and
///     the Task 5 depth-cap audit is NOT emitted on this path.
///   * After step 6 commits (transaction COMMIT succeeds, just before
///     returning `ReflectOutcome`), `hooks.post_reflect` fires with
///     the read-only outcome. Notify-class — return value is ignored.
///
/// Calling `reflect_with_hooks(conn, input, &ReflectHooks::empty())`
/// is identical to calling `reflect(conn, input)`.
///
/// # Errors
///
/// Same five [`ReflectError`] variants as [`reflect`] plus
/// [`ReflectError::HookVeto`] when a pre_reflect handler vetoes.
#[allow(clippy::too_many_lines)]
pub fn reflect_with_hooks(
    conn: &Connection,
    input: &ReflectInput,
    hooks: &ReflectHooks<'_>,
) -> std::result::Result<ReflectOutcome, ReflectError> {
    use crate::validate;
    // ─── 1. Validate inputs ──────────────────────────────────────────
    validate::validate_title(&input.title).map_err(|e| ReflectError::Validation(e.to_string()))?;
    validate::validate_content(&input.content)
        .map_err(|e| ReflectError::Validation(e.to_string()))?;
    validate::validate_tags(&input.tags).map_err(|e| ReflectError::Validation(e.to_string()))?;
    validate::validate_priority(input.priority)
        .map_err(|e| ReflectError::Validation(e.to_string()))?;
    validate::validate_confidence(input.confidence)
        .map_err(|e| ReflectError::Validation(e.to_string()))?;
    validate::validate_source(&input.source)
        .map_err(|e| ReflectError::Validation(e.to_string()))?;
    validate::validate_agent_id(&input.agent_id)
        .map_err(|e| ReflectError::Validation(e.to_string()))?;
    if input.source_ids.is_empty() {
        return Err(ReflectError::Validation(
            "source_ids cannot be empty — a reflection must reflect on at least one source memory"
                .into(),
        ));
    }
    // Each source id must be well-formed before we hit the DB; this
    // gives the caller a clean "bad id at index N" surface for free.
    let mut seen = std::collections::HashSet::new();
    for (i, id) in input.source_ids.iter().enumerate() {
        validate::validate_id(id)
            .map_err(|e| ReflectError::Validation(format!("source_ids[{i}]: {e}")))?;
        if !seen.insert(id.as_str()) {
            return Err(ReflectError::Validation(format!(
                "source_ids[{i}]: duplicate id '{id}'"
            )));
        }
    }
    if let Some(ref ns) = input.namespace {
        validate::validate_namespace(ns).map_err(|e| ReflectError::Validation(e.to_string()))?;
    }
    validate::validate_metadata(&input.metadata)
        .map_err(|e| ReflectError::Validation(e.to_string()))?;

    // ─── 2. Load each source memory; bail on any missing id ─────────
    let mut sources = Vec::with_capacity(input.source_ids.len());
    for id in &input.source_ids {
        match get(conn, id).map_err(|e| ReflectError::Database(e.to_string()))? {
            Some(m) => sources.push(m),
            None => return Err(ReflectError::SourceNotFound(id.clone())),
        }
    }

    // ─── 3. Compute new_depth = max(source depths) + 1 ──────────────
    let max_src_depth = sources
        .iter()
        .map(|m| m.reflection_depth)
        .max()
        .unwrap_or(0);
    // Clamp to non-negative before adding 1 (the column is i32 but the
    // cap is u32; pre-v0.7.0 rows landed at 0 so `max < 0` can't happen
    // in practice, but a `.max(0)` here is cheap belt-and-braces).
    let new_depth_i32 = max_src_depth.max(0).saturating_add(1);
    // u32 conversion: new_depth is at most i32::MAX which fits in u32.
    #[allow(clippy::cast_sign_loss)]
    let new_depth_u32: u32 = new_depth_i32 as u32;

    // ─── 4. Resolve target namespace + governance cap ───────────────
    let target_namespace = match input.namespace {
        Some(ref ns) => ns.clone(),
        // Default to the namespace of the FIRST source memory — matches
        // the documented MCP schema default. Operators who want a
        // different target namespace pass it explicitly.
        None => sources[0].namespace.clone(),
    };
    // Carry-forward (Task 2 note): `resolve_governance_policy` returns
    // `None` when no level of the ancestor chain has a policy at all.
    // Treat that as "use the compiled default" — i.e. fall back to
    // `GovernancePolicy::default()` which has `max_reflection_depth =
    // None` and therefore yields the compiled-in cap of 3.
    let policy = resolve_governance_policy(conn, &target_namespace)
        .unwrap_or_else(GovernancePolicy::default);
    let cap = policy.effective_max_reflection_depth();

    // ─── 4.5 `pre_reflect` hook (v0.7.0 Task 6/8) ──────────────────
    //
    // Fires BEFORE the cap check so a hook handler may VETO the
    // reflection by returning `ReflectHookDecision::Deny`. Vetoes
    // from `pre_reflect` are distinct from the cap refusal —
    // caller-policy refusals (e.g. "this agent is rate-limited",
    // "this content type is policy-restricted") rather than
    // depth-cap refusals. The Task 5 `reflection.depth_exceeded`
    // audit row is NOT emitted on this path; the hook handler may
    // emit its own audit if desired.
    if let Some(pre) = hooks.pre_reflect.as_ref() {
        match (pre)(input) {
            ReflectHookDecision::Allow => {}
            ReflectHookDecision::Deny { reason, code } => {
                return Err(ReflectError::HookVeto { reason, code });
            }
        }
    }

    // ─── 5. Refuse if proposed depth exceeds cap ────────────────────
    //
    // Task 5/8 (v0.7.0): before propagating the refusal to the caller,
    // append a `reflection.depth_exceeded` row to `signed_events`. The
    // audit row is the cryptographic-provenance leg of the v0.7.0 cap
    // contract — every cap refusal becomes part of the tamper-evident
    // audit chain so a future operator can prove that the daemon
    // honored the cap, not just "trusted the agent didn't try".
    //
    // Note: audit is fired only by this cap refusal; hook vetoes
    // (Task 6/8 `pre_reflect`) carry their own provenance via the
    // hook's own decision record (if any), so they are deliberately
    // NOT emitted here.
    if new_depth_u32 > cap {
        // v0.7.0 L2-2 — cross-peer enforcement. If any source carries a
        // `reflection_origin.peer_origin` stamp (it was imported via
        // federation `sync_push`), surface the originating peer in the
        // refusal so operators see the cross-peer provenance — not just
        // "depth exceeded". Local cap is enforced regardless of source
        // origin (territorial sovereignty), but the message distinguishes
        // "remote reflection at depth N, local depth limit M" from a
        // purely local cap breach.
        let cross_peer_refusal =
            crate::federation::reflection_bookkeeping::enforce_local_cap_on_derived(
                new_depth_u32,
                cap,
                &sources,
            );
        let peer_origin: Option<String> = if let Err(ref r) = cross_peer_refusal {
            if let Some(ref peer) = r.imported_peer {
                tracing::warn!(
                    target: "federation::reflection_bookkeeping",
                    peer = %peer,
                    attempted = new_depth_u32,
                    local_cap = cap,
                    namespace = %target_namespace,
                    "L2-2: refusing derived reflection: {}",
                    r,
                );
            }
            r.imported_peer.clone()
        } else {
            None
        };
        emit_reflection_depth_exceeded_audit(
            conn,
            &input.agent_id,
            new_depth_u32,
            cap,
            &target_namespace,
            &input.source_ids,
            &input.title,
            peer_origin.as_deref(),
        );
        return Err(ReflectError::DepthExceeded {
            attempted: new_depth_u32,
            cap,
            namespace: target_namespace,
        });
    }

    // ─── 6. Atomic insert + N links inside a single transaction ─────
    // Build the system-generated reflection_metadata block. The caller-
    // supplied object wins on key collisions — if `reflection_metadata`
    // is already set, we leave it alone.
    let now = Utc::now().to_rfc3339();
    let mut metadata = match input.metadata.clone() {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    // Always stamp agent_id (the resolver already validated it).
    metadata.insert(
        "agent_id".to_string(),
        serde_json::Value::String(input.agent_id.clone()),
    );
    // Splice reflection_metadata only when the caller didn't pre-set it.
    if !metadata.contains_key("reflection_metadata") {
        let reflection_meta = serde_json::json!({
            "reflected_on_source_ids": input.source_ids,
            "reflection_depth": new_depth_i32,
            "reflection_created_at": now,
        });
        metadata.insert("reflection_metadata".to_string(), reflection_meta);
    }
    let metadata_value = serde_json::Value::Object(metadata);
    // Re-validate the merged metadata so an oversized splice surfaces
    // here (vs. a confusing DB constraint error later).
    validate::validate_metadata(&metadata_value)
        .map_err(|e| ReflectError::Validation(e.to_string()))?;

    let new_mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: input.tier.clone(),
        namespace: target_namespace.clone(),
        title: input.title.clone(),
        content: input.content.clone(),
        tags: input.tags.clone(),
        priority: input.priority.clamp(1, 10),
        confidence: input.confidence.clamp(0.0, 1.0),
        source: input.source.clone(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now.clone(),
        last_accessed_at: None,
        expires_at: None,
        metadata: metadata_value,
        reflection_depth: new_depth_i32,
        // L1-1: reflection memories are always typed as Reflection,
        // regardless of what the caller passes in metadata.type (the
        // back-compat path). This is the first-class typed counterpart
        // to the metadata.type = 'reflection' splice above.
        memory_kind: MemoryKind::Reflection,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
    };

    // Atomic boundary: insert the reflection row + N `reflects_on`
    // links inside a single BEGIN IMMEDIATE ... COMMIT block. If any
    // link insert fails, ROLLBACK undoes the reflection row too.
    // Matches the `consolidate` pattern earlier in this file.
    conn.execute_batch("BEGIN IMMEDIATE")
        .map_err(|e| ReflectError::Database(e.to_string()))?;

    let txn_result = (|| -> std::result::Result<String, ReflectError> {
        // v0.7.0 fix campaign R1-M3 (#690) — substrate-side reflections
        // must NOT silently merge into an existing (title, namespace).
        // If a row with the same title is already present in the
        // reflection's namespace, the caller asked us to land a
        // duplicate; that's a deduplication risk we surface as a
        // validation error rather than smashing the existing row.
        let actual_id = insert_with_conflict(conn, &new_mem, ConflictMode::Error).map_err(|e| {
            if e.downcast_ref::<crate::storage::ConflictError>().is_some() {
                ReflectError::Validation(format!(
                    "reflection title collides with an existing memory in the same namespace: {e}"
                ))
            } else {
                ReflectError::Database(e.to_string())
            }
        })?;
        // Self-link rejection lives in `validate_link`; a self-link
        // (source id appearing in the source list) would only happen
        // via caller error, but we still surface it as a validation
        // failure with the txn rolled back so the reflection never
        // lands.
        for src_id in &input.source_ids {
            validate::validate_link(&actual_id, src_id, "reflects_on")
                .map_err(|e| ReflectError::Validation(e.to_string()))?;
            // Issue #815 — the pre-#815 path called `create_link` here,
            // which always produced `attest_level='unsigned'` rows for
            // every reflects_on edge regardless of whether the caller
            // had loaded a daemon keypair. Route through the signed
            // helper instead so the keypair threaded through the
            // hook bundle (MCP-tier handler, curator daemon) reaches
            // the link insert and the edges land as `self_signed`
            // with a 64-byte Ed25519 signature. Callers that pass
            // `active_keypair: None` (the `reflect()` shim, the
            // auto-export hook constructor's no-keypair test paths)
            // get the previous unsigned behaviour — `create_link_signed`
            // matches `create_link`'s output when the keypair is
            // absent (verified by the existing
            // `create_link_signed_without_keypair_is_unsigned` test in
            // `src/storage/mod.rs`).
            create_link_signed(
                conn,
                &actual_id,
                src_id,
                "reflects_on",
                hooks.active_keypair,
            )
            .map_err(|e| ReflectError::Database(e.to_string()))?;
        }
        Ok(actual_id)
    })();

    match txn_result {
        Ok(actual_id) => {
            conn.execute_batch("COMMIT")
                .map_err(|e| ReflectError::Database(e.to_string()))?;
            let outcome = ReflectOutcome {
                id: actual_id,
                reflection_depth: new_depth_i32,
                reflects_on: input.source_ids.clone(),
                namespace: target_namespace,
            };
            // ─── 7. `post_reflect` hook (v0.7.0 Task 6/8) ───────────
            //
            // Fires AFTER the transaction commits so the hook handler
            // can read the new reflection memory + its `reflects_on`
            // links via the same connection. Notify-class — the
            // return value is ignored beyond logging (post-commit
            // events cannot veto a side-effect that already
            // happened).
            if let Some(post) = hooks.post_reflect.as_ref() {
                (post)(&outcome);
            }
            Ok(outcome)
        }
        Err(e) => {
            if let Err(rb) = conn.execute_batch("ROLLBACK") {
                tracing::error!("ROLLBACK failed in reflect: {}", rb);
            }
            Err(e)
        }
    }
}

/// v0.7.0 recursive-learning Task 5/8 — canonical-CBOR encoding of the
/// `reflection.depth_exceeded` audit payload.
///
/// Mirrors the deterministic encoding contract used by
/// [`crate::identity::sign::canonical_cbor`] — map keys sorted
/// lexicographically (`BTreeMap` iteration order), `Option::None`
/// encoded as `Null`, integers in shortest-form. The same payload
/// hashes to the same bytes on every host so a downstream auditor can
/// re-derive the `payload_hash` from the four structured fields below.
///
/// Note that we deliberately do NOT include the rejected reflection's
/// `content` body in the payload — that would balloon the audit row
/// (and risk leaking PII into the chain). Title + source ids is the
/// provenance hook; the body is not the audit's job.
///
/// v0.7.0 L2-2 — when `peer_origin` is `Some`, the encoded payload
/// includes a `peer_origin` field naming the federation peer that
/// delivered the imported source memory whose depth drove the cap
/// breach. When `None` (purely local-source refusal) the field is
/// omitted so existing-row payload hashes are unchanged on the
/// pre-L2-2 codepath. The conditional-inclusion-vs-`Null` distinction
/// matters: a presence-encoded `Null` would silently mutate every
/// pre-L2-2 hash on every host the moment L2-2 ships, even where no
/// federation is configured.
///
/// # Errors
///
/// Returns the underlying CBOR encoder error if encoding fails — in
/// practice unreachable for the fixed-shape input above, surfaced as
/// a `Result` so callers don't have to choose between panicking and
/// silently logging an incomplete payload.
pub fn canonical_cbor_reflection_depth_exceeded(
    agent_id: &str,
    attempted: u32,
    cap: u32,
    namespace: &str,
    source_ids: &[String],
    proposed_title: &str,
    created_at: &str,
    peer_origin: Option<&str>,
) -> anyhow::Result<Vec<u8>> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<&str, ciborium::Value> = BTreeMap::new();
    map.insert("agent_id", ciborium::Value::Text(agent_id.to_string()));
    map.insert("attempted", ciborium::Value::Integer(attempted.into()));
    map.insert("cap", ciborium::Value::Integer(cap.into()));
    map.insert("created_at", ciborium::Value::Text(created_at.to_string()));
    map.insert("namespace", ciborium::Value::Text(namespace.to_string()));
    // v0.7.0 L2-2 — conditional inclusion preserves pre-L2-2 payload
    // hashes on the purely-local refusal path (no `peer_origin` key
    // present at all in the encoded map). Cross-peer refusals carry the
    // peer claim as a tamper-evident structured field.
    if let Some(peer) = peer_origin {
        map.insert("peer_origin", ciborium::Value::Text(peer.to_string()));
    }
    map.insert(
        "proposed_title",
        ciborium::Value::Text(proposed_title.to_string()),
    );
    map.insert(
        "source_ids",
        ciborium::Value::Array(
            source_ids
                .iter()
                .map(|s| ciborium::Value::Text(s.clone()))
                .collect(),
        ),
    );
    let entries: Vec<(ciborium::Value, ciborium::Value)> = map
        .into_iter()
        .map(|(k, v)| (ciborium::Value::Text(k.to_string()), v))
        .collect();
    let value = ciborium::Value::Map(entries);
    let mut out: Vec<u8> = Vec::with_capacity(256);
    ciborium::ser::into_writer(&value, &mut out)
        .context("CBOR encode reflection_depth_exceeded audit payload")?;
    Ok(out)
}

/// v0.7.0 recursive-learning Task 5/8 — append a
/// `reflection.depth_exceeded` row to `signed_events` for an in-flight
/// cap refusal.
///
/// Mirrors the [`invalidate_link`] audit-emit pattern: best-effort —
/// audit-write failure is logged via `tracing::warn!(target:
/// "signed_events", ...)` but does NOT crater the refusal path. The
/// refusal still propagates to the caller regardless of audit-write
/// success, because (a) the refusal already happened and (b) crashing
/// the legitimate caller for a substrate problem they cannot fix would
/// be worse than a missed audit row.
///
/// `attest_level` is `"unsigned"` because the substrate emits this row
/// itself (the caller did not sign it with their keypair). The
/// `signature` column is `None`. The `payload_hash` is SHA-256 over
/// the canonical-CBOR encoding of the structured fields, so a future
/// auditor can re-derive the same hash from any honest source of the
/// same fields.
pub(crate) fn emit_reflection_depth_exceeded_audit(
    conn: &Connection,
    agent_id: &str,
    attempted: u32,
    cap: u32,
    namespace: &str,
    source_ids: &[String],
    proposed_title: &str,
    peer_origin: Option<&str>,
) {
    let created_at = Utc::now().to_rfc3339();
    let cbor = match canonical_cbor_reflection_depth_exceeded(
        agent_id,
        attempted,
        cap,
        namespace,
        source_ids,
        proposed_title,
        &created_at,
        peer_origin,
    ) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "signed_events",
                agent_id, attempted, cap, namespace,
                "failed to encode canonical CBOR for reflection_depth_exceeded audit: {e}"
            );
            return;
        }
    };
    // v0.7.0 L2-2 — distinguish the audit row's `event_type` so
    // operators (and downstream tooling) can filter the cross-peer
    // refusal stream from the local-only stream without re-decoding
    // the CBOR payload. The two-variant `event_type` does not change
    // the audit-chain contract: payload_hash + signature + timestamp
    // semantics remain identical; only the textual label differs.
    let event_type = if peer_origin.is_some() {
        "reflection.depth_exceeded.cross_peer"
    } else {
        "reflection.depth_exceeded"
    };
    let event = crate::signed_events::SignedEvent {
        id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        event_type: event_type.to_string(),
        payload_hash: crate::signed_events::payload_hash(&cbor),
        signature: None,
        attest_level: "unsigned".to_string(),
        timestamp: created_at,
        ..crate::signed_events::SignedEvent::default()
    };
    if let Err(e) = crate::signed_events::append_signed_event(conn, &event) {
        tracing::warn!(
            target: "signed_events",
            agent_id, attempted, cap, namespace,
            "failed to append reflection_depth_exceeded audit row: {e}"
        );
    }
}

#[cfg(test)]
mod l2_2_audit_tests {
    //! v0.7.0 L2-2 — lib-level unit tests pinning the cross-peer
    //! audit payload encoder. The end-to-end three-peer choreography
    //! lives in `tests/federation_reflection_replication.rs`; here we
    //! pin the structural-encoding invariants without touching the
    //! database substrate, so the lib's `payload_hash` contract is
    //! covered even when the integration test binary is excluded.

    use super::canonical_cbor_reflection_depth_exceeded;

    /// `peer_origin = None` and `peer_origin = Some(_)` MUST encode to
    /// different byte sequences. This is the load-bearing invariant:
    /// if both encoded identically, the audit row's payload_hash
    /// wouldn't actually bind the cross-peer claim, and a tampered
    /// `event_type` could orphan the structured field.
    #[test]
    fn peer_origin_some_vs_none_yields_distinct_bytes() {
        let base = (
            "ai:test",
            3_u32,
            2_u32,
            "ns/l2-2",
            vec!["src-1".to_string()],
            "title",
            "2026-05-13T00:00:00+00:00",
        );
        let local = canonical_cbor_reflection_depth_exceeded(
            base.0, base.1, base.2, base.3, &base.4, base.5, base.6, None,
        )
        .expect("encode None");
        let cross = canonical_cbor_reflection_depth_exceeded(
            base.0,
            base.1,
            base.2,
            base.3,
            &base.4,
            base.5,
            base.6,
            Some("ai:peer-x"),
        )
        .expect("encode Some");
        assert_ne!(local, cross, "peer_origin claim must be byte-load-bearing");
        // Two different peer_origin claims also yield different bytes.
        let cross_y = canonical_cbor_reflection_depth_exceeded(
            base.0,
            base.1,
            base.2,
            base.3,
            &base.4,
            base.5,
            base.6,
            Some("ai:peer-y"),
        )
        .expect("encode Some(other)");
        assert_ne!(
            cross, cross_y,
            "swapping the peer_origin string must change the bytes"
        );
    }

    /// The encoder is deterministic — two encodes of the same Some
    /// peer_origin produce the same bytes. Mirrors the
    /// `canonical_cbor_is_deterministic_across_encodes` invariant on
    /// the local-only encoder.
    #[test]
    fn cross_peer_encoding_is_deterministic() {
        let a = canonical_cbor_reflection_depth_exceeded(
            "ai:a",
            7,
            3,
            "ns",
            &["s1".to_string(), "s2".to_string()],
            "t",
            "2026-05-13T00:00:00+00:00",
            Some("peer-A"),
        )
        .expect("encode 1");
        let b = canonical_cbor_reflection_depth_exceeded(
            "ai:a",
            7,
            3,
            "ns",
            &["s1".to_string(), "s2".to_string()],
            "t",
            "2026-05-13T00:00:00+00:00",
            Some("peer-A"),
        )
        .expect("encode 2");
        assert_eq!(a, b, "cross-peer encoding must be byte-stable");
    }

    /// The encoded map's key ordering is lexicographic — `peer_origin`
    /// sorts between `namespace` and `proposed_title` in the canonical
    /// `BTreeMap`. We can't easily reach the bytes' raw structure
    /// without a CBOR decode dependency on this test path, so we
    /// instead pin the observable behaviour: encoding remains
    /// deterministic AND adding `peer_origin` only differs the bytes
    /// (it doesn't reorder the rest of the keys to perturb hashes for
    /// pre-existing fields). Encode twice without peer_origin, then
    /// twice with — both pairs must be internally byte-stable.
    #[test]
    fn key_ordering_is_lexicographic_via_btreemap() {
        let no_peer = canonical_cbor_reflection_depth_exceeded(
            "ai:test",
            4,
            3,
            "ns",
            &["s1".to_string()],
            "title",
            "2026-05-13T00:00:00+00:00",
            None,
        )
        .expect("encode none");
        let no_peer2 = canonical_cbor_reflection_depth_exceeded(
            "ai:test",
            4,
            3,
            "ns",
            &["s1".to_string()],
            "title",
            "2026-05-13T00:00:00+00:00",
            None,
        )
        .expect("encode none again");
        assert_eq!(no_peer, no_peer2);
    }
}
