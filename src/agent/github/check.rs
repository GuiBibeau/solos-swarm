//! `solos-swarm agent github check`: whether the App, as configured, can do
//! what the agent asks of it, one line at a time, each naming what to do when
//! it cannot.

use super::{
    Access, AppId, AppKey, GithubApi, Got, IDENTITIES_FILE, Identities, InstallState, Minter,
    MinterConfig,
};
use crate::agent::config::AgentConfig;
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

pub struct CheckArgs {
    /// The agent's `agent.yaml`.
    pub config: PathBuf,
    /// The key to check. By default `github.key_file`; on the box, where
    /// `serve` has deleted that copy, the durable one in `~/agent/tokens`.
    pub key_file: Option<PathBuf>,
    /// Where the account pins live. Without it, accounts are looked up and
    /// shown, and nothing is written.
    pub state_dir: Option<PathBuf>,
}

/// Check the App against GitHub and print the table to `out`. Nothing is
/// written anywhere except missing account pins, and only with a state
/// directory; the key file is read, never deleted. Returns whether every
/// check passed.
pub async fn check(args: CheckArgs, out: &mut dyn Write) -> Result<bool> {
    let config = AgentConfig::load(&args.config)?;
    let Some(github) = &config.github else {
        bail!("the config has no github: section; see docs/agent-mode/how-to/github-app.md");
    };
    let Some(forge) = &config.forge else {
        bail!("the config has no forge: section, whose repos are the repositories the App acts on");
    };
    let repos: Vec<_> = forge.repos().keys().cloned().collect();
    let key_file = args.key_file.unwrap_or_else(|| github.key_file.clone());
    let key = AppKey::read(&key_file)?;
    let minter = Minter::new(
        MinterConfig::new(
            AppId(github.app_id),
            &github.api_base,
            github.writes,
            repos.iter().cloned().collect(),
        ),
        key,
        Arc::new(SystemTime::now),
    )?;
    let minter = Arc::new(minter);
    let mut ok = true;
    let mut say = |line: String| -> Result<()> {
        writeln!(out, "  {line}")?;
        Ok(())
    };

    let slug = match minter.slug().await {
        Ok(slug) => slug.clone(),
        Err(err) => {
            say(format!("FAIL {err:#}"))?;
            return Ok(false);
        }
    };
    say(format!(
        "app {slug} ({}) acts as {slug}[bot]; people write @{slug}",
        github.app_id
    ))?;

    let listed = config.github_logins();
    let identities = match &args.state_dir {
        Some(dir) => Identities::load(dir.join(IDENTITIES_FILE), listed.clone())?,
        None => Identities::load(PathBuf::new(), listed.clone())?,
    };
    let api = GithubApi::new(minter.clone(), Arc::new(identities))?;

    let found = match minter.preflight(&repos).await {
        Ok(found) => found,
        Err(err) => {
            say(format!("FAIL {err:#}"))?;
            return Ok(false);
        }
    };
    for repo in found {
        let name = &repo.repo;
        match repo.state {
            InstallState::NotInstalled { install_url } => {
                ok = false;
                say(format!(
                    "{name}  FAIL the App is not installed on it; install it at {install_url}"
                ))?;
            }
            InstallState::Suspended => {
                ok = false;
                say(format!(
                    "{name}  FAIL the App is suspended on it; unsuspend it under Settings, \
                     Applications, Installed GitHub Apps (the agent waits until then)"
                ))?;
            }
            InstallState::Installed {
                installation,
                permissions,
                missing,
                extra,
            } => {
                let of = |level: &str| -> String {
                    permissions
                        .iter()
                        .filter(|(_, held)| held == level)
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                say(format!(
                    "{name}  installation {installation}  write: {}",
                    of("write")
                ))?;
                let absent = |permission: &str| {
                    if extra.iter().any(|held| held == permission) {
                        "PRESENT (refused)"
                    } else {
                        "absent (good)"
                    }
                };
                say(format!(
                    "{:width$}  read: {}   workflows: {}  administration: {}",
                    "",
                    of("read"),
                    absent("workflows"),
                    absent("administration"),
                    width = name.as_str().len()
                ))?;
                for lacks in &missing {
                    ok = false;
                    say(format!(
                        "{name}  FAIL the installation lacks {lacks}; add it under the App's \
                         permissions (it is in manifest.json) and accept the new permissions"
                    ))?;
                }
                for held in &extra {
                    ok = false;
                    say(format!(
                        "{name}  FAIL the installation holds {held}; remove it (the agent never \
                         edits CI or branch protection)"
                    ))?;
                }
                let terms = &forge.repos()[name];
                let mut tried = String::new();
                for access in [Access::Read, Access::Write] {
                    let kind = if access == Access::Read {
                        "read"
                    } else {
                        "write"
                    };
                    if access == Access::Write && !github.writes {
                        tried.push_str("  mint write skipped (github.writes: false)");
                        continue;
                    }
                    match minter.token(name, access).await {
                        Ok(_) if access == Access::Write => {
                            tried.push_str("  mint write ok (unused here)");
                        }
                        Ok(_) => tried.push_str(&format!("  mint {kind} ok")),
                        Err(err) => {
                            ok = false;
                            tried.push_str(&format!("  mint {kind} FAIL {err:#}"));
                        }
                    }
                }
                let base_url = api.url(&format!("repos/{name}/branches/{}", terms.base));
                let base = match api.get(name, &base_url, None).await {
                    Ok(Got::Ok(_)) => format!("  base {} exists", terms.base),
                    Ok(Got::Refused { status, message }) => {
                        ok = false;
                        format!("  base {} FAIL HTTP {status}: {message}", terms.base)
                    }
                    Ok(Got::NotModified) => String::new(),
                    Err(err) => {
                        ok = false;
                        format!("  base {} FAIL {err:#}", terms.base)
                    }
                };
                say(format!(
                    "{:width$} {tried}{base}",
                    "",
                    width = name.as_str().len()
                ))?;
            }
        }
    }

    match minter.bot().await {
        Ok(bot) => say(format!("commits are by {} <{}>", bot.login, bot.email))?,
        Err(err) => {
            ok = false;
            say(format!(
                "FAIL the bot's account could not be learned: {err:#}"
            ))?;
        }
    }

    for login in &listed {
        if login.ends_with("[bot]") {
            continue;
        }
        let account = match api.get_public(&format!("users/{login}")).await {
            Ok(Got::Ok(page)) => page.body["id"].as_u64(),
            _ => None,
        };
        let Some(account) = account else {
            ok = false;
            say(format!(
                "github:{login}  FAIL the account could not be looked up; nobody is admitted \
                 under that login until it can"
            ))?;
            continue;
        };
        let Some(dir) = &args.state_dir else {
            say(format!(
                "github:{login} is account {account} (not stored: pass --state-dir to pin it)"
            ))?;
            continue;
        };
        let identities = api.identities();
        if !identities.pins().contains_key(login) {
            identities
                .pin_missing(&api)
                .await
                .with_context(|| format!("cannot pin github:{login}"))?;
        }
        let file = dir.join(IDENTITIES_FILE);
        match identities.pins().get(login) {
            Some(pinned) if *pinned == account => say(format!(
                "github:{login} pinned to account {account} ({})",
                file.display()
            ))?,
            Some(pinned) => {
                ok = false;
                say(format!(
                    "github:{login}  FAIL pinned to account {pinned}, but the login is now \
                     account {account}; if its owner recreated the account, delete the pin in {}",
                    file.display()
                ))?;
            }
            None => {
                ok = false;
                say(format!(
                    "github:{login}  FAIL the account could not be pinned"
                ))?;
            }
        }
    }
    say(if ok {
        "ok".to_owned()
    } else {
        "FAILED".to_owned()
    })?;
    Ok(ok)
}
