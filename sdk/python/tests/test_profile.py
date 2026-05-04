# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
"""v0.6.4-007 — unit tests for the ``require_profile`` SDK helper.

These tests use a hand-rolled fake that satisfies the structural
``_request("GET", "/api/v1/capabilities")`` shape — no live daemon
needed. Every test runs in CI without ``AI_MEMORY_TEST_DAEMON=1``.
"""

from __future__ import annotations

import asyncio
from typing import Any

import pytest

from ai_memory import (
    ProfileNotLoaded,
    require_profile,
    require_profile_async,
    resolve_required_families,
)


def _families_payload(rows: list[dict[str, Any]]) -> dict[str, Any]:
    """Build the canonical capabilities-with-families response shape."""

    return {
        "schema_version": "2",
        "features": {},
        "families": {
            "schema_version": "v0.6.4-families-1",
            "always_on": ["memory_capabilities"],
            "families": rows,
        },
    }


ALL_LOADED = [
    {"name": "core", "loaded": True, "tool_count": 5},
    {"name": "lifecycle", "loaded": True, "tool_count": 5},
    {"name": "graph", "loaded": True, "tool_count": 8},
    {"name": "governance", "loaded": True, "tool_count": 8},
    {"name": "power", "loaded": True, "tool_count": 6},
    {"name": "meta", "loaded": True, "tool_count": 5},
    {"name": "archive", "loaded": True, "tool_count": 4},
    {"name": "other", "loaded": True, "tool_count": 2},
]

ONLY_CORE = [
    {"name": "core", "loaded": True, "tool_count": 5},
    {"name": "lifecycle", "loaded": False, "tool_count": 5},
    {"name": "graph", "loaded": False, "tool_count": 8},
    {"name": "governance", "loaded": False, "tool_count": 8},
    {"name": "power", "loaded": False, "tool_count": 6},
    {"name": "meta", "loaded": False, "tool_count": 5},
    {"name": "archive", "loaded": False, "tool_count": 4},
    {"name": "other", "loaded": False, "tool_count": 2},
]


class _SyncProbe:
    def __init__(self, payload: Any) -> None:
        self.payload = payload
        self.calls: list[tuple[str, str]] = []

    def _request(self, method: str, path: str, **kwargs: Any) -> Any:
        self.calls.append((method, path))
        return self.payload


class _AsyncProbe:
    def __init__(self, payload: Any) -> None:
        self.payload = payload
        self.calls: list[tuple[str, str]] = []

    async def _request(self, method: str, path: str, **kwargs: Any) -> Any:
        self.calls.append((method, path))
        return self.payload


# ---------------------------------------------------------------------
# resolve_required_families
# ---------------------------------------------------------------------


class TestResolveRequiredFamilies:
    def test_named_profiles(self) -> None:
        assert resolve_required_families("core") == ["core"]
        assert sorted(resolve_required_families("graph")) == ["core", "graph"]
        assert sorted(resolve_required_families("admin")) == [
            "core",
            "governance",
            "lifecycle",
        ]
        assert sorted(resolve_required_families("power")) == ["core", "power"]
        assert len(resolve_required_families("full")) == 8

    def test_empty_returns_core(self) -> None:
        assert resolve_required_families("") == ["core"]
        assert resolve_required_families("   ") == ["core"]

    def test_comma_list_dedup(self) -> None:
        result = resolve_required_families("core,graph,core")
        assert sorted(result) == ["core", "graph"]

    def test_comma_list_full_subsumes(self) -> None:
        assert len(resolve_required_families("core,graph,full")) == 8

    def test_implicit_core(self) -> None:
        assert "core" in resolve_required_families("archive")

    def test_unknown_family_raises(self) -> None:
        with pytest.raises(ValueError, match="unknown profile or family"):
            resolve_required_families("xyz")


# ---------------------------------------------------------------------
# require_profile (sync)
# ---------------------------------------------------------------------


class TestRequireProfile:
    def test_resolves_when_all_loaded(self) -> None:
        probe = _SyncProbe(_families_payload(ALL_LOADED))
        require_profile(probe, "graph")
        require_profile(probe, "full")
        assert probe.calls == [
            ("GET", "/api/v1/capabilities"),
            ("GET", "/api/v1/capabilities"),
        ]

    def test_raises_when_graph_missing(self) -> None:
        probe = _SyncProbe(_families_payload(ONLY_CORE))
        with pytest.raises(ProfileNotLoaded) as excinfo:
            require_profile(probe, "graph")
        err = excinfo.value
        assert "graph" in err.missing
        assert "--profile graph" in err.hint
        assert "AI_MEMORY_PROFILE=graph" in err.hint
        assert err.requested == "graph"

    def test_core_passes_with_only_core(self) -> None:
        probe = _SyncProbe(_families_payload(ONLY_CORE))
        require_profile(probe, "core")  # no raise

    def test_admin_fails_when_lifecycle_missing(self) -> None:
        rows = [r.copy() for r in ALL_LOADED]
        for r in rows:
            if r["name"] == "lifecycle":
                r["loaded"] = False
        probe = _SyncProbe(_families_payload(rows))
        with pytest.raises(ProfileNotLoaded, match="lifecycle"):
            require_profile(probe, "admin")

    def test_pre_v064_daemon_falls_back_with_warning(self) -> None:
        # Legacy capabilities response lacks the `families` block.
        legacy = {"schema_version": "2", "features": {}}
        probe = _SyncProbe(legacy)
        with pytest.warns(UserWarning, match="predates v0.6.4"):
            require_profile(probe, "graph")  # no raise


# ---------------------------------------------------------------------
# require_profile_async
# ---------------------------------------------------------------------


class TestRequireProfileAsync:
    def test_async_resolves(self) -> None:
        probe = _AsyncProbe(_families_payload(ALL_LOADED))
        asyncio.run(require_profile_async(probe, "graph"))

    def test_async_raises(self) -> None:
        probe = _AsyncProbe(_families_payload(ONLY_CORE))
        with pytest.raises(ProfileNotLoaded):
            asyncio.run(require_profile_async(probe, "graph"))

    def test_async_pre_v064_warns(self) -> None:
        probe = _AsyncProbe({"schema_version": "2"})
        with pytest.warns(UserWarning, match="predates v0.6.4"):
            asyncio.run(require_profile_async(probe, "graph"))
