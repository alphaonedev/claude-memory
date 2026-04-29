# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
"""Asynchronous client for the ai-memory HTTP API.

Mirror of :class:`ai_memory.client.AiMemoryClient` but built on
:class:`httpx.AsyncClient`. Every public method is a coroutine; semantics
are otherwise identical.
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


class AsyncAiMemoryClient:
    """Async client bound to a single daemon instance.

    Use as an async context manager or call :meth:`aclose` explicitly.
    Arguments match :class:`ai_memory.client.AiMemoryClient`.
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
        self._client = httpx.AsyncClient(
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
    async def aclose(self) -> None:
        await self._client.aclose()

    async def __aenter__(self) -> AsyncAiMemoryClient:
        return self

    async def __aexit__(
        self,
        exc_type: type[BaseException] | None,
        exc: BaseException | None,
        tb: TracebackType | None,
    ) -> None:
        await self.aclose()

    # -- low-level request --------------------------------------------------
    async def _request(
        self,
        method: str,
        path: str,
        *,
        json_body: Any = None,
        params: dict[str, Any] | None = None,
    ) -> Any:
        try:
            response = await self._client.request(
                method,
                path,
                json=prep_json(json_body) if json_body is not None else None,
                params={k: v for k, v in (params or {}).items() if v is not None} or None,
            )
        except httpx.HTTPError as exc:
            raise wrap_transport_error(exc) from exc
        return handle_response(response)

    # -- health & metrics ---------------------------------------------------
    async def health(self) -> dict[str, Any]:
        return await self._request("GET", "/api/v1/health")

    async def metrics(self) -> str:
        return await self._request("GET", "/api/v1/metrics")

    # -- core CRUD ----------------------------------------------------------
    async def store(
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
        return await self._request("POST", "/api/v1/memories", json_body=body)

    async def bulk_store(
        self, memories: list[CreateMemory | dict[str, Any]]
    ) -> BulkCreateResponse:
        payload = [prep_json(m) for m in memories]
        raw = await self._request("POST", "/api/v1/memories/bulk", json_body=payload)
        return BulkCreateResponse.model_validate(raw)

    async def get(self, memory_id: str) -> Memory:
        raw = await self._request("GET", f"/api/v1/memories/{memory_id}")
        return Memory.model_validate(raw)

    async def update(
        self, memory_id: str, update: UpdateMemory | dict[str, Any]
    ) -> dict[str, Any]:
        return await self._request("PUT", f"/api/v1/memories/{memory_id}", json_body=update)

    async def delete(self, memory_id: str) -> dict[str, Any]:
        return await self._request("DELETE", f"/api/v1/memories/{memory_id}")

    async def promote(self, memory_id: str) -> dict[str, Any]:
        return await self._request("POST", f"/api/v1/memories/{memory_id}/promote")

    # -- listing / search / recall -----------------------------------------
    async def list(
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
        raw = await self._request(
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

    async def search(
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
        raw = await self._request(
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

    async def recall(
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
        raw = await self._request("POST", "/api/v1/recall", json_body=body)
        return RecallResponse.model_validate(raw)

    async def forget(
        self,
        *,
        namespace: str | None = None,
        pattern: str | None = None,
        tier: str | None = None,
    ) -> dict[str, Any]:
        return await self._request(
            "POST",
            "/api/v1/forget",
            params={"namespace": namespace, "pattern": pattern, "tier": tier},
        )

    # -- links / stats / admin ---------------------------------------------
    async def link(
        self, source_id: str, target_id: str, relation: str = "related_to"
    ) -> dict[str, Any]:
        return await self._request(
            "POST",
            "/api/v1/links",
            json_body={"source_id": source_id, "target_id": target_id, "relation": relation},
        )

    async def get_links(self, memory_id: str) -> list[dict[str, Any]]:
        raw = await self._request("GET", f"/api/v1/links/{memory_id}")
        return raw.get("links", raw) if isinstance(raw, dict) else raw

    async def stats(self) -> Stats:
        return Stats.model_validate(await self._request("GET", "/api/v1/stats"))

    async def namespaces(self) -> list[dict[str, Any]]:
        raw = await self._request("GET", "/api/v1/namespaces")
        return raw.get("namespaces", raw) if isinstance(raw, dict) else raw

    async def gc(self) -> dict[str, Any]:
        return await self._request("POST", "/api/v1/gc")

    async def export(self) -> Any:
        return await self._request("GET", "/api/v1/export")

    async def import_(self, payload: Any) -> dict[str, Any]:
        return await self._request("POST", "/api/v1/import", json_body=payload)

    # -- subscriptions / webhooks ------------------------------------------
    async def subscribe(self, request: SubscriptionRequest | dict[str, Any]) -> Subscription:
        raw = await self._request("POST", "/api/v1/subscriptions", json_body=request)
        return Subscription.model_validate(raw)

    async def unsubscribe(self, subscription_id: str) -> dict[str, Any]:
        return await self._request("DELETE", f"/api/v1/subscriptions/{subscription_id}")

    async def subscriptions(self) -> list[Subscription]:
        raw = await self._request("GET", "/api/v1/subscriptions")
        items = raw.get("subscriptions", raw) if isinstance(raw, dict) else raw
        return [Subscription.model_validate(s) for s in items]

    # -- notify / inbox ----------------------------------------------------
    async def notify(self, request: NotifyRequest | dict[str, Any]) -> dict[str, Any]:
        return await self._request("POST", "/api/v1/notify", json_body=request)

    async def inbox(
        self,
        *,
        agent_id: str | None = None,
        unread_only: bool | None = None,
        limit: int | None = None,
    ) -> list[dict[str, Any]]:
        raw = await self._request(
            "GET",
            "/api/v1/inbox",
            params={"agent_id": agent_id, "unread_only": unread_only, "limit": limit},
        )
        return raw.get("messages", raw) if isinstance(raw, dict) else raw

    # -- grant / revoke ----------------------------------------------------
    async def grant(
        self, memory_id: str, agent_id: str, permission: str = "read"
    ) -> dict[str, Any]:
        return await self._request(
            "POST",
            f"/api/v1/memories/{memory_id}/grant",
            json_body={"agent_id": agent_id, "permission": permission},
        )

    async def revoke(self, memory_id: str, agent_id: str) -> dict[str, Any]:
        return await self._request(
            "POST",
            f"/api/v1/memories/{memory_id}/revoke",
            json_body={"agent_id": agent_id},
        )

    # -- cluster ------------------------------------------------------------
    async def cluster(self, request: dict[str, Any] | None = None) -> dict[str, Any]:
        return await self._request("POST", "/api/v1/cluster", json_body=request or {})

    # -- agents -------------------------------------------------------------
    async def agents(self) -> list[AgentRegistration]:
        raw = await self._request("GET", "/api/v1/agents")
        items = raw.get("agents", raw) if isinstance(raw, dict) else raw
        return [AgentRegistration.model_validate(a) for a in items]

    async def register_agent(
        self, agent_id: str, agent_type: str, capabilities: list[str] | None = None
    ) -> dict[str, Any]:
        return await self._request(
            "POST",
            "/api/v1/agents",
            json_body={
                "agent_id": agent_id,
                "agent_type": agent_type,
                "capabilities": capabilities or [],
            },
        )
