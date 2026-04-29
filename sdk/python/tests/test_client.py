# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
"""Smoke tests for :class:`AiMemoryClient`.

The suite is split in two:

* **Offline tests** (always run) — exercise the pure-Python parts: model
  serialization, webhook HMAC, error mapping.
* **Daemon tests** (opt-in) — run only when ``AI_MEMORY_TEST_DAEMON=1`` is
  set and a daemon is reachable at ``http://localhost:9077``. Every daemon
  test writes and deletes its own namespace to avoid polluting shared state.
"""

from __future__ import annotations

import os
import uuid

import httpx
import pytest

from ai_memory import (
    AiMemoryClient,
    AiMemoryError,
    CreateMemory,
    NotFoundError,
    Tier,
    ValidationError,
    verify_webhook_signature,
)
from ai_memory.errors import raise_for_status
from ai_memory.models import Memory

TEST_BASE_URL = os.environ.get("AI_MEMORY_TEST_BASE_URL", "http://localhost:9077")
DAEMON_ENABLED = os.environ.get("AI_MEMORY_TEST_DAEMON") == "1"


def _daemon_reachable() -> bool:
    if not DAEMON_ENABLED:
        return False
    try:
        response = httpx.get(f"{TEST_BASE_URL}/api/v1/health", timeout=2.0)
    except httpx.HTTPError:
        return False
    return response.status_code == 200


skip_without_daemon = pytest.mark.skipif(
    not _daemon_reachable(),
    reason="AI_MEMORY_TEST_DAEMON!=1 or daemon not reachable at localhost:9077",
)


# ---------------------------------------------------------------------------
# Offline: model + error + webhook tests
# ---------------------------------------------------------------------------


def test_tier_enum_values() -> None:
    assert Tier.SHORT.value == "short"
    assert Tier.MID.value == "mid"
    assert Tier.LONG.value == "long"


def test_create_memory_defaults_match_server() -> None:
    body = CreateMemory(title="t", content="c")
    dumped = body.model_dump(by_alias=True)
    assert dumped["tier"] == "mid"
    assert dumped["namespace"] == "global"
    assert dumped["priority"] == 5
    assert dumped["confidence"] == 1.0
    assert dumped["source"] == "api"


def test_memory_round_trips_metadata() -> None:
    payload = {
        "id": "abc",
        "tier": "long",
        "namespace": "global",
        "title": "t",
        "content": "c",
        "tags": ["x"],
        "priority": 7,
        "confidence": 0.8,
        "source": "api",
        "access_count": 3,
        "created_at": "2026-04-19T00:00:00Z",
        "updated_at": "2026-04-19T00:00:00Z",
        "metadata": {"agent_id": "alice", "scope": "team"},
    }
    m = Memory.model_validate(payload)
    assert m.metadata["agent_id"] == "alice"
    assert m.tier is Tier.LONG


def test_raise_for_status_maps_404() -> None:
    with pytest.raises(NotFoundError) as info:
        raise_for_status(404, {"error": "not found"})
    assert info.value.status_code == 404


def test_raise_for_status_maps_400_to_validation() -> None:
    with pytest.raises(ValidationError):
        raise_for_status(400, {"error": "title cannot be empty"})


def test_raise_for_status_passes_on_2xx() -> None:
    raise_for_status(200, {"ok": True})  # does not raise


def test_webhook_signature_round_trip() -> None:
    import hashlib
    import hmac

    body = b'{"event":"memory.created"}'
    secret = "s3cret"
    sig = "sha256=" + hmac.new(secret.encode(), body, hashlib.sha256).hexdigest()
    assert verify_webhook_signature(body, sig, secret)
    assert not verify_webhook_signature(body, sig, "wrong")
    assert not verify_webhook_signature(body + b"tampered", sig, secret)


def test_webhook_signature_rejects_malformed() -> None:
    assert not verify_webhook_signature(b"x", "", "s")
    assert not verify_webhook_signature(b"x", "sha256=notHex", "s")


# ---------------------------------------------------------------------------
# Daemon-backed integration tests (opt-in)
# ---------------------------------------------------------------------------


@skip_without_daemon
def test_health_ok() -> None:
    with AiMemoryClient(base_url=TEST_BASE_URL) as c:
        out = c.health()
        assert out.get("status") == "ok"


@skip_without_daemon
def test_store_and_get_roundtrip() -> None:
    ns = f"sdk-test-{uuid.uuid4().hex[:8]}"
    with AiMemoryClient(base_url=TEST_BASE_URL) as c:
        created = c.store(title="hello", content="world", namespace=ns)
        memory_id = created["id"]
        try:
            fetched = c.get(memory_id)
            assert fetched.namespace == ns
            assert fetched.title == "hello"
        finally:
            c.forget(namespace=ns)


@skip_without_daemon
def test_recall_returns_wrapper() -> None:
    ns = f"sdk-test-{uuid.uuid4().hex[:8]}"
    with AiMemoryClient(base_url=TEST_BASE_URL) as c:
        c.store(title="recall subject", content="body text", namespace=ns)
        try:
            resp = c.recall(context="recall subject", namespace=ns)
            assert resp.count >= 0
            assert isinstance(resp.memories, list)
        finally:
            c.forget(namespace=ns)


@skip_without_daemon
def test_not_found_raises() -> None:
    with AiMemoryClient(base_url=TEST_BASE_URL) as c:
        with pytest.raises(AiMemoryError):
            c.get("does-not-exist-" + uuid.uuid4().hex)
