// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

/**
 * Error hierarchy mirroring `src/errors.rs` `MemoryError`:
 *
 * - `NOT_FOUND`        → 404 → {@link NotFoundError}
 * - `VALIDATION_FAILED`→ 400 → {@link ValidationError}
 * - `CONFLICT`         → 409 → {@link ConflictError}
 * - `DATABASE_ERROR`   → 500 → {@link ServerError}
 *
 * Auth failures surface as {@link UnauthorizedError} (401, from the
 * `api_key_auth` middleware layer — not part of the Rust `MemoryError` enum).
 */

/** Wire shape of the JSON error body (see `src/errors.rs::ApiError`). */
export interface ApiErrorBody {
  code?: string;
  message?: string;
  /** Some handlers emit `error: "..."` instead of `message`. */
  error?: string;
}

export class ApiError extends Error {
  public readonly status: number;
  public readonly code: string;
  public readonly body: ApiErrorBody | string | undefined;
  public readonly url: string;

  constructor(
    message: string,
    opts: {
      status: number;
      code: string;
      body?: ApiErrorBody | string;
      url: string;
    },
  ) {
    super(message);
    this.name = "ApiError";
    this.status = opts.status;
    this.code = opts.code;
    this.body = opts.body;
    this.url = opts.url;
    // Preserve prototype chain across transpile targets.
    Object.setPrototypeOf(this, new.target.prototype);
  }
}

export class ValidationError extends ApiError {
  constructor(message: string, opts: { body?: ApiErrorBody | string; url: string }) {
    super(message, { status: 400, code: "VALIDATION_FAILED", ...opts });
    this.name = "ValidationError";
    Object.setPrototypeOf(this, ValidationError.prototype);
  }
}

export class UnauthorizedError extends ApiError {
  constructor(message: string, opts: { body?: ApiErrorBody | string; url: string }) {
    super(message, { status: 401, code: "UNAUTHORIZED", ...opts });
    this.name = "UnauthorizedError";
    Object.setPrototypeOf(this, UnauthorizedError.prototype);
  }
}

export class NotFoundError extends ApiError {
  constructor(message: string, opts: { body?: ApiErrorBody | string; url: string }) {
    super(message, { status: 404, code: "NOT_FOUND", ...opts });
    this.name = "NotFoundError";
    Object.setPrototypeOf(this, NotFoundError.prototype);
  }
}

export class ConflictError extends ApiError {
  constructor(message: string, opts: { body?: ApiErrorBody | string; url: string }) {
    super(message, { status: 409, code: "CONFLICT", ...opts });
    this.name = "ConflictError";
    Object.setPrototypeOf(this, ConflictError.prototype);
  }
}

export class ServerError extends ApiError {
  constructor(
    message: string,
    opts: { status?: number; body?: ApiErrorBody | string; url: string },
  ) {
    super(message, {
      status: opts.status ?? 500,
      code: "DATABASE_ERROR",
      body: opts.body,
      url: opts.url,
    });
    this.name = "ServerError";
    Object.setPrototypeOf(this, ServerError.prototype);
  }
}

export class NetworkError extends ApiError {
  constructor(message: string, opts: { url: string; cause?: unknown }) {
    super(message, { status: 0, code: "NETWORK_ERROR", url: opts.url });
    this.name = "NetworkError";
    if (opts.cause !== undefined) {
      (this as unknown as { cause: unknown }).cause = opts.cause;
    }
    Object.setPrototypeOf(this, NetworkError.prototype);
  }
}

/**
 * Factory: inspect HTTP status + body and return the most specific error
 * subclass. Never throws on its own — always returns an `ApiError`.
 */
export function apiErrorFromResponse(
  status: number,
  url: string,
  body: ApiErrorBody | string | undefined,
): ApiError {
  const message =
    (typeof body === "object" && body && (body.message ?? body.error)) ||
    (typeof body === "string" ? body : undefined) ||
    `HTTP ${status}`;
  const opts = { body, url } as const;
  switch (status) {
    case 400:
      return new ValidationError(message, opts);
    case 401:
    case 403:
      return new UnauthorizedError(message, opts);
    case 404:
      return new NotFoundError(message, opts);
    case 409:
      return new ConflictError(message, opts);
    default:
      if (status >= 500) {
        return new ServerError(message, { ...opts, status });
      }
      return new ApiError(message, {
        status,
        code: (typeof body === "object" && body?.code) || "HTTP_ERROR",
        body,
        url,
      });
  }
}
