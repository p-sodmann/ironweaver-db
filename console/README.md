# Operator console

A web interface for operators of an Ironweaver DB server (step 16a, [ADR 0037](../documentation/adr/0037-operator-console.md)). Two pages:

- **Explore** (`index.html`): walk through a graph like Neo4j's browser, and browse and edit a namespace like phpMyAdmin. The schema (namespaces, labels, edge types, indexes, constraints) is on the left; the result of the query line is in the middle as a graph, a table, a plan or the namespace's structure; the inspector opens with a selection. Hover a node to peek at its neighbours, double-click to bring them in. Edits (a property in the inspector, connecting or detaching on the canvas) are staged and committed together (`⌘S`).
- **Status** (`status.html`): the server at one glance. Health and problems, commits, latencies, memory and active requests over the last minute and a half, every namespace's state, latency by operation, active requests (with cancel), change-stream readers, managed analytics jobs (with cancel), index builds, and the log.

It runs on a real server, served by `iwdb-server` itself or through `serve.py`, or on mock data on its own.

## Served by the server

A server built with the `console` feature serves the pages at `/console/` when `[console] enabled = true` (`IWDB_CONSOLE_ENABLED=true`), on its own port, with the REST API on the same origin ([ADR 0041](../documentation/adr/0041-console-served-by-the-server.md)). The pages are compiled into the binary.

**Logging in** (step 15a, [ADR 0046](../documentation/adr/0046-the-console-session.md)). With the server's authentication on (the default), the pages open with a login: a server account (`iwctl user create`, or the first admin from `IWDB_AUTH_BOOTSTRAP_PASSWORD`). The session is an HttpOnly, SameSite=Strict cookie the server sets: no script can read it, and the pages send `X-Iwdb-Csrf` with every request. When it ends (expiry, logout, a password change, a restart), the next call shows the login over the page, and nothing staged is lost. The rail shows the user and LOG OUT. The roles are the user's: a `read` user can browse but not commit.

**HTTPS** (step 15b, [ADR 0048](../documentation/adr/0048-tls-and-mtls.md)). The server speaks TLS, so the console is at `https://`, and the session cookie is `Secure` there. `compose.yaml` uses the development certificate of `docker/dev-cert.sh`: trust `docker/tls/ca.pem` in the browser (or accept its warning once). A server whose TLS was turned off serves the console over plain HTTP; off localhost the login then says that the password crosses the network in clear.

```sh
sh docker/dev-cert.sh                                 # once: docker/tls/ca.pem and the server's certificate
IWDB_ADMIN_PASSWORD=... docker compose up --build    # iwdb-server on 127.0.0.1:7600, console on, user admin
IWDB_USER=admin IWDB_PASSWORD=... NODE_EXTRA_CA_CERTS=docker/tls/ca.pem node console/tools/seed.mjs   # optional: the sample namespaces
open https://127.0.0.1:7600/console/
```

Or without Docker: `cargo run -p iwdb-server --features console` with `IWDB_DATA_DIR=data IWDB_CONSOLE_ENABLED=true IWDB_TLS_CERT=docker/tls/server.pem IWDB_TLS_KEY=docker/tls/server.key IWDB_AUTH_BOOTSTRAP_PASSWORD=...`.

## Through the Flask proxy (development)

`serve.py` serves the pages from disk, so editing them needs no rebuild. It is a small Flask app: it serves the pages and passes `/v1/...` through to the server, so the pages and the API share one origin. (The server answers no CORS preflight on purpose, so a page on another origin can't call it; the proxy keeps that guard.)

```sh
IWDB_ADMIN_PASSWORD=... docker compose up --build   # iwdb-server on https://127.0.0.1:7600
IWDB_USER=admin IWDB_PASSWORD=... NODE_EXTRA_CA_CERTS=docker/tls/ca.pem node console/tools/seed.mjs   # optional: the sample namespaces (social, inventory, orders, archive_2025)
uv run --with flask console/serve.py           # or: pip install flask && python console/serve.py
open http://127.0.0.1:8000/
```

`IWDB_URL` (or `--upstream`) points it at another server (default `https://127.0.0.1:7600`), `--port` changes its port. It verifies the server against `--ca` (`IWDB_CA`; by default `docker/tls/ca.pem` when it exists, else the system's trust store). It passes the `Authorization` and `Cookie` headers and the console's `X-Iwdb-Csrf` through, and the server's `Set-Cookie` back (without `Secure`: the proxy's page is plain HTTP on localhost, and the cookie travels to the server over TLS from the proxy), so the login works as on `/console/`. `seed.mjs` skips namespaces that exist; `--replace` drops them first.

On a server, the explorer works fully: reads, edits and commits, index creation. The status page shows the server's status views (step 16c, [ADR 0053](../documentation/adr/0053-the-schema-read.md)): its state, every namespace's, the running requests (cancel one, and its caller gets `cancelled`; commits can't be cancelled), the change-stream readers, the managed analytics jobs (step 16f: their state, how long they ran, the projection's size and rows; cancel a queued or running one), latency per operation, memory and disk, and the server's log for a server-wide admin (anyone else sees the page's own requests). The series of the last minute and a half are the differences between the page's polls of `/v1/metrics`, so they start empty and fill every 3 s. The schema navigator's label counts are exact; keys and edge types come from the first 10 000 nodes and 100 000 edges, and the structure view says when a namespace is bigger. A `find` doesn't count its matches (an unbounded count), so the pager counts pages as it goes.

## On mock data

Open `index.html` or `status.html` from the file system (or `?source=mock` on `serve.py`): `src/mock.js` generates the four namespaces and simulates a running server, and the rail says `MOCK DATA`. `?scenario=degraded` shows how problems look (a read-only namespace, a failed checkpoint, memory above the server's warning line). The mock has a login too: `admin` / `admin` (a server-wide admin) or `reader` / `reader` (read on `social`), remembered for the tab (`sessionStorage`; a mock flag, never a credential).

No build step is needed either way: React, the design system's bundle and the console's scripts are committed. The fonts (Archivo, IBM Plex Mono; SIL OFL 1.1) are in `design-system/fonts/` and served with the pages: the console loads nothing from another site.

## The query line

The console runs a subset of [`iwctl shell`](../documentation/iwctl.md#query-shell)'s commands, so what works here works there:

| Command | What it does |
|---|---|
| `match <pattern>` | the core's pattern text, `(a:Person {age: 30})-[k:KNOWS*1..2]->(b)` (the mock matches single chains) |
| `find <filter>` | the core's filter JSON, `{"Label": "Person"}`; 50 per page; the plan view shows its explain |
| `explain <filter>` | the plan of a find |
| `node <id>...`, `neighbours <id>` | lookups |
| `\limit <n>\|off` | on its own line: the command's limit |
| `-- …` | a comment |

## Keys

Hold `⌥` to see each key next to its control; `?` lists them. `⌘K` goes to anything (a namespace, a label, a node, a page, a command). `1` `2` `3` `4` switch between graph, table, plan and structure. `⌘L` opens the query editor, `⌘↵` runs it, `⌘J` the log, `⌘I` the instruments, `⌘S` commits. On the status page, `Space` pauses the live updates.

## Layout

| Path | What |
|---|---|
| `index.html`, `status.html` | The pages: classic scripts, so they work from `file://` |
| `src/source.js` | The Source contract: every read and write the pages make |
| `src/mock.js` | The mock Source |
| `src/rest.js` | The REST Source (`?source=rest`, which `serve.py` opens) |
| `serve.py` | Flask: serves the pages, passes `/v1` through to the server |
| `tools/seed.mjs` | Loads the sample namespaces into a server |
| `src/query.js` | The query line: commands, plan rows |
| `src/shared.js` | Formatting, values as text, label colours, the theme, the palette, the layout of results |
| `src/explorer.js`, `src/status.js` | The pages |
| `src/console.css` | The console's layout, built only from the design system's tokens |
| `design-system/` | The vendored design system and the console's changes to it ([README](design-system/README.md)) |
| `vendor/` | React 18.3.1 (UMD, production, MIT) |
| `tools/build.mjs` | Generates `design-system/tokens.css` and `design-system/bundle.js` |
| `test/` | `node --test` tests of both Sources, the query line and the helpers; pytest tests of `serve.py` |

## Develop

```sh
cd console
npm ci                 # esbuild, only to rebuild the bundle
npm run build          # after changing design-system/src/index.jsx or tokens.json
npm run check          # what CI runs: the committed generated files are up to date
npm test
uv run --with flask --with pytest pytest console/test   # from the repository root
```

Rules: no build step for the pages themselves; no dependencies at run time beyond `vendor/`; colours, type, spacing and motion only from the design system's tokens; a component the design system has is used, not rebuilt; the pages read and write only through the Source.
