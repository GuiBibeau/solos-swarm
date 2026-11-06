use crate::model::{Ms, TicketId};
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

/// A commit id: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits. The host's
/// lease and the forge's calls take a sha, so a malformed one can never reach
/// git's argv or a URL.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CommitSha(String);

impl CommitSha {
    /// Twelve digits, for people.
    pub fn short(&self) -> &str {
        &self.0[..12]
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CommitSha {
    type Error = String;
    fn try_from(value: String) -> Result<Self, String> {
        let hex = value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if matches!(value.len(), 40 | 64) && hex {
            Ok(Self(value))
        } else {
            Err(format!(
                "{value:?} is not a commit id: expected 40 or 64 lowercase hex digits"
            ))
        }
    }
}

impl From<CommitSha> for String {
    fn from(value: CommitSha) -> String {
        value.0
    }
}

impl std::fmt::Display for CommitSha {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The longest slug a branch name carries.
const MAX_SLUG: usize = 40;

/// `solos/<ticket id>/<slug>`: the only kind of branch the agent ever writes.
/// The host names it; the model's only influence is the Ticket's title, which
/// passes through [`BranchName::slug`] (ASCII lowercase letters, digits and
/// `-`, at most 40 characters, never empty). The Ticket id is inside the name,
/// so ownership is derived: a branch a forge reports belongs to
/// [`ticket`](Self::ticket) or to nobody.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BranchName(String);

impl BranchName {
    pub const NAMESPACE: &'static str = "solos";

    pub fn for_ticket(ticket: &TicketId, title: &str) -> Self {
        let id: String = ticket
            .as_str()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let id = if id.is_empty() {
            "ticket".to_owned()
        } else {
            id
        };
        Self(format!("{}/{id}/{}", Self::NAMESPACE, Self::slug(title)))
    }

    /// The title's ASCII letters and digits, lowercased, in runs joined by
    /// one `-`, cut at 40 characters. `change` when nothing is left.
    pub fn slug(title: &str) -> String {
        let mut slug = String::new();
        for c in title.chars() {
            if c.is_ascii_alphanumeric() {
                slug.push(c.to_ascii_lowercase());
            } else if !slug.is_empty() && !slug.ends_with('-') {
                slug.push('-');
            }
        }
        slug.truncate(MAX_SLUG);
        let slug = slug.trim_end_matches('-');
        if slug.is_empty() {
            "change".to_owned()
        } else {
            slug.to_owned()
        }
    }

    /// Only the exact shape `for_ticket` makes. Used on branch names a forge
    /// reports, which are not trusted to be ours until they parse.
    pub fn parse(raw: &str) -> Option<Self> {
        let rest = raw.strip_prefix(Self::NAMESPACE)?.strip_prefix('/')?;
        let (ticket, slug) = rest.split_once('/')?;
        let ticket_ok = !ticket.is_empty()
            && ticket
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
        let slug_ok = !slug.is_empty()
            && slug.len() <= MAX_SLUG
            && !slug.starts_with('-')
            && !slug.ends_with('-')
            && !slug.contains("--")
            && slug
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-');
        (ticket_ok && slug_ok).then(|| Self(raw.to_owned()))
    }

    /// The id between the first two slashes.
    pub fn ticket(&self) -> TicketId {
        let id = self.0.split('/').nth(1).unwrap_or_default();
        TicketId::new(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for BranchName {
    type Error = String;
    fn try_from(value: String) -> Result<Self, String> {
        Self::parse(&value).ok_or_else(|| format!("{value:?} is not a solos branch"))
    }
}

impl From<BranchName> for String {
    fn from(value: BranchName) -> String {
        value.0
    }
}

impl std::fmt::Display for BranchName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PrRef {
    pub repo: RepoName,
    pub number: u64,
}

/// CI at one head, as the host last read it from the forge. A repository with
/// no CI says so in its terms, so `Pending { reported: 0 }` is never read as
/// "passed".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Checks {
    /// Something has not finished, or nothing has been reported yet.
    Pending {
        reported: u32,
    },
    Passed {
        checks: u32,
    },
    Failed {
        failed: Vec<FailedCheck>,
    },
}

/// One failing check. Every string was clipped by the adapter and is data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedCheck {
    pub name: String,
    pub url: String,
    pub summary: String,
}

/// What an accept grants about merging, fixed at accept like the budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMode {
    /// The agent never merges; people do, on GitHub.
    Off,
    /// A person's click or an approving review, at the current head.
    Ask,
    /// CI at the head passed and nobody outside the Policy steered the
    /// Ticket: nothing else is asked.
    Auto,
}

/// What an accept grants about the forge, fixed at accept like the budget: a
/// config edit never changes the authority of work in flight.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgeTerms {
    pub merge: MergeMode,
    /// Whether the repository has CI the merge must wait for.
    pub ci_expected: bool,
    /// Heads the host admits for one Ticket, repairs and review rounds
    /// included.
    pub max_pushes: u32,
    /// CI failures that may resume an ended Ticket.
    pub max_repairs: u32,
}

/// Why the forge would not take the work: a decided no, not a transport
/// error. A suspended or uninstalled App is not here: it is a wait that the
/// Effect queue repeats until the owner lifts it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum Withheld {
    /// Someone else pushed to the Ticket's branch. The host never overwrites
    /// it.
    BranchMoved { found: CommitSha },
    /// A person closed the pull request; the host never reopens it.
    PrClosed,
    /// The forge refused the push or the pull request, in words we keep
    /// (clipped).
    Forbidden { message: String },
    /// `github.writes: false`: nothing is pushed.
    ReadOnly,
}

impl Withheld {
    /// What a person is told, in a sentence that says what is left.
    pub fn tell(&self, title: &str, branch: &BranchName, head: &CommitSha) -> String {
        match self {
            Self::BranchMoved { found } => format!(
                "\"{title}\" is not published: someone else pushed {} to {branch}, and I never \
                 overwrite another person's commits. My commit {} stays in the ticket's clone. \
                 Look at the branch, and ask again for a new pull request if you want one.",
                found.short(),
                head.short()
            ),
            Self::PrClosed => format!(
                "\"{title}\": the pull request for {branch} was closed, so I left it closed and \
                 pushed nothing more. Ask again for a new one."
            ),
            Self::Forbidden { message } => format!(
                "\"{title}\" is not published: GitHub refused it ({message}). My commit {} stays \
                 in the ticket's clone.",
                head.short()
            ),
            Self::ReadOnly => format!(
                "\"{title}\" is not published: this agent is read-only (github.writes is false). \
                 My commit {} stays in the ticket's clone.",
                head.short()
            ),
        }
    }
}

/// What the forge holds, set only from the host's own read-back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivered {
    pub pr: PrRef,
    pub head: CommitSha,
    pub url: String,
    pub at_ms: Ms,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ChangeState {
    Open,
    Merged { commit: CommitSha, at_ms: Ms },
    Closed { at_ms: Ms },
}

/// One Ticket's one pull request: `Option<Change>` on the Ticket, because one
/// Ticket owes one deliverable. The Store's record is the only one; the
/// forge's branch and pull request are what the host made of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// Named by the host at the first admitted `ready`; never changes.
    pub branch: BranchName,
    /// The base branch the clone started from; the pull request's base.
    pub base: String,
    /// The latest head the host admitted from the worker.
    pub head: CommitSha,
    /// The worker's words for `head`, clipped. Kept here so the pull request
    /// and the Result say what was admitted, whatever later Reports say.
    pub text: String,
    /// What the forge holds. `None` until the first Publish converges.
    /// `delivered.head != head` means a delivery is owed: derived, never a
    /// second flag.
    pub delivered: Option<Delivered>,
    /// CI at `delivered.head`.
    pub checks: Checks,
    pub state: ChangeState,
    /// Heads admitted so far.
    pub pushes: u32,
    /// A decided no from the forge. Terminal for this Ticket: the agent never
    /// fights a forge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<Withheld>,
}

impl Change {
    /// A Change for the first admitted head. The branch is the host's.
    pub fn first(branch: BranchName, harvest: &Harvest, text: String) -> Self {
        Self {
            branch,
            base: harvest.base.clone(),
            head: harvest.head.clone(),
            text,
            delivered: None,
            checks: Checks::Pending { reported: 0 },
            state: ChangeState::Open,
            pushes: 1,
            held: None,
        }
    }

    /// A later head: the worker's words and the head replace the earlier ones.
    pub fn admit(&mut self, harvest: &Harvest, text: String) {
        self.head = harvest.head.clone();
        self.text = text;
        self.pushes += 1;
    }

    /// The forge does not hold `head` yet, and nothing has said it never will.
    pub fn delivery_owed(&self) -> bool {
        self.state == ChangeState::Open
            && self.held.is_none()
            && self.delivered.as_ref().map(|d| &d.head) != Some(&self.head)
    }
}

/// What the broker measured of the worker's tree and the forge adapter
/// admitted. Only the broker builds one, from a sandbox the host ran, never
/// from the model's tool arguments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Harvest {
    pub head: CommitSha,
    /// The base branch the clone started from.
    pub base: String,
    pub commits: u32,
    pub files: u32,
    pub bytes: u64,
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
