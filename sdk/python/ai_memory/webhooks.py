# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
"""Webhook signature verification.

When the server delivers a subscription event it adds an
``X-AI-Memory-Signature`` header of the form ``sha256=<hex>`` computed as
``hmac_sha256(secret, body)``. :func:`verify_webhook_signature` performs a
constant-time compare of that signature against a locally computed one.
"""

from __future__ import annotations

import hashlib
import hmac


def verify_webhook_signature(body: bytes, signature: str, secret: str) -> bool:
    """Verify an HMAC-SHA256 signature produced by the ai-memory daemon.

    Args:
        body: The raw request body bytes exactly as received (do **not**
            re-encode a parsed JSON payload; whitespace differences will
            break the HMAC).
        signature: The value of the ``X-AI-Memory-Signature`` header.
            Accepts either ``"sha256=<hex>"`` (the preferred form) or a
            bare hex digest.
        secret: The shared secret configured when the subscription was
            created.

    Returns:
        ``True`` when the signature matches, ``False`` otherwise. Returns
        ``False`` — never raises — for malformed input so callers can treat
        any non-``True`` result as "reject this delivery."
    """
    if not signature or not isinstance(secret, str) or not secret:
        return False

    hex_sig = signature.strip()
    if hex_sig.startswith("sha256="):
        hex_sig = hex_sig[len("sha256=") :]

    try:
        received = bytes.fromhex(hex_sig)
    except ValueError:
        return False

    mac = hmac.new(secret.encode("utf-8"), body, hashlib.sha256).digest()
    return hmac.compare_digest(mac, received)
