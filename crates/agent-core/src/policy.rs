//! People and bots are named `source:id` in their Source's own identities
//! (`slack:U024BE7LH`, `github:octocat`); the core never maps one Source's
//! identity to another's.

use crate::ids::key;
use crate::model::{Actor, ActorKind, Mandate, MicroUsd, Ms, Strength, Trigger};
use crate::ports::{Gate, Policy};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// How proactive the agent may be with work nobody asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptMode {
    /// Only explicit requests.
    Off,
    /// Inferred candidates are judged and recorded as "would accept"; nothing
    /// starts.
    #[default]
    Observe,
    /// An inferred candidate that matches enabled rules starts work.
    Auto,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowlistPolicy {
    #[serde(default)]
    pub paused: bool,
    /// People who may ask for work.
    #[serde(default)]
    pub people: BTreeSet<String>,
    /// Bots whose messages may become inferred candidates, never requests.
    #[serde(default)]
    pub bots: BTreeSet<String>,
    /// People who may approve a budget above `auto_approve_microusd`.
    #[serde(default)]
    pub approvers: BTreeSet<String>,
    /// Sources whose explicit messages always get an answer: where the Router would ignore one,
    /// the host starts a session instead. For a one-to-one chat, silence is not an answer.
    #[serde(default)]
    pub answer_sources: BTreeSet<String>,
    #[serde(default)]
    pub accept_mode: AcceptMode,
    /// Enabled proactive rules, by name; their text is Router prompt.
    #[serde(default)]
    pub rules: BTreeSet<String>,
    #[serde(default = "default_auto_approve")]
    pub auto_approve_microusd: MicroUsd,
    #[serde(default = "default_eval_timeout")]
    pub eval_timeout_ms: Ms,
}

fn default_auto_approve() -> MicroUsd {
    2_000_000
}
fn default_eval_timeout() -> Ms {
    120_000
}

impl Default for AllowlistPolicy {
    fn default() -> Self {
        Self {
            paused: false,
            people: BTreeSet::new(),
            bots: BTreeSet::new(),
            approvers: BTreeSet::new(),
            answer_sources: BTreeSet::new(),
            accept_mode: AcceptMode::default(),
            rules: BTreeSet::new(),
            auto_approve_microusd: default_auto_approve(),
            eval_timeout_ms: default_eval_timeout(),
        }
    }
}

fn ident(source: &str, actor: &Actor) -> String {
    format!("{source}:{}", actor.id)
}

impl Policy for AllowlistPolicy {
    /// Covers what a Mandate relies on: who may ask, which bots count, the
    /// accept mode, the rules and the approval threshold. Pausing, approvers
    /// and timeouts do not change it, so they never discard running Evals.
    fn version(&self) -> String {
        let basis = serde_json::json!([
            self.people,
            self.bots,
            self.accept_mode,
            self.rules,
            self.auto_approve_microusd,
        ]);
        key("pv", &[&basis.to_string()])
    }
    fn paused(&self) -> bool {
        self.paused
    }
    fn gate(&self, trigger: &Trigger, has_tickets: bool) -> Gate {
        let who = ident(&trigger.source, &trigger.actor);
        let reject = |reason: &str| Gate::Reject {
            reason: reason.to_owned(),
        };
        let admit = |strength| Gate::Admit { strength };
        match trigger.actor.kind {
            ActorKind::System => admit(Strength::Explicit),
            ActorKind::Bot if !self.bots.contains(&who) => reject("bot_not_listed"),
            ActorKind::Bot if self.accept_mode == AcceptMode::Off => reject("accept_mode_off"),
            ActorKind::Bot => admit(Strength::Inferred),
            ActorKind::Person => {
                let listed = self.people.contains(&who);
                let addressed = trigger.addressed.is_addressed();
                match (listed, addressed, has_tickets) {
                    (true, true, _) | (true, _, true) => admit(Strength::Explicit),
                    (false, _, false) => reject("not_listed"),
                    _ if self.accept_mode == AcceptMode::Off => reject("accept_mode_off"),
                    (true, false, false) | (false, _, true) => admit(Strength::Inferred),
                }
            }
        }
    }
    fn admits_inferred(&self, rules: &[String]) -> bool {
        self.accept_mode == AcceptMode::Auto
            && !rules.is_empty()
            && rules.iter().all(|rule| self.rules.contains(rule))
    }
    fn revalidate(&self, mandate: &Mandate) -> bool {
        match mandate.strength {
            Strength::Explicit => {
                mandate.requester.kind == ActorKind::System
                    || self
                        .people
                        .contains(&ident(&mandate.source, &mandate.requester))
            }
            Strength::Inferred => self.admits_inferred(&mandate.rules),
        }
    }
    fn eval_timeout_ms(&self) -> Ms {
        self.eval_timeout_ms
    }
    fn auto_approve_microusd(&self) -> MicroUsd {
        self.auto_approve_microusd
    }
    fn may_approve(&self, source: &str, actor: &Actor) -> bool {
        self.approvers.contains(&ident(source, actor))
    }
    fn may_stop(&self, source: &str, actor: &Actor) -> bool {
        actor.is_person()
            && (self.may_approve(source, actor) || self.people.contains(&ident(source, actor)))
    }
    /// The people who may stop work: what they write as a memo is what a
    /// person the agent works for said.
    fn may_remember(&self, source: &str, actor: &Actor) -> bool {
        self.may_stop(source, actor)
    }
}
