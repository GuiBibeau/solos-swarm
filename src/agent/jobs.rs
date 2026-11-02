//! A worker's background jobs: `job_start`, `job_wait`, `job_stop`, and a
//! `run_code` that moves to the background when it runs long. Each job runs
//! in the shared sandbox ([`CodeExecutor`]) in its Ticket's directory and
//! belongs to one Assignment; it dies with that Assignment's worker.
//!
//! A job's end reaches the worker once. In a turn, only the worker's own
//! `job_wait` or `job_stop` takes it; a job that ended in a turn and was not
//! taken there, or that ends between turns, becomes a Note on the Ticket,
//! which wakes the worker with a new turn. One lock covers the turn and the
//! jobs, so the two paths cannot both take one end: whoever takes it removes
//! the job from the table.

use super::ports::{Fact, FactSink};
use crate::code::{CodeExecutor, Ended, MAX_TIMEOUT_SECONDS, OutputBounds, Started};
use serde::Deserialize;
use serde_json::{Value, json};
use solos_agent_core::ids::key;
use solos_agent_core::model::{AssignmentId, TicketId};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// The tools a worker with `run_code` also gets.
pub const JOB_TOOLS: [&str; 3] = ["job_start", "job_wait", "job_stop"];
pub const DEFAULT_WAIT_SECS: u64 = 30;
/// A worker in `job_wait` reads no new input, so a wait stays short.
pub const MAX_WAIT_SECS: u64 = 120;
pub const DEFAULT_BACKGROUND_AFTER_SECS: u64 = 5;
pub const DEFAULT_MAX_JOBS: usize = 2;
pub const DEFAULT_JOB_MAX_SECS: u64 = 1_800;
pub const DEFAULT_JOB_OUTPUT_BYTES: usize = 65_536;
/// How long a stop waits for the sandbox to go.
const STOP_WAIT: Duration = Duration::from_secs(10);
const COMMAND_CHARS: usize = 120;
// A Note's text is quoted into the worker's prompt, which keeps 4,000
// characters of it.
const NOTE_STDOUT_CHARS: usize = 2_400;
const NOTE_STDERR_CHARS: usize = 800;

/// What a Ticket's jobs may take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobLimits {
    /// A `run_code` still running after this moves to the background.
    pub background_after: Duration,
    /// Jobs a Ticket may hold whose end has not reached its worker.
    pub max_jobs: usize,
    /// The longest a `job_start` job may run.
    pub max_wall: Duration,
    /// What a `job_start` job keeps of each stream until it is read.
    pub output_bytes: usize,
}

impl Default for JobLimits {
    fn default() -> Self {
        Self {
            background_after: Duration::from_secs(DEFAULT_BACKGROUND_AFTER_SECS),
            max_jobs: DEFAULT_MAX_JOBS,
            max_wall: Duration::from_secs(DEFAULT_JOB_MAX_SECS),
            output_bytes: DEFAULT_JOB_OUTPUT_BYTES,
        }
    }
}

impl JobLimits {
    /// The longest a job may run must hold a whole `run_code`, so one that
    /// moves to the background keeps its own timeout.
    pub const MAX_WALL_SECS: std::ops::RangeInclusive<u64> = MAX_TIMEOUT_SECONDS..=86_400;
}

/// Why a job call was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    BadArguments(String),
    /// No such job of this Assignment waits: it never existed, belongs to
    /// another, or its end was already reported.
    Unknown(String),
    /// The Ticket holds its most jobs, or its worker has ended.
    Full(String),
}

/// Where one worker's jobs run and where their ends go.
pub struct Host<'a> {
    pub code: &'a Arc<CodeExecutor>,
    pub limits: JobLimits,
    pub facts: &'a FactSink,
    pub ticket: &'a TicketId,
    pub assignment: &'a AssignmentId,
    pub work_dir: &'a Path,
}

/// One Assignment's jobs whose end has not reached the worker, and
/// whether the worker is in a turn.
pub struct Jobs {
    state: Mutex<State>,
    /// Batches of new input that wait for the next turn, counted this turn.
    inputs: watch::Sender<u64>,
}

#[derive(Default)]
struct State {
    in_turn: bool,
    /// The input batches a `job_wait` has told the worker about this turn.
    told: u64,
    /// The worker has ended: no job starts, and no end is reported.
    closed: bool,
    started: u64,
    jobs: BTreeMap<String, Job>,
}

struct Job {
    sandbox: Started,
    since: Instant,
    command: String,
}

impl std::fmt::Debug for Jobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("Jobs")
            .field("in_turn", &state.in_turn)
            .field("closed", &state.closed)
            .field("jobs", &state.jobs.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Default for Jobs {
    fn default() -> Self {
        Self {
            state: Mutex::new(State::default()),
            inputs: watch::Sender::new(0),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    code: String,
    #[serde(default)]
    timeout_s: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    job: String,
    #[serde(default)]
    wait_s: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StopArgs {
    job: String,
}

fn arguments<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, Refused> {
    serde_json::from_value(args)
        .map_err(|err| Refused::BadArguments(format!("bad arguments: {err}")))
}

const LATER: &str = "Keep working: if it ends after your turn, its result comes to you as new \
                     input. job_wait waits for it now; job_stop stops it.";

impl Jobs {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A prompt goes out: ends that come from now on wait for the turn.
    pub fn begin_turn(&self) {
        let mut state = self.lock();
        state.in_turn = true;
        state.told = 0;
        self.inputs.send_replace(0);
    }

    /// The turn ended: the texts of the ends it did not take, for Notes.
    pub fn end_turn(&self) -> Vec<String> {
        let mut state = self.lock();
        state.in_turn = false;
        let ended: Vec<String> = state
            .jobs
            .iter()
            .filter(|(_, job)| job.sandbox.ended().is_some())
            .map(|(id, _)| id.clone())
            .collect();
        ended
            .iter()
            .filter_map(|id| state.jobs.remove(id).map(|job| note(id, &job)))
            .collect()
    }

    /// New input waits for the next turn, so a `job_wait` returns to say so.
    pub fn input_waiting(&self) {
        self.inputs.send_modify(|batches| *batches += 1);
    }

    /// Whether a job still runs or waits for a slot.
    pub fn running(&self) -> bool {
        self.lock()
            .jobs
            .values()
            .any(|job| job.sandbox.ended().is_none())
    }

    /// The worker has ended: stop every job, report none, and wait a
    /// bounded time for their sandboxes to go.
    pub async fn stop_all(&self) {
        let jobs = self.close();
        for job in &jobs {
            job.sandbox.stop();
        }
        let deadline = tokio::time::Instant::now() + STOP_WAIT;
        for job in &jobs {
            let _ = tokio::time::timeout_at(deadline, job.sandbox.end()).await;
        }
    }

    /// [`stop_all`](Self::stop_all) without the wait, for a worker whose
    /// token is revoked.
    pub fn abandon(&self) {
        drop(self.close());
    }

    fn close(&self) -> Vec<Job> {
        let mut state = self.lock();
        state.closed = true;
        std::mem::take(&mut state.jobs).into_values().collect()
    }

    /// `job_start`: `code` in the background, at once.
    pub fn start(self: &Arc<Self>, host: &Host<'_>, args: Value) -> Result<Value, Refused> {
        let StartArgs { code, timeout_s } = arguments(args)?;
        let max = host.limits.max_wall.as_secs();
        let timeout_s = timeout_s.unwrap_or(max);
        if !(1..=max).contains(&timeout_s) {
            return Err(Refused::BadArguments(format!(
                "job_start timeout_s must be 1..={max}"
            )));
        }
        let bounds = OutputBounds {
            stdout_bytes: host.limits.output_bytes,
            stderr_bytes: host.limits.output_bytes,
        };
        let mut state = self.lock();
        room(&state, host.limits)?;
        let sandbox = host
            .code
            .start_job_in(host.work_dir, &code, Duration::from_secs(timeout_s), bounds)
            .map_err(|err| Refused::BadArguments(format!("{err:#}")))?;
        let id = self.keep(&mut state, host, &code, sandbox);
        Ok(json!({"job": id, "status": "started", "timeout_s": timeout_s, "next": LATER}))
    }

    /// `run_code`: as before when it ends within the limits' threshold;
    /// after it, a job if the Ticket has room for one, else in the
    /// foreground until it ends, as before.
    pub async fn run_code(
        self: &Arc<Self>,
        host: &Host<'_>,
        args: &Value,
    ) -> Result<Value, Refused> {
        let sandbox = host
            .code
            .start_in(host.work_dir, args)
            .map_err(|err| Refused::BadArguments(format!("{err:#}")))?;
        if let Some(ended) = sandbox.wait(host.limits.background_after).await {
            return receipt(ended);
        }
        let code = args["code"].as_str().unwrap_or_default();
        let kept = {
            let mut state = self.lock();
            if room(&state, host.limits).is_ok() && sandbox.ended().is_none() {
                Ok(self.keep(&mut state, host, code, sandbox))
            } else {
                Err(sandbox)
            }
        };
        let id = match kept {
            Ok(id) => id,
            Err(sandbox) => return receipt(sandbox.end().await),
        };
        let after = host.limits.background_after.as_secs();
        Ok(json!({
            "job": id,
            "status": "running",
            "note": format!("Still running after {after} s, so it moved to the background. {LATER}"),
        }))
    }

    /// Put `sandbox` in the table, and watch for its end.
    fn keep(
        self: &Arc<Self>,
        state: &mut State,
        host: &Host<'_>,
        code: &str,
        sandbox: Started,
    ) -> String {
        state.started += 1;
        let id = key(
            "jb",
            &[host.assignment.as_str(), &state.started.to_string()],
        );
        let end = sandbox.end();
        state.jobs.insert(
            id.clone(),
            Job {
                sandbox,
                since: Instant::now(),
                command: command_line(code),
            },
        );
        let (jobs, facts, ticket, job) = (
            self.clone(),
            host.facts.clone(),
            host.ticket.clone(),
            id.clone(),
        );
        tokio::spawn(async move {
            end.await;
            if let Some(text) = jobs.ended_between_turns(&job) {
                let fact = Fact::Note { ticket, text };
                if facts.send(fact).await.is_err() {
                    eprintln!("agent: the end of {job} was not told: the runtime has stopped");
                }
            }
        });
        id
    }

    /// `job`'s sandbox has ended. Between turns it is reported now.
    fn ended_between_turns(&self, job: &str) -> Option<String> {
        let mut state = self.lock();
        if state.in_turn || state.closed {
            return None;
        }
        state.jobs.remove(job).map(|kept| note(job, &kept))
    }

    /// `job_wait`: up to `wait_s` for the job's end, which it then takes;
    /// else what it printed since the last look. New input cuts the wait
    /// short, once per batch.
    pub async fn wait(&self, args: Value) -> Result<Value, Refused> {
        let WaitArgs { job, wait_s } = arguments(args)?;
        let wait_s = wait_s.unwrap_or(DEFAULT_WAIT_SECS);
        if wait_s > MAX_WAIT_SECS {
            return Err(Refused::BadArguments(format!(
                "job_wait wait_s must be 0..={MAX_WAIT_SECS}; end your turn instead: its end comes to \
                 you as new input"
            )));
        }
        let (end, told) = {
            let state = self.lock();
            let kept = state.jobs.get(&job).ok_or_else(|| unknown(&job))?;
            (kept.sandbox.end(), state.told)
        };
        let mut inputs = self.inputs.subscribe();
        tokio::select! {
            _ = tokio::time::timeout(Duration::from_secs(wait_s), end) => {}
            _ = inputs.wait_for(|batches| *batches > told) => {}
        }
        let mut state = self.lock();
        let kept = state.jobs.get(&job).ok_or_else(|| unknown(&job))?;
        if let Some(ended) = kept.sandbox.ended() {
            state.jobs.remove(&job);
            return Ok(finished(&job, ended));
        }
        let unread = kept.sandbox.read();
        let mut reply = json!({
            "job": job,
            "status": if kept.sandbox.running() { "running" } else { "queued" },
            "elapsed_ms": kept.since.elapsed().as_millis() as u64,
            "stdout": unread.stdout,
            "stderr": unread.stderr,
            "truncated": unread.truncated,
            "next": "Still running. Call job_wait again, or keep working: if it ends after your turn, \
                     its result comes to you as new input.",
        });
        let batches = *self.inputs.borrow();
        if batches > state.told {
            state.told = batches;
            reply["input_waiting"] = json!(true);
            reply["next"] = json!(
                "New input waits for you. End your turn to read it; the job keeps running, and if it \
                 ends after your turn its result comes to you as new input."
            );
        }
        Ok(reply)
    }

    /// `job_stop`: kill the job and take its end, with what it printed
    /// that was not read yet.
    pub async fn stop(&self, args: Value) -> Result<Value, Refused> {
        let StopArgs { job } = arguments(args)?;
        let end = {
            let state = self.lock();
            let kept = state.jobs.get(&job).ok_or_else(|| unknown(&job))?;
            kept.sandbox.stop();
            kept.sandbox.end()
        };
        let _ = tokio::time::timeout(STOP_WAIT, end).await;
        let mut state = self.lock();
        let kept = state.jobs.get(&job).ok_or_else(|| unknown(&job))?;
        match kept.sandbox.ended() {
            Some(ended) => {
                state.jobs.remove(&job);
                Ok(finished(&job, ended))
            }
            None => Ok(json!({"job": job, "status": "stopping", "next": LATER})),
        }
    }
}

fn room(state: &State, limits: JobLimits) -> Result<(), Refused> {
    if state.closed {
        return Err(Refused::Full("this assignment has ended".into()));
    }
    if state.jobs.len() >= limits.max_jobs {
        return Err(Refused::Full(format!(
            "this ticket already holds {} jobs, its most; job_wait or job_stop one first",
            limits.max_jobs
        )));
    }
    Ok(())
}

fn unknown(job: &str) -> Refused {
    Refused::Unknown(format!(
        "no job {job} of this assignment waits: a job's end is reported once, and ids belong to the \
         assignment that started them"
    ))
}

/// `run_code`'s reply for a request that ended in the foreground.
fn receipt(ended: Ended) -> Result<Value, Refused> {
    match ended {
        Ended::Ran { receipt, .. } => Ok(receipt),
        Ended::Failed(why) => Err(Refused::BadArguments(why)),
    }
}

fn status(ended: &Ended) -> &'static str {
    match ended {
        Ended::Failed(_) => "failed",
        Ended::Ran { stopped: true, .. } => "stopped",
        Ended::Ran { receipt, .. } if receipt["timed_out"] == true => "timed_out",
        Ended::Ran { .. } => "exited",
    }
}

/// A taken end, as a tool reply: `run_code`'s receipt with the job and how
/// it ended.
fn finished(job: &str, ended: Ended) -> Value {
    let how = status(&ended);
    match ended {
        Ended::Ran { mut receipt, .. } => {
            receipt["job"] = json!(job);
            receipt["status"] = json!(how);
            receipt
        }
        Ended::Failed(why) => json!({"job": job, "status": how, "error": why}),
    }
}

/// The first line of `code`, short, so a Note says which command it was.
fn command_line(code: &str) -> String {
    let line = code.trim().lines().next().unwrap_or_default();
    let mut kept: String = line.chars().take(COMMAND_CHARS).collect();
    if kept.len() < code.trim().len() {
        kept.push_str("...");
    }
    kept
}

/// The last `chars` characters of `text`, and how many came before them.
fn tail(text: &str, chars: usize) -> (&str, usize) {
    let total = text.chars().count();
    let skip = total.saturating_sub(chars);
    let at = text
        .char_indices()
        .nth(skip)
        .map_or(text.len(), |(at, _)| at);
    (&text[at..], skip)
}

/// A job's end as a Note's text: how it ended and the end of what it
/// printed that nobody read.
fn note(id: &str, job: &Job) -> String {
    let ended = job
        .sandbox
        .ended()
        .unwrap_or_else(|| Ended::Failed("it ended without a receipt".into()));
    let mut text = format!("Background job {id} (`{}`) ", job.command);
    let receipt = match &ended {
        Ended::Failed(why) => {
            let _ = write!(text, "could not run: {why}");
            return text;
        }
        Ended::Ran { receipt, .. } => receipt,
    };
    let secs = receipt["duration_ms"].as_u64().unwrap_or(0) as f64 / 1_000.0;
    let _ = match status(&ended) {
        "stopped" => write!(text, "was stopped after {secs:.1} s."),
        "timed_out" => write!(
            text,
            "reached its timeout after {secs:.1} s and was killed."
        ),
        _ => write!(
            text,
            "exited with code {} after {secs:.1} s.",
            receipt["exit_code"]
        ),
    };
    let mut cut = false;
    for (name, limit) in [("stdout", NOTE_STDOUT_CHARS), ("stderr", NOTE_STDERR_CHARS)] {
        let printed = receipt[name].as_str().unwrap_or_default();
        if printed.trim().is_empty() {
            continue;
        }
        let (kept, skipped) = tail(printed, limit);
        cut |= skipped > 0;
        let _ = write!(text, "\n{name}:\n{kept}");
    }
    if cut || receipt["truncated"] == true {
        text.push_str(
            "\n(Only the end of its output is here. Write long output to a file in /work to read it \
             whole.)",
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{command_line, tail};

    #[test]
    fn a_note_names_the_first_line_of_the_command_and_keeps_the_end_of_the_output() {
        assert_eq!(command_line("  make test\n"), "make test");
        assert_eq!(command_line("cd src\nmake"), "cd src...");
        assert_eq!(tail("héllo", 3), ("llo", 2));
        assert_eq!(tail("ok", 5), ("ok", 0));
    }
}
