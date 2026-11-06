//! Links from replies to a Ticket's page on the agent's web dashboard
//! (`web.public_url` in agent.yaml). The host adds them, never a model, so
//! one cannot make a reply point somewhere else.
//!
//! Which messages carry one: a Result, where the answer is read, and an
//! Attention, where a person must decide and the page holds what they need to
//! decide with. The Acknowledgement that opens a Ticket and the answers to a
//! button click do not: a Result follows within moments and links to the same
//! page, so a link on every message of a thread would be noise.

use anyhow::{Result, ensure};
use reqwest::Url;
use solos_agent_core::model::{MessageKind, OutboundMessage, TicketId};

/// The longest `public_url`, a bound no real address nears.
const MAX_URL_CHARS: usize = 200;

/// Where a Ticket's page is, when the agent has a dashboard.
#[derive(Clone, Debug, Default)]
pub struct TicketLinks {
    base: Option<Url>,
}

impl TicketLinks {
    /// No dashboard: no message carries a link.
    pub fn none() -> Self {
        Self::default()
    }

    /// `public_url` as agent.yaml spells it: `https://host[/prefix]`.
    pub fn new(public_url: &str) -> Result<Self> {
        Ok(Self {
            base: Some(parse(public_url)?),
        })
    }

    /// The address of the Ticket's page, `{public_url}/tickets/{id}`.
    pub fn ticket_page(&self, ticket: &TicketId) -> Option<String> {
        let mut url = self.base.clone()?;
        url.path_segments_mut()
            .ok()?
            .pop_if_empty()
            .extend(["tickets", ticket.as_str()]);
        // The address goes inside Markdown and mrkdwn link syntax.
        Some(
            url.as_str()
                .replace('(', "%28")
                .replace(')', "%29")
                .replace('|', "%7C"),
        )
    }

    /// The page to link from `message`, when it should carry one.
    pub fn page(&self, message: &OutboundMessage) -> Option<String> {
        match message.kind {
            MessageKind::Result | MessageKind::Attention => {
                self.ticket_page(message.ticket.as_ref()?)
            }
            MessageKind::Acknowledgement => None,
        }
    }
}

fn parse(public_url: &str) -> Result<Url> {
    ensure!(
        !public_url.is_empty() && public_url.chars().count() <= MAX_URL_CHARS,
        "web.public_url must be 1 to {MAX_URL_CHARS} characters"
    );
    ensure!(
        !public_url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "()<>|\"`\\".contains(c)),
        "web.public_url {public_url} holds a space, a control character or one of ()<>|\"`\\"
    );
    ensure!(
        !public_url.ends_with('/'),
        "web.public_url {public_url} must not end with a slash"
    );
    let url = Url::parse(public_url)
        .map_err(|err| anyhow::anyhow!("web.public_url {public_url} is not a URL: {err}"))?;
    ensure!(
        url.scheme() == "https",
        "web.public_url {public_url} must be an https URL: people reach the dashboard through \
         a TLS-terminating proxy such as Cloudflare Access"
    );
    ensure!(
        url.host_str().is_some_and(|host| !host.is_empty())
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "web.public_url {public_url} must be https://host[/path], with no credentials, query or \
         fragment"
    );
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::TicketLinks;
    use solos_agent_core::model::{ChannelRef, MessageId, MessageKind, OutboundMessage, TicketId};
    use std::collections::BTreeMap;

    fn message(kind: MessageKind, ticket: Option<&str>) -> OutboundMessage {
        OutboundMessage {
            id: MessageId::new("ms_1"),
            channel: ChannelRef {
                source: "discord".into(),
                address: BTreeMap::new(),
            },
            kind,
            text: "hi".into(),
            ticket: ticket.map(TicketId::new),
            ask: None,
        }
    }

    #[test]
    fn a_ticket_page_hangs_off_the_public_url() {
        let links = TicketLinks::new("https://agent.example.com").unwrap();
        assert_eq!(
            links.ticket_page(&TicketId::new("tk_0123")).unwrap(),
            "https://agent.example.com/tickets/tk_0123"
        );
        let prefixed = TicketLinks::new("https://example.com:8443/agent").unwrap();
        assert_eq!(
            prefixed.ticket_page(&TicketId::new("tk_0123")).unwrap(),
            "https://example.com:8443/agent/tickets/tk_0123"
        );
        assert_eq!(
            TicketLinks::none().ticket_page(&TicketId::new("tk_1")),
            None
        );
    }

    #[test]
    fn an_id_cannot_break_out_of_the_link_syntax() {
        let links = TicketLinks::new("https://agent.example.com").unwrap();
        let url = links
            .ticket_page(&TicketId::new("a b/../c)](x)|<>"))
            .unwrap();
        assert!(!url.contains([' ', '(', ')', '|', '<', '>']), "{url}");
        assert!(
            url.starts_with("https://agent.example.com/tickets/"),
            "{url}"
        );
    }

    #[test]
    fn results_and_questions_link_and_acknowledgements_and_orphans_do_not() {
        let links = TicketLinks::new("https://agent.example.com").unwrap();
        for (kind, linked) in [
            (MessageKind::Result, true),
            (MessageKind::Attention, true),
            (MessageKind::Acknowledgement, false),
        ] {
            assert_eq!(
                links.page(&message(kind, Some("tk_1"))).is_some(),
                linked,
                "{kind:?}"
            );
        }
        assert_eq!(links.page(&message(MessageKind::Result, None)), None);
        assert_eq!(
            TicketLinks::none().page(&message(MessageKind::Result, Some("tk_1"))),
            None
        );
    }

    #[test]
    fn only_a_plain_https_address_without_a_trailing_slash_is_accepted() {
        for good in [
            "https://agent.example.com",
            "https://agent.example.com:4443",
            // Any https base: a dashboard on Vercel, with or without a path.
            "https://solos-swarm-dashboard.vercel.app",
            "https://solos-swarm-git-main-acme.vercel.app",
            "https://acme.vercel.app/solos",
            "https://example.com/agent",
            "https://127.0.0.1:4180",
        ] {
            TicketLinks::new(good).unwrap_or_else(|err| panic!("{good}: {err}"));
        }
        for bad in [
            "",
            "http://agent.example.com",
            "agent.example.com",
            "https://agent.example.com/",
            "https://example.com/agent/",
            "https://user:pw@agent.example.com",
            "https://agent.example.com?x=1",
            "https://agent.example.com#top",
            "https://agent.example.com/a b",
            "https://agent.example.com/a)(b",
            "https://agent.example.com/<b>",
            "https://",
            "ftp://agent.example.com",
            "javascript:alert(1)",
            &format!("https://{}.example.com", "a".repeat(200)),
        ] {
            assert!(TicketLinks::new(bad).is_err(), "{bad} was accepted");
        }
    }
}
