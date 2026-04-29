# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
"""Exception hierarchy for the ai-memory SDK.

The server returns ``{"error": "<message>"}`` on failure. We map the HTTP
status code onto a dedicated subclass so callers can match on class rather
than code. ``AiMemoryError`` is the common base so a single ``except`` clause
catches every SDK-raised error.
"""

from __future__ import annotations

from typing import Any


class AiMemoryError(Exception):
    """Base class for every error raised by the SDK.

    Attributes:
        status_code: The HTTP status returned by the daemon, or ``None`` when
            the failure is local (e.g. connection refused, JSON decode error).
        payload: The decoded error body, if the server returned one.
    """

    def __init__(
        self,
        message: str,
        *,
        status_code: int | None = None,
        payload: Any = None,
    ) -> None:
        super().__init__(message)
        self.status_code = status_code
        self.payload = payload


class TransportError(AiMemoryError):
    """Network / transport failure before a response was received."""


class ValidationError(AiMemoryError):
    """400 — request body rejected by ``src/validate.rs``."""


class AuthError(AiMemoryError):
    """401 — missing or invalid API key / mTLS cert."""


class ForbiddenError(AiMemoryError):
    """403 — request rejected by governance or ACL."""


class NotFoundError(AiMemoryError):
    """404 — memory / agent / subscription does not exist."""


class ConflictError(AiMemoryError):
    """409 — duplicate id, governance pending, or state conflict."""


class RateLimitError(AiMemoryError):
    """429 — rate limit exceeded."""


class ServerError(AiMemoryError):
    """5xx — daemon-side failure."""


def raise_for_status(status: int, payload: Any) -> None:
    """Raise the appropriate :class:`AiMemoryError` subclass.

    Called by the client after a non-2xx response. ``payload`` is the decoded
    JSON body (or ``None`` when the body was not JSON).
    """
    if status < 400:
        return
    message = _extract_message(payload) or f"HTTP {status}"
    cls: type[AiMemoryError]
    if status == 400:
        cls = ValidationError
    elif status == 401:
        cls = AuthError
    elif status == 403:
        cls = ForbiddenError
    elif status == 404:
        cls = NotFoundError
    elif status == 409:
        cls = ConflictError
    elif status == 429:
        cls = RateLimitError
    elif 500 <= status < 600:
        cls = ServerError
    else:
        cls = AiMemoryError
    raise cls(message, status_code=status, payload=payload)


def _extract_message(payload: Any) -> str | None:
    if isinstance(payload, dict):
        for key in ("error", "message", "reason", "detail"):
            v = payload.get(key)
            if isinstance(v, str) and v:
                return v
    return None
