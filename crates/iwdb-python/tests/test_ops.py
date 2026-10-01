"""Backups, verify and restore from Python."""

import datetime

import pytest

import iwdb
from conftest import commit


def history(store, n, start=0):
    for i in range(start, start + n):
        commit(store, id=f"n{i % 5}", attr={"i": i})


def test_backup_verify_restore(tmp_path):
    data, archive = tmp_path / "data", tmp_path / "archive"
    with iwdb.Store.open(data, archive=archive, segment_size=1024, checkpoint_keep=1) as store:
        history(store, 29)
        store.checkpoint()
        history(store, 1, start=29)
        report = store.backup(tmp_path / "backup")
        assert report["seq"] == 30 and report["history"] == store.history()
        # The commit time of record 30 (a backup right at a checkpoint holds no record: None)
        assert isinstance(report["time"], datetime.datetime) and report["time"].tzinfo is not None
        with pytest.raises(iwdb.InvalidError):
            store.backup(tmp_path / "backup")
        history(store, 20, start=30)
        store.checkpoint()
    verified = iwdb.verify(tmp_path / "backup")
    assert verified["ok"] and verified["kind"] == "backup" and verified["seq"] == 30
    assert iwdb.verify(archive)["ok"]

    restored = iwdb.restore(tmp_path / "r1", backup=tmp_path / "backup")
    assert restored["seq"] == 30 and restored["history"] != report["history"]
    restored = iwdb.restore(tmp_path / "r2", backup=tmp_path / "backup", archive=archive, seq=40)
    assert restored["seq"] == 40
    with iwdb.Store.open(tmp_path / "r2") as store:
        assert store.seq() == 40
        assert store.node("n4")["attr"] == {"i": 39}
        assert store.node("n0")["attr"] == {"i": 35}
    # The store refuses to open the backup itself
    with pytest.raises(iwdb.InvalidError, match="is a backup"):
        iwdb.Store.open(tmp_path / "backup")
    # Beyond what the sources hold
    with pytest.raises(iwdb.InvalidError):
        iwdb.restore(tmp_path / "r3", backup=tmp_path / "backup", seq=31)
    with pytest.raises(iwdb.InvalidError):
        iwdb.restore(tmp_path / "r4")
    with pytest.raises(ValueError):
        iwdb.restore(tmp_path / "r5", archive=archive, seq=1, time=datetime.datetime.now(datetime.timezone.utc))


def test_restore_to_a_time(tmp_path):
    data, archive = tmp_path / "data", tmp_path / "archive"
    with iwdb.Store.open(data, archive=archive, segment_size=1024, checkpoint_keep=1) as store:
        history(store, 10)
        middle = datetime.datetime.now(datetime.timezone.utc)
        seq_then = store.seq()
        history(store, 10, start=10)
        store.checkpoint()
    restored = iwdb.restore(tmp_path / "r", backup=data, archive=archive, time=middle)
    assert restored["seq"] == seq_then
    assert restored["time"] <= middle
    with pytest.raises(ValueError, match="aware"):
        iwdb.restore(tmp_path / "naive", archive=archive, time=datetime.datetime.now())
    with pytest.raises(iwdb.InvalidError, match="no commit at or before"):
        iwdb.restore(tmp_path / "early", archive=archive, time=datetime.datetime(2000, 1, 1, tzinfo=datetime.timezone.utc))


def test_verify_a_live_store_is_locked(path):
    with iwdb.Store.open(path) as store:
        commit(store, id="a")
        with pytest.raises(iwdb.LockedError):
            iwdb.verify(path)
    assert iwdb.verify(path)["ok"]
