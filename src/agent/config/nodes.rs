//! The `nodes` section: where remote nodes dial in, the agent's own
//! certificate, and each node that may connect, pinned by its fingerprint.
//! `executor.environment` names where a session runs when its Plan names
//! nowhere.

use super::{AgentConfig, ExecutorConfig};
use crate::agent::environment::LOCAL;
use crate::remote::Fingerprint;
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

const ABOUT_CHARS: usize = 300;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodesConfig {
    /// Where nodes dial in, such as `0.0.0.0:7443`.
    pub listen: SocketAddr,
    /// From `solos-swarm agent init-nodes`, relative to this file.
    pub cert: PathBuf,
    pub key: PathBuf,
    /// The nodes that may connect, by name.
    pub allow: BTreeMap<String, AllowedNode>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowedNode {
    /// The node's certificate, as `solos-swarm node init` printed it.
    pub fingerprint: Fingerprint,
    /// What the Router is told the machine is for.
    #[serde(default)]
    pub about: Option<String>,
}

impl NodesConfig {
    pub(super) fn resolve(&mut self, base: &Path) {
        self.cert = base.join(&self.cert);
        self.key = base.join(&self.key);
    }
}

impl AgentConfig {
    pub(super) fn validate_nodes(&self) -> Result<()> {
        let environment = match &self.executor {
            ExecutorConfig::Pi { environment, .. } => environment.as_deref(),
            ExecutorConfig::Scripted {} | ExecutorConfig::Simulated { .. } => None,
        };
        let Some(nodes) = &self.nodes else {
            ensure!(
                environment.is_none_or(|name| name == LOCAL),
                "executor.environment names a node, but the config has no nodes"
            );
            return Ok(());
        };
        ensure!(
            matches!(self.executor, ExecutorConfig::Pi { .. }),
            "nodes run pi workers' commands, and the executor is not pi"
        );
        ensure!(!nodes.allow.is_empty(), "nodes.allow names no node");
        let mut fingerprints = BTreeSet::new();
        for (name, node) in &nodes.allow {
            ensure!(
                !name.is_empty()
                    && name.len() <= 64
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    && name != LOCAL,
                "node name {name:?} is not a short word other than {LOCAL}"
            );
            ensure!(
                fingerprints.insert(node.fingerprint),
                "nodes {name} and another pin the same certificate"
            );
            ensure!(
                node.about
                    .as_ref()
                    .is_none_or(|about| about.chars().count() <= ABOUT_CHARS),
                "nodes.allow.{name}.about is longer than {ABOUT_CHARS} characters"
            );
        }
        if let Some(name) = environment {
            ensure!(
                name == LOCAL || nodes.allow.contains_key(name),
                "executor.environment {name} is neither {LOCAL} nor a node in nodes.allow"
            );
        }
        Ok(())
    }

    /// What the Router's prompt says about where a session can run, if
    /// the agent has nodes, and about the browser, if it has one.
    pub fn environment_note(&self) -> String {
        let browser = match (&self.browser, &self.vault) {
            (None, _) => "",
            (Some(_), None) => {
                "\nBrowser: a session on this host has a headless browser for work that needs a \
                 web page; every step it takes is recorded and signed."
            }
            (Some(_), Some(_)) => {
                "\nBrowser: a session on this host has a headless browser for work that needs a \
                 web page, and it can sign in with the vault's logins without seeing them; every \
                 step it takes is recorded and signed."
            }
        };
        let Some(nodes) = &self.nodes else {
            return browser.to_owned();
        };
        let default = match &self.executor {
            ExecutorConfig::Pi {
                environment: Some(name),
                ..
            } => name.as_str(),
            _ => LOCAL,
        };
        let here = if self.browser.is_some() {
            "this host, with the browser"
        } else {
            "this host"
        };
        let mut note = format!(
            "{browser}\nMachines: a session's run_code runs on {default} unless the accept names \
             an `environment`: {LOCAL} ({here})"
        );
        for (name, node) in &nodes.allow {
            let _ = match &node.about {
                Some(about) => write!(note, ", {name} ({})", about.trim()),
                None => write!(note, ", {name}"),
            };
        }
        note.push_str(
            ". Work on a machine that is offline waits for it to connect; name one only when the \
             work needs what it has.",
        );
        note
    }
}

#[cfg(test)]
mod tests {
    use super::super::AgentConfig;

    const FINGERPRINT: &str =
        "sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn config(nodes: &str, environment: &str) -> anyhow::Result<AgentConfig> {
        let yaml = format!(
            "
policy:
  people: [slack:alice]
sources:
  - {{kind: jsonl, path: triggers.jsonl}}
channels:
  - {{kind: jsonl, source: slack, path: outbox.jsonl}}
router: {{kind: scripted}}
executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {{input_microusd_per_million: 300000, output_microusd_per_million: 1200000}}
{environment}
budgets: {{session_microusd: 500000}}
{nodes}"
        );
        let config: AgentConfig = serde_yaml::from_str(&yaml)?;
        config.validate()?;
        Ok(config)
    }

    fn nodes(allow: &str) -> String {
        format!(
            "nodes:\n  listen: 127.0.0.1:7443\n  cert: tls/cert.pem\n  key: tls/key.pem\n  allow:\n{allow}"
        )
    }

    #[test]
    fn nodes_load_with_their_pins_and_the_executor_may_default_to_one() {
        let allow = format!(
            "    laptop: {{fingerprint: \"{FINGERPRINT}\", about: \"the solos checkout at /repo\"}}\n"
        );
        let loaded = config(&nodes(&allow), "  environment: laptop").unwrap();
        let note = loaded.environment_note();
        assert!(
            note.contains("runs on laptop unless")
                && note.contains("local (this host), laptop (the solos checkout at /repo)"),
            "{note}"
        );
        let nodes = loaded.nodes.unwrap();
        assert_eq!(nodes.allow["laptop"].fingerprint.to_string(), FINGERPRINT);
        assert!(config("", "").unwrap().environment_note().is_empty());
    }

    #[test]
    fn the_router_is_told_which_machine_has_the_browser() {
        let browser = "browser: {lightpanda: /opt/lightpanda, audit_key: /keys/trail.key}";
        let laptop = format!("    laptop: {{fingerprint: \"{FINGERPRINT}\"}}\n");
        let alone = config(browser, "").unwrap().environment_note();
        assert!(
            alone.starts_with("\nBrowser: a session on this host has a headless browser")
                && !alone.contains("Machines"),
            "{alone}"
        );
        let with_nodes = config(&format!("{browser}\n{}", nodes(&laptop)), "")
            .unwrap()
            .environment_note();
        assert!(
            with_nodes.contains("local (this host, with the browser), laptop."),
            "{with_nodes}"
        );
        assert!(!alone.contains("vault"), "no vault, no logins: {alone}");
    }

    #[test]
    fn nodes_refuse_what_cannot_be_told_apart_or_reached() {
        let laptop = format!("    laptop: {{fingerprint: \"{FINGERPRINT}\"}}\n");
        for (case, nodes, environment) in [
            (
                "an unknown default",
                nodes(&laptop),
                "  environment: desktop",
            ),
            (
                "a node default without nodes",
                String::new(),
                "  environment: laptop",
            ),
            (
                "two nodes on one certificate",
                nodes(&format!(
                    "{laptop}    desktop: {{fingerprint: \"{FINGERPRINT}\"}}\n"
                )),
                "",
            ),
            (
                "a node named local",
                nodes(&format!("    local: {{fingerprint: \"{FINGERPRINT}\"}}\n")),
                "",
            ),
            (
                "no node",
                "nodes:\n  listen: 127.0.0.1:7443\n  cert: c\n  key: k\n  allow: {}\n".into(),
                "",
            ),
            (
                "a bad fingerprint",
                nodes("    laptop: {fingerprint: \"sha256:00\"}\n"),
                "",
            ),
            (
                "an unknown field",
                nodes(&format!(
                    "    laptop: {{fingerprint: \"{FINGERPRINT}\", key: x}}\n"
                )),
                "",
            ),
        ] {
            assert!(config(&nodes, environment).is_err(), "{case}: accepted");
        }
        assert!(config("", "  environment: local").is_ok());
    }
}
