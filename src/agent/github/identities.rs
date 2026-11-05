//! Listed logins pinned to account ids.
//!
//! A GitHub login can be renamed away and claimed by another account. An
//! approval now merges code and a mention spends budget, so a listed login
//! with another id must not speak with the first account's authority.

use super::{GithubApi, Got};
use crate::sealed;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use solos_agent_core::model::{Actor, ActorKind};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

/// `<state>/github-identities.json`.
pub const IDENTITIES_FILE: &str = "github-identities.json";

/// GitHub logins the Policy lists (`policy.people`, `approvers`, `bots`),
/// pinned to the numeric account id each had when the agent first looked it
/// up.
///
/// A GitHub user object whose login is listed and whose id equals the pin is
/// `Actor { id: login }`, lowercased, as the Policy spells it. One whose id
/// differs is `Actor { id: "<login>#<id>" }`, which no Policy lists, and the
/// mismatch is logged once. A listed login with no pin yet (its lookup failed)
/// is treated the same way until the pin exists. Unlisted logins pass through
/// unpinned, since they are not admitted anyway.
///
/// Pins are made at start by `GET /users/<login>` for every listed login
/// without one, written with mode 0600 all or nothing, and never change by
/// themselves: re-listing a person who recreated their account means deleting
/// their pin from the file.
pub struct Identities {
    path: PathBuf,
    /// Lowercased `github:` logins from the Policy, bot accounts left out
    /// (nobody can claim a `[bot]` login).
    listed: BTreeSet<String>,
    pins: Mutex<BTreeMap<String, u64>>,
    /// Logins whose mismatch or missing pin was already said.
    said: Mutex<BTreeSet<String>>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pinned {
    pins: BTreeMap<String, u64>,
}

impl Identities {
    /// The pins in `path`, if it exists. `listed` are the Policy's logins,
    /// without the `github:` prefix.
    pub fn load(path: PathBuf, listed: BTreeSet<String>) -> Result<Self> {
        let pins = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice::<Pinned>(&bytes)
                    .with_context(|| {
                        format!(
                            "{} is not the GitHub account pins; delete it to pin again",
                            path.display()
                        )
                    })?
                    .pins
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        let listed = listed
            .into_iter()
            .map(|login| login.to_ascii_lowercase())
            .filter(|login| !login.ends_with("[bot]"))
            .collect();
        Ok(Self {
            path,
            listed,
            pins: Mutex::new(pins),
            said: Mutex::default(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The pin of each listed login that has one.
    pub fn pins(&self) -> BTreeMap<String, u64> {
        self.pins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The listed logins that have no pin yet.
    pub fn missing(&self) -> Vec<String> {
        let pins = self.pins.lock().unwrap_or_else(PoisonError::into_inner);
        self.listed
            .iter()
            .filter(|login| !pins.contains_key(*login))
            .cloned()
            .collect()
    }

    /// Look up and pin every listed login without a pin. A failure leaves
    /// that login unpinned, and so not admitted, and is tried again at the
    /// next call; the pin file is written once, whole, when any pin is new.
    pub(crate) async fn pin_missing(&self, api: &GithubApi) -> Result<()> {
        let mut added = false;
        for login in self.missing() {
            let id = match api.get_public(&format!("users/{login}")).await {
                Ok(Got::Ok(page)) => page.body["id"].as_u64(),
                Ok(Got::Refused { status, message }) => {
                    self.say(&login, || {
                        format!(
                            "policy lists github:{login}, whose account could not be looked up \
                             (HTTP {status}: {message}); nobody is admitted under that login \
                             until it can"
                        )
                    });
                    None
                }
                Ok(Got::NotModified) => None,
                Err(err) => {
                    // A wait or a rate limit: the next call tries again.
                    eprintln!("agent: github: cannot pin github:{login}: {err:#}");
                    None
                }
            };
            if let Some(id) = id {
                self.pins
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(login, id);
                added = true;
            }
        }
        if added {
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let pinned = Pinned { pins: self.pins() };
        let bytes = serde_json::to_vec_pretty(&pinned)?;
        sealed::private_file(&self.path, &bytes, true)
            .with_context(|| format!("cannot write {}", self.path.display()))
    }

    /// The `Actor` a GitHub user object (`login`, `id`, `type`) is. A `type`
    /// of `Bot` or a `[bot]` suffix is `ActorKind::Bot`. Every place that turns
    /// a payload's user into an actor goes through here.
    pub fn actor(&self, user: &Value) -> Option<Actor> {
        let login = user["login"].as_str()?.to_ascii_lowercase();
        let kind = if user["type"] == "Bot" || login.ends_with("[bot]") {
            ActorKind::Bot
        } else {
            ActorKind::Person
        };
        let id = if self.listed.contains(&login) {
            let seen = user["id"].as_u64();
            let pinned = self
                .pins
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&login)
                .copied();
            match (pinned, seen) {
                (Some(pin), Some(seen)) if pin == seen => login,
                (pinned, seen) => {
                    let seen = seen.map_or_else(|| "unknown".to_owned(), |id| id.to_string());
                    self.say(&login, || match pinned {
                        Some(pin) => format!(
                            "github:{login} is pinned to account {pin}, but a comment came from \
                             account {seen}; it is {login}#{seen} here and is admitted by nobody"
                        ),
                        None => format!(
                            "github:{login} has no pinned account yet, so account {seen} is \
                             {login}#{seen} here and is admitted by nobody"
                        ),
                    });
                    format!("{login}#{seen}")
                }
            }
        } else {
            login
        };
        Some(Actor {
            id,
            kind,
            name: None,
        })
    }

    /// Print `what` once per login.
    fn say(&self, login: &str, what: impl FnOnce() -> String) {
        let first = self
            .said
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(login.to_owned());
        if first {
            eprintln!("agent: github: {}", what());
        }
    }
}
