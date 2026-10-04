"""Step 14: `iwdb.aio`, the asyncio wrappers (ADR 0035), embedded and
remote."""

import asyncio

import pytest

import iwdb
import iwdb.aio
from conftest import ADMIN


def run(coroutine):
    return asyncio.run(coroutine)


async def exercise(store):
    async with store:
        async with store.transaction() as tx:
            tx.upsert_node("a", labels=["P"])
            tx.upsert_node("b", labels=["P"])
            tx.add_edge("a", "b")
            assert len(tx) == 3
        assert tx.result["seq"] == 1
        assert (await store.node("a"))["id"] == "a"
        assert [n["id"] for n in (await store.find(iwdb.label("P")))["nodes"]] == ["a", "b"]
        # Calls run concurrently
        results = await asyncio.gather(*(store.node(n) for n in "ab"))
        assert [r["id"] for r in results] == ["a", "b"]
        await store.create_namespace("other")
        ns = await store.namespace("other")
        assert ns.name == "other" and isinstance(ns.id, int)
        tx = ns.transaction()
        tx.upsert_node("x")
        assert (await tx.commit())["seq"] == 1
        assert (await ns.node("x"))["id"] == "x"
        with pytest.raises(iwdb.NotFoundError):
            await store.namespace("nope")
        # An exception in the block commits nothing
        with pytest.raises(RuntimeError):
            async with store.transaction() as tx:
                tx.upsert_node("never")
                raise RuntimeError
        assert await store.node("never") is None
        assert not store.closed
    assert store.closed


def test_embedded(path):
    async def main():
        await exercise(await iwdb.aio.open(path, fsync="off"))

    run(main())


def test_remote(server):
    async def main():
        await exercise(await iwdb.aio.connect(server.endpoint, user=ADMIN[0], password=ADMIN[1]))

    run(main())
