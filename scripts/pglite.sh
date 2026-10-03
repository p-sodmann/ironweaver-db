#!/bin/sh
# Start PGlite (Postgres compiled to WebAssembly, https://pglite.dev) on a
# local port for the Postgres tests of projection mode (step 13, ADR 0032).
# Needs only Node (npx); the data lives in memory and is gone when it stops.
#
#   scripts/pglite.sh &              # or in another terminal
#   export IWDB_TEST_POSTGRES_URL=postgresql://postgres:postgres@127.0.0.1:55432/postgres?sslmode=disable
#   cargo test -p iwdb --features postgres --test postgres
#
# PORT overrides the port. PGlite is one Postgres session: concurrent
# connections are served one statement at a time, so the tests use
# autocommit statements only.
set -eu
PORT="${PORT:-55432}"
echo "IWDB_TEST_POSTGRES_URL=postgresql://postgres:postgres@127.0.0.1:${PORT}/postgres?sslmode=disable" >&2
exec npx --yes -p @electric-sql/pglite@0.5.8 -p @electric-sql/pglite-socket@0.2.11 \
    pglite-server --host=127.0.0.1 --port="${PORT}" --max-connections=16
