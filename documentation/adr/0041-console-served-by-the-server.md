# ADR 0041: The server serves the operator console

Status: accepted
Date: 2026-10-04

## Context

The operator console (step 16a, ADR 0037) reaches a server through a Flask proxy, so the pages and the API share an origin (the server answers no CORS preflight, on purpose). The owner wants it served by `iwdb-server` itself. The console writes to the database, and the server has no authentication until step 15, which is still to do. Points to decide: whether and how the server serves the pages, and what keeps that from exposing an unauthenticated write interface.

## Decision

- **Compiled in, behind a feature.** A cargo feature `console` (implies `rest`; off by default) compiles the pages into the binary with `include_bytes!`: the two pages, their scripts and styles, the design system's bundle and React, about 460 KB. A fixed list, so nothing else under `console/` (tools, tests, `serve.py`) is reachable and no request path touches the disk; a test checks that every file a page loads is in the list. The Docker image builds with it.
- **Off unless configured.** `[console] enabled = false` by default. When on, the gate serves `/console/` (redirect to `index.html?source=rest`), the files, and `console-config.json`, at any time, also during recovery. The pages use the REST API on the same origin, so no CORS is needed and the server still answers none.
- **Loopback only, unless said otherwise.** Until step 15, enabling the console on a non-loopback `listen` address is a configuration error unless `[console] public = true`: the same rule as step 15's "plaintext only with an explicit flag". In the image (which listens on `0.0.0.0` inside the container) that is `IWDB_CONSOLE_ENABLED=true IWDB_CONSOLE_PUBLIC=true` with the port published on localhost.
- **Headers.** `Cache-Control: no-cache` (a new binary may bring new pages), `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY` (the console writes: no other site may frame it), `Referrer-Policy: no-referrer`.
- **The proxy stays** for development: it serves the pages from disk, so editing them needs no rebuild. `rest.js` reads `console-config.json` relative to the page, so both work.

## Consequences

- One process and one port for the API and the console; no Python needed to use it.
- The pages are part of the binary: changing them needs a rebuild of a server with the feature.
- Step 15 puts the console behind authentication (the console's requests carry the same credentials as any REST client) and can drop the `public` switch.
