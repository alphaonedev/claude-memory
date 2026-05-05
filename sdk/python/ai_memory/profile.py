# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0

"""v0.6.4-007 — ``require_profile`` SDK helper for the Python SDK.

NHI agents that depend on tools outside the v0.6.4 default ``core``
profile call :func:`require_profile` (or :func:`require_profile_async`
for the async client) at startup. The helper fetches
``GET /api/v1/capabilities``, inspects the ``families`` block added by
v0.6.4-006, and raises :class:`ProfileNotLoaded` with an actionable
hint when any required family is missing.

The helper is *purely additive* — existing SDK consumers that don't
need profile-aware bootstrap remain unaffected.

Example
-------

.. code-block:: python

    from ai_memory import AiMemoryClient, require_profile, ProfileNotLoaded

    with AiMemoryClient(base_url="http://localhost:9077") as c:
        try:
            require_profile(c, "graph")
        except ProfileNotLoaded as e:
            print("Restart MCP server with:", e.hint)
            raise SystemExit(2)
"""

from __future__ import annotations

import warnings
from typing import Any, Protocol

# Map of profile-name → families that must be loaded. Source-anchored
# at ``src/profile.rs::Profile::*``.
_PROFILE_FAMILY_REQUIREMENTS: dict[str, list[str]] = {
    "core": ["core"],
    "graph": ["core", "graph"],
    "admin": ["core", "lifecycle", "governance"],
    "power": ["core", "power"],
    "full": [
        "core",
        "lifecycle",
        "graph",
        "governance",
        "power",
        "meta",
        "archive",
        "other",
    ],
}

_VALID_FAMILIES = (
    "core",
    "lifecycle",
    "graph",
    "governance",
    "power",
    "meta",
    "archive",
    "other",
)


class ProfileNotLoaded(Exception):
    """Raised when a daemon does not load every family required for the
    requested profile.

    The :attr:`hint` attribute carries a one-line CLI/env snippet the
    operator can paste to restart the server with the right profile.

    Attributes
    ----------
    hint : str
        Actionable remediation snippet ("--profile graph").
    missing : list[str]
        Family names that the daemon reported as ``loaded=False``.
    requested : str
        The profile name the caller asked for.
    """

    def __init__(self, requested: str, missing: list[str]) -> None:
        cli = f"--profile {requested}"
        env = f"AI_MEMORY_PROFILE={requested}"
        hint = (
            f"restart the ai-memory MCP server with `{cli}` (or set {env}); "
            f"missing families: {', '.join(missing)}"
        )
        super().__init__(f"profile '{requested}' not fully loaded — {hint}")
        self.hint = hint
        self.missing = missing
        self.requested = requested


def resolve_required_families(profile: str) -> list[str]:
    """Return the family set required by ``profile``.

    Accepts the named profiles (``core``, ``graph``, ``admin``, ``power``,
    ``full``) and comma-separated custom lists. Empty / whitespace-only
    input resolves to ``["core"]``.

    Raises
    ------
    ValueError
        If a token is neither a known profile nor a known family.
    """

    trimmed = profile.strip()
    if trimmed == "":
        return ["core"]
    named = _PROFILE_FAMILY_REQUIREMENTS.get(trimmed)
    if named is not None:
        return list(named)

    # Comma-list custom.
    requested: list[str] = ["core"]
    for raw in trimmed.split(","):
        tok = raw.strip()
        if tok == "":
            continue
        if tok == "full":
            return list(_PROFILE_FAMILY_REQUIREMENTS["full"])
        if tok in _PROFILE_FAMILY_REQUIREMENTS:
            for f in _PROFILE_FAMILY_REQUIREMENTS[tok]:
                if f not in requested:
                    requested.append(f)
            continue
        if tok not in _VALID_FAMILIES:
            raise ValueError(
                f"unknown profile or family '{tok}'. "
                f"Valid: {', '.join(_VALID_FAMILIES)}, full"
            )
        if tok not in requested:
            requested.append(tok)
    return requested


class _SyncCapabilitiesProbe(Protocol):
    """Structural interface for the sync capabilities path. Any object
    with a ``_request("GET", "/api/v1/capabilities")`` shape satisfies
    it (including :class:`ai_memory.AiMemoryClient`)."""

    def _request(self, method: str, path: str, **kwargs: Any) -> Any:  # noqa: D401
        ...


class _AsyncCapabilitiesProbe(Protocol):
    """Async counterpart for :class:`AsyncAiMemoryClient`."""

    async def _request(self, method: str, path: str, **kwargs: Any) -> Any:  # noqa: D401
        ...


def _missing_from(payload: Any, required: list[str]) -> list[str] | None:
    """Compute the set of required-but-not-loaded families from a
    capabilities response payload. Returns ``None`` when the payload
    predates v0.6.4 (no ``families`` block) so callers can take the
    permissive fallback path."""

    families_block = (
        payload.get("families") if isinstance(payload, dict) else None
    )
    if not isinstance(families_block, dict):
        return None
    rows = families_block.get("families")
    if not isinstance(rows, list):
        return None
    loaded: set[str] = set()
    for row in rows:
        if isinstance(row, dict) and row.get("loaded") is True:
            name = row.get("name")
            if isinstance(name, str):
                loaded.add(name)
    return [f for f in required if f not in loaded]


def require_profile(client: _SyncCapabilitiesProbe, profile: str) -> None:
    """Verify the daemon loads every family required by ``profile``.

    Raises
    ------
    ProfileNotLoaded
        If any required family is missing.

    Notes
    -----
    Pre-v0.6.4 daemons do not return a ``families`` block in their
    capabilities response. In that case this helper emits a
    :class:`UserWarning` and returns silently — operators upgrading
    the SDK before the daemon should not see a regression.
    """

    required = resolve_required_families(profile)
    payload = client._request("GET", "/api/v1/capabilities")
    missing = _missing_from(payload, required)
    if missing is None:
        warnings.warn(
            "ai-memory SDK require_profile: daemon predates v0.6.4; "
            "cannot verify profile. Skipping check.",
            UserWarning,
            stacklevel=2,
        )
        return
    if missing:
        raise ProfileNotLoaded(profile, missing)


async def require_profile_async(
    client: _AsyncCapabilitiesProbe, profile: str
) -> None:
    """Async counterpart of :func:`require_profile` for use with
    :class:`ai_memory.AsyncAiMemoryClient`."""

    required = resolve_required_families(profile)
    payload = await client._request("GET", "/api/v1/capabilities")
    missing = _missing_from(payload, required)
    if missing is None:
        warnings.warn(
            "ai-memory SDK require_profile_async: daemon predates v0.6.4; "
            "cannot verify profile. Skipping check.",
            UserWarning,
            stacklevel=2,
        )
        return
    if missing:
        raise ProfileNotLoaded(profile, missing)
