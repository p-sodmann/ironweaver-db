# Operator console

A web interface for operators of an Ironweaver DB server (step 16a, [ADR 0037](../documentation/adr/0037-operator-console.md)). Two pages:

- **Explore** (`index.html`): walk through a graph like Neo4j's browser, and browse and edit a namespace like phpMyAdmin. The schema (namespaces, labels, edge types, indexes, constraints) is on the left; the result of the query line is in the middle as a graph, a table, a plan or the namespace's structure; the inspector opens with a selection. Hover a node to peek at its neighbours, double-click to bring them in. Edits (a property in the inspector, connecting or detaching on the canvas) are staged and committed together (`⌘S`).
- **Status** (`status.html`): the server at one glance. Health and problems, commits, latencies, memory and active requests over the last minute and a half, every namespace's state, latency by operation, active requests (with cancel), change-stream consumers, jobs and index builds, and the log.

**It runs on mock data.** Nothing talks to a server yet: `src/mock.js` generates four namespaces and simulates a running server. The rail says `MOCK DATA`. `?scenario=degraded` shows how problems look (a read-only namespace, a failed checkpoint, memory near the limit). Hooking it up is a later step: a Source over the REST API with the methods in `src/source.js`, served by `iwdb-server`.

## Open it

Open `index.html` or `status.html` in a browser, from the file system or any static server:

```sh
open console/index.html                      # macOS; or xdg-open
python3 -m http.server -d console 8000       # then http://localhost:8000
```

No build step is needed to run it: React, the design system's bundle and the console's scripts are committed. The fonts come from Google Fonts; offline, the pages fall back to Helvetica and the system's monospace.

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
| `src/query.js` | The query line: commands, plan rows |
| `src/shared.js` | Formatting, values as text, label colours, the theme, the palette, the layout of results |
| `src/explorer.js`, `src/status.js` | The pages |
| `src/console.css` | The console's layout, built only from the design system's tokens |
| `design-system/` | The vendored design system and the console's changes to it ([README](design-system/README.md)) |
| `vendor/` | React 18.3.1 (UMD, production, MIT) |
| `tools/build.mjs` | Generates `design-system/tokens.css` and `design-system/bundle.js` |
| `test/` | `node --test` tests of the mock, the query line and the helpers |

## Develop

```sh
cd console
npm ci                 # esbuild, only to rebuild the bundle
npm run build          # after changing design-system/src/index.jsx or tokens.json
npm run check          # what CI runs: the committed generated files are up to date
npm test
```

Rules: no build step for the pages themselves; no dependencies at run time beyond `vendor/`; colours, type, spacing and motion only from the design system's tokens; a component the design system has is used, not rebuilt; the pages read and write only through the Source.
