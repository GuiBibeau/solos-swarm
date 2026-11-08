//! A repository's model policy: which providers may read its text, in which
//! role. Pure: built once from config, and asked by every path that would show
//! a model a repository's text. Nothing here does I/O.
//!
//! `forge.repos.<repo>.models` is required on every repository, so no
//! repository reaches a provider by default. The providers are
//! `deepseek`, `venice` and `anthropic`; an upstream runs on `deepseek` or
//! `venice` in this build.

use super::config::{ExecutorConfig, HarnessKind, UpstreamConfig};
use crate::gateway::{Wire, endpoint, upstream_profile};
use anyhow::{Context, Result, bail, ensure};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use solos_agent_core::change::RepoName;
use std::collections::{BTreeMap, BTreeSet};

/// Who receives the text and bills for it. The policy keys on this alone:
/// the wire and the harness do not change who reads the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Deepseek,
    Venice,
    /// Admitted in a repository's `allow` from this release, but no upstream
    /// runs on it until its transport and key land.
    Anthropic,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deepseek => "deepseek",
            Self::Venice => "venice",
            Self::Anthropic => "anthropic",
        }
    }

    fn named(name: &str) -> Result<Self> {
        match name {
            "deepseek" => Ok(Self::Deepseek),
            "venice" => Ok(Self::Venice),
            "anthropic" => Ok(Self::Anthropic),
            other => bail!("provider outside the deployment allowlist: {other}"),
        }
    }

    /// The providers an upstream may run on here. Anthropic is a valid entry
    /// in a repository's `allow`, but its transport is a later change.
    fn upstream_here(self) -> Result<Self> {
        ensure!(
            self != Self::Anthropic,
            "provider anthropic is not admitted yet: an upstream runs on deepseek or venice"
        );
        Ok(self)
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What reads a repository's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// The Router's cards, its `router_read`, and its system prompt.
    Router,
    /// The repository's Tickets' workers, and any worker shown a memo that
    /// carries it.
    Worker,
    /// The memory review and the dream.
    Memory,
}

impl Role {
    pub const ALL: [Self; 3] = [Self::Router, Self::Worker, Self::Memory];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Router => "router",
            Self::Worker => "worker",
            Self::Memory => "memory",
        }
    }
}

/// An upstream's name: `default` (the executor's own fields) or a key of
/// `executor.upstreams`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(transparent)]
pub struct UpstreamName(String);

impl UpstreamName {
    pub const DEFAULT: &'static str = "default";

    pub fn default_upstream() -> Self {
        Self(Self::DEFAULT.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_default(&self) -> bool {
        self.0 == Self::DEFAULT
    }
}

impl std::fmt::Display for UpstreamName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `forge.repos.<repo>.models`. Required on every repository: no repository
/// reaches any provider by default.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsConfig {
    pub allow: Allow,
    /// The upstream its workers run on. Without it: `default` when its
    /// provider is allowed, else the one allowed upstream.
    #[serde(default)]
    pub worker: Option<UpstreamName>,
}

/// A list applies to every role; a map names all three.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Allow {
    Every(BTreeSet<Provider>),
    PerRole(PerRole),
}

impl Allow {
    fn per_role(&self) -> PerRole {
        match self {
            Self::Every(allowed) => PerRole {
                router: allowed.clone(),
                worker: allowed.clone(),
                memory: allowed.clone(),
            },
            Self::PerRole(roles) => roles.clone(),
        }
    }
}

impl<'de> Deserialize<'de> for Allow {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = serde_yaml::Value::deserialize(deserializer)?;
        match value {
            serde_yaml::Value::Sequence(_) => BTreeSet::<Provider>::deserialize(value)
                .map(Self::Every)
                .map_err(D::Error::custom),
            serde_yaml::Value::Mapping(_) => PerRole::deserialize(value)
                .map(Self::PerRole)
                .map_err(D::Error::custom),
            _ => Err(D::Error::custom(
                "allow is a list of providers, or a map naming `router`, `worker` and `memory`",
            )),
        }
    }
}

/// The providers each role allows, where `allow` is a map.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerRole {
    pub router: BTreeSet<Provider>,
    pub worker: BTreeSet<Provider>,
    pub memory: BTreeSet<Provider>,
}

impl PerRole {
    pub fn allows(&self, role: Role) -> &BTreeSet<Provider> {
        match role {
            Role::Router => &self.router,
            Role::Worker => &self.worker,
            Role::Memory => &self.memory,
        }
    }
}

/// The policy every read path asks. Built once from config.
#[derive(Debug)]
pub struct ModelPolicy {
    repos: BTreeMap<RepoName, PerRole>,
    /// Every upstream here by name, `default` included, and the provider it
    /// runs on.
    upstreams: BTreeMap<UpstreamName, Provider>,
    /// Each repository's worker upstream, or `None` where no upstream here
    /// is allowed: its Tickets have no upstream to run on.
    workers: BTreeMap<RepoName, Option<UpstreamName>>,
}

impl ModelPolicy {
    /// Build and check the policy: every repository names `models`, a role
    /// map names all three roles, a provider is admitted, and each
    /// repository's worker upstream resolves.
    pub fn from_config(config: &super::AgentConfig) -> Result<Self> {
        let upstreams = Self::upstreams(config)?;
        let mut repos = BTreeMap::new();
        let mut workers = BTreeMap::new();
        let Some(forge) = &config.forge else {
            return Ok(Self {
                repos,
                upstreams,
                workers,
            });
        };
        let candidates: Vec<(UpstreamName, Provider)> = upstreams
            .iter()
            .map(|(name, provider)| (name.clone(), *provider))
            .collect();
        for (repo, settings) in forge.repos() {
            let models = settings.models.as_ref().with_context(|| {
                format!(
                    "forge.repos.{repo} names no models: every repository says which providers \
                     its text may reach, and none reaches any by default. Add \
                     `models: {{allow: [deepseek]}}` under it; see \
                     docs/agent-mode/configuration.md#repository-models"
                )
            })?;
            let roles = models.allow.per_role();
            let worker = worker_upstream(repo, models, &roles, &candidates)?;
            repos.insert(repo.clone(), roles);
            workers.insert(repo.clone(), worker);
        }
        Ok(Self {
            repos,
            upstreams,
            workers,
        })
    }

    /// The named upstreams and the default one, checked, where the executor
    /// is Pi.
    fn upstreams(config: &super::AgentConfig) -> Result<BTreeMap<UpstreamName, Provider>> {
        let ExecutorConfig::Pi {
            provider,
            upstreams,
            claude,
            ..
        } = &config.executor
        else {
            return Ok(BTreeMap::new());
        };
        let mut checked = BTreeMap::new();
        for (name, upstream) in upstreams {
            ensure!(
                !name.is_default(),
                "executor.upstreams names `default`, which is the executor's own upstream"
            );
            validate_upstream(name, upstream, claude.is_some())?;
            checked.insert(name.clone(), upstream.provider);
        }
        let default = Provider::named(provider)?.upstream_here()?;
        checked.insert(UpstreamName::default_upstream(), default);
        Ok(checked)
    }

    /// Whether `repo`'s text may reach `provider` in `role`. `None` (no
    /// repository) is always admitted: such text is a person's own.
    pub fn admits(&self, repo: Option<&RepoName>, role: Role, provider: Provider) -> bool {
        repo.is_none_or(|repo| {
            self.repos
                .get(repo)
                .is_some_and(|roles| roles.allows(role).contains(&provider))
        })
    }

    /// The providers `repo` allows in `role`. `None` for a repository the
    /// config does not name.
    pub fn allowed(&self, repo: &RepoName, role: Role) -> Option<&BTreeSet<Provider>> {
        self.repos.get(repo).map(|roles| roles.allows(role))
    }

    /// The upstream `repo`'s workers run on, or `None` where no upstream
    /// here is allowed: its Tickets are declined.
    pub fn worker_upstream(&self, repo: &RepoName) -> Option<&UpstreamName> {
        self.workers.get(repo).and_then(Option::as_ref)
    }

    /// The provider an upstream runs on, `default` included.
    pub fn upstream_provider(&self, upstream: &UpstreamName) -> Option<Provider> {
        self.upstreams.get(upstream).copied()
    }

    /// What `agent serve` says at start: each repository's allowed providers
    /// per role, its worker upstream, and the repositories none here may run.
    pub fn start_report(&self) -> Vec<String> {
        self.repos
            .iter()
            .map(|(repo, roles)| {
                let mut line = format!(
                    "{repo}: router [{}], worker [{}], memory [{}]",
                    providers(roles.allows(Role::Router)),
                    providers(roles.allows(Role::Worker)),
                    providers(roles.allows(Role::Memory)),
                );
                match self.worker_upstream(repo) {
                    Some(upstream) => {
                        let provider = self
                            .upstream_provider(upstream)
                            .map_or_else(|| "?".to_owned(), |provider| provider.to_string());
                        line.push_str(&format!(", workers run on {upstream} ({provider})"));
                    }
                    None => line.push_str(", no upstream here may run its workers"),
                }
                line
            })
            .collect()
    }
}

fn validate_upstream(name: &UpstreamName, upstream: &UpstreamConfig, claude: bool) -> Result<()> {
    let context = || format!("executor.upstreams.{name}");
    let provider = upstream.provider.upstream_here().with_context(context)?;
    upstream_profile(provider.as_str(), &upstream.model).with_context(context)?;
    endpoint(provider.as_str(), upstream.wire).with_context(context)?;
    ensure!(
        upstream.harness == HarnessKind::ClaudeCode || upstream.wire == Wire::Chat,
        "{}: harness pi needs wire: chat; wire: anthropic needs harness: claude-code",
        context()
    );
    ensure!(
        upstream.harness != HarnessKind::ClaudeCode || claude,
        "{}: harness claude-code needs executor.claude",
        context()
    );
    upstream
        .prices
        .validate()
        .with_context(|| format!("{}.prices", context()))?;
    ensure!(
        upstream
            .max_input_bytes
            .is_none_or(|max| (1..=16_777_216).contains(&max)),
        "{}.max_input_bytes must be 1..=16777216",
        context()
    );
    ensure!(
        upstream
            .max_output_tokens
            .is_none_or(|max| (1..=393_216).contains(&max)),
        "{}.max_output_tokens must be 1..=393216",
        context()
    );
    Ok(())
}

/// Resolve one repository's worker upstream from its allow list and the
/// upstreams here.
fn worker_upstream(
    repo: &RepoName,
    models: &ModelsConfig,
    roles: &PerRole,
    candidates: &[(UpstreamName, Provider)],
) -> Result<Option<UpstreamName>> {
    let allowed = || providers(&roles.worker);
    if let Some(name) = &models.worker {
        let Some((_, provider)) = candidates.iter().find(|(candidate, _)| candidate == name) else {
            if name.is_default() {
                bail!(
                    "forge.repos.{repo}.models.worker names `default`, and executor here is not \
                     pi, so there is no default upstream"
                );
            }
            bail!(
                "forge.repos.{repo}.models.worker names {name}, which is not a key of \
                 executor.upstreams"
            );
        };
        ensure!(
            roles.worker.contains(provider),
            "forge.repos.{repo}.models.worker names {name}, which runs on {provider}, and its \
             worker role allows only {}",
            allowed()
        );
        return Ok(Some(name.clone()));
    }
    let runnable: Vec<&(UpstreamName, Provider)> = candidates
        .iter()
        .filter(|(_, provider)| roles.worker.contains(provider))
        .collect();
    match runnable.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some(only.0.clone())),
        many => {
            let named = many
                .iter()
                .map(|(name, provider)| format!("{name} ({provider})"))
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "forge.repos.{repo}.models.worker is absent, and {} upstreams here may run its \
                 workers ({named}); name one with models.worker",
                many.len()
            )
        }
    }
}

fn providers(allowed: &BTreeSet<Provider>) -> String {
    if allowed.is_empty() {
        "none".to_owned()
    } else {
        allowed
            .iter()
            .map(|provider| provider.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::{ModelPolicy, Provider, Role, UpstreamName};
    use crate::agent::AgentConfig;
    use solos_agent_core::change::RepoName;

    fn config(forge: &str, executor_extra: &str) -> AgentConfig {
        let yaml = format!(
            "policy: {{people: [slack:alice]}}
sources: [{{kind: jsonl, path: triggers.jsonl}}]
channels: [{{kind: jsonl, source: slack, path: outbox.jsonl}}]
router: {{kind: scripted}}
executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {{input_microusd_per_million: 300000, output_microusd_per_million: 1200000}}
  run_code: {{}}
{executor_extra}forge:
  kind: local
  remote: remotes
  identity: {{name: solos-bot, email: bot@example.com}}
  repos:
{forge}"
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    fn build(forge: &str, executor_extra: &str) -> anyhow::Result<ModelPolicy> {
        ModelPolicy::from_config(&config(forge, executor_extra))
    }

    fn repo(name: &str) -> RepoName {
        RepoName::try_from(name.to_owned()).unwrap()
    }

    #[test]
    fn a_list_applies_to_every_role_and_no_repository_is_admitted_by_default() {
        let policy = build(
            "    acme/widgets: {about: Widgets., base: main, models: {allow: [deepseek]}}",
            "",
        )
        .unwrap();
        let widgets = repo("acme/widgets");
        for role in Role::ALL {
            assert!(policy.admits(Some(&widgets), role, Provider::Deepseek));
            assert!(!policy.admits(Some(&widgets), role, Provider::Venice));
            assert!(!policy.admits(Some(&widgets), role, Provider::Anthropic));
        }
        assert!(
            policy.admits(None, Role::Router, Provider::Anthropic),
            "a person's own text names no repository, so every provider may read it"
        );
        assert!(
            !policy.admits(
                Some(&repo("acme/unknown")),
                Role::Worker,
                Provider::Deepseek
            ),
            "a repository the config does not name reaches nobody"
        );
    }

    #[test]
    fn a_role_map_answers_each_role_on_its_own() {
        let policy = build(
            "    acme/widgets:
      about: Widgets.
      base: main
      models:
        allow: {router: [deepseek], worker: [anthropic], memory: [venice]}",
            "",
        )
        .unwrap();
        let widgets = repo("acme/widgets");
        assert!(policy.admits(Some(&widgets), Role::Router, Provider::Deepseek));
        assert!(!policy.admits(Some(&widgets), Role::Router, Provider::Anthropic));
        assert!(policy.admits(Some(&widgets), Role::Worker, Provider::Anthropic));
        assert!(!policy.admits(Some(&widgets), Role::Worker, Provider::Deepseek));
        assert!(policy.admits(Some(&widgets), Role::Memory, Provider::Venice));
        assert!(!policy.admits(Some(&widgets), Role::Memory, Provider::Deepseek));
    }

    #[test]
    fn an_empty_allow_reaches_nobody_and_still_loads() {
        let policy = build(
            "    acme/widgets: {about: Widgets., base: main, models: {allow: []}}",
            "",
        )
        .unwrap();
        let widgets = repo("acme/widgets");
        for role in Role::ALL {
            for provider in [Provider::Deepseek, Provider::Venice, Provider::Anthropic] {
                assert!(!policy.admits(Some(&widgets), role, provider));
            }
        }
        assert_eq!(policy.worker_upstream(&widgets), None);
    }

    #[test]
    fn the_worker_upstream_resolves_from_the_allow_list() {
        let named = "  upstreams:
    venice:
      provider: venice
      model: zai-org-glm-5
      wire: chat
      harness: pi
      key_file: keys/venice.key
      prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}
";
        // The default is the only upstream its worker role allows.
        let policy = build(
            "    acme/widgets: {about: Widgets., base: main, models: {allow: [deepseek]}}",
            "",
        )
        .unwrap();
        let widgets = repo("acme/widgets");
        assert_eq!(
            policy.worker_upstream(&widgets).map(UpstreamName::as_str),
            Some(UpstreamName::DEFAULT)
        );
        assert_eq!(
            policy.upstream_provider(&UpstreamName::default_upstream()),
            Some(Provider::Deepseek)
        );

        // A repository that names its upstream.
        let policy = build(
            "    acme/widgets:
      about: Widgets.
      base: main
      models: {allow: [deepseek, venice], worker: venice}",
            named,
        )
        .unwrap();
        assert_eq!(
            policy.worker_upstream(&widgets).map(UpstreamName::as_str),
            Some("venice")
        );

        // One allowed upstream, so it is the one.
        let policy = build(
            "    acme/widgets: {about: Widgets., base: main, models: {allow: [venice]}}",
            named,
        )
        .unwrap();
        assert_eq!(
            policy.worker_upstream(&widgets).map(UpstreamName::as_str),
            Some("venice")
        );

        // None here: the repository still loads, and nobody may run it here.
        let policy = build(
            "    acme/widgets: {about: Widgets., base: main, models: {allow: [anthropic]}}",
            named,
        )
        .unwrap();
        assert_eq!(policy.worker_upstream(&widgets), None);
        assert!(policy.start_report()[0].contains("no upstream here may run its workers"));
    }

    #[test]
    fn two_allowed_upstreams_and_no_worker_is_refused() {
        let refused = build(
            "    acme/widgets: {about: Widgets., base: main, models: {allow: [deepseek, venice]}}",
            "  upstreams:
    venice:
      provider: venice
      model: zai-org-glm-5
      wire: chat
      harness: pi
      key_file: keys/venice.key
      prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}
",
        )
        .unwrap_err();
        let said = format!("{refused:#}");
        assert!(said.contains("models.worker is absent"), "{said}");
        assert!(said.contains("name one with models.worker"), "{said}");
    }

    #[test]
    fn a_worker_upstream_outside_the_allow_list_or_the_config_is_refused() {
        let named = "  upstreams:
    venice:
      provider: venice
      model: zai-org-glm-5
      wire: chat
      harness: pi
      key_file: keys/venice.key
      prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}
";
        let said = |forge: &str, extra: &str| {
            format!(
                "{:#}",
                build(forge, extra).expect_err("the policy was admitted")
            )
        };
        let outside = said(
            "    acme/widgets:
      about: Widgets.
      base: main
      models: {allow: [deepseek], worker: venice}",
            named,
        );
        assert!(
            outside.contains("runs on venice")
                && outside.contains("worker role allows only deepseek"),
            "{outside}"
        );
        let unknown = said(
            "    acme/widgets:
      about: Widgets.
      base: main
      models: {allow: [deepseek], worker: claude}",
            "",
        );
        assert!(
            unknown.contains("not a key of executor.upstreams"),
            "{unknown}"
        );
    }
}
