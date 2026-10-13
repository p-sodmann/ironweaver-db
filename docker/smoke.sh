#!/bin/sh
# The image's smoke test (step 13a; moved out of ci.yml#docker in step 17e,
# so the release workflow runs the same checks on the image it ships):
# the image serves over TLS, refuses to start without a certificate or with
# half the plaintext flags, answers with authentication on, reloads its
# certificate on SIGHUP, turns healthy, and stops cleanly on docker stop.
#
#   sh docker/smoke.sh <image> [<gRPC-only image>]
#
# <image> has the default features (grpc, rest, postgres, console); the
# plaintext check runs on the gRPC-only image if one is given, else on
# <image>. With EXPECT_VERSION set, `--version` must print it. Run it from
# the repository's root, with ports 7600 and 7601 free; it leaves its
# containers behind (CI's runners are thrown away).

set -eu

image=$1
grpc=${2:-}

docker run --rm "$image" --version | tee version.txt
grep -q "grpc, rest, postgres, console" version.txt
if [ -n "${EXPECT_VERSION:-}" ]; then
    grep -q "^iwdb-server ${EXPECT_VERSION} (" version.txt
    docker run --rm --entrypoint iwctl "$image" --version | grep -q "^iwctl ${EXPECT_VERSION}$"
fi
if [ -n "$grpc" ]; then
    docker run --rm "$grpc" --version | tee version.txt
    grep -q "(grpc)" version.txt
fi
# No certificate or key in the image (step 15b)
test -z "$(docker run --rm --entrypoint find "$image" / -xdev \( -name '*.pem' -o -name '*.key' \) -not -path '/proc/*' -not -path '/usr/share/*' -not -path '/etc/ssl/*')"
# TLS is the default: without a mounted certificate the image doesn't start (exit 2)
docker run --name refused "$image" || test "$(docker inspect -f '{{.State.ExitCode}}' refused)" = 2
docker logs refused 2>&1 | grep -q "can't read the TLS certificate /etc/iwdb/tls/server.pem"
# Plaintext on 0.0.0.0 needs both flags (exit 2 with one)
docker run --name half -e IWDB_TLS_ENABLED=false "$image" || test "$(docker inspect -f '{{.State.ExitCode}}' half)" = 2
docker logs half 2>&1 | grep -q IWDB_SERVER_PLAINTEXT_PUBLIC
# Over TLS with the development certificate
sh docker/dev-cert.sh
docker run -d --name iwdb -p 127.0.0.1:7600:7600 -v "$PWD/docker/tls:/etc/iwdb/tls:ro" \
  -e IWDB_AUTH_BOOTSTRAP_PASSWORD=ci-admin-password "$image"
CURL="curl --cacert docker/tls/ca.pem"
for i in $(seq 1 30); do $CURL -fsS https://127.0.0.1:7600/v1/health/ready && break; sleep 1; done
docker exec iwdb iwdb-server --probe --config /etc/iwdb/iwdb.toml
docker exec iwdb iwdb-server --probe https://127.0.0.1:7600
# The HEALTHCHECK (the probe from the config, over TLS) turns healthy
for i in $(seq 1 60); do test "$(docker inspect -f '{{.State.Health.Status}}' iwdb)" = healthy && break; sleep 1; done
test "$(docker inspect -f '{{.State.Health.Status}}' iwdb)" = healthy
# Plaintext gets no answer (`! curl` wouldn't fail under set -e)
if curl -fsS --max-time 5 http://127.0.0.1:7600/v1/health/ready; then
    echo "the TLS port answered over plaintext" >&2
    exit 1
fi
# Authentication is on: no token, 401; log in, then the namespaces
test "$($CURL -s -o /dev/null -w '%{http_code}' https://127.0.0.1:7600/v1/namespaces)" = 401
TOKEN=$($CURL -fsS https://127.0.0.1:7600/v1/auth/login -H 'content-type: application/json' \
  -d '{"user": "admin", "password": "ci-admin-password"}' | python3 -c 'import json, sys; print(json.load(sys.stdin)["token"])')
$CURL -fsS https://127.0.0.1:7600/v1/namespaces -H "authorization: Bearer $TOKEN" | tee namespaces.json
grep -q default namespaces.json
# SIGHUP reloads the certificate
docker kill --signal HUP iwdb
for i in $(seq 1 10); do docker logs iwdb 2>&1 | grep -q "reloaded the TLS certificate" && break; sleep 1; done
docker logs iwdb 2>&1 | grep -q "reloaded the TLS certificate"
docker stop iwdb
docker logs iwdb 2>&1 | tee server.log
test "$(docker inspect -f '{{.State.ExitCode}}' iwdb)" = 0
grep -q "closed /var/lib/iwdb" server.log
grep -q '"message":"serving /var/lib/iwdb on' server.log
# Plaintext with both flags, and its HEALTHCHECK over HTTP
docker run -d --name plain -p 127.0.0.1:7601:7600 -e IWDB_TLS_ENABLED=false \
  -e IWDB_SERVER_PLAINTEXT_PUBLIC=true -e IWDB_AUTH_BOOTSTRAP_PASSWORD=ci-admin-password "${grpc:-$image}"
for i in $(seq 1 30); do curl -fsS http://127.0.0.1:7601/v1/health/ready && break; sleep 1; done
docker exec plain iwdb-server --probe --config /etc/iwdb/iwdb.toml
for i in $(seq 1 60); do test "$(docker inspect -f '{{.State.Health.Status}}' plain)" = healthy && break; sleep 1; done
test "$(docker inspect -f '{{.State.Health.Status}}' plain)" = healthy
docker stop plain
