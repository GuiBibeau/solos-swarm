//! Merge queue, config and the Router's tool (src/agent/config.rs,
//! src/agent/broker.rs, src/agent/broker/registry.rs,
//! extensions/agent-tools.json). Documentation, not compiled code.

use serde::Deserialize;

/// `forge.repos.<repo>.queue`. Its presence is the owner's opt-in: without
/// it, `router_decide` refuses `plan: queue` for the repository.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueConfig {
    /// Pull requests one queue may name, 1 to 30.
    #[serde(default = "max_pulls")]
    pub max_pulls: u32,
    /// Host-made updates of one pull request before the queue stops, 1 to 10.
    #[serde(default = "max_updates")]
    pub max_updates: u32,
    /// Who meets a conflict: a worker's resolve round, or a person.
    #[serde(default)]
    pub resolve: ResolveConfig,
    /// Resolve rounds per pull request, 0 to 5.
    #[serde(default = "max_rounds")]
    pub max_rounds: u32,
    /// A head with no check reported for this long stops, 300 to 86400.
    #[serde(default = "ci_wait_secs")]
    pub ci_wait_secs: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolveConfig {
    #[default]
    Worker,
    Stop,
}

fn max_pulls() -> u32 {
    10
}
fn max_updates() -> u32 {
    3
}
fn max_rounds() -> u32 {
    2
}
fn ci_wait_secs() -> u64 {
    3_600
}

/// Config load refuses, each with one sentence: `queue` on `forge: local`;
/// with `owes` below `pull_request`; with `merge: off`; with
/// `github.writes: false`; a field out of range. `ci: none` with `queue`
/// loads and logs that merges then wait on a yes alone.
pub fn validate(_queue: &QueueConfig) -> anyhow::Result<()> {
    todo!()
}

// ---------------------------------------------------------------------------
// router_decide. `plan` gains "queue"; one new argument:
//
//   "pulls": {
//     "type": "array", "items": {"type": "integer", "minimum": 1},
//     "description": "accept with plan queue: the pull requests to merge into
//       the repository's base, in the order the person asked for, by number."
//   }
//
// `DecideArgs::verdict` (broker.rs:1841-1981) for `plan: queue`:
//   - needs `repo`, refuses `deliverable`, `agents`, `goal_template` and
//     `environment`;
//   - refuses a repository without `queue:`, or with `merge: off`;
//   - refuses `pulls` that is empty, repeats a number, or is longer than
//     `max_pulls`;
//   - fills `Accept.forge` with `ForgeTerms` and `QueueTerms` from config
//     (never from the model), as broker.rs:1949-1961 does for a pull request.
// At accept, the Engine refuses (records Ignore with the sentence) a queue for
// a requester who may not approve, a Thread whose channels cannot show
// buttons, or a repository that already has an open queue.
// ---------------------------------------------------------------------------

// The production shape (`~/agent/agent.yaml` on the box):
//
// forge:
//   kind: github
//   repos:
//     GuiBibeau/regimes:
//       about: Regime-switching research
//       base: main
//       owes: pull_request
//       merge: ask
//       ci: expected
//       queue: {max_pulls: 10, max_updates: 3, resolve: worker, max_rounds: 2, ci_wait_secs: 3600}
//
// Tests (src/agent/config.rs, src/agent/broker.rs):
//  - each refusal above with its sentence; the defaults; `queue: {}` loads.
//  - router_decide: a queue accept carries QueueTerms from config whatever
//    the model sent; each refusal above; the tool schema and fixture agree
//    (the existing drift test, with the fixture updated for this one tool).
