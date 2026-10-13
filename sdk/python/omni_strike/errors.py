"""Typed SDK errors."""

from __future__ import annotations

from typing import Any, Optional


class OmniStrikeError(Exception):
    """Every failure raised by the SDK.

    * HTTP errors: ``status`` is the HTTP status; ``code`` / ``message`` come from the server
      envelope ``{"error": {"code", "message"}}`` (``HTTP_<status>`` when the body is not one).
    * Client-side failures: ``status`` is 0 and ``code`` is ``TIMEOUT``, ``NETWORK_ERROR`` or
      ``INVALID_RESPONSE``.
    """

    def __init__(
        self,
        status: int,
        code: str,
        message: str,
        *,
        body: Any = None,
        retry_after: Optional[float] = None,
    ) -> None:
        super().__init__(f"[{status or 'client'} {code}] {message}")
        self.status = status
        self.code = code
        self.message = message
        self.body = body
        #: ``Retry-After`` in seconds when the server sent one.
        self.retry_after = retry_after

    @property
    def is_retryable(self) -> bool:
        """True for 429 / 503: the request may succeed later."""
        return self.status in (429, 503)

    def __repr__(self) -> str:
        return f"OmniStrikeError(status={self.status!r}, code={self.code!r}, message={self.message!r})"
