// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

/**
 * `AiMemoryClient` — thin typed wrapper over the ai-memory HTTP API.
 *
 * The client uses `undici.fetch` under the hood. On Node 20+ `undici` is
 * already bundled as the platform fetch implementation, so this works in
 * both Node and the browser (with a WHATWG fetch polyfill).
 *
 * Auth:
 * - `apiKey` is sent as `X-API-Key` (see `handlers::api_key_auth`).
 * - `agentId` is sent as `X-Agent-Id`. Per-call `agentId` overrides the
 *   constructor default. `.store()` also accepts a body-level `agent_id`
 *   which the server prefers over the header (precedence documented in
 *   `docs/CLAUDE.md` Agent Identity §).
 *
 * mTLS: terminate TLS with client certificates at the reverse proxy (nginx,
 * Caddy, Envoy) in front of ai-memory. The daemon itself is HTTP-only; mTLS
 * is an edge concern. See README for a suggested nginx config.
 */

import { fetch as undiciFetch, Agent, type RequestInit } from "undici";

import { apiErrorFromResponse, NetworkError } from "./errors.js";
import type {
  AgentRegistration,
  ClientOptions,
  ClusterRequest,
  ClusterResponse,
  CreateMemoryRequest,
  CreateSubscriptionRequest,
  ForgetRequest,
  GrantRequest,
  HealthResponse,
  InboxQuery,
  InboxResponse,
  LinkRequest,
  ListQuery,
  ListResponse,
  Memory,
  MemoryLink,
  MetricsResponse,
  NotifyRequest,
  RecallRequest,
  RecallResponse,
  RegisterAgentRequest,
  RequestOptions,
  RevokeRequest,
  SearchQuery,
  SearchResponse,
  Stats,
  Subscription,
  ListSubscriptionsResponse,
  UpdateMemoryRequest,
} from "./types.js";

type FetchImpl = typeof undiciFetch;

/** Minimal JSON type for generic bodies. */
type JsonValue =
  | string
  | number
  | boolean
  | null
  | JsonValue[]
  | { [k: string]: JsonValue };

/** Internal fetch-call options. */
interface CallOptions<TBody = unknown> {
  method: "GET" | "POST" | "PUT" | "DELETE";
  path: string;
  query?: Record<string, string | number | boolean | undefined>;
  body?: TBody;
  requestOpts?: RequestOptions;
  /** If true, return the raw text body (for Prometheus metrics). */
  asText?: boolean;
}

export class AiMemoryClient {
  private readonly baseUrl: string;
  private readonly apiKey: string | undefined;
  private readonly agentId: string | undefined;
  private readonly defaultHeaders: Record<string, string>;
  private readonly timeoutMs: number;
  private readonly fetchImpl: FetchImpl;
  private readonly dispatcher: Agent | undefined;

  constructor(opts: ClientOptions, fetchImpl?: FetchImpl) {
    if (!opts.baseUrl) {
      throw new Error("AiMemoryClient: baseUrl is required");
    }
    this.baseUrl = opts.baseUrl.replace(/\/+$/, "");
    this.apiKey = opts.apiKey;
    this.agentId = opts.agentId;
    this.defaultHeaders = opts.headers ?? {};
    this.timeoutMs = opts.timeoutMs ?? 30_000;
    this.fetchImpl = fetchImpl ?? (undiciFetch as FetchImpl);
    // A shared Agent gives keep-alive across calls.
    this.dispatcher =
      typeof Agent === "function"
        ? new Agent({ connectTimeout: this.timeoutMs })
        : undefined;
  }

  // ---- HTTP plumbing ------------------------------------------------------

  private buildUrl(
    path: string,
    query?: Record<string, string | number | boolean | undefined>,
  ): string {
    const url = new URL(`${this.baseUrl}${path}`);
    if (query) {
      for (const [k, v] of Object.entries(query)) {
        if (v === undefined || v === null) continue;
        url.searchParams.set(k, String(v));
      }
    }
    return url.toString();
  }

  private buildHeaders(
    requestOpts?: RequestOptions,
    hasBody?: boolean,
  ): Record<string, string> {
    const headers: Record<string, string> = { ...this.defaultHeaders };
    if (hasBody) headers["content-type"] = "application/json";
    headers["accept"] = "application/json";
    if (this.apiKey) headers["x-api-key"] = this.apiKey;
    const effectiveAgentId = requestOpts?.agentId ?? this.agentId;
    if (effectiveAgentId) headers["x-agent-id"] = effectiveAgentId;
    if (requestOpts?.headers) {
      for (const [k, v] of Object.entries(requestOpts.headers)) {
        headers[k.toLowerCase()] = v;
      }
    }
    return headers;
  }

  private async call<TResp, TBody = unknown>(
    opts: CallOptions<TBody>,
  ): Promise<TResp> {
    const url = this.buildUrl(opts.path, opts.query);
    const hasBody = opts.body !== undefined && opts.body !== null;
    const headers = this.buildHeaders(opts.requestOpts, hasBody);

    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), this.timeoutMs);
    const externalSignal = opts.requestOpts?.signal;
    if (externalSignal) {
      if (externalSignal.aborted) controller.abort();
      else externalSignal.addEventListener("abort", () => controller.abort(), { once: true });
    }

    const init: RequestInit = {
      method: opts.method,
      headers,
      signal: controller.signal,
    };
    if (this.dispatcher) {
      (init as RequestInit & { dispatcher?: Agent }).dispatcher = this.dispatcher;
    }
    if (hasBody) init.body = JSON.stringify(opts.body);

    let res: Awaited<ReturnType<FetchImpl>>;
    try {
      res = await this.fetchImpl(url, init);
    } catch (err) {
      clearTimeout(timeout);
      throw new NetworkError(
        err instanceof Error ? err.message : "network error",
        { url, cause: err },
      );
    }
    clearTimeout(timeout);

    if (opts.asText) {
      const text = await res.text();
      if (!res.ok) {
        throw apiErrorFromResponse(res.status, url, text);
      }
      const contentType = res.headers.get("content-type") ?? "text/plain";
      return { body: text, content_type: contentType } as unknown as TResp;
    }

    // Some endpoints (health) may return 503 with JSON — still parse as JSON.
    const contentType = res.headers.get("content-type") ?? "";
    let parsed: unknown = undefined;
    if (contentType.includes("application/json")) {
      try {
        parsed = await res.json();
      } catch {
        // fall through — treat as no body
      }
    } else {
      try {
        const text = await res.text();
        parsed = text.length > 0 ? text : undefined;
      } catch {
        parsed = undefined;
      }
    }

    if (!res.ok) {
      throw apiErrorFromResponse(
        res.status,
        url,
        parsed as Parameters<typeof apiErrorFromResponse>[2],
      );
    }

    return parsed as TResp;
  }

  // ========================================================================
  // Memory CRUD
  // ========================================================================

  /** `POST /api/v1/memories` — create a new memory. */
  async store(
    body: CreateMemoryRequest,
    opts?: RequestOptions,
  ): Promise<Memory> {
    return this.call<Memory, CreateMemoryRequest>({
      method: "POST",
      path: "/api/v1/memories",
      body,
      requestOpts: opts,
    });
  }

  /** `POST /api/v1/memories/bulk` — batch insert (<=1000). */
  async storeBulk(
    memories: CreateMemoryRequest[],
    opts?: RequestOptions,
  ): Promise<{ created: Memory[]; count: number }> {
    return this.call({
      method: "POST",
      path: "/api/v1/memories/bulk",
      body: { memories },
      requestOpts: opts,
    });
  }

  /** `GET /api/v1/memories/:id` — fetch by id. */
  async get(id: string, opts?: RequestOptions): Promise<Memory> {
    return this.call<Memory>({
      method: "GET",
      path: `/api/v1/memories/${encodeURIComponent(id)}`,
      requestOpts: opts,
    });
  }

  /** `PUT /api/v1/memories/:id`. */
  async update(
    id: string,
    body: UpdateMemoryRequest,
    opts?: RequestOptions,
  ): Promise<Memory> {
    return this.call<Memory, UpdateMemoryRequest>({
      method: "PUT",
      path: `/api/v1/memories/${encodeURIComponent(id)}`,
      body,
      requestOpts: opts,
    });
  }

  /** `DELETE /api/v1/memories/:id`. */
  async delete(id: string, opts?: RequestOptions): Promise<{ deleted: boolean }> {
    return this.call<{ deleted: boolean }>({
      method: "DELETE",
      path: `/api/v1/memories/${encodeURIComponent(id)}`,
      requestOpts: opts,
    });
  }

  /** `POST /api/v1/memories/:id/promote` — promote to long tier. */
  async promote(id: string, opts?: RequestOptions): Promise<Memory> {
    return this.call<Memory>({
      method: "POST",
      path: `/api/v1/memories/${encodeURIComponent(id)}/promote`,
      requestOpts: opts,
    });
  }

  /** `GET /api/v1/memories` — paginated list with filters. */
  async list(query?: ListQuery, opts?: RequestOptions): Promise<ListResponse> {
    return this.call<ListResponse>({
      method: "GET",
      path: "/api/v1/memories",
      query: query as Record<string, string | number | boolean | undefined>,
      requestOpts: opts,
    });
  }

  // ========================================================================
  // Recall / search / forget
  // ========================================================================

  /** `POST /api/v1/recall` — fuzzy hybrid recall (mutates access_count + TTL). */
  async recall(
    body: RecallRequest,
    opts?: RequestOptions,
  ): Promise<RecallResponse> {
    return this.call<RecallResponse, RecallRequest>({
      method: "POST",
      path: "/api/v1/recall",
      body,
      requestOpts: opts,
    });
  }

  /** `GET /api/v1/search` — AND keyword search (read-only). */
  async search(
    query: SearchQuery,
    opts?: RequestOptions,
  ): Promise<SearchResponse> {
    return this.call<SearchResponse>({
      method: "GET",
      path: "/api/v1/search",
      query: query as unknown as Record<string, string | number | boolean | undefined>,
      requestOpts: opts,
    });
  }

  /** `POST /api/v1/forget` — bulk delete by pattern/namespace/tier. */
  async forget(
    body: ForgetRequest,
    opts?: RequestOptions,
  ): Promise<{ deleted: number }> {
    return this.call<{ deleted: number }, ForgetRequest>({
      method: "POST",
      path: "/api/v1/forget",
      body,
      requestOpts: opts,
    });
  }

  // ========================================================================
  // Links
  // ========================================================================

  /** `POST /api/v1/links` — link two memories. */
  async link(body: LinkRequest, opts?: RequestOptions): Promise<MemoryLink> {
    return this.call<MemoryLink, LinkRequest>({
      method: "POST",
      path: "/api/v1/links",
      body,
      requestOpts: opts,
    });
  }

  /** `GET /api/v1/links/:id` — fetch all links for a memory. */
  async getLinks(
    id: string,
    opts?: RequestOptions,
  ): Promise<{ links: MemoryLink[]; count: number }> {
    return this.call({
      method: "GET",
      path: `/api/v1/links/${encodeURIComponent(id)}`,
      requestOpts: opts,
    });
  }

  // ========================================================================
  // Stats / health / namespaces
  // ========================================================================

  /** `GET /api/v1/health` — liveness + backend probe. */
  async health(opts?: RequestOptions): Promise<HealthResponse> {
    return this.call<HealthResponse>({
      method: "GET",
      path: "/api/v1/health",
      requestOpts: opts,
    });
  }

  /** `GET /api/v1/stats` — aggregate stats. */
  async stats(opts?: RequestOptions): Promise<Stats> {
    return this.call<Stats>({
      method: "GET",
      path: "/api/v1/stats",
      requestOpts: opts,
    });
  }

  /** `GET /api/v1/namespaces`. */
  async namespaces(opts?: RequestOptions): Promise<{ namespaces: { namespace: string; count: number }[] }> {
    return this.call({
      method: "GET",
      path: "/api/v1/namespaces",
      requestOpts: opts,
    });
  }

  // ========================================================================
  // Agents (registered NHI identities)
  // ========================================================================

  /** `GET /api/v1/agents` — list registered agents. */
  async agents(opts?: RequestOptions): Promise<{ agents: AgentRegistration[] }> {
    return this.call({
      method: "GET",
      path: "/api/v1/agents",
      requestOpts: opts,
    });
  }

  /** `POST /api/v1/agents` — register a new agent. */
  async registerAgent(
    body: RegisterAgentRequest,
    opts?: RequestOptions,
  ): Promise<AgentRegistration> {
    return this.call<AgentRegistration, RegisterAgentRequest>({
      method: "POST",
      path: "/api/v1/agents",
      body,
      requestOpts: opts,
    });
  }

  // ========================================================================
  // v0.6.0.0 new endpoints — subscriptions, notify/inbox, grant/revoke,
  // cluster, Prometheus metrics. Some may not be merged server-side yet.
  // ========================================================================

  /** `GET /api/v1/metrics` — Prometheus text-format exposition. */
  async metrics(opts?: RequestOptions): Promise<MetricsResponse> {
    return this.call<MetricsResponse>({
      method: "GET",
      path: "/api/v1/metrics",
      requestOpts: opts,
      asText: true,
    });
  }

  /** `POST /api/v1/subscriptions` — register a webhook. */
  async subscribe(
    body: CreateSubscriptionRequest,
    opts?: RequestOptions,
  ): Promise<Subscription> {
    return this.call<Subscription, CreateSubscriptionRequest>({
      method: "POST",
      path: "/api/v1/subscriptions",
      body,
      requestOpts: opts,
    });
  }

  /** `DELETE /api/v1/subscriptions/:id` — remove a webhook. */
  async unsubscribe(
    id: string,
    opts?: RequestOptions,
  ): Promise<{ deleted: boolean }> {
    return this.call<{ deleted: boolean }>({
      method: "DELETE",
      path: `/api/v1/subscriptions/${encodeURIComponent(id)}`,
      requestOpts: opts,
    });
  }

  /** `GET /api/v1/subscriptions` — list current webhooks. */
  async listSubscriptions(
    opts?: RequestOptions,
  ): Promise<ListSubscriptionsResponse> {
    return this.call<ListSubscriptionsResponse>({
      method: "GET",
      path: "/api/v1/subscriptions",
      requestOpts: opts,
    });
  }

  /** `POST /api/v1/memories/:id/grant` — grant another agent access. */
  async grant(
    memoryId: string,
    body: GrantRequest,
    opts?: RequestOptions,
  ): Promise<{ granted: boolean }> {
    return this.call<{ granted: boolean }, GrantRequest>({
      method: "POST",
      path: `/api/v1/memories/${encodeURIComponent(memoryId)}/grant`,
      body,
      requestOpts: opts,
    });
  }

  /** `POST /api/v1/memories/:id/revoke` — revoke access. */
  async revoke(
    memoryId: string,
    body: RevokeRequest,
    opts?: RequestOptions,
  ): Promise<{ revoked: boolean }> {
    return this.call<{ revoked: boolean }, RevokeRequest>({
      method: "POST",
      path: `/api/v1/memories/${encodeURIComponent(memoryId)}/revoke`,
      body,
      requestOpts: opts,
    });
  }

  /** `POST /api/v1/notify` — send a message to another agent's inbox. */
  async notify(
    body: NotifyRequest,
    opts?: RequestOptions,
  ): Promise<{ id: string; sent: boolean }> {
    return this.call<{ id: string; sent: boolean }, NotifyRequest>({
      method: "POST",
      path: "/api/v1/notify",
      body,
      requestOpts: opts,
    });
  }

  /** `GET /api/v1/inbox` — fetch the calling agent's inbox. */
  async inbox(
    query?: InboxQuery,
    opts?: RequestOptions,
  ): Promise<InboxResponse> {
    return this.call<InboxResponse>({
      method: "GET",
      path: "/api/v1/inbox",
      query: query as Record<string, string | number | boolean | undefined>,
      requestOpts: opts,
    });
  }

  /** `POST /api/v1/cluster` — peer management (join/leave/list/status). */
  async cluster(
    body: ClusterRequest,
    opts?: RequestOptions,
  ): Promise<ClusterResponse> {
    return this.call<ClusterResponse, ClusterRequest>({
      method: "POST",
      path: "/api/v1/cluster",
      body,
      requestOpts: opts,
    });
  }

  // ========================================================================
  // Low-level escape hatch
  // ========================================================================

  /**
   * Raw request for endpoints not yet wrapped. Returns parsed JSON with no
   * type refinement — cast at call site.
   */
  async raw<T = unknown>(
    method: "GET" | "POST" | "PUT" | "DELETE",
    path: string,
    body?: JsonValue,
    opts?: RequestOptions,
  ): Promise<T> {
    return this.call<T>({
      method,
      path,
      ...(body !== undefined ? { body } : {}),
      requestOpts: opts,
    });
  }
}
