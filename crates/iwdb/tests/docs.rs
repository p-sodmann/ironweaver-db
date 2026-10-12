//! Step 17d: the user-facing documentation against the code.
//!
//! - `documentation/guarantees.md`: every guarantee names the tests that
//!   check it, and every test it names exists. A guarantee is a top-level
//!   bullet, or a row of a table with a `Tests` column, outside the parts
//!   marked `**Assumes:**`, `**Not guaranteed:**` and `**Notes:**`; it ends
//!   with `Tests:` and the tests in backticks: a function name, `file::name`
//!   (a test in `file.rs` or `file.py`), or `workflow.yml#job` (a CI job).
//! - `documentation/README.md`, `concepts.md` and `guarantees.md`: every
//!   relative link leads to a file that exists, and every `#anchor` to a
//!   heading of its page.
//!
//! The examples of the other pages are checked where they live:
//! `operations.rs` and `crates/iwctl/src/guide.rs` (the operations guide,
//! `iwctl.md`, the configuration blocks), and `test_docs.py` (the Python
//! blocks of the README and `python-api.md`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// Directories that hold no test of ours.
const SKIP: &[&str] = &["target", ".git", "node_modules", "vendor", "design-system", "__pycache__", ".venv", "venv"];

/// Every `.rs` and `.py` file under `dir`.
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if path.is_dir() {
            if !SKIP.contains(&name.as_str()) && !name.starts_with('.') {
                sources(&path, out);
            }
        } else if name.ends_with(".rs") || name.ends_with(".py") {
            out.push(path);
        }
    }
}

/// The identifier after each `keyword ` in `text` (`fn` or `def`).
fn defined(text: &str, keyword: &str) -> Vec<String> {
    let mut out = Vec::new();
    let pattern = format!("{} ", keyword);
    let mut from = 0;
    while let Some(at) = text[from..].find(&pattern) {
        let start = from + at;
        from = start + pattern.len();
        let before = text[..start].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let name: String = text[from..].chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        if !name.is_empty() {
            out.push(name);
        }
    }
    out
}

/// What a guarantee can name: functions by name, by file stem, and CI jobs.
struct Known {
    any: BTreeSet<String>,
    by_file: BTreeMap<String, BTreeSet<String>>,
    jobs: BTreeSet<String>,
}

impl Known {
    fn load() -> Known {
        let root = root();
        let mut files = Vec::new();
        sources(&root, &mut files);
        let mut known = Known { any: BTreeSet::new(), by_file: BTreeMap::new(), jobs: BTreeSet::new() };
        for file in files {
            let text = fs::read_to_string(&file).unwrap_or_default();
            let keyword = if file.extension().is_some_and(|e| e == "py") { "def" } else { "fn" };
            let stem = file.file_stem().unwrap().to_string_lossy().into_owned();
            for name in defined(&text, keyword) {
                known.any.insert(name.clone());
                known.by_file.entry(stem.clone()).or_default().insert(name);
            }
        }
        // The jobs of the workflows: the keys two spaces deep under `jobs:`
        for entry in fs::read_dir(root.join(".github/workflows")).unwrap() {
            let path = entry.unwrap().path();
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            let text = fs::read_to_string(&path).unwrap();
            let mut in_jobs = false;
            for line in text.lines() {
                if !line.starts_with(' ') && !line.is_empty() {
                    in_jobs = line == "jobs:";
                } else if in_jobs
                    && let Some(job) = line.strip_prefix("  ").and_then(|l| l.strip_suffix(':'))
                    && !job.starts_with(' ')
                {
                    known.jobs.insert(format!("{}#{}", file, job));
                }
            }
        }
        known
    }

    fn resolves(&self, reference: &str) -> bool {
        if reference.contains('#') {
            return self.jobs.contains(reference);
        }
        match reference.split_once("::") {
            Some((file, name)) => self.by_file.get(file).is_some_and(|names| names.contains(name)),
            None => self.any.contains(reference),
        }
    }
}

/// The text between backticks in `text`.
fn quoted(text: &str) -> Vec<&str> {
    text.split('`').skip(1).step_by(2).collect()
}

/// A guarantee's tests: what follows its last `Tests:`.
fn tests_of(text: &str) -> Option<Vec<&str>> {
    text.rfind("Tests:").map(|at| quoted(&text[at..]))
}

/// The problems of a guarantees page: guarantees without tests, and tests
/// that don't exist. Also returns how many guarantees it checked.
fn check_guarantees(page: &str, known: &Known) -> (usize, Vec<String>) {
    let mut problems = Vec::new();
    let mut checked = 0;
    // Before the first section: the page's introduction
    let mut exempt = true;
    let mut tests_column: Option<usize> = None;
    let mut section = String::new();
    for (n, line) in page.lines().enumerate() {
        let at = format!("line {} ({})", n + 1, section);
        if let Some(heading) = line.strip_prefix("## ").or_else(|| line.strip_prefix("### ")) {
            section = heading.to_owned();
            exempt = false;
            tests_column = None;
            continue;
        }
        if line.starts_with("**") && line.trim_end().ends_with(":**") {
            exempt = matches!(line.trim_end(), "**Assumes:**" | "**Not guaranteed:**" | "**Notes:**");
            continue;
        }
        if line.starts_with('|') {
            let cells: Vec<&str> = line.trim().trim_matches('|').split('|').map(str::trim).collect();
            if cells.iter().all(|c| c.chars().all(|c| c == '-' || c == ':')) {
                continue;
            }
            match tests_column {
                None => tests_column = cells.iter().position(|c| *c == "Tests"),
                Some(column) => {
                    checked += 1;
                    let named = cells.get(column).map(|c| quoted(c)).unwrap_or_default();
                    if named.is_empty() {
                        problems.push(format!("{}: a row without tests: {}", at, cells[0]));
                    }
                    for test in named.into_iter().filter(|t| !known.resolves(t)) {
                        problems.push(format!("{}: no test `{}`", at, test));
                    }
                }
            }
            continue;
        }
        tests_column = None;
        if exempt || !line.starts_with("- ") {
            continue;
        }
        checked += 1;
        match tests_of(line) {
            Some(named) if !named.is_empty() => {
                for test in named.into_iter().filter(|t| !known.resolves(t)) {
                    problems.push(format!("{}: no test `{}`", at, test));
                }
            }
            _ => problems.push(format!("{}: a guarantee that names no test: {:.80}", at, line)),
        }
    }
    (checked, problems)
}

#[test]
fn every_guarantee_names_tests_that_exist() {
    let page = fs::read_to_string(root().join("documentation/guarantees.md")).unwrap();
    let known = Known::load();
    let (checked, problems) = check_guarantees(&page, &known);
    assert!(problems.is_empty(), "guarantees.md:\n{}", problems.join("\n"));
    assert!(checked > 150, "only {} guarantees found", checked);
}

#[test]
fn the_guarantees_check_finds_what_it_should() {
    let known = Known {
        any: ["a_test".to_owned()].into(),
        by_file: [("file".to_owned(), ["b_test".to_owned()].into())].into(),
        jobs: ["ci.yml#rust".to_owned()].into(),
    };
    let page = "intro\n- not a guarantee\n## A\n- kept. Tests: `a_test`, `file::b_test`, `ci.yml#rust`.\n\
                - no tests\n- missing. Tests: `gone`, `other::b_test`.\n**Not guaranteed:**\n- free\n\
                **Sizes:**\n- back. Tests: `a_test`.\n\n| Failure | Tests |\n|---|---|\n| x | `a_test` |\n| y | |\n";
    let (checked, problems) = check_guarantees(page, &known);
    assert_eq!(checked, 6);
    assert_eq!(problems.len(), 4, "{:#?}", problems);
    assert!(problems[0].contains("names no test"));
    assert!(problems[1].contains("`gone`") && problems[2].contains("`other::b_test`"));
    assert!(problems[3].contains("a row without tests: y"));
}

/// A heading's anchor, as GitHub makes it.
fn slug(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

fn anchors(page: &str) -> BTreeSet<String> {
    let mut fenced = false;
    let mut out = BTreeSet::new();
    for line in page.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
        } else if !fenced && line.starts_with('#') {
            out.insert(slug(line.trim_start_matches('#')));
        }
    }
    out
}

/// The targets of a page's inline links (`](target)`), outside code.
fn links(page: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fenced = false;
    for line in page.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let mut rest = line;
        while let Some(at) = rest.find("](") {
            let after = &rest[at + 2..];
            let Some(end) = after.find(')') else { break };
            out.push(after[..end].to_owned());
            rest = &after[end..];
        }
    }
    out
}

#[test]
fn the_index_concepts_and_guarantees_link_to_what_exists() {
    let docs = root().join("documentation");
    let mut problems = Vec::new();
    let mut checked = 0;
    for name in ["README.md", "concepts.md", "guarantees.md"] {
        let page = fs::read_to_string(docs.join(name)).unwrap();
        for link in links(&page) {
            if link.starts_with("http://") || link.starts_with("https://") || link.starts_with("mailto:") {
                continue;
            }
            checked += 1;
            let (file, anchor) = match link.split_once('#') {
                Some((file, anchor)) => (file, Some(anchor)),
                None => (link.as_str(), None),
            };
            let target = if file.is_empty() { docs.join(name) } else { docs.join(file) };
            if !target.exists() {
                problems.push(format!("{}: {} doesn't exist", name, link));
                continue;
            }
            if let Some(anchor) = anchor
                && target.extension().is_some_and(|e| e == "md")
                && !anchors(&fs::read_to_string(&target).unwrap()).contains(anchor)
            {
                problems.push(format!("{}: no heading for {}", name, link));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    assert!(checked > 40, "only {} links found", checked);
}

#[test]
fn slugs_are_githubs() {
    assert_eq!(slug(" Authentication and the first admin"), "authentication-and-the-first-admin");
    assert_eq!(slug("TLS and mTLS"), "tls-and-mtls");
    assert_eq!(slug("`Store::open` (step 5)"), "storeopen-step-5");
}
