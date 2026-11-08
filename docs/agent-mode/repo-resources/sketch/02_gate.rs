//! Lives at: src/agent/gate/ (new): mod.rs, connect.rs, tunnel.rs,
//! inspect.rs, relay.rs.
//!
//! Sketch only: not compiled. It names the seams and the rules, not
//! working code.
//!
//! The gate: an HTTP CONNECT proxy inside `agent serve`. Each repository
//! Ticket with `egress` gets one Unix socket in a directory bound read-only
//! into its sandboxes at `/runtime/egress`. The socket is the Ticket's identity.
//! Nothing else leaves the sandbox, which keeps `--unshare-all`.
//!
//! It reuses `broker_fetch::is_public_ip` and the resolve-once-and-pin rule of
//! `FetchGuard::pin` (src/broker_fetch.rs:446), `KeyHeader::reveal_into`
//! (src/vault.rs:442) for keys, `Fact::KeyUsed` before a keyed request leaves
//! (src/agent/broker/secrets.rs:68), `Fact::Note` for refusals, and the
//! ledger's holds for prices.

use crate::agent::config::{EgressConfig, HostRule, Mode};
use anyhow::Result;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

/// Why the gate refused, as the row, the 403 body, `X-Solos-Egress`, the log
/// line and the Note name it.
pub mod reason {
    pub const NOT_LISTED: &str = "host_not_listed";
    pub const PORT: &str = "port_not_listed";
    pub const IP_LITERAL: &str = "ip_literal";
    pub const PLAIN_HTTP: &str = "plain_http";
    pub const PRIVATE_ADDRESS: &str = "private_address";
    pub const SNI: &str = "sni_mismatch";
    /// Encrypted Client Hello or ESNI: the real name is hidden.
    pub const ECH: &str = "encrypted_client_hello";
    /// `Upgrade` or HTTP/2 extended CONNECT in inspect mode.
    pub const UPGRADE: &str = "upgrade";
    pub const AUTHORITY: &str = "authority_mismatch";
    pub const METHOD: &str = "method_not_allowed";
    pub const PATH: &str = "path_not_allowed";
    pub const TICKET_BYTES: &str = "ticket_bytes";
    pub const DAILY_BYTES: &str = "daily_bytes";
    pub const BUDGET: &str = "budget";
    pub const APPROVAL: &str = "key_needs_approval";
    pub const PAUSED: &str = "paused";
    pub const ENDED: &str = "ticket_ended";
    pub const TOO_MANY: &str = "too_many_connections";
    pub const CUT: &str = "cut";
}

/// One running gate, shared by every Ticket's listener.
pub struct Gate {
    /// The CA made at this start. Its key never leaves memory, and it is
    /// name-constrained to every host in the config.
    ca: Arc<Ca>,
    /// `<state>/egress`, mode 0700.
    dir: PathBuf,
}

pub struct Ca;

/// What one Ticket's listener knows.
pub struct TicketGate {
    pub ticket: String,
    pub repo: String,
    pub rules: EgressConfig,
    pub socket: PathBuf,
}

/// A CONNECT after the gate judged it.
pub enum Admitted {
    Tunnel { host: String, port: u16, addrs: Vec<SocketAddr> },
    Inspect { rule: HostRule, host: String, port: u16, addrs: Vec<SocketAddr> },
}

pub struct Refused {
    pub reason: &'static str,
    /// Written for the worker: what was refused and what is allowed.
    pub message: String,
}

impl Gate {
    pub fn start(state_dir: &std::path::Path, hosts: &[String]) -> Result<Self> {
        let _ = (state_dir, hosts);
        todo!("make <state>/egress, the CA")
    }

    /// The Ticket's socket and listener, at its first Assignment.
    pub async fn open(&self, ticket: &str, repo: &str, rules: EgressConfig) -> Result<PathBuf> {
        let _ = (ticket, repo, rules);
        todo!()
    }

    /// Close and refuse `host` for `ticket` from now on: #102's tripwire.
    pub async fn cut(&self, ticket: &str, host: &str) {
        let _ = (ticket, host);
        todo!()
    }

    /// The Ticket ended or was stopped: close its socket and connections.
    pub async fn close(&self, ticket: &str) {
        let _ = ticket;
        todo!()
    }

    /// The CA certificate, for the bundle the sandbox binds.
    pub fn ca_pem(&self) -> String {
        todo!()
    }
}

impl TicketGate {
    /// The CONNECT head: exact name, listed port, no IP literal, Ticket open,
    /// pause gate open, caps not reached, fewer than 32 connections. Then,
    /// and only then, resolve; every address public, else refused.
    pub async fn admit(&self, head: &str) -> Result<Admitted, Refused> {
        let _ = head;
        todo!()
    }
}

/// Tunnel mode: the ClientHello's SNI must equal the CONNECT name, and it
/// must carry no Encrypted Client Hello or ESNI extension. Reads at most
/// 16 KiB within 5 s, and returns the bytes read so they go upstream
/// unchanged.
pub async fn peek_sni(client: &mut tokio::net::UnixStream) -> Result<(Option<String>, Vec<u8>)> {
    let _ = client;
    todo!()
}

/// Inspect mode, per request, before it goes upstream:
/// - `Host`, `:authority` and an absolute target must all be the CONNECT
///   name (port normalized), else `AUTHORITY`;
/// - method and path rules;
/// - hop-by-hop, `Proxy-*`, `Forwarded`, `X-Forwarded-*`, `X-Original-URL`
///   and `X-Rewrite-URL` removed;
/// - on an entry with keys or `github`: every client value of the keys'
///   headers, `Authorization` and `Cookie` removed, then each key put on with
///   `KeyHeader::reveal_into`.
pub fn admit_request(
    rule: &HostRule,
    connect_host: &str,
    request: &mut http::Request<()>,
) -> Result<(), Refused> {
    let _ = (rule, connect_host, request);
    todo!()
}

/// Every vault value found in a response body is overwritten byte for byte
/// with `*`, so gRPC and HTTP framing keep their lengths.
pub fn mask(body: &mut [u8], values: &[&[u8]]) -> usize {
    let _ = (body, values);
    todo!()
}

/// The relay inside the sandbox: `solos-swarm egress-relay --listen
/// 127.0.0.1:3128 --socket /runtime/egress/sock -- /usr/bin/bash -c CODE`.
/// It dials `socket` for each client connection, so a gate restart is reached
/// by the next one. It decides nothing; it runs the command as its child and
/// exits with its status.
pub fn relay(listen: SocketAddr, socket: PathBuf, command: Vec<std::ffi::OsString>) -> Result<i32> {
    let _ = (listen, socket, command);
    todo!()
}

/// The variables a Ticket with egress gets.
pub fn proxy_env() -> Vec<(&'static str, &'static str)> {
    const PROXY: &str = "http://127.0.0.1:3128";
    vec![
        ("HTTPS_PROXY", PROXY),
        ("https_proxy", PROXY),
        ("HTTP_PROXY", PROXY),
        ("http_proxy", PROXY),
        ("ALL_PROXY", PROXY),
        ("all_proxy", PROXY),
        ("NO_PROXY", ""),
        ("no_proxy", ""),
        ("NODE_USE_ENV_PROXY", "1"),
    ]
}

pub fn mode_of(rule: &HostRule) -> Mode {
    rule.mode()
}
