# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
"""Pydantic models mirroring Rust structs in ``src/models.rs``.

Design notes
------------
* Every model uses ``model_config = ConfigDict(populate_by_name=True,
  extra="allow")`` so:
  * Callers can pass snake_case keys (the wire form) or Pythonic field names.
  * Forward-compatible fields added server-side (e.g. Task 1.10+ payloads)
    don't break deserialization — we keep them on the object verbatim.
* ``metadata`` is typed as ``dict[str, Any]`` because the Rust side is
  ``serde_json::Value`` — servers stamp arbitrary keys (``agent_id``,
  ``scope``, ``governance``, ``imported_from_agent_id``, …) into it.
* Timestamps are RFC3339 strings on the wire; we keep them as ``str`` to
  avoid clobbering fractional seconds / timezone offsets produced by Rust's
  ``chrono``. Callers that want ``datetime`` can call
  ``datetime.fromisoformat(...)`` after Python 3.11 (3.10 needs
  ``dateutil`` for ``Z`` offsets).
* ``Optional[T]`` is used for every field the Rust struct declares as
  ``Option<T>``; all other fields are required. ``CreateMemory`` mirrors
  the server-side defaults so SDK callers can omit tier/namespace/etc.
"""

from __future__ import annotations

from enum import Enum
from typing import Any

from pydantic import BaseModel, ConfigDict, Field


class _Base(BaseModel):
    """Shared config: populate by alias or field name, keep unknown keys."""

    model_config = ConfigDict(populate_by_name=True, extra="allow")


class Tier(str, Enum):
    """Memory tier — mirrors ``enum Tier`` in ``src/models.rs``."""

    SHORT = "short"
    MID = "mid"
    LONG = "long"


class GovernanceLevel(str, Enum):
    """``enum GovernanceLevel`` — who may perform a governed action."""

    ANY = "any"
    REGISTERED = "registered"
    OWNER = "owner"
    APPROVE = "approve"


class ApproverType(_Base):
    """``enum ApproverType`` — serialized externally tagged.

    The Rust enum serializes three shapes:

    * ``"human"`` — bare string
    * ``{"agent": "<id>"}`` — single-key object
    * ``{"consensus": <n>}`` — single-key object

    We model it as a struct with two optional fields + a ``kind`` tag so
    callers can introspect. Use :meth:`to_wire` to emit the externally
    tagged JSON form.
    """

    kind: str = Field(description="human | agent | consensus")
    agent_id: str | None = None
    consensus: int | None = None

    @classmethod
    def human(cls) -> ApproverType:
        return cls(kind="human")

    @classmethod
    def agent(cls, agent_id: str) -> ApproverType:
        return cls(kind="agent", agent_id=agent_id)

    @classmethod
    def consensus_of(cls, n: int) -> ApproverType:
        return cls(kind="consensus", consensus=n)

    def to_wire(self) -> Any:
        if self.kind == "human":
            return "human"
        if self.kind == "agent":
            return {"agent": self.agent_id}
        if self.kind == "consensus":
            return {"consensus": self.consensus}
        raise ValueError(f"unknown ApproverType kind: {self.kind!r}")


class GovernancePolicy(_Base):
    """``struct GovernancePolicy`` — per-namespace action gating."""

    write: GovernanceLevel = GovernanceLevel.ANY
    promote: GovernanceLevel = GovernanceLevel.ANY
    delete: GovernanceLevel = GovernanceLevel.OWNER
    approver: ApproverType = Field(default_factory=ApproverType.human)


class Memory(_Base):
    """Full ``struct Memory`` — 15 fields.

    Every field present on the Rust side is mapped here. ``metadata`` is
    a free-form ``dict`` since the server stores ``serde_json::Value``.
    """

    id: str
    tier: Tier
    namespace: str
    title: str
    content: str
    tags: list[str] = Field(default_factory=list)
    priority: int = 5
    confidence: float = 1.0
    source: str = "api"
    access_count: int = 0
    created_at: str
    updated_at: str
    last_accessed_at: str | None = None
    expires_at: str | None = None
    metadata: dict[str, Any] = Field(default_factory=dict)


class MemoryLink(_Base):
    """``struct MemoryLink`` — typed directional relationship."""

    source_id: str
    target_id: str
    relation: str = "related_to"
    created_at: str


class CreateMemory(_Base):
    """Request body for ``POST /api/v1/memories``.

    Mirrors ``struct CreateMemory`` including server-side defaults. The
    server will stamp ``metadata.agent_id`` from the body, the
    ``X-Agent-Id`` header, or a per-request anonymous id — callers only
    need to set it when they want a specific NHI claim.
    """

    title: str
    content: str
    tier: Tier = Tier.MID
    namespace: str = "global"
    tags: list[str] = Field(default_factory=list)
    priority: int = 5
    confidence: float = 1.0
    source: str = "api"
    expires_at: str | None = None
    ttl_secs: int | None = None
    metadata: dict[str, Any] = Field(default_factory=dict)
    agent_id: str | None = None
    scope: str | None = None


class UpdateMemory(_Base):
    """Request body for ``PUT /api/v1/memories/{id}`` — all fields optional."""

    title: str | None = None
    content: str | None = None
    tier: Tier | None = None
    namespace: str | None = None
    tags: list[str] | None = None
    priority: int | None = None
    confidence: float | None = None
    expires_at: str | None = None
    metadata: dict[str, Any] | None = None


class RecallRequest(_Base):
    """Body of ``POST /api/v1/recall`` (and query params of the GET form)."""

    context: str
    namespace: str | None = None
    limit: int | None = 10
    tags: str | None = None
    since: str | None = None
    until: str | None = None
    as_agent: str | None = None
    budget_tokens: int | None = None


class RecallResponse(_Base):
    """Typed wrapper around the recall response payload.

    The server currently returns ``{"count": N, "memories": [Memory, ...]}``.
    We keep both fields optional on the wrapper so a future version can add
    rerank scores / explanations without breaking deserialization.
    """

    count: int = 0
    memories: list[Memory] = Field(default_factory=list)


class AgentRegistration(_Base):
    """``struct AgentRegistration`` — one row from ``GET /api/v1/agents``."""

    agent_id: str
    agent_type: str
    capabilities: list[str] = Field(default_factory=list)
    registered_at: str
    last_seen_at: str


class PendingAction(_Base):
    """``struct PendingAction`` — governance-queued action."""

    id: str
    action_type: str
    memory_id: str | None = None
    namespace: str
    payload: dict[str, Any] = Field(default_factory=dict)
    requested_by: str
    requested_at: str
    status: str
    decided_by: str | None = None
    decided_at: str | None = None
    approvals: list[dict[str, Any]] = Field(default_factory=list)


class Stats(_Base):
    """``struct Stats`` — output of ``GET /api/v1/stats``."""

    total: int
    by_tier: list[dict[str, Any]] = Field(default_factory=list)
    by_namespace: list[dict[str, Any]] = Field(default_factory=list)
    expiring_soon: int = 0
    links_count: int = 0
    db_size_bytes: int = 0


# ---------------------------------------------------------------------------
# Subscriptions / webhooks / inbox / cluster
#
# These endpoints may not be merged on every server — the models are kept
# loose (extra=allow) so the SDK can target in-flight server branches
# without breaking when fields shift. Requests use snake_case to match the
# existing Rust serde conventions.
# ---------------------------------------------------------------------------


class SubscriptionRequest(_Base):
    """Body for ``POST /api/v1/subscriptions``.

    Subscribers receive webhook deliveries signed with ``secret`` via
    HMAC-SHA256 (see :mod:`ai_memory.webhooks`).
    """

    url: str
    events: list[str] = Field(default_factory=list)
    namespace: str | None = None
    secret: str | None = None
    filter: dict[str, Any] | None = None


class Subscription(_Base):
    """``GET /api/v1/subscriptions`` row."""

    id: str
    url: str
    events: list[str] = Field(default_factory=list)
    namespace: str | None = None
    created_at: str | None = None


class NotifyRequest(_Base):
    """Body for ``POST /api/v1/notify`` — agent-to-agent message."""

    to: str
    subject: str
    body: str
    namespace: str | None = None
    metadata: dict[str, Any] = Field(default_factory=dict)


class InboxMessage(_Base):
    """Row from ``GET /api/v1/inbox``."""

    id: str
    from_: str = Field(alias="from")
    to: str
    subject: str
    body: str
    received_at: str
    read: bool = False


class BulkCreateResponse(_Base):
    """Response envelope for ``POST /api/v1/memories/bulk``."""

    created: int = 0
    ids: list[str] = Field(default_factory=list)
    errors: list[dict[str, Any]] = Field(default_factory=list)
