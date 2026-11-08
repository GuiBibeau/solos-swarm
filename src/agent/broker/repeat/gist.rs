//! What a worker was doing when the guard stopped it, in a few words the
//! person can read, and the one question that would get it going again:
//! "I kept re-reading the same parts of meteora-dlmm-allocate-encode.js without
//! making progress, so I stopped. What should I be looking for in
//! meteora-dlmm-allocate-encode.js?"
//!
//! The guard cannot know what a call meant, only what its arguments say, so
//! this reads a shell command for the programs it runs and the files it names.
//! It never quotes the command: the text goes to a public thread, and a command
//! can hold a heredoc of source, a token or a path the person never wrote. It
//! names programs and files, and falls back to the tool's name.

use serde_json::Value;
use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Programs that show part of a file, and programs that search.
const READERS: &[&str] = &[
    "sed", "cat", "head", "tail", "less", "more", "nl", "bat", "wc",
];
const SEARCHERS: &[&str] = &["grep", "egrep", "fgrep", "rg", "ag", "find", "fd", "ack"];
/// Programs that write the file they name, wherever their output goes.
const WRITERS: &[&str] = &["tee", "patch"];
/// Programs that only set the scene for the one after them.
const SCENERY: &[&str] = &["cd", "echo", "export", "set", "true", "test", "pwd"];
/// Programs whose second word says what they do: `cargo test`, `git diff`.
const SUBCOMMANDS: &[&str] = &[
    "cargo", "git", "npm", "pnpm", "yarn", "bun", "go", "make", "uv", "pip", "docker", "gh", "just",
];
/// The longest a program's name or a file's name is, to be quoted.
const NAME_CHARS: usize = 32;
/// The most programs a sentence names before it only counts the calls.
const NAMED: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verb {
    Read,
    Search,
    Write,
    Other,
}

/// A file or directory a call names, by its own name.
#[derive(Clone, Debug)]
struct Subject {
    name: String,
    file: bool,
}

/// One call, as far as its arguments say what it did.
#[derive(Clone, Debug)]
pub(super) struct Doing {
    /// The call's hash, to tell how many different calls there were.
    key: u64,
    /// The hash of what the call is about: for a command, the programs it runs
    /// and the files it names, whatever else is in it, so a command sent again
    /// with other whitespace, quotes or a heredoc body is the same one.
    shape: u64,
    tool: String,
    verb: Verb,
    /// The programs a shell command runs, like `cargo test`, without
    /// their arguments.
    programs: Vec<String>,
    subjects: Vec<Subject>,
}

impl Doing {
    pub(super) fn of(key: u64, tool: &str, args: &Value) -> Self {
        let (verb, programs, subjects, shape) = match tool {
            "run_code" | "job_start" => {
                let parsed = parse(args["code"].as_str().unwrap_or_default());
                let shape = (!parsed.words.is_empty()).then(|| {
                    let mut hasher = DefaultHasher::new();
                    (tool, &parsed.words).hash(&mut hasher);
                    for subject in &parsed.subjects {
                        subject.name.hash(&mut hasher);
                    }
                    hasher.finish()
                });
                (parsed.verb, parsed.programs, parsed.subjects, shape)
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
                (Verb::Read, Vec::new(), subjects, None)
            }
            _ => (Verb::Other, Vec::new(), Vec::new(), None),
        };
        Self {
            key,
            shape: shape.unwrap_or(key),
            tool: tool.to_owned(),
            verb,
            programs,
            subjects,
        }
    }

    pub(super) fn shape(&self) -> u64 {
        self.shape
    }

    /// What it was to try this, to follow "to ": `change lib.sh`.
    fn attempt(&self) -> String {
        let subject = self.subjects.first().map(|subject| subject.name.as_str());
        match (self.verb, subject) {
            (Verb::Write, subject) => format!("change {}", subject.unwrap_or("a file")),
            (Verb::Read, subject) => format!("read {}", subject.unwrap_or("a file")),
            (Verb::Search, subject) => format!("search {}", subject.unwrap_or("the code")),
            (Verb::Other, _) if self.programs.is_empty() => format!("call {}", self.tool),
            (Verb::Other, subject) => {
                let on = subject.map_or_else(String::new, |name| format!(" on {name}"));
                format!("run {}{on}", quoted(&self.programs))
            }
        }
    }
}

/// What the host tells the person when a worker's calls failed the same way
/// `times` times.
pub(super) fn failed(doing: &Doing, times: usize) -> String {
    let subject = doing.subjects.first().map(|subject| subject.name.as_str());
    format!(
        "I tried {times} times to {}, and it failed the same way each time, so I stopped. {}",
        doing.attempt(),
        other_way(subject)
    )
}

/// What the host tells the person when a worker went round the same calls
/// without finding anything new.
pub(super) fn stuck(doings: &[&Doing]) -> String {
    let subject = most_common_subject(doings);
    let looking = doings
        .iter()
        .all(|doing| matches!(doing.verb, Verb::Read | Verb::Search));
    let question = match (&subject, looking) {
        (Some(name), true) => format!("What should I be looking for in {name}?"),
        (None, true) => "What should I be looking for?".to_owned(),
        _ => other_way(subject.as_deref()),
    };
    format!(
        "I kept {} without making progress, so I stopped. {question}",
        gist(doings)
    )
}

/// The one question to ask when the way the worker went did not work.
fn other_way(subject: Option<&str>) -> String {
    format!(
        "Is there another way you want me to handle {}, or should I stop here?",
        subject.unwrap_or("this")
    )
}

/// What a worker kept doing, to follow "I kept ": a clause that names the
/// file, the programs or the tool.
fn gist(doings: &[&Doing]) -> String {
    let calls: HashSet<u64> = doings.iter().map(|doing| doing.key).collect();
    let subject = most_common_subject(doings);
    if doings.iter().all(|doing| doing.verb == Verb::Read) {
        let part = if calls.len() > 1 { "parts" } else { "part" };
        return match subject {
            Some(name) => format!("re-reading the same {part} of {name}"),
            None => "re-reading the same files".to_owned(),
        };
    }
    if doings.iter().all(|doing| doing.verb == Verb::Write) {
        return match subject {
            Some(name) => format!("rewriting {name}"),
            None => "rewriting the same files".to_owned(),
        };
    }
    if doings.iter().all(|doing| doing.verb != Verb::Other) {
        let what = if doings.iter().all(|doing| doing.verb == Verb::Search) {
            "repeating the same searches"
        } else if doings.iter().any(|doing| doing.verb == Verb::Write) {
            "editing and re-reading the same code"
        } else {
            "searching and re-reading the same code"
        };
        return match subject {
            Some(name) => format!("{what} in {name}"),
            None => what.to_owned(),
        };
    }
    let mut programs: Vec<String> = Vec::new();
    for program in doings.iter().flat_map(|doing| &doing.programs) {
        if !programs.contains(program) {
            programs.push(program.clone());
        }
    }
    match programs.as_slice() {
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
            let on = subject.map_or_else(String::new, |name| format!(" on {name}"));
            format!("re-running {}{on}", quoted(some))
        }
    }
}

/// `` `a` ``, `` `a` and `b` ``, `` `a`, `b` and `c` ``.
fn quoted<S: AsRef<str>>(items: &[S]) -> String {
    let items: Vec<String> = items
        .iter()
        .map(|item| format!("`{}`", item.as_ref()))
        .collect();
    join(&items)
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

/// What a shell command does.
struct Parsed {
    verb: Verb,
    /// The programs it runs, without the ones that only set the scene.
    programs: Vec<String>,
    /// The program of every step of it, scene-setters too.
    words: Vec<String>,
    subjects: Vec<Subject>,
}

/// What a shell command does, and the files it names. It splits on `;`, `&&`,
/// `||`, `|` and newlines without reading quotes, which is enough to name a
/// file in a stuck loop. A heredoc's body is not read as commands: it is a
/// script's source or a file's text, and names a file only when the command
/// line itself names none, as `python3 - <<'PY'` does.
fn parse(command: &str) -> Parsed {
    let (script, bodies) = split_heredocs(command);
    let mut verbs = Vec::new();
    let mut parsed = Parsed {
        verb: Verb::Other,
        programs: Vec::new(),
        words: Vec::new(),
        subjects: Vec::new(),
    };
    let flat = script.replace("&&", ";").replace("||", ";");
    for part in flat.split([';', '|', '\n']) {
        let words: Vec<&str> = part
            .split_whitespace()
            .skip_while(|word| assignment(word))
            .collect();
        let Some((program, args)) = words.split_first() else {
            continue;
        };
        let Some(label) = label(program, args.first().copied()) else {
            continue;
        };
        parsed.words.push(label.clone());
        let name = program.rsplit('/').next().unwrap_or(program);
        if SCENERY.contains(&name) {
            continue;
        }
        if !parsed.programs.contains(&label) {
            parsed.programs.push(label);
        }
        let subjects: Vec<Subject> = args.iter().filter_map(|word| subject(word)).collect();
        let writes = WRITERS.contains(&name)
            || (name == "sed"
                && args
                    .iter()
                    .any(|a| a.starts_with("-i") || *a == "--in-place"))
            || redirects_to_a_file(args);
        verbs.push(if writes {
            Verb::Write
        } else if READERS.contains(&name) {
            Verb::Read
        } else if SEARCHERS.contains(&name) {
            Verb::Search
        } else {
            Verb::Other
        });
        parsed.subjects.extend(subjects);
    }
    if parsed.subjects.is_empty() {
        parsed
            .subjects
            .extend(bodies.iter().flat_map(|line| quoted_files(line)));
    }
    if parsed.programs.is_empty() {
        // A command that only sets scenes (`echo`, `cd`) is still named.
        for word in &parsed.words {
            if !parsed.programs.contains(word) {
                parsed.programs.push(word.clone());
            }
        }
    }
    parsed.verb = if verbs.is_empty() || verbs.contains(&Verb::Other) {
        Verb::Other
    } else if verbs.contains(&Verb::Write) {
        Verb::Write
    } else if verbs.contains(&Verb::Search) {
        Verb::Search
    } else {
        Verb::Read
    };
    parsed
}

/// A program's name for a person, with its subcommand where it has one. A
/// word that is no plain name (`print("x")`, `$(foo)`) is no program.
fn label(program: &str, next: Option<&str>) -> Option<String> {
    let name = program.rsplit('/').next()?;
    let plain = |word: &str, more: &str| {
        word.chars()
            .all(|c| c.is_ascii_alphanumeric() || more.contains(c))
    };
    let named = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric());
    if !named || name.chars().count() > NAME_CHARS || !plain(name, "._+-") {
        return None;
    }
    let sub = next.filter(|word| {
        SUBCOMMANDS.contains(&name)
            && !word.starts_with('-')
            && word.len() <= 16
            && word.chars().all(|c| c.is_ascii_lowercase() || c == '-')
    });
    Some(match sub {
        Some(sub) => format!("{name} {sub}"),
        None => name.to_owned(),
    })
}

/// Whether the words redirect output into a file (`> out.txt`, `>>out.txt`),
/// not into `/dev/null` or another descriptor (`2>&1`).
fn redirects_to_a_file(args: &[&str]) -> bool {
    args.iter().enumerate().any(|(at, word)| {
        let target = match *word {
            ">" | ">>" => args.get(at + 1).copied(),
            _ => word
                .strip_prefix(">>")
                .or_else(|| word.strip_prefix('>'))
                .filter(|target| !target.starts_with('&')),
        };
        target.and_then(subject).is_some_and(|subject| subject.file)
    })
}

/// The script's lines apart from the bodies of its heredocs, and those bodies'
/// lines. A body ends at its delimiter line, which `<<-` may indent with tabs.
fn split_heredocs(command: &str) -> (String, Vec<&str>) {
    let mut script = String::new();
    let mut bodies = Vec::new();
    let mut ends: Vec<(String, bool)> = Vec::new();
    for line in command.lines() {
        if let Some((end, indented)) = ends.first() {
            let line_end = if *indented {
                line.trim_start_matches('\t')
            } else {
                line
            };
            if line_end.trim_end() == end {
                ends.remove(0);
            } else {
                bodies.push(line);
            }
            continue;
        }
        script.push_str(line);
        script.push('\n');
        ends.extend(heredoc_ends(line));
    }
    (script, bodies)
}

/// The delimiters of the heredocs a line opens, and whether each is `<<-`.
fn heredoc_ends(line: &str) -> Vec<(String, bool)> {
    let mut ends = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("<<") {
        rest = &rest[at + 2..];
        if let Some(after) = rest.strip_prefix('<') {
            rest = after;
            continue;
        }
        let (indented, after) = rest
            .strip_prefix('-')
            .map_or((false, rest), |after| (true, after));
        let word: String = after
            .trim_start()
            .trim_start_matches(['\'', '"', '\\'])
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !word.is_empty() {
            ends.push((word, indented));
        }
    }
    ends
}

/// The files that quoted strings on a line name: `p='bin/lib.sh'`.
fn quoted_files(line: &str) -> Vec<Subject> {
    ['\'', '"']
        .iter()
        .flat_map(|quote| line.split(*quote).skip(1).step_by(2))
        .filter(|literal| literal.len() <= 200)
        .filter_map(subject)
        .filter(|subject| subject.file)
        .collect()
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
    if name.chars().count() > NAME_CHARS {
        return None;
    }
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
    use super::{Doing, failed, gist, stuck};
    use serde_json::json;

    fn doings(calls: &[(&str, serde_json::Value)]) -> Vec<Doing> {
        calls
            .iter()
            .enumerate()
            .map(|(key, (tool, args))| Doing::of(key as u64, tool, args))
            .collect()
    }

    fn said(calls: &[(&str, serde_json::Value)]) -> String {
        gist(&doings(calls).iter().collect::<Vec<_>>())
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
    fn searches_and_other_commands_are_named_by_the_programs_they_run() {
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
            "re-running `cargo test` and `git diff`",
            "a program's arguments are not quoted"
        );
        assert_eq!(
            said(&[code("cargo test  \\\n --lib"), code("cargo test --lib")]),
            "re-running `cargo test`",
            "a repeat is one program"
        );
        assert_eq!(
            said(&[code("python3 build.py --token abc123")]),
            "re-running `python3` on build.py"
        );
        let many: Vec<_> = ["a", "b", "c", "d"].iter().map(|c| code(c)).collect();
        assert_eq!(said(&many), "going round the same 4 commands");
        let compound = code("cargo test; git diff; npm run; yarn add");
        assert_eq!(
            said(&[compound.clone(), compound]),
            "going round the same 4 commands",
            "one repeated call with four programs still names four commands"
        );
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
            said(&[code("sed -n 1,5p notes.md"), code("sed -n 6,9p notes.md")]),
            "re-reading the same parts of notes.md"
        );
        assert_eq!(
            said(&[code("cat 1.5")]),
            "re-reading the same files",
            "a version is no file"
        );
    }

    /// The edit that went wrong in #94: a Python script on a heredoc that names
    /// its file on its first line, and a file rewritten with `cat`.
    const EDIT: &str = "cd /work/.harness && python3 - <<'PY'\n\
        p='bin/lib.sh'\n\
        s=open(p).read()\n\
        old = '''if [ -z \"$TOKEN\" ]; then\n  echo \"no token: ghp_SECRET\"\nfi'''\n\
        assert old in s\n\
        open(p,'w').write(s.replace(old, 'true'))\n\
        PY";
    const REWRITE: &str = "cat > bin/lib.sh <<'EOF'\n\
        #!/usr/bin/env bash\n\
        echo \"password=hunter2\" > other.txt\n\
        EOF";

    #[test]
    fn an_edit_on_a_heredoc_is_named_by_its_program_and_file_and_never_quoted() {
        let edits = [code(EDIT), code(REWRITE)];
        for call in &edits {
            let one = doings(std::slice::from_ref(call));
            let text = failed(&one[0], 3);
            for secret in ["ghp_SECRET", "hunter2", "<<", "open(p", "other.txt"] {
                assert!(!text.contains(secret), "{secret:?} is in {text}");
            }
            assert!(text.contains("lib.sh"), "the file is named: {text}");
        }
        assert_eq!(
            failed(&doings(&[code(EDIT)])[0], 3),
            "I tried 3 times to run `python3` on lib.sh, and it failed the same way each \
             time, so I stopped. Is there another way you want me to handle lib.sh, or \
             should I stop here?"
        );
        assert_eq!(
            failed(&doings(&[code(REWRITE)])[0], 3),
            "I tried 3 times to change lib.sh, and it failed the same way each time, so I \
             stopped. Is there another way you want me to handle lib.sh, or should I stop \
             here?"
        );
        assert_eq!(
            said(&[code(EDIT), code(REWRITE), code(EDIT)]),
            "re-running `python3` and `cat` on lib.sh",
            "a loop of both names both programs"
        );
        assert_eq!(said(&[code(REWRITE)]), "rewriting lib.sh");
    }

    #[test]
    fn calls_that_differ_only_in_whitespace_quotes_or_a_body_have_one_shape() {
        let shape = |command: &str| doings(&[code(command)])[0].shape();
        let edit = shape(EDIT);
        assert_eq!(
            shape(&EDIT.replace("p='bin/lib.sh'", "p = \"bin/lib.sh\"")),
            edit,
            "quotes and spaces"
        );
        assert_eq!(
            shape(&EDIT.replace("'true'", "'false'  ")),
            edit,
            "a body that changed"
        );
        assert_eq!(shape(&format!("  {EDIT}\n\n")), edit, "blank lines");
        assert_ne!(shape(&EDIT.replace("bin/lib.sh", "bin/other.sh")), edit);
        assert_ne!(shape(&EDIT.replace("python3", "python")), edit);
        assert_ne!(shape(REWRITE), edit);
        assert_ne!(shape("cd /nonexistent"), shape("test -f done.flag"));
    }

    #[test]
    fn a_stuck_sentence_asks_one_question() {
        let slices: Vec<_> = [(1, 5), (6, 9)]
            .iter()
            .map(|(from, to)| code(&format!("sed -n {from},{to}p src/encode.js")))
            .collect();
        let all = doings(&slices);
        assert_eq!(
            stuck(&all.iter().collect::<Vec<_>>()),
            "I kept re-reading the same parts of encode.js without making progress, so I \
             stopped. What should I be looking for in encode.js?"
        );
        let all = doings(&[code("cargo test"), code("git diff")]);
        assert_eq!(
            stuck(&all.iter().collect::<Vec<_>>()),
            "I kept re-running `cargo test` and `git diff` without making progress, so I \
             stopped. Is there another way you want me to handle this, or should I stop here?"
        );
        for text in [stuck(&all.iter().collect::<Vec<_>>()), failed(&all[0], 3)] {
            assert_eq!(text.matches('?').count(), 1, "{text}");
            assert!(text.ends_with('?') && text.len() < 500, "{text}");
        }
    }
}
