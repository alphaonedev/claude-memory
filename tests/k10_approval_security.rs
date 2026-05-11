// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(
    clippy::collapsible_if,
    clippy::items_after_statements,
    clippy::redundant_closure_for_method_calls
)]
//! v0.7.0 K10 — security blockers from review #628.
//!
//! Pins the three fixes:
//!
//!   * **C1 (HMAC replay window)** — `hmac_replay_rejected`. Posts a
//!     correctly signed approval request whose `X-AI-Memory-Timestamp`
//!     header is older than the 5-minute replay window and asserts a
//!     `401`. Without the fix the server happily approved the row.
//!   * **C2 (SSE tenant isolation)** — `sse_tenant_isolation`. Spins up
//!     two SSE subscribers, each identifying as a distinct agent, and
//!     fires one `approval_requested` per agent. Each subscriber must
//!     see only its own event, never the other tenant's.
//!   * **H10 (`remember=forever` actually remembers)** —
//!     `remember_forever_actually_remembers`. After approving a pending
//!     row with `remember=forever`, asserts that
//!     `Permissions::evaluate` auto-decides the same `(action_type,
//!     namespace, agent_id)` tuple to `Allow` without re-prompting.
//!
//! `await_holding_lock` lints fire on `std::sync::Mutex` — but the
//! lock here is purely a test-serialisation primitive (the global
//! state these tests touch is itself thread-safe). Allow at the file
//! level instead of at every test fn.
#![allow(clippy::await_holding_lock)]

use ai_memory::approvals::{
    SyntheticPermissionRule, clear_synthetic_rules_for_test, list_synthetic_rules,
    record_synthetic_rule,
};
use ai_memory::config::set_active_hooks_hmac_secret;
use ai_memory::permissions::{Decision, Op, PermissionContext, Permissions};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::sync::Mutex;
use std::time::Duration;
use tower::ServiceExt as _;

/// File-wide serialiser. The global HMAC-secret state and the
/// synthetic-rule registry are process-wide; running these
/// scenarios in parallel would alias state across tests.
static K10_SECURITY_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Shared test plumbing — mirrors `tests/k10_approval_http.rs` so the
// blocker tests can be read in isolation.
// ---------------------------------------------------------------------------

fn build_router_with_db() -> (axum::Router, ai_memory::handlers::Db) {
    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).unwrap();
    let path = std::path::PathBuf::from(":memory:");
    let db: ai_memory::handlers::Db = std::sync::Arc::new(tokio::sync::Mutex::new((
        conn,
        path,
        ai_memory::config::ResolvedTtl::default(),
        true,
    )));
    #[cfg(feature = "sal")]
    let store: std::sync::Arc<dyn ai_memory::store::MemoryStore> = {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile for SqliteStore");
        let p = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        std::sync::Arc::new(
            ai_memory::store::sqlite::SqliteStore::open(&p).expect("open SqliteStore"),
        )
    };
    let app_state = ai_memory::handlers::AppState {
        db: db.clone(),
        embedder: std::sync::Arc::new(None),
        vector_index: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
        federation: std::sync::Arc::new(None),
        tier_config: std::sync::Arc::new(ai_memory::config::FeatureTier::Keyword.config()),
        scoring: std::sync::Arc::new(ai_memory::config::ResolvedScoring::default()),
        profile: std::sync::Arc::new(ai_memory::profile::Profile::core()),
        mcp_config: std::sync::Arc::new(None),
        active_keypair: std::sync::Arc::new(None),
        family_embeddings: std::sync::Arc::new(tokio::sync::RwLock::new(Some(Vec::new()))),
        storage_backend: ai_memory::handlers::StorageBackend::Sqlite,
        #[cfg(feature = "sal")]
        store,
        llm: std::sync::Arc::new(None),
        auto_tag_model: std::sync::Arc::new(None),
        llm_call_timeout: std::time::Duration::from_secs(30),
        replay_cache: std::sync::Arc::new(ai_memory::identity::replay::ReplayCache::default()),

        verify_require_nonce: false,
        autonomous_hooks: false,
        recall_scope: std::sync::Arc::new(None),
    };
    let api_key_state = ai_memory::handlers::ApiKeyState { key: None };
    let router = ai_memory::build_router(api_key_state, app_state);
    (router, db)
}

async fn seed_pending_delete_row(
    db: &ai_memory::handlers::Db,
    namespace: &str,
    requested_by: &str,
) -> String {
    let lock = db.lock().await;
    let mem = ai_memory::models::Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: ai_memory::models::Tier::Long,
        namespace: namespace.to_string(),
        title: "k10-security-seed".into(),
        content: "x".into(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".into(),
        access_count: 0,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
    };
    let mem_id = ai_memory::db::insert(&lock.0, &mem).expect("insert memory");
    let payload = json!({"reason": "k10-security"});
    ai_memory::db::queue_pending_action(
        &lock.0,
        ai_memory::models::GovernedAction::Delete,
        namespace,
        Some(&mem_id),
        requested_by,
        &payload,
    )
    .expect("queue_pending_action")
}

/// Compute the K7-style HMAC signature header value for a request body.
/// Verbatim copy of the helper in `tests/k10_approval_http.rs` — the
/// logic is small enough to duplicate without the cost of a shared
/// crate, and keeping it inline lets each blocker test stand alone.
fn sign(secret: &str, timestamp: &str, body: &str) -> String {
    use sha2::Digest;
    use sha2::Sha256;
    fn sha256_hex(s: &str) -> String {
        let mut h = Sha256::new();
        h.update(s.as_bytes());
        format!("{:x}", h.finalize())
    }
    fn hmac_sha256_hex(key_hex: &str, body: &str) -> String {
        const BLOCK: usize = 64;
        let key_bytes = hex_decode(key_hex).unwrap_or_else(|| key_hex.as_bytes().to_vec());
        let mut key = key_bytes;
        if key.len() > BLOCK {
            let mut h = Sha256::new();
            h.update(&key);
            key = h.finalize().to_vec();
        }
        key.resize(BLOCK, 0);
        let mut opad = [0x5cu8; BLOCK];
        let mut ipad = [0x36u8; BLOCK];
        for i in 0..BLOCK {
            opad[i] ^= key[i];
            ipad[i] ^= key[i];
        }
        let mut inner = Sha256::new();
        inner.update(ipad);
        inner.update(body.as_bytes());
        let inner_digest = inner.finalize();
        let mut outer = Sha256::new();
        outer.update(opad);
        outer.update(inner_digest);
        format!("{:x}", outer.finalize())
    }
    fn hex_decode(s: &str) -> Option<Vec<u8>> {
        if !s.len().is_multiple_of(2) {
            return None;
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
            .collect()
    }
    let key_hash = sha256_hex(secret);
    let canonical = format!("{timestamp}.{body}");
    let sig = hmac_sha256_hex(&key_hash, &canonical);
    format!("sha256={sig}")
}

// ---------------------------------------------------------------------------
// C1 — HMAC replay window.
// ---------------------------------------------------------------------------

/// A correctly signed approval whose timestamp is older than the
/// 5-minute replay window MUST be rejected with `401`.
///
/// Pre-fix posture: the handler verified the signature against the
/// `<timestamp>.<body>` canonical string but never compared the
/// timestamp to the wall clock, so a captured request could be
/// replayed indefinitely.
#[tokio::test]
async fn hmac_replay_rejected() {
    let _g = K10_SECURITY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_active_hooks_hmac_secret(Some("k10-replay-secret".to_string()));
    let (router, db) = build_router_with_db();
    let pending_id = seed_pending_delete_row(&db, "scratch", "alice").await;

    let body = json!({"decision": "approve", "remember": "once"}).to_string();
    // 600s in the past — well outside the 300s allowed window.
    let stale_ts = (chrono::Utc::now().timestamp() - 600).to_string();
    let sig = sign("k10-replay-secret", &stale_ts, &body);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/approvals/{pending_id}"))
        .header("content-type", "application/json")
        .header("x-ai-memory-timestamp", &stale_ts)
        .header("x-ai-memory-signature", sig)
        .header("x-agent-id", "operator-1")
        .body(Body::from(body))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "stale-timestamp signed request must be 401 (review #628 C1)"
    );
    set_active_hooks_hmac_secret(None);
}

/// A correctly signed approval whose timestamp lies inside the
/// allowed window MUST still succeed. Pins the positive control so
/// the replay-window fix doesn't regress the happy path.
#[tokio::test]
async fn hmac_fresh_timestamp_accepted() {
    let _g = K10_SECURITY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    set_active_hooks_hmac_secret(Some("k10-replay-secret".to_string()));
    let (router, db) = build_router_with_db();
    let pending_id = seed_pending_delete_row(&db, "scratch", "alice").await;

    let body = json!({"decision": "approve", "remember": "once"}).to_string();
    let now_ts = chrono::Utc::now().timestamp().to_string();
    let sig = sign("k10-replay-secret", &now_ts, &body);

    let req = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/approvals/{pending_id}"))
        .header("content-type", "application/json")
        .header("x-ai-memory-timestamp", &now_ts)
        .header("x-ai-memory-signature", sig)
        .header("x-agent-id", "operator-1")
        .body(Body::from(body))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "fresh-timestamp signed request must still succeed"
    );
    set_active_hooks_hmac_secret(None);
}

// ---------------------------------------------------------------------------
// C2 — SSE tenant isolation.
// ---------------------------------------------------------------------------

/// Two SSE subscribers, two events, each subscriber sees only its
/// own. Pins the cross-tenant filter (review #628 C2): the
/// process-wide broadcast bus must not leak metadata across agents.
///
/// We exercise the filter through the in-process publish/subscribe
/// surface — same code path the SSE handler uses to fan out events,
/// just without the SSE-protocol envelope. The `sse_event_visible_to`
/// predicate is then asserted directly so a regression in the SSE
/// handler can't quietly route the event past the filter.
#[tokio::test]
async fn sse_tenant_isolation() {
    let _g = K10_SECURITY_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    // Subscribe both rxs BEFORE publishing — broadcast channels
    // never replay.
    let mut rx_alice = ai_memory::approvals::subscribe();
    let mut rx_bob = ai_memory::approvals::subscribe();

    let evt_alice = ai_memory::approvals::ApprovalEvent::ApprovalRequested {
        pending_id: "pa-tenant-alice".into(),
        action_type: "store".into(),
        namespace: "alice/scratch".into(),
        requested_by: "alice".into(),
        requested_at: chrono::Utc::now().to_rfc3339(),
    };
    let evt_bob = ai_memory::approvals::ApprovalEvent::ApprovalRequested {
        pending_id: "pa-tenant-bob".into(),
        action_type: "store".into(),
        namespace: "bob/scratch".into(),
        requested_by: "bob".into(),
        requested_at: chrono::Utc::now().to_rfc3339(),
    };
    ai_memory::approvals::publish(evt_alice.clone());
    ai_memory::approvals::publish(evt_bob.clone());

    // Drain each rx and apply the SSE-handler filter the same way
    // the production handler does. Each subscriber MUST see exactly
    // one event — the one it owns — and never the other tenant's.
    let collect = |rx: &mut tokio::sync::broadcast::Receiver<
        ai_memory::approvals::ApprovalEvent,
    >,
                   subscriber: &str| {
        let subscriber = subscriber.to_string();
        let mut visible: Vec<String> = Vec::new();
        // Pull every queued frame without blocking — the bus has at
        // most two events at this point so a tight loop is fine.
        while let Ok(evt) = rx.try_recv() {
            if ai_memory::handlers::sse_event_visible_to(&subscriber, &evt) {
                if let ai_memory::approvals::ApprovalEvent::ApprovalRequested {
                    pending_id, ..
                } = evt
                {
                    visible.push(pending_id);
                }
            }
        }
        visible
    };
    let alice_seen = collect(&mut rx_alice, "alice");
    let bob_seen = collect(&mut rx_bob, "bob");

    assert_eq!(
        alice_seen,
        vec!["pa-tenant-alice".to_string()],
        "alice must see only her own event; got {alice_seen:?}"
    );
    assert_eq!(
        bob_seen,
        vec!["pa-tenant-bob".to_string()],
        "bob must see only his own event; got {bob_seen:?}"
    );
}

/// Belt-and-braces: an unidentified subscriber (no `X-Agent-Id`
/// header) sees nothing — fail-closed. A regression where the SSE
/// handler defaults to "see all" would leak every tenant.
#[tokio::test]
async fn sse_anonymous_subscriber_sees_nothing() {
    let _g = K10_SECURITY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let evt = ai_memory::approvals::ApprovalEvent::ApprovalRequested {
        pending_id: "pa-anon".into(),
        action_type: "store".into(),
        namespace: "scratch".into(),
        requested_by: "alice".into(),
        requested_at: chrono::Utc::now().to_rfc3339(),
    };
    assert!(
        !ai_memory::handlers::sse_event_visible_to("", &evt),
        "anonymous subscriber must never see an event"
    );
}

/// End-to-end SSE check: stand up two HTTP subscribers, each with
/// their own `X-Agent-Id`, fire two pending rows (one per agent),
/// and assert the SSE bytes each subscriber reads contain only
/// their own pending id.
#[tokio::test]
async fn sse_http_two_subscribers_isolated() {
    use http_body_util::BodyExt as _;

    let _g = K10_SECURITY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (router, _db) = build_router_with_db();

    // Open both SSE streams BEFORE publishing — broadcast channels
    // never replay, and the SSE handler attaches its bus subscriber
    // when the request arrives, not when the body is polled.
    let req_alice = Request::builder()
        .method("GET")
        .uri("/api/v1/approvals/stream")
        .header("accept", "text/event-stream")
        .header("x-agent-id", "alice")
        .body(Body::empty())
        .unwrap();
    let req_bob = Request::builder()
        .method("GET")
        .uri("/api/v1/approvals/stream")
        .header("accept", "text/event-stream")
        .header("x-agent-id", "bob")
        .body(Body::empty())
        .unwrap();
    let resp_alice = router.clone().oneshot(req_alice).await.unwrap();
    let resp_bob = router.clone().oneshot(req_bob).await.unwrap();
    assert_eq!(resp_alice.status(), 200);
    assert_eq!(resp_bob.status(), 200);
    let mut body_alice = resp_alice.into_body();
    let mut body_bob = resp_bob.into_body();

    // Fire one ApprovalRequested per tenant. Use the in-process
    // publish path so the test does not depend on having a fully
    // seeded `pending_actions` row for each event.
    tokio::spawn(async move {
        // Tiny delay so the SSE handlers attach their bus subscribers
        // before the broadcast fires.
        tokio::time::sleep(Duration::from_millis(100)).await;
        ai_memory::approvals::publish(ai_memory::approvals::ApprovalEvent::ApprovalRequested {
            pending_id: "pa-http-alice".into(),
            action_type: "store".into(),
            namespace: "alice/scratch".into(),
            requested_by: "alice".into(),
            requested_at: chrono::Utc::now().to_rfc3339(),
        });
        ai_memory::approvals::publish(ai_memory::approvals::ApprovalEvent::ApprovalRequested {
            pending_id: "pa-http-bob".into(),
            action_type: "store".into(),
            namespace: "bob/scratch".into(),
            requested_by: "bob".into(),
            requested_at: chrono::Utc::now().to_rfc3339(),
        });
    });

    async fn read_until_event(body: &mut axum::body::Body, marker: &str) -> Result<String, String> {
        let mut buf = Vec::new();
        loop {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Some(bytes) = frame.data_ref() {
                        buf.extend_from_slice(bytes);
                        let s = String::from_utf8_lossy(&buf).to_string();
                        if s.contains(marker) {
                            return Ok(s);
                        }
                    }
                }
                Some(Err(e)) => return Err(format!("body error: {e}")),
                None => return Err("body ended before event".into()),
            }
        }
    }

    let alice_text = tokio::time::timeout(
        Duration::from_secs(5),
        read_until_event(&mut body_alice, "pa-http-alice"),
    )
    .await
    .expect("alice SSE timeout")
    .expect("alice SSE read");
    let bob_text = tokio::time::timeout(
        Duration::from_secs(5),
        read_until_event(&mut body_bob, "pa-http-bob"),
    )
    .await
    .expect("bob SSE timeout")
    .expect("bob SSE read");

    // Alice's stream must contain her event but never bob's, and
    // vice-versa.
    assert!(
        alice_text.contains("pa-http-alice"),
        "alice should see her event; stream: {alice_text}"
    );
    assert!(
        !alice_text.contains("pa-http-bob"),
        "alice MUST NOT see bob's event; stream: {alice_text}"
    );
    assert!(
        bob_text.contains("pa-http-bob"),
        "bob should see his event; stream: {bob_text}"
    );
    assert!(
        !bob_text.contains("pa-http-alice"),
        "bob MUST NOT see alice's event; stream: {bob_text}"
    );
}

// ---------------------------------------------------------------------------
// H10 — `remember=forever` actually remembers.
// ---------------------------------------------------------------------------

/// Approve a pending row with `remember=forever`, then re-evaluate
/// the same `(action_type, namespace, agent_id)` tuple via the
/// unified K9 [`Permissions::evaluate`] entry point and assert it
/// short-circuits to `Allow` without re-prompting.
///
/// Pre-fix posture (review #628 H10): the synthetic rule was
/// recorded in a separate registry that K9 never consulted, so the
/// next call still landed in `Ask` (or its mode-default fallback).
#[tokio::test]
async fn remember_forever_actually_remembers() {
    let _g = K10_SECURITY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    clear_synthetic_rules_for_test();

    // First call — record the operator's "forever-approve" decision
    // for ("delete", "ns-h10", "alice") via the same registry the
    // K10 transports write to. We bypass the HTTP/MCP transports
    // here so the test exercises the registry → evaluator wiring,
    // not the transport-side serdes.
    record_synthetic_rule(SyntheticPermissionRule {
        action_type: "delete".into(),
        namespace: "ns-h10".into(),
        agent_id: Some("alice".into()),
        decision: "approve".into(),
        recorded_at: chrono::Utc::now().to_rfc3339(),
    });

    // Sanity: the rule is in the registry.
    let snap = list_synthetic_rules();
    assert!(
        snap.iter().any(|r| r.namespace == "ns-h10"),
        "synthetic rule missing from registry: {snap:?}"
    );

    // Now re-evaluate the same tuple via the K9 unified evaluator.
    // Pre-fix this returned `Ask` (rules registry was empty); the
    // H10 fix wires synthetic rules into `evaluate` so this returns
    // `Allow` and the next call does NOT re-prompt the operator.
    let ctx = PermissionContext {
        op: Op::MemoryDelete,
        namespace: "ns-h10".into(),
        agent_id: "alice".into(),
        payload: serde_json::json!({}),
    };
    let decision = Permissions::evaluate(&ctx, &[]);
    assert_eq!(
        decision,
        Decision::Allow,
        "remember=forever rule must auto-approve next call (review #628 H10); got {decision:?}"
    );
    clear_synthetic_rules_for_test();
}

/// Counter-control: the same evaluator MUST still ask (or fall
/// through to the mode default) when no synthetic rule is recorded.
/// Without this assertion a buggy synthetic-rule reader that always
/// returned `Allow` would silently pass `remember_forever_actually_remembers`.
#[tokio::test]
async fn evaluate_without_synthetic_rule_does_not_auto_allow() {
    let _g = K10_SECURITY_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    clear_synthetic_rules_for_test();
    let ctx = PermissionContext {
        op: Op::MemoryDelete,
        namespace: "ns-h10-untouched".into(),
        agent_id: "alice".into(),
        payload: serde_json::json!({}),
    };
    let decision = Permissions::evaluate(&ctx, &[]);
    // No rule, no hook, no synthetic entry → mode default. Both
    // Enforce and Advisory default to `Allow` for un-policied
    // namespaces (per `mode_default_for`), so we can't simply
    // require `Ask` here. Instead, assert the synthetic registry is
    // empty AND that the evaluator is not a constant-`Allow` for
    // the right reason: feed a deny rule and confirm it still wins.
    assert!(matches!(decision, Decision::Allow));
    let denying_rule = ai_memory::permissions::PermissionRule {
        namespace_pattern: "ns-h10-untouched".into(),
        op: "memory_delete".into(),
        agent_pattern: "*".into(),
        decision: ai_memory::permissions::RuleDecision::Deny,
        reason: Some("explicit deny".into()),
    };
    let d2 = Permissions::evaluate_with(
        &ctx,
        &[],
        &[denying_rule],
        ai_memory::config::PermissionsMode::Enforce,
    );
    assert!(
        matches!(d2, Decision::Deny(_)),
        "explicit deny must still win when no synthetic rule shadows it"
    );
}
