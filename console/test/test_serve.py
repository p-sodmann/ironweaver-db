"""Tests of serve.py (step 16a): the pages are served, /v1 is passed through with its status, body and
content type, nothing else under console/ is served, and a server that doesn't answer is `unavailable`.

    uv run --with flask --with pytest pytest console/test
"""

import json
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
    for path in ["/serve.py", "/package.json", "/tools/seed.mjs", "/test/test_serve.py", "/src/../serve.py", "/node_modules/x"]:
        assert client.get(path).status_code == 404, path


def test_v1_is_passed_through(client):
    r = client.get("/v1/namespaces?max_results=3")
    assert r.status_code == 200 and r.json["query"] == "/v1/namespaces?max_results=3"
    r = client.get("/v1/namespaces/x")
    assert r.status_code == 404 and r.json["code"] == "not_found"
    r = client.post("/v1/namespaces/s/find", data='{"filter":{"Const":true}}', headers={"Content-Type": "application/json", "Accept": "application/x-ndjson", "Cookie": "a=b"})
    assert r.status_code == 200 and r.headers["Content-Type"] == "application/x-ndjson" and r.data.count(b"\n") == 2
    method, path, headers, body = SEEN[-1]
    assert body == b'{"filter":{"Const":true}}' and headers["Accept"] == "application/x-ndjson"
    assert "Cookie" not in headers, "only the headers the server reads are forwarded"


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
