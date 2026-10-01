"""One exception per kind of error, each an iwdb.Error, with the Rust
message (documentation/python-api.md, "Exceptions")."""

import shutil

import pytest

import iwdb
from conftest import commit

ALL = [
    iwdb.ConflictError,
    iwdb.ConstraintError,
    iwdb.NotFoundError,
    iwdb.InvalidError,
    iwdb.ReadOnlyError,
    iwdb.LockedError,
    iwdb.IoError,
    iwdb.CorruptError,
    iwdb.ClosedError,
    iwdb.InternalError,
]


def test_every_exception_is_an_iwdb_error():
    for cls in ALL:
        assert issubclass(cls, iwdb.Error) and issubclass(cls, Exception)
        assert cls.__module__ == "iwdb"
        assert cls.__doc__


def test_commit_errors(store):
    commit(store, id="a")
    with pytest.raises(iwdb.NotFoundError, match="node 'nobody' not found"):
        with store.transaction() as tx:
            tx.delete_node("nobody")
    with pytest.raises(iwdb.ConflictError):
        commit(store, id="a", expected_version=7)
    with pytest.raises(iwdb.InvalidError, match="not a list"):
        with store.transaction() as tx:
            tx.set_attr("a", "x", 1)
            tx.append_attr("a", "x", 2)
    assert store.seq() == 1, "a failed commit changes nothing"
    assert store.read_only() is None


def test_a_failed_wal_write_makes_the_store_read_only(path):
    # Small segments: the next rotation needs to create a file in wal/
    store = iwdb.Store.open(path, segment_size=1024, checkpoint_background=False)
    commit(store, id="a", attr={"pad": "x" * 400})
    shutil.rmtree(path / "ns" / "00000000000000000001" / "wal")
    with pytest.raises(iwdb.IoError):
        for i in range(10):
            commit(store, id="a", attr={"pad": "x" * 400, "i": i})
    assert store.read_only() is not None
    with pytest.raises(iwdb.ReadOnlyError):
        commit(store, id="b")
    assert store.node("a") is not None, "reads still work"
    store_closed = False
    try:
        store.close()
    except iwdb.ReadOnlyError:
        store_closed = True
    assert store_closed and store.closed


def test_damage_is_corrupt(path):
    with iwdb.Store.open(path, segment_size=1024, checkpoint_background=False, checkpoint_on_close=False) as store:
        for i in range(20):
            commit(store, id="a", attr={"pad": "x" * 200, "i": i})
    first = sorted((path / "ns" / "00000000000000000001" / "wal").iterdir())[0]
    data = bytearray(first.read_bytes())
    data[40] ^= 0xFF
    first.write_bytes(bytes(data))
    with pytest.raises(iwdb.CorruptError, match="corrupt"):
        iwdb.Store.open(path)
    report = iwdb.verify(path)
    assert report["ok"] is False and report["problems"]


def test_a_panic_in_the_bindings_is_an_exception():
    with pytest.raises(iwdb.InternalError, match="a test panic"):
        iwdb._iwdb._panic_for_tests()
    # The interpreter goes on
    assert iwdb.__version__


def test_not_a_store(tmp_path):
    (tmp_path / "notes.txt").write_text("x")
    with pytest.raises(iwdb.InvalidError, match="not an Ironweaver DB data directory"):
        iwdb.Store.open(tmp_path)
