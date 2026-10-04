"""Serve the operator console against a running iwdb-server (step 16a).

Flask serves the console's pages and passes every /v1/... request through to the server, so the pages and the
API share one origin. The server answers no CORS preflight on purpose (documentation/api/rest.md): a page on
another origin can't write to it, and this proxy keeps that, because it forwards the request's Content-Type as
it is and adds no CORS headers of its own.

    uv run --with flask console/serve.py                    # the server on 127.0.0.1:7600 (docker compose up)
    IWDB_URL=http://10.0.0.5:7600 uv run --with flask console/serve.py --port 8080

Then open http://127.0.0.1:8000/. This is a development tool: it binds to localhost and has no authentication,
like the server until step 15. Streamed answers (NDJSON, the change stream's Server-Sent Events) are passed on
as they arrive.
"""

from __future__ import annotations

import argparse
import json
import os
import urllib.error
import urllib.request
from pathlib import Path

from flask import Flask, Response, abort, redirect, request, send_from_directory, stream_with_context

ROOT = Path(__file__).resolve().parent
# What the pages load; nothing else under console/ (tools, tests, node_modules) is served
SERVED = {"index.html", "status.html"}
SERVED_DIRS = ("src/", "design-system/", "vendor/")
# Request headers the server reads; everything else (cookies, the browser's Origin) stays here
FORWARD = ("Content-Type", "Accept", "Last-Event-ID")
ANSWER = ("Content-Type", "Cache-Control")
METHODS = ["GET", "POST", "PUT", "DELETE"]


def create_app(upstream: str, timeout: float = 60.0) -> Flask:
    upstream = upstream.rstrip("/")
    app = Flask(__name__, static_folder=None)

    @app.get("/")
    def home():
        return redirect("/index.html?source=rest")

    @app.get("/console-config.json")
    def config():
        return {"upstream": upstream, "version": server_version(upstream)}

    @app.route("/v1/<path:rest>", methods=METHODS)
    def proxy(rest: str):
        url = f"{upstream}/v1/{rest}"
        if request.query_string:
            url += "?" + request.query_string.decode("latin-1")
        headers = {k: request.headers[k] for k in FORWARD if k in request.headers}
        body = request.get_data() or None
        req = urllib.request.Request(url, data=body, headers=headers, method=request.method)
        try:
            answer = urllib.request.urlopen(req, timeout=timeout)
        except urllib.error.HTTPError as e:  # the server's error: its status and its Error body
            answer = e
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


def server_version(upstream: str) -> str | None:
    """The server's version from its OpenAPI document, if it answers."""
    try:
        with urllib.request.urlopen(f"{upstream}/v1/openapi.json", timeout=3) as r:
            return json.load(r).get("info", {}).get("version")
    except (urllib.error.URLError, OSError, ValueError):
        return None


def main() -> None:
    p = argparse.ArgumentParser(description="Serve the operator console against an iwdb-server.")
    p.add_argument("--upstream", default=os.environ.get("IWDB_URL", "http://127.0.0.1:7600"), help="the server's URL (default: $IWDB_URL or http://127.0.0.1:7600)")
    p.add_argument("--host", default="127.0.0.1", help="where to listen (default: 127.0.0.1)")
    p.add_argument("--port", type=int, default=8000)
    a = p.parse_args()
    print(f"operator console on http://{a.host}:{a.port}/ -> {a.upstream}")
    create_app(a.upstream).run(host=a.host, port=a.port, threaded=True)


if __name__ == "__main__":
    main()
