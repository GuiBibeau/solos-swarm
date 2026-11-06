//! GitHub, as the App. The mirror fetches with a token narrowed to the one
//! repository and to read access; the push and the pull request use one
//! narrowed to write access, which goes in git's environment for that command
//! alone, never in its argv, which any local user can read. The clone's
//! commits are by the App's bot, and so is the pull request.

use super::mirror::{Branch, Mirrors, Remote};
use crate::agent::config::Identity;
use crate::agent::github::{self, Access, GithubApi, Got, InstallToken, Minter, quiet, shape};
use crate::agent::ports::{
    BoxFuture, Execute, Forge, Proposal, Publish, Refusal, TicketDirs, Workspace,
};
use anyhow::{Context, Result, bail};
use reqwest::{Method, Url};
use serde_json::{Value, json};
use solos_agent_core::change::{CommitSha, Harvest, PrRef, RepoName, Withheld};
use solos_agent_core::delivery::Observed;
use solos_agent_core::model::{TicketId, Trigger};
use std::sync::Arc;
use zeroize::Zeroizing;

/// The pull request's body is the worker's words, cut like a comment, then
/// the host's footer.
const BODY_CHARS: usize = 16_000;
const TITLE_CHARS: usize = 120;
/// What of GitHub's refusal a Ticket keeps.
const MESSAGE_CHARS: usize = 300;

pub struct GithubForge {
    pub(super) mirrors: Mirrors,
    pub(super) minter: Arc<Minter>,
    pub(super) api: Arc<GithubApi>,
    /// Ends in `/`; a mirror fetches and pushes `<git_base><owner>/<name>.git`.
    pub(super) git_base: String,
}

/// Where a mirror fetches `repo` from, and the git config key and value that
/// authenticate that command: the token as an `extraheader` for that base
/// alone.
pub(super) fn github_remote(
    git_base: &str,
    repo: &RepoName,
    token: &InstallToken,
) -> (String, String, Zeroizing<String>) {
    (
        format!("{git_base}{repo}.git"),
        format!("http.{git_base}.extraheader"),
        token.git_header(),
    )
}

/// The hidden line that ties a pull request to its Ticket, so a pull request
/// that merely has our branch's name is not taken for ours.
pub fn ticket_marker(ticket: &TicketId) -> String {
    format!("<!-- solos-swarm ticket {ticket} -->")
}

impl Forge for GithubForge {
    fn prepare<'a>(
        &'a self,
        repo: &'a RepoName,
        dirs: TicketDirs<'a>,
    ) -> BoxFuture<'a, Result<Workspace>> {
        Box::pin(async move {
            // A finished clone needs nothing from GitHub, so a recovery does
            // not wait on a suspended installation.
            if let Some(done) = super::mirror::prepared(&dirs.host.join(super::REPO_JSON))? {
                return Ok(done);
            }
            let token = self.minter.token(repo, Access::Read).await?;
            let (name, email) = self.minter.bot().await?.commit_identity();
            let (url, key, header) = github_remote(&self.git_base, repo, &token);
            let remote = Remote {
                url: &url,
                auth: Some((&key, &header)),
            };
            let identity = Identity { name, email };
            self.mirrors.prepare(repo, dirs, &remote, &identity).await
        })
    }

    fn admit<'a>(
        &'a self,
        proposal: Proposal<'a>,
    ) -> BoxFuture<'a, Result<std::result::Result<Harvest, Refusal>>> {
        Box::pin(async move {
            let (name, email) = self.minter.bot().await?.commit_identity();
            let bot = Identity { name, email };
            self.mirrors.admit(proposal.repo, &proposal, &bot).await
        })
    }

    /// Publish, every step a no-op where it is already true: the branch holds
    /// the head (`ls-remote` first: equal to the head is done, equal to the
    /// lease is pushed with `--force-with-lease` against it, anything else is
    /// someone else's and is never overwritten), then the pull request is
    /// found by its head or made ready (never a draft), and read back.
    fn execute<'a>(&'a self, job: Execute<'a>) -> BoxFuture<'a, Result<Observed>> {
        Box::pin(async move {
            match job {
                Execute::Publish(publish) => self.publish(publish).await,
            }
        })
    }
}

impl GithubForge {
    async fn publish(&self, p: Publish<'_>) -> Result<Observed> {
        let withheld = |why| {
            Ok(Observed::Withheld {
                ticket: p.ticket.clone(),
                head: p.head.clone(),
                why,
            })
        };
        // Read-only is physical: no write token is minted.
        if !self.minter.writes() {
            return withheld(Withheld::ReadOnly);
        }
        // A suspended or missing installation is a wait the queue repeats.
        let token = self.minter.token(p.repo, Access::Write).await?;
        let (url, key, header) = github_remote(&self.git_base, p.repo, &token);
        let remote = Remote {
            url: &url,
            auth: Some((&key, &header)),
        };
        let pushed = self
            .mirrors
            .push_head(p.repo, &remote, p.ticket, p.head, p.branch, p.lease)
            .await?;
        match pushed {
            Branch::Pushed | Branch::Present => {}
            Branch::Moved(found) => return withheld(Withheld::BranchMoved { found }),
            Branch::Gone => {
                return withheld(Withheld::Forbidden {
                    message: "the branch is gone from GitHub".into(),
                });
            }
            Branch::Rejected(message) => {
                return withheld(Withheld::Forbidden {
                    message: clipped(&message),
                });
            }
        }
        let (number, html_url, created) = match self.pull_request(&p).await? {
            Pull::Ready {
                number,
                url,
                created,
            } => (number, url, created),
            Pull::Closed => return withheld(Withheld::PrClosed),
            Pull::Refused(message) => return withheld(Withheld::Forbidden { message }),
        };
        Ok(Observed::Published {
            ticket: p.ticket.clone(),
            head: p.head.clone(),
            pr: PrRef {
                repo: p.repo.clone(),
                number,
            },
            url: html_url,
            thread: github::thread(p.repo.as_str(), number),
            reply_to: github::reply_to(p.repo.as_str(), number),
            created,
        })
    }

    /// The pull request for the Ticket's branch: the open one found by its
    /// head, or one made now, ready for review. What GitHub shows of its head
    /// is read back, and must be the head just pushed.
    async fn pull_request(&self, p: &Publish<'_>) -> Result<Pull> {
        let mut created = false;
        let mut found = self.find_pull(p).await?;
        if matches!(found, Found::None) {
            match self.open_pull(p).await? {
                Opened::Made(pull) => {
                    created = true;
                    found = Found::Open(pull);
                }
                Opened::Exists => found = self.find_pull(p).await?,
                Opened::Refused(message) => return Ok(Pull::Refused(message)),
            }
        }
        let pull = match found {
            Found::Open(pull) => pull,
            Found::Closed => return Ok(Pull::Closed),
            Found::None => bail!(
                "GitHub says a pull request exists for {}, and lists none",
                p.branch
            ),
        };
        if !pull.body.contains(&ticket_marker(p.ticket)) {
            return Ok(Pull::Refused(
                "a pull request on this branch is not this ticket's".into(),
            ));
        }
        if &pull.head != p.head {
            // What a push moves a pull request to may lag a moment.
            bail!(
                "GitHub shows {} as the head of #{}, not the {} just pushed: waiting for it",
                pull.head.short(),
                pull.number,
                p.head.short()
            );
        }
        Ok(Pull::Ready {
            number: pull.number,
            url: pull.url,
            created,
        })
    }

    /// `GET pulls?head=<owner>:<branch>&state=all`: an open one first, else
    /// one a person closed.
    async fn find_pull(&self, p: &Publish<'_>) -> Result<Found> {
        let head = format!("{}:{}", p.repo.owner(), p.branch);
        let query = [
            ("head", head.as_str()),
            ("state", "all"),
            ("per_page", "100"),
        ];
        let base = self.api.url(&format!("repos/{}/pulls", p.repo));
        let url = Url::parse_with_params(&base, query)?;
        let page = match self.api.get(p.repo, url.as_str(), None).await? {
            Got::Ok(page) => page,
            Got::NotModified => bail!("github GET pulls: an unasked 304"),
            Got::Refused { status, message } => {
                bail!("github GET pulls on {}: HTTP {status}: {message}", p.repo)
            }
        };
        let mut closed = false;
        for pull in page.body.as_array().into_iter().flatten() {
            let ours = pull["head"]["ref"].as_str() == Some(p.branch.as_str());
            if !ours {
                continue;
            }
            if pull["state"] == "open" {
                return Ok(Found::Open(PullInfo::read(pull)?));
            }
            closed = true;
        }
        Ok(if closed { Found::Closed } else { Found::None })
    }

    /// `POST pulls`: ready, never a draft.
    async fn open_pull(&self, p: &Publish<'_>) -> Result<Opened> {
        let body = json!({
            "title": title(p.title),
            "head": p.branch.as_str(),
            "base": p.base,
            "body": body(p),
            "draft": false,
            "maintainer_can_modify": false,
        });
        let url = self.api.url(&format!("repos/{}/pulls", p.repo));
        match self
            .api
            .send(p.repo, Method::POST, &url, Some(&body))
            .await?
        {
            Got::Ok(page) => Ok(Opened::Made(PullInfo::read(&page.body)?)),
            Got::NotModified => bail!("github POST pulls: an unasked 304"),
            Got::Refused { status, message } => {
                let said = message.to_ascii_lowercase();
                if status.as_u16() == 422 && said.contains("already exists") {
                    Ok(Opened::Exists)
                } else if status.is_client_error() {
                    Ok(Opened::Refused(clipped(&format!(
                        "HTTP {status}: {message}"
                    ))))
                } else {
                    bail!("github POST pulls on {}: HTTP {status}: {message}", p.repo)
                }
            }
        }
    }
}

/// A pull request as GitHub shows it.
struct PullInfo {
    number: u64,
    url: String,
    head: CommitSha,
    body: String,
}

enum Found {
    Open(PullInfo),
    /// Only closed ones, which are never reopened.
    Closed,
    None,
}

enum Opened {
    Made(PullInfo),
    Exists,
    Refused(String),
}

/// What `pull_request` came to.
enum Pull {
    Ready {
        number: u64,
        url: String,
        created: bool,
    },
    Closed,
    Refused(String),
}

impl PullInfo {
    fn read(value: &Value) -> Result<Self> {
        let number = value["number"]
            .as_u64()
            .context("GitHub's pull request holds no number")?;
        let url = value["html_url"]
            .as_str()
            .context("GitHub's pull request holds no url")?
            .to_owned();
        let head = value["head"]["sha"]
            .as_str()
            .context("GitHub's pull request holds no head")?;
        let head = CommitSha::try_from(head.to_owned())
            .map_err(|why| anyhow::anyhow!("GitHub's pull request: {why}"))?;
        Ok(Self {
            number,
            url,
            head,
            body: value["body"].as_str().unwrap_or_default().to_owned(),
        })
    }
}

fn clipped(text: &str) -> String {
    let text = text.trim();
    let end = text
        .char_indices()
        .nth(MESSAGE_CHARS)
        .map_or(text.len(), |(at, _)| at);
    text[..end].to_owned()
}

/// The Ticket's title as a pull request title: one line, no control
/// characters, mentions quieted, at most 120 characters.
pub fn title(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    let (quiet, _) = quiet(&flat);
    if quiet.chars().count() <= TITLE_CHARS {
        return quiet;
    }
    let kept: String = quiet.chars().take(TITLE_CHARS - 1).collect();
    format!("{}…", kept.trim_end())
}

/// The issue of `repo` a Trigger opened or commented on, which the pull
/// request closes at merge. The host reads it from the Trigger's own record
/// of where it came from, never from text.
pub fn closes(origin: &Trigger, repo: &RepoName) -> Option<u64> {
    if origin.source != github::SOURCE {
        return None;
    }
    let meta = &origin.meta;
    if meta["repo"].as_str() != Some(repo.as_str()) {
        return None;
    }
    let number = meta["number"].as_u64()?;
    let on_issue = match meta["kind"].as_str()? {
        "issue" => true,
        "comment" => meta["pull_request"] == json!(false),
        _ => false,
    };
    on_issue.then_some(number)
}

/// The worker's words, shaped like every comment the agent posts, then the
/// host's footer: `Closes #n` when the request was an issue of this
/// repository, who asked and the Ticket, and the hidden marker last.
pub fn body(p: &Publish<'_>) -> String {
    let mut body = shape(p.text, BODY_CHARS, None);
    body.push_str("\n\n---\n");
    if let Some(number) = closes(p.origin, p.repo) {
        body.push_str(&format!("Closes #{number}\n"));
    }
    let who = p.requester.name.as_deref().unwrap_or(&p.requester.id);
    let (who, _) = quiet(&who.split_whitespace().collect::<Vec<_>>().join(" "));
    body.push_str(&format!(
        "Requested by {who} via {} · ticket {}\n\n{}",
        p.source,
        p.ticket,
        ticket_marker(p.ticket)
    ));
    body
}

#[cfg(test)]
mod tests {
    use super::{body, closes, ticket_marker, title};
    use crate::agent::ports::Publish;
    use serde_json::json;
    use solos_agent_core::change::{BranchName, CommitSha, RepoName};
    use solos_agent_core::model::{Actor, Addressed, ThreadKey, TicketId, Trigger, TriggerId};

    fn repo() -> RepoName {
        RepoName::try_from("acme/widgets".to_owned()).unwrap()
    }

    fn origin(source: &str, meta: serde_json::Value) -> Trigger {
        Trigger {
            id: TriggerId::new("t1"),
            source: source.into(),
            at_ms: 1,
            actor: Actor::person("alice"),
            addressed: Addressed::Mention,
            text: "@solos-bot fix it".into(),
            threads: vec![ThreadKey::new("th")],
            reply_to: Vec::new(),
            meta,
        }
    }

    #[test]
    fn a_pull_request_closes_the_issue_it_came_from_only_when_the_trigger_says_it_was_one() {
        let issue = json!({"kind":"issue","repo":"acme/widgets","number":12});
        let comment = |pull: bool| json!({"kind":"comment","repo":"acme/widgets","number":12,"pull_request":pull});
        assert_eq!(closes(&origin("github", issue.clone()), &repo()), Some(12));
        assert_eq!(closes(&origin("github", comment(false)), &repo()), Some(12));
        for not in [
            origin("github", comment(true)),
            origin(
                "github",
                json!({"kind":"pull_request","repo":"acme/widgets","number":12}),
            ),
            origin(
                "github",
                json!({"kind":"review_comment","repo":"acme/widgets","number":12}),
            ),
            origin(
                "github",
                json!({"kind":"issue","repo":"acme/docs","number":12}),
            ),
            origin("github", json!({})),
            origin("discord", issue),
        ] {
            assert_eq!(closes(&not, &repo()), None, "{not:?}");
        }
    }

    #[test]
    fn the_body_is_the_workers_words_quieted_then_the_hosts_footer_ending_in_the_marker() {
        let ticket = TicketId::new("tk_9f2a");
        let branch = BranchName::for_ticket(&ticket, "Fix");
        let head = CommitSha::try_from("a".repeat(40)).unwrap();
        let alice = Actor::person("alice");
        let from = origin(
            "github",
            json!({"kind":"issue","repo":"acme/widgets","number":12}),
        );
        let text = "Fixed it, @bob and @acme/core: `@keep` stays.\n\n```\n@code stays\n```";
        let publish = Publish {
            ticket: &ticket,
            repo: &repo(),
            branch: &branch,
            base: "main",
            head: &head,
            lease: None,
            title: "Fix",
            text,
            requester: &alice,
            source: "github",
            origin: &from,
        };

        let made = body(&publish);

        assert!(
            made.starts_with(
                "Fixed it, `@bob` and `@acme/core`: `@keep` stays.\n\n```\n@code stays\n```"
            ),
            "{made}"
        );
        assert!(made.contains("\nCloses #12\n"), "{made}");
        assert!(
            made.contains("Requested by alice via github · ticket tk_9f2a"),
            "{made}"
        );
        assert!(made.ends_with(&ticket_marker(&ticket)), "{made}");
        let other = origin("discord", json!({}));
        let made = body(&Publish {
            origin: &other,
            ..publish
        });
        assert!(!made.contains("Closes"), "{made}");
    }

    #[test]
    fn a_title_is_one_quiet_line_of_at_most_a_hundred_and_twenty_characters() {
        assert_eq!(
            title("  Add --json\tto\n`agent status`  "),
            "Add --json to `agent status`"
        );
        assert_eq!(title("Ask @bob about it"), "Ask `@bob` about it");
        assert_eq!(title("x\u{7}y\u{1b}[31mz"), "x y [31mz");
        let long = title(&"word ".repeat(60));
        assert!(long.chars().count() <= 120 && long.ends_with('…'), "{long}");
    }
}
