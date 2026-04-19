# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
"""Shared helpers for the sync and async clients.

The two clients have near-identical surfaces; everything that does not need
an ``await`` lives here. In particular:

* Auth header injection (``X-API-Key`` and/or ``X-Agent-Id``)
* mTLS ``httpx`` config builder
* Response -> error mapping (delegates to :mod:`ai_memory.errors`)
* JSON body prep that strips ``None`` values (so omitted optional request
  fields don't clobber server defaults)
"""

from __future__ import annotations

import json
from typing import Any

import httpx
from pydantic import BaseModel

from ai_memory.errors import TransportError, raise_for_status

DEFAULT_BASE_URL = "http://localhost:9077"
DEFAULT_TIMEOUT = 30.0


def build_httpx_kwargs(
    *,
    base_url: str,
    api_key: str | None,
    agent_id: str | None,
    timeout: float,
    verify: bool | str | None,
    cert: str | tuple[str, str] | None,
    extra_headers: dict[str, str] | None,
) -> dict[str, Any]:
    """Build the ``httpx.Client`` / ``httpx.AsyncClient`` kwargs.

    mTLS is wired through the stock httpx params:

    * ``verify`` — path to the server CA bundle or ``True`` / ``False``.
    * ``cert`` — client certificate; accepts a single path or ``(cert, key)``.

    ``api_key`` is sent as ``X-API-Key`` (the server also accepts
    ``?api_key=`` query params, but a header keeps it out of access logs).
    ``agent_id`` is sent as ``X-Agent-Id`` — the HTTP daemon's default
    agent resolution precedence is body → header → per-request anonymous.
    """
    headers: dict[str, str] = {
        "User-Agent": "ai-memory-python/0.6.0-alpha.0",
        "Accept": "application/json",
    }
    if api_key:
        headers["X-API-Key"] = api_key
    if agent_id:
        headers["X-Agent-Id"] = agent_id
    if extra_headers:
        headers.update(extra_headers)

    kwargs: dict[str, Any] = {
        "base_url": base_url.rstrip("/"),
        "headers": headers,
        "timeout": timeout,
    }
    if verify is not None:
        kwargs["verify"] = verify
    if cert is not None:
        kwargs["cert"] = cert
    return kwargs


def prep_json(body: Any) -> Any:
    """Serialize Pydantic models and drop ``None`` at the top level.

    The server uses ``#[serde(default)]`` on most optional fields, so
    sending an explicit ``null`` would *not* be equivalent to omitting the
    key — some handlers would clobber an existing value. We drop keys whose
    value is ``None`` to preserve server-side defaults.
    """
    if isinstance(body, BaseModel):
        # by_alias=True so request models that use Field(alias=...) round-trip
        # (e.g. InboxMessage's ``from_`` → ``from``).
        return {k: v for k, v in body.model_dump(by_alias=True).items() if v is not None}
    if isinstance(body, dict):
        return {k: v for k, v in body.items() if v is not None}
    return body


def handle_response(response: httpx.Response) -> Any:
    """Raise on error, otherwise decode JSON (or return text for text/plain).

    ``/api/v1/metrics`` is Prometheus text; every other endpoint is JSON.
    """
    content_type = response.headers.get("content-type", "")
    if response.status_code >= 400:
        payload: Any
        try:
            payload = response.json()
        except (ValueError, json.JSONDecodeError):
            payload = response.text or None
        raise_for_status(response.status_code, payload)
    if "application/json" in content_type:
        return response.json()
    return response.text


def wrap_transport_error(exc: Exception) -> TransportError:
    """Convert an httpx transport error to our hierarchy."""
    return TransportError(f"transport error: {exc}", payload=None)
