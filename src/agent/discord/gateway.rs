use super::Token;
use anyhow::{Context, Result, ensure};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use zeroize::Zeroizing;

const GUILDS: u32 = 1;
const GUILD_MESSAGES: u32 = 1 << 9;
const DIRECT_MESSAGES: u32 = 1 << 12;
const MESSAGE_CONTENT: u32 = 1 << 15;

/// `MESSAGE_CONTENT` is privileged: the developer portal must enable it.
pub const INTENTS: u32 = GUILDS | GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT;

pub(super) mod op {
    pub const DISPATCH: u8 = 0;
    pub const HEARTBEAT: u8 = 1;
    pub const RECONNECT: u8 = 7;
    pub const INVALID_SESSION: u8 = 9;
    pub const HELLO: u8 = 10;
    pub const HEARTBEAT_ACK: u8 = 11;
}

#[derive(Deserialize)]
pub(super) struct Frame {
    pub op: u8,
    #[serde(default)]
    pub d: Value,
    pub s: Option<u64>,
    pub t: Option<String>,
}

pub(super) fn heartbeat(seq: Option<u64>) -> String {
    json!({ "op": op::HEARTBEAT, "d": seq }).to_string()
}

pub(super) fn identify(token: &Token) -> Zeroizing<String> {
    Zeroizing::new(
        json!({
            "op": 2,
            "d": {
                "token": token.secret(),
                "intents": INTENTS,
                "properties": {
                    "os": std::env::consts::OS,
                    "browser": "solos-swarm",
                    "device": "solos-swarm",
                },
            },
        })
        .to_string(),
    )
}

pub(super) fn resume(token: &Token, session: &str, seq: u64) -> Zeroizing<String> {
    Zeroizing::new(
        json!({
            "op": 6,
            "d": { "token": token.secret(), "session_id": session, "seq": seq },
        })
        .to_string(),
    )
}

pub(super) fn endpoint(base: &str) -> Result<String> {
    let mut url = Url::parse(base).context("Discord named an unreadable gateway URL")?;
    ensure!(
        matches!(url.scheme(), "ws" | "wss"),
        "Discord named a gateway URL that is not a WebSocket"
    );
    url.set_query(Some("v=10&encoding=json"));
    Ok(url.into())
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Closing {
    Fatal(&'static str),
    Reidentify,
    Resume,
}

pub(super) fn closing(code: u16) -> Closing {
    match code {
        4004 => Closing::Fatal(
            "the bot token was refused. Reset it on the application's Bot page in the developer \
             portal and write the new token to the token file",
        ),
        4010 => Closing::Fatal(
            "Discord rejected the shard the agent sent. This is a bug in this build; report it",
        ),
        4011 => Closing::Fatal(
            "the bot is in too many servers for one connection and the agent does not shard",
        ),
        4012 => Closing::Fatal(
            "Discord does not serve the gateway version this build speaks (v10). Update \
             solos-swarm",
        ),
        4013 => Closing::Fatal(
            "Discord rejected the intents the agent sends (Guilds, Guild Messages, Direct \
             Messages and Message Content). This is a bug in this build; report it",
        ),
        4014 => Closing::Fatal(
            "the agent asks for an intent the application has not enabled. Turn on Message \
             Content Intent under Privileged Gateway Intents on the application's Bot page in \
             the developer portal, then start the agent again",
        ),
        4007 | 4009 => Closing::Reidentify,
        _ => Closing::Resume,
    }
}

#[derive(Debug)]
pub(super) struct Fatal {
    pub code: u16,
    pub fix: &'static str,
}

impl std::fmt::Display for Fatal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Discord closed the gateway with code {}: {}",
            self.code, self.fix
        )
    }
}

impl std::error::Error for Fatal {}

#[cfg(test)]
mod tests {
    use super::{Closing, INTENTS, closing, endpoint, heartbeat};

    #[test]
    fn close_codes_split_into_fatal_new_session_and_resume() {
        for code in [4004, 4010, 4011, 4012, 4013, 4014] {
            assert!(matches!(closing(code), Closing::Fatal(_)), "{code}");
        }
        assert_eq!(closing(4007), Closing::Reidentify);
        assert_eq!(closing(4009), Closing::Reidentify);
        for code in [1000, 1001, 1006, 4000, 4001, 4002, 4003, 4005, 4008, 4999] {
            assert_eq!(closing(code), Closing::Resume, "{code}");
        }
    }

    #[test]
    fn the_intents_are_the_four_the_agent_needs() {
        assert_eq!(INTENTS, 1 | (1 << 9) | (1 << 12) | (1 << 15));
        let parsed = |text: String| serde_json::from_str::<serde_json::Value>(&text).unwrap();
        assert_eq!(
            parsed(heartbeat(None)),
            serde_json::json!({ "op": 1, "d": null })
        );
        assert_eq!(
            parsed(heartbeat(Some(7))),
            serde_json::json!({ "op": 1, "d": 7 })
        );
    }

    #[test]
    fn a_gateway_url_gets_the_version_and_encoding() {
        assert_eq!(
            endpoint("wss://gateway.discord.gg").unwrap(),
            "wss://gateway.discord.gg/?v=10&encoding=json"
        );
        assert!(endpoint("https://gateway.discord.gg").is_err());
        assert!(endpoint("not a url").is_err());
    }
}
