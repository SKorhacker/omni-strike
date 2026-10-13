import type { ApiErrorCode } from "./types.js";

/**
 * Every failure raised by the SDK.
 *
 * - HTTP errors: `status` is the HTTP status, `code`/`message` come from the server envelope
 *   `{"error":{"code","message"}}` (or `HTTP_<status>` when the body is not an envelope).
 * - Client-side failures: `status` is 0 and `code` is `TIMEOUT`, `NETWORK_ERROR`,
 *   `ABORTED` or `INVALID_RESPONSE`.
 */
export class OmniStrikeError extends Error {
  readonly status: number;
  readonly code: ApiErrorCode | "TIMEOUT" | "NETWORK_ERROR" | "ABORTED" | "INVALID_RESPONSE";
  /** Raw response body (parsed JSON when possible) for diagnostics. */
  readonly body: unknown;
  /** Value of the `Retry-After` header in milliseconds, when the server sent one. */
  readonly retryAfterMs: number | undefined;

  constructor(
    status: number,
    code: OmniStrikeError["code"],
    message: string,
    options: { body?: unknown; retryAfterMs?: number; cause?: unknown } = {},
  ) {
    super(`[${status || "client"} ${code}] ${message}`);
    this.name = "OmniStrikeError";
    this.status = status;
    this.code = code;
    this.body = options.body;
    this.retryAfterMs = options.retryAfterMs;
    if (options.cause !== undefined) {
      (this as { cause?: unknown }).cause = options.cause;
    }
  }

  /** The server message without the `[status code]` prefix. */
  get serverMessage(): string {
    return this.message.replace(/^\[[^\]]*\]\s*/, "");
  }

  /** True for 429 / 503: the request may succeed if retried later. */
  get isRetryable(): boolean {
    return this.status === 429 || this.status === 503;
  }
}
