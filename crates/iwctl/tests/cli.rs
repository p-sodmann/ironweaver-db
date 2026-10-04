//! `iwctl` run as a binary on temporary directories: each command, its
//! text and JSON output, and each exit code (0 ok, 1 damage, 2 usage,
//! 3 locked, 4 other failures).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use iwdb::{Mutation, Store, StoreOptions};

fn iwctl(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_iwctl")).args(args).output().unwrap()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("{}: {}", e, stdout(output)))
}

fn p(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// A closed store with `n` commits and a checkpoint in the middle (none at
/// the end), archiving into `archive` if given.
fn store(dir: &Path, n: usize, archive: Option<&Path>) {
    let mut options = StoreOptions { archive: archive.map(Path::to_path_buf), ..StoreOptions::default() };
    // The WAL holds records after the last checkpoint
    options.checkpoint.on_close = false;
    let store = Store::open(dir, options).unwrap();
    for i in 0..n {
        let mutation = Mutation::UpsertNode {
            id: format!("n{}", i % 7),
            labels: vec!["L".into()],
            attr: [("v".to_owned(), iwdb::Value::Int(i as i64))].into(),
            meta: Default::default(),
            expected_version: None,
        };
        store.commit(&[mutation]).unwrap();
        if i == n / 2 {
            store.checkpoint().unwrap();
        }
    }
    store.close().unwrap();
}

#[test]
fn status_of_a_store_a_backup_and_an_archive() {
    let work = tempfile::tempdir().unwrap();
    let (data, archive) = (work.path().join("data"), work.path().join("archive"));
    store(&data, 20, Some(&archive));
    let out = iwctl(&["status", p(&data)]);
    assert_eq!(code(&out), 0, "{}", stdout(&out));
    assert!(stdout(&out).contains("store: seq 20, synced seq 20, fsync always"), "{}", stdout(&out));
    // Under off, the synced seq is "none", not a durable seq
    let out = iwctl(&["--fsync", "off", "status", p(&data)]);
    assert!(stdout(&out).contains("synced seq none"), "{}", stdout(&out));
    let out = iwctl(&["--json", "--fsync", "off", "status", p(&data)]);
    let value = json(&out);
    assert_eq!(value["kind"], "data directory");
    assert_eq!(value["store"]["seq"], 20);
    assert!(value["store"]["synced_seq"].is_null());
    assert_eq!(value["in_use"], false);

    let backup = work.path().join("backup");
    assert_eq!(code(&iwctl(&["backup", p(&data), p(&backup)])), 0);
    let value = json(&iwctl(&["--json", "status", p(&backup)]));
    assert_eq!((value["kind"].as_str(), value["backup"]["seq"].as_u64()), (Some("backup"), Some(20)));
    assert!(value["store"].is_null(), "a backup isn't opened");
    let value = json(&iwctl(&["--json", "status", p(&archive)]));
    assert_eq!(value["kind"], "archive");
}

#[test]
fn a_store_in_use_is_locked() {
    let work = tempfile::tempdir().unwrap();
    let data = work.path().join("data");
    store(&data, 5, None);
    let open = Store::open(&data, StoreOptions::default()).unwrap();
    // status reads the files and says so
    let out = iwctl(&["status", p(&data)]);
    assert_eq!(code(&out), 3);
    assert!(stdout(&out).contains("in use"), "{}", stdout(&out));
    for args in [
        vec!["checkpoint", p(&data), "--no-archive"],
        vec!["backup", p(&data), p(&work.path().join("b"))],
        vec!["verify", p(&data)],
    ] {
        let out = iwctl(&args);
        assert_eq!(code(&out), 3, "{:?}: {}", args, String::from_utf8_lossy(&out.stderr));
        assert!(String::from_utf8_lossy(&out.stderr).contains("in use"), "{:?}", args);
    }
    drop(open);
    assert_eq!(code(&iwctl(&["verify", p(&data)])), 0);
}

#[test]
fn checkpoint_backup_restore_and_verify() {
    let work = tempfile::tempdir().unwrap();
    let (data, archive) = (work.path().join("data"), work.path().join("archive"));
    store(&data, 30, Some(&archive));
    let out = iwctl(&["--json", "checkpoint", p(&data), "--archive", p(&archive), "--keep", "1"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let value = json(&out);
    assert_eq!((value["seq"].as_u64(), value["written"].as_bool()), (Some(30), Some(true)));
    // Nothing new: nothing written, nothing removed
    let value = json(&iwctl(&["--json", "checkpoint", p(&data), "--archive", p(&archive), "--keep", "1"]));
    assert_eq!(value["written"], false);
    assert_eq!(value["removed_segments"], serde_json::json!([]));

    let backup = work.path().join("backup");
    let out = iwctl(&["backup", p(&data), p(&backup)]);
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).contains("default at seq 30") && stdout(&out).contains(": ok"), "{}", stdout(&out));
    // Not into a directory that has files
    assert_eq!(code(&iwctl(&["backup", p(&data), p(&backup)])), 4);

    let restored = work.path().join("restored");
    let out =
        iwctl(&["--json", "restore", p(&restored), "--backup", p(&backup), "--archive", p(&archive), "--seq", "12"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    // Two JSON objects: the restore, then its verification
    let text = stdout(&out);
    let mut lines = text.lines();
    let restore: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let verify: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    assert_eq!((restore["restore"]["seq"].as_u64(), verify["verify"]["ok"].as_bool()), (Some(12), Some(true)));
    assert_eq!(Store::open(&restored, StoreOptions::default()).unwrap().seq(), 12);
    // To a time later than every commit: the latest
    let out = iwctl(&[
        "restore",
        p(&work.path().join("by-time")),
        "--archive",
        p(&archive),
        "--time",
        "2999-01-01T00:00:00Z",
    ]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    // Beyond what the sources hold: a failure, not damage
    let out = iwctl(&["restore", p(&work.path().join("late")), "--backup", p(&backup), "--seq", "31"]);
    assert_eq!(code(&out), 4);

    let out = iwctl(&["verify", p(&archive)]);
    assert_eq!(code(&out), 0, "{}", stdout(&out));
    assert!(stdout(&out).contains("(archive): ok"), "{}", stdout(&out));
}

#[test]
fn verify_reports_damage_with_exit_code_1() {
    let work = tempfile::tempdir().unwrap();
    let data = work.path().join("data");
    store(&data, 20, None);
    let checkpoint =
        fs::read_dir(data.join("ns/00000000000000000001/checkpoints")).unwrap().next().unwrap().unwrap().path();
    let mut bytes = fs::read(&checkpoint).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0x40;
    fs::write(&checkpoint, bytes).unwrap();
    let out = iwctl(&["verify", p(&data)]);
    assert_eq!(code(&out), 1);
    assert!(stdout(&out).contains("DAMAGED") && stdout(&out).contains("problem:"), "{}", stdout(&out));
    let value = json(&iwctl(&["--json", "verify", p(&data)]));
    assert_eq!(value["verify"]["ok"], false);
    assert!(!value["verify"]["problems"].as_array().unwrap().is_empty());
}

#[test]
fn usage_errors_and_other_failures() {
    let work = tempfile::tempdir().unwrap();
    for args in [
        vec![],
        vec!["frobnicate"],
        vec!["status"],
        vec!["checkpoint", "x"],
        vec!["restore", "x"],
        vec!["verify", "--nope", "x"],
    ] {
        let out = iwctl(&args);
        assert_eq!(code(&out), 2, "{:?}", args);
        assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"), "{:?}", args);
    }
    assert_eq!(code(&iwctl(&["--help"])), 0);
    assert!(stdout(&iwctl(&["--version"])).starts_with("iwctl "));
    // Not a data directory: verify, status, checkpoint never create one
    let empty = work.path().join("empty");
    fs::create_dir(&empty).unwrap();
    for args in [vec!["verify", p(&empty)], vec!["status", p(&empty)], vec!["checkpoint", p(&empty), "--no-archive"]] {
        assert_eq!(code(&iwctl(&args)), 4, "{:?}", args);
    }
    assert_eq!(fs::read_dir(&empty).unwrap().count(), 0);
    let out = iwctl(&["--json", "verify", p(&work.path().join("missing"))]);
    assert_eq!(code(&out), 4);
    assert!(json(&out)["error"].as_str().unwrap().contains("not an Ironweaver DB data directory"));
}

#[test]
fn import_and_export() {
    let work = tempfile::tempdir().unwrap();
    let dir = work.path().join("data");
    store(&dir, 20, None);
    let lgf = work.path().join("graph.lgf");
    fs::write(&lgf, "@nodes\nlabel name\na \"Ann Lee\"\nb Bob\n@arcs\n\t\tweight\na b 2.5\n@attributes\ncaption x\n")
        .unwrap();

    let out = iwctl(&["import", p(&dir), "people", p(&lgf)]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let text = stdout(&out);
    assert!(text.contains("imported namespace 'people' (id 2) from a lgf file: 2 nodes, 1 edges"), "{}", text);
    assert!(text.contains("left out (a namespace has no place for them): @attributes 'caption'"), "{}", text);

    // Exported as JSON by extension, and as binary; both import back
    let json_file = work.path().join("people.json");
    let out = iwctl(&["--json", "export", p(&dir), p(&json_file), "-n", "people"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    let exported = &json(&out)["exported"];
    assert_eq!(
        (exported["format"].as_str(), exported["seq"].as_u64(), exported["nodes"].as_u64()),
        (Some("json"), Some(1), Some(2))
    );
    assert_eq!(exported["bytes"].as_u64(), Some(fs::metadata(&json_file).unwrap().len()));
    let bin_file = work.path().join("people.out");
    assert_eq!(code(&iwctl(&["export", p(&dir), p(&bin_file), "-n", "people", "--format", "binary"])), 0);
    assert!(fs::read(&bin_file).unwrap().starts_with(b"IRONWEAV"));
    for (name, file) in [("from_json", &json_file), ("from_bin", &bin_file)] {
        let out = iwctl(&["--json", "import", p(&dir), name, p(file)]);
        assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(json(&out)["imported"]["edges"].as_u64(), Some(1));
    }
    // A merge into an existing namespace, default too
    let out = iwctl(&["import", p(&dir), "default", p(&lgf), "--merge"]);
    assert_eq!(code(&out), 0, "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        stdout(&out).contains("merged a lgf file into namespace 'default': 2 nodes, 1 edges in 2 commits"),
        "{}",
        stdout(&out)
    );
    let out = iwctl(&["--json", "import", p(&dir), "people", p(&lgf), "--merge"]);
    assert_eq!(json(&out)["merged"]["commits"].as_u64(), Some(2));
    assert_eq!(code(&iwctl(&["import", p(&dir), "nobody", p(&lgf), "--merge"])), 4);
    assert_eq!(code(&iwctl(&["export", p(&dir), p(&bin_file), "--merge"])), 2);

    // The default namespace exports too
    let out = iwctl(&["export", p(&dir), p(&work.path().join("default.bin"))]);
    assert!(stdout(&out).contains("(9 nodes, 1 edges)"), "{}", stdout(&out));

    // Failures: an existing namespace, a bad file, usage errors
    assert_eq!(code(&iwctl(&["import", p(&dir), "people", p(&lgf)])), 4);
    let bad = work.path().join("bad.lgf");
    fs::write(&bad, "@nodes\nlabel\na\n@arcs\nw\na b 1\n").unwrap();
    let out = iwctl(&["import", p(&dir), "bad", p(&bad)]);
    assert_eq!(code(&out), 4);
    assert!(String::from_utf8_lossy(&out.stderr).contains("line 6: no node 'b'"));
    for args in [
        vec!["import", p(&dir), "x"],
        vec!["import", p(&dir), "x", p(&lgf), "--format", "csv"],
        vec!["export", p(&dir), p(&bin_file), "--format", "lgf"],
        vec!["status", p(&dir), "--format", "json"],
    ] {
        assert_eq!(code(&iwctl(&args)), 2, "{:?}", args);
    }
    let out = iwctl(&["--json", "namespaces", p(&dir)]);
    assert_eq!(code(&out), 0);
    assert!(iwdb::verify(&dir).unwrap().is_ok());
}
