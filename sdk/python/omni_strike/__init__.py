"""Official Python SDK for the Omni Strike crypto options best-execution API.

Stdlib only. Decimals are ``decimal.Decimal`` (never float); errors are
:class:`OmniStrikeError` with ``status``, ``code`` and ``message``.
"""

from .client import (
    DEFAULT_BASE_URL,
    AdminClient,
    HttpResponse,
    OmniStrike,
    OmniStrikeAdmin,
    Transport,
    UrllibTransport,
    __version__,
    orders_message,
    parse_retry_after,
)
from .errors import OmniStrikeError

__all__ = [
    "AdminClient",
    "DEFAULT_BASE_URL",
    "HttpResponse",
    "OmniStrike",
    "OmniStrikeAdmin",
    "OmniStrikeError",
    "Transport",
    "UrllibTransport",
    "__version__",
    "orders_message",
    "parse_retry_after",
]
