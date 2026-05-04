# Copyright 2026 AlphaOne LLC
# SPDX-License-Identifier: Apache-2.0
"""ai-memory — Python SDK for the ai-memory HTTP API.

The SDK exposes a synchronous :class:`AiMemoryClient` and an asynchronous
:class:`AsyncAiMemoryClient` over the daemon's ``/api/v1/`` REST surface
(port 9077 by default). Models mirror the Rust structs in ``src/models.rs``
and use Pydantic v2 with ``populate_by_name=True`` so callers may construct
objects using either snake_case aliases (wire form) or Pythonic names.

Example
-------
>>> from ai_memory import AiMemoryClient, Tier
>>> with AiMemoryClient(base_url="http://localhost:9077") as c:
...     created = c.store(title="hello", content="world", tier=Tier.MID)
...     hits = c.recall(context="hello")
"""

from ai_memory.async_client import AsyncAiMemoryClient
from ai_memory.client import AiMemoryClient
from ai_memory.errors import (
    AiMemoryError,
    AuthError,
    ConflictError,
    ForbiddenError,
    NotFoundError,
    RateLimitError,
    ServerError,
    TransportError,
    ValidationError,
)
from ai_memory.models import (
    AgentRegistration,
    ApproverType,
    BulkCreateResponse,
    CreateMemory,
    GovernanceLevel,
    GovernancePolicy,
    InboxMessage,
    Memory,
    MemoryLink,
    NotifyRequest,
    PendingAction,
    RecallRequest,
    RecallResponse,
    Stats,
    Subscription,
    SubscriptionRequest,
    Tier,
    UpdateMemory,
)
from ai_memory.profile import (
    ProfileNotLoaded,
    require_profile,
    require_profile_async,
    resolve_required_families,
)
from ai_memory.webhooks import verify_webhook_signature

__version__ = "0.6.0-alpha.0"

__all__ = [
    "AgentRegistration",
    "AiMemoryClient",
    "AiMemoryError",
    "ApproverType",
    "AsyncAiMemoryClient",
    "AuthError",
    "BulkCreateResponse",
    "ConflictError",
    "CreateMemory",
    "ForbiddenError",
    "GovernanceLevel",
    "GovernancePolicy",
    "InboxMessage",
    "Memory",
    "MemoryLink",
    "NotFoundError",
    "NotifyRequest",
    "PendingAction",
    "ProfileNotLoaded",
    "RateLimitError",
    "RecallRequest",
    "RecallResponse",
    "ServerError",
    "Stats",
    "Subscription",
    "SubscriptionRequest",
    "Tier",
    "TransportError",
    "UpdateMemory",
    "ValidationError",
    "__version__",
    "require_profile",
    "require_profile_async",
    "resolve_required_families",
    "verify_webhook_signature",
]
