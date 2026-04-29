# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
"""Synchronous client for the ai-memory HTTP API.

All methods are thin wrappers over :class:`httpx.Client`. Requests are
pre-processed through :func:`ai_memory._common.prep_json` to drop ``None``
values so server-side defaults are respected. Errors surface through the
:mod:`ai_memory.errors` hierarchy (``NotFoundError``, ``ValidationError``,
``AuthError``, etc.).

See :class:`AsyncAiMemoryClient` for the asyncio counterpart.
"""

from __future__ import annotations

from types import TracebackType
from typing import Any

import httpx

from ai_memory._common import (
    DEFAULT_BASE_URL,
    DEFAULT_TIMEOUT,
    build_httpx_kwargs,
    handle_response,
    prep_json,
    wrap_transport_error,
)
from ai_memory.models import (
    AgentRegistration,
    BulkCreateResponse,
    CreateMemory,
    Memory,
    NotifyRequest,
    RecallRequest,
    RecallResponse,
    Stats,
    Subscription,
    SubscriptionRequest,
    UpdateMemory,
)


class AiMemoryClient:
    """Synchronous client bound to a single daemon instance.

    Use as a context manager to ensure the underlying ``httpx.Client`` is
    closed, or call :meth:`close` explicitly.

    Args:
        base_url: Daemon URL, default ``http://localhost:9077``.
        api_key: If provided, sent as ``X-API-Key`` on every request.
        agent_id: If provided, sent as ``X-Agent-Id`` so the server stamps
            this identity on stored memories (see CLAUDE.md §Agent Identity).
        timeout: Seconds before a request is aborted.
        verify: ``httpx`` ``verify`` — path to server CA bundle or bool.
        cert: ``httpx`` ``cert`` — client cert for mTLS (path or
            ``(cert, key)``).
        headers: Additional headers to send on every request.
    """

    def __init__(
        self,
        base_url: str = DEFAULT_BASE_URL,
        *,
        api_key: str | None = None,
        agent_id: str | None = None,
        timeout: float = DEFAULT_TIMEOUT,
        verify: bool | str | None = None,
        cert: str | tuple[str, str] | None = None,
        headers: dict[str, str] | None = None,
    ) -> None:
        self._client = httpx.Client(
            **build_httpx_kwargs(
                base_url=base_url,
                api_key=api_key,
                agent_id=agent_id,
                timeout=timeout,
                verify=verify,
                cert=cert,
                extra_headers=headers,
            )
        )

    # -- lifecycle ----------------------------------------------------------
    def close(self) -> None:
        self._client.close()

    def __enter__(self) -> AiMemoryClient:
        return self

    def __exit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        self.close()

    # -- low-level request --------------------------------------------------
    def _request(
        self,
        method: str,
        path: str,
        *,
        json_body: Any = None,
        params: dict[str, Any] | None = None,
    ) -> Any:
        try:
            response = self._client.request(
                method,
                path,
                json=prep_json(json_body) if json_body is not None else None,
                params={k: v for k, v in (params or {}).items() if v is not None} or None,
            )
        except httpx.HTTPError as exc:
            raise wrap_transport_error(exc) from exc
        return handle_response(response)

    # -- health & metrics ---------------------------------------------------
    def health(self) -> dict[str, Any]:
        """``GET /api/v1/health`` — liveness/readiness probe."""
        return self._request("GET", "/api/v1/health")

    def metrics(self) -> str:
        """``GET /api/v1/metrics`` — Prometheus text format (as ``str``)."""
        return self._request("GET", "/api/v1/metrics")

    # -- core CRUD ----------------------------------------------------------
    def store(
        self,
        *,
        title: str,
        content: str,
        tier: str | None = None,
        namespace: str | None = None,
        tags: list[str] | None = None,
        priority: int | None = None,
        confidence: float | None = None,
        source: str | None = None,
        expires_at: str | None = None,
        ttl_secs: int | None = None,
        metadata: dict[str, Any] | None = None,
        agent_id: str | None = None,
        scope: str | None = None,
    ) -> dict[str, Any]:
        """``POST /api/v1/memories`` — create (or upsert on title+ns)."""
        body = CreateMemory(
            title=title,
            content=content,
            **{
                k: v
                for k, v in {
                    "tier": tier,
                    "namespace": namespace,
                    "tags": tags,
                    "priority": priority,
                    "confidence": confidence,
                    "source": source,
                    "expires_at": expires_at,
                    "ttl_secs": ttl_secs,
                    "metadata": metadata,
                    "agent_id": agent_id,
                    "scope": scope,
                }.items()
                if v is not None
            },
        )
        return self._request("POST", "/api/v1/memories", json_body=body)

    def bulk_store(self, memories: list[CreateMemory | dict[str, Any]]) -> BulkCreateResponse:
        """``POST /api/v1/memories/bulk`` — insert up to 1000 at once."""
        payload = [prep_json(m) for m in memories]
        raw = self._request("POST", "/api/v1/memories/bulk", json_body=payload)
        return BulkCreateResponse.model_validate(raw)

    def get(self, memory_id: str) -> Memory:
        """``GET /api/v1/memories/{id}``."""
        raw = self._request("GET", f"/api/v1/memories/{memory_id}")
        return Memory.model_validate(raw)

    def update(self, memory_id: str, update: UpdateMemory | dict[str, Any]) -> dict[str, Any]:
        """``PUT /api/v1/memories/{id}``."""
        return self._request("PUT", f"/api/v1/memories/{memory_id}", json_body=update)

    def delete(self, memory_id: str) -> dict[str, Any]:
        """``DELETE /api/v1/memories/{id}``."""
        return self._request("DELETE", f"/api/v1/memories/{memory_id}")

    def promote(self, memory_id: str) -> dict[str, Any]:
        """``POST /api/v1/memories/{id}/promote`` — tier -> long."""
        return self._request("POST", f"/api/v1/memories/{memory_id}/promote")

    # -- listing / search / recall -----------------------------------------
    def list(
        self,
        *,
        namespace: str | None = None,
        tier: str | None = None,
        limit: int | None = None,
        offset: int | None = None,
        min_priority: int | None = None,
        since: str | None = None,
        until: str | None = None,
        tags: str | None = None,
        agent_id: str | None = None,
    ) -> list[Memory]:
        """``GET /api/v1/memories`` — browse with filters."""
        raw = self._request(
            "GET",
            "/api/v1/memories",
            params={
                "namespace": namespace,
                "tier": tier,
                "limit": limit,
                "offset": offset,
                "min_priority": min_priority,
                "since": since,
                "until": until,
                "tags": tags,
                "agent_id": agent_id,
            },
        )
        items = raw.get("memories", raw) if isinstance(raw, dict) else raw
        return [Memory.model_validate(m) for m in items]

    def search(
        self,
        q: str,
        *,
        namespace: str | None = None,
        tier: str | None = None,
        limit: int | None = None,
        min_priority: int | None = None,
        since: str | None = None,
        until: str | None = None,
        tags: str | None = None,
        agent_id: str | None = None,
        as_agent: str | None = None,
    ) -> list[Memory]:
        """``GET /api/v1/search`` — FTS keyword AND search."""
        raw = self._request(
            "GET",
            "/api/v1/search",
            params={
                "q": q,
                "namespace": namespace,
                "tier": tier,
                "limit": limit,
                "min_priority": min_priority,
                "since": since,
                "until": until,
                "tags": tags,
                "agent_id": agent_id,
                "as_agent": as_agent,
            },
        )
        items = raw.get("memories", raw) if isinstance(raw, dict) else raw
        return [Memory.model_validate(m) for m in items]

    def recall(
        self,
        context: str,
        *,
        namespace: str | None = None,
        limit: int | None = None,
        tags: str | None = None,
        since: str | None = None,
        until: str | None = None,
        as_agent: str | None = None,
        budget_tokens: int | None = None,
    ) -> RecallResponse:
        """``POST /api/v1/recall`` — hybrid FTS + semantic recall.

        Uses the POST form because ``context`` can be long prose. Every
        recall is write-coupled: the server bumps ``access_count`` and
        extends TTLs on returned memories.
        """
        body = RecallRequest(
            context=context,
            namespace=namespace,
            limit=limit,
            tags=tags,
            since=since,
            until=until,
            as_agent=as_agent,
            budget_tokens=budget_tokens,
        )
        raw = self._request("POST", "/api/v1/recall", json_body=body)
        return RecallResponse.model_validate(raw)

    def forget(
        self,
        *,
        namespace: str | None = None,
        pattern: str | None = None,
        tier: str | None = None,
    ) -> dict[str, Any]:
        """``POST /api/v1/forget`` — bulk delete by namespace/pattern/tier."""
        return self._request(
            "POST",
            "/api/v1/forget",
            params={"namespace": namespace, "pattern": pattern, "tier": tier},
        )

    # -- links / stats / admin ---------------------------------------------
    def link(
        self, source_id: str, target_id: str, relation: str = "related_to"
    ) -> dict[str, Any]:
        """``POST /api/v1/links``."""
        return self._request(
            "POST",
            "/api/v1/links",
            json_body={"source_id": source_id, "target_id": target_id, "relation": relation},
        )

    def get_links(self, memory_id: str) -> list[dict[str, Any]]:
        """``GET /api/v1/links/{id}``."""
        raw = self._request("GET", f"/api/v1/links/{memory_id}")
        return raw.get("links", raw) if isinstance(raw, dict) else raw

    def stats(self) -> Stats:
        """``GET /api/v1/stats``."""
        return Stats.model_validate(self._request("GET", "/api/v1/stats"))

    def namespaces(self) -> list[dict[str, Any]]:
        """``GET /api/v1/namespaces``."""
        raw = self._request("GET", "/api/v1/namespaces")
        return raw.get("namespaces", raw) if isinstance(raw, dict) else raw

    def gc(self) -> dict[str, Any]:
        """``POST /api/v1/gc`` — run garbage collection on demand."""
        return self._request("POST", "/api/v1/gc")

    def export(self) -> Any:
        """``GET /api/v1/export`` — dump every memory as JSON."""
        return self._request("GET", "/api/v1/export")

    def import_(self, payload: Any) -> dict[str, Any]:
        """``POST /api/v1/import``."""
        return self._request("POST", "/api/v1/import", json_body=payload)

    # -- subscriptions / webhooks ------------------------------------------
    def subscribe(self, request: SubscriptionRequest | dict[str, Any]) -> Subscription:
        """``POST /api/v1/subscriptions`` — register a webhook."""
        raw = self._request("POST", "/api/v1/subscriptions", json_body=request)
        return Subscription.model_validate(raw)

    def unsubscribe(self, subscription_id: str) -> dict[str, Any]:
        """``DELETE /api/v1/subscriptions/{id}``."""
        return self._request("DELETE", f"/api/v1/subscriptions/{subscription_id}")

    def subscriptions(self) -> list[Subscription]:
        """``GET /api/v1/subscriptions``."""
        raw = self._request("GET", "/api/v1/subscriptions")
        items = raw.get("subscriptions", raw) if isinstance(raw, dict) else raw
        return [Subscription.model_validate(s) for s in items]

    # -- notify / inbox ----------------------------------------------------
    def notify(self, request: NotifyRequest | dict[str, Any]) -> dict[str, Any]:
        """``POST /api/v1/notify`` — send an agent-to-agent message."""
        return self._request("POST", "/api/v1/notify", json_body=request)

    def inbox(
        self,
        *,
        agent_id: str | None = None,
        unread_only: bool | None = None,
        limit: int | None = None,
    ) -> list[dict[str, Any]]:
        """``GET /api/v1/inbox`` — fetch received messages."""
        raw = self._request(
            "GET",
            "/api/v1/inbox",
            params={"agent_id": agent_id, "unread_only": unread_only, "limit": limit},
        )
        return raw.get("messages", raw) if isinstance(raw, dict) else raw

    # -- grant / revoke (per-memory ACL) -----------------------------------
    def grant(
        self, memory_id: str, agent_id: str, permission: str = "read"
    ) -> dict[str, Any]:
        """``POST /api/v1/memories/{id}/grant``."""
        return self._request(
            "POST",
            f"/api/v1/memories/{memory_id}/grant",
            json_body={"agent_id": agent_id, "permission": permission},
        )

    def revoke(self, memory_id: str, agent_id: str) -> dict[str, Any]:
        """``POST /api/v1/memories/{id}/revoke``."""
        return self._request(
            "POST",
            f"/api/v1/memories/{memory_id}/revoke",
            json_body={"agent_id": agent_id},
        )

    # -- cluster ------------------------------------------------------------
    def cluster(self, request: dict[str, Any] | None = None) -> dict[str, Any]:
        """``POST /api/v1/cluster`` — cluster join/peer management."""
        return self._request("POST", "/api/v1/cluster", json_body=request or {})

    # -- agents -------------------------------------------------------------
    def agents(self) -> list[AgentRegistration]:
        """``GET /api/v1/agents``."""
        raw = self._request("GET", "/api/v1/agents")
        items = raw.get("agents", raw) if isinstance(raw, dict) else raw
        return [AgentRegistration.model_validate(a) for a in items]

    def register_agent(
        self, agent_id: str, agent_type: str, capabilities: list[str] | None = None
    ) -> dict[str, Any]:
        """``POST /api/v1/agents``."""
        return self._request(
            "POST",
            "/api/v1/agents",
            json_body={
                "agent_id": agent_id,
                "agent_type": agent_type,
                "capabilities": capabilities or [],
            },
        )
