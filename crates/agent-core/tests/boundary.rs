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

#[test]
fn core_sources_do_no_io() {
    let sources = [
        include_str!("../src/engine.rs"),
        include_str!("../src/model.rs"),
        include_str!("../src/ports.rs"),
        include_str!("../src/policy.rs"),
        include_str!("../src/memory.rs"),
        include_str!("../src/ids.rs"),
        include_str!("../src/lib.rs"),
    ];
    for forbidden in [
        "std::fs",
        "std::net",
        "std::process",
        "std::thread",
        "std::env",
        "SystemTime",
    ] {
        for source in sources {
            assert!(!source.contains(forbidden), "core source uses {forbidden}");
        }
    }
}
