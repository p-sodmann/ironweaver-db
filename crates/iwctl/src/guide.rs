//! Step 16i: every `iwctl` command line that `documentation/operations.md`
//! shows in a code block parses with [`args::parse`](crate::args::parse).
//! `crates/iwdb-server/tests/operations.rs` checks the guide's settings,
//! metrics and TOML blocks, and runs its `sh run` blocks.

use crate::args;

const GUIDE: &str = include_str!("../../../documentation/operations.md");

/// The code blocks of a Markdown document, as (info string, lines), with
/// continuation lines (a trailing `\`) joined.
fn blocks(doc: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let mut current: Option<(String, Vec<String>)> = None;
    for line in doc.lines() {
        match (&mut current, line.strip_prefix("```")) {
            (None, Some(info)) => current = Some((info.trim().to_owned(), Vec::new())),
            (Some(_), Some(_)) => out.extend(current.take()),
            (Some((_, lines)), None) => match lines.last_mut() {
                Some(last) if last.ends_with('\\') => {
                    last.pop();
                    last.push(' ');
                    last.push_str(line.trim_start());
                }
                _ => lines.push(line.to_owned()),
            },
            (None, None) => {}
        }
    }
    out
}

/// A shell line's words, enough for the guide: quotes are removed, `#`
/// starts a comment, and `|`, `;`, `&&`, `>` and a closing `)` end a command
/// (they become words of their own, `"|"`).
fn words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            (None, '#') if !started => break,
            (None, c) if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, '|' | ';' | '>' | ')' | '&') => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
                if c == '&' && chars.peek() == Some(&'&') {
                    chars.next();
                }
                out.push("|".to_owned());
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(word);
    }
    out
}

/// The value the guide gives `S` (`export S="--server ..."`).
fn server_flags(blocks: &[(String, Vec<String>)]) -> Vec<String> {
    let line = blocks
        .iter()
        .flat_map(|(_, lines)| lines)
        .find_map(|l| l.trim().strip_prefix("export S="))
        .expect("the guide sets S");
    words(line).concat().split_whitespace().map(str::to_owned).collect()
}

/// Every `iwctl` command in a document's code blocks, as the arguments
/// after `iwctl` with `$S` expanded and other variables as `x`.
fn commands(doc: &str) -> Vec<(String, Vec<String>)> {
    let blocks = blocks(doc);
    let s = server_flags(&blocks);
    let mut out = Vec::new();
    for (info, lines) in &blocks {
        if info.starts_with("toml") || info.starts_with("yaml") {
            continue;
        }
        for line in lines {
            let words = words(line);
            let mut rest = words.iter();
            // In command position: at the start, after `|` (or `;`, ...),
            // after `NAME=value` prefixes, or in `$(...)`
            let mut command = true;
            while let Some(w) = rest.next() {
                let w = match w.rsplit_once("$(") {
                    Some((_, inner)) => inner,
                    None if w == "|" => {
                        command = true;
                        continue;
                    }
                    None if command && w.contains('=') => continue,
                    None if !command => continue,
                    None => w,
                };
                command = false;
                if !(w == "iwctl" || w.ends_with("iwctl.exe")) {
                    continue;
                }
                let mut args = Vec::new();
                for a in rest.by_ref().take_while(|a| *a != "|") {
                    match a.as_str() {
                        "$S" => args.extend(s.iter().cloned()),
                        a if a.starts_with('$') => args.push("x".to_owned()),
                        a => args.push(a.to_owned()),
                    }
                }
                out.push((line.clone(), args));
                // `take_while` ate the `|` (or the line ended)
                command = true;
            }
        }
    }
    out
}

#[test]
fn the_shell_words_of_the_guide() {
    assert_eq!(words(r#"echo "$P" | iwctl a 'b c' # note"#), ["echo", "$P", "|", "iwctl", "a", "b c"]);
    assert_eq!(
        words("X=$(iwctl t c $S u n | tail -n 1)"),
        ["X=$(iwctl", "t", "c", "$S", "u", "n", "|", "tail", "-n", "1", "|"]
    );
    assert_eq!(words("a && b > f"), ["a", "|", "b", "|", "f"]);
    let doc = "```sh\nexport S=\"--server https://h:1\"\ncargo build -p iwctl\nT=$(echo \"$P\" | iwctl token list $S ann) # c\nX=1 iwctl status d && iwctl.exe verify d\n```\n";
    let found: Vec<Vec<String>> = commands(doc).into_iter().map(|(_, a)| a).collect();
    assert_eq!(
        found,
        [vec!["token", "list", "--server", "https://h:1", "ann"], vec!["status", "d"], vec!["verify", "d"]]
    );
    let b = blocks("x\n```sh run\na \\\n  b\nc\n```\n```toml\nk = 1\n```\n");
    assert_eq!(
        b,
        [("sh run".to_owned(), vec!["a  b".to_owned(), "c".to_owned()]), ("toml".to_owned(), vec!["k = 1".to_owned()])]
    );
}

#[test]
fn every_iwctl_command_in_the_operations_guide_parses() {
    let commands = commands(GUIDE);
    // The guide shows the users, tokens, status, catalog, backup, verify,
    // prune, restore and jobs commands, locally and against a server
    assert!(commands.len() >= 25, "{} commands found", commands.len());
    let mut problems = Vec::new();
    for (line, args) in &commands {
        if let Err(e) = args::parse(args) {
            problems.push(format!("{}\n  {:?}: {}", line, args, e));
        }
    }
    assert!(problems.is_empty(), "iwctl commands in operations.md that don't parse:\n{}", problems.join("\n"));
    for needed in ["status", "backup", "restore", "verify", "archive", "jobs", "user", "token", "requests"] {
        assert!(commands.iter().any(|(_, a)| a.iter().any(|w| w == needed)), "no `iwctl {}` in the guide", needed);
    }
}
