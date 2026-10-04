"""asyncio wrappers of the `iwdb` API (ADR 0035).

    store = await iwdb.aio.connect("http://127.0.0.1:7600")
    async with store:
        async with store.transaction() as tx:
            tx.upsert_node("alice", labels=["Person"])
        print(await store.find(iwdb.label("Person")))

Every call runs the sync call in a worker thread (`asyncio.to_thread`); the
calls release the GIL while they run in Rust, so the event loop goes on.
Arguments, results and exceptions are those of the sync API. Adding a
mutation to a transaction does no I/O and stays a plain call.
"""

import asyncio
import functools
from typing import Any, Optional

from ._iwdb import Namespace, Store, Transaction

__all__ = ["AsyncNamespace", "AsyncStore", "AsyncTransaction", "connect", "open"]


async def connect(
    endpoint: str,
    *,
    token: Optional[str] = None,
    user: Optional[str] = None,
    password: Optional[str] = None,
) -> "AsyncStore":
    """`iwdb.connect`, for asyncio."""
    return AsyncStore(
        await asyncio.to_thread(Store.connect, endpoint, token=token, user=user, password=password)
    )


async def open(path: Any, **options: Any) -> "AsyncStore":  # noqa: A001 (like Store.open)
    """`iwdb.Store.open`, for asyncio."""
    return AsyncStore(await asyncio.to_thread(functools.partial(Store.open, path, **options)))


class _Async:
    """Delegates every method of the wrapped object as a coroutine function."""

    _plain = frozenset()

    def __init__(self, inner: Any) -> None:
        self._inner = inner

    def __getattr__(self, name: str) -> Any:
        value = getattr(self._inner, name)
        if name in self._plain or not callable(value):
            return value

        @functools.wraps(value)
        async def call(*args: Any, **kwargs: Any) -> Any:
            return await asyncio.to_thread(functools.partial(value, *args, **kwargs))

        return call

    def __repr__(self) -> str:
        return "<async {!r}>".format(self._inner)


class AsyncStore(_Async):
    """A `Store` whose calls are coroutines; `async with` closes it."""

    _plain = frozenset({"closed"})

    def transaction(self, **options: Any) -> "AsyncTransaction":
        return AsyncTransaction(self._inner.transaction(**options))

    async def namespace(self, name: str) -> "AsyncNamespace":
        return AsyncNamespace(await asyncio.to_thread(self._inner.namespace, name))

    async def __aenter__(self) -> "AsyncStore":
        return self

    async def __aexit__(self, *exc: Any) -> bool:
        await asyncio.to_thread(self._inner.close)
        return False


class AsyncNamespace(_Async):
    """A `Namespace` whose calls are coroutines."""

    _plain = frozenset({"name", "id"})

    def transaction(self, **options: Any) -> "AsyncTransaction":
        return AsyncTransaction(self._inner.transaction(**options))


class AsyncTransaction:
    """A `Transaction`: mutations are plain calls, `commit` is a coroutine,
    and `async with` commits when the block ends without an exception."""

    def __init__(self, tx: Transaction) -> None:
        self._tx = tx

    def __getattr__(self, name: str) -> Any:
        return getattr(self._tx, name)

    def __len__(self) -> int:
        return len(self._tx)

    async def commit(self) -> Any:
        return await asyncio.to_thread(self._tx.commit)

    async def __aenter__(self) -> "AsyncTransaction":
        self._tx.__enter__()
        return self

    async def __aexit__(self, *exc: Any) -> bool:
        # The sync exit commits or not, exactly as `with` does
        return await asyncio.to_thread(self._tx.__exit__, *exc)
