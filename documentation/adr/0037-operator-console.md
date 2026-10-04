# ADR 0037: The operator console

Status: accepted
Date: 2026-10-04

## Context

Step 16a adds a web interface for operators: a graph explorer in the spirit of Neo4j's browser, combined with phpMyAdmin's directness (browse a label as rows, see a namespace's structure, edit a value in place), plus a status page that shows the server at one glance. The look comes from the IronWeaver::DB design system, which ships React components (the workbench, graph canvas, inspector, result table, plan view, rail, log). The server doesn't expose what the status page needs yet (step 16 adds the status views and metrics), and this step doesn't connect to anything. Points to decide: where the console lives, how it is built, what it reads, and how the design system's demo-wired components show real data.

## Decision

- **Static pages in `console/`, no build step to run them.** Two HTML pages with classic scripts, React 18 (UMD) and the design system's bundle committed under `console/`. They open from `file://` or any static server, and `iwdb-server` can serve them later as files. Node (`esbuild`, a dev dependency) is needed only to regenerate the bundle and `tokens.css`; CI checks that the committed files are what the sources build to. No framework, no bundler for the pages, no runtime dependency beyond React.
- **One Source, a mock for now.** The pages read and write only through a Source object (`console/src/source.js` lists its methods). Its shapes are the REST API's JSON: attributes in the core's value form, filters as the core's `Expr` JSON, patterns as the core's text, mutations as `commit` bodies. The only Source is a mock (`console/src/mock.js`): four generated namespaces and a simulated server (background commits, checkpoints, requests, consumers, an online index build), and `?scenario=degraded` for problems. Connecting the console is writing a REST Source with the same methods; the pages don't change. The methods the server lacks (`schema` with label counts, `server`, `cancel`, the log) are marked in `source.js` as step 16's.
- **The shell's commands, no query language of our own.** The query line takes `match`, `find`, `explain`, `node` and `neighbours` as `iwctl shell` does (ADR 0036), with `\limit` and `--` comments. Remote reads stay bounded (design rule 5): pages of 50, 200 matches, 24 neighbours per expansion.
- **Edits are staged and committed together.** Changing a property, connecting or detaching edges builds a list of mutations; COMMIT sends them as one commit (one seq), DISCARD drops them. The canvas changes nothing before that.
- **The design system, vendored with marked changes.** `console/design-system/` holds its tokens, component source and stylesheet. The components run on demo data (canvas expansion invents nodes, the inspector shows fixed properties, the table sorts by its 4th column), so the console adds optional props (`neighbours` on the canvas, `properties` on the inspector, a pager on the table, …), each marked `console:` and listed in the folder's README. Without them the components behave as the design system's, so the changes can go back into the design system as they are. `tokens.css` is generated from `tokens.json`, since the design system's own copy was older.

## Consequences

- The console is reviewable and testable now, before the server has status views: the mock's behaviour is tested with `node --test`, and the pages are checked to call only the Source's methods.
- Nothing in the console is real yet; the rail says `MOCK DATA` on every page so no one mistakes it for a server's state.
- A JavaScript toolchain enters the repository, limited to `console/` and to rebuilding generated files. The Rust workspace doesn't depend on it.
- Taking a newer design system means merging its source with the marked changes (steps in `console/design-system/README.md`).
- What the mock does that the server doesn't (a `total` for `find`, `matched` in explain, label counts) has to be added server-side or dropped from the pages when the console is connected.
