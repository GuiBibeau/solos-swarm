//! Polls each repository's issues and pull requests, issue comments, pull
//! request review comments and workflow runs, oldest first, `since` a cursor
//! kept in the state directory. First pages carry their `ETag`, so a quiet
//! repository costs 304s, which GitHub does not count against the rate limit.

use super::{
    Fatal, GithubApi, Got, MARKER_PREFIX, SOURCE, is_fatal, reply_to, retry_after, thread,
};
use crate::agent::ports::{BoxFuture, FactSink, Source, TriggerSink};
use crate::agent::utc;
use crate::broker::now_ms;
use anyhow::{Context, Result, bail};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use solos_agent_core::model::{Actor, ActorKind, Addressed, Ms, Trigger, TriggerId};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Listings are read again this far behind the cursor, for items GitHub
/// lists late and for clock skew. The Trigger ids sent within it are kept
/// with the cursor, so nothing is sent twice.
pub const OVERLAP: Duration = Duration::from_secs(600);
const OVERLAP_MS: Ms = 600_000;
const PER_PAGE: &str = "100";
const MAX_PAGES: usize = 10;
/// Workflow run conclusions that count as a failure.
const FAILED: [&str; 3] = ["failure", "timed_out", "startup_failure"];
/// The account GitHub Actions acts as.
const ACTIONS: &str = "github-actions[bot]";

/// Reads the configured repositories only; the agent's own comments are
/// dropped before they reach the Engine.
pub struct GithubSource {
    api: Arc<GithubApi>,
    repos: Vec<String>,
    cursor: PathBuf,
    follow: bool,
    poll: Duration,
    retry_base: Duration,
    retry_max: Duration,
}

impl GithubSource {
    /// `repos` are `owner/name`; the read position is the file `cursor`.
    /// With `follow` it polls every minute until stopped; without, it reads
    /// once and ends, for tests and dry runs.
    pub fn new(
        api: Arc<GithubApi>,
        repos: impl IntoIterator<Item = String>,
        cursor: impl Into<PathBuf>,
        follow: bool,
    ) -> Self {
        Self {
            api,
            repos: repos.into_iter().collect(),
            cursor: cursor.into(),
            follow,
            poll: Duration::from_secs(60),
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(300),
        }
    }

    #[must_use]
    pub fn with_poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    /// After a failed poll, wait `base`, twice as long after each failure in
    /// a row, up to `max`. A rate limit waits as long as GitHub asks.
    #[must_use]
    pub fn with_backoff(mut self, base: Duration, max: Duration) -> Self {
        self.retry_base = base;
        self.retry_max = max;
        self
    }
}

impl Source for GithubSource {
    fn run(self: Box<Self>, sink: TriggerSink, _: FactSink) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { self.serve(&sink).await })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feed {
    Issues,
    Comments,
    ReviewComments,
    Runs,
}

impl Feed {
    const ALL: [Self; 4] = [
        Self::Issues,
        Self::Comments,
        Self::ReviewComments,
        Self::Runs,
    ];

    fn key(self) -> &'static str {
        match self {
            Self::Issues => "issues",
            Self::Comments => "comments",
            Self::ReviewComments => "review_comments",
            Self::Runs => "runs",
        }
    }

    fn path(self, repo: &str) -> String {
        match self {
            Self::Issues => format!("repos/{repo}/issues"),
            Self::Comments => format!("repos/{repo}/issues/comments"),
            Self::ReviewComments => format!("repos/{repo}/pulls/comments"),
            Self::Runs => format!("repos/{repo}/actions/runs"),
        }
    }

    /// Runs have no `since`: the first page holds the latest completed ones.
    fn query(self, since: Ms) -> Vec<(&'static str, String)> {
        let mut query = match self {
            Self::Runs => {
                return vec![
                    ("status", "completed".into()),
                    ("per_page", PER_PAGE.into()),
                ];
            }
            Self::Issues => vec![("state", "all".to_owned())],
            Self::Comments | Self::ReviewComments => Vec::new(),
        };
        query.extend([
            ("sort", "updated".to_owned()),
            ("direction", "asc".to_owned()),
            ("since", utc::format(since)),
            ("per_page", PER_PAGE.to_owned()),
        ]);
        query
    }

    fn pages(self) -> usize {
        if self == Self::Runs { 1 } else { MAX_PAGES }
    }

    fn items(self, mut body: Value) -> Vec<Value> {
        let list = if self == Self::Runs {
            body.get_mut("workflow_runs")
                .map(Value::take)
                .unwrap_or_default()
        } else {
            body
        };
        match list {
            Value::Array(items) => items,
            _ => Vec::new(),
        }
    }

    /// When an item entered the listing: its creation, or a run's end.
    fn time(self, item: &Value) -> Option<Ms> {
        let field = if self == Self::Runs {
            "updated_at"
        } else {
            "created_at"
        };
        utc::parse(item[field].as_str()?)
    }
}

/// The read position, written whole to a temporary file and renamed over
/// the old one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    /// By repository, then listing.
    repos: BTreeMap<String, BTreeMap<String, Position>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Position {
    /// The latest `updated_at` read.
    at: Ms,
    /// Trigger ids sent that the listing can still return, with the time
    /// that keeps them in it.
    #[serde(default)]
    seen: BTreeMap<String, Ms>,
}

impl Cursor {
    /// A repository read for the first time starts at `now`, and that start
    /// is saved at once.
    fn load(path: &Path, repos: &[String], now: Ms) -> Result<Self> {
        let mut cursor: Self = match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "{} is not a GitHub read position; delete it to read from now",
                    path.display()
                )
            })?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        cursor.repos.retain(|repo, _| repos.contains(repo));
        let mut fresh = false;
        for repo in repos {
            let feeds = cursor.repos.entry(repo.clone()).or_default();
            for feed in Feed::ALL {
                feeds.entry(feed.key().to_owned()).or_insert_with(|| {
                    fresh = true;
                    Position {
                        at: now,
                        seen: BTreeMap::new(),
                    }
                });
            }
        }
        if fresh {
            cursor.save(path)?;
        }
        Ok(cursor)
    }

    fn position(&mut self, repo: &str, feed: Feed) -> &mut Position {
        self.repos
            .entry(repo.to_owned())
            .or_default()
            .entry(feed.key().to_owned())
            .or_default()
    }

    fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        let write = || -> std::io::Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&serde_json::to_vec(self)?)?;
            file.sync_all()?;
            std::fs::rename(&tmp, path)?;
            if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
                std::fs::File::open(dir)?.sync_all()?;
            }
            Ok(())
        };
        write().with_context(|| format!("cannot write {}", path.display()))
    }
}

#[derive(Clone)]
struct State {
    cursor: Cursor,
    /// The ETag of each first page read, by its URL.
    etags: BTreeMap<String, String>,
    /// Repositories whose workflow runs the token cannot read.
    no_runs: BTreeSet<String>,
}

enum Listing {
    Items(Vec<Value>),
    Unchanged,
    Refused(StatusCode, String),
}

impl GithubSource {
    async fn serve(&self, sink: &TriggerSink) -> Result<()> {
        let me = self.begin().await?;
        let cursor = Cursor::load(&self.cursor, &self.repos, now_ms())?;
        let mut saved = cursor.clone();
        let mut state = State {
            cursor,
            etags: BTreeMap::new(),
            no_runs: BTreeSet::new(),
        };
        let mut failures = 0;
        loop {
            // The read moves the position only once its batch is handed on.
            let mut next = state.clone();
            match self.poll(&me, &mut next).await {
                Ok(batch) => {
                    failures = 0;
                    if !batch.is_empty() && sink.send(batch).await.is_err() {
                        return Ok(());
                    }
                    state = next;
                    if !self.follow {
                        self.save(sink, &state.cursor, &mut saved).await;
                        return Ok(());
                    }
                    tokio::time::sleep(self.poll).await;
                }
                Err(err) => {
                    let wait = self.retry(err, failures)?;
                    failures = failures.saturating_add(1);
                    tokio::time::sleep(wait).await;
                }
            }
            // Saved a poll interval after its batch went out, so a crash
            // cannot leave it past Triggers the runtime has not stored.
            self.save(sink, &state.cursor, &mut saved).await;
        }
    }

    /// Persist `cursor` once the runtime has taken every batch sent, which
    /// it stores as it takes them.
    async fn save(&self, sink: &TriggerSink, cursor: &Cursor, saved: &mut Cursor) {
        if cursor == saved || sink.reserve_many(sink.max_capacity()).await.is_err() {
            return;
        }
        match cursor.save(&self.cursor) {
            Ok(()) => *saved = cursor.clone(),
            Err(err) => eprintln!("agent: github: the read position was not saved: {err:#}"),
        }
    }

    /// How long to wait before trying again, or the error when waiting
    /// cannot help.
    fn retry(&self, err: anyhow::Error, failures: u32) -> Result<Duration> {
        if is_fatal(&err) {
            return Err(err);
        }
        if let Some(wait) = retry_after(&err) {
            eprintln!("agent: github: {err:#}");
            return Ok(wait);
        }
        if !self.follow {
            return Err(err);
        }
        let wait = self
            .retry_base
            .saturating_mul(2u32.saturating_pow(failures))
            .min(self.retry_max);
        eprintln!("agent: github: {err:#}; retrying in {wait:?}");
        Ok(wait)
    }

    /// The agent's login, once the token reads every configured repository.
    async fn begin(&self) -> Result<String> {
        let mut failures = 0;
        loop {
            let err = match self.start().await {
                Ok(me) => return Ok(me),
                Err(err) => err,
            };
            let wait = self.retry(err, failures)?;
            failures = failures.saturating_add(1);
            tokio::time::sleep(wait).await;
        }
    }

    async fn start(&self) -> Result<String> {
        let me = self.api.login().await?;
        for repo in &self.repos {
            let url = self.api.url(&format!("repos/{repo}"));
            if let Got::Refused { status, message } = self.api.get(&url, None).await? {
                let why = format!("the GitHub token cannot read {repo}: HTTP {status}: {message}");
                return Err(Fatal(why).into());
            }
        }
        Ok(me)
    }

    /// One read of every listing: the new Triggers, oldest first. `state`
    /// moves past what was read.
    async fn poll(&self, me: &str, state: &mut State) -> Result<Vec<Trigger>> {
        let mut etags = BTreeMap::new();
        let mut batch = Vec::new();
        for repo in &self.repos {
            // Open pull requests by head commit, read once a run needs them.
            let mut pulls = None;
            for feed in Feed::ALL {
                if feed == Feed::Runs && state.no_runs.contains(repo) {
                    continue;
                }
                let since = state
                    .cursor
                    .position(repo, feed)
                    .at
                    .saturating_sub(OVERLAP_MS);
                let items = match self
                    .list(repo, feed, since, &state.etags, &mut etags)
                    .await?
                {
                    Listing::Items(items) => items,
                    Listing::Unchanged => continue,
                    Listing::Refused(status, message) => {
                        let what = feed.key();
                        eprintln!("agent: github: cannot read {repo} {what}: {status}: {message}");
                        if feed == Feed::Runs {
                            eprintln!("agent: github: CI results from {repo} are off");
                            state.no_runs.insert(repo.clone());
                        }
                        continue;
                    }
                };
                for item in &items {
                    // Older items were read before, or predate the first read.
                    let time = feed.time(item).filter(|time| *time >= since);
                    let trigger = match feed {
                        _ if time.is_none() => None,
                        Feed::Issues => opened(repo, item, me),
                        Feed::Comments => commented(repo, item, me, false),
                        Feed::ReviewComments => commented(repo, item, me, true),
                        Feed::Runs => self.failed(repo, item, &mut pulls).await?,
                    };
                    let position = state.cursor.position(repo, feed);
                    if let Some(updated) = item["updated_at"].as_str().and_then(utc::parse) {
                        position.at = position.at.max(updated);
                    }
                    if let (Some(time), Some(trigger)) = (time, trigger)
                        && position.seen.insert(trigger.id.to_string(), time).is_none()
                    {
                        batch.push(trigger);
                    }
                }
                let position = state.cursor.position(repo, feed);
                let floor = position.at.saturating_sub(OVERLAP_MS);
                position.seen.retain(|_, time| *time >= floor);
            }
        }
        state.etags = etags;
        batch.sort_by_key(|trigger| trigger.at_ms);
        Ok(batch)
    }

    /// The items of one listing since `since`, following its pages.
    async fn list(
        &self,
        repo: &str,
        feed: Feed,
        since: Ms,
        known: &BTreeMap<String, String>,
        etags: &mut BTreeMap<String, String>,
    ) -> Result<Listing> {
        let url = Url::parse_with_params(&self.api.url(&feed.path(repo)), feed.query(since))?;
        let url = url.to_string();
        let etag = known.get(&url);
        let page = match self.api.get(&url, etag.map(String::as_str)).await? {
            Got::Ok(page) => page,
            Got::NotModified => {
                if let Some(etag) = etag {
                    etags.insert(url, etag.clone());
                }
                return Ok(Listing::Unchanged);
            }
            Got::Refused { status, message } => return Ok(Listing::Refused(status, message)),
        };
        let mut next = page.links.get("next").cloned();
        // New items land on the last page of an oldest-first listing, behind
        // an unchanged first page, so only a listing read whole is asked for
        // conditionally. Runs are read one page by design.
        if (next.is_none() || feed.pages() == 1)
            && let Some(etag) = page.etag
        {
            etags.insert(url, etag);
        }
        let mut items = feed.items(page.body);
        for _ in 1..feed.pages() {
            let Some(url) = next.take() else { break };
            match self.api.get(&url, None).await? {
                Got::Ok(page) => {
                    items.extend(feed.items(page.body));
                    next = page.links.get("next").cloned();
                }
                Got::NotModified | Got::Refused { .. } => {
                    bail!("github: a later page of {repo} {} was refused", feed.key())
                }
            }
        }
        Ok(Listing::Items(items))
    }

    /// A failed workflow run on a pull request. Runs from forks name no pull
    /// request, so their head commit is looked up among the open ones.
    async fn failed(
        &self,
        repo: &str,
        run: &Value,
        pulls: &mut Option<Vec<(u64, String)>>,
    ) -> Result<Option<Trigger>> {
        if !FAILED.contains(&run["conclusion"].as_str().unwrap_or_default()) {
            return Ok(None);
        }
        let sha = run["head_sha"].as_str().unwrap_or_default();
        let mut numbers: Vec<u64> = run["pull_requests"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|pull| pull["number"].as_u64())
            .collect();
        if numbers.is_empty() && !sha.is_empty() {
            if pulls.is_none() {
                *pulls = Some(self.open_pulls(repo).await?);
            }
            numbers = pulls
                .iter()
                .flatten()
                .filter(|(_, head)| head == sha)
                .map(|(number, _)| *number)
                .collect();
        }
        Ok(run_trigger(repo, run, &numbers))
    }

    async fn open_pulls(&self, repo: &str) -> Result<Vec<(u64, String)>> {
        let query = [("state", "open"), ("per_page", PER_PAGE)];
        let url = Url::parse_with_params(&self.api.url(&format!("repos/{repo}/pulls")), query)?;
        match self.api.get(url.as_str(), None).await? {
            Got::Ok(page) => Ok(page
                .body
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|pull| {
                    Some((
                        pull["number"].as_u64()?,
                        pull["head"]["sha"].as_str()?.to_owned(),
                    ))
                })
                .collect()),
            Got::NotModified => Ok(Vec::new()),
            Got::Refused { status, message } => {
                eprintln!("agent: github: cannot read {repo} pull requests: {status}: {message}");
                Ok(Vec::new())
            }
        }
    }
}

fn actor(user: &Value) -> Option<Actor> {
    let login = user["login"].as_str()?;
    let kind = if user["type"] == "Bot" || login.ends_with("[bot]") {
        ActorKind::Bot
    } else {
        ActorKind::Person
    };
    Some(Actor {
        id: login.to_owned(),
        kind,
        name: None,
    })
}

/// A new issue or pull request.
fn opened(repo: &str, issue: &Value, me: &str) -> Option<Trigger> {
    let number = issue["number"].as_u64()?;
    let actor = actor(&issue["user"])?;
    if actor.id.eq_ignore_ascii_case(me) {
        return None;
    }
    let title = issue["title"].as_str().unwrap_or_default().trim();
    let body = issue["body"].as_str().unwrap_or_default().trim();
    let text = if body.is_empty() {
        title.to_owned()
    } else {
        format!("{title}\n\n{body}")
    };
    let kind = if issue["pull_request"].is_null() {
        "issue"
    } else {
        "pull_request"
    };
    let meta = json!({
        "kind": kind,
        "repo": repo,
        "number": number,
        "id": issue["id"],
        "user": actor.id,
        "user_id": issue["user"]["id"],
        "state": issue["state"],
        "html_url": issue["html_url"],
    });
    Some(Trigger {
        id: TriggerId::new(format!("{SOURCE}:{repo}#{number}")),
        source: SOURCE.into(),
        at_ms: utc::parse(issue["created_at"].as_str()?)?,
        actor,
        addressed: addressed(&text, me),
        text,
        threads: vec![thread(repo, number)],
        reply_to: vec![reply_to(repo, number)],
        meta,
    })
}

/// A comment on an issue or a pull request's conversation, or with
/// `review`, on a pull request's diff. The two kinds number their ids
/// apart, so their Trigger ids say which.
fn commented(repo: &str, comment: &Value, me: &str, review: bool) -> Option<Trigger> {
    let id = comment["id"].as_u64()?;
    let on = if review {
        "pull_request_url"
    } else {
        "issue_url"
    };
    let number = comment[on].as_str()?.rsplit('/').next()?.parse().ok()?;
    let actor = actor(&comment["user"])?;
    let body = comment["body"].as_str().unwrap_or_default();
    if actor.id.eq_ignore_ascii_case(me) || body.contains(MARKER_PREFIX) {
        return None;
    }
    let kind = if review { "review_comment" } else { "comment" };
    let on_pull = review
        || comment["html_url"]
            .as_str()
            .is_some_and(|url| url.contains("/pull/"));
    let mut meta = json!({
        "kind": kind,
        "repo": repo,
        "number": number,
        "comment_id": id,
        "pull_request": on_pull,
        "user": actor.id,
        "user_id": comment["user"]["id"],
        "html_url": comment["html_url"],
    });
    if review {
        for key in [
            "path",
            "line",
            "commit_id",
            "in_reply_to_id",
            "pull_request_review_id",
        ] {
            meta[key] = comment[key].clone();
        }
    }
    Some(Trigger {
        id: TriggerId::new(format!("{SOURCE}:{repo}:{kind}:{id}")),
        source: SOURCE.into(),
        at_ms: utc::parse(comment["created_at"].as_str()?)?,
        actor,
        addressed: addressed(body, me),
        text: unmention(body, me),
        threads: vec![thread(repo, number)],
        reply_to: vec![reply_to(repo, number)],
        meta,
    })
}

/// A bot Trigger on each pull request the run tested, primary first.
fn run_trigger(repo: &str, run: &Value, numbers: &[u64]) -> Option<Trigger> {
    let &first = numbers.first()?;
    let id = run["id"].as_u64()?;
    let attempt = run["run_attempt"].as_u64().unwrap_or(1);
    let name = run["name"].as_str().unwrap_or("CI");
    let conclusion = run["conclusion"].as_str()?;
    let sha = run["head_sha"].as_str().unwrap_or_default();
    let url = run["html_url"].as_str().unwrap_or_default();
    let short = sha.get(..7).unwrap_or(sha);
    let text =
        format!("CI failed on #{first}: the {name} workflow ended {conclusion} at {short}. {url}");
    let meta = json!({
        "kind": "workflow_run",
        "repo": repo,
        "number": first,
        "pull_requests": numbers,
        "run_id": id,
        "run_attempt": attempt,
        "workflow_id": run["workflow_id"],
        "name": name,
        "conclusion": conclusion,
        "head_sha": sha,
        "head_branch": run["head_branch"],
        "event": run["event"],
        "html_url": url,
    });
    Some(Trigger {
        id: TriggerId::new(format!("{SOURCE}:{repo}:run:{id}:{attempt}")),
        source: SOURCE.into(),
        at_ms: utc::parse(run["updated_at"].as_str()?)?,
        actor: Actor {
            id: ACTIONS.into(),
            kind: ActorKind::Bot,
            name: Some(name.to_owned()),
        },
        addressed: Addressed::Unaddressed,
        text: text.trim_end().to_owned(),
        threads: numbers.iter().map(|number| thread(repo, *number)).collect(),
        reply_to: vec![reply_to(repo, first)],
        meta,
    })
}

fn addressed(text: &str, me: &str) -> Addressed {
    if mentions(text, me) {
        Addressed::Mention
    } else {
        Addressed::Unaddressed
    }
}

fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/')
}

/// Whether `text` mentions `@login` as GitHub links it: not inside a word or
/// an email address, and not as the start of a longer name or a team.
fn mentions(text: &str, login: &str) -> bool {
    let text = text.to_ascii_lowercase();
    let needle = format!("@{}", login.to_ascii_lowercase());
    text.match_indices(&needle).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + needle.len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_') && !after.is_some_and(name_char)
    })
}

/// The text without the `@login` people open a comment to the agent with.
fn unmention(text: &str, login: &str) -> String {
    let text = text.trim();
    let mention = format!("@{login}");
    let rest = text
        .get(..mention.len())
        .filter(|head| head.eq_ignore_ascii_case(&mention))
        .map(|_| &text[mention.len()..])
        .filter(|rest| !rest.chars().next().is_some_and(name_char));
    match rest {
        Some(rest) => rest.trim_start_matches([',', ':']).trim().to_owned(),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{mentions, unmention};

    #[test]
    fn a_mention_is_the_login_alone_and_any_case() {
        for (text, mentioned) in [
            ("@solos-bot fix it", true),
            ("ping @SOLOS-BOT.", true),
            ("(@solos-bot)", true),
            ("mail alice@solos-bot.dev", false),
            ("@solos-bots fix it", false),
            ("@solos-bot-2 fix it", false),
            ("@solos-bot/team", false),
            ("no mention", false),
        ] {
            assert_eq!(mentions(text, "solos-bot"), mentioned, "{text}");
        }
        assert_eq!(
            unmention("  @Solos-Bot, fix the test ", "solos-bot"),
            "fix the test"
        );
        assert_eq!(unmention("@solos-bot: x", "solos-bot"), "x");
        assert_eq!(unmention("@solos-bots x", "solos-bot"), "@solos-bots x");
        assert_eq!(
            unmention("thanks @solos-bot", "solos-bot"),
            "thanks @solos-bot"
        );
        assert_eq!(unmention("é", "solos-bot"), "é");
    }
}
