import pytest

import iwdb


@pytest.fixture
def path(tmp_path):
    """A path for a new data directory."""
    return tmp_path / "data"


@pytest.fixture
def store(path):
    """An open store (fsync "off": durability isn't under test here)."""
    with iwdb.Store.open(path, fsync="off") as store:
        yield store


def commit(store, **node):
    """Upsert one node; returns the commit result."""
    with store.transaction() as tx:
        tx.upsert_node(**node)
    return tx.result
