// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_verify` handler.

use crate::{db, validate};
use serde_json::{Value, json};
/// v0.7 H4 — `memory_verify` MCP tool handler.
///
/// Looks up the named link by composite PK, re-derives the canonical
/// CBOR payload via [`crate::identity::sign::canonical_cbor`], looks up
/// the `observed_by` public key via
/// [`crate::identity::verify::lookup_peer_public_key`], and re-checks
/// the stored signature with [`crate::identity::verify::verify`].
///
/// Wire shape (always returned, even on the unsigned path):
///
/// ```json
/// {
///   "signature_verified": bool,
///   "attest_level": "unsigned" | "self_signed" | "peer_attested",
///   "signed_by": <observed_by string or null>,
///   "signed_at": <valid_from string or null>
/// }
/// ```
///
/// `signed_by` and `signed_at` are sourced from the `observed_by` and
/// `valid_from` columns respectively — the same columns the H2/H3
/// signature commits to. They are returned `null` on the unsigned path
/// so callers can drop them without a None-check.
///
/// `pub` so the H4 integration test in `tests/memory_verify.rs` can
/// drive the handler directly without standing up the JSON-RPC
/// envelope or spawning the daemon binary. Other handlers in this
/// module stay private because the dispatcher is their sole caller.
///
/// # Errors
///
/// Returned as JSON-RPC error strings (the dispatcher wraps them as
/// `-32602` invalid params). Specifically:
/// - missing required arguments (no `link_id` and no
///   `source_id`+`target_id`)
/// - `link_id` shape doesn't match the composite form
/// - link tuple does not exist in `memory_links`

pub fn handle_verify(conn: &rusqlite::Connection, params: &Value) -> Result<Value, String> {
    // Two callable shapes:
    //   1. link_id="<src>--<rel>-->\<dst>"
    //   2. source_id=… target_id=… [relation="related_to"]
    let (source_id, target_id, relation): (String, String, String) =
        if let Some(lid) = params.get("link_id").and_then(Value::as_str) {
            super::link::parse_link_id(lid).ok_or_else(|| {
                format!(
                    "link_id '{lid}' is not in the expected form \
                         'source_id--relation-->target_id'"
                )
            })?
        } else {
            let src = params
                .get("source_id")
                .and_then(Value::as_str)
                .ok_or("link_id or source_id+target_id is required")?;
            let dst = params
                .get("target_id")
                .and_then(Value::as_str)
                .ok_or("link_id or source_id+target_id is required")?;
            let rel = params
                .get("relation")
                .and_then(Value::as_str)
                .unwrap_or("related_to");
            (src.to_string(), dst.to_string(), rel.to_string())
        };

    // Validate the IDs / relation through the same gate `memory_link`
    // uses on the write path — keeps the verify surface from being a
    // back-door past the validator.
    validate::validate_link(&source_id, &target_id, &relation).map_err(|e| e.to_string())?;

    let record = db::get_link_for_verify(conn, &source_id, &target_id, &relation)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("link not found: ({source_id}, {relation}, {target_id})"))?;

    // Decision matrix mirrors `decide_attest_level` from the H3 tests:
    //   - signature is None → unsigned, signature_verified=false
    //   - signature is Some + observed_by is None → unsigned (no claim
    //     to verify against)
    //   - signature is Some + observed_by is Some + no enrolled
    //     pubkey on this host → return the column's stored attest_level
    //     (which the inbound path already wrote as either "unsigned" on
    //     enrolled-but-tampered, or whatever it landed as) but report
    //     `signature_verified = false` because *we* cannot verify
    //     without the public key.
    //   - signature is Some + observed_by is Some + pubkey enrolled →
    //     verify and report the actual outcome. We deliberately recheck
    //     here even when the column already says "self_signed" or
    //     "peer_attested": the whole point of `memory_verify` is on-
    //     demand re-validation, not a stored-flag readback.
    let stored_attest = record
        .attest_level
        .as_deref()
        .and_then(crate::models::AttestLevel::from_str)
        .unwrap_or(crate::models::AttestLevel::Unsigned);

    let (verified, attest_out): (bool, crate::models::AttestLevel) =
        match (record.signature.as_deref(), record.observed_by.as_deref()) {
            (None, _) | (_, None) => (false, crate::models::AttestLevel::Unsigned),
            (Some(sig_bytes), Some(observed_by)) => {
                let signable = crate::identity::sign::SignableLink {
                    src_id: &record.source_id,
                    dst_id: &record.target_id,
                    relation: &record.relation,
                    observed_by: Some(observed_by),
                    valid_from: record.valid_from.as_deref(),
                    valid_until: record.valid_until.as_deref(),
                };
                match crate::identity::verify::lookup_peer_public_key(observed_by) {
                    Some(pubkey) => {
                        let ok =
                            crate::identity::verify::verify(&pubkey, &signable, sig_bytes).is_ok();
                        if ok {
                            // On a successful re-verify, prefer the stored
                            // attest_level — it distinguishes self_signed
                            // (this host wrote+signed) from peer_attested
                            // (a peer signed and we accepted on inbound).
                            // If the column drifted to None on a very old
                            // row, fall back to PeerAttested (the only
                            // attestation we can re-derive without
                            // knowing whether the signing key is our own).
                            let level = match stored_attest {
                                crate::models::AttestLevel::Unsigned => {
                                    crate::models::AttestLevel::PeerAttested
                                }
                                other => other,
                            };
                            (true, level)
                        } else {
                            (false, crate::models::AttestLevel::Unsigned)
                        }
                    }
                    None => {
                        // Signature is present but we can't look up the
                        // pubkey on this host — surface as not-verified.
                        // Keep the stored attest_level so callers can see
                        // what the inbound path originally decided.
                        (false, stored_attest)
                    }
                }
            }
        };

    let signed_by: Value = if verified {
        record
            .observed_by
            .as_deref()
            .map_or(Value::Null, |s| Value::String(s.to_string()))
    } else {
        Value::Null
    };
    let signed_at: Value = if verified {
        record
            .valid_from
            .as_deref()
            .map_or(Value::Null, |s| Value::String(s.to_string()))
    } else {
        Value::Null
    };

    Ok(json!({
        "signature_verified": verified,
        "attest_level": attest_out.as_str(),
        "signed_by": signed_by,
        "signed_at": signed_at,
    }))
}
