//! What a Goal lets `fetch_url` reach, and the code that holds it there.
//!
//! A Goal's `scope` is a label and its prompt a request; neither stops an
//! agent that was talked into probing another host. The optional `fetch`
//! policy is the part the broker enforces: which hosts, which methods and
//! how fast, judged on the first URL and on every redirect hop. Separately,
//! a fetch never reaches a private, loopback or link-local address unless
//! the policy says so, because the worker's host also runs services that
//! must not be probed through it.
//!
//! Agent mode has no Goal and so no policy, but its `fetch_url` goes through
//! the same guard ([`FetchGuard::open`]): any public host, and a private
//! address only when the host's own config allows it.
use super::FETCH_METHODS;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::{Duration, Instant},
};

const MAX_HOSTS: usize = 64;
const MIN_RPS: f64 = 0.01;
const MAX_RPS: f64 = 1000.0;
/// Bounds the connection of one hop, as `swarm::http_client` does for the
/// shared client.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The reasons a request is refused, as the events and the tool reply name
/// them.
pub const HOST_NOT_ALLOWED: &str = "host_not_allowed";
pub const METHOD_NOT_ALLOWED: &str = "method_not_allowed";
pub const REDIRECT_OFF_LIST: &str = "redirect_off_list";
pub const RATE_LIMITED: &str = "rate_limited";
pub const PRIVATE_ADDRESS: &str = "private_address";
pub const HEADER_NOT_ALLOWED: &str = "header_not_allowed";

/// What a Goal lets `fetch_url` reach. A Goal without one keeps the
/// historical reach (any public host, any method, no rate limit); a Goal with
/// one is held to it. It needs the `fetch_url` grant, and an
/// `authorized-audit` scope added after the policy existed needs it too.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FetchPolicy {
    /// The hosts a request may name, whether the agent asked for the URL or a
    /// redirect named it: an exact name (`app.example.com`), `*.example.com`
    /// for any host under that suffix (never the bare suffix), or an IP
    /// address. Ports are not part of the match.
    pub hosts: Vec<String>,
    /// The methods an agent may use. Absent, every method `fetch_url`
    /// supports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub methods: Option<Vec<String>>,
    /// Calls per second one agent may make. A call that follows redirects
    /// counts once. Absent, no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_agent_rps: Option<f64>,
    /// Off, a redirect to a host off the list is refused and not followed.
    /// On, it is followed, though the URL the agent asked for must still be
    /// listed.
    #[serde(default)]
    pub follow_redirects_off_list: bool,
    /// Off, a request to a private, loopback or link-local address is
    /// refused, on every hop.
    #[serde(default)]
    pub allow_private_networks: bool,
}

impl FetchPolicy {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=MAX_HOSTS).contains(&self.hosts.len()),
            "fetch.hosts must list 1 to {MAX_HOSTS} hosts"
        );
        for entry in &self.hosts {
            HostPattern::parse(entry).with_context(|| format!("fetch.hosts entry {entry:?}"))?;
        }
        if let Some(methods) = &self.methods {
            ensure!(
                !methods.is_empty(),
                "fetch.methods must name a method; leave it out to allow every one"
            );
            for method in methods {
                ensure!(
                    FETCH_METHODS.contains(&method.to_ascii_uppercase().as_str()),
                    "fetch.methods names {method:?}, which fetch_url does not support ({})",
                    FETCH_METHODS.join(", ")
                );
            }
        }
        if let Some(rps) = self.per_agent_rps {
            ensure!(
                rps.is_finite() && (MIN_RPS..=MAX_RPS).contains(&rps),
                "fetch.per_agent_rps must be between {MIN_RPS} and {MAX_RPS}"
            );
        }
        Ok(())
    }
}

/// One `hosts` entry, ready to match a URL's host.
#[derive(Clone, Debug, PartialEq)]
enum HostPattern {
    Name(String),
    /// The suffix of `*.suffix`, without the star.
    Under(String),
    Addr(IpAddr),
}

impl HostPattern {
    fn parse(entry: &str) -> Result<Self> {
        if let Ok(ip) = unbracketed(entry).parse::<IpAddr>() {
            return Ok(Self::Addr(ip));
        }
        let (wild, name) = match entry.strip_prefix("*.") {
            Some(suffix) => (true, suffix),
            None => (false, entry),
        };
        let name = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
        ensure!(
            valid_name(&name),
            "write a host name only, in ASCII (punycode for a non-English name): no scheme, \
             port, path or other `*`"
        );
        if wild {
            ensure!(
                name.contains('.'),
                "a wildcard needs a suffix of two or more labels, such as *.example.com"
            );
            Ok(Self::Under(name))
        } else {
            Ok(Self::Name(name))
        }
    }

    /// `host` is a URL's `host_str`, lowercased by the URL parser already.
    fn matches(&self, host: &str) -> bool {
        let host = host.strip_suffix('.').unwrap_or(host);
        match self {
            Self::Name(name) => host == name,
            Self::Under(suffix) => host
                .strip_suffix(suffix.as_str())
                .is_some_and(|rest| rest.len() > 1 && rest.ends_with('.')),
            Self::Addr(ip) => unbracketed(host).parse::<IpAddr>().is_ok_and(|h| h == *ip),
        }
    }
}

fn unbracketed(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// A request the broker would not send, with what the agent reads about it.
/// It travels as the error of a fetch, so the caller can tell a refusal,
/// which the model is told about and the run records, from a fetch that
/// failed.
#[derive(Clone, Debug, PartialEq)]
pub struct FetchRefused {
    pub reason: &'static str,
    /// Written for the model: what was refused, what is allowed, what to do.
    pub message: String,
    /// The URL refused. For a redirect, the one it named.
    pub url: String,
    /// 0 for the URL the agent asked for, then one per redirect.
    pub hop: usize,
    pub retry_after_ms: Option<u64>,
    /// For a refusal of a private address, the address the host name came to.
    /// The model is not told it, since it names the host's own network; the
    /// run's record is.
    pub address: Option<IpAddr>,
}

impl std::fmt::Display for FetchRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for FetchRefused {}

impl FetchRefused {
    /// The tool result the agent reads in place of a page. It is a result,
    /// not an HTTP error, because a swarm worker's tool bridge shows the
    /// model only the status of a failed call.
    pub fn reply(&self) -> Value {
        let mut reply = json!({
            "refused": true,
            "reason": self.reason,
            "error": self.message,
            "url": self.url,
        });
        if let Some(wait) = self.retry_after_ms {
            reply["retry_after_ms"] = json!(wait);
        }
        reply
    }

    /// The `fetch_refused` event: who asked for what, and which hop was
    /// refused.
    pub fn event(&self, agent: &str, args: &Value) -> Value {
        let mut event = json!({
            "agent_id": agent,
            "url": args["url"],
            "method": args["method"].as_str().unwrap_or("GET").to_ascii_uppercase(),
            "reason": self.reason,
            "hop": self.hop,
            "refused_url": self.url,
            "detail": self.message,
        });
        if let Some(wait) = self.retry_after_ms {
            event["retry_after_ms"] = json!(wait);
        }
        if let Some(address) = self.address {
            event["address"] = json!(address.to_string());
        }
        event
    }

    fn host(host: &str, policy: &FetchPolicy, url: &reqwest::Url) -> Self {
        Self {
            reason: HOST_NOT_ALLOWED,
            message: format!(
                "fetch_url refused: host {host:?} is not on this Goal's fetch allowlist. \
                 Allowed hosts: {}. Nothing was sent. Work only against those hosts.",
                policy.hosts.join(", ")
            ),
            url: url.to_string(),
            hop: 0,
            retry_after_ms: None,
            address: None,
        }
    }

    fn method(method: &str, methods: &[String], url: &reqwest::Url) -> Self {
        let allowed: Vec<String> = methods.iter().map(|m| m.to_ascii_uppercase()).collect();
        Self {
            reason: METHOD_NOT_ALLOWED,
            message: format!(
                "fetch_url refused: method {method} is not allowed by this Goal's fetch policy. \
                 Allowed methods: {}. Nothing was sent. Use one of those.",
                allowed.join(", ")
            ),
            url: url.to_string(),
            hop: 0,
            retry_after_ms: None,
            address: None,
        }
    }

    fn redirect(host: &str, policy: &FetchPolicy, url: &reqwest::Url, via: &Redirect<'_>) -> Self {
        Self {
            reason: REDIRECT_OFF_LIST,
            message: format!(
                "fetch_url refused: {} answered {} and redirects to {url}, whose host {host:?} \
                 is not on this Goal's fetch allowlist (allowed: {}). The redirect was not \
                 followed. Report where it points as evidence; do not fetch it.",
                via.from,
                via.status,
                policy.hosts.join(", ")
            ),
            url: url.to_string(),
            hop: via.hop,
            retry_after_ms: None,
            address: None,
        }
    }

    fn private(host: &str, ip: IpAddr, url: &reqwest::Url, hop: usize) -> Self {
        Self {
            reason: PRIVATE_ADDRESS,
            message: format!(
                "fetch_url refused: {host} resolves to a private address (loopback, private, \
                 link-local, shared or reserved), and fetch_url reaches public addresses only. \
                 Nothing was sent. Do not retry it or look for a way around; if the page is \
                 needed, report that it is on a private network."
            ),
            url: url.to_string(),
            hop,
            retry_after_ms: None,
            address: Some(ip),
        }
    }

    fn host_header(url: &str) -> Self {
        Self {
            reason: HEADER_NOT_ALLOWED,
            message: "fetch_url refused: this Goal's fetch policy sets the Host header from the \
                      URL, so a Host header of your own is not allowed. Nothing was sent. \
                      Resend without it."
                .to_owned(),
            url: url.to_owned(),
            hop: 0,
            retry_after_ms: None,
            address: None,
        }
    }

    pub(super) fn rate(rps: f64, wait_ms: u64, url: &str) -> Self {
        Self {
            reason: RATE_LIMITED,
            message: format!(
                "fetch_url rate limit: this Goal allows {rps} fetch_url call(s) per second for \
                 each agent. Nothing was sent. Retry after {wait_ms} ms."
            ),
            url: url.to_owned(),
            hop: 0,
            retry_after_ms: Some(wait_ms),
            address: None,
        }
    }
}

/// The request a redirect is, for the hop that follows it.
pub(super) struct Redirect<'a> {
    pub hop: usize,
    pub status: u16,
    pub from: &'a reqwest::Url,
}

/// A Goal's fetch policy, or its absence, as one request after another meets
/// it.
pub struct FetchGuard<'a> {
    policy: Option<&'a FetchPolicy>,
    hosts: Vec<HostPattern>,
    /// Whether a private address may be fetched. A policy decides it for a
    /// swarm; agent mode's host config decides it for agent mode.
    allow_private: bool,
    #[cfg(test)]
    seam: Seam,
}

impl<'a> FetchGuard<'a> {
    pub fn new(policy: Option<&'a FetchPolicy>) -> Self {
        // A policy that was never validated drops an entry it cannot read,
        // which narrows what it allows.
        let hosts = policy
            .map(|policy| {
                policy
                    .hosts
                    .iter()
                    .filter_map(|host| HostPattern::parse(host).ok())
                    .collect()
            })
            .unwrap_or_default();
        Self {
            policy,
            hosts,
            allow_private: policy.is_some_and(|policy| policy.allow_private_networks),
            #[cfg(test)]
            seam: Seam::default(),
        }
    }

    /// The guard of a fetch that has no Goal, as agent mode's `fetch_url`:
    /// no host or method list and no rate, but the same refusal of a private
    /// address on every hop unless `allow_private_networks`.
    pub fn open(allow_private_networks: bool) -> Self {
        let mut guard = Self::new(None);
        guard.allow_private = allow_private_networks;
        guard
    }

    /// The policy's host and method check on the request in `args`, before
    /// anything counts against the agent's rate or touches the network. A
    /// call that cannot be parsed passes: the fetch itself refuses it.
    pub fn precheck(&self, args: &Value) -> Result<(), FetchRefused> {
        let Some(url) = args["url"]
            .as_str()
            .and_then(|raw| reqwest::Url::parse(raw).ok())
        else {
            return Ok(());
        };
        let method = args["method"]
            .as_str()
            .unwrap_or("GET")
            .to_ascii_uppercase();
        self.admit(&url, &method, None)?;
        self.admit_headers(args)
    }

    /// Under a policy the request's `Host` is the URL's. A `Host` header the
    /// agent sets would send the request to a listed address and name any
    /// other site that address serves, which is the host rule gone around.
    pub(super) fn admit_headers(&self, args: &Value) -> Result<(), FetchRefused> {
        if self.policy.is_some()
            && args["headers"]
                .as_object()
                .is_some_and(|headers| headers.keys().any(|k| k.eq_ignore_ascii_case("host")))
        {
            return Err(FetchRefused::host_header(
                args["url"].as_str().unwrap_or_default(),
            ));
        }
        Ok(())
    }

    /// Whether the policy admits one request: the method and host of the
    /// first, the host of a redirect.
    pub(super) fn admit(
        &self,
        url: &reqwest::Url,
        method: &str,
        via: Option<&Redirect<'_>>,
    ) -> Result<(), FetchRefused> {
        let Some(policy) = self.policy else {
            return Ok(());
        };
        let host = url.host_str().unwrap_or_default();
        if !self.hosts.iter().any(|pattern| pattern.matches(host)) {
            match via {
                Some(_) if policy.follow_redirects_off_list => {}
                Some(via) => return Err(FetchRefused::redirect(host, policy, url, via)),
                None => return Err(FetchRefused::host(host, policy, url)),
            }
        }
        if via.is_none()
            && let Some(methods) = &policy.methods
            && !methods.iter().any(|m| m.eq_ignore_ascii_case(method))
        {
            return Err(FetchRefused::method(method, methods, url));
        }
        Ok(())
    }

    /// A client that will connect to `url` only at the addresses vetted here:
    /// a name is resolved once, every address must be public unless private
    /// networks are allowed, and the client is then pinned to those
    /// addresses, so a second lookup cannot answer differently. It goes
    /// through no proxy, which would open the connection itself.
    pub(super) async fn pin(&self, url: &reqwest::Url, hop: usize) -> Result<reqwest::Client> {
        ensure!(
            matches!(url.scheme(), "https" | "http"),
            "fetch supports http and https only"
        );
        let host = url.host_str().context("fetch url has no host")?;
        let port = url
            .port_or_known_default()
            .context("fetch url has no port")?;
        let literal = unbracketed(host).parse::<IpAddr>().ok();
        let addrs: Vec<SocketAddr> = match literal {
            Some(ip) => vec![SocketAddr::new(ip, port)],
            None => self.resolve(host, port).await?,
        };
        ensure!(!addrs.is_empty(), "cannot resolve {host}");
        if !self.allow_private
            && let Some(addr) = addrs.iter().find(|addr| !is_public_ip(addr.ip()))
        {
            return Err(FetchRefused::private(host, addr.ip(), url, hop).into());
        }
        #[cfg(test)]
        let addrs = self.seam.dial(addrs);
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none());
        if literal.is_none() {
            builder = builder.resolve_to_addrs(host, &addrs);
        }
        Ok(builder.build()?)
    }

    /// Every address `host` resolves to.
    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        #[cfg(test)]
        if let Some(addrs) = self.seam.lookup(host, port) {
            return Ok(addrs);
        }
        Ok(tokio::net::lookup_host((host, port))
            .await
            .with_context(|| format!("cannot resolve {host}"))?
            .collect())
    }
}

/// What a test can change about a guard's network: what a host name
/// resolves to, and where a vetted connection then goes. Together they let a
/// test fetch from a name that looks public, whose address passes the check,
/// from a server that can only listen on loopback.
#[cfg(test)]
#[derive(Default)]
struct Seam {
    lookup: Option<Lookup>,
    dial: Option<SocketAddr>,
}

#[cfg(test)]
type Lookup = Box<dyn Fn(&str) -> Vec<IpAddr> + Send + Sync>;

#[cfg(test)]
impl Seam {
    fn lookup(&self, host: &str, port: u16) -> Option<Vec<SocketAddr>> {
        let answer = self.lookup.as_ref()?(host);
        Some(
            answer
                .into_iter()
                .map(|ip| SocketAddr::new(ip, port))
                .collect(),
        )
    }

    fn dial(&self, addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
        self.dial.map_or(addrs, |to| vec![to])
    }
}

#[cfg(test)]
impl FetchGuard<'_> {
    /// Resolve names with `lookup`, in place of the system resolver.
    pub(super) fn resolving(
        mut self,
        lookup: impl Fn(&str) -> Vec<IpAddr> + Send + Sync + 'static,
    ) -> Self {
        self.seam.lookup = Some(Box::new(lookup));
        self
    }

    /// Connect to `to`, whatever address a name was vetted at.
    pub(super) fn dialing(mut self, to: SocketAddr) -> Self {
        self.seam.dial = Some(to);
        self
    }
}

/// Whether `ip` is an address on the public internet: not loopback, private,
/// link-local, shared (carrier-grade NAT, which Tailscale uses), multicast,
/// reserved or documentation, and, for an IPv6 address that carries an IPv4
/// one, not one that is any of those.
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => public_v6(ip),
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || a == 0
        || (a == 100 && b & 0xc0 == 64)
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && b & 0xfe == 18)
        || a >= 240)
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    let embedded =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    if let Some(v4) = ip.to_ipv4_mapped() {
        return public_v4(v4);
    }
    // ::/96 holds `::` and `::1` and the deprecated IPv4-compatible form;
    // 64:ff9b::/96 is NAT64. Both carry an IPv4 address in the last 32 bits.
    if s[..6] == [0; 6] || s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return public_v4(embedded(s[6], s[7]));
    }
    // 6to4 carries one right after the prefix.
    if s[0] == 0x2002 {
        return public_v4(embedded(s[1], s[2]));
    }
    !(ip.is_multicast()
        || s[0] & 0xffc0 == 0xfe80
        || s[0] & 0xffc0 == 0xfec0
        || s[0] & 0xfe00 == 0xfc00
        || (s[0] == 0x2001 && s[1] == 0x0db8)
        || (s[0] == 0x0100 && s[1..4] == [0; 3]))
}

/// When each agent may next call `fetch_url`, on a clock that never goes
/// back. One call opens the next `1 / rps` seconds closed to that agent: no
/// burst, so the rate holds over any window.
pub struct Pacer {
    started: Instant,
    next_ms: BTreeMap<String, u64>,
}

impl Default for Pacer {
    fn default() -> Self {
        Self::new()
    }
}

impl Pacer {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            next_ms: BTreeMap::new(),
        }
    }

    pub fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Lets `agent` call at `now_ms`, or says how many milliseconds to wait.
    /// A refused call does not move the window.
    pub fn take(&mut self, agent: &str, interval_ms: u64, now_ms: u64) -> Result<(), u64> {
        let next = self.next_ms.get(agent).copied().unwrap_or(0);
        if now_ms < next {
            return Err(next - now_ms);
        }
        self.next_ms
            .insert(agent.to_owned(), now_ms.saturating_add(interval_ms));
        Ok(())
    }
}

/// The gap between two calls at `rps`, rounded up so the rate is never
/// exceeded.
pub fn interval_ms(rps: f64) -> u64 {
    (1000.0 / rps).ceil().max(1.0) as u64
}
