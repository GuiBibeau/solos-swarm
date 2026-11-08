//! The `web` section: where the agent's dashboard lives and how its tickets
//! get there. The dashboard is a separate web app (`web/agent-dashboard`) on
//! Vercel; the agent's box accepts no inbound connections, so `agent publish`
//! pushes snapshots of the Store to a private Vercel Blob store and the app
//! reads them from there.

use anyhow::{Result, ensure};
use serde::Deserialize;
use solos_agent_core::model::TicketId;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// The dashboard's own address and what publishes to it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    /// The dashboard's origin, `https://` and no trailing slash. A ticket's
    /// page is [`WebConfig::ticket_url`].
    pub public_url: String,
    /// Without it, `agent publish` has nothing to do (links can still be
    /// made, by whatever serves `public_url`).
    #[serde(default)]
    pub publish: Option<PublishConfig>,
    /// The dashboard's chat inbox, which `agent publish` reads and spools as
    /// Trigger lines. Needs `publish`: the inbox is read with the same Blob
    /// token. Without it, nothing is read.
    #[serde(default)]
    pub chat: Option<ChatConfig>,
}

/// `web.chat`: where the hosted dashboard writes what people type, and who
/// may become a Trigger.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatConfig {
    /// The inbox's pathname in the private Blob store, `web/inbox.json` by
    /// the dashboard's contract.
    pub inbox: String,
    /// The authors whose messages may become Triggers, by the email the
    /// dashboard signs them in with. Anyone else's message is logged without
    /// its text and skipped. This is not the same list as `policy.people`:
    /// the agent's Policy checks `web:<author>` again.
    pub people: BTreeSet<String>,
    /// Where Trigger JSON lines are appended. Defaults to
    /// `<state>/web/triggers.jsonl`; a path here is relative to the config's
    /// directory, like every other path in it.
    #[serde(default)]
    pub spool: Option<PathBuf>,
}

/// `web.publish`: what `agent publish` uploads with.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishConfig {
    /// The Blob store's read-write token (`vercel_blob_rw_...`), a one-use
    /// token file like every other: read once and deleted. Relative to the
    /// config's directory.
    pub token_file: PathBuf,
    /// How often the Store is looked at. A look that finds the Store
    /// unchanged costs one pragma, and an upload happens only for a snapshot
    /// whose content changed.
    #[serde(default = "default_every_secs")]
    pub every_secs: u64,
    /// The index is uploaded again after this long even if nothing changed,
    /// so the dashboard can tell a quiet agent from a dead publisher.
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
    /// Test seam: the Blob API's root, `https://vercel.com/api/blob` by
    /// default. Plain `http` is accepted for a loopback address only.
    #[serde(default = "default_api_url")]
    pub api_url: String,
}

/// What the Blob API is, unless a test points `api_url` elsewhere.
pub const DEFAULT_API_URL: &str = "https://vercel.com/api/blob";

fn default_every_secs() -> u64 {
    5
}

fn default_heartbeat_secs() -> u64 {
    300
}

fn default_api_url() -> String {
    DEFAULT_API_URL.to_owned()
}

impl WebConfig {
    /// The page of `ticket` on the dashboard.
    pub fn ticket_url(&self, ticket: &TicketId) -> String {
        format!("{}/tickets/{ticket}", self.public_url)
    }

    pub fn validate(&self) -> Result<()> {
        check_public_url(&self.public_url)?;
        if let Some(publish) = &self.publish {
            publish.validate()?;
        }
        if let Some(chat) = &self.chat {
            ensure!(
                self.publish.is_some(),
                "web.chat needs web.publish: the inbox is read with the same Blob token"
            );
            chat.validate()?;
        }
        Ok(())
    }
}

impl ChatConfig {
    fn validate(&self) -> Result<()> {
        crate::agent::publish::check_pathname(&self.inbox)
            .map_err(|err| anyhow::anyhow!("web.chat.inbox is not a Blob pathname: {err:#}"))?;
        ensure!(
            !self.people.is_empty(),
            "web.chat.people names nobody, so no message would ever become a Trigger"
        );
        for person in &self.people {
            ensure!(
                !person.is_empty()
                    && person.contains('@')
                    && !person.chars().any(char::is_whitespace),
                "web.chat.people must hold the authors' email addresses: {person:?}"
            );
        }
        Ok(())
    }
}

impl PublishConfig {
    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=3_600).contains(&self.every_secs),
            "web.publish.every_secs must be between 1 and 3600"
        );
        ensure!(
            (self.every_secs..=86_400).contains(&self.heartbeat_secs),
            "web.publish.heartbeat_secs must be at least every_secs and at most 86400"
        );
        let url = reqwest::Url::parse(&self.api_url)
            .map_err(|err| anyhow::anyhow!("web.publish.api_url is not a URL: {err}"))?;
        let loopback = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && loopback),
            "web.publish.api_url must be https (http only for a loopback address)"
        );
        ensure!(
            !self.api_url.ends_with('/') && url.query().is_none() && url.fragment().is_none(),
            "web.publish.api_url must have no trailing slash, query or fragment"
        );
        Ok(())
    }
}

fn check_public_url(text: &str) -> Result<()> {
    ensure!(
        !text.ends_with('/'),
        "web.public_url must have no trailing slash: {text:?}"
    );
    let url = reqwest::Url::parse(text)
        .map_err(|err| anyhow::anyhow!("web.public_url is not a URL: {err}"))?;
    ensure!(
        url.scheme() == "https",
        "web.public_url must be https: {text:?}"
    );
    ensure!(
        url.host_str().is_some_and(|host| !host.is_empty()),
        "web.public_url has no host: {text:?}"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "web.public_url must not carry a login: {text:?}"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "web.public_url must have no query or fragment: {text:?}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{PublishConfig, WebConfig};
    use solos_agent_core::model::TicketId;
    use std::path::PathBuf;

    fn web(yaml: &str) -> anyhow::Result<WebConfig> {
        let web: WebConfig = serde_yaml::from_str(yaml)?;
        web.validate()?;
        Ok(web)
    }

    #[test]
    fn the_documented_section_loads_with_its_defaults() {
        let config = web("
public_url: https://agent.guibibeau.com
publish:
  token_file: /run/user/1000/solos-agent/vercel-blob-token
")
        .unwrap();
        assert_eq!(config.public_url, "https://agent.guibibeau.com");
        assert_eq!(
            config.publish,
            Some(PublishConfig {
                token_file: PathBuf::from("/run/user/1000/solos-agent/vercel-blob-token"),
                every_secs: 5,
                heartbeat_secs: 300,
                api_url: "https://vercel.com/api/blob".into(),
            })
        );
        assert_eq!(
            config.ticket_url(&TicketId::new("tk_0123456789abcdef")),
            "https://agent.guibibeau.com/tickets/tk_0123456789abcdef"
        );
    }

    #[test]
    fn links_alone_need_no_publisher() {
        let config = web("public_url: https://agent.guibibeau.com").unwrap();
        assert_eq!(config.publish, None);
    }

    #[test]
    fn a_public_url_is_an_https_origin_with_no_trailing_slash() {
        for bad in [
            "http://agent.guibibeau.com",
            "https://agent.guibibeau.com/",
            "https://agent.guibibeau.com/?x=1",
            "https://agent.guibibeau.com#top",
            "https://user:pw@agent.guibibeau.com",
            "agent.guibibeau.com",
            "https://",
            "",
        ] {
            let refused = web(&format!("public_url: '{bad}'"));
            assert!(refused.is_err(), "{bad:?} was accepted");
        }
        assert!(web("public_url: https://example.com/agent").is_ok());
    }

    #[test]
    fn the_publisher_is_checked_and_a_typo_is_an_error() {
        let with = |publish: &str| {
            web(&format!(
                "public_url: https://a.example\npublish:\n  token_file: t\n{publish}"
            ))
        };
        assert!(with("  every_secs: 0").is_err());
        assert!(with("  every_secs: 3601").is_err());
        assert!(with("  every_secs: 30\n  heartbeat_secs: 10").is_err());
        assert!(with("  every_secs: 2\n  heartbeat_secs: 60").is_ok());
        assert!(with("  api_url: http://blob.example/api").is_err());
        assert!(with("  api_url: http://127.0.0.1:8080").is_ok());
        assert!(with("  api_url: https://blob.example/api/").is_err());
        assert!(
            with("  token_fil: x").is_err(),
            "unknown fields are refused"
        );
        assert!(web("public_url: https://a.example\nbind: 0.0.0.0:80").is_err());
        assert!(
            web("publish: {token_file: t}").is_err(),
            "public_url is required"
        );
    }

    #[test]
    fn the_chat_inbox_loads_beside_the_publisher() {
        let config = web("
public_url: https://agent.guibibeau.com
publish:
  token_file: blob.token
chat:
  inbox: web/inbox.json
  people: [g.bibeaulaviolette@gmail.com]
")
        .unwrap();
        let chat = config.chat.unwrap();
        assert_eq!(chat.inbox, "web/inbox.json");
        assert_eq!(
            chat.people.iter().collect::<Vec<_>>(),
            ["g.bibeaulaviolette@gmail.com"]
        );
        assert_eq!(chat.spool, None, "the state directory fills the default");
    }

    #[test]
    fn the_chat_inbox_needs_a_publisher_people_and_a_pathname() {
        let with = |chat: &str| {
            web(&format!(
                "public_url: https://a.example\npublish:\n  token_file: t\nchat:\n{chat}"
            ))
        };
        assert!(with("  inbox: web/inbox.json\n  people: [a@b]").is_ok());
        assert!(
            with("  inbox: web/inbox.json\n  people: [a@b]\n  spool: state/web/t.jsonl").is_ok()
        );
        // No publisher: no token to read the inbox with.
        assert!(
            web("public_url: https://a.example\nchat: {inbox: web/inbox.json, people: [a@b]}")
                .is_err()
        );
        assert!(with("  people: [a@b]").is_err(), "the inbox is required");
        assert!(
            with("  inbox: web/inbox.json").is_err(),
            "people are required"
        );
        assert!(
            with("  inbox: web/inbox.json\n  people: []").is_err(),
            "an empty list would never Trigger"
        );
        for bad in ["", "/web/inbox.json", "web//inbox.json"] {
            assert!(
                with(&format!("  inbox: '{bad}'\n  people: [a@b]")).is_err(),
                "{bad:?} was accepted"
            );
        }
        assert!(
            with("  inbox: web/inbox.json\n  people: [not-an-email]").is_err(),
            "people are emails"
        );
        assert!(
            with("  inbox: web/inbox.json\n  people: [a@b]\n  inbx: x").is_err(),
            "unknown fields are refused"
        );
    }
}
