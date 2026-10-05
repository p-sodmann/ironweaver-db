# Security policy

Ironweaver DB is a small project with one maintainer. Security reports are welcome and taken seriously; this page says how to send one and what happens next.

## Reporting a vulnerability

Please **don't open a public issue** for a vulnerability. Report it privately on GitHub instead:

1. Go to the repository's [Security tab](https://github.com/p-sodmann/ironweaver-db/security) and choose **Report a vulnerability** (or open [the form](https://github.com/p-sodmann/ironweaver-db/security/advisories/new) directly).
2. Describe the problem, the affected version or commit, and how to reproduce it. A minimal reproduction helps most.

Only you and the maintainer see the report. It becomes a private advisory where we can discuss it, work on a fix together, and publish it when the fix is out.

A vulnerability in [`ironweaver-core`](https://github.com/p-sodmann/Ironweaver) that affects the database can be reported here too.

## What to expect

These are aims for a one-person project, not contractual guarantees:

- **An acknowledgement within 7 days.**
- **A first assessment within 14 days**: whether it is a vulnerability, how severe, and what is affected.
- **A fix or a mitigation, and coordinated disclosure, within 90 days.** If it takes longer, we agree on a date with you.
- **Credit** in the advisory and the changelog, unless you prefer not to be named.

## Supported versions

There is no release yet. Until 1.0, only the latest release and `main` receive security fixes. From 1.0 on, the latest minor release does.

## Scope

Everything in this repository is in scope:
- the server (`iwdb-server`), with its gRPC and REST APIs
- the libraries
- `iwctl`
- the Python package
- the operator console
- the Docker image

Examples of what counts:
- authentication or authorisation bypasses
- secrets in logs or errors
- durability or integrity guarantees broken by crafted input ([guarantees.md](documentation/guarantees.md))
- crashes or unbounded resource use triggered from the network

Out of scope:
- deployments that turn protections off, such as `[auth] enabled = false` or plaintext on a public address with `[server] plaintext_public = true`
- access by someone who can already read the data directory, whose file permissions are the boundary

## No telemetry

Ironweaver DB collects no data. The server, the libraries, `iwctl`, the Python package and the console send no telemetry, usage statistics, crash reports or update checks to anyone.

- The server opens no network connection of its own except to the projection sources you configure (Postgres, [projections.md](documentation/api/projections.md)). A test runs the server through logins, user, token, namespace and catalog changes and checks that its only sockets are its listener and the connections to it.
- The console's pages, fonts included, are served by the server itself and load nothing from another site.
- The logs and the audit log ([ADR 0049](documentation/adr/0049-audit-log.md)) stay where you configure them.
- Metrics are pulled, never pushed: a scraper you set up reads `GET /metrics`, with a token like any route ([metrics.md](documentation/api/metrics.md)). The log tail (`GetLog`, for the operator console) is held in the server's memory and readable by server admins only.
