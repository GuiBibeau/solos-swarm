use serde::{Deserialize, Serialize};

/// `owner/name`, as the forge names a repository. Each side is ASCII
/// letters, digits, `-`, `_` and `.`, and neither is `.` or `..`, so a name
/// can reach no other API path and no other directory.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepoName(String);

impl RepoName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn owner(&self) -> &str {
        self.parts().0
    }
    pub fn name(&self) -> &str {
        self.parts().1
    }
    fn parts(&self) -> (&str, &str) {
        self.0.split_once('/').expect("a RepoName holds one slash")
    }
}

impl TryFrom<String> for RepoName {
    type Error = String;
    fn try_from(value: String) -> Result<Self, String> {
        let part = |part: &str| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        };
        if value
            .split_once('/')
            .is_some_and(|(owner, name)| part(owner) && part(name))
        {
            Ok(Self(value))
        } else {
            Err(format!(
                "{value:?} is not a repository: expected owner/name of ASCII letters, digits, \
                 '-', '_' and '.'"
            ))
        }
    }
}

impl From<RepoName> for String {
    fn from(value: RepoName) -> String {
        value.0
    }
}

impl std::fmt::Display for RepoName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a Ticket's finished work owes, lowest first: each owes what the one
/// below it does, and more.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Deliverable {
    /// An answer in the report.
    #[default]
    Answer,
    /// A change committed in the Ticket's clone.
    Change,
    /// A change in a pull request on the forge.
    PullRequest,
}

impl Deliverable {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Answer => "answer",
            Self::Change => "change",
            Self::PullRequest => "pull_request",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Deliverable, RepoName};

    #[test]
    fn a_repo_name_is_one_owner_and_one_name() {
        for good in [
            "acme/widgets",
            "GuiBibeau/solos-swarm",
            "a.b/c_d-e",
            "o/.github",
        ] {
            let name = RepoName::try_from(good.to_owned()).unwrap();
            assert_eq!(format!("{}/{}", name.owner(), name.name()), good);
        }
        for bad in [
            "", "acme", "acme/", "/widgets", "a/b/c", "a/../b", "../b", "a/..", "./b", "a/b c",
            "a/b%2f", "a\\b",
        ] {
            assert!(RepoName::try_from(bad.to_owned()).is_err(), "{bad}");
        }
        let parsed: Result<RepoName, _> = serde_json::from_str(r#""a/../etc""#);
        assert!(parsed.is_err());
    }

    #[test]
    fn deliverables_rise_from_an_answer_to_a_pull_request_and_read_in_snake_case() {
        assert!(Deliverable::Answer < Deliverable::Change);
        assert!(Deliverable::Change < Deliverable::PullRequest);
        assert_eq!(Deliverable::default(), Deliverable::Answer);
        for owed in [
            Deliverable::Answer,
            Deliverable::Change,
            Deliverable::PullRequest,
        ] {
            let written = serde_json::to_string(&owed).unwrap();
            assert_eq!(written, format!("\"{}\"", owed.as_str()));
            assert_eq!(serde_json::from_str::<Deliverable>(&written).unwrap(), owed);
        }
    }
}
