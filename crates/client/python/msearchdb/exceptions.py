"""Custom exceptions for the MSearchDB Python client."""

from __future__ import annotations


class MSearchDBError(Exception):
    """Base exception for all MSearchDB client errors."""

    def __init__(self, message: str = "", status_code: int | None = None):
        self.message = message
        self.status_code = status_code
        super().__init__(message)


class ConnectionError(MSearchDBError):
    """Raised when the client cannot connect to any MSearchDB node."""


class TimeoutError(MSearchDBError):
    """Raised when a request times out."""


class NotFoundError(MSearchDBError):
    """Raised when a requested resource (document, collection) is not found (404)."""

    def __init__(self, message: str = "Resource not found"):
        super().__init__(message, status_code=404)


class ConflictError(MSearchDBError):
    """Raised when a resource already exists (409)."""

    def __init__(self, message: str = "Resource already exists"):
        super().__init__(message, status_code=409)


class AuthenticationError(MSearchDBError):
    """Raised when authentication fails (401)."""

    def __init__(self, message: str = "Authentication failed"):
        super().__init__(message, status_code=401)


class InvalidInputError(MSearchDBError):
    """Raised when the server rejects invalid input (400)."""

    def __init__(self, message: str = "Invalid input"):
        super().__init__(message, status_code=400)


class ServiceUnavailableError(MSearchDBError):
    """Raised when the server is unavailable (503)."""

    def __init__(self, message: str = "Service unavailable"):
        super().__init__(message, status_code=503)


class BulkError(MSearchDBError):
    """Raised when a bulk operation has partial failures."""

    def __init__(self, message: str, errors: list[dict] | None = None):
        super().__init__(message)
        self.errors = errors or []


# ---------------------------------------------------------------------------
# Mapping from HTTP status codes to exception classes
# ---------------------------------------------------------------------------

_STATUS_MAP: dict[int, type[MSearchDBError]] = {
    400: InvalidInputError,
    401: AuthenticationError,
    404: NotFoundError,
    409: ConflictError,
    503: ServiceUnavailableError,
}


def raise_for_status(status_code: int, body: dict | str) -> None:
    """Raise an appropriate exception for an error HTTP status code.

    Args:
        status_code: HTTP response status code.
        body: Parsed JSON body (dict) or raw text.
    """
    if 200 <= status_code < 300:
        return

    if isinstance(body, dict):
        error_obj = body.get("error", {})
        if isinstance(error_obj, dict):
            reason = error_obj.get("reason", str(body))
        else:
            reason = str(body)
    else:
        reason = str(body)

    exc_class = _STATUS_MAP.get(status_code, MSearchDBError)
    raise exc_class(reason)
