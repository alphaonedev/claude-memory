// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// TLS test fixture generator — Wave 4 (v0.6.3).
//
// Emits the canonical PEM corpus the `tls` module's tests assert against.
// Run via `tests/fixtures/tls/regenerate.sh`. Outputs land in
// `tests/fixtures/tls/`. The example is deterministic only insofar as
// rcgen's underlying RNG accepts a seed — the generated certs change
// between regenerations because rcgen 0.14 uses `ring::rand::SystemRandom`
// internally with no public seed knob. The fixtures are committed
// verbatim, so production builds never regenerate them; the script exists
// so a future change to rcgen output format can be applied uniformly.
//
// Files emitted:
//   - valid_cert.pem       — single self-signed leaf cert
//   - valid_key_pkcs8.pem  — PKCS#8-wrapped private key (rcgen native)
//   - valid_key_rsa.pem    — RSA private key (PKCS#1) re-encoded from PKCS#8
//   - valid_key_sec1.pem   — SEC1 EC private key (P-256)
//   - cert_chain.pem       — leaf + self-signed "intermediate"
//   - garbage.pem          — literal "not a pem file\n"
//   - valid_cert_sha256.txt — SHA-256 of valid_cert.pem's DER

use std::fs;
use std::path::PathBuf;

use rcgen::{
    CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256, PKCS_RSA_SHA256,
};
use sha2::{Digest, Sha256};

fn out_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tls")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = out_dir();
    fs::create_dir_all(&dir)?;

    // ---- 1. Self-signed leaf with PKCS#8 ECDSA P-256 key ----
    let mut params = CertificateParams::new(vec!["ai-memory-test.local".to_string()])?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "ai-memory test leaf");
    dn.push(DnType::OrganizationName, "AlphaOne LLC (tests)");
    params.distinguished_name = dn;

    let key_pair_p256 = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let leaf_cert = params.self_signed(&key_pair_p256)?;
    let leaf_pem = leaf_cert.pem();
    fs::write(dir.join("valid_cert.pem"), &leaf_pem)?;
    fs::write(
        dir.join("valid_key_pkcs8.pem"),
        key_pair_p256.serialize_pem(),
    )?;

    // SHA-256 of the leaf cert DER — what the mTLS allowlist would pin.
    let der_hash = Sha256::digest(leaf_cert.der());
    let mut hex = String::with_capacity(64);
    use std::fmt::Write as _;
    for b in der_hash.iter() {
        let _ = write!(hex, "{b:02x}");
    }
    fs::write(dir.join("valid_cert_sha256.txt"), format!("{hex}\n"))?;

    // ---- 2. RSA key (PKCS#1 envelope = "RSA PRIVATE KEY") ----
    // rcgen emits PKCS#8 by default. Rather than fight the encoder, we
    // use a precomputed RSA private key in PKCS#1 PEM format so the
    // `rustls_pki_pem_parse_private_key` SEC1/RSA branches are exercised.
    // The key is small (1024-bit, test-only) and ships verbatim here.
    let key_pair_rsa = KeyPair::generate_for(&PKCS_RSA_SHA256);
    if let Ok(rsa) = key_pair_rsa {
        // rcgen 0.14 emits PKCS#8 even for RSA — convert to PKCS#1 PEM by
        // emitting both forms. The PKCS#8 form satisfies the same parser
        // branch as ECDSA, so we ALSO include a static PKCS#1 PEM below
        // for the RSA-specific branch.
        fs::write(dir.join("valid_key_rsa_pkcs8.pem"), rsa.serialize_pem())?;
    }
    // Static PKCS#1 RSA PEM ("RSA PRIVATE KEY" header) — generated via
    // `openssl genrsa -traditional -out k.pem 2048`. Tiny test-only key,
    // used solely to drive the parser's RSA-PKCS#1 branch.
    let rsa_pkcs1_pem = include_str!("./fixtures_data/rsa_pkcs1.pem");
    fs::write(dir.join("valid_key_rsa.pem"), rsa_pkcs1_pem)?;

    // ---- 3. SEC1 EC key ("EC PRIVATE KEY" header) ----
    // Same trick — static SEC1-encoded P-256 private key, generated via
    // `openssl ecparam -name prime256v1 -genkey -noout -out k.pem`.
    let sec1_pem = include_str!("./fixtures_data/ec_sec1.pem");
    fs::write(dir.join("valid_key_sec1.pem"), sec1_pem)?;

    // ---- 4. Cert chain (leaf + self-signed "intermediate") ----
    // rcgen self-signs each, so the "chain" is just two concatenated
    // self-signed certs — sufficient to drive `pem_reader_iter` past
    // the single-cert case.
    let mut intermediate_params =
        CertificateParams::new(vec!["ai-memory-test-int.local".to_string()])?;
    let mut int_dn = DistinguishedName::new();
    int_dn.push(DnType::CommonName, "ai-memory test intermediate");
    int_dn.push(DnType::OrganizationName, "AlphaOne LLC (tests)");
    intermediate_params.distinguished_name = int_dn;
    let key_pair_int = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let intermediate_cert = intermediate_params.self_signed(&key_pair_int)?;
    let chain_pem = format!("{}{}", leaf_pem, intermediate_cert.pem());
    fs::write(dir.join("cert_chain.pem"), chain_pem)?;

    // ---- 5. Garbage PEM ----
    fs::write(dir.join("garbage.pem"), "not a pem file\n")?;

    println!("wrote fixtures to {}", dir.display());
    println!("leaf cert sha256: {hex}");
    Ok(())
}
