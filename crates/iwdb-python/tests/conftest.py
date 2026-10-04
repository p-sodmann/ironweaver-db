"""Fixtures of the Python suite.

`store` runs each test that uses it twice: against an embedded store and
against a store from `iwdb.connect` to an `iwdb-server` started for the test
(ADR 0035). A test of calls only an embedded store has (`sync`,
`checkpoint`, backups, files) is marked `@pytest.mark.embedded`.

The server binary is `$IWDB_SERVER`, or the newest of
`target/{debug,release}/iwdb-server` (`cargo build -p iwdb-server`). Without
one the remote runs are skipped, unless `IWDB_REQUIRE_REMOTE=1` (CI), which
makes them fail.
"""

import os
import re
import signal
import subprocess
import threading
from pathlib import Path

import pytest

import iwdb

REPO = Path(__file__).resolve().parents[3]


def pytest_configure(config):
    config.addinivalue_line("markers", "embedded: the test needs an embedded store (skipped remotely)")


def server_binary():
    """The iwdb-server to start, or None."""
    if os.environ.get("IWDB_SERVER"):
        return Path(os.environ["IWDB_SERVER"])
    found = [REPO / "target" / kind / "iwdb-server" for kind in ("debug", "release")]
    found = [p for p in found if p.exists()]
    return max(found, key=lambda p: p.stat().st_mtime) if found else None


class Server:
    """An iwdb-server on a free port, serving `data_dir`."""

    def __init__(self, data_dir, config_dir):
        binary = server_binary()
        if binary is None:
            message = "no iwdb-server binary (cargo build -p iwdb-server, or set IWDB_SERVER)"
            if os.environ.get("IWDB_REQUIRE_REMOTE") == "1":
                pytest.fail(message)
            pytest.skip(message)
        config = Path(config_dir) / "iwdb.toml"
        config.write_text(
            'data_dir = "{}"\nlisten = "127.0.0.1:0"\n\n[store]\nfsync = "off"\n'.format(
                str(data_dir).replace("\\", "\\\\")
            )
        )
        self.stderr = []
        self.process = subprocess.Popen(
            [str(binary), "--config", str(config)], stderr=subprocess.PIPE, text=True
        )
        # "iwdb-server: serving <dir> on <address>"
        for line in self.process.stderr:
            self.stderr.append(line)
            match = re.search(r"serving .* on (\S+)$", line.strip())
            if match:
                self.endpoint = "http://" + match.group(1)
                break
        else:
            self.process.wait()
            pytest.fail("iwdb-server didn't start:\n" + "".join(self.stderr))
        # Keep reading, so the server never blocks on a full pipe
        threading.Thread(target=self._drain, daemon=True).start()

    def _drain(self):
        for line in self.process.stderr:
            self.stderr.append(line)

    def stop(self):
        """Shut down gracefully (SIGTERM); returns the exit code."""
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
        try:
            return self.process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.process.kill()
            raise


@pytest.fixture
def server(tmp_path):
    """A running iwdb-server on a new data directory."""
    server = Server(tmp_path / "server-data", tmp_path)
    yield server
    server.stop()


@pytest.fixture
def path(tmp_path):
    """A path for a new data directory."""
    return tmp_path / "data"


@pytest.fixture(params=["embedded", "remote"])
def store(request, path, tmp_path):
    """An open store (fsync "off": durability isn't under test here):
    embedded, or a client of a server."""
    if request.param == "embedded":
        with iwdb.Store.open(path, fsync="off") as store:
            yield store
        return
    if request.node.get_closest_marker("embedded"):
        pytest.skip("needs an embedded store")
    server = Server(path, tmp_path)
    try:
        with iwdb.connect(server.endpoint) as store:
            yield store
    finally:
        assert server.stop() == 0, "".join(server.stderr)


def commit(store, **node):
    """Upsert one node; returns the commit result."""
    with store.transaction() as tx:
        tx.upsert_node(**node)
    return tx.result
