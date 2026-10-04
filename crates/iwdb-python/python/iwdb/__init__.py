"""Ironweaver DB: a durable graph store, embedded in a directory
(`iwdb.Store.open`) or served by `iwdb-server` (`iwdb.connect`).

The API is described in documentation/python-api.md of the repository; both
kinds of store have the same one (ADR 0035). `iwdb.aio` wraps it for
asyncio.
"""

import os
from typing import Optional, Union

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

PathLike = Union[str, "os.PathLike[str]"]


def connect(
    endpoint: str,
    *,
    token: Optional[str] = None,
    user: Optional[str] = None,
    password: Optional[str] = None,
    ca: Optional[PathLike] = None,
    cert: Optional[PathLike] = None,
    key: Optional[PathLike] = None,
) -> Store:
    """A store served by the `iwdb-server` at `endpoint` (`"https://host:port"`,
    or `"http://host:port"` for a server without TLS): the API of
    `Store.open`'s store, minus the calls that need the store's directory. It
    connects on the first call, and again after a lost connection; without a
    server (or with a TLS handshake that fails: a certificate of another CA,
    an expired one), calls raise `UnavailableError`.

    TLS: `ca` is the PEM file of the CA the server's certificate is verified
    against (default: the system's trust store). `cert` and `key` are PEM
    files of a client certificate: on a server that verifies client
    certificates (mTLS), it authenticates as the user it names, with no token
    or password needed.

    With authentication on (the server's default), pass a `token` (an API
    token, or a session's), or a `user` and `password`: that logs in now
    (`UnauthenticatedError` if they are wrong) and keeps the session's token,
    which lasts until the server's session lifetime ends; then calls raise
    `UnauthenticatedError` and you connect again. A call the user's roles
    don't allow raises `PermissionDeniedError`."""
    return Store.connect(
        endpoint, token=token, user=user, password=password, ca=ca, cert=cert, key=key
    )


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
