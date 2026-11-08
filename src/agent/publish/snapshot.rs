//! What `agent publish` uploads: the shapes of `tickets/index.json`,
//! `tickets/<id>.json` and `notes/index.json`, and the pure pieces that fill
//! them. Nothing here reads the Store; [`super::source`] does, and hands over
//! what it read.
//!
//! The dashboard (`web/agent-dashboard/lib/types.ts`) reads these shapes, so
//! a change here is a change there. [`FORMAT`] and [`NOTES_FORMAT`] count
//! them.
//!
//! A snapshot holds nothing that changes while the Ticket does not: no
//! "age" and no "generated at" inside a ticket's snapshot, so an unchanged
//! Ticket hashes to the same bytes and is not uploaded again. The dashboard
//! computes an open Ticket's age from `created_ms`; an ended Ticket's
//! `duration_ms` is fixed. The one exception is the index's `generated_at`,
//! which is how the dashboard sees that the publisher is alive; the hash that
//! decides whether to upload leaves it out.

use crate::agent::utc;
use serde::Serialize;
use solos_agent_core::model::{
    Actor, ActorKind, Approval, AssignmentState, Decision, DecisionKind, Ms, Report, ReportStatus,
    Ticket, TicketOutcome, Trigger,
};

/// The version of the snapshots' shape.
pub const FORMAT: u32 = 1;
/// The version of `notes/index.json`, which the dashboard reads too.
pub const NOTES_FORMAT: u32 = 1;

/// A longer message is cut here and says so. A blob this size still reads
/// on a phone; one far larger is not a conversation.
pub const MAX_TEXT_CHARS: usize = 100_000;
/// The latest this many Reports of an Assignment are kept.
pub const MAX_REPORTS: usize = 20;
/// The latest this many Decisions on web Threads `notes/index.json` keeps.
pub const NOTES_IN_INDEX: usize = 50;

#[derive(Clone, Debug, Serialize)]
pub struct Person {
    pub id: String,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Person {
    pub fn of(actor: &Actor) -> Self {
        Self {
            id: actor.id.clone(),
            kind: match actor.kind {
                ActorKind::Person => "person",
                ActorKind::Bot => "bot",
                ActorKind::System => "system",
            },
            name: actor.name.clone().filter(|name| !name.trim().is_empty()),
        }
    }

    /// What a page calls them: their name, or their id.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }
}

/// `tickets/index.json`: the open Tickets first, then the latest ended.
#[derive(Clone, Debug, Serialize)]
pub struct Index {
    pub v: u32,
    /// When the publisher last wrote this file. Not part of the hash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    pub open: usize,
    pub tickets: Vec<IndexRow>,
}

#[derive(Clone, Debug, Serialize)]
pub struct IndexRow {
    pub id: String,
    pub title: String,
    pub stage: &'static str,
    pub open: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<&'static str>,
    pub created_ms: Ms,
    pub created_at: String,
    pub updated_ms: Ms,
    pub updated_at: String,
    /// How long an ended Ticket took. An open one's age is the dashboard's
    /// to compute from `created_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<Ms>,
    pub source: String,
    /// The conversation the request came from, where its address says. The
    /// dashboard matches a web chat thread to its ticket through this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    pub asker: Person,
    pub spent_microusd: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_microusd: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignment: Option<AssignmentBrief>,
    /// The question or approval waiting for a person, clipped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AssignmentBrief {
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<&'static str>,
    pub reports: usize,
}

/// `tickets/<id>.json`.
#[derive(Clone, Debug, Serialize)]
pub struct TicketSnapshot {
    pub v: u32,
    pub id: String,
    pub title: String,
    pub stage: &'static str,
    pub open: bool,
    pub created_ms: Ms,
    pub created_at: String,
    pub updated_ms: Ms,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended: Option<EndedView>,
    /// What the Router understood the work to be, fixed at accept.
    pub scope: String,
    pub plan: PlanView,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// What the finished work owes: an answer, a change or a pull request.
    pub deliverable: &'static str,
    pub approval: ApprovalView,
    pub budget_microusd: Option<u64>,
    pub spent_microusd: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attention: Option<String>,
    pub source: String,
    /// The conversation the request came from, where its address says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    pub requester: Person,
    /// The message that started it. It is not repeated in `conversation`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<Turn>,
    /// What came after the request, in order: the Thread's later messages
    /// the Ticket received, and every reply the agent sent.
    pub conversation: Vec<Turn>,
    pub decisions: Vec<DecisionView>,
    pub assignments: Vec<AssignmentView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub browser: Option<BrowserView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EndedView {
    pub at_ms: Ms,
    pub at: String,
    pub by: &'static str,
    pub outcome: &'static str,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanView {
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<u16>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApprovalView {
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

/// One message in the conversation.
#[derive(Clone, Debug, Serialize)]
pub struct Turn {
    pub at_ms: Ms,
    pub at: String,
    /// `person`, `bot`, `system`, `router` or `agent`.
    pub from: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub who: Option<String>,
    /// `request`, `message`, `acknowledgement`, `result`, `attention`,
    /// `note`, `approval` or `secret`.
    pub kind: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// Where an agent reply went (`discord`) and whether it was delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Delivery {
    pub delivered: bool,
    pub attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DecisionView {
    pub id: String,
    pub kind: &'static str,
    pub reason: String,
    pub at_ms: Ms,
    pub at: String,
}

/// `notes/index.json`: the Router's latest Decisions on web Threads, newest
/// last, so the dashboard's chat can say what the agent decided about a
/// message that became no Ticket.
#[derive(Clone, Debug, Serialize)]
pub struct Notes {
    pub v: u32,
    /// When the publisher last wrote this file. Not part of the hash.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    pub notes: Vec<NoteView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct NoteView {
    pub thread: String,
    pub at_ms: Ms,
    /// `accept`, `ignore`, `stop`, `unavailable` or `declined`.
    pub kind: &'static str,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AssignmentView {
    pub id: String,
    pub state: &'static str,
    pub created_ms: Ms,
    pub created_at: String,
    /// When the executor started it, from the event log.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<Ms>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<Ms>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_reason: Option<String>,
    pub report_count: usize,
    /// The latest [`MAX_REPORTS`], oldest first; the last is the last report.
    pub reports: Vec<ReportView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReportView {
    pub seq: u64,
    pub status: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    pub spent_microusd: u64,
    pub at_ms: Ms,
    pub at: String,
}

/// What the Ticket's browsers did, from the event log's trail events: where
/// they went, not what they typed (the chain holds only digests of that) and
/// not the query of any address.
#[derive(Clone, Debug, Default, Serialize)]
pub struct BrowserView {
    /// Steps asked, over every Assignment.
    pub steps: u32,
    pub refused: u32,
    /// Steps with no signed answer.
    pub lost: u32,
    pub closed: u32,
    /// The vault items a step stopped to ask a person about.
    pub needs_approval: Vec<String>,
    /// The latest pages, oldest first.
    pub pages: Vec<PageView>,
    /// Pages before the latest ones.
    pub pages_omitted: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct PageView {
    pub n: u32,
    pub at: String,
    pub action: String,
    pub url: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

pub fn iso(ms: Ms) -> String {
    utc::format(ms)
}

/// `text`, cut at [`MAX_TEXT_CHARS`], and whether it was cut.
pub fn clip_text(text: &str) -> (String, bool) {
    match text.char_indices().nth(MAX_TEXT_CHARS) {
        Some((end, _)) => (
            format!("{}\n\n[cut: the message is longer]", &text[..end]),
            true,
        ),
        None => (text.to_owned(), false),
    }
}

/// `text` as one line of at most `max` characters.
pub fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(max) {
        Some((end, _)) => format!("{}...", &line[..end]),
        None => line,
    }
}

pub fn report_status(status: ReportStatus) -> &'static str {
    match status {
        ReportStatus::Progress => "progress",
        ReportStatus::Waiting => "waiting",
        ReportStatus::Ready => "ready",
        ReportStatus::Blocked => "blocked",
        ReportStatus::Failed => "failed",
        ReportStatus::Dropped => "dropped",
    }
}

pub fn assignment_state(state: AssignmentState) -> &'static str {
    match state {
        AssignmentState::Pending => "pending",
        AssignmentState::Running => "running",
        AssignmentState::Ended => "ended",
    }
}

pub fn outcome(outcome: TicketOutcome) -> &'static str {
    match outcome {
        TicketOutcome::Completed => "completed",
        TicketOutcome::Dropped => "dropped",
        TicketOutcome::Stopped => "stopped",
    }
}

pub fn decision_kind(kind: DecisionKind) -> &'static str {
    match kind {
        DecisionKind::Accept => "accept",
        DecisionKind::Ignore => "ignore",
        DecisionKind::Stop => "stop",
        DecisionKind::Unavailable => "unavailable",
        DecisionKind::Declined => "declined",
    }
}

pub fn report_view(report: &Report) -> ReportView {
    let (text, truncated) = clip_text(&report.text);
    ReportView {
        seq: report.seq,
        status: report_status(report.status),
        text,
        truncated,
        question: report.question.clone(),
        spent_microusd: report.spent_microusd,
        at_ms: report.at_ms,
        at: iso(report.at_ms),
    }
}

pub fn decision_view(decision: &Decision) -> DecisionView {
    DecisionView {
        id: decision.id.to_string(),
        kind: decision_kind(decision.kind),
        reason: decision.reason.clone(),
        at_ms: decision.at_ms,
        at: iso(decision.at_ms),
    }
}

pub fn note_view(decision: &Decision) -> NoteView {
    NoteView {
        thread: decision.thread.to_string(),
        at_ms: decision.at_ms,
        kind: decision_kind(decision.kind),
        reason: decision.reason.clone(),
    }
}

pub fn approval_view(approval: &Approval) -> ApprovalView {
    match approval {
        Approval::NotNeeded => ApprovalView {
            state: "not_needed",
            by: None,
            at: None,
        },
        Approval::Waiting => ApprovalView {
            state: "waiting",
            by: None,
            at: None,
        },
        Approval::Granted { by, at_ms } => ApprovalView {
            state: "granted",
            by: Some(Person::of(by).label().to_owned()),
            at: Some(iso(*at_ms)),
        },
    }
}

/// The last time anything in `ticket` itself moved.
pub fn touched_ms(ticket: &Ticket) -> Ms {
    let mut latest = ticket.created_ms;
    let mut see = |at: Ms| latest = latest.max(at);
    for input in &ticket.inputs {
        see(input.at_ms);
    }
    for assignment in &ticket.assignments {
        see(assignment.created_ms);
        for report in &assignment.reports {
            see(report.at_ms);
        }
    }
    if let Approval::Granted { at_ms, .. } = &ticket.approval {
        see(*at_ms);
    }
    if let Some(ended) = &ticket.ended {
        see(ended.at_ms);
    }
    latest
}

/// `https://host/path` of an address a browser stopped at: no login, query
/// or fragment, which carry tokens. `None` when it is not an address.
pub fn scrub_url(url: &str) -> Option<String> {
    let mut parsed = reqwest::Url::parse(url).ok()?;
    parsed.set_query(None);
    parsed.set_fragment(None);
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    parsed.host_str()?;
    Some(parsed.to_string())
}

/// The conversation a Trigger came from, as a link, when its Source's
/// addresses make one: a Discord message, a GitHub issue or pull request on
/// github.com, or a web chat thread whose address the bridge wrote. Slack's
/// links need the workspace's name, which the agent does not hold.
pub fn source_url(trigger: &Trigger) -> Option<String> {
    match trigger.source.as_str() {
        "web" => {
            // The bridge writes the dashboard's address in `meta.url`; keep
            // only its origin and path, and take the thread from the
            // Trigger's own key, so no query or fragment it carried survives.
            let raw = trigger.meta.get("url")?.as_str()?;
            let thread = trigger.threads.first()?.as_str().strip_prefix("web:")?;
            let shaped = thread.len() == 35
                && thread.starts_with("wc_")
                && thread[3..].bytes().all(|b| b.is_ascii_hexdigit());
            if !shaped {
                return None;
            }
            let mut url = reqwest::Url::parse(&scrub_url(raw)?).ok()?;
            url.query_pairs_mut().append_pair("t", thread);
            Some(url.to_string())
        }
        "discord" => {
            // An id the Source did not write, or none at all: absent is
            // fine for a guild (a direct message) and the message, but one
            // that is there and is not digits makes no link.
            let id = |key: &str| -> Option<Option<&str>> {
                match trigger.meta.get(key).and_then(|value| value.as_str()) {
                    None => Some(None),
                    Some(id) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) => {
                        Some(Some(id))
                    }
                    Some(_) => None,
                }
            };
            let channel = id("channel")??;
            let place = id("guild")?.unwrap_or("@me");
            Some(match id("message")? {
                Some(message) => {
                    format!("https://discord.com/channels/{place}/{channel}/{message}")
                }
                None => format!("https://discord.com/channels/{place}/{channel}"),
            })
        }
        "github" => {
            let key = trigger.threads.first()?.as_str();
            let (repo, number) = key.strip_prefix("github:")?.split_once('#')?;
            let valid = crate::agent::github::valid_repo(repo)
                && !number.is_empty()
                && number.bytes().all(|b| b.is_ascii_digit());
            // github.com sends /issues/N to the pull request when N is one.
            valid.then(|| format!("https://github.com/{repo}/issues/{number}"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_TEXT_CHARS, clip_text, one_line, scrub_url, source_url};
    use serde_json::json;
    use solos_agent_core::model::{Actor, Addressed, ThreadKey, Trigger, TriggerId};

    fn trigger(source: &str, thread: &str, meta: serde_json::Value) -> Trigger {
        Trigger {
            id: TriggerId::new("t1"),
            source: source.into(),
            at_ms: 1,
            actor: Actor::person("alice"),
            addressed: Addressed::Mention,
            text: "hi".into(),
            threads: vec![ThreadKey::new(thread)],
            reply_to: Vec::new(),
            meta,
        }
    }

    #[test]
    fn a_discord_message_links_to_its_place_and_a_direct_message_to_me() {
        let guild = trigger(
            "discord",
            "discord:20",
            json!({"guild": "10", "channel": "20", "message": "30"}),
        );
        assert_eq!(
            source_url(&guild).as_deref(),
            Some("https://discord.com/channels/10/20/30")
        );
        let direct = trigger(
            "discord",
            "discord:20",
            json!({"channel": "20", "message": "30"}),
        );
        assert_eq!(
            source_url(&direct).as_deref(),
            Some("https://discord.com/channels/@me/20/30")
        );
        let hostile = trigger(
            "discord",
            "discord:20",
            json!({"guild": "1/../x", "channel": "20", "message": "3 0"}),
        );
        assert_eq!(source_url(&hostile), None, "only digits make an id");
        let odd_message = trigger(
            "discord",
            "discord:20",
            json!({"guild": "10", "channel": "20", "message": "3 0"}),
        );
        assert_eq!(source_url(&odd_message), None);
        let channel_only = trigger(
            "discord",
            "discord:20",
            json!({"guild": "10", "channel": "20"}),
        );
        assert_eq!(
            source_url(&channel_only).as_deref(),
            Some("https://discord.com/channels/10/20")
        );
        assert_eq!(
            source_url(&trigger("discord", "discord:20", json!({}))),
            None
        );
    }

    #[test]
    fn a_github_thread_links_to_its_issue_and_other_sources_to_nothing() {
        let issue = trigger("github", "github:acme/app#42", json!({}));
        assert_eq!(
            source_url(&issue).as_deref(),
            Some("https://github.com/acme/app/issues/42")
        );
        for bad in [
            "github:acme#42",
            "github:acme/app#x",
            "github:../app#1",
            "github:a/b",
        ] {
            assert_eq!(
                source_url(&trigger("github", bad, json!({}))),
                None,
                "{bad}"
            );
        }
        assert_eq!(
            source_url(&trigger("slack", "slack:C1:123.4", json!({}))),
            None
        );
    }

    #[test]
    fn a_web_thread_links_to_its_conversation_and_keeps_only_the_thread_query() {
        let thread = format!("wc_{}", "ab".repeat(16));
        let web = trigger(
            "web",
            &format!("web:{thread}"),
            json!({
                "thread": thread,
                "url": format!("https://agent.guibibeau.com/?t={thread}"),
            }),
        );
        assert_eq!(
            source_url(&web),
            Some(format!("https://agent.guibibeau.com/?t={thread}"))
        );

        // A login, another query and a fragment do not survive; the thread
        // in the Trigger's own key fills the query the dashboard matches on.
        let hostile = trigger(
            "web",
            &format!("web:{thread}"),
            json!({"url": "https://user:pw@agent.guibibeau.com/tickets/1?token=secret&t=evil#frag"}),
        );
        assert_eq!(
            source_url(&hostile),
            Some(format!("https://agent.guibibeau.com/tickets/1?t={thread}"))
        );

        for (key, meta) in [
            ("web:not-a-thread", json!({"url": "https://a.example/"})),
            (
                &format!("web:wc_{}", "zz".repeat(16)),
                json!({"url": "https://a.example/"}),
            ),
            (&format!("web:{thread}"), json!({})),
            (&format!("web:{thread}"), json!("https://a.example/")),
            (&format!("web:{thread}"), json!({"url": "not a url"})),
            ("github:acme/app#1", json!({"url": "https://a.example/"})),
        ] {
            assert_eq!(source_url(&trigger("web", key, meta)), None, "{key}");
        }
    }

    #[test]
    fn a_page_address_loses_its_login_query_and_fragment() {
        assert_eq!(
            scrub_url("https://u:p@example.com/a/b?token=secret#frag").as_deref(),
            Some("https://example.com/a/b")
        );
        assert_eq!(scrub_url("not a url"), None);
        assert_eq!(scrub_url("data:text/plain,hi"), None);
    }

    #[test]
    fn long_text_is_cut_on_a_character_boundary_and_says_so() {
        let (short, cut) = clip_text("hello");
        assert_eq!((short.as_str(), cut), ("hello", false));
        let long = "é".repeat(MAX_TEXT_CHARS + 5);
        let (text, cut) = clip_text(&long);
        assert!(cut);
        assert!(text.starts_with(&"é".repeat(MAX_TEXT_CHARS)));
        assert!(text.ends_with("longer]"));
        assert_eq!(one_line("a  b\n c", 10), "a b c");
        assert_eq!(one_line("abcdef", 3), "abc...");
    }
}
