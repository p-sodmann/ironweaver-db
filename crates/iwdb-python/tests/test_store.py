"""The Store and Transaction API (documentation/python-api.md)."""

import os
import threading

import pytest

import iwdb
from conftest import commit


def test_open_close_and_reopen(path):
    store = iwdb.Store.open(path)
    assert not store.closed
    assert "open" in repr(store)
    commit(store, id="a", attr={"x": 1})
    history = store.history()
    assert len(history) == 32
    store.close()
    assert store.closed
    store.close()  # closing twice does nothing
    with pytest.raises(iwdb.ClosedError):
        store.seq()
    with pytest.raises(iwdb.ClosedError):
        store.transaction().commit()
    # The lock is released: reopen at once, same history, same data
    with iwdb.Store.open(path) as again:
        assert again.seq() == 1
        assert again.history() == history
        assert again.node("a")["attr"] == {"x": 1}
    assert again.closed


def test_a_second_open_is_locked(path):
    with iwdb.Store.open(path):
        with pytest.raises(iwdb.LockedError):
            iwdb.Store.open(path)


def test_os_pathlike_and_options(tmp_path):
    with iwdb.Store.open(
        os.fspath(tmp_path / "a"),
        fsync="group",
        group_max_delay=0.001,
        group_max_batch=4,
        segment_size=1024,
        checkpoint_wal_size=None,
        checkpoint_interval=None,
        checkpoint_on_close=False,
        checkpoint_keep=1,
        checkpoint_background=False,
    ) as store:
        assert store.status()["fsync"] == "group"
    with pytest.raises(ValueError):
        iwdb.Store.open(tmp_path / "b", fsync="sometimes")
    with pytest.raises(iwdb.InvalidError):
        iwdb.Store.open(tmp_path / "c", segment_size=10)
    with pytest.raises(iwdb.InvalidError):
        iwdb.Store.open(tmp_path / "d", create_if_missing=False)


def test_a_transaction_commits_at_the_end_of_its_block(store):
    with store.transaction() as tx:
        tx.upsert_node("alice", labels=["Person", "Admin"], attr={"name": "Alice"}, meta={"source": "test"})
        tx.upsert_node("bob", labels=["Person"])
        first = tx.add_edge("alice", "bob", type="KNOWS", attr={"since": 2020})
        second = tx.add_edge("bob", "alice")
        assert len(tx) == 4
        assert tx.result is None
        assert store.seq() == 0, "nothing is visible before the block ends"
    assert (first, second) == (0, 1)
    result = tx.result
    assert result["seq"] == 1
    assert result["edge_ids"] == [0, 1]
    assert result["versions"] == {"nodes": {"alice": 1, "bob": 1}, "edges": {0: 1, 1: 1}}
    assert store.node("alice") == {
        "id": "alice",
        "labels": ["Admin", "Person"],
        "attr": {"name": "Alice"},
        "meta": {"source": "test"},
        "version": 1,
    }
    assert store.edge(0) == {
        "id": 0,
        "from": "alice",
        "to": "bob",
        "type": "KNOWS",
        "attr": {"since": 2020},
        "meta": {},
        "version": 1,
    }
    assert store.edge(1)["type"] is None
    assert store.node("nobody") is None
    assert store.edge(99) is None


def test_an_exception_in_the_block_commits_nothing(store):
    commit(store, id="a")
    with pytest.raises(RuntimeError, match="boom"):
        with store.transaction() as tx:
            tx.upsert_node("b")
            raise RuntimeError("boom")
    assert tx.result is None
    assert store.seq() == 1
    assert store.node("b") is None
    with pytest.raises(iwdb.InvalidError):
        tx.upsert_node("c")


def test_explicit_commit_and_an_empty_block(store):
    tx = store.transaction()
    tx.upsert_node("a")
    result = tx.commit()
    assert result == tx.result and result["seq"] == 1
    with pytest.raises(iwdb.InvalidError):
        tx.commit()
    with pytest.raises(iwdb.InvalidError):
        tx.upsert_node("b")
    with store.transaction() as empty:
        pass
    assert empty.result is None
    assert store.seq() == 1
    # Committed inside the block: the end of the block doesn't commit again
    with store.transaction() as tx:
        tx.upsert_node("b")
        tx.commit()
    assert store.seq() == 2


def test_every_mutation(store):
    with store.transaction() as tx:
        tx.upsert_node("a", attr={"n": 1})
        tx.upsert_node("b")
        tx.add_edge("a", "b", type="T")
    with store.transaction() as tx:
        tx.set_attr("a", "x", 1)
        tx.set_attr(0, "w", 0.5)
        tx.append_attr("a", "log", "first")
        tx.append_attr("a", "log", "second")
        tx.remove_attr("a", "n")
        tx.add_label("a", "L")
        tx.add_label("b", "L")
        tx.remove_label("b", "L")
        tx.set_edge_type(0, None)
        position = tx.upsert_edge(id=0, attr={"w": 1.0})
        added = tx.upsert_edge(from_="b", to="a", type="BACK", attr={"k": True})
    assert (position, added) == (0, 1)
    assert tx.result["edge_ids"] == [0, 1]
    a = store.node("a")
    assert a["attr"] == {"x": 1, "log": ["first", "second"]}
    assert a["labels"] == ["L"] and store.node("b")["labels"] == []
    assert a["version"] == 2
    assert store.edge(0)["attr"] == {"w": 1.0} and store.edge(0)["type"] is None
    assert store.edge(1)["from"] == "b" and store.edge(1)["type"] == "BACK"
    with store.transaction() as tx:
        tx.delete_edge(1)
        tx.delete_node("b")
    assert store.edge(0) is None, "deleting a node deletes its edges"
    assert store.node("b") is None
    with pytest.raises(ValueError):
        store.transaction().upsert_edge(id=0, from_="a", to="b")
    with pytest.raises(TypeError):
        store.transaction().set_attr(1.5, "k", 1)


def test_expected_versions(store):
    commit(store, id="a", attr={"v": 1})
    with store.transaction() as tx:
        tx.set_attr("a", "v", 2, expected_version=1)
    with pytest.raises(iwdb.ConflictError, match="expected version 1, found 2"):
        with store.transaction() as tx:
            tx.set_attr("a", "v", 3, expected_version=1)
    assert store.node("a")["attr"] == {"v": 2}
    with pytest.raises(iwdb.ConflictError):
        commit(store, id="a", expected_version=0)
    commit(store, id="new", expected_version=0)


def test_catalog(store):
    assert store.catalog() == {"indexes": [], "constraints": []}
    store.create_index("email")
    store.create_index(["address", "city"])
    result = store.add_constraint("unique", "Person", "email")
    assert result["seq"] == 3 and result["edge_ids"] == []
    store.add_constraint("required", "Person", ["name"])
    assert store.catalog() == {
        "indexes": [["address", "city"], ["email"]],
        "constraints": [
            {"kind": "unique", "label": "Person", "path": ["email"]},
            {"kind": "required", "label": "Person", "path": ["name"]},
        ],
    }
    commit(store, id="a", labels=["Person"], attr={"name": "A", "email": "a@x"})
    with pytest.raises(iwdb.ConstraintError):
        commit(store, id="b", labels=["Person"], attr={"name": "B", "email": "a@x"})
    with pytest.raises(iwdb.ConstraintError):
        commit(store, id="c", labels=["Person"], attr={"email": "c@x"})
    with pytest.raises(iwdb.InvalidError):
        store.create_index("email")
    with pytest.raises(ValueError):
        store.add_constraint("sometimes", "Person", "email")
    with pytest.raises(iwdb.InvalidError):
        store.create_index([])
    store.drop_constraint("required", "Person", "name")
    store.drop_constraint("unique", "Person", "email")
    store.drop_index("email")
    store.drop_index(["address", "city"])
    assert store.catalog() == {"indexes": [], "constraints": []}


def test_reads_status_sync_and_checkpoint(path):
    with iwdb.Store.open(path, fsync="off", checkpoint_on_close=False) as store:
        commit(store, id="a")
        assert store.seq() == 1
        assert store.synced_seq() is None, "under off nothing is known to be durable"
        store.sync()
        assert store.synced_seq() == 1
        assert store.read_only() is None
        outcome = store.checkpoint()
        assert outcome["seq"] == 1 and outcome["written"]
        status = store.status()
        assert status["seq"] == 1 and status["checkpoint"] == 1 and status["fsync"] == "off"
        assert status["history"] == store.history()
        assert status["recovery"]["created"] is True
    with iwdb.Store.open(path) as store:
        assert store.synced_seq() == 1
        assert store.status()["recovery"]["checkpoint"] == 1


def test_threads_share_a_store(store):
    def work(n):
        for i in range(20):
            with store.transaction() as tx:
                tx.upsert_node(f"t{n}", attr={"i": i})

    threads = [threading.Thread(target=work, args=(n,)) for n in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert store.seq() == 160
    assert all(store.node(f"t{n}")["attr"] == {"i": 19} for n in range(8))


def test_close_waits_for_calls_in_progress(path):
    store = iwdb.Store.open(path)
    stop = threading.Event()
    errors = []

    def work():
        i = 0
        while not stop.is_set():
            try:
                commit(store, id="x", attr={"i": i})
            except iwdb.ClosedError:
                return
            except Exception as e:  # noqa: BLE001
                errors.append(e)
                return
            i += 1

    thread = threading.Thread(target=work)
    thread.start()
    store.close()
    stop.set()
    thread.join()
    assert errors == []
    with iwdb.Store.open(path) as again:
        assert again.seq() >= 0
