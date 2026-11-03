//! `solos-swarm agent bench`: the ablation of DeepSeek's *Exploring
//! Super-agent Design*, for this agent. Each run serves a fresh agent on a
//! fresh state directory, feeds it a generated stream of chat and CI
//! Triggers at their arrival times, lets simulated work take each request's
//! time, and measures from the Store what every request waited and what the
//! Router did and spent.

pub mod report;
pub mod workload;

use super::simulated::TimeFactor;
use super::{STORE_FILE, ServeOptions, serve_until};
use crate::broker::now_ms;
use anyhow::{Context, Result, bail, ensure};
use report::{BenchReport, DesignReport, Measure, RunReport, Stopped};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use solos_agent_core::Repositories;
use solos_agent_core::model::{MicroUsd, ReportStatus, Trigger, TriggerId};
use solos_agent_store::SqliteStore;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};
use workload::{CI_BOT, Kind, SOURCE, Workload, WorkloadSpec, generate};
use zeroize::Zeroizing;

/// What the Router gives each request's Ticket. No simulated work spends it.
const SESSION_MICROUSD: MicroUsd = 500_000;
/// How often a run looks at its Store.
const CHECK: Duration = Duration::from_millis(100);
/// Added to the default deadline, in workload minutes.
const DEADLINE_SLACK_MINUTES: f64 = 30.0;
/// The most worker slots the non-serial designs get: every request at once.
const PARALLEL_WORKERS: usize = 64;

/// One design of the ablation, as agent.yaml.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Design {
    /// Agent mode as it is: pass-through, parallel workers.
    SuperAgent,
    /// A person's follow-up on a subscribed Thread also goes to the Router.
    NoPassThrough,
    /// One worker slot, and the Router waits while work runs.
    Serial,
}

impl Design {
    pub const ALL: [Self; 3] = [Self::SuperAgent, Self::NoPassThrough, Self::Serial];

    pub fn name(self) -> &'static str {
        match self {
            Self::SuperAgent => "super-agent",
            Self::NoPassThrough => "no-pass-through",
            Self::Serial => "serial",
        }
    }
}

impl std::fmt::Display for Design {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Design {
    type Err = String;
    fn from_str(name: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|design| design.name() == name)
            .ok_or_else(|| {
                format!("no design {name}; the designs are super-agent, no-pass-through, serial")
            })
    }
}

/// `agent bench`'s flags as given, before any is checked.
#[derive(Default)]
pub struct BenchArgs {
    pub workload: Option<PathBuf>,
    pub designs: Vec<Design>,
    pub runs: usize,
    pub out: PathBuf,
    /// `scripted` or `pi`.
    pub router: String,
    pub time_factor: Option<f64>,
    pub deadline_minutes: Option<f64>,
    pub router_config: Option<PathBuf>,
    pub pi_root: Option<PathBuf>,
    pub node: Option<PathBuf>,
    pub credential_file: Option<PathBuf>,
    pub max_spend_usd: Option<f64>,
}

pub enum BenchRouter {
    /// Free: measures scheduling only.
    Scripted,
    /// A model Router, spending money.
    Pi(Box<PaidRouter>),
}

pub struct PaidRouter {
    /// The `router` section of the agent.yaml `--router-config` named.
    pub router: Value,
    pub pi_root: PathBuf,
    pub node: PathBuf,
    pub credential: Zeroizing<String>,
    /// For the whole bench; each run gets an even share.
    pub max_spend_microusd: MicroUsd,
}

pub struct BenchOptions {
    pub workload: WorkloadSpec,
    pub designs: Vec<Design>,
    pub runs: usize,
    pub out: PathBuf,
    /// In workload minutes; by default the stream, all of its work done one
    /// after another, and half an hour.
    pub deadline_minutes: Option<f64>,
    pub router: BenchRouter,
}

impl BenchArgs {
    /// Check the flags. A paid run is refused without `--max-spend-usd`
    /// before anything else is read, so no key enters the process.
    pub fn into_options(self) -> Result<BenchOptions> {
        let paid_flags = self.router_config.is_some()
            || self.pi_root.is_some()
            || self.node.is_some()
            || self.credential_file.is_some();
        let router = match self.router.as_str() {
            "scripted" => {
                ensure!(
                    self.max_spend_usd.is_none() && !paid_flags,
                    "--router scripted spends nothing; --max-spend-usd, --router-config, \
                     --pi-root, --node and --credential-file are for --router pi"
                );
                BenchRouter::Scripted
            }
            "pi" => {
                let Some(max_spend_usd) = self.max_spend_usd else {
                    bail!(
                        "a paid bench (--router pi) calls a real model and spends money: give \
                         --max-spend-usd, the most the whole bench may spend"
                    );
                };
                ensure!(
                    max_spend_usd.is_finite() && max_spend_usd > 0.0 && max_spend_usd <= 1_000.0,
                    "--max-spend-usd must be above 0 and at most 1000"
                );
                let (Some(config), Some(pi_root), Some(node), Some(credential_file)) = (
                    self.router_config,
                    self.pi_root,
                    self.node,
                    self.credential_file,
                ) else {
                    bail!(
                        "--router pi needs --router-config, --pi-root, --node and \
                         --credential-file"
                    );
                };
                let credential = take_credential(&credential_file)?;
                let router = pi_router(&config)?;
                BenchRouter::Pi(Box::new(PaidRouter {
                    router,
                    pi_root,
                    node,
                    credential,
                    max_spend_microusd: (max_spend_usd * 1_000_000.0).floor() as MicroUsd,
                }))
            }
            other => bail!("no router {other}; --router is scripted or pi"),
        };
        let mut workload = match &self.workload {
            Some(path) => WorkloadSpec::load(path)?,
            None => WorkloadSpec::default(),
        };
        if let Some(factor) = self.time_factor {
            workload.time_factor = TimeFactor::new(factor).context("--time-factor")?;
        }
        ensure!(!self.designs.is_empty(), "name at least one --design");
        ensure!((1..=100).contains(&self.runs), "--runs must be 1..=100");
        if let Some(minutes) = self.deadline_minutes {
            ensure!(
                minutes.is_finite() && minutes > 0.0,
                "--deadline-minutes must be above 0"
            );
        }
        let mut designs = self.designs;
        designs.dedup();
        Ok(BenchOptions {
            workload,
            designs,
            runs: self.runs,
            out: self.out,
            deadline_minutes: self.deadline_minutes,
            router,
        })
    }
}

/// Read the model key once and delete its file, whether or not the read
/// worked, as `agent serve --credential-file` does.
fn take_credential(path: &Path) -> Result<Zeroizing<String>> {
    let read = std::fs::read_to_string(path);
    let _ = std::fs::remove_file(path);
    let raw = Zeroizing::new(read.context("cannot read the credential file")?);
    Ok(Zeroizing::new(raw.trim().to_owned()))
}

/// The `router` section of an agent.yaml, which must be a Pi Router.
fn pi_router(path: &Path) -> Result<Value> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let config: serde_yaml::Value = serde_yaml::from_slice(&bytes)
        .with_context(|| format!("invalid agent config {}", path.display()))?;
    let router = serde_json::to_value(config.get("router").cloned())?;
    ensure!(
        router.get("kind").and_then(Value::as_str) == Some("pi"),
        "{} has no `router: {{kind: pi, ...}}`",
        path.display()
    );
    Ok(router)
}

/// What every run of a bench shares.
struct Bench<'a> {
    options: &'a BenchOptions,
    deadline_minutes: f64,
    run_cap: Option<MicroUsd>,
}

/// Run every design `runs` times and write `report.json` and `report.md`
/// into the output directory, which must be new or empty.
pub async fn run(options: BenchOptions) -> Result<BenchReport> {
    options.workload.validate()?;
    let out = &options.out;
    std::fs::create_dir_all(out).with_context(|| format!("cannot create {}", out.display()))?;
    ensure!(
        std::fs::read_dir(out)?.next().is_none(),
        "{} is not empty; give the bench a new --out",
        out.display()
    );
    let spec = &options.workload;
    let first = generate(spec, spec.seed)?;
    let deadline_minutes = match options.deadline_minutes {
        Some(minutes) => minutes,
        None => (0..options.runs as u64)
            .map(|n| generate(spec, spec.seed + n).map(|w| default_deadline_minutes(&w)))
            .collect::<Result<Vec<f64>>>()?
            .into_iter()
            .fold(0.0, f64::max),
    };
    let runs = options.designs.len() * options.runs;
    let run_cap = match &options.router {
        BenchRouter::Scripted => None,
        BenchRouter::Pi(paid) => {
            let cap = paid.max_spend_microusd / runs as MicroUsd;
            ensure!(cap > 0, "--max-spend-usd is too small for {runs} runs");
            Some(cap)
        }
    };
    if let BenchRouter::Pi(paid) = &options.router {
        eprintln!(
            "{}",
            estimate(&options, &first, paid, run_cap.unwrap_or(0))?
        );
    }
    std::fs::write(
        out.join("workload.yaml"),
        serde_yaml::to_string(&options.workload)?,
    )?;
    let bench = Bench {
        options: &options,
        deadline_minutes,
        run_cap,
    };
    let mut designs = Vec::new();
    for design in &options.designs {
        let mut reports = Vec::new();
        for n in 0..options.runs {
            let report = bench.run_once(*design, n).await?;
            eprintln!(
                "bench: {design} run {}/{}: {} of {} requests, p50 {} / p95 {} min, {} Router \
                 decisions, {} model calls, ${:.4}, stopped {:?} after {:.0}s",
                n + 1,
                options.runs,
                report.requests.len() - report.unfinished.len(),
                report.requests.len(),
                report.p50_minutes.map_or("-".into(), |m| format!("{m:.1}")),
                report.p95_minutes.map_or("-".into(), |m| format!("{m:.1}")),
                report.router_decisions,
                report.model_calls,
                report.spend_microusd as f64 / 1_000_000.0,
                report.stopped,
                report.wall_secs,
            );
            let budget = report.stopped == Stopped::Budget;
            reports.push(report);
            if budget {
                eprintln!("bench: {design} reached its spend cap; its remaining runs are skipped");
                break;
            }
        }
        designs.push(DesignReport::of(*design, reports));
    }
    let report = BenchReport {
        router: match options.router {
            BenchRouter::Scripted => "scripted".into(),
            BenchRouter::Pi(_) => "pi".into(),
        },
        workload: options.workload.clone(),
        runs_per_design: options.runs,
        deadline_minutes,
        run_cap_microusd: run_cap,
        designs,
    };
    std::fs::write(out.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
    std::fs::write(out.join("report.md"), report.markdown())?;
    Ok(report)
}

/// The stream, then every request's work one after another, then slack:
/// enough for `serial` to finish.
fn default_deadline_minutes(workload: &Workload) -> f64 {
    let work: u64 = workload.requests.iter().map(|r| r.work_ms).sum();
    (workload.span_ms() + work) as f64 / 60_000.0 + DEADLINE_SLACK_MINUTES
}

/// What a paid bench is likely to cost, printed before it starts. A
/// long-lived Router session reads its whole context on every call, so the
/// input per call grows with the Candidates it has seen.
fn estimate(
    options: &BenchOptions,
    workload: &Workload,
    paid: &PaidRouter,
    run_cap: MicroUsd,
) -> Result<String> {
    let spec = &options.workload;
    let price = |name: &str| {
        paid.router
            .pointer(&format!("/prices/{name}"))
            .and_then(Value::as_u64)
    };
    let input = price("input_microusd_per_million").context("router.prices")?;
    let output = price("output_microusd_per_million").context("router.prices")?;
    let hit = price("cache_hit_microusd_per_million");
    let allowed: BTreeSet<String> = spec.allowed().into_iter().collect();
    let to_router = workload
        .triggers
        .iter()
        .filter(|t| reaches_router(t, &allowed))
        .count();
    let follow_ups = workload.count(Kind::FollowUp);
    let mut text = format!(
        "bench: a paid run calls a real model. Cap: ${:.4} per run (budgets.router_daily_microusd \
         {run_cap}), ${:.2} in all for {} runs; a run stops when the Router's next call cannot \
         fit.",
        run_cap as f64 / 1_000_000.0,
        paid.max_spend_microusd as f64 / 1_000_000.0,
        options.designs.len() * options.runs,
    );
    let mut total = 0.0;
    for design in &options.designs {
        let candidates = to_router
            + if *design == Design::NoPassThrough {
                follow_ups
            } else {
                0
            };
        let calls = 2 * candidates;
        let mean_input = 4_000 + 150 * candidates;
        let hit_rate = if hit.is_some() { 0.9 } else { 0.0 };
        let input_rate = hit.unwrap_or(input) as f64 * hit_rate + input as f64 * (1.0 - hit_rate);
        let per_run = calls as f64 * (mean_input as f64 * input_rate + 200.0 * output as f64)
            / 1_000_000.0
            / 1_000_000.0;
        total += per_run * options.runs as f64;
        let _ = write!(
            text,
            "\nEstimate for {design}: about {candidates} Router candidates a run, so about \
             {calls} model calls at about {mean_input} input tokens ({:.0}% cache hits) and 200 \
             output tokens each: about ${per_run:.4} a run.",
            hit_rate * 100.0
        );
    }
    let _ = write!(text, "\nEstimate for the whole bench: about ${total:.4}.");
    if spec.time_factor.get() < 1.0 {
        text.push_str(
            "\nThe time factor is below 1: the Router's real latency does not shrink with it, so \
             completion minutes overstate its share. --time-factor 1 compares with the article.",
        );
    }
    Ok(text)
}

/// Whether the gate sends `trigger` to the Router with pass-through on: a
/// request, an allowed person's chatter, or the listed CI bot's message.
fn reaches_router(trigger: &Trigger, allowed: &BTreeSet<String>) -> bool {
    let who = format!("{}:{}", trigger.source, trigger.actor.id);
    match Kind::of(trigger) {
        Some(Kind::Request) => true,
        Some(Kind::FollowUp) => false,
        Some(Kind::Ci) => trigger.actor.id == CI_BOT,
        Some(Kind::Chatter) | None => allowed.contains(&who),
    }
}

impl Bench<'_> {
    async fn run_once(&self, design: Design, n: usize) -> Result<RunReport> {
        let options = self.options;
        let spec = &options.workload;
        let relative = PathBuf::from(design.name()).join(format!("run-{}", n + 1));
        let dir = options.out.join(&relative);
        let state_dir = dir.join("state");
        std::fs::create_dir_all(&dir)?;
        let workload = generate(spec, spec.seed + n as u64)?;
        workload.write_jsonl(&dir.join("workload.jsonl"))?;
        let feed_path = dir.join("triggers.jsonl");
        let mut feed = File::create(&feed_path)?;
        let config = dir.join("agent.yaml");
        std::fs::write(&config, self.agent_yaml(design)?)?;

        let serve_options = ServeOptions {
            until_idle: false,
            ..ServeOptions::default()
        };
        let serve_options = match &options.router {
            BenchRouter::Scripted => serve_options,
            BenchRouter::Pi(paid) => ServeOptions {
                pi_root: Some(paid.pi_root.clone()),
                node: Some(paid.node.clone()),
                credential: Some(paid.credential.clone()),
                ..serve_options
            },
        };
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serve = serve_until(&config, &state_dir, serve_options, async {
            let _ = stopped.await;
        });
        tokio::pin!(serve);
        let started = Instant::now();
        let started_ms = now_ms();
        let mut run = Run {
            workload: &workload,
            time_factor: spec.time_factor,
            started,
            fed: 0,
            store: None,
            read_once: false,
        };
        let deadline = spec
            .time_factor
            .wall((self.deadline_minutes * 60_000.0) as u64);
        let drive = run.drive(&mut feed, &state_dir, deadline, self.run_cap);
        let stopped_by = tokio::select! {
            ended = &mut serve => {
                ended.context("agent serve failed")?;
                bail!("agent serve stopped before the run ended");
            }
            outcome = drive => outcome?,
        };
        let _ = stop.send(());
        let summary = serve.await.context("agent serve failed")?;
        let wall_secs = started.elapsed().as_secs_f64();
        let fed = run.fed;
        drop(run);
        let store = SqliteStore::open_read_only(state_dir.join(STORE_FILE))
            .context("cannot read the run's store")?;
        Measure {
            design,
            run: n + 1,
            dir: relative,
            workload: &workload,
            time_factor: spec.time_factor,
            started_ms,
            stopped: stopped_by,
            wall_secs,
            triggers_fed: fed,
            summary,
        }
        .read(&store)
    }

    /// agent.yaml for `design`. It goes through `AgentConfig::load` like
    /// any other.
    fn agent_yaml(&self, design: Design) -> Result<String> {
        let spec = &self.options.workload;
        let mut policy = json!({
            "people": spec.allowed(),
            "bots": [format!("{SOURCE}:{CI_BOT}")],
        });
        if design == Design::Serial {
            // Evals wait while work runs, so they must not expire meanwhile.
            let deadline = spec
                .time_factor
                .wall((self.deadline_minutes * 60_000.0) as u64);
            policy["eval_timeout_ms"] = json!(deadline.as_millis() as u64 + 60_000);
        }
        let router = match &self.options.router {
            BenchRouter::Scripted => json!({"kind": "scripted", "ignore_follow_ups": true}),
            BenchRouter::Pi(paid) => paid.router.clone(),
        };
        let mut budgets = json!({"session_microusd": SESSION_MICROUSD});
        if let Some(cap) = self.run_cap {
            budgets["router_daily_microusd"] = json!(cap);
        }
        let workers = if design == Design::Serial {
            1
        } else {
            PARALLEL_WORKERS
        };
        let config = json!({
            "policy": policy,
            "pass_through": design != Design::NoPassThrough,
            "sources": [{"kind": "jsonl", "path": "triggers.jsonl"}],
            "channels": [{"kind": "jsonl", "source": SOURCE, "path": "outbox.jsonl"}],
            "router": router,
            "executor": {
                "kind": "simulated",
                "max_workers": workers,
                "time_factor": spec.time_factor.get(),
                "router_waits_for_work": design == Design::Serial,
            },
            "budgets": budgets,
        });
        Ok(serde_yaml::to_string(&config)?)
    }
}

/// One run as it goes: what has been fed, and the Store it watches.
struct Run<'a> {
    workload: &'a Workload,
    time_factor: TimeFactor,
    started: Instant,
    fed: usize,
    store: Option<SqliteStore>,
    /// The Store has answered once, so its schema exists.
    read_once: bool,
}

impl Run<'_> {
    /// Feed each Trigger when it is due, with its wall time as `at_ms`,
    /// until every request has its result, everything is fed and the Router
    /// has nothing left; or the deadline passes; or the Router's next call
    /// would not fit under `cap`.
    async fn drive(
        &mut self,
        feed: &mut File,
        state_dir: &Path,
        deadline: Duration,
        cap: Option<MicroUsd>,
    ) -> Result<Stopped> {
        let workload = self.workload;
        let triggers = &workload.triggers;
        let requests: BTreeSet<&TriggerId> = workload.requests.iter().map(|r| &r.trigger).collect();
        loop {
            let elapsed = self.started.elapsed();
            while let Some(trigger) = triggers.get(self.fed) {
                if self.time_factor.wall(trigger.at_ms) > elapsed {
                    break;
                }
                let mut line = serde_json::to_vec(&Trigger {
                    at_ms: now_ms(),
                    ..trigger.clone()
                })?;
                line.push(b'\n');
                feed.write_all(&line)?;
                self.fed += 1;
            }
            if elapsed >= deadline {
                return Ok(Stopped::Deadline);
            }
            if self.store.is_none() {
                self.store = SqliteStore::open_read_only(state_dir.join(STORE_FILE)).ok();
            }
            let fed = self.fed == triggers.len();
            let looked = self
                .store
                .as_ref()
                .map(|store| look(store, cap, fed, &requests));
            match looked {
                Some(Ok(Some(stopped))) => return Ok(stopped),
                Some(Ok(None)) => self.read_once = true,
                // The agent may still be creating the Store's schema.
                Some(Err(_)) if !self.read_once => self.store = None,
                Some(Err(err)) => return Err(err),
                None => {}
            }
            let next = triggers
                .get(self.fed)
                .map_or(CHECK, |t| {
                    self.time_factor.wall(t.at_ms).saturating_sub(elapsed)
                })
                .min(CHECK);
            tokio::time::sleep(next).await;
        }
    }
}

/// Why the run stops now, if it does.
fn look(
    store: &SqliteStore,
    cap: Option<MicroUsd>,
    fed: bool,
    requests: &BTreeSet<&TriggerId>,
) -> Result<Option<Stopped>> {
    if let Some(cap) = cap
        && no_room(store, cap)?
    {
        return Ok(Some(Stopped::Budget));
    }
    Ok((fed && settled(store, requests)?).then_some(Stopped::Done))
}

/// Every request has a `ready` Report and no Thread waits for the Router.
fn settled(store: &SqliteStore, requests: &BTreeSet<&TriggerId>) -> Result<bool> {
    let ready: BTreeSet<TriggerId> = store
        .tickets()?
        .into_iter()
        .filter(|ticket| {
            ticket
                .assignments
                .iter()
                .flat_map(|a| &a.reports)
                .any(|r| r.status == ReportStatus::Ready)
        })
        .map(|ticket| ticket.origin)
        .collect();
    Ok(requests.iter().all(|id| ready.contains(*id)) && store.active_threads().is_empty())
}

/// The Router's account cannot take another call as large as its largest
/// so far. The run's whole spend counts, so a UTC midnight during a run
/// does not reopen the cap as the broker's daily window would.
fn no_room(store: &SqliteStore, cap: MicroUsd) -> Result<bool> {
    let rows = store.spend("router")?;
    let Some(largest) = rows.iter().map(|row| row.hold_microusd).max() else {
        return Ok(false);
    };
    Ok(store.committed("router", 0)? + largest > cap)
}
