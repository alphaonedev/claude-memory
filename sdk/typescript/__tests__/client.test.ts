// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

/**
 * Integration tests for `AiMemoryClient`.
 *
 * These tests hit a real ai-memory daemon at `AI_MEMORY_TEST_URL`
 * (default `http://localhost:9077`). The entire block is skipped unless
 * `AI_MEMORY_TEST_DAEMON=1` is set, so CI without a daemon stays green.
 *
 * Start a daemon for local testing:
 *
 * ```bash
 * AI_MEMORY_NO_CONFIG=1 cargo run -- daemon --port 9077
 * AI_MEMORY_TEST_DAEMON=1 npm test
 * ```
 */

import { AiMemoryClient } from "../src/client.js";
import { verifyWebhookSignature, signWebhookBody } from "../src/webhooks.js";
import { ValidationError, NotFoundError } from "../src/errors.js";

const BASE_URL = process.env.AI_MEMORY_TEST_URL ?? "http://localhost:9077";
const DAEMON_ENABLED = process.env.AI_MEMORY_TEST_DAEMON === "1";
const describeIntegration = DAEMON_ENABLED ? describe : describe.skip;

// ---------------------------------------------------------------------------
// Pure unit tests — always run. Do not require a daemon.
// ---------------------------------------------------------------------------

describe("webhooks", () => {
  test("verifyWebhookSignature round-trips", () => {
    const body = JSON.stringify({ event: "memory.stored", id: "abc" });
    const secret = "s3kr3t";
    const sig = signWebhookBody(body, secret);
    expect(sig.startsWith("sha256=")).toBe(true);
    expect(verifyWebhookSignature(body, sig, secret)).toBe(true);
  });

  test("rejects wrong secret", () => {
    const body = "hello";
    const sig = signWebhookBody(body, "correct");
    expect(verifyWebhookSignature(body, sig, "wrong")).toBe(false);
  });

  test("rejects tampered body", () => {
    const sig = signWebhookBody("original", "k");
    expect(verifyWebhookSignature("tampered", sig, "k")).toBe(false);
  });

  test("accepts bare hex (no sha256= prefix)", () => {
    const body = "payload";
    const sig = signWebhookBody(body, "k").replace("sha256=", "");
    expect(verifyWebhookSignature(body, sig, "k")).toBe(true);
  });

  test("rejects empty / malformed signatures", () => {
    expect(verifyWebhookSignature("b", "", "k")).toBe(false);
    expect(verifyWebhookSignature("b", "sha256=zzzz", "k")).toBe(false);
    expect(verifyWebhookSignature("b", "sha256=abc", "k")).toBe(false); // odd length
  });
});

describe("AiMemoryClient constructor", () => {
  test("requires baseUrl", () => {
    expect(() => new AiMemoryClient({ baseUrl: "" })).toThrow();
  });

  test("strips trailing slash", () => {
    const c = new AiMemoryClient({ baseUrl: "http://localhost:9077/" });
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    expect((c as any).baseUrl).toBe("http://localhost:9077");
  });
});

// ---------------------------------------------------------------------------
// Live integration tests — opt-in via AI_MEMORY_TEST_DAEMON=1.
// ---------------------------------------------------------------------------

describeIntegration("AiMemoryClient (live daemon)", () => {
  const client = new AiMemoryClient({
    baseUrl: BASE_URL,
    apiKey: process.env.AI_MEMORY_TEST_API_KEY,
    agentId: "sdk-test-agent",
  });

  let createdId: string | undefined;

  test("health", async () => {
    const h = await client.health();
    expect(["ok", "error"]).toContain(h.status);
  });

  test("store + get + delete round-trip", async () => {
    const created = await client.store({
      title: `sdk-test-${Date.now()}`,
      content: "integration test memory",
      tier: "short",
      namespace: "sdk-tests",
      tags: ["sdk", "test"],
    });
    expect(created.id).toBeDefined();
    createdId = created.id;

    const fetched = await client.get(created.id);
    expect(fetched.id).toBe(created.id);
    expect(fetched.title).toBe(created.title);

    const del = await client.delete(created.id);
    expect(del.deleted).toBe(true);
    createdId = undefined;
  });

  test("validation error maps to ValidationError", async () => {
    await expect(
      client.store({ title: "", content: "empty title fails" }),
    ).rejects.toBeInstanceOf(ValidationError);
  });

  test("not found maps to NotFoundError", async () => {
    await expect(client.get("does-not-exist-xyz")).rejects.toBeInstanceOf(
      NotFoundError,
    );
  });

  test("recall returns scored results", async () => {
    const stored = await client.store({
      title: "recall test fixture",
      content: "the quick brown fox jumps over the lazy dog",
      namespace: "sdk-tests",
    });
    try {
      const r = await client.recall({
        context: "quick fox",
        namespace: "sdk-tests",
        limit: 5,
      });
      expect(Array.isArray(r.memories)).toBe(true);
      expect(typeof r.tokens_used).toBe("number");
    } finally {
      await client.delete(stored.id);
    }
  });

  test("search", async () => {
    const r = await client.search({ q: "test", namespace: "sdk-tests", limit: 5 });
    expect(Array.isArray(r.results)).toBe(true);
    expect(r.query).toBe("test");
  });

  test("stats", async () => {
    const s = await client.stats();
    expect(typeof s.total).toBe("number");
    expect(Array.isArray(s.by_tier)).toBe(true);
  });

  afterAll(async () => {
    if (createdId) {
      try {
        await client.delete(createdId);
      } catch {
        // best-effort
      }
    }
  });
});
