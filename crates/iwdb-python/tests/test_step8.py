"""Step 8: idempotency keys, commit times, read-your-writes and concurrent
readers (documentation/python-api.md)."""

import datetime
import threading
import time

import pytest

import iwdb
from conftest import commit


def test_results_have_a_commit_time_and_are_not_deduplicated(store):
    before = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(seconds=5)
    result = commit(store, id="a")
    assert result["deduplicated"] is False
    assert result["time"].tzinfo is not None
    assert before <= result["time"] <= datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(seconds=5)
    assert store.create_index("x")["time"] >= result["time"]


def test_a_retry_with_the_same_key_returns_the_original_result(store):
    commit(store, id="a")
    commit(store, id="b")
    with store.transaction(idempotency_key="req-1") as tx:
        tx.add_edge("a", "b", type="KNOWS")
        tx.set_attr("a", "n", 1)
    first = tx.result
    with store.transaction(idempotency_key="req-1") as again:
        again.add_edge("a", "b", type="KNOWS")
        again.set_attr("a", "n", 1)
    assert again.result["deduplicated"] is True
    assert {**again.result, "deduplicated": False} == first
    assert store.seq() == first["seq"]
    assert store.node("a")["version"] == 2


def test_the_same_key_for_another_request_is_refused(store):
    with store.transaction(idempotency_key="k") as tx:
        tx.upsert_node("a", attr={"n": 1})
    tx = store.transaction(idempotency_key="k")
    tx.upsert_node("a", attr={"n": 2})
    with pytest.raises(iwdb.ConflictError, match="different request"):
        tx.commit()
    assert store.node("a")["attr"] == {"n": 1}


def test_invalid_keys_are_refused(store):
    for key in ["", "x" * 256]:
        with pytest.raises(iwdb.InvalidError):
            store.transaction(idempotency_key=key)
    store.transaction(idempotency_key="x" * 255)


def test_catalog_changes_take_keys(store):
    first = store.create_index("email", idempotency_key="ix")
    again = store.create_index("email", idempotency_key="ix")
    assert again["deduplicated"] and again["seq"] == first["seq"]
    with pytest.raises(iwdb.ConflictError):
        store.create_index("email")


def test_keys_survive_reopening_and_checkpoints(path):
    with iwdb.Store.open(path) as store:
        with store.transaction(idempotency_key="once") as tx:
            tx.upsert_node("a")
        store.checkpoint()
    with iwdb.Store.open(path) as store:
        with store.transaction(idempotency_key="once") as tx:
            tx.upsert_node("a")
        assert tx.result["deduplicated"] and store.seq() == 1
        assert store.node("a")["version"] == 1


def test_keys_survive_a_backup_and_restore(tmp_path):
    with iwdb.Store.open(tmp_path / "data") as store:
        with store.transaction(idempotency_key="before") as tx:
            tx.upsert_node("a")
        store.backup(tmp_path / "backup")
        with store.transaction(idempotency_key="after") as tx:
            tx.upsert_node("b")
    iwdb.restore(tmp_path / "restored", backup=tmp_path / "backup")
    with iwdb.Store.open(tmp_path / "restored") as restored:
        with restored.transaction(idempotency_key="before") as tx:
            tx.upsert_node("a")
        assert tx.result["deduplicated"]
        # The commit after the backup isn't in the restored history: it applies
        with restored.transaction(idempotency_key="after") as tx:
            tx.upsert_node("b")
        assert not tx.result["deduplicated"] and restored.seq() == 2


def test_min_seq_reads_wait_for_the_commit(store):
    result = commit(store, id="a", attr={"n": 1})
    # Applied already: no wait
    assert store.node("a", min_seq=result["seq"])["attr"] == {"n": 1}
    assert store.wait_for_seq(result["seq"]) == result["seq"]
    seen = {}

    def reader():
        seen["node"] = store.node("b", min_seq=result["seq"] + 1, timeout=10)

    thread = threading.Thread(target=reader)
    thread.start()
    time.sleep(0.05)
    assert "node" not in seen  # waiting, with the GIL released
    commit(store, id="b")
    thread.join(10)
    assert seen["node"]["id"] == "b"


def test_min_seq_times_out(store):
    start = time.monotonic()
    with pytest.raises(iwdb.TimeoutError):
        store.edge(0, min_seq=5, timeout=0.05)
    with pytest.raises(iwdb.TimeoutError):
        store.catalog(min_seq=5, timeout=0)
    with pytest.raises(iwdb.TimeoutError):
        store.wait_for_seq(1, timeout=0.01)
    assert time.monotonic() - start < 5
    with pytest.raises(ValueError):
        store.node("a", min_seq=1, timeout=-1)


def test_readers_run_while_a_writer_commits(store):
    commit(store, id="a", attr={"n": 0})
    errors = []
    stop = threading.Event()

    def reader():
        while not stop.is_set():
            node = store.node("a")
            # Both attributes are written by one transaction: never half of it
            if node["attr"].get("n") != node["attr"].get("m", node["attr"].get("n")):
                errors.append(node)

    threads = [threading.Thread(target=reader) for _ in range(4)]
    for t in threads:
        t.start()
    for i in range(1, 100):
        with store.transaction() as tx:
            tx.set_attr("a", "n", i)
            tx.set_attr("a", "m", i)
    stop.set()
    for t in threads:
        t.join()
    assert errors == []
