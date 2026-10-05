"""Tests of serve.py (step 16a): the pages are served, /v1 is passed through with its status, body and
content type (and, step 15a, the credentials and the session cookie), nothing else under console/ is served,
and a server that doesn't answer is `unavailable`; a server over TLS (step 15b) is verified against the CA.

    uv run --with flask --with pytest pytest console/test
"""

import json
import ssl
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import serve  # noqa: E402

SEEN = []


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def answer(self, status, body, ctype="application/json"):
        data = body.encode()
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        SEEN.append(("GET", self.path, dict(self.headers)))
        if self.path.startswith("/v1/namespaces/x"):
            self.answer(404, json.dumps({"code": "not_found", "message": "no namespace 'x'"}))
        elif self.path == "/v1/openapi.json":
            self.answer(200, json.dumps({"info": {"version": "v1"}}))
        else:
            self.answer(200, json.dumps({"namespaces": [], "query": self.path}))

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        SEEN.append(("POST", self.path, dict(self.headers), body))
        if self.path == "/v1/auth/login":
            data = b'{"user":{"name":"ann"}}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Set-Cookie", "iwdb_session=t0k; Path=/; HttpOnly; SameSite=Strict; Max-Age=60; Secure")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        if self.headers.get("Content-Type") != "application/json":
            return self.answer(415, json.dumps({"code": "invalid_argument", "message": "a request body must be application/json"}))
        self.answer(200, '{"n":1}\n{"meta":{}}\n', "application/x-ndjson")


@pytest.fixture(scope="module")
def client():
    httpd = HTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    app = serve.create_app(f"http://127.0.0.1:{httpd.server_port}")
    yield app.test_client()
    httpd.shutdown()


def test_the_pages_and_their_files_are_served(client):
    assert client.get("/").headers["Location"].endswith("/index.html?source=rest")
    for path in ["/index.html", "/status.html", "/src/rest.js", "/design-system/bundle.js", "/vendor/react.production.min.js"]:
        assert client.get(path).status_code == 200, path
    # The fonts come with the pages (step 15c), not from Google Fonts
    font = client.get("/design-system/fonts/archivo-latin-wght-normal.woff2")
    assert font.status_code == 200 and font.headers["Content-Type"] == "font/woff2"
    for path in ["/serve.py", "/package.json", "/tools/seed.mjs", "/test/test_serve.py", "/src/../serve.py", "/node_modules/x"]:
        assert client.get(path).status_code == 404, path


def test_v1_is_passed_through(client):
    r = client.get("/v1/namespaces?max_results=3")
    assert r.status_code == 200 and r.json["query"] == "/v1/namespaces?max_results=3"
    r = client.get("/v1/namespaces/x")
    assert r.status_code == 404 and r.json["code"] == "not_found"
    r = client.post("/v1/namespaces/s/find", data='{"filter":{"Const":true}}', headers={"Content-Type": "application/json", "Accept": "application/x-ndjson", "Origin": "http://elsewhere"})
    assert r.status_code == 200 and r.headers["Content-Type"] == "application/x-ndjson" and r.data.count(b"\n") == 2
    method, path, headers, body = SEEN[-1]
    assert body == b'{"filter":{"Const":true}}' and headers["Accept"] == "application/x-ndjson"
    assert "Origin" not in headers, "only the headers the server reads are forwarded"


def test_credentials_and_the_session_cookie_pass_through(client):
    r = client.post("/v1/auth/login", data='{"user":"ann","password":"pw","cookie":true}', headers={"Content-Type": "application/json"})
    assert r.status_code == 200
    assert r.headers["Set-Cookie"].startswith("iwdb_session=t0k;") and "HttpOnly" in r.headers["Set-Cookie"]
    # The page is plain HTTP on localhost: no Secure, or the browser would drop the cookie
    assert "Secure" not in r.headers["Set-Cookie"] and r.headers["Set-Cookie"].endswith("Max-Age=60")
    client.get("/v1/namespaces", headers={"Authorization": "Bearer abc", "Cookie": "iwdb_session=t0k", "X-Iwdb-Csrf": "1"})
    method, path, headers = SEEN[-1]
    assert headers["Authorization"] == "Bearer abc" and headers["Cookie"] == "iwdb_session=t0k" and headers["X-Iwdb-Csrf"] == "1"


def test_the_servers_guard_still_holds(client):
    # A cross-site form can't send application/json; the proxy forwards what it got, and the server refuses it
    r = client.post("/v1/namespaces/s/commit", data="{}", headers={"Content-Type": "text/plain"})
    assert r.status_code == 415
    assert "Access-Control-Allow-Origin" not in r.headers


def test_config_and_a_missing_server():
    app = serve.create_app("http://127.0.0.1:9")  # nothing listens on the discard port
    c = app.test_client()
    r = c.get("/v1/namespaces")
    assert r.status_code == 502 and r.json["code"] == "unavailable"
    assert c.get("/console-config.json").json == {"upstream": "http://127.0.0.1:9", "version": None}


TLS = Path(__file__).resolve().parents[2] / "tests" / "fixtures" / "tls"


def test_a_server_over_tls_is_verified_against_the_ca():
    """The upstream speaks TLS with the test certificate (test-only): the proxy trusts the given CA only."""
    httpd = HTTPServer(("127.0.0.1", 0), Upstream)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(TLS / "server.pem", TLS / "server.key")
    httpd.socket = context.wrap_socket(httpd.socket, server_side=True)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    try:
        upstream = f"https://127.0.0.1:{httpd.server_port}"
        c = serve.create_app(upstream, ca=str(TLS / "ca.pem")).test_client()
        assert c.get("/v1/namespaces").status_code == 200
        assert c.get("/console-config.json").json == {"upstream": upstream, "version": "v1"}
        r = serve.create_app(upstream, ca=str(TLS / "other-ca.pem")).test_client().get("/v1/namespaces")
        assert r.status_code == 502 and r.json["code"] == "unavailable" and "CERTIFICATE_VERIFY_FAILED" in r.json["message"]
    finally:
        httpd.shutdown()
