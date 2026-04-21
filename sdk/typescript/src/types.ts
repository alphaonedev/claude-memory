// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

/**
 * TypeScript mirror of `src/models.rs`. Field names match the Rust struct
 * serde output verbatim (snake_case on the wire). Keep this file in lock-step
 * with `src/models.rs` in the main repo.
 */

/** Memory tier — mirrors human memory systems (short: 6h TTL, mid: 7d, long: permanent). */
export type Tier = "short" | "mid" | "long";

/** Visibility scope (Task 1.5). Controls which agents can see the memory. */
export type Scope = "private" | "team" | "unit" | "org" | "collective";

/** Link relation kinds (closed set — server validates). */
export type Relation =
  | "related_to"
  | "supersedes"
  | "contradicts"
  | "derived_from";

/** Allowed `source` values — see `src/validate.rs` `VALID_SOURCES`. */
export type Source =
  | "user"
  | "claude"
  | "hook"
  | "api"
  | "cli"
  | "import"
  | "consolidation"
  | "system"
  | "chaos";

/**
 * A Memory row. Corresponds to `ai_memory::models::Memory` (15 fields).
 *
 * NOTE: `metadata` is `serde_json::Value` server-side — we expose it as
 * `Record<string, unknown>` on the SDK side (server validates it must be
 * a JSON object at write time).
 */
export interface Memory {
  id: string;
  tier: Tier;
  namespace: string;
  title: string;
  content: string;
  tags: string[];
  /** 1..=10 */
  priority: number;
  /** 0.0..=1.0 */
  confidence: number;
  source: string;
  access_count: number;
  /** RFC3339 */
  created_at: string;
  /** RFC3339 */
  updated_at: string;
  last_accessed_at?: string | null;
  expires_at?: string | null;
  metadata: Record<string, unknown>;
}

/** A scored Memory returned by `/recall` (Memory + `score` field). */
export interface ScoredMemory extends Memory {
  score: number;
}

/** Typed directional relationship between two memories. */
export interface MemoryLink {
  source_id: string;
  target_id: string;
  relation: string;
  created_at: string;
}

/** Body for `POST /api/v1/memories`. */
export interface CreateMemoryRequest {
  title: string;
  content: string;
  tier?: Tier;
  namespace?: string;
  tags?: string[];
  /** 1..=10 (default 5) */
  priority?: number;
  /** 0.0..=1.0 (default 1.0) */
  confidence?: number;
  source?: Source | string;
  /** RFC3339 */
  expires_at?: string;
  /** Positive, <=1 year */
  ttl_secs?: number;
  metadata?: Record<string, unknown>;
  /**
   * Optional explicit agent_id (precedence: this > `X-Agent-Id` header >
   * server-side anonymous fallback).
   */
  agent_id?: string;
  scope?: Scope;
}

/** Body for `PUT /api/v1/memories/:id`. */
export interface UpdateMemoryRequest {
  title?: string;
  content?: string;
  tier?: Tier;
  namespace?: string;
  tags?: string[];
  priority?: number;
  confidence?: number;
  expires_at?: string;
  metadata?: Record<string, unknown>;
}

/** Body for `POST /api/v1/recall`. */
export interface RecallRequest {
  context: string;
  namespace?: string;
  limit?: number;
  /** Comma-separated tag filter. */
  tags?: string;
  since?: string;
  until?: string;
  as_agent?: string;
  budget_tokens?: number;
}

/** Query for `GET /api/v1/recall`. */
export interface RecallQuery extends Partial<RecallRequest> {
  context?: string;
}

/** Response from `/recall`. */
export interface RecallResponse {
  memories: ScoredMemory[];
  count: number;
  tokens_used: number;
  budget_tokens?: number;
}

/** Query for `GET /api/v1/search`. */
export interface SearchQuery {
  q: string;
  namespace?: string;
  tier?: Tier;
  limit?: number;
  min_priority?: number;
  since?: string;
  until?: string;
  tags?: string;
  agent_id?: string;
  as_agent?: string;
}

/** Response from `/search`. */
export interface SearchResponse {
  results: Memory[];
  count: number;
  query: string;
}

/** Query for `GET /api/v1/memories`. */
export interface ListQuery {
  namespace?: string;
  tier?: Tier;
  limit?: number;
  offset?: number;
  min_priority?: number;
  since?: string;
  until?: string;
  tags?: string;
  agent_id?: string;
}

/** Response from `/memories` list. */
export interface ListResponse {
  memories: Memory[];
  count: number;
}

/** Body for `POST /api/v1/links`. */
export interface LinkRequest {
  source_id: string;
  target_id: string;
  /** Default: "related_to". */
  relation?: Relation;
}

/** Body for `POST /api/v1/forget`. */
export interface ForgetRequest {
  namespace?: string;
  pattern?: string;
  tier?: Tier;
}

export interface TierCount {
  tier: string;
  count: number;
}
export interface NamespaceCount {
  namespace: string;
  count: number;
}

export interface Stats {
  total: number;
  by_tier: TierCount[];
  by_namespace: NamespaceCount[];
  expiring_soon: number;
  links_count: number;
  db_size_bytes: number;
}

export interface HealthResponse {
  status: "ok" | "error";
  service: string;
}

/** Agent registration (Task 1.3). */
export interface AgentRegistration {
  agent_id: string;
  agent_type: string;
  capabilities: string[];
  registered_at: string;
  last_seen_at: string;
}

export interface RegisterAgentRequest {
  agent_id: string;
  agent_type: string;
  capabilities?: string[];
}

// --------------------------------------------------------------------------
// v0.6.0.0 new endpoints (target shape — some may not yet be merged in Rust)
// --------------------------------------------------------------------------

/** Webhook subscription for memory events. */
export interface Subscription {
  id: string;
  agent_id: string;
  /** Target URL that receives POSTed events. */
  callback_url: string;
  /** Event types to subscribe to (e.g. "memory.stored", "memory.updated"). */
  events: string[];
  /** HMAC-SHA256 secret for webhook signature verification. */
  secret?: string;
  /** Optional namespace filter. */
  namespace?: string;
  created_at: string;
}

export interface CreateSubscriptionRequest {
  callback_url: string;
  events: string[];
  secret?: string;
  namespace?: string;
}

export interface ListSubscriptionsResponse {
  subscriptions: Subscription[];
  count: number;
}

/** Memory ACL grant/revoke (Task 1.5 extensions). */
export interface GrantRequest {
  /** Agent receiving access. */
  agent_id: string;
  /** Permission level granted. */
  permission: "read" | "write" | "admin";
}

export interface RevokeRequest {
  agent_id: string;
}

/** Agent-to-agent notification (inbox). */
export interface NotifyRequest {
  /** Target agent_id. */
  to: string;
  subject: string;
  body: string;
  /** Optional memory_id this notification relates to. */
  memory_id?: string;
  /** Arbitrary structured payload. */
  payload?: Record<string, unknown>;
}

export interface InboxMessage {
  id: string;
  from: string;
  to: string;
  subject: string;
  body: string;
  memory_id?: string;
  payload?: Record<string, unknown>;
  read: boolean;
  created_at: string;
}

export interface InboxResponse {
  messages: InboxMessage[];
  count: number;
  unread: number;
}

export interface InboxQuery {
  /** Only return unread messages. */
  unread?: boolean;
  limit?: number;
  since?: string;
}

/** Cluster peer info. */
export interface ClusterPeer {
  agent_id: string;
  endpoint: string;
  last_seen_at: string;
  status: "healthy" | "degraded" | "unreachable";
}

export interface ClusterRequest {
  /** Action: "join", "leave", "list", "status". */
  action: "join" | "leave" | "list" | "status";
  endpoint?: string;
  agent_id?: string;
}

export interface ClusterResponse {
  peers: ClusterPeer[];
  self: ClusterPeer;
}

/** Raw Prometheus text-format payload wrapper. */
export interface MetricsResponse {
  /** Prometheus exposition format text. */
  body: string;
  content_type: string;
}

// --------------------------------------------------------------------------
// Client configuration
// --------------------------------------------------------------------------

export interface ClientOptions {
  /**
   * Base URL, e.g. `http://localhost:9077`. The `/api/v1/` prefix is added
   * by the client — pass only the scheme + host + port.
   */
  baseUrl: string;
  /** Optional API key (sent as `X-API-Key` header). */
  apiKey?: string;
  /**
   * Optional default `X-Agent-Id` header sent on every request. Can be
   * overridden per-call via request-level options.
   */
  agentId?: string;
  /** Request timeout in milliseconds. Default: 30_000. */
  timeoutMs?: number;
  /** Extra headers merged into every request. */
  headers?: Record<string, string>;
  /**
   * Optional AbortSignal — when aborted, all in-flight requests using this
   * client's fetch() invocation will abort.
   */
  signal?: AbortSignal;
}

/** Per-call overrides for any client method. */
export interface RequestOptions {
  /** Overrides the client's default agent_id header for this request. */
  agentId?: string;
  signal?: AbortSignal;
  /** Extra headers merged into this request. */
  headers?: Record<string, string>;
}
