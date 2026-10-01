"""kill -9 a Python process that commits: every acknowledged commit is
recovered (fsync "always")."""

import os
import signal
import subprocess
import sys
import textwrap

import pytest

import iwdb

CHILD = textwrap.dedent(
    """
    import sys
    import iwdb
    store = iwdb.Store.open(sys.argv[1], segment_size=1024)
    i = store.seq()
    print("open", i, flush=True)
    while True:
        i += 1
        with store.transaction() as tx:
            tx.upsert_node("n%d" % (i % 13), attr={"i": i, "pad": "x" * 100})
        print("ack", tx.result["seq"], i, flush=True)
    """
)


@pytest.mark.skipif(sys.platform == "win32", reason="SIGKILL")
@pytest.mark.parametrize("round", range(3))
def test_kill_9_loses_no_acknowledged_commit(path, round):
    acked = {}
    for _ in range(round + 1):
        child = subprocess.Popen([sys.executable, "-c", CHILD, str(path)], stdout=subprocess.PIPE, text=True)
        seen = 0
        for line in child.stdout:
            words = line.split()
            if words[0] == "ack":
                seq, i = int(words[1]), int(words[2])
                acked[seq] = i
                seen += 1
                if seen >= 40:
                    break
        os.kill(child.pid, signal.SIGKILL)
        child.wait()
        child.stdout.close()
    last = max(acked)
    with iwdb.Store.open(path) as store:
        assert store.seq() in (last, last + 1), "every acknowledged commit, maybe the one in flight"
        assert store.status()["recovery"]["seq"] == store.seq()
        # The last acknowledged write to each node is there
        for seq in range(last - 12, last + 1):
            i = acked[seq]
            node = store.node("n%d" % (i % 13))
            assert node["attr"]["i"] >= i
    assert iwdb.verify(path)["ok"]
