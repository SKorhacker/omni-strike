import type { ApiErrorBody } from "./types";

export class ApiError extends Error {
  constructor(
    public status: number,
    public code: string,
    message: string,
  ) {
    super(message);
  }
}

const TIMEOUT_MS = 60_000;

export async function api<T>(path: string, init?: { method?: string; body?: unknown }): Promise<T> {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), TIMEOUT_MS);
  try {
    const resp = await fetch(`/api${path}`, {
      method: init?.method ?? "GET",
      headers: init?.body ? { "content-type": "application/json" } : undefined,
      body: init?.body ? JSON.stringify(init.body) : undefined,
      signal: ctrl.signal,
      cache: "no-store",
    });
    const text = await resp.text();
    let json: unknown = null;
    try {
      json = text ? JSON.parse(text) : null;
    } catch {
      throw new ApiError(resp.status, "BACKEND_UNREACHABLE", "Backend did not return JSON (is oa-server running?)");
    }
    if (!resp.ok) {
      const e = (json as ApiErrorBody | null)?.error;
      throw new ApiError(resp.status, e?.code ?? `HTTP_${resp.status}`, e?.message ?? text.slice(0, 300));
    }
    return json as T;
  } catch (e) {
    if (e instanceof ApiError) throw e;
    if (e instanceof DOMException && e.name === "AbortError") {
      throw new ApiError(0, "TIMEOUT", `No answer after ${TIMEOUT_MS / 1000}s`);
    }
    throw new ApiError(0, "NETWORK", String(e));
  } finally {
    clearTimeout(timer);
  }
}
