//! The bench's workload: a seeded stream of group-chat and CI Triggers with
//! work requests among them, in workload time from 0. The same spec and
//! seed always give the same stream.

use crate::agent::simulated::TimeFactor;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use solos_agent_core::model::{Actor, Addressed, ChannelRef, ThreadKey, Trigger, TriggerId};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

/// The Source every bench Trigger names, and the Channel that answers it.
pub const SOURCE: &str = "bench";
/// The CI bot on `policy.bots`.
pub const CI_BOT: &str = "ci-bot";
/// A bot that is not on the list, so the gate turns it away.
pub const UNLISTED_BOT: &str = "deploy-bot";

const MINUTE_MS: f64 = 60_000.0;
/// Chatter that replies picks one of this many latest chat Threads.
const RECENT_CHATS: usize = 5;
/// Where in its request's expected work a follow-up and a CI message land.
const FOLLOW_UP_WINDOW: (f64, f64) = (0.1, 0.5);
const CI_WINDOW: (f64, f64) = (0.2, 0.9);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arrivals {
    /// Exponential gaps: a Poisson stream at `per_minute`.
    Poisson,
    /// One message every `1 / per_minute` minutes.
    Even,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Span {
    pub min: f64,
    pub max: f64,
}

/// What `agent bench` generates. Every field has a default, shaped like the
/// article's workload: about 300 messages with about 20 work requests.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkloadSpec {
    /// Run `n` (from 0) of each design uses `seed + n`, so designs compare
    /// on the same streams.
    pub seed: u64,
    /// Every Trigger in the stream, requests included.
    pub messages: usize,
    /// Addressed work requests from allowed people, each on its own Thread.
    pub requests: usize,
    /// Replies by a request's author on its Thread while the work runs.
    pub follow_ups: usize,
    /// CI and bot messages on request Threads.
    pub ci_messages: usize,
    /// Messages per workload minute.
    pub per_minute: f64,
    pub arrivals: Arrivals,
    /// Each request's expected work, in workload minutes, drawn uniformly.
    pub work_minutes: Span,
    /// People on `policy.people`.
    pub people: usize,
    /// People not on it.
    pub outsiders: usize,
    /// The share of chatter that outsiders write.
    pub outsider_share: f64,
    /// The share of outsiders' chatter that mentions the agent. Allowed
    /// people's chatter never does, since an addressed message from them is
    /// a request.
    pub addressed_share: f64,
    /// The share of chatter that replies in a recent chat Thread instead of
    /// starting one.
    pub reply_share: f64,
    /// The share of CI and bot messages from a bot not on `policy.bots`.
    pub unlisted_bot_share: f64,
    /// Requests arrive within this first share of the stream, leaving room
    /// for their follow-ups and CI.
    pub request_span: f64,
    pub time_factor: TimeFactor,
}

impl Default for WorkloadSpec {
    fn default() -> Self {
        Self {
            seed: 1,
            messages: 300,
            requests: 20,
            follow_ups: 40,
            ci_messages: 40,
            per_minute: 5.0,
            arrivals: Arrivals::Poisson,
            work_minutes: Span {
                min: 6.0,
                max: 16.0,
            },
            people: 6,
            outsiders: 6,
            outsider_share: 0.5,
            addressed_share: 0.2,
            reply_share: 0.5,
            unlisted_bot_share: 0.25,
            request_span: 0.8,
            time_factor: TimeFactor::MINUTE_A_SECOND,
        }
    }
}

fn share(name: &str, value: f64) -> Result<()> {
    ensure!(
        (0.0..=1.0).contains(&value),
        "workload.{name} is a share, 0 to 1, not {value}"
    );
    Ok(())
}

impl WorkloadSpec {
    /// A YAML spec; the fields it leaves out keep their defaults.
    pub fn load(path: &Path) -> Result<Self> {
        let bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let spec: Self = serde_yaml::from_slice(&bytes)
            .with_context(|| format!("invalid workload spec {}", path.display()))?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=100_000).contains(&self.messages),
            "workload.messages must be 1..=100000"
        );
        ensure!(self.requests > 0, "workload.requests must be above zero");
        ensure!(
            self.requests + self.follow_ups + self.ci_messages <= self.messages,
            "workload.messages must hold the requests, their follow-ups and their CI messages"
        );
        ensure!(
            self.per_minute.is_finite() && self.per_minute > 0.0 && self.per_minute <= 10_000.0,
            "workload.per_minute must be above 0 and at most 10000"
        );
        let Span { min, max } = self.work_minutes;
        ensure!(
            min.is_finite() && max.is_finite() && min > 0.0 && min <= max && max <= 1_440.0,
            "workload.work_minutes must have 0 < min <= max <= 1440"
        );
        ensure!(self.people > 0, "workload.people must be above zero");
        share("outsider_share", self.outsider_share)?;
        share("addressed_share", self.addressed_share)?;
        share("reply_share", self.reply_share)?;
        share("unlisted_bot_share", self.unlisted_bot_share)?;
        ensure!(
            self.outsiders > 0 || self.outsider_share == 0.0,
            "workload.outsider_share needs workload.outsiders"
        );
        ensure!(
            self.request_span > 0.0 && self.request_span <= 1.0,
            "workload.request_span must be above 0 and at most 1"
        );
        ensure!(
            request_slots(self) >= self.requests,
            "workload.request_span leaves fewer messages than workload.requests"
        );
        Ok(())
    }

    /// The people `policy.people` lists, as `source:id`.
    pub fn allowed(&self) -> Vec<String> {
        (1..=self.people)
            .map(|n| format!("{SOURCE}:{}", person(n)))
            .collect()
    }
}

fn request_slots(spec: &WorkloadSpec) -> usize {
    (spec.messages as f64 * spec.request_span).floor() as usize
}

fn person(n: usize) -> String {
    format!("dev-{n}")
}

fn outsider(n: usize) -> String {
    format!("guest-{n}")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Request,
    FollowUp,
    Ci,
    Chatter,
}

impl Kind {
    pub fn of(trigger: &Trigger) -> Option<Self> {
        let kind = trigger.meta.pointer("/bench/kind")?.clone();
        serde_json::from_value(kind).ok()
    }
}

/// A work request and what it expects.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub trigger: TriggerId,
    pub thread: ThreadKey,
    pub at_ms: u64,
    pub work_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Workload {
    pub seed: u64,
    /// In arrival order, `at_ms` in workload time.
    pub triggers: Vec<Trigger>,
    pub requests: Vec<Request>,
}

impl Workload {
    pub fn count(&self, kind: Kind) -> usize {
        self.triggers
            .iter()
            .filter(|t| Kind::of(t) == Some(kind))
            .count()
    }

    /// Workload time from the first Trigger to the last.
    pub fn span_ms(&self) -> u64 {
        match (self.triggers.first(), self.triggers.last()) {
            (Some(first), Some(last)) => last.at_ms - first.at_ms,
            _ => 0,
        }
    }

    /// One Trigger per line, in arrival order.
    pub fn write_jsonl(&self, path: &Path) -> Result<()> {
        let mut file = std::io::BufWriter::new(
            std::fs::File::create(path)
                .with_context(|| format!("cannot create {}", path.display()))?,
        );
        for trigger in &self.triggers {
            serde_json::to_writer(&mut file, trigger)?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        Ok(())
    }
}

/// SplitMix64, written out so a seed names the same workload in every
/// build, whatever the `rand` crate's algorithms become.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `0..n`; `n` is above zero.
    fn below(&mut self, n: usize) -> usize {
        ((self.unit() * n as f64) as usize).min(n.saturating_sub(1))
    }

    fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

const TASKS: [&str; 12] = [
    "fix the flaky retry test in the payments service",
    "add pagination to the /orders endpoint",
    "find out why the nightly export is slow and speed it up",
    "upgrade the logging library and fix what breaks",
    "write a migration that drops the unused audit columns",
    "add a dark mode toggle to the settings page",
    "track down the memory leak in the image worker",
    "make the CLI print a clear error when the config is missing",
    "add rate limiting to the public search API",
    "port the billing report from the old cron box to the scheduler",
    "cover the session refresh path with integration tests",
    "rename the legacy feature flags and remove the dead ones",
];

const FOLLOW_UPS: [&str; 8] = [
    "Also cover the empty-input case, please.",
    "Any update on this?",
    "The logs are in the usual bucket if that helps.",
    "Please keep the public API unchanged.",
    "Thanks, no rush on this one.",
    "One more thing: the staging config differs from prod here.",
    "If it gets big, a draft PR first is fine.",
    "Ping me when it's ready for review.",
];

const CI: [&str; 4] = [
    "CI: build failed on the branch for this, 2 tests red.",
    "CI: build passed.",
    "CI: lint failed, 3 warnings promoted to errors.",
    "CI: coverage dropped by 0.4%.",
];

const DEPLOYS: [&str; 3] = [
    "Deployed the branch for this to staging.",
    "Staging deploy rolled back after a failed health check.",
    "Preview environment is up.",
];

const CHATTER: [&str; 16] = [
    "Lunch at noon?",
    "Standup moved to 10:30 today.",
    "Has anyone seen the new dashboard yet?",
    "The coffee machine on 3 is fixed.",
    "I'm out Friday afternoon.",
    "Reminder: retro notes are due tomorrow.",
    "Nice work on the release yesterday.",
    "Does anyone have the link to the design doc?",
    "The office wifi is flaky again.",
    "Who is on call this week?",
    "Planning poker after lunch.",
    "Welcome to the team!",
    "Can someone review my PR when they have a minute?",
    "The demo went well.",
    "Heads up: the VPN certificate rotates tonight.",
    "Happy birthday!",
];

const MENTIONS: [&str; 4] = [
    "@agent can you order pizza for the team?",
    "@agent what's the weather like?",
    "@agent tell me a joke",
    "@agent are you there?",
];

/// The stream for `seed`. Message times follow the spec's rate; requests
/// take slots within `request_span`, each follow-up and CI message the slot
/// in its request's work window nearest at random, and chatter the rest.
pub fn generate(spec: &WorkloadSpec, seed: u64) -> Result<Workload> {
    spec.validate()?;
    let mut rng = Rng(seed);
    let gap = MINUTE_MS / spec.per_minute;
    let mut at = 0.0;
    let mut times = Vec::with_capacity(spec.messages);
    for n in 0..spec.messages {
        if n > 0 {
            at += match spec.arrivals {
                Arrivals::Even => gap,
                Arrivals::Poisson => -gap * (1.0 - rng.unit()).ln(),
            };
        }
        times.push(at.round() as u64);
    }
    let mut roles: Vec<Option<Role>> = vec![None; spec.messages];

    let mut pool: Vec<usize> = (0..request_slots(spec)).collect();
    let mut slots = Vec::with_capacity(spec.requests);
    for n in 0..spec.requests {
        let pick = n + rng.below(pool.len() - n);
        pool.swap(n, pick);
        slots.push(pool[n]);
    }
    slots.sort_unstable();
    let Span { min, max } = spec.work_minutes;
    let mut requests = Vec::with_capacity(spec.requests);
    let mut authors = Vec::with_capacity(spec.requests);
    for (n, slot) in slots.iter().enumerate() {
        let minutes = min + (max - min) * rng.unit();
        let work_ms = ((minutes * MINUTE_MS / 1_000.0).round() * 1_000.0) as u64;
        let id = format!("req-{:02}", n + 1);
        roles[*slot] = Some(Role::Request { request: n });
        authors.push(1 + rng.below(spec.people));
        requests.push(Request {
            trigger: trigger_id(*slot),
            thread: ThreadKey::new(format!("{SOURCE}:{id}")),
            id,
            at_ms: times[*slot],
            work_ms,
        });
    }

    for n in 0..spec.follow_ups + spec.ci_messages {
        let follow_up = n < spec.follow_ups;
        let request = rng.below(requests.len());
        let (lo, hi) = if follow_up {
            FOLLOW_UP_WINDOW
        } else {
            CI_WINDOW
        };
        let Request { at_ms, work_ms, .. } = requests[request];
        let window = (
            at_ms + (work_ms as f64 * lo) as u64,
            at_ms + (work_ms as f64 * hi) as u64,
        );
        let Some(slot) = place(&roles, &times, slots[request], window, &mut rng) else {
            bail!(
                "the stream has no room after {} for its follow-ups and CI messages; \
                 lower workload.request_span or add messages",
                requests[request].id
            );
        };
        roles[slot] = Some(if follow_up {
            Role::FollowUp { request }
        } else {
            Role::Ci {
                request,
                unlisted: rng.chance(spec.unlisted_bot_share),
            }
        });
    }

    let mut chats = 0;
    let mut recent: Vec<ThreadKey> = Vec::new();
    let mut triggers = Vec::with_capacity(spec.messages);
    for (slot, role) in roles.iter().enumerate() {
        let at_ms = times[slot];
        let trigger = match *role {
            Some(Role::Request { request }) => {
                let plan = &requests[request];
                let text = format!("@agent {}", TASKS[request % TASKS.len()]);
                let meta = json!({"bench": {
                    "kind": Kind::Request, "request": plan.id, "work_ms": plan.work_ms,
                }});
                let actor = Actor::person(person(authors[request]));
                message(
                    slot,
                    at_ms,
                    actor,
                    Addressed::Mention,
                    text,
                    &plan.thread,
                    meta,
                )
            }
            Some(Role::FollowUp { request }) => {
                let plan = &requests[request];
                let meta = json!({"bench": {"kind": Kind::FollowUp, "request": plan.id}});
                let text = rng.pick(&FOLLOW_UPS).to_owned();
                let actor = Actor::person(person(authors[request]));
                message(
                    slot,
                    at_ms,
                    actor,
                    Addressed::Unaddressed,
                    text,
                    &plan.thread,
                    meta,
                )
            }
            Some(Role::Ci { request, unlisted }) => {
                let plan = &requests[request];
                let meta = json!({"bench": {"kind": Kind::Ci, "request": plan.id}});
                let (bot, text) = if unlisted {
                    (UNLISTED_BOT, rng.pick(&DEPLOYS))
                } else {
                    (CI_BOT, rng.pick(&CI))
                };
                let actor = Actor::bot(bot);
                let text = text.to_owned();
                message(
                    slot,
                    at_ms,
                    actor,
                    Addressed::Unaddressed,
                    text,
                    &plan.thread,
                    meta,
                )
            }
            None => {
                let from_outsider = rng.chance(spec.outsider_share);
                let addressed = from_outsider && rng.chance(spec.addressed_share);
                let thread = if !recent.is_empty() && rng.chance(spec.reply_share) {
                    recent[rng.below(recent.len())].clone()
                } else {
                    chats += 1;
                    let thread = ThreadKey::new(format!("{SOURCE}:chat-{chats}"));
                    recent.push(thread.clone());
                    if recent.len() > RECENT_CHATS {
                        recent.remove(0);
                    }
                    thread
                };
                let (actor, text) = if from_outsider {
                    let actor = Actor::person(outsider(1 + rng.below(spec.outsiders)));
                    let bank: &[&str] = if addressed { &MENTIONS } else { &CHATTER };
                    (actor, rng.pick(bank))
                } else {
                    (
                        Actor::person(person(1 + rng.below(spec.people))),
                        rng.pick(&CHATTER),
                    )
                };
                let addressed = if addressed {
                    Addressed::Mention
                } else {
                    Addressed::Unaddressed
                };
                let meta = json!({"bench": {"kind": Kind::Chatter}});
                message(
                    slot,
                    at_ms,
                    actor,
                    addressed,
                    text.to_owned(),
                    &thread,
                    meta,
                )
            }
        };
        triggers.push(trigger);
    }
    Ok(Workload {
        seed,
        triggers,
        requests,
    })
}

#[derive(Clone, Copy, Debug)]
enum Role {
    Request { request: usize },
    FollowUp { request: usize },
    Ci { request: usize, unlisted: bool },
}

fn trigger_id(slot: usize) -> TriggerId {
    TriggerId::new(format!("{SOURCE}:{slot:05}"))
}

/// A free slot after `after` whose time is within `window`, at random; else
/// the first free one after the window opens; else the first free one after
/// `after`.
fn place(
    roles: &[Option<Role>],
    times: &[u64],
    after: usize,
    window: (u64, u64),
    rng: &mut Rng,
) -> Option<usize> {
    let free: Vec<usize> = (after + 1..roles.len())
        .filter(|slot| roles[*slot].is_none())
        .collect();
    let inside: Vec<usize> = free
        .iter()
        .copied()
        .filter(|slot| (window.0..=window.1).contains(&times[*slot]))
        .collect();
    if !inside.is_empty() {
        return Some(inside[rng.below(inside.len())]);
    }
    free.iter()
        .copied()
        .find(|slot| times[*slot] >= window.0)
        .or_else(|| free.first().copied())
}

fn message(
    slot: usize,
    at_ms: u64,
    actor: Actor,
    addressed: Addressed,
    text: String,
    thread: &ThreadKey,
    meta: serde_json::Value,
) -> Trigger {
    Trigger {
        id: trigger_id(slot),
        source: SOURCE.into(),
        at_ms,
        actor,
        addressed,
        text,
        threads: vec![thread.clone()],
        reply_to: vec![ChannelRef {
            source: SOURCE.into(),
            address: BTreeMap::from([("thread".to_owned(), thread.to_string())]),
        }],
        meta,
    }
}

#[cfg(test)]
mod tests {
    use super::{Arrivals, Kind, WorkloadSpec, generate};
    use solos_agent_core::model::{ActorKind, Addressed};
    use std::collections::BTreeMap;

    #[test]
    fn a_seed_names_one_workload() {
        let spec = WorkloadSpec::default();
        let first = generate(&spec, 7).unwrap();
        assert_eq!(first, generate(&spec, 7).unwrap());
        assert_ne!(first.triggers, generate(&spec, 8).unwrap().triggers);
        let first_ids: Vec<_> = first.triggers.iter().map(|t| &t.id).collect();
        let mut sorted = first_ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted, first_ids, "ids are unique and in arrival order");
    }

    #[test]
    fn the_default_stream_has_its_counts_and_its_rate() {
        let spec = WorkloadSpec::default();
        for seed in 1..=5 {
            let workload = generate(&spec, seed).unwrap();
            assert_eq!(workload.triggers.len(), 300);
            assert_eq!(workload.count(Kind::Request), 20);
            assert_eq!(workload.count(Kind::FollowUp), 40);
            assert_eq!(workload.count(Kind::Ci), 40);
            assert_eq!(workload.count(Kind::Chatter), 200);
            assert_eq!(workload.requests.len(), 20);
            let minutes = workload.span_ms() as f64 / 60_000.0;
            let rate = 299.0 / minutes;
            assert!(
                (4.0..=6.0).contains(&rate),
                "seed {seed}: {rate:.2} a minute, not about 5"
            );
            assert!(
                workload
                    .triggers
                    .windows(2)
                    .all(|w| w[0].at_ms <= w[1].at_ms),
                "arrival order"
            );
        }
    }

    #[test]
    fn even_arrivals_keep_their_gap() {
        let spec = WorkloadSpec {
            arrivals: Arrivals::Even,
            per_minute: 12.0,
            ..WorkloadSpec::default()
        };
        let workload = generate(&spec, 1).unwrap();
        assert!(
            workload
                .triggers
                .windows(2)
                .all(|w| w[1].at_ms - w[0].at_ms == 5_000),
            "one message every five seconds"
        );
    }

    #[test]
    fn requests_follow_ups_and_ci_land_where_they_belong() {
        let spec = WorkloadSpec::default();
        let workload = generate(&spec, 3).unwrap();
        let allowed = spec.allowed();
        let by_id: BTreeMap<&str, _> = workload
            .requests
            .iter()
            .map(|r| (r.id.as_str(), r))
            .collect();
        for (slot, trigger) in workload.triggers.iter().enumerate() {
            let who = format!("{}:{}", trigger.source, trigger.actor.id);
            match Kind::of(trigger).unwrap() {
                Kind::Request => {
                    assert_eq!(trigger.addressed, Addressed::Mention);
                    assert!(allowed.contains(&who), "{who} may ask for work");
                    let work = trigger.meta["bench"]["work_ms"].as_u64().unwrap();
                    assert!((360_000..=960_000).contains(&work), "{work}");
                    assert!(slot < 240, "requests come in the first 80% of the stream");
                }
                kind @ (Kind::FollowUp | Kind::Ci) => {
                    let request = by_id[trigger.meta["bench"]["request"].as_str().unwrap()];
                    assert_eq!(trigger.threads, std::slice::from_ref(&request.thread));
                    assert!(trigger.at_ms > request.at_ms, "{kind:?} after its request");
                    assert_eq!(trigger.addressed, Addressed::Unaddressed);
                    let expected = if kind == Kind::Ci {
                        ActorKind::Bot
                    } else {
                        ActorKind::Person
                    };
                    assert_eq!(trigger.actor.kind, expected);
                }
                Kind::Chatter => {
                    assert!(trigger.threads[0].as_str().starts_with("bench:chat-"));
                    if trigger.addressed == Addressed::Mention {
                        assert!(!allowed.contains(&who), "only outsiders mention the agent");
                    }
                }
            }
        }
    }

    #[test]
    fn a_spec_that_cannot_hold_its_requests_is_refused() {
        let cases = [
            WorkloadSpec {
                messages: 30,
                ..WorkloadSpec::default()
            },
            WorkloadSpec {
                requests: 0,
                ..WorkloadSpec::default()
            },
            WorkloadSpec {
                per_minute: 0.0,
                ..WorkloadSpec::default()
            },
            WorkloadSpec {
                outsider_share: 1.5,
                ..WorkloadSpec::default()
            },
            WorkloadSpec {
                request_span: 0.01,
                ..WorkloadSpec::default()
            },
        ];
        for spec in cases {
            assert!(generate(&spec, 1).is_err(), "{spec:?}");
        }
        let yaml = "messages: 40\nrequests: 4\nfollow_ups: 2\nci_messages: 2\nwork_minutes: {min: 1, max: 2}\n";
        let spec: WorkloadSpec = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(generate(&spec, 1).unwrap().triggers.len(), 40);
        assert!(serde_yaml::from_str::<WorkloadSpec>("mesages: 40\n").is_err());
    }
}
