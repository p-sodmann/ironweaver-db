"""Ironweaver DB: a durable graph store, embedded in a directory
(`iwdb.Store.open`) or served by `iwdb-server` (`iwdb.connect`).

The API is described in documentation/python-api.md of the repository; both
kinds of store have the same one (ADR 0035). `iwdb.aio` wraps it for
asyncio.
"""

from typing import Optional

from ._iwdb import (
    BudgetExceededError,
    ClosedError,
    ConflictError,
    ConstraintError,
    CorruptError,
    CursorExpiredError,
    Error,
    InternalError,
    InvalidError,
    IoError,
    LockedError,
    Namespace,
    NotFoundError,
    NotRetainedError,
    PermissionDeniedError,
    ReadOnlyError,
    Store,
    TimeoutError,
    Transaction,
    UnauthenticatedError,
    UnavailableError,
    __version__,
    restore,
    verify,
)
from .filter import Attr, Filter, attr, const, edge_type, label


def connect(
    endpoint: str,
    *,
    token: Optional[str] = None,
    user: Optional[str] = None,
    password: Optional[str] = None,
) -> Store:
    """A store served by the `iwdb-server` at `endpoint` (`"http://host:port"`):
    the API of `Store.open`'s store, minus the calls that need the store's
    directory. It connects on the first call, and again after a lost
    connection; without a server, calls raise `UnavailableError`.

    With authentication on (the server's default), pass a `token` (an API
    token, or a session's), or a `user` and `password`: that logs in now
    (`UnauthenticatedError` if they are wrong) and keeps the session's token,
    which lasts until the server's session lifetime ends; then calls raise
    `UnauthenticatedError` and you connect again. A call the user's roles
    don't allow raises `PermissionDeniedError`."""
    return Store.connect(endpoint, token=token, user=user, password=password)


__all__ = [
    "Attr",
    "BudgetExceededError",
    "ClosedError",
    "ConflictError",
    "ConstraintError",
    "CorruptError",
    "CursorExpiredError",
    "Error",
    "Filter",
    "InternalError",
    "InvalidError",
    "IoError",
    "LockedError",
    "Namespace",
    "NotFoundError",
    "NotRetainedError",
    "PermissionDeniedError",
    "ReadOnlyError",
    "Store",
    "TimeoutError",
    "Transaction",
    "UnauthenticatedError",
    "UnavailableError",
    "__version__",
    "attr",
    "connect",
    "const",
    "edge_type",
    "label",
    "restore",
    "verify",
]
