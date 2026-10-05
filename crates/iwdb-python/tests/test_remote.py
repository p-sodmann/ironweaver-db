"""Step 14: what only the remote client has (ADR 0035): connecting, the
calls that need an embedded store, read-your-writes across clients."""

import json
import socket
import ssl
import time
import urllib.error
import urllib.request

import pytest

import iwdb
from conftest import ADMIN, CA, TLS, Server


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
    store = server.connect()
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
    with server.connect() as store:
        with pytest.raises(iwdb.InvalidError, match="needs an embedded store"):
            call(store)


def test_read_your_writes_with_the_last_seq(server):
    with server.connect() as a, server.connect() as b:
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
    with server.connect() as store:
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
    with server.connect() as store:
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
    store = server.connect()
    assert store.seq() == 0
    assert server.stop() == 0
    with pytest.raises(iwdb.UnavailableError):
        store.seq()
    store.close()


# Authentication (step 15a)


def rest(server, method, path, body=None, token=None):
    """A REST call; the answer's JSON. The tests set users up over REST
    (the Python client has no account API): against a server built
    without REST they are skipped."""
    request = urllib.request.Request(server.endpoint + path, method=method)
    if body is not None:
        request.add_header("content-type", "application/json")
        request.data = json.dumps(body).encode()
    if token:
        request.add_header("authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(request, context=ssl.create_default_context(cafile=str(CA))) as answer:
            return json.loads(answer.read() or b"{}")
    except urllib.error.HTTPError as e:
        if e.code == 404 and not e.read():
            pytest.skip("this iwdb-server has no REST API (build it with --features rest)")
        raise


def admin_token(server):
    return rest(server, "POST", "/v1/auth/login", {"user": ADMIN[0], "password": ADMIN[1]})["token"]


def test_without_credentials_calls_are_unauthenticated(server):
    with server.client() as store:
        with pytest.raises(iwdb.UnauthenticatedError, match="needs credentials"):
            store.node("a")
    assert issubclass(iwdb.UnauthenticatedError, iwdb.Error)


def test_a_wrong_password_is_unauthenticated(server):
    with pytest.raises(iwdb.UnauthenticatedError, match="wrong user or password") as caught:
        server.client(user=ADMIN[0], password="not the password")
    assert "not the password" not in str(caught.value)
    with pytest.raises(iwdb.UnauthenticatedError):
        server.client(user="nobody", password="whatever-password")


def test_credentials_are_a_token_or_a_user_and_password(server):
    with pytest.raises(iwdb.InvalidError):
        server.client(user=ADMIN[0])
    with pytest.raises(iwdb.InvalidError):
        server.client(token="t", user=ADMIN[0], password=ADMIN[1])


def test_an_expired_session_is_unauthenticated(tmp_path):
    server = Server(tmp_path / "data", tmp_path, auth="session_lifetime_secs = 1")
    try:
        with server.connect() as store:
            assert store.node("a") is None
            time.sleep(1.5)
            with pytest.raises(iwdb.UnauthenticatedError, match="expired"):
                store.node("a")
        # Logging in again works
        with server.connect() as store:
            assert store.node("a") is None
    finally:
        assert server.stop() == 0, "".join(server.stderr)


def test_api_tokens_and_roles(server):
    token = admin_token(server)
    rest(server, "POST", "/v1/users", {"name": "ann", "password": "ann-password"}, token)
    rest(server, "PUT", "/v1/users/ann/grants/default", {"role": "ROLE_READ"}, token)
    ann = rest(server, "POST", "/v1/users/ann/tokens", {"name": "py"}, token)["token"]
    with server.connect() as admin:
        with admin.transaction() as tx:
            tx.upsert_node("a", labels=["P"])
    with server.client(token=ann) as store:
        assert store.node("a")["labels"] == ["P"]
        with pytest.raises(iwdb.PermissionDeniedError, match="'write' role"):
            with store.transaction() as tx:
                tx.upsert_node("b")
        with pytest.raises(iwdb.PermissionDeniedError):
            store.create_namespace("other")
        assert [n["name"] for n in store.namespaces()] == ["default"]
    # A revoked token is unauthenticated
    rest(server, "DELETE", "/v1/users/ann/tokens/py", None, token)
    with server.client(token=ann) as store:
        with pytest.raises(iwdb.UnauthenticatedError):
            store.node("a")


# TLS and mTLS (step 15b)


def test_the_server_speaks_tls_only(server):
    plain = server.endpoint.replace("https://", "http://")
    with iwdb.connect(plain) as store:
        with pytest.raises(iwdb.UnavailableError):
            store.node("a", timeout=2)
    with pytest.raises(iwdb.InvalidError, match="https://"):
        iwdb.connect(plain, ca=CA)


def test_a_server_of_another_ca_is_unavailable(server):
    with iwdb.connect(server.endpoint, ca=TLS / "other-ca.pem") as store:
        with pytest.raises(iwdb.UnavailableError, match="UnknownIssuer"):
            store.node("a", timeout=2)
    with pytest.raises(iwdb.InvalidError, match="missing.pem"):
        iwdb.connect(server.endpoint, ca=TLS / "missing.pem")


def test_a_client_certificate_logs_in_as_its_user(tmp_path):
    server = Server(tmp_path / "data", tmp_path, tls='client_ca = "{}"'.format(str(CA).replace("\\", "\\\\")))
    try:
        token = admin_token(server)
        rest(server, "POST", "/v1/users", {"name": "ann", "password": "ann-password"}, token)
        rest(server, "PUT", "/v1/users/ann/grants/default", {"role": "ROLE_READ"}, token)
        with server.connect() as admin:
            with admin.transaction() as tx:
                tx.upsert_node("a", labels=["P"])
        ann = {"cert": TLS / "client-ann.pem", "key": TLS / "client-ann.key"}
        with server.client(**ann) as store:
            assert store.node("a")["labels"] == ["P"]
            with pytest.raises(iwdb.PermissionDeniedError, match="user 'ann'"):
                with store.transaction() as tx:
                    tx.upsert_node("b")
        # An expired certificate: no handshake
        expired = {"cert": TLS / "client-expired.pem", "key": TLS / "client-expired.key"}
        with server.client(**expired) as store:
            with pytest.raises(iwdb.UnavailableError):
                store.node("a", timeout=2)
        with pytest.raises(iwdb.InvalidError, match="needs its key"):
            server.client(cert=TLS / "client-ann.pem")
    finally:
        assert server.stop() == 0, "".join(server.stderr)
