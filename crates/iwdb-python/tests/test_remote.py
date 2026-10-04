"""Step 14: what only the remote client has (ADR 0035): connecting, the
calls that need an embedded store, read-your-writes across clients."""

import socket

import pytest

import iwdb


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def test_no_server_is_unavailable():
    store = iwdb.connect("http://127.0.0.1:{}".format(free_port()))
    with pytest.raises(iwdb.UnavailableError):
        store.node("a", timeout=2)
    assert issubclass(iwdb.UnavailableError, iwdb.Error)
    store.close()


def test_an_invalid_endpoint_is_invalid():
    with pytest.raises(iwdb.InvalidError):
        iwdb.connect("not a url")


def test_repr_and_close(server):
    store = iwdb.connect(server.endpoint)
    assert repr(store) == "<iwdb.Store '{}' (open)>".format(server.endpoint)
    store.close()
    store.close()
    assert store.closed
    with pytest.raises(iwdb.ClosedError):
        store.node("a")


@pytest.mark.parametrize(
    "call",
    [
        lambda s: s.sync(),
        lambda s: s.checkpoint(),
        lambda s: s.checkpoint_all(),
        lambda s: s.history(),
        lambda s: s.status(),
        lambda s: s.backup("/tmp/never"),
        lambda s: s.export("/tmp/never.json"),
        lambda s: s.import_file("/tmp/never.json"),
        lambda s: s.import_namespace("x", "/tmp/never.json"),
        lambda s: s.namespace("default").sync(),
        lambda s: s.namespace("default").checkpoint(),
    ],
)
def test_local_only_calls_are_invalid(server, call):
    with iwdb.connect(server.endpoint) as store:
        with pytest.raises(iwdb.InvalidError, match="needs an embedded store"):
            call(store)


def test_read_your_writes_with_the_last_seq(server):
    with iwdb.connect(server.endpoint) as a, iwdb.connect(server.endpoint) as b:
        with a.transaction() as tx:
            tx.upsert_node("x")
        # `a` sends its last seq; `b` passes `a`'s seq on
        assert a.node("x") is not None
        assert b.node("x", min_seq=tx.result["seq"]) is not None
        # A namespace's seq is its own
        a.create_namespace("other")
        other = a.namespace("other")
        with other.transaction() as tx2:
            tx2.upsert_node("y")
        assert tx2.result["seq"] == 1
        assert a.node("x")["id"] == "x"
        assert other.node("y")["id"] == "y"


def test_a_namespace_created_again_starts_its_seqs_again(server):
    with iwdb.connect(server.endpoint) as store:
        store.create_namespace("n")
        ns = store.namespace("n")
        for i in range(3):
            with ns.transaction() as tx:
                tx.upsert_node(str(i))
        store.drop_namespace("n")
        store.create_namespace("n")
        # Without forgetting seq 3, this would wait for it until the timeout
        assert store.namespace("n").node("0", timeout=2) is None


def test_errors_keep_their_class_over_the_network(server):
    with iwdb.connect(server.endpoint) as store:
        with pytest.raises(iwdb.NotFoundError):
            store.namespace("nope")
        with pytest.raises(iwdb.ConflictError):
            with store.transaction() as tx:
                tx.upsert_node("a", expected_version=3)
        store.add_constraint("unique", "P", "email")
        with store.transaction() as tx:
            tx.upsert_node("a", labels=["P"], attr={"email": "x"})
        with pytest.raises(iwdb.ConstraintError):
            with store.transaction() as tx:
                tx.upsert_node("b", labels=["P"], attr={"email": "x"})


def test_a_server_that_stops_makes_calls_unavailable(server):
    store = iwdb.connect(server.endpoint)
    assert store.seq() == 0
    assert server.stop() == 0
    with pytest.raises(iwdb.UnavailableError):
        store.seq()
    store.close()
