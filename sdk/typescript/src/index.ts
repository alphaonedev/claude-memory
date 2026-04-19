// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

/**
 * `@alphaone/ai-memory` — TypeScript SDK for the ai-memory HTTP API.
 *
 * Primary entry point:
 * - {@link AiMemoryClient} — main client class.
 *
 * Also re-exports all request/response types and the error hierarchy.
 * Webhook helpers live in the `./webhooks` subpath so the SDK can ship
 * to browsers without pulling in `node:crypto` by default.
 */

export { AiMemoryClient } from "./client.js";

export * from "./types.js";

export {
  ApiError,
  ValidationError,
  UnauthorizedError,
  NotFoundError,
  ConflictError,
  ServerError,
  NetworkError,
  apiErrorFromResponse,
} from "./errors.js";

export type { ApiErrorBody } from "./errors.js";

export { verifyWebhookSignature, signWebhookBody } from "./webhooks.js";
