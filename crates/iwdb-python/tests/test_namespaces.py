"""Step 9: namespaces, per-namespace catalog and status, backup and
restore with several namespaces (documentation/python-api.md)."""

import threading

import pytest

import iwdb


def put(ns, id, **attr):
    with ns.transaction() as tx:
        tx.upsert_node(id, labels=["Person"], attr=attr)
    return tx.result


def test_namespaces_are_independent_graphs(store):
    assert [n["name"] for n in store.namespaces()] == ["default"]
    created = store.create_namespace("social")
    assert created["name"] == "social" and created["id"] == 2 and created["deduplicated"] is False
    social = store.namespace("social")
    assert (social.name, social.id) == ("social", 2)
    put(store, "a")
    put(social, "x")
    put(social, "y")
    assert store.node("x") is None and social.node("a") is None
    assert (store.seq(), social.seq()) == (1, 2)
    # min_seq is per namespace
    assert social.node("y", min_seq=2)["id"] == "y"
    assert [n["name"] for n in store.namespaces()] == ["default", "social"]


def test_names_and_errors(store):
    for bad in ["", "-x", "a b", "x" * 65, "é"]:
        with pytest.raises(iwdb.InvalidError):
            store.create_namespace(bad)
    store.create_namespace("ok_1-x")
    with pytest.raises(iwdb.InvalidError):
        store.create_namespace("ok_1-x")
    with pytest.raises(iwdb.InvalidError):
        store.drop_namespace("default")
    with pytest.raises(iwdb.NotFoundError):
        store.drop_namespace("nope")
    with pytest.raises(iwdb.NotFoundError):
        store.namespace("nope")


def test_keys_for_create_and_drop(store):
    first = store.create_namespace("a", idempotency_key="mk-a")
    again = store.create_namespace("a", idempotency_key="mk-a")
    assert again["deduplicated"] is True and again["id"] == first["id"]
    with pytest.raises(iwdb.InvalidError):
        store.create_namespace("b", idempotency_key="mk-a")
    dropped = store.drop_namespace("a", idempotency_key="rm-a")
    assert store.drop_namespace("a", idempotency_key="rm-a")["deduplicated"] is True
    assert dropped["id"] == first["id"]
    # Ids aren't reused
    assert store.create_namespace("a")["id"] > first["id"]


def test_a_dropped_namespace_fails_its_handle_and_its_waiters(store):
    store.create_namespace("a")
    handle = store.namespace("a")
    errors = []

    def wait():
        try:
            handle.wait_for_seq(5, timeout=10)
        except iwdb.NotFoundError as e:
            errors.append(e)

    thread = threading.Thread(target=wait)
    thread.start()
    store.drop_namespace("a")
    thread.join(timeout=10)
    assert len(errors) == 1 and isinstance(errors[0], iwdb.NotFoundError)
    with pytest.raises(iwdb.NotFoundError):
        put(handle, "x")


def test_unique_constraints_and_indexes_are_per_namespace(store):
    store.create_namespace("a")
    store.create_namespace("b")
    a, b = store.namespace("a"), store.namespace("b")
    a.create_index("email")
    a.add_constraint("unique", "Person", "email")
    put(a, "p1", email="x@example.org")
    with pytest.raises(iwdb.ConstraintError):
        put(a, "p2", email="x@example.org")
    # The same value in another namespace is fine
    put(b, "p2", email="x@example.org")
    assert [i["path"] for i in a.indexes()] == [["email"]]
    assert a.indexes()[0]["state"] == "ready"
    assert a.indexes()[0]["entries"] == a.indexes()[0]["distinct_keys"] > 0
    assert a.indexes()[0]["memory_bytes"] > 0
    assert b.indexes() == [] and b.catalog()["constraints"] == []
    assert a.catalog()["constraints"][0]["kind"] == "unique"


def test_status_per_namespace(store):
    store.create_namespace("a")
    a = store.namespace("a")
    a.create_index("n")
    for i in range(5):
        put(a, f"n{i}", n=i)
    status = store.status()
    by_name = {n["name"]: n for n in status["namespaces"]}
    assert set(by_name) == {"default", "a"}
    assert by_name["a"]["nodes"] == 5 and by_name["default"]["nodes"] == 0
    assert by_name["a"]["indexes"][0]["state"] == "ready"
    assert by_name["a"]["memory_bytes"] > by_name["default"]["memory_bytes"]
    assert a.status()["seq"] == 6


def test_namespaces_survive_a_reopen(path):
    with iwdb.Store.open(path, fsync="off") as store:
        store.create_namespace("a")
        put(store.namespace("a"), "x")
        store.create_namespace("gone")
        store.drop_namespace("gone")
    with iwdb.Store.open(path, fsync="off") as store:
        assert [n["name"] for n in store.namespaces()] == ["a", "default"]
        assert store.namespace("a").node("x")["id"] == "x"
    assert iwdb.verify(path)["ok"] is True


def test_backup_and_restore_with_several_namespaces(path, tmp_path):
    with iwdb.Store.open(path, fsync="off") as store:
        store.create_namespace("a")
        store.create_namespace("b")
        put(store, "d1")
        put(store.namespace("a"), "x")
        put(store.namespace("b"), "y")
        report = store.backup(tmp_path / "backup")
        assert sorted(n["name"] for n in report["namespaces"]) == ["a", "b", "default"]
        store.create_namespace("late")
        store.drop_namespace("b")
    result = iwdb.verify(tmp_path / "backup")
    assert result["ok"] is True and len(result["namespaces"]) == 3
    restored = iwdb.restore(tmp_path / "restored", backup=tmp_path / "backup")
    assert sorted(n["name"] for n in restored["namespaces"]) == ["a", "b", "default"]
    with iwdb.Store.open(tmp_path / "restored", fsync="off") as store:
        assert store.namespace("b").node("y")["id"] == "y"
        assert store.namespace("a").node("x")["id"] == "x"
        assert store.node("d1")["id"] == "d1"
    # A seq needs one namespace
    with pytest.raises(iwdb.InvalidError):
        iwdb.restore(tmp_path / "r2", backup=tmp_path / "backup", seq=1)
    only = iwdb.restore(tmp_path / "r3", backup=tmp_path / "backup", seq=1, namespaces=["a"])
    assert [n["name"] for n in only["namespaces"]] == ["a"]


def test_checkpoint_all(store):
    store.create_namespace("a")
    put(store, "d")
    put(store.namespace("a"), "x")
    done = store.checkpoint_all()
    assert set(done) == {"default", "a"}
    assert done["a"]["seq"] == 1 and done["a"]["written"] is True
