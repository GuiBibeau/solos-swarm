//! Replies as comments on the issue or pull request a Trigger came from.
//! GitHub does not deduplicate posts, so each comment ends with a hidden
//! marker naming its `MessageId`, and a repeat looks for it first.

use super::{GithubApi, Got, Page, marker, valid_repo};
use crate::agent::links::TicketLinks;
use crate::agent::markdown::{closes, fence_run};
use crate::agent::ports::{BoxFuture, Channel, SendOptions, SendOutcome};
use anyhow::{Result, bail};
use reqwest::{Method, StatusCode, Url};
use serde_json::{Value, json};
use solos_agent_core::change::RepoName;
use solos_agent_core::model::{ChannelRef, MessageId, MessageKind, OutboundMessage};
use std::sync::Arc;

/// Acknowledgements and questions stay short; a result can be a structured
/// report, cut well under GitHub's 65,536-character limit.
const SHORT_CHARS: usize = 2_000;
const RESULT_CHARS: usize = 16_000;
const PER_PAGE: &str = "100";
/// Bounds the search of one conversation for an earlier post.
const MAX_PAGES: usize = 30;

pub struct GithubChannel {
    api: Arc<GithubApi>,
    links: TicketLinks,
}

enum Found {
    Posted,
    Missing,
    Gone,
}

impl GithubChannel {
    pub fn new(api: Arc<GithubApi>) -> Self {
        Self {
            api,
            links: TicketLinks::none(),
        }
    }

    /// Link the messages that carry one to their Ticket's page.
    pub fn with_links(mut self, links: TicketLinks) -> Self {
        self.links = links;
        self
    }

    async fn post(&self, message: OutboundMessage, options: SendOptions) -> Result<SendOutcome> {
        let Some((repo, number)) = address(&message.channel) else {
            eprintln!("agent: github: message {} names no issue", message.id);
            return Ok(SendOutcome::Gone);
        };
        if !self.api.allows(&repo) {
            eprintln!(
                "agent: github: message {} is for {repo}, which is not in forge.repos, so the \
                 App holds no token for it",
                message.id
            );
            return Ok(SendOutcome::Gone);
        }
        let url = self
            .api
            .url(&format!("repos/{repo}/issues/{number}/comments"));
        if options.repeat {
            match self.find(&repo, &url, &message.id).await? {
                Found::Posted => return Ok(SendOutcome::Sent),
                Found::Gone => return Ok(SendOutcome::Gone),
                Found::Missing => {}
            }
        }
        let body = json!({ "body": body(&message, &self.links) });
        match self
            .api
            .send(&repo, Method::POST, &url, Some(&body))
            .await?
        {
            Got::Ok(_) => Ok(SendOutcome::Sent),
            Got::Refused {
                status,
                message: why,
            } if gone(status, &why) => {
                eprintln!("agent: github: cannot comment on {repo}#{number}: {status}: {why}");
                Ok(SendOutcome::Gone)
            }
            Got::Refused {
                status,
                message: why,
            } => {
                bail!("github comment on {repo}#{number}: HTTP {status}: {why}")
            }
            Got::NotModified => bail!("github comment on {repo}#{number}: an unasked 304"),
        }
    }

    /// Whether the conversation already holds the agent's comment `id`.
    async fn find(&self, repo: &RepoName, url: &str, id: &MessageId) -> Result<Found> {
        let me = self.api.bot().await?.login;
        let marker = marker(id);
        let Some(first) = self.comments(repo, url, 1).await? else {
            return Ok(Found::Gone);
        };
        if holds(&first.body, &me, &marker) {
            return Ok(Found::Posted);
        }
        let last = first
            .links
            .get("last")
            .and_then(|url| page_of(url))
            .unwrap_or(1);
        // GitHub lists oldest first, and an earlier attempt is among the newest.
        for page in (2..=last).rev().take(MAX_PAGES) {
            let Some(found) = self.comments(repo, url, page).await? else {
                return Ok(Found::Gone);
            };
            if holds(&found.body, &me, &marker) {
                return Ok(Found::Posted);
            }
        }
        if last.saturating_sub(1) > MAX_PAGES as u64 {
            eprintln!("agent: github: {url} is too long to search; posting");
        }
        Ok(Found::Missing)
    }

    /// One page of the conversation, or `None` when it is gone.
    async fn comments(&self, repo: &RepoName, url: &str, page: u64) -> Result<Option<Page>> {
        let query = [("per_page", PER_PAGE), ("page", &page.to_string())];
        let paged = Url::parse_with_params(url, query)?;
        match self.api.get(repo, paged.as_str(), None).await? {
            Got::Ok(found) => Ok(Some(found)),
            Got::Refused { status, message } if gone(status, &message) => {
                eprintln!("agent: github: cannot read {url}: {status}: {message}");
                Ok(None)
            }
            Got::Refused { status, message } => bail!("github comments: HTTP {status}: {message}"),
            Got::NotModified => bail!("github comments: an unasked 304"),
        }
    }
}

impl Channel for GithubChannel {
    fn send(
        &self,
        message: OutboundMessage,
        options: SendOptions,
    ) -> BoxFuture<'_, Result<SendOutcome>> {
        Box::pin(self.post(message, options))
    }
}

fn address(channel: &ChannelRef) -> Option<(RepoName, u64)> {
    let repo = channel
        .address
        .get("repo")
        .filter(|repo| valid_repo(repo))?;
    let repo = RepoName::try_from(repo.clone()).ok()?;
    let number = channel.address.get("number")?.parse().ok()?;
    Some((repo, number))
}

/// The issue can never take the comment: it is gone, issues are off, or it
/// is locked to people outside the repository.
fn gone(status: StatusCode, message: &str) -> bool {
    matches!(status, StatusCode::NOT_FOUND | StatusCode::GONE)
        || (matches!(
            status,
            StatusCode::FORBIDDEN | StatusCode::UNPROCESSABLE_ENTITY
        ) && message.to_ascii_lowercase().contains("locked"))
}

fn holds(comments: &Value, me: &str, marker: &str) -> bool {
    comments.as_array().is_some_and(|comments| {
        comments.iter().any(|comment| {
            comment["user"]["login"]
                .as_str()
                .is_some_and(|login| login.eq_ignore_ascii_case(me))
                && comment["body"]
                    .as_str()
                    .is_some_and(|body| body.trim_end().ends_with(marker))
        })
    })
}

fn page_of(url: &str) -> Option<u64> {
    let url = Url::parse(url).ok()?;
    let page = url.query_pairs().find(|(key, _)| key == "page")?.1;
    page.parse().ok()
}

/// The comment: the text, the link to the Ticket's page for a message that
/// carries one, and the marker, which stays last for a repeat to find.
fn body(message: &OutboundMessage, links: &TicketLinks) -> String {
    let limit = match message.kind {
        MessageKind::Result => RESULT_CHARS,
        MessageKind::Acknowledgement | MessageKind::Attention => SHORT_CHARS,
    };
    let page = links.page(message);
    format!(
        "{}\n\n{}",
        shape(&message.text, limit, page.as_deref()),
        marker(&message.id)
    )
}

/// Cut to `limit` characters with a note that says so, and quiet every
/// mention, so text from a worker cannot notify anyone. `page`, the Ticket's
/// page, is linked after the text, and named as where the rest is when the
/// text is cut.
fn shape(text: &str, limit: usize, page: Option<&str>) -> String {
    let text = text.trim();
    let total = text.chars().count();
    let (kept, cut) = if total <= limit {
        (text, false)
    } else {
        let end = text
            .char_indices()
            .nth(limit)
            .map_or(text.len(), |(at, _)| at);
        let cut = &text[..end];
        // End on a whole word when one ends near the limit.
        let kept = match cut.rfind(char::is_whitespace) {
            Some(at) if at >= cut.len() * 4 / 5 => &cut[..at],
            _ => cut,
        };
        (kept.trim_end(), true)
    };
    let (mut shaped, open_fence) = quiet(kept);
    if cut {
        shaped.push_str(" …");
    }
    // A cut can end inside a code block, which would swallow the note and
    // show the marker.
    if let Some(fence) = open_fence {
        shaped.push('\n');
        shaped.push_str(&fence);
    }
    match (cut, page) {
        (true, Some(page)) => {
            shaped.push_str(&format!("\n\n…the rest is on the [task page]({page})"));
        }
        (true, None) => {
            let shown = kept.chars().count();
            shaped.push_str(&format!(
                "\n\n_[Cut short: {shown} of {total} characters shown.]_"
            ));
        }
        (false, Some(page)) => shaped.push_str(&format!("\n\n[Task details]({page})")),
        (false, None) => {}
    }
    shaped
}

/// Wraps `@name` and `@org/team` outside code in backticks. Returns the
/// fence of a code block left open.
fn quiet(text: &str) -> (String, Option<String>) {
    let mut out = String::with_capacity(text.len());
    let mut fence: Option<String> = None;
    for line in text.split_inclusive('\n') {
        let run = fence_run(line);
        match (&fence, run) {
            (None, Some(run)) => {
                fence = Some(run);
                out.push_str(line);
            }
            (Some(open), Some(_)) if closes(line, open) => {
                fence = None;
                out.push_str(line);
            }
            (Some(_), _) => out.push_str(line),
            (None, None) => quiet_line(line, &mut out),
        }
    }
    (out, fence)
}

fn quiet_line(line: &str, out: &mut String) {
    let chars: Vec<char> = line.chars().collect();
    let mut at = 0;
    let mut before: Option<char> = None;
    while at < chars.len() {
        let c = chars[at];
        if c == '`' {
            let run = chars[at..].iter().take_while(|c| **c == '`').count();
            let close = (at + run..chars.len()).find(|&i| {
                chars[i..].iter().take_while(|c| **c == '`').count() == run
                    && (i == 0 || chars[i - 1] != '`')
            });
            // An unclosed run is literal text, and what follows is not code.
            let end = close.map_or(at + run, |i| i + run);
            out.extend(&chars[at..end]);
            at = end;
            before = Some('`');
            continue;
        }
        let starts = c == '@'
            && !before.is_some_and(char::is_alphanumeric)
            && chars.get(at + 1).is_some_and(char::is_ascii_alphanumeric)
            && !in_url(&chars[..at]);
        if !starts {
            out.push(c);
            before = Some(c);
            at += 1;
            continue;
        }
        let mut end = at + 1;
        while chars
            .get(end)
            .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '-')
        {
            end += 1;
        }
        if chars.get(end) == Some(&'/')
            && chars.get(end + 1).is_some_and(char::is_ascii_alphanumeric)
        {
            end += 1;
            while chars
                .get(end)
                .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            {
                end += 1;
            }
        }
        out.push('`');
        out.extend(&chars[at..end]);
        out.push('`');
        at = end;
        before = Some('`');
    }
}

/// Whether the word that ends `before` is a URL, where GitHub links no
/// mention and backticks would break the link.
fn in_url(before: &[char]) -> bool {
    let start = before
        .iter()
        .rposition(|c| c.is_whitespace())
        .map_or(0, |at| at + 1);
    let word: String = before[start..].iter().collect();
    word.contains("://")
}

#[cfg(test)]
mod tests {
    use super::{quiet, shape};

    #[test]
    fn mentions_outside_code_are_quieted() {
        let (text, open) = quiet(
            "Done, @alice and @acme/core. Mail bob@example.com.\nSee `@keep` and ``a ` @keep``.\n\
             ```rust\n@Override fn x() {}\n```\nA stray ` then @carol.\n``` not a fence ``` @dan\n\
             See https://www.npmjs.com/package/@types/node.\n",
        );
        assert_eq!(
            text,
            "Done, `@alice` and `@acme/core`. Mail bob@example.com.\nSee `@keep` and ``a ` @keep``.\n\
             ```rust\n@Override fn x() {}\n```\nA stray ` then `@carol`.\n``` not a fence ``` `@dan`\n\
             See https://www.npmjs.com/package/@types/node.\n"
        );
        assert_eq!(open, None);
        assert_eq!(quiet("~~~~\n@x\n~~~\n@y").1.as_deref(), Some("~~~~"));
    }

    #[test]
    fn a_long_text_is_cut_on_a_word_and_an_open_code_block_is_closed() {
        let long = format!("Report\n```\n{}", "line of code\n".repeat(50));
        let shaped = shape(&long, 100, None);
        assert!(shaped.starts_with("Report\n```\nline of code"), "{shaped}");
        assert!(
            shaped.ends_with(" …\n```\n\n_[Cut short: 96 of 660 characters shown.]_"),
            "{shaped}"
        );
        assert_eq!(shape("  short  ", 100, None), "short");
        assert!(shape(&"x".repeat(20), 10, None).starts_with("xxxxxxxxxx …"));
    }

    #[test]
    fn a_page_is_linked_after_the_text_and_names_where_the_rest_is_when_the_text_is_cut() {
        let page = "https://agent.example.com/tickets/tk_1";
        assert_eq!(
            shape(
                "  ## Done\n\n| a | b |\n|---|---|\n| 1 | 2 |  ",
                100,
                Some(page)
            ),
            format!("## Done\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n[Task details]({page})")
        );
        let long = format!("Report\n```\n{}", "line of code\n".repeat(50));
        let shaped = shape(&long, 100, Some(page));
        assert!(
            shaped.ends_with(&format!(
                " …\n```\n\n…the rest is on the [task page]({page})"
            )),
            "{shaped}"
        );
        assert!(!shaped.contains("Cut short") && !shaped.contains("Task details"));
    }
}
