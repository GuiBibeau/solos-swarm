//! Which paths a repository protects, as globs: `**` is any number of
//! directories (and, last, everything under one), `*` any characters inside
//! one name, `?` one character. Matching ignores ASCII case, since a
//! case-folding checkout or a reviewer reads `.GitHub/` as `.github/`.

const MAX_GLOB_CHARS: usize = 200;

/// Whether `glob` is one this matcher reads as written: a repository-relative
/// pattern, with no `..` and no empty directory.
pub fn compiles(glob: &str) -> Result<(), String> {
    if glob.is_empty() || glob.chars().count() > MAX_GLOB_CHARS {
        return Err(format!("a glob is 1 to {MAX_GLOB_CHARS} characters"));
    }
    if glob.starts_with('/') || glob.contains('\\') || glob.chars().any(char::is_control) {
        return Err("a glob is relative to the repository, with / between directories".into());
    }
    if glob
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err("a glob has no empty, . or .. directory".into());
    }
    Ok(())
}

/// Whether any of `globs` matches `path`, a repository-relative path with `/`
/// between directories.
pub fn any(globs: &[String], path: &str) -> bool {
    let path: Vec<&str> = path.split('/').collect();
    globs
        .iter()
        .any(|glob| walk(&glob.split('/').collect::<Vec<_>>(), &path))
}

fn walk(glob: &[&str], path: &[&str]) -> bool {
    match glob.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|skip| walk(rest, &path[skip..])),
        Some((first, rest)) => path
            .split_first()
            .is_some_and(|(name, below)| name_matches(first, name) && walk(rest, below)),
    }
}

/// One name against one glob segment: `*` and `?`, ignoring ASCII case.
fn name_matches(glob: &str, name: &str) -> bool {
    let (glob, name): (Vec<char>, Vec<char>) = (
        glob.chars().map(|c| c.to_ascii_lowercase()).collect(),
        name.chars().map(|c| c.to_ascii_lowercase()).collect(),
    );
    let (mut g, mut n) = (0, 0);
    let (mut star, mut resume) = (None, 0);
    while n < name.len() {
        match glob.get(g) {
            Some('*') => {
                star = Some(g);
                resume = n;
                g += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                g += 1;
                n += 1;
            }
            _ => match star {
                Some(at) => {
                    g = at + 1;
                    resume += 1;
                    n = resume;
                }
                None => return false,
            },
        }
    }
    glob[g..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::{any, compiles};

    fn hit(glob: &str, path: &str) -> bool {
        any(&[glob.to_owned()], path)
    }

    #[test]
    fn a_glob_reads_directories_names_and_case_the_way_a_reviewer_does() {
        for (glob, path, matched) in [
            (".github/**", ".github/workflows/ci.yml", true),
            (".github/**", ".github/CODEOWNERS", true),
            (".github/**", ".GitHub/Workflows/CI.yml", true),
            (".github/**", ".github", true),
            (".github/**", "src/.github/ci.yml", false),
            (".github/**", ".githubx/ci.yml", false),
            (".github/**", "github/ci.yml", false),
            ("vendor/pstack/**", "vendor/pstack/skills/a.md", true),
            ("vendor/pstack/**", "vendor/other", false),
            ("**/secrets.env", "a/b/secrets.env", true),
            ("**/secrets.env", "secrets.env", true),
            ("**/secrets.env", "a/secrets.env.bak", false),
            ("*.lock", "Cargo.lock", true),
            ("*.lock", "src/Cargo.lock", false),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("docs/*/index.md", "docs/guide/index.md", true),
            ("docs/*/index.md", "docs/a/b/index.md", false),
        ] {
            assert_eq!(hit(glob, path), matched, "{glob} against {path}");
        }
    }

    #[test]
    fn a_glob_that_could_escape_the_repository_or_match_nothing_is_refused() {
        for good in [".github/**", "**/x", "a/b.txt", "*.lock"] {
            assert!(compiles(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "/etc/**",
            "../x",
            "a/../b",
            "a//b",
            "a\\b",
            "./a",
            "a\0b",
            &"x".repeat(201),
        ] {
            assert!(compiles(bad).is_err(), "{bad:?}");
        }
    }
}
