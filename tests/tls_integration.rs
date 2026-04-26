// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `ai_memory::tls`. Exercises the on-disk PEM /
//! allowlist load paths end-to-end, including the operator-friendly error
//! messages around missing-file and empty-allowlist failure modes.
//!
//! Fixtures live under `tests/fixtures/tls/` and are regenerated via
//! `tests/fixtures/tls/regenerate.sh`. The cert/key files are produced
//! by `examples/gen_tls_fixtures.rs` (rcgen 0.14.7); the allowlist `.txt`
//! files are hand-authored.

use ai_memory::tls;
use std::path::PathBuf;

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tls")
        .join(rel)
}

#[tokio::test]
async fn test_load_rustls_config_with_valid_pem() {
    let cert = fixture("valid_cert.pem");
    let key = fixture("valid_key_pkcs8.pem");
    // Install ring as the default provider — required by rustls 0.23 the
    // first time a config is built. Ignored if already installed; the
    // config builder is what actually exercises the production path.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = tls::load_rustls_config(&cert, &key)
        .await
        .expect("load_rustls_config with rcgen-generated leaf cert + PKCS#8 key");
    // The returned RustlsConfig is opaque; if the ?-cascade above
    // returned Ok, every parser branch and the from_pem builder ran
    // successfully — that's the production path under test here.
    drop(config);
}

#[tokio::test]
async fn test_load_rustls_config_missing_cert_file_error_mentions_path() {
    let cert = fixture("does_not_exist.pem");
    let key = fixture("valid_key_pkcs8.pem");
    let err = tls::load_rustls_config(&cert, &key)
        .await
        .expect_err("missing cert file must error");
    let msg = err.to_string();
    assert!(
        msg.contains("failed to read TLS cert"),
        "expected operator-friendly cert read error, got: {msg}"
    );
    // The path must appear in the error so the operator can debug — this
    // is the whole reason the production code uses `with_context`.
    assert!(
        msg.contains("does_not_exist.pem")
            || err
                .chain()
                .any(|c| c.to_string().contains("does_not_exist.pem")),
        "expected path in error chain, got: {msg}"
    );
}

#[tokio::test]
async fn test_load_mtls_rustls_config_empty_allowlist_bails() {
    let cert = fixture("valid_cert.pem");
    let key = fixture("valid_key_pkcs8.pem");
    let allowlist = fixture("allowlist_empty.txt");
    let _ = rustls::crypto::ring::default_provider().install_default();
    let err = tls::load_mtls_rustls_config(&cert, &key, &allowlist)
        .await
        .expect_err("empty allowlist must refuse to start");
    let msg = err.to_string();
    assert!(
        msg.contains("empty"),
        "expected 'empty allowlist' error, got: {msg}"
    );
    assert!(
        msg.contains("refuse to start"),
        "expected 'refuse to start' wording, got: {msg}"
    );
}
