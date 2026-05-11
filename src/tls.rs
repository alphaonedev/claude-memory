// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! TLS / mTLS configuration and verifiers for the HTTP daemon.
//!
//! Wave 4 (v0.6.3) — extracted verbatim from `src/main.rs`. Three layers:
//!
//! 1. **Layer 1** — server-side TLS via `axum-server` + rustls.
//!    `load_rustls_config` parses a PEM cert + PEM key (PKCS#8 / RSA / SEC1)
//!    and surfaces operator-friendly errors instead of letting rustls' wrapped
//!    IO errors bubble up. TLS misconfiguration is the #1 new-deploy footgun.
//!
//! 2. **Layer 2** — mTLS with SHA-256 client-cert fingerprint allowlist.
//!    `load_mtls_rustls_config` builds a rustls `ServerConfig` that:
//!      - presents the local cert/key (same as Layer 1),
//!      - demands a client certificate on every connection,
//!      - accepts the client cert only if its SHA-256 fingerprint appears on
//!        the operator-configured allowlist. Any other cert — including ones
//!        signed by trusted CAs — is rejected. This is the fastest path to
//!        "only authorised peers can even connect" without depending on a
//!        PKI/CA ecosystem. Fingerprint pinning is a well-understood primitive
//!        (HTTP Public Key Pinning, SSH host keys).
//!
//!    The allowlist parser tolerates:
//!      - blank lines and `#` full-line comments,
//!      - trailing inline comments (issue #358),
//!      - optional `:` separators in the hex,
//!      - an optional leading `sha256:` marker (forward-compat).
//!    It rejects embedded whitespace inside the hex run (issue #338) so
//!    soft-wrap copy-paste artefacts surface a clear "unexpected character"
//!    error rather than a misleading length error further down.
//!
//! 3. **Layer 2 (client side)** — `build_rustls_client_config` builds a
//!    `rustls::ClientConfig` with client-cert auth and a "dangerously-accept-
//!    any-server-cert" verifier. Used by the sync-daemon to present its
//!    client cert on every outbound request while connecting to peers with
//!    self-signed server certs. Peer authenticity is established on the
//!    other direction (they verify us via `--mtls-allowlist`).
//!
//! Every public symbol below is move-extracted byte-for-byte from `main.rs`
//! at the W3 commit, with `pub` added for cross-module visibility. Behaviour
//! must remain bit-for-bit identical at the call sites.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

/// v0.7.0 H3 — pin the rustls protocol-version floor to TLS 1.2 with TLS 1.3
/// preferred. Listed in descending preference order; rustls negotiates the
/// highest protocol both peers support. TLS 1.0 / 1.1 are deliberately
/// omitted: they have known weaknesses (BEAST, POODLE, no AEAD) and are
/// disabled in every modern client (Chrome ≥ 84, Firefox ≥ 78, Safari ≥ 13).
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&rustls::SupportedProtocolVersion] =
    &[&rustls::version::TLS13, &rustls::version::TLS12];

/// v0.7.0 H4 — emit a `tracing::warn!` when the on-disk TLS key file is
/// world- or group-readable. On Unix, "loose" means
/// `mode & 0o077 != 0` — any bit in the group/world triad is set.
///
/// We intentionally do **not** refuse to load. Operators may have
/// deliberately set up a shared-group keymat layout (e.g. nginx-style
/// `ssl-cert` group), and refusing here would regress those flows.
/// Warning is the right surface: loud in `journalctl`, scrapable by
/// the SIEM, but never blocks startup.
///
/// On non-Unix targets the check is a no-op (Windows ACLs are richer
/// than `st_mode` bits and would warrant a separate audit).
fn warn_if_key_perms_loose(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.mode() & 0o777;
            if mode & 0o077 != 0 {
                tracing::warn!(
                    target: "ai_memory::tls",
                    path = %path.display(),
                    mode = format!("{mode:#o}"),
                    "TLS private key file is group- or world-accessible \
                     (mode {mode:#o}); recommended permissions are 0600. \
                     Loading anyway — operator may have intentional shared-group setup."
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        // Windows uses ACLs, not POSIX modes. A separate audit would be
        // needed to surface "Everyone has Read" — out of v0.7.0 scope.
        let _ = path;
    }
}

/// Load a PEM cert + PEM key (PKCS#8 or RSA) into an `axum-server`
/// rustls config. Returns an error with a specific message for the
/// operator rather than letting rustls' wrapped IO error bubble up —
/// TLS misconfigurations are the #1 new-deploy footgun.
///
/// **v0.7.0 H3** — protocol versions are pinned to TLS 1.3 (preferred)
/// + TLS 1.2 (floor). See [`SUPPORTED_PROTOCOL_VERSIONS`].
///
/// **v0.7.0 H4** — private key file permissions are checked before
/// loading; loose permissions surface as a WARN but do not refuse.
pub async fn load_rustls_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<axum_server::tls_rustls::RustlsConfig> {
    warn_if_key_perms_loose(key_path);
    let cert_pem = tokio::fs::read(cert_path)
        .await
        .with_context(|| format!("failed to read TLS cert from {}", cert_path.display()))?;
    let key_pem = tokio::fs::read(key_path)
        .await
        .with_context(|| format!("failed to read TLS key from {}", key_path.display()))?;

    // v0.7.0 H3 — `RustlsConfig::from_pem` doesn't expose protocol-
    // version pinning. We build a `rustls::ServerConfig` directly with
    // `with_protocol_versions(&[TLS13, TLS12])`, then wrap it for
    // axum_server. Same parser surface, but with the version floor
    // bolted on.
    let certs = rustls_pki_pem_iter_certs(&cert_pem)?;
    let key = rustls_pki_pem_parse_private_key(&key_pem)?;
    let server_config =
        rustls::ServerConfig::builder_with_protocol_versions(SUPPORTED_PROTOCOL_VERSIONS)
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context(
                "failed to build rustls ServerConfig — ensure PEM-encoded (cert may be fullchain; \
         key must be PKCS#8 or RSA)",
            )?;
    Ok(axum_server::tls_rustls::RustlsConfig::from_config(
        Arc::new(server_config),
    ))
}

// ---------------------------------------------------------------------------
// Layer 2 — mTLS with SHA-256 fingerprint allowlist.
// ---------------------------------------------------------------------------

/// Load a rustls server config with client-cert-fingerprint verification.
pub async fn load_mtls_rustls_config(
    cert_path: &Path,
    key_path: &Path,
    allowlist_path: &Path,
) -> Result<axum_server::tls_rustls::RustlsConfig> {
    let allowlist = load_fingerprint_allowlist(allowlist_path).await?;
    if allowlist.is_empty() {
        anyhow::bail!(
            "mTLS allowlist at {} is empty — refuse to start rather than silently accept all peers",
            allowlist_path.display()
        );
    }

    warn_if_key_perms_loose(key_path);
    let cert_pem = tokio::fs::read(cert_path)
        .await
        .with_context(|| format!("failed to read TLS cert from {}", cert_path.display()))?;
    let key_pem = tokio::fs::read(key_path)
        .await
        .with_context(|| format!("failed to read TLS key from {}", key_path.display()))?;

    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pki_pem_iter_certs(&cert_pem)?;
    let key = rustls_pki_pem_parse_private_key(&key_pem)?;

    let verifier = Arc::new(FingerprintAllowlistVerifier { allowlist });
    // v0.7.0 H3 — same protocol-version pinning as the non-mTLS server
    // config above. TLS 1.3 preferred, TLS 1.2 floor.
    let server_config =
        rustls::ServerConfig::builder_with_protocol_versions(SUPPORTED_PROTOCOL_VERSIONS)
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .context("failed to build rustls ServerConfig for mTLS")?;

    Ok(axum_server::tls_rustls::RustlsConfig::from_config(
        Arc::new(server_config),
    ))
}

/// Parse the allowlist file: one SHA-256 fingerprint per line, case-insensitive
/// hex with optional `:` separators. Empty lines and `#` comments are skipped.
pub async fn load_fingerprint_allowlist(path: &Path) -> Result<HashSet<[u8; 32]>> {
    let text = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read mTLS allowlist from {}", path.display()))?;
    let mut set = HashSet::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Issue #358: tolerate inline trailing comments — anything after `#`
        // on a non-comment line is dropped before the strict hex/colon
        // validation below. Safe because `#` is not a valid hex/colon char,
        // so it cannot appear in a legitimate SHA-256 fingerprint.
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        // Accept a leading `sha256:` marker for forward-compat with richer formats.
        let hex_part = line.strip_prefix("sha256:").unwrap_or(line);
        // Ultrareview #338: reject any non-hex, non-colon character —
        // including embedded whitespace/tabs. Previously the parser
        // stripped only `:` and relied on the length check to catch
        // whitespace, but silent acceptance of copy-paste artefacts
        // (e.g. soft-wraps producing internal spaces) would produce
        // misleading parse errors further down rather than a clear
        // "whitespace not allowed" signal. Keep it strict.
        if let Some(bad) = hex_part
            .chars()
            .find(|c| !c.is_ascii_hexdigit() && *c != ':')
        {
            anyhow::bail!(
                "mTLS allowlist line {}: unexpected character {:?} — \
                 entries must be 64 hex chars with optional `:` separators",
                lineno + 1,
                bad
            );
        }
        let hex_clean: String = hex_part.chars().filter(|c| *c != ':').collect();
        if hex_clean.len() != 64 {
            anyhow::bail!(
                "mTLS allowlist line {}: expected 64 hex chars (optionally with `:` separators), got {}",
                lineno + 1,
                hex_clean.len()
            );
        }
        let mut bytes = [0u8; 32];
        for i in 0..32 {
            bytes[i] = u8::from_str_radix(&hex_clean[i * 2..i * 2 + 2], 16)
                .with_context(|| format!("mTLS allowlist line {}: invalid hex", lineno + 1))?;
        }
        set.insert(bytes);
    }
    Ok(set)
}

pub fn rustls_pki_pem_iter_certs(
    pem: &[u8],
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    use rustls::pki_types::pem::PemObject as _;
    let mut cursor = std::io::Cursor::new(pem);
    let certs: Vec<_> = rustls::pki_types::CertificateDer::pem_reader_iter(&mut cursor)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("failed to parse TLS cert PEM")?;
    if certs.is_empty() {
        anyhow::bail!("TLS cert PEM contained no certificates");
    }
    Ok(certs)
}

pub fn rustls_pki_pem_parse_private_key(
    pem: &[u8],
) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    use rustls::pki_types::pem::PemObject as _;
    let mut cursor = std::io::Cursor::new(pem);
    let key = rustls::pki_types::PrivateKeyDer::from_pem_reader(&mut cursor)
        .context("failed to parse TLS key PEM — expected PKCS#8, RSA, or SEC1")?;
    Ok(key)
}

/// Custom `ClientCertVerifier` that accepts only client certs whose SHA-256
/// DER fingerprint is on the allowlist. Ignores CA chain — fingerprint
/// pinning is the trust anchor here, same model as SSH `known_hosts`.
#[derive(Debug)]
pub struct FingerprintAllowlistVerifier {
    pub allowlist: HashSet<[u8; 32]>,
}

impl rustls::server::danger::ClientCertVerifier for FingerprintAllowlistVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        use sha2::{Digest, Sha256};
        let fp: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if allowlist_contains_ct(&self.allowlist, &fp) {
            Ok(rustls::server::danger::ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "client cert fingerprint {} not in mTLS allowlist",
                hex_short(&fp)
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub fn hex_short(fp: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(12);
    for b in &fp[..6] {
        let _ = write!(s, "{b:02x}");
    }
    s.push('…');
    s
}

/// v0.7.0 M1 — constant-time allowlist membership check.
///
/// `HashSet::contains` is O(1) but the SipHash probe + early-exit
/// comparison both leak timing signal: the response time of a verify
/// handshake correlates with whether the offered fingerprint hash-
/// collides with any allowlist entry (and, on collision, how many
/// bytes match). A remote attacker who can observe TLS handshake
/// timing can in principle enumerate the allowlist that way.
///
/// We walk every entry in the allowlist on every call and XOR-fold
/// each byte through `subtle::ConstantTimeEq`. The result is the
/// OR-reduction of "this entry matched" across every entry — same
/// per-call cost regardless of whether a match exists or where in
/// the iteration order it sits. `subtle` is the RustCrypto-default
/// constant-time primitive (used by ring, ed25519-dalek, etc.).
///
/// Cost is O(N · 32) bytes per handshake. With a 1000-entry
/// allowlist that's 32 KB of memory comparison — well below the
/// dozens of milliseconds of cryptographic handshake work that
/// precedes it. The timing-attack threat dominates the perf cost.
fn allowlist_contains_ct(allowlist: &HashSet<[u8; 32]>, fp: &[u8; 32]) -> bool {
    use subtle::ConstantTimeEq as _;
    let mut found: subtle::Choice = subtle::Choice::from(0);
    for entry in allowlist {
        // `ct_eq` returns a `Choice` (0 or 1) without branching on
        // the comparison outcome — the inner XOR-fold runs the full
        // 32 bytes every call.
        found |= entry.ct_eq(fp);
    }
    bool::from(found)
}

/// Build a rustls `ClientConfig` with client-cert auth and a
/// "dangerously-accept-any-server-cert" verifier. Used by the
/// sync-daemon to present its client cert on every outbound request
/// while connecting to peers with self-signed server certs. Peer
/// authenticity is established on the other direction (they verify
/// us via `--mtls-allowlist`).
pub async fn build_rustls_client_config(
    cert_path: &Path,
    key_path: &Path,
) -> Result<rustls::ClientConfig> {
    warn_if_key_perms_loose(key_path);
    let cert_pem = tokio::fs::read(cert_path)
        .await
        .with_context(|| format!("failed to read client cert from {}", cert_path.display()))?;
    let key_pem = tokio::fs::read(key_path)
        .await
        .with_context(|| format!("failed to read client key from {}", key_path.display()))?;

    let certs = rustls_pki_pem_iter_certs(&cert_pem)?;
    let key = rustls_pki_pem_parse_private_key(&key_pem)?;

    // SAFETY: we accept any server cert because the server authenticates
    // US via our client cert fingerprint (Layer 2's trust anchor), not
    // via server-cert validation. Server-cert pinning is a Layer 2b
    // refinement tracked in #224.
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(DangerousAnyServerVerifier))
        .with_client_auth_cert(certs, key)
        .context("failed to build rustls ClientConfig with client cert")?;
    Ok(config)
}

/// `ServerCertVerifier` that accepts any peer certificate. Safe ONLY when
/// paired with a strong reverse authentication channel — in our case the
/// peer's `--mtls-allowlist` fingerprint-pins our client cert.
#[derive(Debug)]
pub struct DangerousAnyServerVerifier;

impl rustls::client::danger::ServerCertVerifier for DangerousAnyServerVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ---------------------------------------------------------------------------
// Unit tests — pure-function and verifier coverage. Integration tests
// (anything requiring on-disk PEM fixtures end-to-end) live in
// `tests/tls_integration.rs` so the bin's compile time stays small.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use rustls::server::danger::ClientCertVerifier;

    /// Convenience: write `body` to a temp file and return the temp file
    /// (kept so the caller can `tmp.path()`).
    fn write_tmp(body: &str) -> tempfile::NamedTempFile {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), body).unwrap();
        tmp
    }

    // -----------------------------------------------------------------------
    // Allowlist parser
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_allowlist_empty_file_errors() {
        // An empty allowlist file produces an empty set. The "refuse to
        // start" check lives in `load_mtls_rustls_config`, not the parser
        // — so the parser succeeds with zero entries.
        let tmp = write_tmp("");
        let set = load_fingerprint_allowlist(tmp.path()).await.unwrap();
        assert!(set.is_empty());
    }

    #[tokio::test]
    async fn test_allowlist_only_comments_errors() {
        // Comment-only file should likewise produce an empty set; the
        // empty-allowlist guard is enforced one layer up.
        let tmp = write_tmp("# header\n# more\n  # indented\n");
        let set = load_fingerprint_allowlist(tmp.path()).await.unwrap();
        assert!(set.is_empty());
    }

    #[tokio::test]
    async fn test_allowlist_single_valid_fp() {
        let fp = "a".repeat(64);
        let tmp = write_tmp(&format!("{fp}\n"));
        let set = load_fingerprint_allowlist(tmp.path()).await.unwrap();
        assert_eq!(set.len(), 1);
        assert!(set.contains(&[0xaa; 32]));
    }

    #[tokio::test]
    async fn test_allowlist_with_colons() {
        let fp = format!("{}:{}", "b".repeat(32), "b".repeat(32));
        let tmp = write_tmp(&format!("{fp}\n"));
        let set = load_fingerprint_allowlist(tmp.path()).await.unwrap();
        assert_eq!(set.len(), 1);
        assert!(set.contains(&[0xbb; 32]));
    }

    #[tokio::test]
    async fn test_allowlist_sha256_prefix() {
        let fp = format!("sha256:{}", "c".repeat(64));
        let tmp = write_tmp(&format!("{fp}\n"));
        let set = load_fingerprint_allowlist(tmp.path()).await.unwrap();
        assert_eq!(set.len(), 1);
        assert!(set.contains(&[0xcc; 32]));
    }

    /// Issue #358 — trailing inline comment after a fingerprint must parse.
    #[tokio::test]
    async fn test_allowlist_inline_comment() {
        let fp = "d".repeat(64);
        let body = format!("{fp}  # node-1 mTLS\n");
        let tmp = write_tmp(&body);
        let set = load_fingerprint_allowlist(tmp.path()).await.unwrap();
        assert_eq!(set.len(), 1);
        assert!(set.contains(&[0xdd; 32]));
    }

    #[tokio::test]
    async fn test_allowlist_too_short_errors() {
        let tmp = write_tmp(&"a".repeat(63));
        let err = load_fingerprint_allowlist(tmp.path()).await.unwrap_err();
        assert!(
            err.to_string().contains("expected 64 hex chars"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_allowlist_too_long_errors() {
        let tmp = write_tmp(&"a".repeat(65));
        let err = load_fingerprint_allowlist(tmp.path()).await.unwrap_err();
        assert!(
            err.to_string().contains("expected 64 hex chars"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_allowlist_invalid_hex_errors() {
        // 64 chars, but `z` is non-hex → must hit the strict char check.
        let mut s = "a".repeat(63);
        s.push('z');
        let tmp = write_tmp(&s);
        let err = load_fingerprint_allowlist(tmp.path()).await.unwrap_err();
        assert!(
            err.to_string().contains("unexpected character"),
            "got: {err}"
        );
    }

    /// Issue #338 — embedded whitespace inside the hex run must error
    /// with "unexpected character", not silently get stripped.
    #[tokio::test]
    async fn test_allowlist_embedded_whitespace_errors() {
        let body = format!("{} {}\n", "a".repeat(32), "a".repeat(32));
        let tmp = write_tmp(&body);
        let err = load_fingerprint_allowlist(tmp.path()).await.unwrap_err();
        assert!(
            err.to_string().contains("unexpected character"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_allowlist_tab_in_hex_errors() {
        let body = format!("{}\t{}\n", "a".repeat(32), "a".repeat(32));
        let tmp = write_tmp(&body);
        let err = load_fingerprint_allowlist(tmp.path()).await.unwrap_err();
        assert!(
            err.to_string().contains("unexpected character"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn test_allowlist_blank_lines_skipped() {
        let fp = "a".repeat(64);
        let body = format!("\n\n  \n{fp}\n\n   \n");
        let tmp = write_tmp(&body);
        let set = load_fingerprint_allowlist(tmp.path()).await.unwrap();
        assert_eq!(set.len(), 1);
    }

    #[tokio::test]
    async fn test_allowlist_multiple_entries() {
        let fp_a = "a".repeat(64);
        let fp_b = "b".repeat(64);
        let fp_c = format!("{}:{}", "c".repeat(32), "c".repeat(32));
        let body = format!(
            "# header\n\
             {fp_a}\n\
             sha256:{fp_b}\n\
             {fp_c}\n"
        );
        let tmp = write_tmp(&body);
        let set = load_fingerprint_allowlist(tmp.path()).await.unwrap();
        assert_eq!(set.len(), 3);
        assert!(set.contains(&[0xaa; 32]));
        assert!(set.contains(&[0xbb; 32]));
        assert!(set.contains(&[0xcc; 32]));
    }

    #[tokio::test]
    async fn test_allowlist_duplicate_entries_dedup() {
        let fp = "e".repeat(64);
        let body = format!("{fp}\n{fp}\n{fp}\n");
        let tmp = write_tmp(&body);
        let set = load_fingerprint_allowlist(tmp.path()).await.unwrap();
        // HashSet collapses dupes — exactly one fingerprint registered.
        assert_eq!(set.len(), 1);
        assert!(set.contains(&[0xee; 32]));
    }

    // -----------------------------------------------------------------------
    // PEM parsers
    // -----------------------------------------------------------------------

    #[test]
    fn test_pem_iter_certs_empty_errors() {
        let err = rustls_pki_pem_iter_certs(b"").unwrap_err();
        // No certs at all → either parse-error or "contained no certificates".
        // The empty input is not a parse failure, it's just zero certs.
        assert!(
            err.to_string().contains("no certificates")
                || err.to_string().contains("failed to parse"),
            "got: {err}"
        );
    }

    #[test]
    fn test_pem_iter_certs_garbage_errors() {
        let err = rustls_pki_pem_iter_certs(b"not a pem file\n").unwrap_err();
        assert!(
            err.to_string().contains("no certificates")
                || err.to_string().contains("failed to parse"),
            "got: {err}"
        );
    }

    #[test]
    fn test_pem_iter_certs_single_cert() {
        let pem = std::fs::read("tests/fixtures/tls/valid_cert.pem")
            .expect("regenerate fixtures via tests/fixtures/tls/regenerate.sh");
        let certs = rustls_pki_pem_iter_certs(&pem).unwrap();
        assert_eq!(
            certs.len(),
            1,
            "expected exactly one cert in valid_cert.pem"
        );
    }

    #[test]
    fn test_pem_iter_certs_chain() {
        let pem = std::fs::read("tests/fixtures/tls/cert_chain.pem")
            .expect("regenerate fixtures via tests/fixtures/tls/regenerate.sh");
        let certs = rustls_pki_pem_iter_certs(&pem).unwrap();
        assert!(
            certs.len() >= 2,
            "expected leaf + intermediate, got {}",
            certs.len()
        );
    }

    #[test]
    fn test_pem_parse_pkcs8_key() {
        let pem = std::fs::read("tests/fixtures/tls/valid_key_pkcs8.pem")
            .expect("regenerate fixtures via tests/fixtures/tls/regenerate.sh");
        let key = rustls_pki_pem_parse_private_key(&pem).unwrap();
        // PKCS#8 envelopes RSA / ECDSA / Ed25519. The discriminant tells us
        // rustls picked the right branch — any PrivateKeyDer variant is fine.
        let _ = key;
    }

    #[test]
    fn test_pem_parse_rsa_key() {
        let pem = std::fs::read("tests/fixtures/tls/valid_key_rsa.pem")
            .expect("regenerate fixtures via tests/fixtures/tls/regenerate.sh");
        let key = rustls_pki_pem_parse_private_key(&pem).unwrap();
        let _ = key;
    }

    #[test]
    fn test_pem_parse_sec1_key() {
        let pem = std::fs::read("tests/fixtures/tls/valid_key_sec1.pem")
            .expect("regenerate fixtures via tests/fixtures/tls/regenerate.sh");
        let key = rustls_pki_pem_parse_private_key(&pem).unwrap();
        let _ = key;
    }

    #[test]
    fn test_pem_parse_garbage_errors() {
        let err = rustls_pki_pem_parse_private_key(b"not a pem file\n").unwrap_err();
        assert!(err.to_string().contains("failed to parse TLS key PEM"));
    }

    // -----------------------------------------------------------------------
    // hex_short
    // -----------------------------------------------------------------------

    #[test]
    fn test_hex_short_format() {
        // 6 bytes prefix → 12 hex chars + ellipsis.
        let mut fp = [0u8; 32];
        fp[0] = 0xde;
        fp[1] = 0xad;
        fp[2] = 0xbe;
        fp[3] = 0xef;
        fp[4] = 0x12;
        fp[5] = 0x34;
        // Bytes 6..32 must NOT appear in the output.
        for (i, slot) in fp.iter_mut().enumerate().skip(6) {
            *slot = (i as u8).wrapping_mul(7);
        }
        assert_eq!(hex_short(&fp), "deadbeef1234…");
    }

    #[test]
    fn test_hex_short_truncates_to_6_bytes() {
        let fp = [0xff; 32];
        let s = hex_short(&fp);
        // Strip the trailing ellipsis (`…` is 3 bytes in UTF-8).
        let hex_only = s.trim_end_matches('…');
        assert_eq!(hex_only.len(), 12, "expected 6 bytes = 12 hex chars");
        assert_eq!(hex_only, "ffffffffffff");
    }

    // -----------------------------------------------------------------------
    // FingerprintAllowlistVerifier
    // -----------------------------------------------------------------------

    #[test]
    fn test_verifier_accepts_allowlisted_fp() {
        use sha2::{Digest, Sha256};
        // Synthesize a "cert" — the verifier doesn't validate ASN.1 here,
        // only hashes the DER bytes. Any byte slice works; we just need
        // the fingerprint and the cert bytes to match.
        let fake_cert = b"fake certificate DER bytes for fingerprint test";
        let fp: [u8; 32] = Sha256::digest(fake_cert).into();
        let mut allowlist = HashSet::new();
        allowlist.insert(fp);
        let verifier = FingerprintAllowlistVerifier { allowlist };
        let cert = rustls::pki_types::CertificateDer::from(fake_cert.to_vec());
        let now = rustls::pki_types::UnixTime::now();
        let result = verifier.verify_client_cert(&cert, &[], now);
        assert!(result.is_ok(), "expected accept, got: {result:?}");
    }

    #[test]
    fn test_verifier_rejects_unknown_fp() {
        let allowlist = HashSet::new();
        let verifier = FingerprintAllowlistVerifier { allowlist };
        let cert = rustls::pki_types::CertificateDer::from(b"unknown".to_vec());
        let now = rustls::pki_types::UnixTime::now();
        let err = verifier.verify_client_cert(&cert, &[], now).unwrap_err();
        assert!(
            err.to_string().contains("not in mTLS allowlist"),
            "got: {err}"
        );
    }

    #[test]
    fn test_verifier_error_includes_truncated_fp() {
        let allowlist = HashSet::new();
        let verifier = FingerprintAllowlistVerifier { allowlist };
        let cert_bytes = b"some cert that won't be in the allowlist";
        let cert = rustls::pki_types::CertificateDer::from(cert_bytes.to_vec());
        let now = rustls::pki_types::UnixTime::now();
        let err = verifier.verify_client_cert(&cert, &[], now).unwrap_err();
        let msg = err.to_string();
        // Compute the expected truncated fp prefix and assert it's present.
        use sha2::{Digest, Sha256};
        let fp: [u8; 32] = Sha256::digest(cert_bytes).into();
        let short = hex_short(&fp);
        assert!(msg.contains(&short), "expected fp {short} in: {msg}");
        // And the trailing `…` must be there — the fp must be truncated,
        // not full-length.
        assert!(msg.contains('…'), "expected truncation marker in: {msg}");
    }

    #[test]
    fn test_verifier_offer_client_auth_returns_true() {
        let verifier = FingerprintAllowlistVerifier {
            allowlist: HashSet::new(),
        };
        assert!(verifier.offer_client_auth());
    }

    #[test]
    fn test_verifier_client_auth_mandatory_returns_true() {
        let verifier = FingerprintAllowlistVerifier {
            allowlist: HashSet::new(),
        };
        assert!(verifier.client_auth_mandatory());
        // Also exercise root_hint_subjects — it's a one-line getter that
        // would otherwise sit at zero coverage.
        assert_eq!(verifier.root_hint_subjects().len(), 0);
    }

    /// Build a bogus `DigitallySignedStruct` from the on-the-wire byte
    /// format: 2-byte big-endian scheme + 2-byte big-endian signature
    /// length + N signature bytes. `DigitallySignedStruct::new` is
    /// crate-private in rustls 0.23, but the wire decoder is reachable
    /// through `rustls::internal::msgs::codec::{Codec, Reader}`.
    fn bogus_dss() -> rustls::DigitallySignedStruct {
        use rustls::internal::msgs::codec::{Codec, Reader};
        // ED25519 = 0x0807. Sig length = 0x0040 (64). Then 64 zero bytes.
        let mut wire = Vec::with_capacity(4 + 64);
        wire.extend_from_slice(&[0x08, 0x07]);
        wire.extend_from_slice(&[0x00, 0x40]);
        wire.extend_from_slice(&[0u8; 64]);
        let mut reader = Reader::init(&wire);
        rustls::DigitallySignedStruct::read(&mut reader)
            .expect("hand-rolled wire bytes must round-trip the Codec")
    }

    /// Exercise the rustls `verify_tls{12,13}_signature` + `supported_verify_schemes`
    /// trait methods on `FingerprintAllowlistVerifier`. We feed them a
    /// deliberately invalid signature so the underlying ring-backed
    /// verifier returns Err — that's fine, the test only asserts the
    /// method runs to completion (covers the body) without panicking.
    #[test]
    fn test_verifier_signature_methods_run() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let verifier = FingerprintAllowlistVerifier {
            allowlist: HashSet::new(),
        };
        // supported_verify_schemes is pure — must return non-empty.
        let schemes = verifier.supported_verify_schemes();
        assert!(
            !schemes.is_empty(),
            "ring provider must expose at least one signature scheme"
        );

        // verify_tls{12,13}_signature: feed bogus inputs and expect Err.
        let cert = rustls::pki_types::CertificateDer::from(vec![0u8; 32]);
        let dss = bogus_dss();
        let _ = verifier.verify_tls12_signature(b"bogus message", &cert, &dss);
        let _ = verifier.verify_tls13_signature(b"bogus message", &cert, &dss);
    }

    // -----------------------------------------------------------------------
    // DangerousAnyServerVerifier — the sync-daemon's client-side verifier.
    // verify_server_cert always Ok; the signature methods delegate to the
    // ring provider exactly like the server-side verifier above.
    // -----------------------------------------------------------------------

    #[test]
    fn test_dangerous_any_server_verifier_accepts_any_cert() {
        use rustls::client::danger::ServerCertVerifier;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let verifier = DangerousAnyServerVerifier;
        let cert = rustls::pki_types::CertificateDer::from(b"any bytes here".to_vec());
        let server_name = rustls::pki_types::ServerName::try_from("example.com").unwrap();
        let now = rustls::pki_types::UnixTime::now();
        let result = verifier.verify_server_cert(&cert, &[], &server_name, &[], now);
        assert!(
            result.is_ok(),
            "DangerousAnyServerVerifier accepts any cert (compensating mTLS control)"
        );
    }

    #[test]
    fn test_dangerous_any_server_verifier_signature_methods_run() {
        use rustls::client::danger::ServerCertVerifier;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let verifier = DangerousAnyServerVerifier;
        let schemes = verifier.supported_verify_schemes();
        assert!(!schemes.is_empty());

        let cert = rustls::pki_types::CertificateDer::from(vec![0u8; 32]);
        let dss = bogus_dss();
        let _ = verifier.verify_tls12_signature(b"bogus message", &cert, &dss);
        let _ = verifier.verify_tls13_signature(b"bogus message", &cert, &dss);
    }

    // -----------------------------------------------------------------------
    // build_rustls_client_config — exercises the sync-daemon's outbound
    // TLS config path. Covers both the happy and the missing-cert error
    // paths so the entire function body is reached by unit tests.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_build_rustls_client_config_happy_path() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cert = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tls/valid_cert.pem");
        let key = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tls/valid_key_pkcs8.pem");
        let config = build_rustls_client_config(&cert, &key)
            .await
            .expect("client config build with valid cert+key");
        // The returned ClientConfig is opaque; if the ?-cascade above
        // returned Ok, every parser branch and the builder ran.
        drop(config);
    }

    // -----------------------------------------------------------------------
    // H3 — TLS version pinning. Both server configs MUST negotiate only
    // TLS 1.2 or TLS 1.3; legacy versions are off the table.
    // -----------------------------------------------------------------------

    #[test]
    fn test_supported_protocol_versions_pinned_to_tls12_and_tls13() {
        // The exported constant must list exactly TLS 1.3 (preferred) and
        // TLS 1.2 (floor) in that order. If a future rustls upgrade adds
        // a fourth `SupportedProtocolVersion` we want this test to fail
        // so the H3 review surfaces the change.
        assert_eq!(
            SUPPORTED_PROTOCOL_VERSIONS.len(),
            2,
            "expected exactly 2 pinned versions (TLS 1.3 + TLS 1.2)"
        );
        // rustls's `SupportedProtocolVersion::version` exposes the
        // wire-level `ProtocolVersion` enum. TLS 1.3 = 0x0304,
        // TLS 1.2 = 0x0303 (per RFC 8446 §4.1.2 / RFC 5246 §A.1).
        let v0 = SUPPORTED_PROTOCOL_VERSIONS[0].version;
        let v1 = SUPPORTED_PROTOCOL_VERSIONS[1].version;
        assert_eq!(v0, rustls::ProtocolVersion::TLSv1_3, "TLS 1.3 preferred");
        assert_eq!(v1, rustls::ProtocolVersion::TLSv1_2, "TLS 1.2 floor");
    }

    #[tokio::test]
    async fn test_load_rustls_config_pins_tls13_and_tls12() {
        // End-to-end: build a real ServerConfig via the production
        // helper and assert it accepts ONLY TLS 1.2 + TLS 1.3.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cert = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tls/valid_cert.pem");
        let key = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tls/valid_key_pkcs8.pem");

        // `rustls::ServerConfig`'s `versions` field is private in 0.23+,
        // so we assert version pinning at the input layer (the
        // `SUPPORTED_PROTOCOL_VERSIONS` constant the production builder
        // consumes) and rely on the test above
        // (`test_supported_protocol_versions_pinned_to_tls12_and_tls13`)
        // for the strict version-list assertion. Here we just confirm
        // the production async path consumes that constant successfully.
        let _config = load_rustls_config(&cert, &key)
            .await
            .expect("load_rustls_config must succeed with valid fixtures");

        // And exercise the mTLS path's protocol pinning by building a
        // FingerprintAllowlistVerifier + ServerConfig with the same
        // version-list input the production builder uses. A successful
        // build is sufficient — rustls refuses to construct a
        // ServerConfig if the version list is empty or malformed.
        let cert_pem = std::fs::read(&cert).unwrap();
        let key_pem = std::fs::read(&key).unwrap();
        let certs = rustls_pki_pem_iter_certs(&cert_pem).unwrap();
        let signing_key = rustls_pki_pem_parse_private_key(&key_pem).unwrap();
        let _server_config =
            rustls::ServerConfig::builder_with_protocol_versions(SUPPORTED_PROTOCOL_VERSIONS)
                .with_no_client_auth()
                .with_single_cert(certs, signing_key)
                .expect("ServerConfig with pinned versions must build");
    }

    // -----------------------------------------------------------------------
    // H4 — loose-permission warning. The check is best-effort + WARN-only
    // by design; we exercise the path on Unix where it has observable
    // semantics, and confirm it's a no-op when permissions are tight.
    // -----------------------------------------------------------------------

    /// Shared `MakeWriter` shim for the H4 WARN-capture tests. Uses an
    /// `Arc<Mutex<Vec<u8>>>` so the test can inspect every byte the
    /// subscriber emitted after the WARN call. Defined outside the
    /// per-test fn so the `MakeWriter` impl is namespace-stable.
    #[cfg(unix)]
    #[derive(Clone, Default)]
    struct WarnBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    #[cfg(unix)]
    impl std::io::Write for WarnBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[cfg(unix)]
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for WarnBuf {
        type Writer = WarnBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_warn_if_key_perms_loose_emits_warn_on_world_readable() {
        use std::os::unix::fs::PermissionsExt as _;
        use tracing::Level;

        let sink = WarnBuf::default();
        let buf = sink.0.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(Level::WARN)
            .with_writer(sink)
            .without_time()
            .finish();

        let key = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(key.path(), b"dummy keymat").unwrap();
        std::fs::set_permissions(key.path(), std::fs::Permissions::from_mode(0o644)).unwrap();

        tracing::subscriber::with_default(subscriber, || {
            warn_if_key_perms_loose(key.path());
        });

        let captured = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            captured.contains("group- or world-accessible"),
            "expected WARN about loose perms, got: {captured:?}"
        );
        assert!(
            captured.contains("0600"),
            "expected guidance pointer to 0600 in WARN, got: {captured:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_warn_if_key_perms_loose_silent_on_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        use tracing::Level;

        let sink = WarnBuf::default();
        let buf = sink.0.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(Level::WARN)
            .with_writer(sink)
            .without_time()
            .finish();

        let key = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(key.path(), b"dummy keymat").unwrap();
        std::fs::set_permissions(key.path(), std::fs::Permissions::from_mode(0o600)).unwrap();

        tracing::subscriber::with_default(subscriber, || {
            warn_if_key_perms_loose(key.path());
        });

        let captured = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(
            !captured.contains("group- or world-accessible"),
            "0600 perms must NOT trigger the WARN; got: {captured:?}"
        );
    }

    // -----------------------------------------------------------------------
    // M1 — constant-time allowlist membership. We can't assert timing
    // directly in a unit test (jitter / scheduler noise), but we can
    // assert the correctness of the function on a populated allowlist
    // and on a near-miss (single-byte difference) to confirm the
    // XOR-fold runs the full 32 bytes before reporting.
    // -----------------------------------------------------------------------

    #[test]
    fn test_allowlist_contains_ct_matches_real_entry() {
        let mut allowlist = HashSet::new();
        allowlist.insert([0xaa; 32]);
        allowlist.insert([0xbb; 32]);
        allowlist.insert([0xcc; 32]);
        assert!(allowlist_contains_ct(&allowlist, &[0xbb; 32]));
    }

    #[test]
    fn test_allowlist_contains_ct_rejects_one_byte_off() {
        let mut allowlist = HashSet::new();
        allowlist.insert([0xaa; 32]);
        let mut near = [0xaa; 32];
        near[31] = 0xab; // single-byte flip
        assert!(!allowlist_contains_ct(&allowlist, &near));
    }

    #[test]
    fn test_allowlist_contains_ct_empty_allowlist_rejects() {
        let allowlist = HashSet::new();
        assert!(!allowlist_contains_ct(&allowlist, &[0u8; 32]));
    }

    #[tokio::test]
    async fn test_build_rustls_client_config_missing_cert_errors() {
        let cert = std::path::PathBuf::from("/does/not/exist/cert.pem");
        let key = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tls/valid_key_pkcs8.pem");
        let err = build_rustls_client_config(&cert, &key)
            .await
            .expect_err("missing client cert must error");
        assert!(
            err.to_string().contains("failed to read client cert"),
            "got: {err}"
        );
    }
}
