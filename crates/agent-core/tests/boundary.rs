const ALLOWED: &[&str] = &["serde", "serde_json", "sha2"];

#[test]
fn core_depends_only_on_pure_crates() {
    let manifest = include_str!("../Cargo.toml");
    let mut section = "";
    let mut found = Vec::new();
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line;
            continue;
        }
        let in_deps = section.ends_with("dependencies]");
        if in_deps && !line.is_empty() && !line.starts_with('#') {
            let name = line.split(['=', ' ', '.']).next().unwrap_or_default();
            found.push((section.to_owned(), name.to_owned()));
        }
    }
    for (section, name) in &found {
        assert!(
            section == "[dev-dependencies]" || ALLOWED.contains(&name.as_str()),
            "{name} in {section} is not a pure dependency; put it behind a port"
        );
    }
    assert!(
        found.iter().any(|(_, n)| n == "serde"),
        "parser saw the dependencies"
    );
}

/// The `std` modules a pure core may name. Anything else, such as `fs`,
/// `net`, `process`, `env`, `io`, `thread` or `time`, is I/O or a clock.
const PURE_STD: &[&str] = &[
    "borrow",
    "cell",
    "cmp",
    "collections",
    "convert",
    "default",
    "error",
    "fmt",
    "hash",
    "iter",
    "marker",
    "mem",
    "num",
    "ops",
    "option",
    "rc",
    "result",
    "slice",
    "str",
    "string",
    "vec",
];

/// The top-level module of every `std::` path, including each item of a
/// `use std::{...}` group.
fn std_modules(source: &str) -> Vec<String> {
    let ident = |s: &str| -> String {
        s.trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect()
    };
    let mut out = Vec::new();
    let mut rest = source;
    while let Some(i) = rest.find("std::") {
        rest = &rest[i + "std::".len()..];
        let Some(group) = rest.strip_prefix('{') else {
            out.push(ident(rest));
            continue;
        };
        let (mut depth, mut item) = (0, String::new());
        for c in group.chars() {
            match c {
                '{' => depth += 1,
                '}' if depth == 0 => break,
                '}' => depth -= 1,
                ',' if depth == 0 => out.push(ident(&std::mem::take(&mut item))),
                _ if depth == 0 => item.push(c),
                _ => {}
            }
        }
        out.push(ident(&item));
    }
    out.retain(|m| !m.is_empty());
    out
}

#[test]
fn core_sources_do_no_io() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path).unwrap();
        for module in std_modules(&source) {
            assert!(
                PURE_STD.contains(&module.as_str()),
                "{} uses std::{module}; put it behind a port",
                path.display()
            );
        }
        for alias in ["std as ", "extern crate", "SystemTime", "Instant"] {
            assert!(!source.contains(alias), "{} uses {alias}", path.display());
        }
        checked += 1;
    }
    assert!(checked >= 7, "read the core sources");
}

#[test]
fn the_std_parser_sees_groups_and_aliases() {
    let source = "use std::{env as _e, fs as _f, collections::{BTreeMap, BTreeSet}};\nfn f() { std::net::TcpStream::connect(\"x\"); }";
    assert_eq!(std_modules(source), ["env", "fs", "collections", "net"]);
}
