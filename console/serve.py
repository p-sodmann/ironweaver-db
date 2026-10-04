"""Serve the operator console against a running iwdb-server (step 16a).

Flask serves the console's pages and passes every /v1/... request through to the server, so the pages and the
API share one origin. The server answers no CORS preflight on purpose (documentation/api/rest.md): a page on
another origin can't write to it, and this proxy keeps that, because it forwards the request's Content-Type as
it is and adds no CORS headers of its own.

    uv run --with flask console/serve.py                    # the server on 127.0.0.1:7600 (docker compose up)
    IWDB_URL=https://10.0.0.5:7600 IWDB_CA=ca.pem uv run --with flask console/serve.py --port 8080

Then open http://127.0.0.1:8000/. This is a development tool: it binds to localhost. Authentication is the
server's (step 15a): the proxy passes the `Authorization` header, the session cookie and the console's
`X-Iwdb-Csrf` header through, and the server's `Set-Cookie` back, so the login and the session work as on the
server's own `/console/`. Streamed answers (NDJSON, the change stream's Server-Sent Events) are passed on as they
arrive.

The server speaks TLS (step 15b): the proxy verifies it against `--ca` (default: `$IWDB_CA`, else
docker/tls/ca.pem when docker/dev-cert.sh has made it, else the system's trust store). The pages are served over
plain HTTP on localhost, so the proxy removes `Secure` from the session cookie it passes on: the cookie still
travels to the server over TLS only, from the proxy.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import ssl
import urllib.error
import urllib.request
from pathlib import Path

from flask import Flask, Response, abort, redirect, request, send_from_directory, stream_with_context

ROOT = Path(__file__).resolve().parent
# What the pages load; nothing else under console/ (tools, tests, node_modules) is served
SERVED = {"index.html", "status.html"}
SERVED_DIRS = ("src/", "design-system/", "vendor/")
# Request headers the server reads (credentials included, step 15a); everything else (the browser's Origin,
# Referer, ...) stays here
FORWARD = ("Content-Type", "Accept", "Last-Event-ID", "Authorization", "Cookie", "X-Iwdb-Csrf")
ANSWER = ("Content-Type", "Cache-Control")
METHODS = ["GET", "POST", "PUT", "DELETE"]
# The development CA of docker/dev-cert.sh (compose.yaml's server)
DEV_CA = ROOT.parent / "docker" / "tls" / "ca.pem"


def tls_context(upstream: str, ca: str | None) -> ssl.SSLContext | None:
    """How an https:// upstream is verified: against `ca`, or the system's trust store."""
    if not upstream.startswith("https://"):
        return None
    return ssl.create_default_context(cafile=ca)


def insecure_cookie(cookie: str) -> str:
    """A Set-Cookie for the page on plain HTTP: without `Secure`, which a browser would refuse there."""
    return re.sub(r";\s*Secure(?=;|$)", "", cookie, flags=re.IGNORECASE)


def create_app(upstream: str, timeout: float = 60.0, ca: str | None = None) -> Flask:
    upstream = upstream.rstrip("/")
    context = tls_context(upstream, ca)
    app = Flask(__name__, static_folder=None)

    @app.get("/")
    def home():
        return redirect("/index.html?source=rest")

    @app.get("/console-config.json")
    def config():
        return {"upstream": upstream, "version": server_version(upstream, context)}

    @app.route("/v1/<path:rest>", methods=METHODS)
    def proxy(rest: str):
        url = f"{upstream}/v1/{rest}"
        if request.query_string:
            url += "?" + request.query_string.decode("latin-1")
        headers = {k: request.headers[k] for k in FORWARD if k in request.headers}
        body = request.get_data() or None
        req = urllib.request.Request(url, data=body, headers=headers, method=request.method)
        try:
            answer = urllib.request.urlopen(req, timeout=timeout, context=context)
        except urllib.error.HTTPError as e:  # the server's error: its status and its Error body
            answer = e
            # Its traceback holds this frame, which holds `answer`: a cycle that only the garbage collector would
            # free, so the stream (and its request context) would end at a random later moment
            answer.__traceback__ = None
        except (urllib.error.URLError, OSError) as e:
            # The shape of the server's own errors, so the pages show it like one
            return Response(
                json.dumps({"code": "unavailable", "message": f"no answer from the server at {upstream}: {getattr(e, 'reason', e)}"}),
                status=502,
                content_type="application/json",
            )

        def chunks():
            try:
                while True:
                    data = answer.read1(65536) if hasattr(answer, "read1") else answer.read(65536)
                    if not data:
                        break
                    yield data
            finally:
                answer.close()

        out = Response(stream_with_context(chunks()), status=answer.status)
        for k in ANSWER:
            if answer.headers.get(k):
                out.headers[k] = answer.headers[k]
        # The session cookie of a login, and its removal at logout (a header that may come more than once)
        for cookie in answer.headers.get_all("Set-Cookie") or []:
            out.headers.add("Set-Cookie", insecure_cookie(cookie))
        return out

    @app.get("/<path:name>")
    def page(name: str):
        # Judge the resolved path, so src/../serve.py is not under src/
        path = (ROOT / name).resolve()
        rel = path.relative_to(ROOT).as_posix() if path.is_relative_to(ROOT) else None
        if rel is None or (rel not in SERVED and not rel.startswith(SERVED_DIRS)):
            abort(404)
        return send_from_directory(ROOT, rel)

    return app


def server_version(upstream: str, context: ssl.SSLContext | None = None) -> str | None:
    """The server's version from its OpenAPI document, if it answers."""
    try:
        with urllib.request.urlopen(f"{upstream}/v1/openapi.json", timeout=3, context=context) as r:
            return json.load(r).get("info", {}).get("version")
    except (urllib.error.URLError, OSError, ValueError):
        return None


def main() -> None:
    p = argparse.ArgumentParser(description="Serve the operator console against an iwdb-server.")
    p.add_argument("--upstream", default=os.environ.get("IWDB_URL", "https://127.0.0.1:7600"), help="the server's URL (default: $IWDB_URL or https://127.0.0.1:7600)")
    default_ca = os.environ.get("IWDB_CA") or (str(DEV_CA) if DEV_CA.exists() else None)
    p.add_argument("--ca", default=default_ca, help="the CA (PEM) to verify the server against (default: $IWDB_CA, docker/tls/ca.pem if there, or the system's)")
    p.add_argument("--host", default="127.0.0.1", help="where to listen (default: 127.0.0.1)")
    p.add_argument("--port", type=int, default=8000)
    a = p.parse_args()
    print(f"operator console on http://{a.host}:{a.port}/ -> {a.upstream}" + (f" (CA {a.ca})" if a.ca else ""))
    create_app(a.upstream, ca=a.ca).run(host=a.host, port=a.port, threaded=True)


if __name__ == "__main__":
    main()
