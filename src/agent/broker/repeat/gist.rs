//! What a worker was doing when the guard stopped it, in a few words the
//! person can read: "re-reading the same parts of meteora-dlmm-allocate-encode.js".
//! The guard cannot know what a call meant, only what its arguments say, so
//! this reads a shell command for the program it runs and the files it names,
//! and falls back to the command itself or to the tool's name.

use super::super::clip;
use serde_json::Value;
use std::collections::HashSet;

/// Programs that show part of a file, and programs that search.
const READERS: &[&str] = &[
    "sed", "cat", "head", "tail", "less", "more", "nl", "bat", "wc",
];
const SEARCHERS: &[&str] = &["grep", "egrep", "fgrep", "rg", "ag", "find", "fd", "ack"];
/// Programs that only set the scene for the one after them.
const SCENERY: &[&str] = &["cd", "echo", "export", "set", "true", "test", "pwd"];
/// The longest a command is quoted.
const COMMAND_CHARS: usize = 60;
/// The most commands a sentence names before it only counts them.
const NAMED: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verb {
    Read,
    Search,
    Other,
}

/// A file or directory a call names, by its own name.
#[derive(Debug)]
struct Subject {
    name: String,
    file: bool,
}

/// One call, as far as its arguments say what it did.
#[derive(Debug)]
pub(super) struct Doing {
    /// The call's hash, to tell how many different calls there were.
    key: u64,
    tool: String,
    verb: Verb,
    /// A shell command's text, for a tool that runs one.
    command: Option<String>,
    subjects: Vec<Subject>,
}

impl Doing {
    pub(super) fn of(key: u64, tool: &str, args: &Value) -> Self {
        let (verb, command, subjects) = match tool {
            "run_code" | "job_start" => {
                let code = args["code"].as_str().unwrap_or_default();
                let (verb, subjects) = parse(code);
                (verb, Some(shown(code)), subjects)
            }
            "read" => {
                let path = args["path"].as_str().unwrap_or_default();
                let name = path.trim_end_matches('/').rsplit('/').next();
                let subjects = name
                    .filter(|name| !name.is_empty())
                    .map(|name| Subject {
                        name: name.to_owned(),
                        file: true,
                    })
                    .into_iter()
                    .collect();
                (Verb::Read, None, subjects)
            }
            _ => (Verb::Other, None, Vec::new()),
        };
        Self {
            key,
            tool: tool.to_owned(),
            verb,
            command,
            subjects,
        }
    }
}

/// What a worker kept doing, to follow "I kept ": a clause that names the
/// file, the command or the tool.
pub(super) fn gist(doings: &[&Doing]) -> String {
    let calls: HashSet<u64> = doings.iter().map(|doing| doing.key).collect();
    let subject = most_common_subject(doings);
    if doings.iter().all(|doing| doing.verb == Verb::Read) {
        let part = if calls.len() > 1 { "parts" } else { "part" };
        return match subject {
            Some(name) => format!("re-reading the same {part} of {name}"),
            None => "re-reading the same files".to_owned(),
        };
    }
    if doings.iter().all(|doing| doing.verb != Verb::Other) {
        let what = if doings.iter().all(|doing| doing.verb == Verb::Search) {
            "repeating the same searches"
        } else {
            "searching and re-reading the same code"
        };
        return match subject {
            Some(name) => format!("{what} in {name}"),
            None => what.to_owned(),
        };
    }
    let mut commands: Vec<&str> = Vec::new();
    for command in doings.iter().filter_map(|doing| doing.command.as_deref()) {
        if !commands.contains(&command) {
            commands.push(command);
        }
    }
    match commands.as_slice() {
        [] => {
            let mut tools: Vec<&str> = Vec::new();
            for doing in doings {
                if !tools.contains(&doing.tool.as_str()) {
                    tools.push(&doing.tool);
                }
            }
            format!("calling {} with the same arguments", join(&tools))
        }
        many if many.len() > NAMED => format!("going round the same {} commands", many.len()),
        some => {
            let quoted: Vec<String> = some.iter().map(|command| format!("`{command}`")).collect();
            format!("re-running {}", join(&quoted))
        }
    }
}

/// `a`, `a and b`, `a, b and c`.
fn join<S: AsRef<str>>(items: &[S]) -> String {
    match items {
        [] => String::new(),
        [only] => only.as_ref().to_owned(),
        [first @ .., last] => {
            let first: Vec<&str> = first.iter().map(AsRef::as_ref).collect();
            format!("{} and {}", first.join(", "), last.as_ref())
        }
    }
}

/// The name that most calls share, a file before a directory, the first of
/// them on a tie.
fn most_common_subject(doings: &[&Doing]) -> Option<String> {
    let mut counts: Vec<(&Subject, usize)> = Vec::new();
    for doing in doings {
        let mut once = HashSet::new();
        for subject in &doing.subjects {
            if !once.insert(subject.name.as_str()) {
                continue;
            }
            match counts
                .iter_mut()
                .find(|(seen, _)| seen.name == subject.name)
            {
                Some((_, times)) => *times += 1,
                None => counts.push((subject, 1)),
            }
        }
    }
    let commonest = |file: bool| {
        counts
            .iter()
            .rev()
            .filter(|(subject, _)| subject.file == file)
            .max_by_key(|(_, times)| *times)
            .map(|(subject, _)| subject.name.clone())
    };
    commonest(true).or_else(|| commonest(false))
}

/// A command on one line, without the `cd /work &&` most start with, cut.
fn shown(command: &str) -> String {
    let squashed = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let squashed = squashed.strip_prefix("cd /work && ").unwrap_or(&squashed);
    clip(squashed, COMMAND_CHARS).0
}

/// What a shell command does, and the files it names. It splits on `;`, `&&`,
/// `||`, `|` and newlines without reading quotes, which is enough to name a
/// file in a stuck loop.
fn parse(command: &str) -> (Verb, Vec<Subject>) {
    let mut verbs = Vec::new();
    let mut subjects = Vec::new();
    let flat = command.replace("&&", ";").replace("||", ";");
    for part in flat.split([';', '|', '\n']) {
        let mut words = part.split_whitespace().skip_while(|word| assignment(word));
        let Some(program) = words.next() else {
            continue;
        };
        let program = program.rsplit('/').next().unwrap_or(program);
        if SCENERY.contains(&program) {
            continue;
        }
        verbs.push(if READERS.contains(&program) {
            Verb::Read
        } else if SEARCHERS.contains(&program) {
            Verb::Search
        } else {
            Verb::Other
        });
        subjects.extend(words.filter_map(subject));
    }
    let verb = if verbs.is_empty() || verbs.contains(&Verb::Other) {
        Verb::Other
    } else if verbs.contains(&Verb::Search) {
        Verb::Search
    } else {
        Verb::Read
    };
    (verb, subjects)
}

/// `NAME=value`, which a shell takes before the program.
fn assignment(word: &str) -> bool {
    matches!(word.split_once('='), Some((name, _))
        if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// The file or directory a word of a command names. A file has an extension;
/// a directory starts at the root or the current directory; anything with
/// other characters (a sed script, a regex, a redirect) names nothing.
fn subject(word: &str) -> Option<Subject> {
    let word = word.trim_matches(['\'', '"', '`']);
    if word.is_empty() || word.starts_with('-') {
        return None;
    }
    if !word
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._/-~@+".contains(c))
    {
        return None;
    }
    let name = word.trim_end_matches('/').rsplit('/').next()?;
    let file = name.rsplit_once('.').is_some_and(|(stem, extension)| {
        !stem.is_empty()
            && (1..=5).contains(&extension.len())
            && extension.chars().all(|c| c.is_ascii_alphanumeric())
            && extension.chars().any(|c| c.is_ascii_alphabetic())
    });
    let directory = ["/", "./", "../"]
        .iter()
        .any(|start| word.starts_with(start))
        && !["", ".", "..", "work", "host"].contains(&name);
    (file || directory).then(|| Subject {
        name: name.to_owned(),
        file,
    })
}

#[cfg(test)]
mod tests {
    use super::{Doing, gist};
    use serde_json::json;

    fn said(calls: &[(&str, serde_json::Value)]) -> String {
        let doings: Vec<Doing> = calls
            .iter()
            .enumerate()
            .map(|(key, (tool, args))| Doing::of(key as u64, tool, args))
            .collect();
        gist(&doings.iter().collect::<Vec<_>>())
    }

    fn code(command: &str) -> (&'static str, serde_json::Value) {
        ("run_code", json!({"code": command}))
    }

    #[test]
    fn slices_of_one_file_are_re_reading_that_file() {
        let file = "packages/solana/src/liquidity/meteora-dlmm-allocate-encode.js";
        let slices: Vec<_> = [(60, 95), (95, 125), (125, 150)]
            .iter()
            .map(|(from, to)| {
                code(&format!(
                    "cd /work && sed -n 1,60p {file} | tail -1; sed -n {from},{to}p {file}"
                ))
            })
            .collect();
        assert_eq!(
            said(&slices),
            "re-reading the same parts of meteora-dlmm-allocate-encode.js"
        );
        assert_eq!(
            said(&slices[..1]),
            "re-reading the same part of meteora-dlmm-allocate-encode.js"
        );
        assert_eq!(
            said(&[("read", json!({"path": "/work/src/main.rs"}))]),
            "re-reading the same part of main.rs"
        );
        assert_eq!(
            said(&[code("cat /work/Makefile"), code("head -5 /work/src/lib.rs")]),
            "re-reading the same parts of lib.rs",
            "a file is named before a path that may be a directory"
        );
    }

    #[test]
    fn searches_and_other_commands_are_named_by_what_they_run() {
        assert_eq!(
            said(&[
                code("cd /work && rg -n 'bps' src/agent/broker.rs | head"),
                code("grep -rn slippage src/agent/broker.rs"),
            ]),
            "repeating the same searches in broker.rs"
        );
        assert_eq!(
            said(&[code("rg -n bps src/a.rs"), code("sed -n 1,9p src/a.rs"),]),
            "searching and re-reading the same code in a.rs"
        );
        assert_eq!(
            said(&[code("cd /work && cargo test --lib"), code("git diff")]),
            "re-running `cargo test --lib` and `git diff`"
        );
        assert_eq!(
            said(&[code("cargo test  \n --lib"), code("cargo test --lib")]),
            "re-running `cargo test --lib`",
            "whitespace is squashed and a repeat is one command"
        );
        let many: Vec<_> = ["a", "b", "c", "d"].iter().map(|c| code(c)).collect();
        assert_eq!(said(&many), "going round the same 4 commands");
        let long = code(&"x".repeat(200));
        assert!(said(&[long]).len() < 80);
    }

    #[test]
    fn a_tool_that_runs_no_command_is_named_by_the_tool() {
        let fetch = ("fetch_url", json!({"url": "https://example.com/"}));
        assert_eq!(
            said(std::slice::from_ref(&fetch)),
            "calling fetch_url with the same arguments"
        );
        let other = ("browser_open", json!({"url": "https://example.com/"}));
        assert_eq!(
            said(&[fetch, other]),
            "calling fetch_url and browser_open with the same arguments"
        );
    }

    #[test]
    fn what_looks_like_a_file_is_a_file_and_a_sed_script_is_not() {
        assert_eq!(
            said(&[
                code("sed -i 's/foo/bar/' notes.md"),
                code("sed -n 1,5p notes.md")
            ]),
            "re-reading the same parts of notes.md"
        );
        assert_eq!(
            said(&[code("cat 1.5")]),
            "re-reading the same files",
            "a version is no file"
        );
    }
}
