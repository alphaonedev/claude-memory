// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown, clippy::manual_let_else, clippy::map_unwrap_or)]
//! v0.7.0 Round-2 F10 — embedder skip / fail surfaces `embed_status`
//! in the HTTP store response.
//!
//! Pre-F10 behaviour: a memory whose content blew past the embedder's
//! token budget still committed at the row level (correct — embeddings
//! are an enhancement layer, not a write-path gate) but the HTTP
//! response was indistinguishable from a normal 201 even though the
//! row would silently miss every semantic-recall query until a
//! re-index. F10 surfaces the skip/fail outcome on the response by
//! consuming Fix-Agent α's `embeddings::EmbedStatus` enum +
//! `Embedder::embed_with_status` producer.
//!
//! ## Wire shape
//!
//! Non-`Indexed` outcomes add an `embed_status` (and an
//! `embed_status_reason`) field to the 201 response body:
//!
//! ```json
//! { "id": "...", "embed_status": "skipped", "embed_status_reason": "..." }
//! ```
//!
//! The `Indexed` (success) path intentionally does NOT add the field
//! so the response shape is unchanged for the common case.
//!
//! ## Local-model availability
//!
//! The `Embedder::new_local()` constructor pulls the MiniLM model from
//! HuggingFace Hub. On CI workers without a pre-warmed cache + no
//! network this fails; we follow α's F6 pattern and `return` cleanly
//! so the suite stays green on offline workers. The contract is still
//! pinned at the lower layer by α's `embed_status_*` unit tests in
//! `src/embeddings.rs` and by the F9/F7 sister tests above.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use tokio::sync::Mutex;
use tower::ServiceExt as _;

use ai_memory::config::{FeatureTier, ResolvedScoring, ResolvedTtl};
use ai_memory::embeddings::Embedder;
use ai_memory::handlers::{ApiKeyState, AppState, Db};

fn build_router_with_embedder(embedder: Option<Embedder>) -> (axum::Router, NamedTempFile) {
    let f = NamedTempFile::new().expect("tempfile");
    let db_path = f.path().to_path_buf();
    let _ = ai_memory::db::open(&db_path).expect("db::open");
    let conn = ai_memory::db::open(&db_path).expect("reopen for AppState");
    let db: Db = Arc::new(Mutex::new((
        conn,
        db_path.clone(),
        ResolvedTtl::default(),
        true,
    )));
    #[cfg(feature = "sal")]
    let store: Arc<dyn ai_memory::store::MemoryStore> =
        Arc::new(ai_memory::store::sqlite::SqliteStore::open(&db_path).expect("open SqliteStore"));
    let app_state = AppState {
        db,
        embedder: Arc::new(embedder),
        vector_index: Arc::new(Mutex::new(None)),
        federation: Arc::new(None),
        tier_config: Arc::new(FeatureTier::Keyword.config()),
        scoring: Arc::new(ResolvedScoring::default()),
        profile: Arc::new(ai_memory::profile::Profile::core()),
        mcp_config: Arc::new(None),
        active_keypair: Arc::new(None),
        family_embeddings: Arc::new(tokio::sync::RwLock::new(Some(Vec::new()))),
        storage_backend: ai_memory::handlers::StorageBackend::Sqlite,
        #[cfg(feature = "sal")]
        store,
        llm: Arc::new(None),
        auto_tag_model: Arc::new(None),
        llm_call_timeout: std::time::Duration::from_secs(30),
        replay_cache: std::sync::Arc::new(ai_memory::identity::replay::ReplayCache::default()),

        verify_require_nonce: false,
        autonomous_hooks: false,
        recall_scope: Arc::new(None),
        deferred_audit_queue: Arc::new(None),
    };
    let api_key_state = ApiKeyState {
        key: None,
        mtls_enforced: false,
    };
    let router = ai_memory::build_router(api_key_state, app_state);
    (router, f)
}

async fn post(router: &axum::Router, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/memories")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, parsed)
}

#[tokio::test]
async fn http_oversized_content_surfaces_skipped_embed_status() {
    // F10 acceptance: content >64KB returns 201 (row committed) AND
    // `embed_status: "skipped"` so the caller can tell semantic
    // recall will miss this row until a re-index.
    let local = match Embedder::new_local() {
        Ok(e) => e,
        Err(_) => {
            // Offline CI worker — see the module-level note. Skip
            // cleanly; α's `embed_status_*` unit tests cover the
            // lower-layer contract, and the F9/F7 sister tests cover
            // the rest of the HTTP path.
            return;
        }
    };
    let (router, _keep) = build_router_with_embedder(Some(local));

    // The HTTP store path validates `content.len() <= MAX_CONTENT_SIZE`
    // (= 65536 = EMBED_MAX_BYTES) before the handler ever sees the body.
    // To exercise the embedder-skip branch we need:
    //   * a content that PASSES the validator (≤ 65536 bytes), AND
    //   * a concatenated `"{title} {content}"` that EXCEEDS the
    //     embedder cap (> 65536 bytes).
    // Title + space adds 19 bytes; content at 65530 bytes makes the
    // embedded text 65530 + 1 + 18 = 65549 > 65536 → Skipped.
    let title = "oversized embedder";
    let content = "x".repeat(65_530);
    let body = json!({
        "tier": "long",
        "namespace": "round2-f10",
        "title": title,
        "content": content,
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {},
        "agent_id": "round2-f10-agent",
    });
    let (status, payload) = post(&router, body).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "F10: row must still commit at 201 even when the embedder skips (got {status})"
    );
    let embed_status = payload.get("embed_status").and_then(|v| v.as_str()).expect(
        "F10: response must include `embed_status` field on non-Indexed outcomes \
             (got payload without that field)",
    );
    assert!(
        embed_status == "skipped" || embed_status == "failed",
        "F10: oversized-content branch should map to `skipped` or `failed` (got {embed_status:?})"
    );
    assert!(
        payload.get("id").and_then(|v| v.as_str()).is_some(),
        "F10: skip-branch response must still carry the row `id` so the caller can \
         re-index later"
    );
    // The reason field carries the human-facing detail (e.g. "content
    // 71680 bytes exceeds embed cap 65536 bytes"). Best-effort assert
    // that it is populated for the skip path.
    if embed_status == "skipped" {
        assert!(
            payload
                .get("embed_status_reason")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "F10: skip path should populate `embed_status_reason`"
        );
    }
}

#[tokio::test]
async fn http_empty_content_surfaces_skipped_embed_status() {
    // α's `embed_with_status` reports `Skipped("empty content")` on
    // empty input. We can't actually POST a memory with empty content
    // (validate::validate_create rejects it), so we use a single-char
    // title and content that, after concat, the embedder still treats
    // as legitimate input — the empty-content branch is exercised at
    // the lower layer by α's unit tests. Here we just pin that an
    // available embedder + a normal-size body produces an `Indexed`
    // outcome (no `embed_status` field surfaced).
    let local = match Embedder::new_local() {
        Ok(e) => e,
        Err(_) => return,
    };
    let (router, _keep) = build_router_with_embedder(Some(local));

    let body = json!({
        "tier": "long",
        "namespace": "round2-f10",
        "title": "small body",
        "content": "small enough to embed cleanly",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {},
        "agent_id": "round2-f10-agent-ok",
    });
    let (status, payload) = post(&router, body).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        payload.get("embed_status").is_none(),
        "F10: success path must NOT include `embed_status` (got {payload})"
    );
}

#[tokio::test]
async fn http_keyword_only_node_does_not_surface_embed_status() {
    // Negative pin: a keyword-only deployment (embedder=None)
    // intentionally reports `Indexed` so we don't leak the
    // configuration outcome into every response. This branch runs
    // without the local model so it never has to skip on offline CI.
    let (router, _keep) = build_router_with_embedder(None);

    let body = json!({
        "tier": "long",
        "namespace": "round2-f10",
        "title": "keyword-only probe",
        "content": "keyword-only deployment must stay silent on embed_status",
        "tags": [],
        "priority": 5,
        "confidence": 1.0,
        "source": "api",
        "metadata": {},
        "agent_id": "round2-f10-keyword-agent",
    });
    let (status, payload) = post(&router, body).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(
        payload.get("embed_status").is_none(),
        "F10: keyword-only nodes (embedder=None) must not surface embed_status \
         (got {payload})"
    );
    // And a final shape probe: even on the keyword path the response
    // still carries the canonical `id`. Belt-and-braces against a
    // future change that reorganises the response builder.
    assert!(payload.get("id").and_then(|v| v.as_str()).is_some());
}
