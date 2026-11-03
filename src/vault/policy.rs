use super::{Admission, Approval, GrantState, ItemInfo, ItemKind, UseContext, joined};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Display};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// `scheme://host[:port]` as the URL parser normalizes it, without the
/// scheme's default port. https only, except on a loopback host.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Origin(String);

impl Origin {
    pub fn of(url: &Url) -> Result<Self, String> {
        let host = url.host_str().ok_or_else(|| format!("{url} has no host"))?;
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback(host)) {
            return Err(format!("{url} is not https"));
        }
        let port = url
            .port()
            .map(|port| format!(":{port}"))
            .unwrap_or_default();
        Ok(Self(format!("{}://{host}{port}", url.scheme())))
    }

    fn authority(&self) -> &str {
        self.0
            .split_once("://")
            .map_or("", |(_, authority)| authority)
    }
}

impl TryFrom<String> for Origin {
    type Error = String;
    fn try_from(text: String) -> Result<Self, String> {
        let url = Url::parse(&text)
            .map_err(|_| format!("{text:?} is not an origin such as https://github.com"))?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(format!(
                "{text:?} is not an origin: give only scheme://host[:port]"
            ));
        }
        Self::of(&url)
    }
}

/// `host` or `host:port`, exactly, with no wildcard. Without a port, the
/// scheme's default.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostSpec(String);

impl TryFrom<String> for HostSpec {
    type Error = String;
    fn try_from(text: String) -> Result<Self, String> {
        let bad = || format!("{text:?} is not a host such as api.openai.com or localhost:8080");
        if text.contains(['/', '@', '*', '?', '#', '\\']) {
            return Err(bad());
        }
        let url = Url::parse(&format!("https://{text}")).map_err(|_| bad())?;
        let host = url
            .host_str()
            .filter(|host| !host.is_empty())
            .ok_or_else(bad)?;
        let port = url
            .port()
            .map(|port| format!(":{port}"))
            .unwrap_or_default();
        Ok(Self(format!("{host}{port}")))
    }
}

fn loopback(host: &str) -> bool {
    let ipv6 = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .and_then(|host| host.parse::<Ipv6Addr>().ok());
    host == "localhost"
        || host.parse::<Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
        || ipv6.is_some_and(|ip| ip.is_loopback())
}

pub fn site_listed(sites: &[Origin], page: &Origin) -> bool {
    sites.contains(page)
}

/// `url` is https (or http to a loopback host), carries no userinfo, and
/// its host and port are one of `hosts`.
pub fn key_admits(hosts: &[HostSpec], url: &Url) -> bool {
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    Origin::of(url).is_ok_and(|origin| hosts.iter().any(|host| host.0 == origin.authority()))
}

/// Where a Ticket stands with an item it may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    /// The item needs no person's approval.
    None,
    /// Its first use on the Ticket asks a person.
    Needed,
    /// A person was asked and has not answered.
    Waiting,
    Granted,
    Denied,
    /// It needs a person's approval, and this agent takes none.
    NoApprover,
}

pub fn standing(item: &ItemInfo, context: &UseContext) -> Standing {
    if item.approval == Approval::None {
        return Standing::None;
    }
    match context.grants.get(&item.name) {
        Some(GrantState::Granted) => Standing::Granted,
        Some(GrantState::Denied) => Standing::Denied,
        _ if !context.takes_approvals => Standing::NoApprover,
        Some(GrantState::Waiting) => Standing::Waiting,
        None => Standing::Needed,
    }
}

pub fn for_requester(item: &ItemInfo, requester: &str) -> bool {
    item.users.is_empty() || item.users.contains(requester)
}

pub fn use_rule(item: &ItemInfo, context: &UseContext) -> Admission<()> {
    let name = &item.name;
    if !for_requester(item, &context.requester) {
        return Admission::Refused(format!("{name} is not for this ticket's requester"));
    }
    match standing(item, context) {
        Standing::None | Standing::Granted => Admission::Allowed(()),
        Standing::Denied => Admission::Refused(format!("a person denied {name} for this ticket")),
        Standing::NoApprover => Admission::Refused(format!(
            "{name} needs a person's approval, and this agent takes none"
        )),
        Standing::Needed | Standing::Waiting => Admission::NeedsApproval {
            secret: format!("{}/{name}", context.environment),
            label: label(item),
            answerable_by: item.users.iter().cloned().collect(),
        },
    }
}

fn label(item: &ItemInfo) -> String {
    match &item.kind {
        ItemKind::Login {
            sites, username, ..
        } => format!(
            "sign in to {} as {username} (login {})",
            joined(sites, " or "),
            item.name
        ),
        ItemKind::Key { hosts, .. } => format!(
            "send the API key {} to {}",
            item.name,
            joined(hosts, " or ")
        ),
    }
}

super::text_newtype!(Origin, HostSpec);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::SecretName;
    use std::collections::BTreeMap;

    fn origin(text: &str) -> Origin {
        text.parse().unwrap()
    }

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn origins_and_hosts_parse_to_one_spelling_or_are_refused() {
        for (text, parsed) in [
            ("https://github.com", Some("https://github.com")),
            ("https://GitHub.com/", Some("https://github.com")),
            ("https://github.com:443", Some("https://github.com")),
            ("https://github.com:8443", Some("https://github.com:8443")),
            ("http://localhost:8080", Some("http://localhost:8080")),
            ("http://127.0.0.1:9", Some("http://127.0.0.1:9")),
            ("http://[::1]:9", Some("http://[::1]:9")),
            ("http://github.com", None),
            ("https://github.com/login", None),
            ("https://github.com/?next=1", None),
            ("https://gui@github.com", None),
            ("github.com", None),
            ("file:///etc/passwd", None),
        ] {
            assert_eq!(
                Origin::from_str(text).ok().map(String::from).as_deref(),
                parsed,
                "{text}"
            );
        }
        for (text, parsed) in [
            ("api.openai.com", Some("api.openai.com")),
            ("API.OpenAI.com:443", Some("api.openai.com")),
            ("localhost:8080", Some("localhost:8080")),
            ("*.openai.com", None),
            ("https://api.openai.com", None),
            ("api.openai.com/v1", None),
            ("key@api.openai.com", None),
            ("", None),
        ] {
            assert_eq!(
                HostSpec::from_str(text).ok().map(String::from).as_deref(),
                parsed,
                "{text:?}"
            );
        }
    }

    #[test]
    fn a_login_is_listed_only_on_its_exact_origin() {
        let sites = [
            origin("https://github.com"),
            origin("http://localhost:3000"),
        ];
        for (page, listed) in [
            ("https://github.com", true),
            ("https://GITHUB.com", true),
            ("http://localhost:3000", true),
            ("https://gist.github.com", false),
            ("https://github.com:8443", false),
            ("https://github.com.evil.example", false),
            ("http://localhost:3001", false),
        ] {
            assert_eq!(site_listed(&sites, &origin(page)), listed, "{page}");
        }
    }

    #[test]
    fn a_key_goes_only_to_its_hosts_over_https() {
        let hosts =
            ["api.openai.com", "127.0.0.1:8080"].map(|host| HostSpec::from_str(host).unwrap());
        for (request, admitted) in [
            ("https://api.openai.com/v1/models", true),
            ("https://API.openai.com:443/v1?x=1", true),
            ("http://127.0.0.1:8080/echo", true),
            ("https://127.0.0.1:8080/echo", true),
            ("http://api.openai.com/v1/models", false),
            ("https://api.openai.com:8443/v1", false),
            ("https://evil.example/?to=api.openai.com", false),
            ("https://api.openai.com.evil.example/", false),
            ("https://user:pass@api.openai.com/", false),
            ("http://127.0.0.1:8081/", false),
            ("ftp://api.openai.com/", false),
        ] {
            assert_eq!(key_admits(&hosts, &url(request)), admitted, "{request}");
        }
    }

    fn item(users: &[&str], approval: Approval) -> ItemInfo {
        ItemInfo {
            name: SecretName::from_str("github-gui").unwrap(),
            kind: ItemKind::Login {
                sites: vec![origin("https://github.com")],
                username: "gui".into(),
                hint: "work".into(),
            },
            users: users.iter().map(|user| (*user).to_owned()).collect(),
            approval,
            created_ms: 0,
            updated_ms: 0,
        }
    }

    fn context(grant: Option<GrantState>, takes_approvals: bool) -> UseContext {
        UseContext {
            requester: "slack:U1".into(),
            environment: "local".into(),
            grants: grant
                .map(|grant| (SecretName::from_str("github-gui").unwrap(), grant))
                .into_iter()
                .collect::<BTreeMap<_, _>>(),
            takes_approvals,
        }
    }

    #[derive(Debug, PartialEq)]
    enum Rule {
        Allowed,
        Asks(Vec<String>),
        Refused,
    }

    #[test]
    fn the_use_rule_checks_the_requester_then_the_grant() {
        use Approval::{FirstUse, None as Unattended};
        use GrantState::{Denied, Granted, Waiting};
        let asks = |users: &[&str]| Rule::Asks(users.iter().map(|u| (*u).to_owned()).collect());
        for (case, users, approval, grant, approvals, expected) in [
            (
                "unattended",
                &[][..],
                Unattended,
                None,
                false,
                Rule::Allowed,
            ),
            (
                "for someone else",
                &["slack:U2"],
                Unattended,
                Some(Granted),
                true,
                Rule::Refused,
            ),
            (
                "for the requester",
                &["slack:U1"],
                Unattended,
                None,
                false,
                Rule::Allowed,
            ),
            ("first use asks", &[], FirstUse, None, true, asks(&[])),
            (
                "still waiting",
                &["slack:U1"],
                FirstUse,
                Some(Waiting),
                true,
                asks(&["slack:U1"]),
            ),
            (
                "granted",
                &[],
                FirstUse,
                Some(Granted),
                false,
                Rule::Allowed,
            ),
            ("denied", &[], FirstUse, Some(Denied), true, Rule::Refused),
            ("no approver", &[], FirstUse, None, false, Rule::Refused),
            (
                "waiting, no approver",
                &[],
                FirstUse,
                Some(Waiting),
                false,
                Rule::Refused,
            ),
        ] {
            let got = match use_rule(&item(users, approval), &context(grant, approvals)) {
                Admission::Allowed(()) => Rule::Allowed,
                Admission::NeedsApproval {
                    secret,
                    label,
                    answerable_by,
                } => {
                    assert_eq!(secret, "local/github-gui", "{case}");
                    assert_eq!(
                        label, "sign in to https://github.com as gui (login github-gui)",
                        "{case}"
                    );
                    Rule::Asks(answerable_by)
                }
                Admission::Refused(_) => Rule::Refused,
            };
            assert_eq!(got, expected, "{case}");
        }
    }
}
