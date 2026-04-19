// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

/**
 * Webhook signature verification for ai-memory subscription callbacks.
 *
 * The ai-memory daemon signs webhook bodies with HMAC-SHA256 using the
 * per-subscription secret, and delivers the hex digest in the
 * `X-AI-Memory-Signature` header (format: `sha256=<hex>`). SDK users call
 * {@link verifyWebhookSignature} inside their HTTP handler to confirm
 * authenticity before acting on the payload.
 *
 * Uses Node's built-in `node:crypto` — no third-party crypto dependency.
 */

import { createHmac, timingSafeEqual } from "node:crypto";

/**
 * Verify an HMAC-SHA256 webhook signature.
 *
 * @param body   Raw request body (string or Buffer). Pass the EXACT bytes
 *               the server signed — not a re-serialized JSON object.
 * @param signature The value of the `X-AI-Memory-Signature` header. Accepts
 *               both `sha256=<hex>` and bare `<hex>` forms.
 * @param secret The subscription secret established at `.subscribe()` time.
 * @returns `true` iff the signature matches; `false` otherwise.
 *          Never throws for malformed input — returns `false`.
 *
 * @example
 * ```ts
 * import { verifyWebhookSignature } from "@alphaone/ai-memory/webhooks";
 *
 * app.post("/webhook", (req, res) => {
 *   const sig = req.header("X-AI-Memory-Signature") ?? "";
 *   if (!verifyWebhookSignature(req.rawBody, sig, process.env.WEBHOOK_SECRET!)) {
 *     return res.status(401).send("bad signature");
 *   }
 *   // ... handle event
 * });
 * ```
 */
export function verifyWebhookSignature(
  body: string | Uint8Array,
  signature: string,
  secret: string,
): boolean {
  if (!signature || !secret) return false;

  const provided = signature.startsWith("sha256=")
    ? signature.slice("sha256=".length)
    : signature;

  // Hex must be non-empty and even-length; anything else rejects early.
  if (provided.length === 0 || provided.length % 2 !== 0) return false;
  if (!/^[0-9a-fA-F]+$/.test(provided)) return false;

  const expectedHex = createHmac("sha256", secret).update(body).digest("hex");

  // Length mismatch implies forgery attempt — timingSafeEqual would throw.
  if (expectedHex.length !== provided.length) return false;

  try {
    const a = Buffer.from(expectedHex, "hex");
    const b = Buffer.from(provided, "hex");
    if (a.length !== b.length) return false;
    return timingSafeEqual(a, b);
  } catch {
    return false;
  }
}

/**
 * Convenience: sign a payload. Useful for tests and local webhook
 * replay harnesses. Production SDK users should not need this.
 */
export function signWebhookBody(
  body: string | Uint8Array,
  secret: string,
): string {
  const hex = createHmac("sha256", secret).update(body).digest("hex");
  return `sha256=${hex}`;
}
