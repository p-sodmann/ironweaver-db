"""The change stream (ADR 0031): commits as logged, in batches that resume
without gaps, long polls, and seqs that are no longer retained."""

import threading
import time

import pytest

import iwdb
from conftest import commit


def all_events(store, from_seq=0, max_results=3):
    events, next_seq = [], from_seq
    while True:
        batch = store.changes(next_seq, max_results=max_results)
        if not batch["events"]:
            return events
        assert len(batch["events"]) <= max_results
        events.extend(batch["events"])
        next_seq = batch["next_seq"]


def test_commits_come_back_as_logged(store):
    commit(store, id="ann", labels=["Person"], attr={"age": 30})
    with store.transaction(idempotency_key="k1") as tx:
        tx.set_attr("ann", "age", 31)
    with store.transaction() as tx:
        tx.remove_attr("ann", "age")
    store.create_index("age")

    events = all_events(store)
    assert [e["seq"] for e in events] == [1, 2, 3, 4]
    assert [e["key"] for e in events] == [None, "k1", None, None]
    assert all(e["time"] is not None for e in events)
    add = events[0]["ops"][0]
    assert add == {"op": "add_node", "id": "ann", "labels": ["Person"], "attr": {"age": 30}, "meta": {}, "version": 1}
    assert events[1]["ops"] == [
        {"op": "set_node_attr", "id": "ann", "key": "age", "value": 31},
        {"op": "set_node_version", "id": "ann", "version": 2},
    ]
    assert events[2]["ops"][0] == {"op": "remove_node_attr", "id": "ann", "key": "age"}
    assert events[3]["catalog"] == {"change": "create_index", "path": ["age"]}


def test_batches_resume_without_gaps(store):
    for i in range(10):
        commit(store, id=f"n{i}")
    whole = all_events(store, max_results=100)
    assert [e["seq"] for e in whole] == list(range(1, 11))
    assert all_events(store, max_results=1) == whole
    batch = store.changes(4, max_results=2)
    assert (batch["next_seq"], batch["first_seq"], batch["seq"]) == (6, 1, 10)
    assert store.namespace("default").changes(9) == store.changes(9)


def test_a_long_poll_waits_for_a_commit(store):
    commit(store, id="a")
    start = time.monotonic()
    batch = store.changes(2, wait=True, timeout=0.2)
    assert batch["events"] == [] and batch["next_seq"] == 2
    assert time.monotonic() - start >= 0.1

    threading.Timer(0.1, lambda: commit(store, id="b")).start()
    batch = store.changes(2, wait=True, timeout=30)
    assert [e["seq"] for e in batch["events"]] == [2]


def test_errors(store):
    commit(store, id="a")
    with pytest.raises(iwdb.InvalidError):
        store.changes(1, history="0" * 32)
    gone = store.namespace(store.create_namespace("x")["name"])
    store.drop_namespace("x")
    with pytest.raises(iwdb.NotFoundError):
        gone.changes(1)


def test_seqs_older_than_the_wal_are_not_retained(path):
    options = dict(fsync="off", segment_size=1024, checkpoint_keep=1, checkpoint_background=False)
    with iwdb.Store.open(path, **options) as store:
        for i in range(60):
            commit(store, id=f"n{i}", attr={"pad": "x" * 100})
        store.checkpoint()
        with pytest.raises(iwdb.NotRetainedError, match="no longer retained"):
            store.changes(1)
    with iwdb.Store.open(path, retain_records=1000, **options) as store:
        for i in range(60):
            commit(store, id=f"m{i}", attr={"pad": "x" * 100})
        first = store.changes(61)["first_seq"]
        store.checkpoint()
        assert store.changes(61)["first_seq"] == first
