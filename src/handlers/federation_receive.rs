// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;

use crate::db;
use crate::federation::peer_attestation::{
    self, AttestError, PEER_ID_HEADER, PeerAttestationConfig,
};
use crate::models::{Memory, MemoryLink};
use crate::validate;

use super::MAX_BULK_SIZE;
#[cfg(feature = "sal")]
use super::store_err_to_response;
use super::{AppState, StorageBackend};

/// v0.7.0 federation security — extract the peer's self-claimed
/// `x-peer-id` header. Lowercase form per HTTP/2 wire convention;
/// axum's `HeaderMap` lookup is case-insensitive so callers can send
/// the canonical `X-Peer-Id`.
fn extract_peer_id(headers: &HeaderMap) -> Option<&str> {
    headers.get(PEER_ID_HEADER).and_then(|v| v.to_str().ok())
}

/// v0.7.0 #238 — render a 403 envelope when the body-claimed
/// `sender_agent_id` does not attest to the wire-level `x-peer-id`
/// header. Surfaces both values so the operator can diff exactly
/// what the peer claimed against what the substrate expected.
fn attestation_refusal_response(err: &AttestError) -> Response {
    let (claimed, peer_header) = match err {
        AttestError::HeaderMissing => (String::new(), String::new()),
        AttestError::Mismatch {
            claimed,
            peer_header,
        } => (claimed.clone(), peer_header.clone()),
    };
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": err.tag(),
            "claimed": claimed,
            "peer_header": peer_header,
            "note": "set AI_MEMORY_FED_TRUST_BODY_AGENT_ID=1 to opt out (legacy peers); \
                     pre-v0.7.0 federation peers must be upgraded to send `x-peer-id`.",
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Phase 3 foundation (issue #224) — HTTP sync endpoints.
//
// These ship in v0.6.0 GA as SKELETONS running today's timestamp-aware merge
// (`db::insert_if_newer`). Field-level CRDT-lite merge rules, streaming,
// resume-on-interrupt, and per-peer auth tokens are v0.8.0 targets.
// ---------------------------------------------------------------------------

/// v0.7.0 S6-LOW2 — log a warning when the sender's claimed wall-clock
/// is more than this many seconds ahead of the receiver's. Threshold is
/// deliberately permissive: ~1 minute of skew is normal for hosts with
/// NTP drift after a sleep cycle. Anything beyond that is operator-
/// signal that the cluster's clocks need attention.
const CLOCK_SKEW_WARN_THRESHOLD_SECS: i64 = 60;

/// v0.7.0 S6-LOW2 — observability-only clock-skew check. Compares the
/// sender's reported wall-clock (or the highest entry in
/// `sender_clock.entries` when the wall-clock field is absent) against
/// the receiver's `chrono::Utc::now()`. When the delta exceeds
/// [`CLOCK_SKEW_WARN_THRESHOLD_SECS`] in either direction, emits a
/// `tracing::warn!` so operators can spot a misconfigured peer. NEVER
/// rejects the push — federation must be tolerant of clock drift; the
/// log is the entire enforcement surface.
fn check_sender_clock_skew(sender_agent_id: &str, body: &SyncPushBody) {
    let sender_ts_str: Option<&str> = body
        .sender_wall_clock
        .as_deref()
        .or_else(|| body.sender_clock.entries.values().max().map(String::as_str));
    let Some(ts_str) = sender_ts_str else {
        return; // No clock signal at all → nothing to compare.
    };
    let Ok(sender_at) = chrono::DateTime::parse_from_rfc3339(ts_str) else {
        tracing::debug!(
            sender = %sender_agent_id,
            sender_ts = %ts_str,
            "sync_push: sender clock not RFC3339; skipping skew check"
        );
        return;
    };
    let now = chrono::Utc::now();
    let skew_secs = sender_at
        .with_timezone(&chrono::Utc)
        .signed_duration_since(now)
        .num_seconds();
    if skew_secs.abs() > CLOCK_SKEW_WARN_THRESHOLD_SECS {
        tracing::warn!(
            target: "federation::clock_skew",
            sender = %sender_agent_id,
            skew_secs,
            sender_ts = %ts_str,
            receiver_ts = %now.to_rfc3339(),
            "sync_push: sender_clock skew exceeds {CLOCK_SKEW_WARN_THRESHOLD_SECS}s threshold \
             (observability-only; push accepted)",
        );
    }
}

/// v0.7.0 S6-M2 — per-agent quota gate for federation receive. Closes
/// the F7 gap (#639) where mTLS-authenticated peers could push past
/// the local `agent_quotas` storage caps that would have blocked an
/// equivalent HTTP `POST /memories` from the same identity.
///
/// `attribute_agent` is the identity the substrate will charge for the
/// row. Resolution precedence (mTLS-attested first; falls back to the
/// claim chain when no cert peeking is available):
///   1. `mem.metadata.agent_id` — the original author of the row
///      (NHI provenance preserved across federation). This is what
///      `quota_status` reports against, so charging this id makes the
///      receiver-side quota a true mirror of the originator's daily
///      budget. A misbehaving peer cannot substitute another agent's
///      id without crashing the upstream signature check (H3).
///   2. `sender_agent_id` — substrate identity of the peer that
///      delivered the row. Used when the row carries no
///      `metadata.agent_id` (legacy / unauthored federation push).
///
/// Returns `Ok(())` on a clean check + record (counters incremented),
/// `Err(QuotaError)` on a refusal. The caller renders the refusal as
/// `429 Too Many Requests` with an `X-Quota-Reset-At` header.
fn attribute_agent_for_quota(sender_agent_id: &str, mem: &Memory) -> String {
    mem.metadata
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| sender_agent_id.to_string())
}

/// v0.7.0 S6-M2 — compute the next UTC midnight in RFC3339, used as
/// the `X-Quota-Reset-At` header value when a federation receive is
/// refused for hitting `memories_per_day` or `links_per_day`. Storage
/// caps reset on midnight UTC via `quotas::reset_daily`. The header
/// matches the HTTP POST refusal surface so clients have one timer
/// to consult regardless of which entry point hit the cap.
fn next_utc_midnight() -> String {
    use chrono::{Duration, Timelike};
    let now = chrono::Utc::now();
    let next = now
        .with_hour(0)
        .and_then(|t| t.with_minute(0))
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .map(|midnight_today| midnight_today + Duration::days(1))
        .unwrap_or_else(|| now + Duration::days(1));
    next.to_rfc3339()
}

/// Request body for `POST /api/v1/sync/push`.
#[derive(Deserialize)]
pub struct SyncPushBody {
    /// Claimed `agent_id` of the peer pushing data. Recorded in
    /// `sync_state` for vector clock advancement.
    ///
    /// v0.7.0 #238 — this body field is now ATTESTED against the
    /// wire-level `x-peer-id` HTTP header before any substrate write
    /// fires. See `src/federation/peer_attestation.rs` for the
    /// decision matrix, env bypass, and operator runbook. Pre-v0.7.0
    /// federation clients that don't send `x-peer-id` are accepted
    /// only when the operator opts in via
    /// `AI_MEMORY_FED_TRUST_BODY_AGENT_ID=1`.
    pub sender_agent_id: String,
    /// Vector clock the sender had at push time. v0.7.0 S6-LOW2: now
    /// consulted for observability-only clock-skew detection — the
    /// receiver logs a `tracing::warn!` when the sender's latest
    /// claimed observation is >60s ahead of the receiver's wall clock.
    /// Full clock reconciliation (CRDT-lite merge) lands with Task 3a.1.
    #[serde(default)]
    pub sender_clock: crate::models::VectorClock,
    /// v0.7.0 S6-LOW2 — sender's wall-clock RFC3339 timestamp at push
    /// time. Optional: when absent, skew detection falls back to the
    /// highest timestamp in `sender_clock.entries`. Observability-only;
    /// never enforced.
    #[serde(default)]
    pub sender_wall_clock: Option<String>,
    /// Memories the sender is offering. Applied via the existing
    /// timestamp-aware merge (`insert_if_newer`).
    pub memories: Vec<Memory>,
    /// Memory IDs the sender has deleted and wants propagated. Applied
    /// via `db::delete`. v0.6.0.1: simple remove (no tombstone row); a
    /// concurrent newer `insert_if_newer` from another peer could revive
    /// the row — a Last-Writer-Wins quirk we live with until v0.7's
    /// CRDT-lite tombstone table lands. In the common 4-node mesh, the
    /// same delete reaches every peer well before any revival window.
    #[serde(default)]
    pub deletions: Vec<String>,
    /// v0.6.2 (S29): memory IDs the sender has explicitly archived and
    /// wants propagated. Applied via `db::archive_memory` — a soft move
    /// from `memories` to `archived_memories`. Missing-on-peer IDs no-op.
    /// Distinct from `deletions`, which is a hard DELETE.
    #[serde(default)]
    pub archives: Vec<String>,
    /// v0.6.2 (S29): memory IDs the sender has restored from archive and
    /// wants propagated. Applied via `db::restore_archived` — moves the
    /// row from `archived_memories` back into `memories`. The inverse of
    /// `archives`. Missing-on-peer IDs (no row in the peer's archive
    /// table, or a live row already exists) no-op so replays are safe.
    #[serde(default)]
    pub restores: Vec<String>,
    /// v0.6.2 (#325): memory links the sender wants propagated. Applied
    /// via `db::create_link` on each peer. Duplicates are a no-op thanks
    /// to the unique `(source_id, target_id, relation)` constraint on
    /// `memory_links`.
    #[serde(default)]
    pub links: Vec<MemoryLink>,
    /// v0.6.2 (S34): pending-action rows the sender wants propagated.
    /// Applied via `db::upsert_pending_action` — preserves the originator's
    /// id + status + approvals so the cluster agrees on pending state.
    /// Without this, `POST /api/v1/pending/{id}/approve` on a peer 404s
    /// because the row only exists on the originator.
    #[serde(default)]
    pub pendings: Vec<crate::models::PendingAction>,
    /// v0.6.2 (S34): pending-action decisions the sender wants propagated
    /// so approve/reject on any node lands consistently. Applied via
    /// `db::decide_pending_action` — already-decided rows no-op, replay-safe.
    #[serde(default)]
    pub pending_decisions: Vec<crate::models::PendingDecision>,
    /// v0.6.2 (S35): namespace-standard meta rows the sender wants
    /// propagated. Applied via `db::set_namespace_standard(conn, ns,
    /// standard_id, parent.as_deref())` so the peer's inheritance-chain
    /// walk uses the originator's explicit parent (not a locally
    /// auto-detected one).
    #[serde(default)]
    pub namespace_meta: Vec<crate::models::NamespaceMetaEntry>,
    /// v0.6.2 (S35 follow-up): namespaces whose standard the sender has
    /// *cleared* and wants propagated. Applied via `db::clear_namespace_standard`
    /// — missing-on-peer namespaces no-op so replays are safe. Without
    /// this, alice clearing a standard on node-1 left the row visible on
    /// node-2's peer, breaking cross-peer rule-lifecycle assertions.
    #[serde(default)]
    pub namespace_meta_clears: Vec<String>,
    /// Preview mode — classify and count, do not write.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Deserialize)]
pub struct SyncSinceQuery {
    /// Return memories with `updated_at > since`. Absent = full snapshot.
    pub since: Option<String>,
    /// Pagination cap. Defaults to 500.
    pub limit: Option<usize>,
    /// Caller's claimed `agent_id`; optional but recorded in `sync_state`
    /// so the caller can later push incremental updates.
    pub peer: Option<String>,
}

/// v0.7.0 Wave-3 Continuation 2 — postgres-backed federation push.
///
/// Dispatches each `Memory` row through `app.store.apply_remote_memory`
/// (idempotent insert-if-newer) and each link / deletion through the
/// matching trait method. Other subcollections (pendings, archives,
/// restores, namespace_meta, pending_decisions) are governance- /
/// archive-state-machine concerns whose write paths live on tables
/// not yet trait-covered; they surface as skipped with a structured
/// `unsupported_on_postgres` count in the response envelope so a
/// heterogeneous (sqlite ↔ postgres) federation degrades gracefully
/// without silent drops.
///
/// Heterogeneous federation contract: a sqlite peer's push of N
/// memories + M links + K deletions reaches steady-state on the
/// postgres receiver via the trait calls. Audit emission for every
/// accepted federation push fires through `audit::emit` regardless
/// of backend (Phase 9).
#[cfg(feature = "sal")]
#[allow(clippy::too_many_lines)]
async fn sync_push_via_store(app: AppState, _headers: HeaderMap, body: SyncPushBody) -> Response {
    if let Err(e) = validate::validate_agent_id(&body.sender_agent_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid sender_agent_id: {e}")})),
        )
            .into_response();
    }
    if body.memories.len() > MAX_BULK_SIZE
        || body.deletions.len() > MAX_BULK_SIZE
        || body.archives.len() > MAX_BULK_SIZE
        || body.restores.len() > MAX_BULK_SIZE
        || body.pendings.len() > MAX_BULK_SIZE
        || body.pending_decisions.len() > MAX_BULK_SIZE
        || body.namespace_meta.len() > MAX_BULK_SIZE
        || body.namespace_meta_clears.len() > MAX_BULK_SIZE
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("sync_push limited to {} entries per subcollection", MAX_BULK_SIZE)
            })),
        )
            .into_response();
    }

    let ctx = crate::store::CallerContext::for_agent(body.sender_agent_id.clone());
    let mut applied = 0usize;
    let mut noop = 0usize;
    let mut skipped = 0usize;
    let mut deleted = 0usize;
    let mut links_applied = 0usize;
    let mut latest_seen: Option<String> = None;
    let mut unsupported_on_postgres = 0usize;
    let mut quota_refused = 0usize;
    let mut first_quota_refusal: Option<crate::quotas::QuotaError> = None;

    // v0.7.0 S6-LOW2 — observability-only sender_clock skew detection
    // (parity with the sqlite path).
    check_sender_clock_skew(&body.sender_agent_id, &body);

    // ---- memories ----------------------------------------------------
    for mem in &body.memories {
        if let Err(e) = validate::validate_memory(mem) {
            tracing::warn!("sync_push: skipping memory {} ({}): {e}", mem.id, mem.title);
            skipped += 1;
            continue;
        }
        if latest_seen
            .as_deref()
            .is_none_or(|current| mem.updated_at.as_str() > current)
        {
            latest_seen = Some(mem.updated_at.clone());
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        // v0.7.0 S6-M2 — per-agent quota gate. `agent_quotas` lives on
        // the SQLite metadata DB even on postgres-backed daemons
        // (Wave-3 hasn't migrated the row to the SAL trait yet), so
        // the postgres path consults the same `app.db` connection the
        // sqlite path uses. F7 closure (#639) — federation receive
        // never bypasses the cap that the equivalent HTTP POST sees.
        let attribute_agent = attribute_agent_for_quota(&body.sender_agent_id, mem);
        let bytes_estimate =
            i64::try_from(mem.title.len() + mem.content.len() + mem.metadata.to_string().len())
                .unwrap_or(i64::MAX);
        {
            let conn = app.db.lock().await;
            match crate::quotas::check_and_record(
                &conn.0,
                &attribute_agent,
                crate::quotas::QuotaOp::Memory {
                    bytes: bytes_estimate,
                },
            ) {
                Ok(()) => {}
                Err(crate::quotas::QuotaCheckError::Quota(q)) => {
                    tracing::warn!(
                        target: "federation::quota",
                        peer = %body.sender_agent_id,
                        attribute_agent = %attribute_agent,
                        limit = q.limit.as_str(),
                        current = q.current,
                        max = q.max,
                        "sync_push (postgres): per-agent quota exceeded"
                    );
                    let _ = crate::signed_events::append_signed_event(
                        &conn.0,
                        &crate::signed_events::SignedEvent {
                            id: uuid::Uuid::new_v4().to_string(),
                            agent_id: attribute_agent.clone(),
                            event_type: "federation.quota_refused".to_string(),
                            payload_hash: crate::signed_events::payload_hash(
                                format!(
                                    "peer={} agent={} limit={} current={} max={}",
                                    body.sender_agent_id,
                                    attribute_agent,
                                    q.limit.as_str(),
                                    q.current,
                                    q.max,
                                )
                                .as_bytes(),
                            ),
                            signature: None,
                            attest_level: "unsigned".to_string(),
                            timestamp: chrono::Utc::now().to_rfc3339(),
                            ..crate::signed_events::SignedEvent::default()
                        },
                    );
                    quota_refused += 1;
                    if first_quota_refusal.is_none() {
                        first_quota_refusal = Some(q);
                    }
                    drop(conn);
                    break;
                }
                Err(crate::quotas::QuotaCheckError::Sql(e)) => {
                    tracing::warn!(
                        "sync_push (postgres): quota substrate read failed for {}: {e}",
                        attribute_agent
                    );
                    skipped += 1;
                    continue;
                }
            }
        }
        // v0.7.0 L2-2 — reflection origin stamping (postgres parity).
        // Use the compiled-default cap on postgres because
        // `resolve_governance_policy` is sqlite-only today; the stamp
        // still carries `peer_origin` + `original_depth` which is the
        // load-bearing provenance.
        let local_cap = crate::models::GovernancePolicy::default().effective_max_reflection_depth();
        let to_insert = crate::federation::reflection_bookkeeping::stamp_reflection_origin(
            mem,
            &body.sender_agent_id,
            local_cap,
        );
        match app.store.apply_remote_memory(&ctx, &to_insert).await {
            Ok(applied_id) => {
                applied += 1;
                // v0.7.0 Wave-3 Continuation 5 (S18+S79 federation
                // semantic recall) — re-embed the incoming memory on
                // the receiver so the postgres `embedding` column
                // lands populated. Federation wire shape doesn't
                // carry the vector; without this step semantic recall
                // queries against a peer that received the memory
                // through sync_push would surface empty.
                if let Some(emb) = app.embedder.as_ref().as_ref() {
                    let embedding_text = format!("{} {}", mem.title, mem.content);
                    if let Ok(vector) = emb.embed(&embedding_text) {
                        let _ = app
                            .store
                            .update_embedding(&ctx, &applied_id, Some(&vector))
                            .await;
                    }
                }
                // F2 audit-chain emit: every accepted federation push
                // chains through the same audit log as a local Store.
                // Phase-9 wiring — file-based audit module is backend-
                // blind so this works for postgres-backed daemons.
                if crate::audit::is_enabled() {
                    let owner = mem
                        .metadata
                        .get("agent_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&body.sender_agent_id);
                    crate::audit::emit(
                        crate::audit::EventBuilder::new(
                            crate::audit::AuditAction::Store,
                            crate::audit::actor(owner, "federation_push", None),
                            crate::audit::target_memory(
                                mem.id.clone(),
                                mem.namespace.clone(),
                                Some(mem.title.clone()),
                                Some(mem.tier.as_str().to_string()),
                                None,
                            ),
                        )
                        .outcome(crate::audit::AuditOutcome::Allow),
                    );
                }
            }
            Err(e) => {
                // Refund the quota we charged so a downstream write
                // failure doesn't leak counters (saturating; safe).
                {
                    let conn = app.db.lock().await;
                    let _ = crate::quotas::refund_op(
                        &conn.0,
                        &attribute_agent,
                        crate::quotas::QuotaOp::Memory {
                            bytes: bytes_estimate,
                        },
                    );
                }
                tracing::warn!("sync_push: apply_remote_memory failed for {}: {e}", mem.id);
                skipped += 1;
            }
        }
    }

    // v0.7.0 S6-M2 — quota refusal short-circuit (postgres path).
    if let Some(q) = first_quota_refusal.take() {
        let reset_at = next_utc_midnight();
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                ("x-quota-reset-at", reset_at.as_str()),
                ("x-quota-limit", q.limit.as_str()),
            ],
            Json(json!({
                "error": "QUOTA_EXCEEDED",
                "limit": q.limit.as_str(),
                "current": q.current,
                "max": q.max,
                "agent_id": q.agent_id,
                "applied_before_refusal": applied,
                "quota_refused": quota_refused,
                "reset_at": reset_at,
                "storage_backend": "postgres",
            })),
        )
            .into_response();
    }

    // ---- deletions ---------------------------------------------------
    for del_id in &body.deletions {
        if validate::validate_id(del_id).is_err() {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match app.store.apply_remote_deletion(&ctx, del_id).await {
            Ok(true) => deleted += 1,
            Ok(false) => noop += 1,
            Err(e) => {
                tracing::warn!("sync_push: apply_remote_deletion failed for {del_id}: {e}");
                skipped += 1;
            }
        }
    }

    // ---- links -------------------------------------------------------
    //
    // H3 verify path: when a link arrives with a signature + observed_by,
    // verify against the locally enrolled public key. Tampered = skip.
    // Unknown observed_by = accept-and-flag as unsigned. Successful =
    // peer_attested. Mirrors the sqlite-backed handler's H3 contract.
    for link in &body.links {
        if validate::validate_link(&link.source_id, &link.target_id, link.relation.as_str())
            .is_err()
        {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        let attest_level = match (link.signature.as_deref(), link.observed_by.as_deref()) {
            (Some(sig_bytes), Some(observed_by)) => {
                match crate::identity::verify::lookup_peer_public_key(observed_by) {
                    Some(pubkey) => {
                        let signable = crate::identity::sign::SignableLink {
                            src_id: &link.source_id,
                            dst_id: &link.target_id,
                            relation: link.relation.as_str(),
                            observed_by: Some(observed_by),
                            valid_from: link.valid_from.as_deref(),
                            valid_until: link.valid_until.as_deref(),
                        };
                        match crate::identity::verify::verify(&pubkey, &signable, sig_bytes) {
                            Ok(()) => "peer_attested",
                            Err(e) => {
                                tracing::warn!(
                                    "sync_push: signature rejected for link \
                                     ({} -> {} / {}) from observed_by={}: {e}",
                                    link.source_id,
                                    link.target_id,
                                    link.relation,
                                    observed_by
                                );
                                skipped += 1;
                                continue;
                            }
                        }
                    }
                    None => "unsigned",
                }
            }
            _ => "unsigned",
        };
        match app.store.apply_remote_link(&ctx, link, attest_level).await {
            Ok(()) => links_applied += 1,
            Err(e) => {
                tracing::warn!(
                    "sync_push: apply_remote_link failed ({} -> {} / {}): {e}",
                    link.source_id,
                    link.target_id,
                    link.relation
                );
                skipped += 1;
            }
        }
    }

    // ---- archives / restores / pendings / pending_decisions /
    //      namespace_meta / namespace_meta_clears -----------------------
    //
    // These subcollections write into tables (archived_memories,
    // pending_actions, namespace_meta) not yet trait-covered. Surface
    // them with the same noop posture sqlite uses on missing rows so
    // a heterogeneous federation reports an honest count.
    unsupported_on_postgres += body.archives.len()
        + body.restores.len()
        + body.pendings.len()
        + body.pending_decisions.len()
        + body.namespace_meta.len()
        + body.namespace_meta_clears.len();

    (
        StatusCode::OK,
        Json(json!({
            "applied": applied,
            "deleted": deleted,
            "links_applied": links_applied,
            "noop": noop,
            "skipped": skipped,
            "quota_refused": quota_refused,
            "unsupported_on_postgres": unsupported_on_postgres,
            "dry_run": body.dry_run,
            "receiver_agent_id": body.sender_agent_id,
            "storage_backend": "postgres",
            "note": "pendings / archives / restores / namespace_meta are sqlite-only \
                     in v0.7.0; memories / deletions / links round-trip via the SAL trait",
        })),
    )
        .into_response()
}

#[allow(clippy::too_many_lines)]
pub async fn sync_push(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SyncPushBody>,
) -> impl IntoResponse {
    let state = app.db.clone();

    // v0.7.0 #238 — body-claimed sender_agent_id MUST attest against
    // the wire-level `x-peer-id` header (or be the unauthored-push
    // legacy shape). Backwards-compat via
    // `AI_MEMORY_FED_TRUST_BODY_AGENT_ID=1`. Runs BEFORE the
    // postgres-dispatch branch so both backends share the same
    // refusal posture. See `src/federation/peer_attestation.rs`.
    let peer_header_owned = extract_peer_id(&headers).map(str::to_string);
    let attest_cfg = PeerAttestationConfig::from_env();
    if !peer_attestation::trust_body_agent_id_bypass() {
        if let Err(e) = peer_attestation::attest_sender(
            peer_header_owned.as_deref(),
            Some(body.sender_agent_id.as_str()),
            &attest_cfg,
        ) {
            tracing::warn!(
                target: "federation::attestation",
                tag = e.tag(),
                claimed = %body.sender_agent_id,
                peer_header = %peer_header_owned.as_deref().unwrap_or(""),
                "sync_push: sender_agent_id failed attestation against x-peer-id header"
            );
            return attestation_refusal_response(&e);
        }
    } else {
        // Bypass set — log once per request at WARN so the operator
        // can see the legacy posture is in effect.
        tracing::warn!(
            target: "federation::attestation",
            "sync_push: AI_MEMORY_FED_TRUST_BODY_AGENT_ID=1 — bypassing #238 \
             sender_agent_id attestation (legacy compat)"
        );
    }

    // v0.7.0 Wave-3 Continuation 2 — postgres-backed federation
    // dispatches through the SAL trait for memories / deletions /
    // links. Pendings / archives / restores / namespace_meta /
    // pending_decisions remain sqlite-only (governance write paths
    // and archive-state-machine state sit on tables not yet covered
    // by the trait surface — those subcollections, when present in a
    // push from a sqlite peer, surface in `skipped` with a structured
    // note in the response envelope).
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        return sync_push_via_store(app, headers, body).await;
    }

    if let Err(e) = validate::validate_agent_id(&body.sender_agent_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid sender_agent_id: {e}")})),
        )
            .into_response();
    }
    // Cap memories per push, matching the bulk-create limit. Without
    // this a malicious peer with a valid mTLS cert could flood the
    // receiver and bottleneck the shared SQLite Mutex (red-team #242).
    if body.memories.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("sync_push limited to {} memories per request", MAX_BULK_SIZE)
            })),
        )
            .into_response();
    }
    if body.deletions.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("sync_push limited to {} deletions per request", MAX_BULK_SIZE)
            })),
        )
            .into_response();
    }
    if body.archives.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("sync_push limited to {} archives per request", MAX_BULK_SIZE)
            })),
        )
            .into_response();
    }
    if body.restores.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("sync_push limited to {} restores per request", MAX_BULK_SIZE)
            })),
        )
            .into_response();
    }
    if body.pendings.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("sync_push limited to {} pendings per request", MAX_BULK_SIZE)
            })),
        )
            .into_response();
    }
    if body.pending_decisions.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "sync_push limited to {} pending_decisions per request",
                    MAX_BULK_SIZE
                )
            })),
        )
            .into_response();
    }
    if body.namespace_meta.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "sync_push limited to {} namespace_meta per request",
                    MAX_BULK_SIZE
                )
            })),
        )
            .into_response();
    }
    if body.namespace_meta_clears.len() > MAX_BULK_SIZE {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!(
                    "sync_push limited to {} namespace_meta_clears per request",
                    MAX_BULK_SIZE
                )
            })),
        )
            .into_response();
    }
    // Receiver's local identity — default to the caller-supplied header,
    // fall back to the anonymous placeholder. Recorded in sync_state rows.
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let local_agent_id = match crate::identity::resolve_http_agent_id(None, header_agent_id) {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid x-agent-id: {e}")})),
            )
                .into_response();
        }
    };

    // v0.7.0 S6-LOW2 — observability-only sender_clock skew detection.
    // Logs a warn when the sender's clock claim is >60s out from ours;
    // does not gate the push. Federation must be tolerant of drift.
    check_sender_clock_skew(&body.sender_agent_id, &body);

    let lock = state.lock().await;
    let mut applied = 0usize;
    let mut noop = 0usize;
    let mut skipped = 0usize;
    let mut deleted = 0usize;
    let mut archived = 0usize;
    let mut restored = 0usize;
    let mut latest_seen: Option<String> = None;
    // v0.7.0 S6-M2 — federation quota refusals. Counted alongside
    // `skipped` so the existing response envelope shape doesn't change,
    // and surfaced as a distinct field so an operator can tell the
    // difference between "peer pushed garbage" and "peer overran its
    // daily cap". The first quota refusal also short-circuits the
    // whole memory loop with a 429 response (matches the HTTP POST
    // store refusal: callers MUST back off, not just skip the offender).
    let mut quota_refused = 0usize;
    let mut first_quota_refusal: Option<crate::quotas::QuotaError> = None;

    // v0.6.0.1 (#322): peers that apply a synced memory must also refresh
    // their embedding + HNSW index so downstream semantic recall surfaces
    // the row. Without this, scenario-18 observed a2a-hermes r14 black-hole
    // pattern: substrate CRUD fanout works, but semantic recall on peers
    // silently misses propagated writes.
    //
    // Collect rows that need an embedding refresh and apply AFTER we drop
    // the DB lock (embedder is CPU-heavy; holding the Mutex across that
    // would serialize unrelated writers for hundreds of ms).
    let mut embedding_refresh: Vec<(String, String)> = Vec::new();
    for mem in &body.memories {
        if let Err(e) = validate::validate_memory(mem) {
            tracing::warn!("sync_push: skipping memory {} ({}): {e}", mem.id, mem.title);
            skipped += 1;
            continue;
        }
        if latest_seen
            .as_deref()
            .is_none_or(|current| mem.updated_at.as_str() > current)
        {
            latest_seen = Some(mem.updated_at.clone());
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        // v0.7.0 S6-M2 — per-agent quota gate. F7 (#639) closed this
        // on the HTTP POST store path but federation receive was a
        // back-door bypass: an mTLS peer could push N memories per
        // second past the local `agent_quotas.max_memories_per_day`
        // ceiling because `insert_if_newer` is the substrate-level
        // upsert and doesn't consult quotas. Charge each accepted
        // memory against the original author's quota row so the cap
        // is a true cluster-wide budget. On refusal: emit a signed
        // refusal event (for the cryptographic audit chain) and
        // short-circuit the loop with `quota_refused`; the outer
        // handler renders 429 + X-Quota-Reset-At so callers back off.
        let attribute_agent = attribute_agent_for_quota(&body.sender_agent_id, mem);
        let bytes_estimate =
            i64::try_from(mem.title.len() + mem.content.len() + mem.metadata.to_string().len())
                .unwrap_or(i64::MAX);
        match crate::quotas::check_and_record(
            &lock.0,
            &attribute_agent,
            crate::quotas::QuotaOp::Memory {
                bytes: bytes_estimate,
            },
        ) {
            Ok(()) => {}
            Err(crate::quotas::QuotaCheckError::Quota(q)) => {
                tracing::warn!(
                    target: "federation::quota",
                    peer = %body.sender_agent_id,
                    attribute_agent = %attribute_agent,
                    limit = q.limit.as_str(),
                    current = q.current,
                    max = q.max,
                    "sync_push: per-agent quota exceeded; refusing federation push"
                );
                // Emit a signed audit event so the refusal lands in the
                // tamper-evident chain alongside the F7-equivalent HTTP
                // POST refusal. Best-effort: audit-write failure is
                // logged but does not change the refusal control flow.
                let _ = crate::signed_events::append_signed_event(
                    &lock.0,
                    &crate::signed_events::SignedEvent {
                        id: uuid::Uuid::new_v4().to_string(),
                        agent_id: attribute_agent.clone(),
                        event_type: "federation.quota_refused".to_string(),
                        payload_hash: crate::signed_events::payload_hash(
                            format!(
                                "peer={} agent={} limit={} current={} max={}",
                                body.sender_agent_id,
                                attribute_agent,
                                q.limit.as_str(),
                                q.current,
                                q.max,
                            )
                            .as_bytes(),
                        ),
                        signature: None,
                        attest_level: "unsigned".to_string(),
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        ..crate::signed_events::SignedEvent::default()
                    },
                );
                quota_refused += 1;
                if first_quota_refusal.is_none() {
                    first_quota_refusal = Some(q);
                }
                // Short-circuit: any further memories in this push
                // would only deepen the cap breach. The remainder of
                // the loop posture (skipping the rest) matches the
                // HTTP POST bulk-create refusal — first cap hit
                // returns 429 with the remaining batch unprocessed.
                break;
            }
            Err(crate::quotas::QuotaCheckError::Sql(e)) => {
                tracing::warn!(
                    "sync_push: quota substrate read failed for {}: {e}",
                    attribute_agent
                );
                skipped += 1;
                continue;
            }
        }
        // v0.7.0 L2-2 (S6-M1) — stamp `metadata.reflection_origin` on
        // inbound reflection rows before the insert. The stamped copy
        // carries `peer_origin`, `original_depth`, and the receiver's
        // local cap at arrival time; the substrate row preserves the
        // original `reflection_depth` so derived-write cap enforcement
        // (storage::reflect) sees the same value the source peer saw.
        // Non-reflection rows (depth == 0) pass through unchanged.
        let cap_for_namespace = crate::storage::resolve_governance_policy(&lock.0, &mem.namespace)
            .unwrap_or_else(crate::models::GovernancePolicy::default)
            .effective_max_reflection_depth();
        let to_insert = crate::federation::reflection_bookkeeping::stamp_reflection_origin(
            mem,
            &body.sender_agent_id,
            cap_for_namespace,
        );
        match db::insert_if_newer(&lock.0, &to_insert) {
            Ok(actual_id) => {
                applied += 1;
                embedding_refresh.push((actual_id, format!("{} {}", mem.title, mem.content)));
            }
            Err(e) => {
                // Best-effort refund so a downstream insert failure
                // doesn't leak quota counters. `refund_op` saturates at
                // zero so a buggy double-refund cannot poison the row.
                let _ = crate::quotas::refund_op(
                    &lock.0,
                    &attribute_agent,
                    crate::quotas::QuotaOp::Memory {
                        bytes: bytes_estimate,
                    },
                );
                tracing::warn!("sync_push: insert_if_newer failed for {}: {e}", mem.id);
                skipped += 1;
            }
        }
    }

    // v0.7.0 S6-M2 — quota refusal short-circuit. The first refusal in
    // the loop produces a 429 with X-Quota-Reset-At so callers back off
    // (matches the HTTP POST store refusal envelope from F7 / #639).
    if let Some(q) = first_quota_refusal.take() {
        drop(lock);
        let reset_at = next_utc_midnight();
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [
                ("x-quota-reset-at", reset_at.as_str()),
                ("x-quota-limit", q.limit.as_str()),
            ],
            Json(json!({
                "error": "QUOTA_EXCEEDED",
                "limit": q.limit.as_str(),
                "current": q.current,
                "max": q.max,
                "agent_id": q.agent_id,
                "applied_before_refusal": applied,
                "quota_refused": quota_refused,
                "reset_at": reset_at,
            })),
        )
            .into_response();
    }

    // Process deletions (v0.6.0.1 — scenario 10 fanout). Invalid ids are
    // skipped silently; missing rows count as no-op. Peers that have
    // already GC'd the row see identical post-state.
    for del_id in &body.deletions {
        if validate::validate_id(del_id).is_err() {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::delete(&lock.0, del_id) {
            Ok(true) => deleted += 1,
            Ok(false) => noop += 1,
            Err(e) => {
                tracing::warn!("sync_push: delete failed for {del_id}: {e}");
                skipped += 1;
            }
        }
    }

    // v0.6.2 (S29): process explicit archives. Soft-move from `memories`
    // to `archived_memories` — distinct from deletions which hard-delete.
    // Missing rows count as no-op (peer may have already archived or
    // never received the original write).
    for arch_id in &body.archives {
        if validate::validate_id(arch_id).is_err() {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::archive_memory(&lock.0, arch_id, Some("sync_push")) {
            Ok(true) => archived += 1,
            Ok(false) => noop += 1,
            Err(e) => {
                tracing::warn!("sync_push: archive_memory failed for {arch_id}: {e}");
                skipped += 1;
            }
        }
    }

    // v0.6.2 (S29): process explicit restores — the inverse of archives.
    // Move the row from `archived_memories` back into `memories`.
    // No-op posture matches archives: missing rows (peer hasn't received
    // the archive, or the row is already live) count as noop so replays
    // and out-of-order restore/archive pairs don't error.
    for res_id in &body.restores {
        if validate::validate_id(res_id).is_err() {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::restore_archived(&lock.0, res_id) {
            Ok(true) => restored += 1,
            Ok(false) => noop += 1,
            Err(e) => {
                tracing::warn!("sync_push: restore_archived failed for {res_id}: {e}");
                skipped += 1;
            }
        }
    }

    // v0.6.2 (#325): process incoming links. Duplicates are expected on
    // retry / re-sync and collapse to a no-op via the unique index on
    // (source_id, target_id, relation). Invalid ids are skipped silently
    // — same posture as deletions.
    //
    // v0.7 H3: when a link arrives with a signature + observed_by claim,
    // verify it against the public key associated with that claim before
    // landing the row. Tampered signatures → reject with a warn log.
    // Unknown observed_by (no enrolled key on this host) → accept-and-
    // flag as `unsigned` so federation back-compat holds for peers that
    // haven't enrolled yet. Successful verify → land with attest_level
    // `peer_attested`.
    let mut links_applied = 0usize;
    for link in &body.links {
        if validate::validate_link(&link.source_id, &link.target_id, link.relation.as_str())
            .is_err()
        {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }

        // Decide attest_level via the H3 verify path before insert.
        let attest_level = match (link.signature.as_deref(), link.observed_by.as_deref()) {
            (Some(sig_bytes), Some(observed_by)) => {
                match crate::identity::verify::lookup_peer_public_key(observed_by) {
                    Some(pubkey) => {
                        let signable = crate::identity::sign::SignableLink {
                            src_id: &link.source_id,
                            dst_id: &link.target_id,
                            relation: link.relation.as_str(),
                            observed_by: Some(observed_by),
                            valid_from: link.valid_from.as_deref(),
                            valid_until: link.valid_until.as_deref(),
                        };
                        match crate::identity::verify::verify(&pubkey, &signable, sig_bytes) {
                            Ok(()) => "peer_attested",
                            Err(e) => {
                                // Tampered / malformed-sig: refuse to land
                                // the row. The receiver-side warn log is
                                // the operator's signal that a peer is
                                // misbehaving (or that a key rotation
                                // got out of sync).
                                tracing::warn!(
                                    "sync_push: signature rejected for link \
                                     ({} -> {} / {}) from observed_by={}: {e}",
                                    link.source_id,
                                    link.target_id,
                                    link.relation,
                                    observed_by
                                );
                                skipped += 1;
                                continue;
                            }
                        }
                    }
                    None => {
                        // No public key enrolled for this peer →
                        // accept-and-flag as unsigned. Operators can
                        // later enroll the key (`identity import`) and
                        // re-sync to upgrade the row's attest_level on
                        // a subsequent re-send.
                        "unsigned"
                    }
                }
            }
            // No signature on the wire (legacy v0.6.x peer) or no
            // observed_by claim → treat as unsigned. Same posture as
            // pre-H3 federation.
            _ => "unsigned",
        };

        match db::create_link_inbound(&lock.0, link, attest_level) {
            Ok(()) => links_applied += 1,
            Err(e) => {
                tracing::warn!(
                    "sync_push: create_link_inbound failed ({} -> {} / {}): {e}",
                    link.source_id,
                    link.target_id,
                    link.relation
                );
                skipped += 1;
            }
        }
    }

    // v0.6.2 (S34): process incoming pending-action rows. Uses
    // `upsert_pending_action` so replays / races converge on the
    // originator's canonical row. Invalid ids skipped silently.
    let mut pendings_applied = 0usize;
    for pa in &body.pendings {
        if validate::validate_id(&pa.id).is_err() {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::upsert_pending_action(&lock.0, pa) {
            Ok(()) => {
                pendings_applied += 1;
                // v0.7.0 K4 — peer-originated pending rows fire the
                // `approval_requested` event on this peer too so local
                // approval-API subscribers get a uniform view of the
                // queue regardless of which node minted the row.
                // `upsert_*` is idempotent (`ON CONFLICT(id) DO UPDATE`)
                // — replays of the same row currently re-fire the
                // event; that's the documented K4 behaviour and matches
                // the existing `pending_action_expired` semantics. K7
                // (subscription reliability) layers DLQ + dedup on top.
                if pa.status == "pending" {
                    crate::subscriptions::dispatch_approval_requested(&lock.0, &pa.id, &lock.1);
                }
            }
            Err(e) => {
                tracing::warn!("sync_push: upsert_pending_action failed for {}: {e}", pa.id);
                skipped += 1;
            }
        }
    }

    // v0.6.2 (S34): process incoming pending-action decisions. No-op on
    // already-decided rows; that's the steady-state when the originator
    // and this peer both saw the decision. Rejected decisions still
    // transition status so retries on either side see `status != 'pending'`.
    let mut pending_decisions_applied = 0usize;
    for dec in &body.pending_decisions {
        if validate::validate_id(&dec.id).is_err() {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::decide_pending_action(&lock.0, &dec.id, dec.approved, &dec.decider) {
            Ok(true) => {
                pending_decisions_applied += 1;
                // On approve, replay the pending payload so the target
                // write (store/delete/promote) actually lands on this
                // peer — matches the originator's post-approve state.
                if dec.approved {
                    match db::execute_pending_action(&lock.0, &dec.id) {
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                "sync_push: execute_pending_action failed for {}: {e}",
                                dec.id
                            );
                        }
                    }
                }
            }
            Ok(false) => noop += 1, // already decided — converged state
            Err(e) => {
                tracing::warn!(
                    "sync_push: decide_pending_action failed for {}: {e}",
                    dec.id
                );
                skipped += 1;
            }
        }
    }

    // v0.6.2 (S35): process incoming namespace_meta rows. Applies via
    // `set_namespace_standard` so the peer's inheritance-chain walk has
    // the originator's explicit parent link. The standard memory itself
    // rides on the same push via `memories` (or arrived earlier through
    // `broadcast_store_quorum`); the namespace-meta row closes the gap.
    let mut namespace_meta_applied = 0usize;
    for entry in &body.namespace_meta {
        if validate::validate_namespace(&entry.namespace).is_err()
            || validate::validate_id(&entry.standard_id).is_err()
        {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::set_namespace_standard(
            &lock.0,
            &entry.namespace,
            &entry.standard_id,
            entry.parent_namespace.as_deref(),
        ) {
            Ok(()) => namespace_meta_applied += 1,
            Err(e) => {
                tracing::warn!(
                    "sync_push: set_namespace_standard failed for {}: {e}",
                    entry.namespace
                );
                skipped += 1;
            }
        }
    }

    // v0.6.2 (S35 follow-up): process incoming namespace_meta_clears. Applies
    // via `db::clear_namespace_standard` so the peer drops its meta row and
    // subsequent `get_standard` returns empty. Missing-on-peer namespaces
    // no-op (`changed == 0`) — replays are safe.
    let mut namespace_meta_cleared = 0usize;
    for ns in &body.namespace_meta_clears {
        if validate::validate_namespace(ns).is_err() {
            skipped += 1;
            continue;
        }
        if body.dry_run {
            noop += 1;
            continue;
        }
        match db::clear_namespace_standard(&lock.0, ns) {
            Ok(true) => namespace_meta_cleared += 1,
            Ok(false) => noop += 1,
            Err(e) => {
                tracing::warn!("sync_push: clear_namespace_standard failed for {ns}: {e}");
                skipped += 1;
            }
        }
    }

    // Advance the vector clock with the highest `updated_at` we observed.
    // Skipped in dry-run mode since the caller is only previewing.
    if !body.dry_run
        && let Some(at) = latest_seen.as_deref()
        && let Err(e) = db::sync_state_observe(&lock.0, &local_agent_id, &body.sender_agent_id, at)
    {
        tracing::warn!("sync_push: sync_state_observe failed: {e}");
    }

    // v0.6.0.1 (#322): regenerate embeddings for applied rows so peer-side
    // semantic recall surfaces the propagated memories. Without this,
    // scenario-18 observed the a2a-hermes r14 black-hole pattern:
    // substrate CRUD fanout works, but semantic recall on peers misses.
    //
    // Embedding + set_embedding are serialized under the existing DB lock;
    // HNSW updates happen after we release the lock to avoid contention.
    let mut hnsw_updates: Vec<(String, Vec<f32>)> = Vec::new();
    if !body.dry_run
        && !embedding_refresh.is_empty()
        && let Some(emb) = app.embedder.as_ref().as_ref()
    {
        for (id, text) in &embedding_refresh {
            match emb.embed(text) {
                Ok(vec) => {
                    if let Err(e) = db::set_embedding(&lock.0, id, &vec) {
                        tracing::warn!("sync_push: set_embedding failed for {id}: {e}");
                        continue;
                    }
                    hnsw_updates.push((id.clone(), vec));
                }
                Err(e) => {
                    tracing::warn!("sync_push: embed failed for {id}: {e}");
                }
            }
        }
    }

    // Receiver's current clock, returned so the sender can learn which
    // peers the receiver has seen. Phase 3 Task 3a.1 will use this to
    // short-circuit redundant pushes.
    let receiver_clock = db::sync_state_load(&lock.0, &local_agent_id)
        .unwrap_or_else(|_| crate::models::VectorClock::default());

    // Release DB lock before touching the HNSW index — the vector index
    // has its own mutex and holding both serializes unrelated writers.
    drop(lock);
    if !hnsw_updates.is_empty() {
        let mut idx_lock = app.vector_index.lock().await;
        if let Some(idx) = idx_lock.as_mut() {
            for (id, vec) in hnsw_updates {
                idx.remove(&id);
                idx.insert(id, vec);
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "applied": applied,
            "deleted": deleted,
            "archived": archived,
            "restored": restored,
            "links_applied": links_applied,
            "pendings_applied": pendings_applied,
            "pending_decisions_applied": pending_decisions_applied,
            "namespace_meta_applied": namespace_meta_applied,
            "namespace_meta_cleared": namespace_meta_cleared,
            "noop": noop,
            "skipped": skipped,
            "quota_refused": quota_refused,
            "dry_run": body.dry_run,
            "receiver_agent_id": local_agent_id,
            "receiver_clock": receiver_clock,
        })),
    )
        .into_response()
}

pub async fn sync_since(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SyncSinceQuery>,
) -> impl IntoResponse {
    let state = app.db.clone();
    // Validate `since` parses as RFC 3339 BEFORE hitting the DB so a
    // garbage timestamp returns a clear 400 instead of a 200 with the
    // entire database (red-team #247).
    if let Some(ref s) = q.since
        && !s.is_empty()
        && chrono::DateTime::parse_from_rfc3339(s).is_err()
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid `since` parameter — expected RFC 3339 timestamp"
            })),
        )
            .into_response();
    }
    let limit = q.limit.unwrap_or(500).min(10_000);

    // v0.7.0 Wave-3 Continuation 2 — dispatch through the SAL trait
    // when postgres-backed. Heterogeneous federation (sqlite ↔ postgres)
    // rides on this single code path so the wire shape is byte-blind
    // to the underlying store.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let mems = match app
            .store
            .list_memories_updated_since(q.since.as_deref(), limit)
            .await
        {
            Ok(v) => v,
            Err(e) => return store_err_to_response(e),
        };
        let earliest_updated_at = mems.first().map(|m| m.updated_at.clone());
        let latest_updated_at = mems.last().map(|m| m.updated_at.clone());
        return (
            StatusCode::OK,
            Json(json!({
                "count": mems.len(),
                "limit": limit,
                "updated_since": q.since,
                "earliest_updated_at": earliest_updated_at,
                "latest_updated_at": latest_updated_at,
                "memories": mems,
                "storage_backend": "postgres",
            })),
        )
            .into_response();
    }

    let lock = state.lock().await;
    let mems = match db::memories_updated_since(&lock.0, q.since.as_deref(), limit) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("sync_since: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };

    // Record the puller as a peer so subsequent incremental push/pull
    // pairs have a durable clock entry. Best-effort; don't fail the
    // response if the side-effect write fails.
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    if let (Some(peer), Ok(local_agent_id)) = (
        q.peer.as_deref(),
        crate::identity::resolve_http_agent_id(None, header_agent_id),
    ) && validate::validate_agent_id(peer).is_ok()
        && let Some(last) = mems.last()
        && let Err(e) = db::sync_state_observe(&lock.0, &local_agent_id, peer, &last.updated_at)
    {
        tracing::debug!("sync_since: sync_state_observe failed: {e}");
    }

    // S39 diagnostic echo (v0.6.2). The testbook scenario writes 6 rows
    // while peer-3 is suspended then queries `/sync/since?since=<ckpt>`
    // and expects the 6 back. When the count comes back 0, the scenario
    // can't tell whether:
    //   a) the server parsed `since` differently than expected,
    //   b) `limit` silently truncated, or
    //   c) the returned timestamps don't actually cover the expected range.
    // Echoing `updated_since` (what the server parsed, verbatim) plus
    // earliest / latest `updated_at` from the result set lets the
    // scenario pin the failure mode without changing any behavior. Fields
    // are additive — no existing caller assertion regresses.
    let earliest_updated_at = mems.first().map(|m| m.updated_at.clone());
    let latest_updated_at = mems.last().map(|m| m.updated_at.clone());

    (
        StatusCode::OK,
        Json(json!({
            "count": mems.len(),
            "limit": limit,
            "updated_since": q.since,
            "earliest_updated_at": earliest_updated_at,
            "latest_updated_at": latest_updated_at,
            "memories": mems,
        })),
    )
        .into_response()
}
