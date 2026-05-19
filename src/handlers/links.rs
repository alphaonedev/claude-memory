// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! HTTP handlers for the v0.7.0 link surface (#650 follow-up
//! per-domain split). Each handler is a thin Axum-layer wrapper that
//! validates the request shape, dispatches through the SAL trait
//! (postgres path) or the legacy `db::*` API (sqlite path), and
//! shapes the result into the canonical wire envelope.
//!
//! All handlers were extracted verbatim from `src/handlers/http.rs`
//! (commit `12e1253`, lines 4260-4824); wire compatibility is
//! preserved via the `pub use links::*` re-export from
//! `src/handlers/mod.rs`. The split keeps the link-CRUD +
//! `links/verify` domain in a single ~570-line module while
//! shrinking the legacy `handlers/http.rs` toward the long-term
//! ≤600-LOC target.
//!
//! Functions in this module:
//!   - `verify_link_handler`    (POST   /api/v1/links/verify)
//!   - `create_link`            (POST   /api/v1/links)
//!   - `delete_link`            (DELETE /api/v1/links)
//!   - `get_links`              (GET    /api/v1/links/{id})

#![allow(clippy::too_many_lines)]

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
#[cfg(feature = "sal")]
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use crate::db;
use crate::models::LinkBody;
#[cfg(feature = "sal")]
use crate::models::MemoryLink;
use crate::validate;

use super::AppState;
#[cfg(feature = "sal")]
use super::StorageBackend;
#[cfg(feature = "sal")]
use super::store_err_to_response;

/// JSON body for `POST /api/v1/links/verify`.
///
/// Either `source_id` (with optional `target_id`) OR `link_id` MUST be
/// supplied. `link_id` on every adapter is the canonical
/// `source_id|target_id|relation` triple — the trait does not expose a
/// rowid surface for links.
#[derive(Debug, Deserialize)]
pub struct VerifyLinkBody {
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub target_id: Option<String>,
    #[serde(default)]
    pub link_id: Option<String>,
    /// v0.7.0 H5 (round-2) — caller-supplied anti-replay nonce.
    /// Expected to be a fresh UUID v4 per verify call. The handler
    /// hashes `(canonical_link_id, verification_nonce)` into a 32-byte
    /// SHA-256 fingerprint and rejects exact-repeat tuples with 409
    /// Conflict. When `[verify] require_nonce = true` is set, missing
    /// nonces produce 400 Bad Request; otherwise a deprecation WARN
    /// is logged and the verify proceeds. See
    /// [`crate::identity::replay`] for the LRU memory bound.
    #[serde(default)]
    pub verification_nonce: Option<String>,
}

/// `POST /api/v1/links/verify` — re-verify a stored link's signature
/// (when present) and project the resolved attest level. Wire shape:
/// `{verified, attest_level, signature_present, observed_by, source_id,
/// target_id, relation, findings}`.
///
/// **v0.7.0 H5 (round-2)** — anti-replay surface. Every successful
/// verify gets a `(canonical_link_id, verification_nonce)` fingerprint
/// recorded in a bounded in-memory LRU (10 000 entries, ~512 KB
/// resident). Repeats produce 409 Conflict so a captured `verify_link`
/// request cannot be replayed indefinitely against the same daemon.
/// The LRU is per-process and per-replica — see
/// [`crate::identity::replay`] for the threat model.
pub async fn verify_link_handler(
    State(app): State<AppState>,
    Json(body): Json<VerifyLinkBody>,
) -> impl IntoResponse {
    if body.source_id.is_none() && body.link_id.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "verify_link requires either source_id or link_id",
                "fields": ["source_id", "link_id"],
            })),
        )
            .into_response();
    }
    if let Some(s) = body.source_id.as_deref()
        && let Err(e) = validate::validate_id(s)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid source_id: {e}")})),
        )
            .into_response();
    }
    if let Some(t) = body.target_id.as_deref()
        && let Err(e) = validate::validate_id(t)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid target_id: {e}")})),
        )
            .into_response();
    }

    // v0.7.0 H5 (round-2) — anti-replay gate. We treat an empty
    // string the same as missing — a `""` nonce trivially collides
    // with itself and would silently neuter the cache.
    let nonce_opt: Option<&str> = body.verification_nonce.as_deref().filter(|s| !s.is_empty());
    match (nonce_opt, app.verify_require_nonce) {
        (None, true) => {
            // Strict mode + missing nonce → 400. The wire shape includes
            // the offending field name so the client can fix the call.
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "verification_nonce is required when [verify] require_nonce = true",
                    "fields": ["verification_nonce"],
                })),
            )
                .into_response();
        }
        (None, false) => {
            // Back-compat mode: log a deprecation WARN and let the
            // verify proceed. Operators see this in journalctl and
            // can decide when to flip require_nonce on.
            tracing::warn!(
                target: "ai_memory::verify",
                "POST /api/v1/links/verify called without verification_nonce — \
                 replay protection is disabled for this request. Add a fresh \
                 UUID-v4 nonce to opt into H5 dedup; flip [verify] require_nonce = true \
                 to enforce."
            );
        }
        (Some(_), _) => {
            // Will be checked below after we know the canonical
            // link triple (so the fingerprint is stable regardless
            // of whether the request used `(source_id, target_id)`
            // or `link_id` to identify the row).
        }
    }

    #[cfg(feature = "sal")]
    {
        let filter = crate::store::VerifyFilter {
            source_id: body.source_id.clone(),
            target_id: body.target_id.clone(),
            link_id: body.link_id.clone(),
        };
        return match app.store.verify_link(filter).await {
            Ok(report) => {
                // H5: derive the canonical link_id from the resolved
                // triple. We use this rather than the request's
                // `link_id` (which may be unset) so the fingerprint
                // is stable across the two filter shapes a caller
                // might use. The signature bytes are not exposed via
                // VerifyLinkReport, so the fingerprint covers
                // `(canonical_id, nonce)` — sufficient to dedup an
                // exact replay of the same request without depending
                // on the trait surface exposing the raw signature.
                if let Some(nonce) = nonce_opt {
                    let canonical_id = format!(
                        "{}|{}|{}",
                        report.source_id, report.target_id, report.relation
                    );
                    // Empty bytes as the "signature" component —
                    // the resolved link's identity already binds the
                    // signature material via canonical_id.
                    let decision = app.replay_cache.record_and_check(&canonical_id, b"", nonce);
                    if matches!(decision, crate::identity::replay::ReplayDecision::Replay) {
                        return (
                            StatusCode::CONFLICT,
                            Json(json!({"error": "verification replay detected"})),
                        )
                            .into_response();
                    }
                }
                if crate::audit::is_enabled() {
                    crate::audit::emit(crate::audit::EventBuilder::new(
                        crate::audit::AuditAction::Link,
                        crate::audit::actor("ai:http", "http_body", None),
                        crate::audit::target_memory(
                            report.source_id.clone(),
                            String::new(),
                            Some(format!(
                                "verify -> {} {}",
                                report.target_id, report.relation
                            )),
                            None,
                            None,
                        ),
                    ));
                }
                Json(json!(report)).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    #[cfg(not(feature = "sal"))]
    {
        let _ = app;
        let _ = body;
        (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "verify_link requires --features sal"})),
        )
            .into_response()
    }
}

/// v0.7.0 G-PHASE-E-1 (#706) — canonical field whitelist for the
/// `/api/v1/links` create + delete bodies. Includes the canonical names
/// and the S82 aliases (`from` / `to` / `rel_type`). Any field outside
/// this set surfaces as a structured `unknown_field` 400 rather than
/// being silently defaulted (the pre-#706 behaviour, where a typoed
/// `link_type` would land a link with `relation = "related_to"`).
const ALLOWED_LINK_BODY_FIELDS: &[&str] = &[
    "source_id",
    "from",
    "target_id",
    "to",
    "relation",
    "rel_type",
];

/// v0.7.0 G-PHASE-E-1 (#706) — closed set of relation values accepted
/// by the SQL CHECK constraint on `memory_links.relation` (migration
/// 0027). Used to surface a structured `invalid_relation` 400 from the
/// HTTP handler before the INSERT crashes with a generic CHECK error.
const ALLOWED_LINK_RELATIONS: &[&str] = &[
    "related_to",
    "supersedes",
    "contradicts",
    "derived_from",
    "reflects_on",
];

/// Return the list of unknown fields in `raw` against
/// [`ALLOWED_LINK_BODY_FIELDS`]. Returns `None` when every key is
/// recognised (including the empty-body case) so callers can use
/// `if let Some(unknown) = …` to branch cleanly into the 400 path.
fn unknown_link_body_fields(raw: &serde_json::Value) -> Option<Vec<String>> {
    let obj = raw.as_object()?;
    let mut unknown: Vec<String> = obj
        .keys()
        .filter(|k| !ALLOWED_LINK_BODY_FIELDS.contains(&k.as_str()))
        .cloned()
        .collect();
    if unknown.is_empty() {
        None
    } else {
        unknown.sort();
        Some(unknown)
    }
}

pub async fn create_link(
    State(app): State<AppState>,
    Json(raw): Json<serde_json::Value>,
) -> impl IntoResponse {
    // v0.7.0 G-PHASE-E-1 (#706) — reject unknown fields with a
    // structured 400 instead of silently defaulting `relation` to
    // `related_to`. The canonical shape is `{source_id|from,
    // target_id|to, relation|rel_type}`; anything else (e.g. the
    // common-typo `link_type`) is a caller bug that previously
    // surfaced as a silently-defaulted insert.
    if let Some(unknown) = unknown_link_body_fields(&raw) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "unknown_field", "fields": unknown})),
        )
            .into_response();
    }
    let body: LinkBody = match serde_json::from_value(raw) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    // S82's wire shape uses `{from, to, rel_type}`; resolve canonical
    // (source_id, target_id, relation) from either field set.
    let (source_id, target_id, relation) = body.resolved();
    if let Err(e) = validate::validate_link(&source_id, &target_id, &relation) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }
    // v0.7.0 G-PHASE-E-1 (#706) — the SQL-side CHECK constraint on
    // `memory_links.relation` (migration 0027) admits only the five
    // canonical relations. `validate_relation` is intentionally more
    // permissive (accepts arbitrary `[a-z0-9_]+`) for forward-compat,
    // but anything outside the canonical set will crash the INSERT
    // with a generic CHECK violation. Pre-flight the relation against
    // the closed set here so callers get a structured 400 instead of
    // a generic 500.
    if crate::models::MemoryLinkRelation::from_str(&relation).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_relation",
                "got": relation,
                "allowed": ALLOWED_LINK_RELATIONS,
            })),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 — Postgres-backed daemons take the SAL trait
    // dispatch path. The trait's `link_signed` returns the resolved
    // `attest_level` so the wire response carries the same byte shape
    // as the legacy `db::create_link_signed` path. Federation fanout
    // is omitted on the postgres branch — quorum-broadcast is still
    // SQLite-bound and lighting it up on Postgres is a follow-on
    // wave.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        let now = Utc::now().to_rfc3339();
        // v0.7.0 fix campaign R1-M4 — wrap the wire String relation
        // into the typed `MemoryLinkRelation`. `validate_link` (above)
        // already vetted the relation against the closed set, so a
        // parse failure here would be a bug; fall back to the default
        // rather than 500 the request.
        // #869 audit (Category B — safe default): `validate_link` (line
        // 317 above) already returned 400 if the relation wasn't in the
        // closed `MemoryLinkRelation::from_str` set; reaching this site
        // with a parse failure would be a typed-set drift bug. The
        // `related_to` default preserves the link instead of dropping
        // the write.
        let relation_typed =
            crate::models::MemoryLinkRelation::from_str(&relation).unwrap_or_default();
        let link = MemoryLink {
            source_id: source_id.clone(),
            target_id: target_id.clone(),
            relation: relation_typed,
            created_at: now,
            valid_from: None,
            valid_until: None,
            observed_by: None,
            signature: None,
            attest_level: None,
        };
        let ctx = crate::store::CallerContext::for_agent("ai:http");
        return match app
            .store
            .link_signed(&ctx, &link, app.active_keypair.as_ref().as_ref())
            .await
        {
            Ok(attest_level) => {
                if crate::audit::is_enabled() {
                    crate::audit::emit(crate::audit::EventBuilder::new(
                        crate::audit::AuditAction::Link,
                        crate::audit::actor("ai:http", "http_body", None),
                        crate::audit::target_memory(
                            source_id.clone(),
                            String::new(),
                            Some(format!("{target_id} -> {relation}")),
                            None,
                            None,
                        ),
                    ));
                }
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "linked": true,
                        "source_id": source_id,
                        "target_id": target_id,
                        "relation": relation,
                        "attest_level": attest_level,
                    })),
                )
                    .into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    // v0.7 H2 — sign with the active keypair when one was loaded at
    // startup. Falls back to unsigned (signature NULL, attest_level
    // "unsigned") when no keypair is configured. Either way the chosen
    // attest level is surfaced on the wire response so callers can
    // observe whether their link was signed without re-querying.
    let create_result = db::create_link_signed(
        &lock.0,
        &source_id,
        &target_id,
        &relation,
        app.active_keypair.as_ref().as_ref(),
    );
    // v0.6.4-017 — G9 HTTP webhook parity. Fire `memory_link_created`
    // after db::create_link commits (mirrors mcp.rs:2569). The link
    // itself does not carry a namespace; we look up the source memory
    // for the namespace + owner agent_id so the event payload matches
    // the MCP contract.
    if create_result.is_ok() {
        let (link_namespace, link_owner) = db::get(&lock.0, &source_id).ok().flatten().map_or_else(
            || ("global".to_string(), None),
            |m| {
                let owner = m
                    .metadata
                    .get("agent_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                (m.namespace, owner)
            },
        );
        let details = serde_json::to_value(crate::subscriptions::LinkCreatedEventDetails {
            target_id: target_id.clone(),
            relation: relation.clone(),
        })
        .ok();
        crate::subscriptions::dispatch_event_with_details(
            &lock.0,
            "memory_link_created",
            &source_id,
            &link_namespace,
            link_owner.as_deref(),
            &lock.1,
            details,
        );
    }
    // Drop DB lock before fanning out — peers POST back to our sync_push
    // and we'd deadlock on the shared Mutex if we held it.
    drop(lock);
    match create_result {
        Ok(attest_level) => {
            // v0.6.2 (#325): propagate link to peers.
            if let Some(fed) = app.federation.as_ref() {
                // v0.7.0 fix campaign R1-M4 — `validate_link` already
                // gated `relation` against the closed set; the parse
                // here cannot fail in practice.
                // #869 audit (Category B — safe default): same posture
                // as the postgres branch above; `validate_link` returned
                // 400 on an unknown relation upstream of this branch.
                let relation_typed =
                    crate::models::MemoryLinkRelation::from_str(&relation).unwrap_or_default();
                let link = crate::models::MemoryLink {
                    source_id: source_id.clone(),
                    target_id: target_id.clone(),
                    relation: relation_typed,
                    created_at: chrono::Utc::now().to_rfc3339(),
                    // H3 wire fields are populated by `export_links`
                    // on the next bulk re-sync; the immediate fanout
                    // path stays unsigned to avoid a redundant DB
                    // round-trip just to fish out the freshly-written
                    // signature row. Receivers will land this as
                    // `unsigned` until a periodic reconciliation pulls
                    // the signed row via `export_links`.
                    signature: None,
                    observed_by: None,
                    valid_from: None,
                    valid_until: None,
                    attest_level: None,
                };
                match crate::federation::broadcast_link_quorum(fed, &link).await {
                    Ok(tracker) => {
                        if let Err(err) = crate::federation::finalise_quorum(&tracker) {
                            // #869 — typed 503 envelope via the shared helper.
                            let payload = crate::federation::QuorumNotMetPayload::from_err(&err);
                            return super::quorum_not_met_response(&payload);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("link fanout error (local committed): {e:?}");
                    }
                }
            }
            // v0.7 H2 — surface attest_level on the wire so callers
            // can tell signed vs unsigned without re-querying.
            (
                StatusCode::CREATED,
                Json(json!({"linked": true, "attest_level": attest_level})),
            )
                .into_response()
        }
        Err(e) => {
            // v0.7.0 fix-campaign A3 (LINK-PARITY, #690) — map the
            // two new storage-layer refusals to their canonical HTTP
            // status codes. Cycle refusals are 409 CONFLICT (the
            // graph state conflicts with the new edge); K9 deny is
            // 403 FORBIDDEN. Anything else stays 500 — those are
            // server faults the caller cannot fix by retrying with
            // different inputs.
            let msg = e.to_string();
            if msg.starts_with(db::LINK_CYCLE_ERR_PREFIX) {
                return (StatusCode::CONFLICT, Json(json!({"error": msg}))).into_response();
            }
            if msg.starts_with(db::LINK_PERMISSION_DENIED_ERR_PREFIX) {
                return (StatusCode::FORBIDDEN, Json(json!({"error": msg}))).into_response();
            }
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// v0.6.2 (#325) — DELETE /api/v1/links. Removes the directional link
/// `source_id → target_id` locally. Deletion is NOT fanned out in v0.6.2:
/// the receiving-side API is `db::delete_link`, and `sync_push` does not
/// yet carry a link-tombstone list. Full link tombstones ship with v0.7
/// CRDT-lite. For current scenario coverage (scenario-11 tests create),
/// create-link fanout is sufficient.
pub async fn delete_link(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(raw): Json<serde_json::Value>,
) -> impl IntoResponse {
    // v0.7.0 G-PHASE-E-1 (#706) — mirror create_link: reject unknown
    // fields with a structured 400 so caller bugs surface loudly
    // instead of silently defaulting.
    if let Some(unknown) = unknown_link_body_fields(&raw) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "unknown_field", "fields": unknown})),
        )
            .into_response();
    }
    let body: LinkBody = match serde_json::from_value(raw) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    let (source_id, target_id, relation) = body.resolved();
    if let Err(e) = validate::validate_link(&source_id, &target_id, &relation) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    // #913 (security-medium / SOC2, 2026-05-19) — admin/destructive
    // action audit. Link delete mutates the graph topology; emit the
    // forensic-chain entry BEFORE the storage write so the audit trail
    // captures intent regardless of downstream success.
    let header_agent_id = headers.get("x-agent-id").and_then(|v| v.to_str().ok());
    let caller = crate::identity::resolve_http_agent_id(None, header_agent_id)
        .unwrap_or_else(|_| "anonymous:invalid".to_string());
    crate::governance::audit::record_decision(
        &caller,
        "allow",
        "link_delete",
        "",
        json!({
            "source_id": source_id,
            "target_id": target_id,
            "relation": relation,
        }),
    );

    let lock = app.db.lock().await;
    let delete_result = db::delete_link(&lock.0, &source_id, &target_id);
    drop(lock);
    match delete_result {
        Ok(removed) => Json(json!({"deleted": removed})).into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

pub async fn get_links(State(app): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    if let Err(e) = validate::validate_id(&id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": e.to_string()})),
        )
            .into_response();
    }

    // v0.7.0 Wave-3 — Postgres-backed daemons walk `list_links` (no
    // namespace filter — the same projection as the legacy
    // `db::get_links` returns) and narrow client-side to edges
    // anchored at `id`. The trait does not (yet) expose a per-anchor
    // edge probe; this filter is O(|edges|), which matches the
    // SQLite path's behaviour for typical workloads.
    #[cfg(feature = "sal")]
    if matches!(app.storage_backend, StorageBackend::Postgres) {
        return match app.store.list_links(None).await {
            Ok(rows) => {
                let edges: Vec<_> = rows
                    .into_iter()
                    .filter(|l| l.source_id == id || l.target_id == id)
                    .collect();
                Json(json!({"links": edges})).into_response()
            }
            Err(e) => store_err_to_response(e),
        };
    }

    let lock = app.db.lock().await;
    match db::get_links(&lock.0, &id) {
        Ok(links) => Json(json!({"links": links})).into_response(),
        Err(e) => {
            tracing::error!("handler error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}
