// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 K7 — server-wide webhook HMAC override.
//!
//! K6-and-earlier signed outbound webhook payloads with the
//! per-subscription `secret` registered via `memory_subscribe`. K7
//! adds a server-wide `[hooks.subscription] hmac_secret` config knob
//! that signs EVERY outbound payload — even subscriptions that didn't
//! register a per-subscription secret — so a paranoid operator can
//! attach a fleet-wide signing key without round-tripping each
//! receiver through `memory_subscribe`.
//!
//! These tests pin:
//!   1. configuration plumbing — `set_active_hooks_hmac_secret` /
//!      `active_hooks_hmac_secret` round-trip cleanly;
//!   2. effective dispatch — when the override is set and a
//!      subscription has no per-sub secret, the wiremock POST carries
//!      the `x-ai-memory-signature: sha256=<hex>` header;
//!   3. the signature payload matches `HMAC-SHA256(SHA256(secret),
//!      "<timestamp>.<body>")` — the same construction the
//!      per-subscription path produces, so receiver verification is
//!      identical regardless of which path configured the secret.
//!
//! The K7 wiring lives in `src/subscriptions.rs::dispatch_event_with_details`
//! (the `signature = match secret_hash {...}` block).

use ai_memory::config::{
    HooksConfig, HooksSubscriptionConfig, active_hooks_hmac_secret, set_active_hooks_hmac_secret,
};
use ai_memory::subscriptions::{self, NewSubscription};
use rusqlite::Connection;
use std::sync::Mutex;
use std::time::Duration;
use tempfile::NamedTempFile;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Serialize the four tests that mutate the process-wide K7 HMAC
/// override. `cargo test` parallelizes by default; without this lock,
/// `..._signed` and `..._unsigned` race on the global state and one of
/// them sees stale config when its dispatch thread runs.
static K7_HMAC_GLOBAL_LOCK: Mutex<()> = Mutex::new(());

fn fresh_db() -> (NamedTempFile, std::path::PathBuf) {
    // H11 (#628 blocker): wiremock binds to 127.0.0.1; loopback
    // webhook URLs are rejected by default, so opt in here.
    ai_memory::config::set_allow_loopback_webhooks(true);
    let f = NamedTempFile::new().expect("tempfile");
    let p = f.path().to_path_buf();
    let _ = ai_memory::db::open(&p).expect("db::open");
    (f, p)
}

/// Wiremock responder mirroring the production K6 ACK contract:
/// 200 + JSON body `{"status":"ack","correlation_id":"<echoed>"}`.
struct AckEcho;
impl wiremock::Respond for AckEcho {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let corr = request
            .headers
            .get("x-ai-memory-correlation-id")
            .map(|v| v.to_str().unwrap_or("").to_string())
            .unwrap_or_default();
        let body = serde_json::json!({"status": "ack", "correlation_id": corr});
        ResponseTemplate::new(200).set_body_json(body)
    }
}

#[test]
fn k7_hooks_subscription_config_round_trips_through_serde() {
    #[derive(serde::Deserialize)]
    struct Wrapper {
        hooks: HooksConfig,
    }

    // Operator-visible TOML deserializes cleanly into the K7
    // `HooksConfig` shape. Asserting this here pins the wire format
    // operators are expected to put in `~/.config/ai-memory/config.toml`.
    let toml_src = r#"
[hooks.subscription]
hmac_secret = "fleet-wide-secret"
"#;
    let cfg: Wrapper = toml::from_str(toml_src).expect("parse [hooks.subscription]");
    let secret = cfg
        .hooks
        .subscription
        .as_ref()
        .and_then(|s| s.hmac_secret.clone());
    assert_eq!(secret.as_deref(), Some("fleet-wide-secret"));

    // The constructor compiles cleanly when used programmatically —
    // mirrors how AppConfig serializes the field.
    let _ = HooksConfig {
        subscription: Some(HooksSubscriptionConfig {
            hmac_secret: Some("x".into()),
        }),
    };
}

#[test]
fn k7_active_hmac_secret_setter_and_getter_round_trip() {
    let _guard = K7_HMAC_GLOBAL_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    set_active_hooks_hmac_secret(Some("k7-test-secret".into()));
    assert_eq!(
        active_hooks_hmac_secret().as_deref(),
        Some("k7-test-secret")
    );

    set_active_hooks_hmac_secret(None);
    assert_eq!(active_hooks_hmac_secret(), None);
}

// Justification for `#[allow(clippy::await_holding_lock)]` on the two
// async K7 tests: we deliberately hold a `std::sync::Mutex` across
// `.await` points to serialize tests that mutate the process-wide
// `ACTIVE_HOOKS_HMAC_SECRET` static. The lint exists to prevent
// deadlocks when async tasks are scheduled on the same OS thread, but
// these tests run with `flavor = "multi_thread"` and never call back
// into themselves while the guard is held. A `tokio::sync::Mutex` would
// satisfy the lint but would also force the synchronous setter test
// (`k7_active_hmac_secret_setter_and_getter_round_trip`) into an
// async context for no semantic gain.
#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn k7_hmac_signature_header_present_when_global_secret_configured() {
    let _guard = K7_HMAC_GLOBAL_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // ----------------------------------------------------------------
    // Setup: stand up a wiremock POST listener with the K6 ACK
    // responder. Register a subscription pointing at it WITHOUT a
    // per-subscription secret. Configure the K7 server-wide HMAC
    // override and dispatch one event. Assert the recorded request
    // carries a `x-ai-memory-signature: sha256=<hex>` header even
    // though the subscription itself was unsigned.
    // ----------------------------------------------------------------
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/k7-hmac"))
        .respond_with(AckEcho)
        .mount(&server)
        .await;

    let (_keep, db_path) = fresh_db();
    let url = format!("{}/k7-hmac", server.uri());
    let _sub_id = {
        let conn = Connection::open(&db_path).unwrap();
        subscriptions::insert(
            &conn,
            &NewSubscription {
                url: &url,
                events: "*",
                // No per-sub secret — the K7 global override is the
                // only signing key. Pre-K7 this would have produced an
                // unsigned payload.
                secret: None,
                namespace_filter: None,
                agent_filter: None,
                created_by: Some("k7-hmac-test"),
                event_types: None,
            },
        )
        .expect("insert subscription")
    };

    set_active_hooks_hmac_secret(Some("k7-fleet-secret".into()));

    // Dispatch a `memory_store` event. The dispatcher spawns a
    // background thread for the actual HTTP send + retry ladder.
    {
        let conn = Connection::open(&db_path).unwrap();
        subscriptions::dispatch_event(
            &conn,
            "memory_store",
            "memory-k7-hmac",
            "ns-k7",
            None,
            &db_path,
        );
    }

    // Poll the mock server for ~5s for the dispatch thread to land.
    let req: Request = poll_for_first_request(&server).await;

    // The header must be present and shaped `sha256=<64-hex-chars>`.
    let sig_header = req
        .headers
        .get("x-ai-memory-signature")
        .expect("signature header must be present when global secret is set")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        sig_header.starts_with("sha256="),
        "signature header malformed: {sig_header}"
    );
    let hex_part = sig_header.trim_start_matches("sha256=");
    assert_eq!(
        hex_part.len(),
        64,
        "sha256 hex digest must be 64 chars; got {hex_part:?}"
    );
    assert!(
        hex_part.chars().all(|c| c.is_ascii_hexdigit()),
        "sha256 digest must be lowercase hex; got {hex_part:?}"
    );

    // Assert the digest matches the K7 canonical construction:
    // HMAC-SHA256(SHA256(secret), "<timestamp>.<body>"). We
    // reconstruct the canonical string from the recorded request
    // headers (timestamp) + body and the configured secret, then
    // expect bit-for-bit equality.
    let timestamp = req
        .headers
        .get("x-ai-memory-timestamp")
        .expect("timestamp header must be present")
        .to_str()
        .unwrap();
    let body = std::str::from_utf8(&req.body).expect("request body utf8");
    let canonical = format!("{timestamp}.{body}");
    let expected = expected_k7_signature("k7-fleet-secret", &canonical);
    assert_eq!(
        hex_part, expected,
        "K7 server-wide signature must equal HMAC-SHA256(SHA256(secret), \"<ts>.<body>\")"
    );

    // Tear down the override so subsequent tests in the same process
    // don't observe stale state.
    set_active_hooks_hmac_secret(None);
}

#[allow(clippy::await_holding_lock)]
#[tokio::test(flavor = "multi_thread")]
async fn k7_hmac_unset_means_unsigned_payload_when_no_per_sub_secret() {
    let _guard = K7_HMAC_GLOBAL_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Sanity counter-test: when neither the per-subscription secret
    // nor the K7 global override is set, the payload is unsigned
    // (preserves pre-K7 behaviour).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/k7-unsigned"))
        .respond_with(AckEcho)
        .mount(&server)
        .await;

    let (_keep, db_path) = fresh_db();
    let url = format!("{}/k7-unsigned", server.uri());
    let _sub_id = {
        let conn = Connection::open(&db_path).unwrap();
        subscriptions::insert(
            &conn,
            &NewSubscription {
                url: &url,
                events: "*",
                secret: None,
                namespace_filter: None,
                agent_filter: None,
                created_by: Some("k7-unsigned-test"),
                event_types: None,
            },
        )
        .expect("insert subscription")
    };

    set_active_hooks_hmac_secret(None);

    {
        let conn = Connection::open(&db_path).unwrap();
        subscriptions::dispatch_event(
            &conn,
            "memory_store",
            "memory-k7-unsigned",
            "ns-k7",
            None,
            &db_path,
        );
    }

    let req: Request = poll_for_first_request(&server).await;
    assert!(
        req.headers.get("x-ai-memory-signature").is_none(),
        "signature header must be absent when no per-sub secret AND no global override"
    );
}

/// Poll the wiremock server for ~5s waiting for at least one request to
/// land. Panics on timeout so the failure mode is loud.
async fn poll_for_first_request(server: &MockServer) -> Request {
    for _ in 0..50 {
        let received = server.received_requests().await.unwrap_or_default();
        if let Some(req) = received.into_iter().next() {
            return req;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("K7 dispatch thread never reached wiremock");
}

/// Independent reference implementation of the K7 canonical signature
/// — `HMAC-SHA256(SHA256(secret), canonical)` — using `sha2` directly
/// so the test can't accidentally pass by sharing the production
/// `hmac_sha256_hex` with itself.
fn expected_k7_signature(plaintext_secret: &str, canonical: &str) -> String {
    use sha2::{Digest, Sha256};

    // Step 2: RFC 2104 HMAC-SHA256 block size.
    const BLOCK: usize = 64;

    // Step 1: SHA-256 the plaintext secret to produce the keying
    // material the K7 dispatcher passes into the HMAC keyed-hash
    // construction. The dispatcher then hex-decodes that string back
    // to bytes, so we mirror it.
    let key_hex = {
        let mut h = Sha256::new();
        h.update(plaintext_secret.as_bytes());
        format!("{:x}", h.finalize())
    };
    let key_bytes = hex_decode(&key_hex);

    let mut key = if key_bytes.len() > BLOCK {
        let mut h = Sha256::new();
        h.update(&key_bytes);
        h.finalize().to_vec()
    } else {
        key_bytes
    };
    key.resize(BLOCK, 0);
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key[i];
        opad[i] ^= key[i];
    }
    let inner = {
        let mut h = Sha256::new();
        h.update(ipad);
        h.update(canonical.as_bytes());
        h.finalize()
    };
    let outer = {
        let mut h = Sha256::new();
        h.update(opad);
        h.update(inner);
        h.finalize()
    };
    format!("{outer:x}")
}

fn hex_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return s.as_bytes().to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        let hi = match pair[0] {
            b'0'..=b'9' => pair[0] - b'0',
            b'a'..=b'f' => pair[0] - b'a' + 10,
            b'A'..=b'F' => pair[0] - b'A' + 10,
            _ => return s.as_bytes().to_vec(),
        };
        let lo = match pair[1] {
            b'0'..=b'9' => pair[1] - b'0',
            b'a'..=b'f' => pair[1] - b'a' + 10,
            b'A'..=b'F' => pair[1] - b'A' + 10,
            _ => return s.as_bytes().to_vec(),
        };
        out.push((hi << 4) | lo);
    }
    out
}
