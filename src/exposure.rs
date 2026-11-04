//! What sessions can see. A sandbox binds the host paths this module gives
//! it, and the checks that keep secrets out of reach (the vault and its key,
//! the trail key, the browser's runtime and the trail blobs) refuse a secret
//! under any of them. Both read the same lists, so a mount added here is a
//! path the checks refuse, and `agent serve` and the `vault` CLI judge with
//! one view.

use crate::agent::config::{AgentConfig, ClaudeConfig, ExecutorConfig, RunCodeConfig};
use crate::agent::{TICKETS_DIR, pstack};
use crate::bwrap::Bwrap;
use crate::code::Mount;
use crate::harness::node_root;
use crate::vault::SandboxView;
use std::path::{Path, PathBuf};

/// Where Claude Code's sandbox binds its program, its node and solos-swarm.
pub const CLAUDE: &str = "/runtime/claude";
pub const NODE: &str = "/runtime/node";
pub const SOLOS_SWARM: &str = "/runtime/solos-swarm";

/// Bind `mount`'s source read-only at its target.
pub fn read_only(sandbox: Bwrap, mount: &Mount) -> Bwrap {
    sandbox.ro_bind(&mount.source, &mount.target)
}

/// The read-only binds of a Pi session: the node runtime and the packages
/// Pi runs from.
pub fn pi_binds(node: &Path, pi_root: &Path) -> Vec<Mount> {
    vec![
        mount(NODE, node_root(node)),
        mount("/runtime/node_modules", &pi_root.join("node_modules")),
        mount("/runtime/extensions", &pi_root.join("extensions")),
    ]
}

/// The pstack tree the host checked (`Pstack::root`), bound at `/pstack` in
/// a Pi session and in the `run_code` sandbox. Only a configured pstack has
/// a tree: `read` alone binds nothing, since a repository Ticket is granted
/// it with no pstack.
pub fn pstack_bind(tree: &Path) -> Mount {
    mount(pstack::MOUNT, tree)
}

/// The read-only binds of a Claude Code session. Without `node` the program
/// runs on its own, so its binary is the bind.
pub fn claude_binds(binary: &Path, node: Option<&Path>, solos_swarm: &Path) -> Vec<Mount> {
    let mut binds = Vec::new();
    match node {
        None => binds.push(mount(CLAUDE, binary)),
        Some(node) => {
            binds.push(mount(NODE, node_root(node)));
            binds.push(mount(CLAUDE, binary.parent().unwrap_or(binary)));
        }
    }
    binds.push(mount(SOLOS_SWARM, solos_swarm));
    binds
}

/// What a `run_code` sandbox mounts: the config's mounts, then the pstack
/// tree when one is open.
pub fn run_code_mounts(run_code: &RunCodeConfig, tree: Option<&Path>) -> Vec<Mount> {
    let mut mounts = run_code.mounts.clone();
    mounts.extend(tree.map(pstack_bind));
    mounts
}

fn mount(target: &str, source: &Path) -> Mount {
    Mount {
        target: PathBuf::from(target),
        source: source.to_owned(),
    }
}

/// The host paths one serving process adds to its sandboxes, beyond the
/// config's. `Host::default()` names none, which is all a CLI knows from a
/// config alone; `agent serve` names what it runs with.
#[derive(Clone, Debug, Default)]
pub struct Host {
    /// The agent's state directory, canonical. Its tickets directory is
    /// where sessions' run directories and files live.
    pub state_dir: Option<PathBuf>,
    /// The Pi checkout, canonical.
    pub pi_root: Option<PathBuf>,
    /// The node binary, canonical.
    pub node: Option<PathBuf>,
    /// The pstack tree `Pstack::open` checked, canonical. A link at
    /// `vendor/pstack` makes it a tree outside `pi_root`, and the sandboxes
    /// bind that tree, so the view holds it by itself.
    pub pstack: Option<PathBuf>,
}

/// Every host path some sandbox of one config and host may read.
pub struct Exposure {
    visible: Vec<PathBuf>,
}

impl Exposure {
    pub fn new(config: &AgentConfig, host: &Host) -> Self {
        let mut visible = Vec::new();
        if let ExecutorConfig::Pi {
            run_code, claude, ..
        } = &config.executor
        {
            if let Some(run_code) = run_code {
                visible.extend(run_code.mounts.iter().map(|mount| mount.source.clone()));
                visible.extend(run_code.uv_root.clone());
            }
            if let Some(claude) = claude {
                visible.extend(claude_dirs(claude));
            }
        }
        visible.extend(host.state_dir.as_ref().map(|state| state.join(TICKETS_DIR)));
        visible.extend(host.node.as_deref().map(|node| node_root(node).to_owned()));
        visible.extend(host.pi_root.clone());
        visible.extend(host.pstack.clone());
        Self { visible }
    }

    /// The view the secret-path checks judge against.
    pub fn view(&self) -> SandboxView {
        SandboxView::new(self.visible.clone())
    }
}

/// The directories Claude Code's binds come from. The native build binds
/// its binary alone, but its directory is what the check refuses, as before.
fn claude_dirs(claude: &ClaudeConfig) -> Vec<PathBuf> {
    let binary_dir = claude.binary.parent().unwrap_or(&claude.binary);
    let solos_swarm = claude
        .solos_swarm
        .clone()
        .or_else(|| std::env::current_exe().ok());
    [
        Some(binary_dir.to_owned()),
        claude
            .node
            .as_deref()
            .map(|node| node_root(node).to_owned()),
        solos_swarm,
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::config::AgentConfig;

    const CONFIG: &str = r#"policy: {people: [slack:alice]}
sources: [{kind: jsonl, path: /srv/state/triggers.jsonl}]
channels: [{kind: jsonl, source: slack, path: /srv/state/outbox.jsonl}]
router: {kind: scripted}
executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  max_output_tokens: 2048
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 6000}
  run_code:
    mounts: [{target: /data, source: /srv/data}]
    uv_root: /srv/uv
  claude: {binary: /opt/claude/cli.js, node: /opt/node/bin/node, solos_swarm: /opt/solos/bin/solos-swarm}
vault: {path: /srv/keys/vault.yaml, key_file: /srv/keys/vault.key}
browser: {lightpanda: /opt/lightpanda, audit_key: /srv/keys/trail.key}
budgets: {session_microusd: 500000}
"#;

    fn representative() -> (AgentConfig, Host) {
        let config: AgentConfig = serde_yaml::from_str(CONFIG).unwrap();
        let host = Host {
            state_dir: Some(PathBuf::from("/srv/state")),
            pi_root: Some(PathBuf::from("/srv/pi")),
            node: Some(PathBuf::from("/opt/node/bin/node")),
            pstack: Some(PathBuf::from("/srv/pi/vendor/pstack")),
        };
        (config, host)
    }

    #[test]
    fn no_secret_path_is_under_a_mount_of_a_representative_config() {
        let (config, host) = representative();
        let view = Exposure::new(&config, &host).view();
        let vault = config.vault.as_ref().unwrap();
        let browser = config.browser.as_ref().unwrap();
        let state = Path::new("/srv/state");
        for secret in [
            vault.path.clone(),
            vault.key_file.clone(),
            browser.audit_key.clone(),
            state.join("browser"),
            config.trail.root(state),
        ] {
            assert!(
                view.hides(&secret).is_ok(),
                "{} is under a mount",
                secret.display()
            );
        }
    }

    #[test]
    fn every_bind_a_session_makes_is_in_its_view() {
        let (config, host) = representative();
        let view = Exposure::new(&config, &host).view();
        let pi_root = Path::new("/srv/pi");
        let node = Path::new("/opt/node/bin/node");
        let tree = host.pstack.as_deref().unwrap();
        let mut binds = pi_binds(node, pi_root);
        binds.push(pstack_bind(tree));
        binds.extend(claude_binds(
            Path::new("/opt/claude/cli.js"),
            Some(node),
            Path::new("/opt/solos/bin/solos-swarm"),
        ));
        binds.extend(claude_binds(
            Path::new("/opt/claude/claude"),
            None,
            Path::new("/opt/solos/bin/solos-swarm"),
        ));
        let ExecutorConfig::Pi {
            run_code: Some(run_code),
            ..
        } = &config.executor
        else {
            panic!("the representative config runs code");
        };
        binds.extend(run_code_mounts(run_code, Some(tree)));
        for bind in binds {
            assert!(
                view.hides(&bind.source).is_err(),
                "{} is bound but not visible",
                bind.source.display()
            );
        }
    }

    #[test]
    fn the_pstack_bind_is_the_checked_tree_wherever_it_is_and_it_is_in_the_view() {
        let (config, mut host) = representative();
        // vendor/pstack is a link to a tree outside the Pi checkout.
        let tree = Path::new("/opt/pstack-0.15.36");
        host.pstack = Some(tree.to_owned());

        let bind = pstack_bind(tree);

        assert_eq!(
            (bind.source.as_path(), bind.target.as_path()),
            (tree, Path::new("/pstack"))
        );
        let view = Exposure::new(&config, &host).view();
        assert!(view.hides(&tree.join("skills/poteto-mode")).is_err());
        let without = Exposure::new(&config, &Host::default()).view();
        assert!(
            without.hides(tree).is_ok(),
            "a host that names no pstack exposes none"
        );
        let ExecutorConfig::Pi {
            run_code: Some(run_code),
            ..
        } = &config.executor
        else {
            panic!("the representative config runs code");
        };
        let pstack_mounts = |tree| {
            run_code_mounts(run_code, tree)
                .into_iter()
                .filter(|mount| mount.target == Path::new("/pstack"))
                .count()
        };
        assert_eq!(pstack_mounts(Some(tree)), 1);
        assert_eq!(pstack_mounts(None), 0, "no pstack, no /pstack mount");
    }

    #[test]
    fn without_a_host_only_the_config_names_mounts() {
        let (config, _) = representative();
        let view = Exposure::new(&config, &Host::default()).view();
        assert!(
            view.hides(Path::new("/srv/state/tickets/vault.yaml"))
                .is_ok()
        );
        assert!(view.hides(Path::new("/srv/pi/vendor/x")).is_ok());
        assert!(view.hides(Path::new("/srv/data/x")).is_err());
    }
}
