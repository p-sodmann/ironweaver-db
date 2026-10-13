//! Step 16i: `documentation/operations.md` against the code. Every setting
//! and `IWDB_*` variable it names is in `config::KEYS`, every TOML block is
//! a valid configuration, and every metric it names (the alert rules
//! included) is exported, with the labels it selects on. On Unix (with the
//! `rest` feature) the blocks fenced as `sh run` run in order against the
//! `iwdb-server` binary: `sh run=server` starts the server it shows and
//! waits for ready, `sh run=stopped` stops it first (SIGTERM, exit 0).
//! `iwctl`'s own test (`crates/iwctl/src/guide.rs`) parses every `iwctl`
//! command line.
//!
//! Step 17d: the configuration blocks of the API references (`grpc.md`,
//! `config.md`, `projections.md`) are valid configurations too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use iwdb_query::metrics::{Kind, METRICS};
use iwdb_server::config::{Config, KEYS};

const GUIDE: &str = include_str!("../../../documentation/operations.md");

/// The code blocks, as (info string, lines), continuation lines kept.
fn blocks(doc: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut current: Option<(String, Vec<String>)> = None;
    for line in doc.lines() {
        match (&mut current, line.strip_prefix("```")) {
            (None, Some(info)) => current = Some((info.trim().to_owned(), Vec::new())),
            (Some(_), Some(_)) => out.extend(current.take()),
            (Some((_, lines)), None) => lines.push(line.to_owned()),
            (None, None) => {}
        }
    }
    out
}

/// The words of `text` made of the characters of `ok` that start with
/// `prefix`, each with the character after it.
fn tokens<'a>(text: &'a str, prefix: &str, ok: fn(char) -> bool) -> Vec<(&'a str, Option<char>)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find(prefix) {
        let start = from + at;
        let before = text[..start].chars().next_back();
        let len = text[start..].find(|c: char| !ok(c)).unwrap_or(text.len() - start);
        if !before.is_some_and(ok) {
            out.push((&text[start..start + len], text[start + len..].chars().next()));
        }
        from = start + len.max(1);
    }
    out
}

/// Variables the guide names that aren't settings: the bootstrap admin's
/// (config.md: variables only), `iwctl`'s token, compose.yaml's password,
/// and the typo of the guide's example of an invalid configuration.
const OTHER_VARIABLES: &[&str] = &[
    "IWDB_AUTH_BOOTSTRAP_PASSWORD",
    "IWDB_AUTH_BOOTSTRAP_USER",
    "IWDB_TOKEN",
    "IWDB_ADMIN_PASSWORD",
    "IWDB_STORE_FSYNCX",
];

#[test]
fn every_variable_and_setting_the_guide_names_exists() {
    let vars = tokens(GUIDE, "IWDB_", |c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    assert!(vars.len() > 20, "{}", vars.len());
    // `IWDB_*` and section prefixes (`IWDB_STORE_`) aren't variables
    for (var, _) in vars.iter().filter(|(v, _)| !v.ends_with('_')) {
        assert!(
            KEYS.iter().any(|k| k.var == *var) || OTHER_VARIABLES.contains(var),
            "no setting {} (operations.md)",
            var
        );
    }
    // `section.key` and `[section] key` in code spans
    let sections: Vec<&str> = KEYS.iter().filter_map(|k| k.key.split_once('.')).map(|(s, _)| s).collect();
    let mut checked = 0;
    for span in GUIDE.split('`').skip(1).step_by(2) {
        let key = match span.strip_prefix('[').and_then(|s| s.split_once("] ")) {
            Some((section, rest)) => format!("{}.{}", section, rest.split([' ', '=']).next().unwrap()),
            None => span.split([' ', '=']).next().unwrap().to_owned(),
        };
        let Some((section, _)) = key.split_once('.') else { continue };
        // The server's certificate files look like `server.*` settings
        let files = ["server.pem", "server.key"];
        if !sections.contains(&section) || key.contains('/') || key.ends_with(".md") || files.contains(&key.as_str()) {
            continue;
        }
        assert!(KEYS.iter().any(|k| k.key == key), "no setting `{}` (operations.md: `{}`)", key, span);
        checked += 1;
    }
    assert!(checked > 30, "{}", checked);
}

#[test]
fn every_toml_block_is_a_valid_configuration() {
    let mut checked = 0;
    for (info, lines) in blocks(GUIDE) {
        if !info.starts_with("toml") {
            continue;
        }
        let text = lines.join("\n");
        let mut table: toml::Table = toml::from_str(&text).unwrap_or_else(|e| panic!("{}\n{}", e, text));
        // A fragment gets what every configuration needs
        table.entry("data_dir").or_insert("data".into());
        let tls = table.entry("tls").or_insert(toml::Table::new().into()).as_table_mut().unwrap();
        tls.entry("cert").or_insert("tls/server.pem".into());
        tls.entry("key").or_insert("tls/server.key".into());
        let whole = toml::to_string(&table).unwrap();
        checked += 1;
        let config = match Config::parse(&whole) {
            Ok(config) => config,
            // A block for a feature this build lacks (`[tracing]` needs
            // `otel`): refused for that reason alone
            Err(e) if !cfg!(feature = "otel") && e == without("otel", "can't export traces") => continue,
            Err(e) => panic!("operations.md: {}\n{}", e, text),
        };
        if info == "toml file=iwdb.toml" {
            assert_eq!(whole, toml::to_string(&toml::from_str::<toml::Table>(&text).unwrap()).unwrap());
            assert!(config.tls.enabled && config.store.archive.is_some());
        }
    }
    assert!(checked >= 4, "{}", checked);
}

/// The TOML blocks of the API references, each a valid configuration
/// once a fragment gets `data_dir` and a certificate.
#[test]
fn every_toml_block_of_the_api_references_is_a_valid_configuration() {
    let pages = [
        ("grpc.md", include_str!("../../../documentation/api/grpc.md")),
        ("config.md", include_str!("../../../documentation/api/config.md")),
        ("projections.md", include_str!("../../../documentation/api/projections.md")),
    ];
    let mut checked = 0;
    for (page, text) in pages {
        for (info, lines) in blocks(text) {
            if !info.starts_with("toml") {
                continue;
            }
            let block = lines.join("\n");
            let mut table: toml::Table =
                toml::from_str(&block).unwrap_or_else(|e| panic!("{}: {}\n{}", page, e, block));
            table.entry("data_dir").or_insert("data".into());
            let tls = table.entry("tls").or_insert(toml::Table::new().into()).as_table_mut().unwrap();
            tls.entry("cert").or_insert("tls/server.pem".into());
            tls.entry("key").or_insert("tls/server.key".into());
            checked += 1;
            match Config::parse(&toml::to_string(&table).unwrap()) {
                Ok(_) => {}
                // A projection's Postgres source in a build without it
                Err(e) if !cfg!(feature = "postgres") && e.contains("built without the postgres feature") => {}
                Err(e) => panic!("{}: {}\n{}", page, e, block),
            }
        }
    }
    assert!(checked >= 2, "{}", checked);
}

/// The refusal of `[tracing] enabled` by a build without `feature`.
fn without(feature: &str, what: &str) -> String {
    format!("[tracing] enabled: this iwdb-server was built without the {} feature, so it {}", feature, what)
}

/// The workspace's crates, which a log filter names (`iwdb_storage=debug`).
const CRATES: &[&str] = &["iwdb_engine", "iwdb_storage", "iwdb_query", "iwdb_server", "iwdb_python"];

#[test]
fn every_metric_the_guide_names_is_exported_with_its_labels() {
    let names = tokens(GUIDE, "iwdb_", |c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    let mut checked = 0;
    for (at, (name, next)) in names.iter().enumerate() {
        if CRATES.contains(name) {
            continue;
        }
        let histogram = |base: &str| METRICS.iter().any(|d| d.name == base && d.kind == Kind::Histogram);
        let (def, bucket) = match ["_bucket", "_sum", "_count"].iter().find_map(|s| name.strip_suffix(s)) {
            Some(base) if histogram(base) => (METRICS.iter().find(|d| d.name == base), name.ends_with("_bucket")),
            _ => (METRICS.iter().find(|d| d.name == *name), false),
        };
        let def = def.unwrap_or_else(|| panic!("operations.md names {}, which isn't exported", name));
        if *next == Some('{') {
            // The selector after this occurrence
            let rest = GUIDE.split(name).nth(1 + names[..at].iter().filter(|(n, _)| n == name).count()).unwrap();
            let selector = &rest[1..rest.find('}').unwrap()];
            for matcher in selector.split(',') {
                let label = matcher.split(['=', '!']).next().unwrap().trim();
                assert!(
                    def.labels.contains(&label) || (bucket && label == "le"),
                    "{} has no label {} (operations.md: {{{}}})",
                    name,
                    label,
                    selector
                );
            }
        }
        checked += 1;
    }
    assert!(checked > 30, "{}", checked);
    // The alert rules are there, on the metrics the step asks for
    let rules = blocks(GUIDE)
        .into_iter()
        .map(|(_, lines)| lines.join("\n"))
        .find(|text| text.starts_with("groups:"))
        .expect("the alert rules");
    for metric in [
        "iwdb_disk_free_bytes",
        "iwdb_wal_bytes",
        "iwdb_checkpoint_lag_commits",
        "iwdb_wal_fsync_duration_seconds_bucket",
        "iwdb_memory_state",
        "code=\"unavailable\"",
        "code=\"timeout\"",
        "iwdb_checkpoint_failed",
        "iwdb_trace_spans_dropped_total",
    ] {
        assert!(rules.contains(metric), "no alert rule on {}", metric);
    }
}

/// The guide's `sh run` blocks, in order, against the binary (Unix: the
/// examples are POSIX `sh`; `rest`: they use REST routes).
#[cfg(all(unix, feature = "rest"))]
mod run {
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use super::{GUIDE, blocks};

    const ADMIN_PASSWORD: &str = "a-password-for-the-first-admin";

    /// Where `cargo test` put `iwdb-server`, and `iwctl` next to it.
    /// `cargo test --workspace` (CI's test job) always builds `iwctl`; a run
    /// of this crate alone (CI's feature-set jobs) may not, and skips.
    fn binaries() -> Option<PathBuf> {
        let dir = Path::new(env!("CARGO_BIN_EXE_iwdb-server")).parent().unwrap().to_owned();
        if dir.join("iwctl").exists() {
            return Some(dir);
        }
        eprintln!("skipped: no iwctl in {} (cargo build -p iwctl, or cargo test --workspace)", dir.display());
        None
    }

    /// Wait for `child` to end, at most `limit`.
    fn wait(child: &mut Child, limit: Duration) -> Option<std::process::ExitStatus> {
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return Some(status);
            }
            if start.elapsed() > limit {
                let _ = child.kill();
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// `sh` with a clean environment: the test's (cargo's variables, any
    /// `IWDB_*`) stays out; `.env` gives the rest.
    fn shell() -> Command {
        let mut command = Command::new("sh");
        command.env_clear();
        for keep in ["PATH", "HOME", "TMPDIR", "LANG"] {
            if let Some(value) = std::env::var_os(keep) {
                command.env(keep, value);
            }
        }
        command
    }

    /// A running server: its process and log file.
    struct Server {
        child: Child,
        log: PathBuf,
    }

    impl Server {
        fn log(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }

        /// SIGTERM, and a graceful end (exit 0, `closed`).
        fn stop(mut self) {
            let pid = self.child.id().to_string();
            assert!(Command::new("kill").args(["-TERM", &pid]).status().unwrap().success());
            let status = wait(&mut self.child, Duration::from_secs(60));
            let log = self.log();
            assert!(status.is_some_and(|s| s.success()), "{:?}\n{}", status, log);
            assert!(log.contains("\"message\":\"closed "), "{}", log);
        }
    }

    #[test]
    fn the_operations_guides_examples_run() {
        let Some(bin) = binaries() else { return };
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/tls");
        std::fs::create_dir(work.join("tls")).unwrap();
        for f in ["server.pem", "server.key", "ca.pem"] {
            std::fs::copy(fixtures.join(f), work.join("tls").join(f)).unwrap();
        }
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let port = |line: &str| line.replace(":7600", &format!(":{}", port));
        // Exported variables carry over from block to block (`export -p`)
        let env = work.join(".env");
        std::fs::write(
            &env,
            format!("export PATH='{}':\"$PATH\"\nexport ADMIN_PASSWORD='{}'\n", bin.display(), ADMIN_PASSWORD),
        )
        .unwrap();
        let blocks = blocks(GUIDE);
        for (info, lines) in &blocks {
            if let Some(name) = info.strip_prefix("toml file=") {
                let text: Vec<String> = lines.iter().map(|l| port(l)).collect();
                std::fs::write(work.join(name), text.join("\n") + "\n").unwrap();
            }
        }
        let mut server: Option<Server> = None;
        let mut ran = 0;
        for (n, (info, lines)) in blocks.iter().enumerate() {
            let script: Vec<String> = lines.iter().map(|l| port(l)).collect();
            let script = script.join("\n");
            match info.as_str() {
                "sh run=server" => {
                    assert!(server.is_none(), "block {}: a server is running already", n);
                    let log = work.join(format!("server-{}.log", n));
                    // `exec env`: the server is the shell's process, so SIGTERM reaches it
                    let child = shell()
                        .arg("-c")
                        .arg(format!(". ./.env && exec env {}", script.trim()))
                        .current_dir(work)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(std::fs::File::create(&log).unwrap())
                        .spawn()
                        .unwrap();
                    let mut s = Server { child, log };
                    let start = Instant::now();
                    while !s.log().contains("\"message\":\"serving ") {
                        let ended = s.child.try_wait().unwrap();
                        assert!(
                            ended.is_none() && start.elapsed() < Duration::from_secs(60),
                            "{}\n{}",
                            script,
                            s.log()
                        );
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    server = Some(s);
                }
                "sh run" | "sh run=stopped" => {
                    if info == "sh run=stopped" {
                        server.take().expect("a server to stop").stop();
                    }
                    let mut child = shell()
                        .arg("-ec")
                        .arg(format!(". ./.env\n{}\nexport -p > ./.env\n", script))
                        .current_dir(work)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .unwrap();
                    let status = wait(&mut child, Duration::from_secs(120));
                    let out = child.wait_with_output().unwrap();
                    assert!(
                        status.is_some_and(|s| s.success()),
                        "operations.md block {} failed ({:?}):\n{}\n--- stdout\n{}--- stderr\n{}--- server\n{}",
                        n,
                        status,
                        script,
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr),
                        server.as_ref().map(Server::log).unwrap_or_default()
                    );
                }
                _ => continue,
            }
            ran += 1;
        }
        assert!(ran >= 15, "{} blocks ran", ran);
        if let Some(s) = server {
            s.stop();
        }
    }
}
