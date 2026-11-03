use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const ALLOWED: [&str; 2] = ["src/vault.rs", "src/vault/"];

const ACCESSORS: [&str; 3] = ["reveal", "reveal_into", "reveal_password"];

const CALLERS: [(&str, &str); 1] = [("src/browse/session.rs", "reveal_password")];

/// Each function outside the vault's tests that reads a value straight
/// from `SecretValue::reveal`, by file.
const READERS: [(&str, &str); 5] = [
    ("src/vault.rs", "check"),
    ("src/vault.rs", "reveal_into"),
    ("src/vault.rs", "reveal_password"),
    ("src/vault/file.rs", "write_value"),
    ("src/vault/scrub.rs", "new"),
];

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

fn words(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
}

#[test]
fn every_accessor_that_yields_a_value_is_named_only_in_allowed_modules() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    let mut defined = BTreeSet::new();
    let mut readers = BTreeSet::new();
    let mut outside = Vec::new();
    let mut public = Vec::new();
    let mut callers = BTreeSet::new();
    for path in files {
        let file = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let allowed = ALLOWED
            .iter()
            .any(|place| file == *place || (place.ends_with('/') && file.starts_with(place)));
        let mut function = String::new();
        for (number, line) in fs::read_to_string(&path).unwrap().lines().enumerate() {
            let at = format!("{file}:{}", number + 1);
            let mut previous = "";
            for word in words(line) {
                if previous == "fn" {
                    function = word.to_owned();
                }
                previous = word;
            }
            for word in words(line).filter(|word| word.starts_with("reveal")) {
                if CALLERS.contains(&(file.as_str(), word)) && !line.contains(&format!("fn {word}"))
                {
                    callers.insert((file.clone(), word.to_owned()));
                } else if !allowed {
                    outside.push(format!("{at}: {word}"));
                } else if line.contains(&format!("fn {word}")) {
                    defined.insert(word.to_owned());
                    if line.contains("pub fn") {
                        public.push(format!("{at}: {word}"));
                    }
                }
            }
            if allowed && file != "src/vault/tests.rs" && line.contains(".reveal()") {
                readers.insert((file.clone(), function.clone()));
            }
        }
    }
    assert!(
        outside.is_empty(),
        "a value accessor is named outside {ALLOWED:?} and {CALLERS:?}:\n{}",
        outside.join("\n")
    );
    assert_eq!(
        callers,
        CALLERS
            .iter()
            .map(|(file, accessor)| ((*file).to_owned(), (*accessor).to_owned()))
            .collect::<BTreeSet<_>>(),
        "an accessor's callers outside the vault changed; list them in CALLERS"
    );
    assert!(
        public.is_empty(),
        "a value accessor is pub:\n{}",
        public.join("\n")
    );
    assert_eq!(
        defined,
        BTreeSet::from(ACCESSORS.map(str::to_owned)),
        "the vault's value accessors changed; list them in ACCESSORS"
    );
    assert_eq!(
        readers,
        READERS
            .iter()
            .map(|(file, function)| ((*file).to_owned(), (*function).to_owned()))
            .collect::<BTreeSet<_>>(),
        "the functions that read a value changed; list them in READERS"
    );
}
