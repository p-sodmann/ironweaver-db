//! `verify` (ADR 0011): a clean directory verifies without
//! problems and reports the seq recovery reaches; what a crash leaves (a
//! torn tail, temporary files, an interrupted cleanup) is a note; damage
//! in any file, and invariants broken behind the store's back, are
//! problems; and verify changes nothing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../iwdb-storage/tests/common/mod.rs"]
mod common;
mod support;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use common::{Action, Call, Rule, TestFs, When};
use iwdb::{Error, Kind, Store, Value, VerifyReport, verify};
use iwdb_engine::catalog::NamespaceName;
use iwdb_engine::codec::{self, GraphMeta};
use iwdb_storage::checkpoint::load_checkpoint;
use support::{checkpoint_path, checkpoints, frame, last_segment, options, reference, run, segment_seqs, snapshot};
use tempfile::TempDir;

/// A closed store with two checkpoints, WAL after the newest, several
/// segments and an index.
fn store_dir(seed: u64) -> (TempDir, u64) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(2)).unwrap();
    let mut reference = reference();
    let steps = support::workload(60, seed);
    run(&store, &mut reference, &steps[..20]);
    store.checkpoint().unwrap();
    run(&store, &mut reference, &steps[20..40]);
    store.checkpoint().unwrap();
    run(&store, &mut reference, &steps[40..]);
    let seq = store.seq();
    drop(store);
    assert_eq!(checkpoints(dir.path()).len(), 2);
    (dir, seq)
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn clean(report: &VerifyReport) {
    assert!(report.is_ok(), "{:#?}", report.problems);
}

#[test]
fn a_store_verifies_clean_and_reports_what_recovery_reaches() {
    let (dir, seq) = store_dir(1);
    let report = verify(dir.path()).unwrap();
    clean(&report);
    assert_eq!(report.kind, Kind::DataDir);
    assert_eq!(report.version, Some(iwdb_storage::layout::LAYOUT_VERSION));
    assert_eq!(report.seq, Some(seq));
    assert_eq!((report.checkpoints, report.checkpoints_checked), (2, 2));
    assert_eq!(report.segments, segment_seqs(dir.path()).len());
    assert!(report.records > 0 && report.time.is_some());
    assert_eq!(report.last_seq, Some(seq));
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert_eq!(store.seq(), seq);
    assert!(report.history == Some(store.history()));
}

#[test]
fn verify_takes_a_shared_lock_and_changes_nothing() {
    let (dir, _) = store_dir(2);
    let store = Store::open(dir.path(), options(2)).unwrap();
    assert!(matches!(verify(dir.path()), Err(Error::Locked { .. })));
    drop(store);
    let before = snapshot(dir.path());
    clean(&verify(dir.path()).unwrap());
    assert_eq!(snapshot(dir.path()), before);
    // Without a LOCK file it doesn't create one
    fs::remove_file(dir.path().join("LOCK")).unwrap();
    let before = snapshot(dir.path());
    clean(&verify(dir.path()).unwrap());
    assert_eq!(snapshot(dir.path()), before);
    // Two verifies at once (shared locks), and a store waits for neither
    fs::write(dir.path().join("LOCK"), b"").unwrap();
    let held = iwdb_storage::layout::lock_shared(dir.path()).unwrap();
    clean(&verify(dir.path()).unwrap());
    assert!(matches!(Store::open(dir.path(), options(2)), Err(Error::Locked { .. })));
    drop(held);
    Store::open(dir.path(), options(2)).unwrap();
}

#[test]
fn what_a_crash_leaves_is_noted_not_damage() {
    let (dir, seq) = store_dir(3);
    // A torn frame at the end, temporary files
    let bytes = frame(seq + 1, seq, &[7; 40]);
    OpenOptions::new().append(true).open(last_segment(dir.path())).unwrap().write_all(&bytes[..30]).unwrap();
    fs::write(dir.path().join("ns/00000000000000000001/checkpoints").join(".x.ckpt.1.0.tmp"), b"half").unwrap();
    fs::write(dir.path().join("ns/00000000000000000001/wal").join("00000000000000009999.wal.tmp"), b"half").unwrap();
    let report = verify(dir.path()).unwrap();
    clean(&report);
    assert_eq!(report.seq, Some(seq));
    assert!(report.notes.iter().any(|n| n.message.contains("torn tail")), "{:#?}", report.notes);
    assert_eq!(report.notes.iter().filter(|n| n.message.contains("temporary file")).count(), 2);
    assert_eq!(Store::open(dir.path(), options(2)).unwrap().seq(), seq);
}

/// A checkpoint whose cleanup failed: an extra checkpoint and the
/// segments it no longer needs stay ("Interrupted cleanup" in data-dir.md).
#[test]
fn an_interrupted_cleanup_is_tolerated() {
    let dir = tempfile::tempdir().unwrap();
    let fs = TestFs::default();
    let store = Store::open_with(fs.clone(), dir.path(), options(1)).unwrap();
    let mut reference = reference();
    let steps = support::workload(40, 4);
    run(&store, &mut reference, &steps[..20]);
    store.checkpoint().unwrap();
    run(&store, &mut reference, &steps[20..]);
    fs.add(Rule::new(Call::RemoveFile, When::Before, Action::Fail).path("/wal/"));
    assert!(store.checkpoint().is_err());
    let seq = store.seq();
    drop(store);
    let report = verify(dir.path()).unwrap();
    clean(&report);
    assert!(report.notes.iter().any(|n| n.message.contains("interrupted cleanup")), "{:#?}", report.notes);
    assert_eq!(report.seq, Some(seq));
}

/// Offsets to damage in a file: its start, a few in the middle, its end.
fn offsets(len: usize) -> Vec<usize> {
    let mut at = vec![0, 8, 12, 20, len / 3, len / 2, len * 2 / 3, len - 8, len - 1];
    at.retain(|&a| a < len);
    at.sort_unstable();
    at.dedup();
    at
}

/// Flip a byte at each of several offsets of every file (a fresh copy
/// each time): verify finds a problem, and changes nothing. The last frame
/// of the last segment is left out: damage there is a torn tail, which is
/// the clean end of the log.
#[test]
fn a_flipped_byte_in_any_file_is_a_problem() {
    let (pristine, _) = store_dir(5);
    let files: Vec<PathBuf> = snapshot(pristine.path())
        .into_iter()
        .map(|(path, _)| path.strip_prefix(pristine.path()).unwrap().to_path_buf())
        .filter(|path| path != Path::new("LOCK"))
        .collect();
    let last = last_segment(pristine.path()).strip_prefix(pristine.path()).unwrap().to_path_buf();
    let last_frame_start = {
        let bytes = fs::read(pristine.path().join(&last)).unwrap();
        let ends = common::frame_ends(&bytes);
        ends[ends.len() - 2]
    };
    let mut checked = 0;
    for file in &files {
        let len = fs::metadata(pristine.path().join(file)).unwrap().len() as usize;
        for at in offsets(len) {
            if *file == last && at >= last_frame_start {
                continue;
            }
            let copy = tempfile::tempdir().unwrap();
            copy_dir(pristine.path(), copy.path());
            let target = copy.path().join(file);
            let mut bytes = fs::read(&target).unwrap();
            bytes[at] ^= 0x20;
            fs::write(&target, &bytes).unwrap();
            let before = snapshot(copy.path());
            match verify(copy.path()) {
                Ok(report) => {
                    assert!(!report.is_ok(), "{} at {}: no problem found ({:#?})", file.display(), at, report.notes)
                }
                // A marker without our magic is someone else's file
                Err(Error::NotADataDir { .. }) if file == Path::new("IWDB") && at < 8 => {}
                Err(e) => panic!("{} at {}: {}", file.display(), at, e),
            }
            assert_eq!(snapshot(copy.path()), before, "verify changed {}", file.display());
            checked += 1;
        }
    }
    assert!(checked > 40, "{}", checked);
}

#[test]
fn missing_and_cut_wal_files_are_problems() {
    let (pristine, _) = store_dir(6);
    let segments = segment_seqs(pristine.path());
    assert!(segments.len() > 3);
    // A segment missing in the middle: a gap
    let copy = tempfile::tempdir().unwrap();
    copy_dir(pristine.path(), copy.path());
    let middle = segments[segments.len() / 2];
    fs::remove_file(copy.path().join("ns/00000000000000000001/wal").join(iwdb_storage::format::segment_name(middle)))
        .unwrap();
    assert!(!verify(copy.path()).unwrap().is_ok());
    // The WAL cut before the newest checkpoint
    let copy = tempfile::tempdir().unwrap();
    copy_dir(pristine.path(), copy.path());
    let newest = *checkpoints(copy.path()).last().unwrap();
    for seq in segment_seqs(copy.path()).into_iter().filter(|s| *s > newest - 3) {
        fs::remove_file(copy.path().join("ns/00000000000000000001/wal").join(iwdb_storage::format::segment_name(seq)))
            .unwrap();
    }
    let report = verify(copy.path()).unwrap();
    assert!(report.problems.iter().any(|p| p.message.contains("before this checkpoint")), "{:#?}", report);
    // The WAL that the oldest checkpoint needs is gone
    let copy = tempfile::tempdir().unwrap();
    copy_dir(pristine.path(), copy.path());
    fs::remove_file(
        copy.path().join("ns/00000000000000000001/wal").join(iwdb_storage::format::segment_name(segments[0])),
    )
    .unwrap();
    let report = verify(copy.path()).unwrap();
    assert!(report.problems.iter().any(|p| p.message.contains("doesn't reach back")), "{:#?}", report);
}

/// Rewrite the checkpoint at `seq` in `dir` after `change` modified its
/// graph: a valid file (checksums and all) with a changed state.
fn rewrite_checkpoint(dir: &Path, seq: u64, change: impl FnOnce(&mut iwdb_engine::DbGraph)) {
    let name = NamespaceName::new(iwdb::NAMESPACE).unwrap();
    let path = checkpoint_path(dir, seq);
    let mut loaded = load_checkpoint(&path, seq, &name).unwrap();
    change(&mut loaded.graph);
    loaded.graph.flush_indexes().unwrap();
    let meta = GraphMeta { ..loaded.meta };
    fs::write(&path, codec::to_binary(&loaded.graph, &meta).unwrap()).unwrap();
}

#[test]
fn invariants_broken_behind_the_stores_back_are_problems() {
    let (pristine, _) = store_dir(7);
    let [older, newer] = checkpoints(pristine.path())[..] else { panic!() };

    // A version of 0 in the newest checkpoint
    let copy = tempfile::tempdir().unwrap();
    copy_dir(pristine.path(), copy.path());
    rewrite_checkpoint(copy.path(), newer, |g| {
        let ix = g.nodes().next().unwrap().0;
        g.node_mut(ix).unwrap().data.version = 0;
    });
    let report = verify(copy.path()).unwrap();
    assert!(report.problems.iter().any(|p| p.message.contains("has version 0")), "{:#?}", report);
    // and its state no longer matches the WAL
    assert!(report.problems.iter().any(|p| p.message.contains("differs from the WAL")), "{:#?}", report);

    // A valid-looking checkpoint whose data differs from what the WAL says
    let copy = tempfile::tempdir().unwrap();
    copy_dir(pristine.path(), copy.path());
    rewrite_checkpoint(copy.path(), newer, |g| {
        let ix = g.nodes().next().unwrap().0;
        g.node_mut(ix).unwrap().data.attr.insert("planted".into(), Value::Int(1));
    });
    let report = verify(copy.path()).unwrap();
    assert_eq!(report.problems.len(), 1, "{:#?}", report);
    assert!(report.problems[0].message.contains("differs from the WAL replayed"), "{:#?}", report);
    // (recovery trusts the newest checkpoint: only verify notices)
    let store = Store::open(copy.path(), options(2)).unwrap();
    assert!(store.recovery().skipped_checkpoints.is_empty());
    drop(store);

    // An index the catalog doesn't declare, saved in the oldest checkpoint
    let copy = tempfile::tempdir().unwrap();
    copy_dir(pristine.path(), copy.path());
    rewrite_checkpoint(copy.path(), older, |g| {
        g.create_index::<iwdb_engine::Error>(&["planted".to_owned()]).unwrap();
    });
    let report = verify(copy.path()).unwrap();
    assert!(report.problems.iter().any(|p| p.message.contains("saved indexes differ")), "{:#?}", report);
}

#[test]
fn directories_that_are_not_data_directories_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(verify(dir.path()), Err(Error::NotADataDir { .. })));
    assert!(matches!(verify(&dir.path().join("missing")), Err(Error::NotADataDir { .. })));
    fs::write(dir.path().join("RESTORING"), b"").unwrap();
    match verify(dir.path()) {
        Err(Error::NotADataDir { reason, .. }) => assert!(reason.contains("interrupted restore"), "{}", reason),
        other => panic!("{:?}", other),
    }
    // A step 5 directory verifies, unchanged
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/data-dir-v1/store");
    let before = snapshot(&fixture);
    let report = verify(&fixture).unwrap();
    clean(&report);
    assert_eq!((report.version, report.history, report.seq), (Some(1), None, Some(58)));
    assert_eq!(snapshot(&fixture), before);
}

/// The WAL cut back before the only checkpoint (as an OS crash under
/// `off` can do): recovery refuses, and so must verify.
#[test]
fn a_wal_that_ends_before_the_only_checkpoint_is_a_problem() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), options(1)).unwrap();
    let mut reference = reference();
    run(&store, &mut reference, &support::workload(20, 8));
    store.checkpoint().unwrap();
    drop(store);
    // Cut the segment holding the checkpoint's seq back to its header, and
    // remove the later ones: the WAL now ends before the checkpoint
    let newest = *checkpoints(dir.path()).last().unwrap();
    let segments = segment_seqs(dir.path());
    let holding = *segments.iter().rfind(|s| **s <= newest).unwrap();
    for seq in segments {
        let path = dir.path().join("ns/00000000000000000001/wal").join(iwdb_storage::format::segment_name(seq));
        if seq == holding {
            let bytes = fs::read(&path).unwrap();
            fs::write(&path, &bytes[..iwdb_storage::format::SEGMENT_HEADER_LEN]).unwrap();
        } else if seq > holding {
            fs::remove_file(&path).unwrap();
        }
    }
    assert!(Store::open(dir.path(), options(1)).is_err());
    let report = verify(dir.path()).unwrap();
    assert!(report.problems.iter().any(|p| p.message.contains("before the checkpoint")), "{:#?}", report);
}

/// Rewrite the key table of the checkpoint at `seq` in `dir`.
fn rewrite_keys(dir: &Path, seq: u64, keys: iwdb::KeyTable) {
    let name = NamespaceName::new(iwdb::NAMESPACE).unwrap();
    let path = checkpoint_path(dir, seq);
    let loaded = load_checkpoint(&path, seq, &name).unwrap();
    let meta = GraphMeta { keys, ..loaded.meta };
    fs::write(&path, codec::to_binary(&loaded.graph, &meta).unwrap()).unwrap();
}

/// The saved table with its first fingerprint's lowest bit flipped.
fn flip_first_fingerprint(json: String) -> String {
    let at = json.find("\"fingerprint\":").unwrap() + "\"fingerprint\":".len();
    let end = at + json[at..].find(|c: char| !c.is_ascii_digit()).unwrap();
    let flipped = json[at..end].parse::<u32>().unwrap() ^ 1;
    format!("{}{}{}", &json[..at], flipped, &json[end..])
}

/// A table read back from its saved JSON after `edit`, unchecked against a
/// seq (so that it can be planted where it doesn't belong).
fn edited(keys: &iwdb::KeyTable, edit: impl FnOnce(String) -> String) -> iwdb::KeyTable {
    let Value::String(json) = keys.to_meta_value() else { panic!("a string") };
    iwdb::KeyTable::from_meta_value(&Value::String(edit(json)), u64::MAX).unwrap()
}

#[test]
fn an_inconsistent_or_invalid_key_table_is_a_problem() {
    // A store with keyed commits before each checkpoint
    let pristine = tempfile::tempdir().unwrap();
    let store = Store::open(pristine.path(), options(2)).unwrap();
    let mut reference = reference();
    let steps = support::workload(50, 11);
    run(&store, &mut reference, &steps[..10]);
    support::run_keyed(&store, &mut reference, &steps[10..20], "a");
    store.checkpoint().unwrap();
    support::run_keyed(&store, &mut reference, &steps[20..35], "b");
    store.checkpoint().unwrap();
    run(&store, &mut reference, &steps[35..]);
    drop(store);
    clean(&verify(pristine.path()).unwrap());
    let [older, newer] = checkpoints(pristine.path())[..] else { panic!() };
    let name = NamespaceName::new(iwdb::NAMESPACE).unwrap();
    let keys = load_checkpoint(&checkpoint_path(pristine.path(), newer), newer, &name).unwrap().meta.keys;
    assert!(keys.len() > 5);

    // A fingerprint the WAL doesn't say: the newer checkpoint differs from the replay
    let copy = tempfile::tempdir().unwrap();
    copy_dir(pristine.path(), copy.path());
    rewrite_keys(copy.path(), newer, edited(&keys, flip_first_fingerprint));
    let report = verify(copy.path()).unwrap();
    assert_eq!(report.problems.len(), 1, "{:#?}", report);
    assert!(report.problems[0].message.contains("idempotency key tables differ"), "{:#?}", report);

    // A table missing its keys altogether
    let copy = tempfile::tempdir().unwrap();
    copy_dir(pristine.path(), copy.path());
    rewrite_keys(copy.path(), newer, iwdb::KeyTable::new());
    let report = verify(copy.path()).unwrap();
    assert!(report.problems.iter().any(|p| p.message.contains("idempotency key tables differ")), "{:#?}", report);

    // Entries after the checkpoint's seq: it doesn't load
    let copy = tempfile::tempdir().unwrap();
    copy_dir(pristine.path(), copy.path());
    rewrite_keys(copy.path(), older, keys.clone());
    let report = verify(copy.path()).unwrap();
    assert!(
        report.problems.iter().any(|p| p.message.contains("can't be loaded") && p.message.contains("key table")),
        "{:#?}",
        report
    );
}
