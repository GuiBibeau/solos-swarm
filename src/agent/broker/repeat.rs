//! A worker that makes the same tool calls again and again is looping, and
//! its model keeps being paid for it: one Ticket spent its whole budget
//! on a hundred byte-identical `run_code` calls, another sent one refused call
//! 767 times, and a third went round five `sed -n` slices of one file 197
//! times each (1,107 calls, about $3.30). The broker sees every tool call a
//! seat makes, so it is the place to say so, in three steps that each catch a
//! loop the one before cannot.
//!
//! 1. A circle is refused. It is up to ten calls in turn, repeated three
//!    times: `A A A`, `A B A B A B`, `A B C D E A B C D E A B C D E`. The call
//!    that closes the third round is refused with a reason the model reads,
//!    which says what the call answered the first time and asks the model to
//!    change its approach or to report. A call that merely comes back among
//!    others that differ, as a test run does between edits, is no circle.
//! 2. A refusal alone does not stop a model that has gone degenerate, so the
//!    guard counts the calls it refuses, and past a limit it says the seat's
//!    turn is [`Stopped`]. The broker then answers that seat's tool calls
//!    with the stop and refuses its next model call, which ends a Pi turn
//!    without another call to the provider, and the host reports in the
//!    worker's place.
//! 3. Some loops are no circle: a worker that goes through a dozen reads in an
//!    order that never settles, or a loop whose calls differ by a counter. So
//!    the guard also stops a turn that has made [`NOTHING_NEW`] calls in a row
//!    with nothing new in them (no call it had not made this turn, and no
//!    answer it had not seen). A turn has no ceiling on its length: a long,
//!    productive investigation is the work, and its Ticket's budget or the
//!    person is what bounds it.
//! 4. A failure that comes back is a loop even when the calls differ. One
//!    Ticket sent eight heredoc edits of one file that failed alike, between
//!    reads of it, and made about ninety calls before the steps above caught
//!    it: each edit differed in whitespace or in its body, so it was a call
//!    the worker had not made, and a failure was never counted as anything.
//!    So the guard stops a turn on [`STOP_FAILING`] calls of one shape (the
//!    same tool, the same programs, the same files) that failed with the same
//!    output, with no call of that shape that succeeded and no `repeatable`
//!    call (a wait for something that may change) between.
//!
//! The guard is per seat and per turn, remembers hashes of the calls and of
//! their answers, and leaves alone a tool whose registry entry says a repeat
//! is normal: such a call is never refused, ends any circle it falls into,
//! and is not counted as a call that found nothing new.

mod gist;

use self::gist::{Doing, failed, stuck};
use super::clip;
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

/// A circle goes round this many times before it is refused.
const ROUNDS: usize = 3;
/// The most calls one round of a circle has.
const LONGEST_ROUND: usize = 10;
/// How many of a seat's latest calls the guard remembers.
const RECENT: usize = ROUNDS * LONGEST_ROUND;
/// The turn is stopped when this many calls in a row were refused, with no
/// call that went through between them. The first refusal says what to do,
/// so this is the second and third calls that ignored it.
pub(super) const STOP_IN_A_ROW: usize = 3;
/// The turn is stopped when this many calls were refused in it, however many
/// calls that went through came between them: a loop with a call in it that
/// differs is still a loop.
pub(super) const STOP_IN_A_TURN: usize = 10;
/// The turn is stopped when this many calls of one shape failed the same way,
/// with nothing in between that could have changed the outcome. The first two
/// are a retry that may be a fix; the third is a worker that does not learn
/// from its failures.
pub(super) const STOP_FAILING: usize = 3;
/// The turn is stopped when this many calls in a row brought nothing new: each
/// is a call the worker already made in the turn, and answered what it had
/// already been answered. Normal work has runs of two or three (a model that
/// has just lost its context to compaction re-reads a file or two); this is
/// ten times that, and as many calls as a whole investigation re-read after a
/// compaction would take.
pub(super) const NOTHING_NEW: usize = 30;
// A turn has no ceiling on how many calls it makes: a long, productive
// investigation is the work, and a person who wants it to stop can say so.
// What ends a turn is a circle (the guards above), a failure that repeats,
// the Ticket's budget, or the Watchdog's stalled retry — never the count.
/// How many calls' results the guard keeps, to say what a refused call
/// answered the first time.
const ANSWERS: usize = 2 * RECENT;
/// How much of an earlier result a refusal quotes: this many lines of
/// stdout and of stderr, each this long, and this long in all.
const QUOTED_LINES: usize = 3;
const QUOTED_LINE_CHARS: usize = 120;
const QUOTED_CHARS: usize = 320;

// What the stop of a turn says about the calls before it is read from the
// latest ones, which this many remembered calls must cover.
const _: () = assert!(NOTHING_NEW <= RECENT);

/// The question when there is nothing to name.
const OTHER_WAY: &str = "Is there another way you want me to handle this, or should I stop here?";

/// Why a call is refused as a repeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Repeated {
    /// This exact call, this many times in a row.
    InARow(usize),
    /// A round of this many different calls, gone round `ROUNDS` times.
    InTurn(usize),
}

impl Repeated {
    /// What the model reads: what it did, what the call answered the first
    /// time (`earlier`, when the guard kept it), what not to do, and what to
    /// do instead.
    pub(super) fn why(self, tool: &str, earlier: Option<&str>) -> String {
        let did = match self {
            Self::InARow(times) => format!("you made this exact call {times} times in a row"),
            Self::InTurn(calls) => format!(
                "you made these {calls} calls in turn, {ROUNDS} times over, and this is the \
                 last of them"
            ),
        };
        let have = earlier.map_or_else(String::new, |earlier| {
            format!(" What it answered before: {earlier}. You already have that answer.")
        });
        format!(
            "{tool} refused: {did}, so repeating it will not tell you anything new.{have} Do \
             not send it again. Change your approach: use other arguments or another tool, or \
             stop and report with worker_report (ready if the work is done, blocked or failed \
             with the reason if you cannot go on). If you are waiting for something to change, \
             end your turn: a finished job comes back as new input, or use job_wait. If you \
             keep sending refused calls, your turn is stopped and the person is told you got \
             stuck."
        )
    }
}

/// Why a seat's turn is stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stopped {
    /// It kept sending calls the guard refused: this many in a row.
    InARow(usize),
    /// This many refused calls in the turn.
    InTurn(usize),
    /// This many calls in a row brought nothing new.
    NothingNew(usize),
    /// This many calls of one shape failed the same way.
    Failing(usize),
}

impl Stopped {
    /// What the seat did, to follow "the worker " or "you ".
    pub(super) fn note(self) -> String {
        match self {
            Self::InARow(count) => format!("sent {count} refused calls in a row"),
            Self::InTurn(count) => format!("sent {count} refused calls in this turn"),
            Self::NothingNew(count) => {
                format!("made {count} calls in a row that found nothing new")
            }
            Self::Failing(count) => {
                format!("made {count} calls of one kind that failed the same way each time")
            }
        }
    }
}

/// One seat's turn so far: its latest calls, and what the guard counts to
/// stop it.
#[derive(Debug, Default)]
pub(super) struct RepeatGuard {
    /// A hash of the tool's name and arguments of each of the latest calls,
    /// or `None` for a call of a tool that may repeat, which is like no other.
    recent: VecDeque<Option<u64>>,
    /// What the latest calls of tools that may not repeat were doing, to say
    /// so when the turn is stopped.
    doings: VecDeque<Doing>,
    /// What the latest calls answered, by call, oldest first.
    answers: VecDeque<(u64, String)>,
    refused_in_a_row: usize,
    refused_in_turn: usize,
    /// The hashes of every call and of every answer of the turn, so a call
    /// that brings nothing new is known as one. They grow with the turn.
    calls_seen: HashSet<u64>,
    answers_seen: HashSet<u64>,
    /// The calls since the turn last saw a new call or a new answer.
    since_new: usize,
    stopped: Option<Stopped>,
    /// How many of the latest calls say what the worker was doing.
    look_back: usize,
    /// How each shape of call has failed lately.
    failures: HashMap<u64, Failure>,
    /// What kept failing, once the turn is stopped on it.
    failing: Option<Doing>,
}

/// A shape of call that failed, how, and how many times in a row.
#[derive(Debug)]
struct Failure {
    how: u64,
    times: usize,
}

/// A hash of a call: the tool's name and its arguments.
fn key(tool: &str, args: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    tool.hash(&mut hasher);
    args.to_string().hash(&mut hasher);
    hasher.finish()
}

/// What a result says, as a hash: a command's exit code and output, else the
/// result without what moves between identical calls (a response's headers).
fn digest(answer: Result<&Value, &str>) -> u64 {
    const MOVING: [&str; 2] = ["headers", "duration_ms"];
    let mut hasher = DefaultHasher::new();
    match answer {
        Err(why) => (0_u8, why).hash(&mut hasher),
        Ok(result) => match result["exit_code"].as_i64() {
            Some(code) => (
                1_u8,
                code,
                result["stdout"].as_str(),
                result["stderr"].as_str(),
                result["timed_out"] == true,
            )
                .hash(&mut hasher),
            None => match result.as_object() {
                Some(object) => {
                    2_u8.hash(&mut hasher);
                    for (name, value) in object {
                        if !MOVING.contains(&name.as_str()) {
                            (name, value.to_string()).hash(&mut hasher);
                        }
                    }
                }
                None => (3_u8, result.to_string()).hash(&mut hasher),
            },
        },
    }
    hasher.finish()
}

/// How a call failed, as a hash, or `None` for a call that did not: a refusal,
/// or a command that timed out or exited with a code other than 0. A command
/// that exits 1 and prints nothing said "no" (`grep` found nothing, `test`
/// was false), which is an answer, not a failure. Digits are left out, so a
/// line number, a duration or a count that moves does not make a failure new.
fn failure(answer: Result<&Value, &str>) -> Option<u64> {
    let mut hasher = DefaultHasher::new();
    let text = match answer {
        Err(why) => why.to_owned(),
        Ok(result) => {
            let code = result["exit_code"].as_i64()?;
            let timed_out = result["timed_out"] == true;
            let (stdout, stderr) = (
                result["stdout"].as_str().unwrap_or_default(),
                result["stderr"].as_str().unwrap_or_default(),
            );
            if !timed_out
                && (code == 0 || code == 1 && stdout.trim().is_empty() && stderr.trim().is_empty())
            {
                return None;
            }
            (code, timed_out).hash(&mut hasher);
            format!("{stdout}\n{stderr}")
        }
    };
    text.split(|c: char| c.is_ascii_digit() || c.is_whitespace())
        .filter(|word| !word.is_empty())
        .for_each(|word| word.hash(&mut hasher));
    Some(hasher.finish())
}

impl RepeatGuard {
    /// A new turn starts from nothing: new input is a reason to look again,
    /// and a stopped turn is over.
    pub(super) fn begin_turn(&mut self) {
        *self = Self::default();
    }

    /// Note a call of `tool` with `args`, and say whether it closes a
    /// circle. A refused call is noted as well, so one that is sent again
    /// stays refused. A `repeatable` tool is noted but never refused, and
    /// it ends any circle it falls into: its call again is normal because
    /// what it answers moves.
    ///
    /// A refusal is counted, and a call that is not refused ends the run of
    /// refusals in a row. A call of a tool that may not repeat is counted
    /// too: as one that brought something new or not. See
    /// [`stopped`](Self::stopped).
    pub(super) fn call(&mut self, tool: &str, args: &Value, repeatable: bool) -> Option<Repeated> {
        let call = (!repeatable).then(|| key(tool, args));
        if repeatable {
            // A wait is what a retry waits for: whatever failed before may
            // not fail now.
            self.failures.clear();
        }
        let mut doing = None;
        if let Some(call) = call {
            let this = Doing::of(call, tool, args);
            doing = Some(this.clone());
            self.doings.push_back(this);
            if self.doings.len() > RECENT {
                self.doings.pop_front();
            }
            if self.calls_seen.insert(call) {
                self.since_new = 0;
            } else {
                self.since_new += 1;
            }
        }
        let repeated = self.circle(call);
        if repeated.is_none() {
            self.refused_in_a_row = 0;
        } else {
            self.refused_in_a_row += 1;
            self.refused_in_turn += 1;
        }
        if self.stopped.is_none()
            && repeated.is_some()
            && let Some(doing) = doing
            && let Some(failed) = self.failures.get(&doing.shape())
            && failed.times + 1 >= STOP_FAILING
        {
            // The call that closes a circle is not run, so it is the third
            // attempt at what failed twice the same way.
            self.stopped = Some(Stopped::Failing(failed.times + 1));
            self.failing = Some(doing);
        }
        if self.stopped.is_none()
            && let Some((stopped, look_back)) = self.stop_for(repeated)
        {
            (self.stopped, self.look_back) = (Some(stopped), look_back);
        }
        repeated
    }

    /// Whether the turn stops with the call just noted, and how many of the
    /// latest calls say what the worker was doing.
    fn stop_for(&self, repeated: Option<Repeated>) -> Option<(Stopped, usize)> {
        if let Some(repeated) = repeated {
            let looped = match repeated {
                Repeated::InARow(streak) => streak,
                Repeated::InTurn(calls) => calls * ROUNDS,
            };
            if self.refused_in_a_row >= STOP_IN_A_ROW {
                return Some((Stopped::InARow(self.refused_in_a_row), looped));
            }
            if self.refused_in_turn >= STOP_IN_A_TURN {
                return Some((Stopped::InTurn(self.refused_in_turn), looped));
            }
        }
        if self.since_new >= NOTHING_NEW {
            return Some((Stopped::NothingNew(self.since_new), self.since_new));
        }
        None
    }

    /// Why the turn is stopped, once the refusals reach a limit or the calls
    /// stop bringing anything new. It stays stopped until the next turn
    /// begins.
    pub(super) fn stopped(&self) -> Option<Stopped> {
        self.stopped
    }

    /// What the host tells the person about a stopped turn, in the worker's
    /// place and in words they can follow: what it kept doing, that it made no
    /// progress, and the one question that would get it going again. It names
    /// the programs and files of the calls and quotes none of them.
    pub(super) fn sentence(&self) -> Option<String> {
        Some(match self.stopped? {
            Stopped::Failing(times) => match &self.failing {
                Some(doing) => failed(doing, times),
                None => format!(
                    "I tried the same step {times} times and it failed the same way each \
                     time, so I stopped. {OTHER_WAY}"
                ),
            },
            Stopped::InARow(_) | Stopped::InTurn(_) | Stopped::NothingNew(_) => {
                let mut latest: Vec<&Doing> =
                    self.doings.iter().rev().take(self.look_back).collect();
                latest.reverse();
                if latest.is_empty() {
                    format!(
                        "I kept repeating the same steps without making progress, so I \
                         stopped. {OTHER_WAY}"
                    )
                } else {
                    stuck(&latest)
                }
            }
        })
    }

    /// Keep what a call of `tool` with `args` answered, so a later refusal
    /// of the same call can say so. A refusal of the call is an answer too:
    /// the same call is refused the same way. An answer the turn has not had
    /// before is something new, whatever call it came from.
    pub(super) fn answered(&mut self, tool: &str, args: &Value, answer: Result<&Value, &str>) {
        let key = key(tool, args);
        if self.answers_seen.insert(digest(answer)) {
            self.since_new = 0;
        }
        let doing = Doing::of(key, tool, args);
        match failure(answer) {
            None => {
                self.failures.remove(&doing.shape());
            }
            Some(how) => {
                let seen = self
                    .failures
                    .entry(doing.shape())
                    .or_insert(Failure { how, times: 0 });
                if seen.how != how {
                    *seen = Failure { how, times: 0 };
                }
                seen.times += 1;
                if seen.times >= STOP_FAILING && self.stopped.is_none() {
                    self.stopped = Some(Stopped::Failing(seen.times));
                    self.failing = Some(doing);
                }
            }
        }
        let quoted = match answer {
            Ok(result) => summary(result),
            Err(why) => format!("it was refused: {}", clip(why, QUOTED_LINE_CHARS).0),
        };
        self.answers.retain(|(kept, _)| *kept != key);
        self.answers.push_back((key, quoted));
        if self.answers.len() > ANSWERS {
            self.answers.pop_front();
        }
    }

    /// What the latest call of `tool` with `args` answered, in a line.
    pub(super) fn earlier(&self, tool: &str, args: &Value) -> Option<&str> {
        let key = key(tool, args);
        let (_, said) = self.answers.iter().find(|(kept, _)| *kept == key)?;
        Some(said)
    }

    /// Note `call` among the latest, and say whether it closes a circle.
    fn circle(&mut self, call: Option<u64>) -> Option<Repeated> {
        self.recent.push_back(call);
        if self.recent.len() > RECENT {
            self.recent.pop_front();
        }
        call?;
        (1..=LONGEST_ROUND)
            .find(|&round| self.circles(round))
            .map(|round| match round {
                1 => Repeated::InARow(self.streak()),
                calls => Repeated::InTurn(calls),
            })
    }

    /// Whether the latest calls are `ROUNDS` rounds of `round` calls, each
    /// like the one before.
    fn circles(&self, round: usize) -> bool {
        let (calls, last) = (round * ROUNDS, self.recent.len().wrapping_sub(1));
        self.recent.len() >= calls
            && (0..calls - round).all(|back| {
                let (now, then) = (self.recent[last - back], self.recent[last - back - round]);
                now.is_some() && now == then
            })
    }

    /// How many of the latest calls are the same call.
    fn streak(&self) -> usize {
        let latest = self.recent.back().copied().flatten();
        self.recent
            .iter()
            .rev()
            .take_while(|&&call| call.is_some() && call == latest)
            .count()
    }
}

/// A result in one line: a command's exit code and the first lines of its
/// output, else the start of the result as JSON.
fn summary(result: &Value) -> String {
    let quoted = match result["exit_code"].as_i64() {
        Some(code) => {
            let mut said = format!("exit code {code}");
            if result["timed_out"] == true {
                said.push_str(", timed out");
            }
            for stream in ["stdout", "stderr"] {
                let lines = first_lines(result[stream].as_str().unwrap_or_default());
                let _ = write!(said, "; {stream}: {lines}");
            }
            said
        }
        None => result.to_string(),
    };
    clip(&quoted, QUOTED_CHARS).0
}

/// The first non-blank lines of `text` on one line, or `empty`.
fn first_lines(text: &str) -> String {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let shown: Vec<String> = lines
        .by_ref()
        .take(QUOTED_LINES)
        .map(|line| clip(line, QUOTED_LINE_CHARS).0)
        .collect();
    if shown.is_empty() {
        return "empty".to_owned();
    }
    let mut said = shown.join(" | ");
    let more = lines.count();
    if more > 0 {
        let _ = write!(said, " | ... {more} more lines");
    }
    said
}

#[cfg(test)]
mod tests {
    use super::{
        ANSWERS, NOTHING_NEW, RECENT, RepeatGuard, Repeated, STOP_FAILING, STOP_IN_A_ROW,
        STOP_IN_A_TURN, Stopped,
    };
    use serde_json::json;

    fn run(guard: &mut RepeatGuard, code: &str) -> Option<Repeated> {
        guard.call("run_code", &json!({"code": code}), false)
    }

    /// `run`, and the command's answer: `output` is what it printed.
    fn run_and_answer(guard: &mut RepeatGuard, code: &str, output: &str) -> Option<Repeated> {
        let refused = run(guard, code);
        let receipt = json!({"exit_code": 0, "stdout": output, "stderr": "", "duration_ms": 7});
        guard.answered("run_code", &json!({"code": code}), Ok(&receipt));
        refused
    }

    #[test]
    fn the_third_identical_call_in_a_row_is_refused_and_stays_refused() {
        let mut guard = RepeatGuard::default();
        assert_eq!(run(&mut guard, "ls"), None);
        assert_eq!(run(&mut guard, "ls"), None, "one retry is allowed");
        assert_eq!(run(&mut guard, "ls"), Some(Repeated::InARow(3)));
        assert_eq!(run(&mut guard, "ls"), Some(Repeated::InARow(4)));
        assert_eq!(run(&mut guard, "pwd"), None, "another call ends the run");
        assert_eq!(run(&mut guard, "ls"), None);
    }

    #[test]
    fn a_call_is_the_same_only_with_the_same_tool_and_the_same_arguments() {
        let mut guard = RepeatGuard::default();
        for _ in 0..2 {
            assert_eq!(guard.call("run_code", &json!({"code": "ls"}), false), None);
        }
        assert_eq!(guard.call("job_start", &json!({"code": "ls"}), false), None);
        assert_eq!(
            guard.call(
                "run_code",
                &json!({"code": "ls", "timeout_seconds": 5}),
                false
            ),
            None
        );
        assert_eq!(guard.call("run_code", &json!({"code": "ls"}), false), None);
    }

    #[test]
    fn calls_that_go_round_in_turn_are_refused_where_a_call_among_others_is_not() {
        // Test, edit, test, edit: the edits differ, so it is progress.
        let mut working = RepeatGuard::default();
        for round in 0..10 {
            assert_eq!(run(&mut working, "cargo test"), None, "round {round}");
            assert_eq!(run(&mut working, &format!("edit {round}")), None);
        }

        let mut looping = RepeatGuard::default();
        for round in 1..=2 {
            assert_eq!(run(&mut looping, "cargo test"), None, "round {round}");
            assert_eq!(run(&mut looping, "git diff"), None, "round {round}");
        }
        assert_eq!(run(&mut looping, "cargo test"), None);
        assert_eq!(
            run(&mut looping, "git diff"),
            Some(Repeated::InTurn(2)),
            "the third round of the same two calls"
        );
        for call in ["cargo test", "git diff"] {
            assert_eq!(
                run(&mut looping, call),
                Some(Repeated::InTurn(2)),
                "{call} stays refused while the circle goes on"
            );
        }
        assert_eq!(
            run(&mut looping, "git log"),
            None,
            "another call is not refused, and ends the circle"
        );
    }

    #[test]
    fn a_round_of_any_length_up_to_ten_is_refused_at_its_third_round() {
        for round in 1..=10 {
            let mut guard = RepeatGuard::default();
            let mut closed_at = None;
            for call in 1..=3 * round {
                let refused = run(&mut guard, &format!("call {}", (call - 1) % round));
                if refused.is_some() && closed_at.is_none() {
                    closed_at = Some((call, refused));
                }
            }
            let calls = Some(if round == 1 {
                Repeated::InARow(3)
            } else {
                Repeated::InTurn(round)
            });
            assert_eq!(
                closed_at,
                Some((3 * round, calls)),
                "a round of {round} calls closes its circle with its last call"
            );
        }

        // Eleven different calls in a round are more than the guard remembers,
        // and too many to be told from work.
        let mut eleven = RepeatGuard::default();
        for _ in 0..3 {
            for call in 0..11 {
                assert_eq!(run(&mut eleven, &format!("call {call}")), None);
            }
        }
    }

    #[test]
    fn a_repeatable_tool_is_never_refused_and_ends_any_circle_it_falls_into() {
        let mut guard = RepeatGuard::default();
        let wait = json!({"job": "job_1"});
        for _ in 0..20 {
            assert_eq!(guard.call("job_wait", &wait, true), None);
        }
        assert_eq!(run(&mut guard, "ls"), None);
        assert_eq!(run(&mut guard, "ls"), None);
        assert_eq!(guard.call("job_wait", &wait, true), None);
        assert_eq!(
            run(&mut guard, "ls"),
            None,
            "a job's wait between two calls may have changed what the third finds"
        );
        for round in 0..4 {
            assert_eq!(run(&mut guard, "tail log"), None, "round {round}");
            assert_eq!(guard.call("job_wait", &wait, true), None);
        }
    }

    #[test]
    fn a_new_turn_forgets_the_calls_of_the_last() {
        let mut guard = RepeatGuard::default();
        assert_eq!(run(&mut guard, "ls"), None);
        assert_eq!(run(&mut guard, "ls"), None);
        guard.begin_turn();
        assert_eq!(run(&mut guard, "ls"), None);
        assert_eq!(run(&mut guard, "ls"), None);
        assert_eq!(run(&mut guard, "ls"), Some(Repeated::InARow(3)));
        assert!(guard.recent.len() <= RECENT);
    }

    #[test]
    fn the_refusal_names_the_tool_what_happened_and_what_to_do_instead() {
        let why = Repeated::InARow(3).why("run_code", None);
        for says in [
            "run_code refused",
            "this exact call 3 times in a row",
            "Do not send it again",
            "Change your approach",
            "worker_report",
            "job_wait",
            "your turn is stopped",
        ] {
            assert!(why.contains(says), "{says:?} is not in {why}");
        }
        assert!(!why.contains("answered before"), "nothing was kept: {why}");
        let why = Repeated::InTurn(2).why("read", None);
        assert!(why.contains("these 2 calls in turn, 3 times over"), "{why}");
    }

    /// Refuse `ls` as often as it takes to see what the guard says: the
    /// first two calls go through.
    fn loop_on(guard: &mut RepeatGuard, code: &str, times: usize) -> Vec<Option<Repeated>> {
        (0..times).map(|_| run(guard, code)).collect()
    }

    #[test]
    fn the_turn_is_stopped_at_the_third_refusal_in_a_row_and_stays_stopped() {
        let mut guard = RepeatGuard::default();
        let refused = loop_on(&mut guard, "ls", 4);
        assert_eq!(refused.iter().filter(|r| r.is_some()).count(), 2);
        assert_eq!(guard.stopped(), None, "two refusals ask for a change");
        assert_eq!(run(&mut guard, "ls"), Some(Repeated::InARow(5)));
        assert_eq!(
            guard.stopped(),
            Some(Stopped::InARow(STOP_IN_A_ROW)),
            "the third refusal in a row: the fifth identical call"
        );
        assert_eq!(
            guard.sentence().as_deref(),
            Some(
                "I kept re-running `ls` without making progress, so I stopped. Is there \
                 another way you want me to handle this, or should I stop here?"
            )
        );
        run(&mut guard, "ls");
        run(&mut guard, "pwd");
        assert_eq!(
            guard.stopped(),
            Some(Stopped::InARow(STOP_IN_A_ROW)),
            "a stopped turn stays stopped, whatever comes next"
        );
        guard.begin_turn();
        assert_eq!(guard.stopped(), None, "a new turn is a new chance");
        assert_eq!(guard.sentence(), None);
    }

    #[test]
    fn two_calls_that_go_round_are_stopped_after_three_refusals_too() {
        let mut guard = RepeatGuard::default();
        let mut sent = 0;
        while guard.stopped().is_none() {
            for call in ["cargo test", "git diff"] {
                sent += 1;
                run(&mut guard, call);
                if guard.stopped().is_some() {
                    break;
                }
            }
            assert!(sent < 20, "the loop was never stopped");
        }
        assert_eq!(
            sent, 8,
            "six calls close the circle, the next two are refused too"
        );
        assert_eq!(guard.stopped(), Some(Stopped::InARow(STOP_IN_A_ROW)));
        assert_eq!(
            guard.sentence().as_deref(),
            Some(
                "I kept re-running `cargo test` and `git diff` without making progress, so I \
                 stopped. Is there another way you want me to handle this, or should I stop \
                 here?"
            )
        );
    }

    #[test]
    fn a_call_that_goes_through_ends_the_run_of_refusals_in_a_row() {
        let mut guard = RepeatGuard::default();
        for round in 0..4 {
            // Two refusals, then another call: it is asked to change and does.
            loop_on(&mut guard, "ls", 4);
            assert_eq!(guard.refused_in_a_row, 2, "round {round}");
            assert_eq!(guard.stopped(), None, "round {round}");
            assert_eq!(run(&mut guard, &format!("other {round}")), None);
            assert_eq!(guard.refused_in_a_row, 0);
        }
        let mut guard = RepeatGuard::default();
        loop_on(&mut guard, "ls", 4);
        assert_eq!(guard.refused_in_a_row, 2);
        let wait = json!({"job": "job_1"});
        assert_eq!(guard.call("job_wait", &wait, true), None);
        loop_on(&mut guard, "ls", 3);
        assert_eq!(
            (guard.refused_in_a_row, guard.stopped()),
            (1, None),
            "a repeatable tool's call is a call that went through, so this is one refusal, not three"
        );
    }

    #[test]
    fn refusals_that_are_spread_out_are_stopped_at_the_turns_ceiling() {
        let mut guard = RepeatGuard::default();
        let mut refused = 0;
        // `ls`, `ls`, `ls` is refused once, and `other` between loops lets the
        // next `ls` go through twice before the refusal again.
        for round in 0.. {
            if guard.stopped().is_some() {
                break;
            }
            assert!(round < 40, "the loop was never stopped");
            for _ in 0..3 {
                refused += usize::from(run(&mut guard, "ls").is_some());
            }
            run(&mut guard, &format!("other {round}"));
        }
        assert_eq!(refused, STOP_IN_A_TURN);
        assert_eq!(guard.stopped(), Some(Stopped::InTurn(STOP_IN_A_TURN)));
    }

    #[test]
    fn a_refusal_says_what_the_call_answered_the_first_time() {
        let mut guard = RepeatGuard::default();
        let args = json!({"code": "grep -rn bps src | head"});
        assert_eq!(guard.earlier("run_code", &args), None);
        let stdout = (1..=7)
            .map(|line| format!("src/{line}.rs:1:bps"))
            .collect::<Vec<_>>()
            .join("\n");
        let receipt = json!({"exit_code": 0, "stdout": stdout, "stderr": "", "timed_out": false});
        guard.answered("run_code", &args, Ok(&receipt));
        assert_eq!(
            guard.earlier("run_code", &args),
            Some(
                "exit code 0; stdout: src/1.rs:1:bps | src/2.rs:1:bps | src/3.rs:1:bps | ... 4 \
                 more lines; stderr: empty"
            )
        );
        assert_eq!(
            guard.earlier("run_code", &json!({"code": "ls"})),
            None,
            "only the same call has that answer"
        );
        assert_eq!(guard.earlier("job_start", &args), None);

        let failed = json!({"exit_code": 1, "stdout": "", "stderr": "cat: /host/repo.json: \
                            No such file or directory\n", "timed_out": true});
        guard.answered("run_code", &args, Ok(&failed));
        assert_eq!(
            guard.earlier("run_code", &args),
            Some(
                "exit code 1, timed out; stdout: empty; stderr: cat: /host/repo.json: No such \
                 file or directory"
            ),
            "the latest answer replaces the one before"
        );

        let read = json!({"lines": "x".repeat(1_000)});
        guard.answered("read", &args, Ok(&read));
        let quoted = guard.earlier("read", &args).unwrap();
        assert!(quoted.starts_with(r#"{"lines":"xxx"#), "{quoted}");
        assert!(
            quoted.chars().count() <= 330 && quoted.ends_with("..."),
            "{quoted}"
        );

        guard.answered("fetch_url", &args, Err("blocked address"));
        assert_eq!(
            guard.earlier("fetch_url", &args),
            Some("it was refused: blocked address")
        );

        let why = Repeated::InARow(3).why("run_code", guard.earlier("run_code", &args));
        assert!(
            why.contains("What it answered before: exit code 1, timed out;"),
            "{why}"
        );
        assert!(why.contains("You already have that answer."), "{why}");

        guard.begin_turn();
        assert_eq!(guard.earlier("run_code", &args), None, "a new turn forgets");
    }

    #[test]
    fn the_guard_keeps_the_answers_of_the_latest_calls_only() {
        let mut guard = RepeatGuard::default();
        let receipt = json!({"exit_code": 0, "stdout": "ok", "stderr": ""});
        for call in 0..100 {
            guard.answered("run_code", &json!({"code": call}), Ok(&receipt));
        }
        assert!(guard.answers.len() <= ANSWERS);
        assert_eq!(guard.earlier("run_code", &json!({"code": 0})), None);
        assert!(guard.earlier("run_code", &json!({"code": 99})).is_some());
    }

    const FILE: &str = "packages/solana/src/liquidity/meteora-dlmm-allocate-encode.js";

    /// The calls of the worker that went round five slices of one file 197
    /// times each: the slice of its `k`-th call.
    fn slice(k: usize) -> String {
        let ranges = [(60, 95), (95, 125), (125, 150), (150, 180), (180, 200)];
        let (from, to) = ranges[k % 5];
        format!("cd /work && sed -n 1,60p {FILE} | tail -1; sed -n {from},{to}p {FILE}")
    }

    #[test]
    fn a_cycle_of_five_calls_is_stopped_at_its_seventeenth_call() {
        // The worker made 125 different calls before it began to go round.
        let mut guard = RepeatGuard::default();
        for call in 0..125 {
            let refused = run_and_answer(&mut guard, &format!("rg -n 'bps' src/{call}.rs"), "x");
            assert_eq!(refused, None);
        }
        let mut sent = 0;
        let mut first_refused = None;
        while guard.stopped().is_none() {
            let refused = run_and_answer(&mut guard, &slice(sent), "the same lines");
            sent += 1;
            assert!(sent < 100, "the loop was never stopped");
            if refused.is_some() && first_refused.is_none() {
                first_refused = Some((sent, refused));
            }
        }
        assert_eq!(
            first_refused,
            Some((15, Some(Repeated::InTurn(5)))),
            "the third round is refused at its last call"
        );
        assert_eq!(
            sent, 17,
            "and the next two calls that ignore it stop the turn"
        );
        assert_eq!(guard.stopped(), Some(Stopped::InARow(STOP_IN_A_ROW)));
        assert_eq!(
            guard.sentence().as_deref(),
            Some(
                "I kept re-reading the same parts of meteora-dlmm-allocate-encode.js without \
                 making progress, so I stopped. What should I be looking for in \
                 meteora-dlmm-allocate-encode.js?"
            )
        );
    }

    #[test]
    fn a_loop_that_is_too_long_to_be_a_circle_is_stopped_when_nothing_new_comes_of_it() {
        // 17 calls in turn: more than a circle's longest round, so the circle
        // check never sees it. The answers repeat as the calls do.
        let mut guard = RepeatGuard::default();
        let mut sent = 0;
        while guard.stopped().is_none() {
            let call = format!("sed -n {0},{0}p {FILE}", sent % 17);
            let refused = run_and_answer(&mut guard, &call, &format!("line {}", sent % 17));
            sent += 1;
            assert_eq!(refused, None, "call {sent} is no circle");
            assert!(sent < 100, "the loop was never stopped");
        }
        assert_eq!(
            sent,
            17 + NOTHING_NEW,
            "17 calls that are new, then {NOTHING_NEW} that bring nothing"
        );
        assert_eq!(guard.stopped(), Some(Stopped::NothingNew(NOTHING_NEW)));
        assert_eq!(
            guard.sentence().as_deref(),
            Some(
                "I kept re-reading the same parts of meteora-dlmm-allocate-encode.js without \
                 making progress, so I stopped. What should I be looking for in \
                 meteora-dlmm-allocate-encode.js?"
            )
        );
        assert_eq!(
            Stopped::NothingNew(30).note(),
            "made 30 calls in a row that found nothing new"
        );

        guard.begin_turn();
        assert_eq!(guard.stopped(), None);
        assert_eq!(guard.since_new, 0, "a new turn starts the count again");
    }

    #[test]
    fn calls_that_repeat_but_answer_something_new_are_not_a_loop() {
        // The same 17 calls in turn for 150 calls, as a worker polls the
        // files a build writes: what each answers is not what it answered.
        let mut guard = RepeatGuard::default();
        for call in 0..150 {
            let command = format!("ls out/{}", call % 17);
            let refused = run_and_answer(&mut guard, &command, &format!("file {call}"));
            assert_eq!(refused, None, "call {call}");
            assert_eq!(guard.stopped(), None, "call {call}");
        }
        assert!(guard.since_new < NOTHING_NEW);
    }

    #[test]
    fn an_answer_that_differs_only_in_what_moves_is_not_new() {
        let mut guard = RepeatGuard::default();
        let page = |date: &str, ms: u64| json!({"status": 200, "body": "same", "headers": {"date": date}, "duration_ms": ms});
        let args = json!({"url": "http://example.test/"});
        guard.call("fetch_url", &args, false);
        guard.answered("fetch_url", &args, Ok(&page("Mon", 5)));
        guard.call("fetch_url", &args, false);
        assert_eq!(guard.since_new, 1);
        guard.answered("fetch_url", &args, Ok(&page("Tue", 9)));
        assert_eq!(
            guard.since_new, 1,
            "the page is the same: only its headers moved"
        );
        guard.answered(
            "fetch_url",
            &args,
            Ok(&json!({"status": 200, "body": "other"})),
        );
        assert_eq!(guard.since_new, 0, "its body changed");
    }

    /// A turn has no ceiling on its length: work that keeps finding something
    /// new runs until it reports, and only a circle or a repeated failure
    /// stops it.
    #[test]
    fn a_long_turn_of_calls_that_keep_finding_new_things_is_never_stopped() {
        let mut guard = RepeatGuard::default();
        for call in 0..1_000 {
            // Different calls with different answers.
            let refused = run_and_answer(&mut guard, &format!("echo {call}"), &call.to_string());
            assert_eq!(refused, None, "call {call}");
            assert_eq!(guard.stopped(), None, "call {call}");
        }
    }

    #[test]
    fn a_long_turn_of_different_calls_with_writes_is_not_stopped() {
        // Eighty edits, each different, with a test run after every fourth:
        // the same call again, but between edits, and with answers that move.
        let mut guard = RepeatGuard::default();
        for edit in 0..80 {
            let write = format!("cat > src/part{edit}.rs <<'EOF'\nfn part{edit}() {{}}\nEOF");
            assert_eq!(run_and_answer(&mut guard, &write, ""), None, "edit {edit}");
            if edit % 4 == 3 {
                let tests = format!("{edit} tests passed");
                assert_eq!(run_and_answer(&mut guard, "cargo test", &tests), None);
            }
            assert_eq!(guard.stopped(), None, "edit {edit}");
        }
    }

    #[test]
    fn waiting_on_a_job_is_not_counted_against_the_turn() {
        let mut guard = RepeatGuard::default();
        let wait = json!({"job": "job_1"});
        for poll in 0..300 {
            assert_eq!(guard.call("job_wait", &wait, true), None, "poll {poll}");
        }
        assert_eq!(
            (guard.since_new, guard.stopped()),
            (0, None),
            "a wait is no call that found nothing new"
        );
        // Between waits a worker reads the log the job writes, which moves.
        for poll in 0..100 {
            assert_eq!(guard.call("job_wait", &wait, true), None);
            let seen = run_and_answer(&mut guard, "tail -n 3 build.log", &format!("line {poll}"));
            assert_eq!(seen, None, "poll {poll}");
        }
        assert_eq!(guard.stopped(), None);
    }

    /// `run`, answered as a command that failed with `stderr`.
    fn fail(guard: &mut RepeatGuard, code: &str, stderr: &str) -> Option<Repeated> {
        let refused = run(guard, code);
        let receipt = json!({"exit_code": 1, "stdout": "", "stderr": stderr});
        guard.answered("run_code", &json!({"code": code}), Ok(&receipt));
        refused
    }

    /// The edit that went wrong on #94: a Python script on a heredoc, its file
    /// named on its first line, and a body that differs a little each time.
    fn edit(attempt: usize) -> String {
        let spaces = " ".repeat(attempt);
        let quote = if attempt.is_multiple_of(2) { '"' } else { '\'' };
        format!(
            "cd /work/.harness && python3 - <<'PY'\np{spaces}={quote}bin/lib.sh{quote}\n\
             s=open(p).read()\nold = {quote}retry {attempt}{quote}\nassert old in s\nPY"
        )
    }

    /// What it printed: the same error, at a line that moves.
    fn traceback(attempt: usize) -> String {
        format!(
            "Traceback (most recent call last):\n  File \"<stdin>\", line {}, in <module>\n\
             AssertionError\n",
            6 + attempt
        )
    }

    #[test]
    fn three_edits_that_fail_alike_stop_the_turn_though_no_two_calls_are_the_same() {
        let mut guard = RepeatGuard::default();
        for attempt in 1..=STOP_FAILING {
            assert_eq!(guard.stopped(), None, "before attempt {attempt}");
            assert_eq!(fail(&mut guard, &edit(attempt), &traceback(attempt)), None);
            // Between the edits, a look at the file: a different slice, which
            // answers something new, so nothing else here sees a loop.
            let look = format!("sed -n {attempt},{}p bin/lib.sh", attempt + 20);
            run_and_answer(&mut guard, &look, &format!("slice {attempt}"));
        }
        assert_eq!(guard.stopped(), Some(Stopped::Failing(STOP_FAILING)));
        let said = guard.sentence().unwrap();
        assert_eq!(
            said,
            "I tried 3 times to run `python3` on lib.sh, and it failed the same way each \
             time, so I stopped. Is there another way you want me to handle lib.sh, or \
             should I stop here?"
        );
        for raw in ["<<", "PY", "open(p)", "retry", "assert"] {
            assert!(!said.contains(raw), "{raw:?} is quoted in {said}");
        }
        assert_eq!(
            Stopped::Failing(3).note(),
            "made 3 calls of one kind that failed the same way each time"
        );
        guard.begin_turn();
        assert_eq!(guard.stopped(), None, "a new turn is a new chance");
    }

    #[test]
    fn the_same_failing_command_three_times_is_stopped_at_the_third_call() {
        let mut guard = RepeatGuard::default();
        for _ in 0..2 {
            assert_eq!(fail(&mut guard, "cargo test --lib", "error[E0432]"), None);
        }
        assert_eq!(guard.stopped(), None);
        assert_eq!(
            run(&mut guard, "cargo test --lib"),
            Some(Repeated::InARow(3)),
            "the third is refused as it always was, and not run"
        );
        assert_eq!(guard.stopped(), Some(Stopped::Failing(3)));
        assert_eq!(
            guard.sentence().as_deref(),
            Some(
                "I tried 3 times to run `cargo test`, and it failed the same way each time, \
                 so I stopped. Is there another way you want me to handle this, or should I \
                 stop here?"
            )
        );
    }

    #[test]
    fn a_failure_that_changes_is_progress() {
        let mut guard = RepeatGuard::default();
        for attempt in 1..=12 {
            let error = format!("AssertionError: step {}", "x".repeat(attempt));
            assert_eq!(fail(&mut guard, &edit(attempt), &error), None);
            assert_eq!(
                guard.stopped(),
                None,
                "attempt {attempt} failed in a new way"
            );
        }
        // Only a count or a line that moves is the same failure.
        let mut guard = RepeatGuard::default();
        for attempt in 1..=2 {
            fail(&mut guard, &edit(attempt), &traceback(attempt));
        }
        fail(&mut guard, &edit(3), "a different error");
        assert_eq!(guard.stopped(), None);
        fail(&mut guard, &edit(4), &traceback(4));
        fail(&mut guard, &edit(5), &traceback(5));
        assert_eq!(
            guard.stopped(),
            None,
            "the count starts again from the new failure"
        );
    }

    #[test]
    fn a_success_or_a_wait_between_failures_starts_the_count_again() {
        let mut guard = RepeatGuard::default();
        for attempt in 1..=2 {
            fail(&mut guard, &edit(attempt), &traceback(attempt));
        }
        run_and_answer(&mut guard, &edit(3), "");
        for attempt in 4..=5 {
            fail(&mut guard, &edit(attempt), &traceback(attempt));
        }
        assert_eq!(guard.stopped(), None, "the edit that worked in between");

        // A server that is coming up: the check fails, a wait for its job
        // goes by, and it is asked again, as long as it takes.
        let wait = json!({"job": "job_1"});
        for poll in 0..30 {
            let check = "curl -sf http://localhost:8080/health";
            assert_eq!(fail(&mut guard, check, "refused"), None);
            assert_eq!(guard.call("job_wait", &wait, true), None);
            assert_eq!(guard.stopped(), None, "poll {poll}");
        }
    }

    #[test]
    fn a_command_that_says_no_is_not_a_failure() {
        // `grep` exits 1 and prints nothing when it finds nothing.
        let mut guard = RepeatGuard::default();
        for pattern in ["alpha", "beta", "gamma", "delta"] {
            let command = json!({"code": format!("grep -n {pattern} src/lib.rs")});
            let no = json!({"exit_code": 1, "stdout": "", "stderr": ""});
            guard.call("run_code", &command, false);
            guard.answered("run_code", &command, Ok(&no));
        }
        assert_eq!(guard.stopped(), None);
        // The same command timing out is a failure.
        let mut guard = RepeatGuard::default();
        for _ in 0..3 {
            let timeout = json!({"exit_code": 1, "stdout": "", "stderr": "", "timed_out": true});
            let command = json!({"code": "make"});
            guard.call("run_code", &command, false);
            guard.answered("run_code", &command, Ok(&timeout));
            if guard.stopped().is_some() {
                break;
            }
        }
        assert_eq!(guard.stopped(), Some(Stopped::Failing(3)));
    }

    #[test]
    fn a_tool_that_refuses_three_times_the_same_way_is_stopped_too() {
        let mut guard = RepeatGuard::default();
        let args = json!({"url": "http://example.test/missing"});
        for call in 0..3 {
            guard.call("fetch_url", &args, false);
            guard.answered("fetch_url", &args, Err("blocked address 10.0.0.1"));
            // Another call that goes through between them changes nothing.
            run_and_answer(&mut guard, &format!("ls {call}"), "ok");
        }
        assert_eq!(guard.stopped(), Some(Stopped::Failing(3)));
        assert_eq!(
            guard.sentence().as_deref(),
            Some(
                "I tried 3 times to call fetch_url, and it failed the same way each time, so \
                 I stopped. Is there another way you want me to handle this, or should I \
                 stop here?"
            )
        );
    }

    #[test]
    fn a_wait_between_refusals_is_a_call_that_went_through() {
        let mut guard = RepeatGuard::default();
        let run = json!({"code": "rg -n for_config src"});
        for _ in 0..2 {
            assert_eq!(guard.call("run_code", &run, false), None);
        }
        assert_eq!(
            guard.call("run_code", &run, false),
            Some(Repeated::InARow(3)),
            "the third identical call is a circle"
        );
        assert_eq!(guard.call("job_wait", &json!({"job": "job_1"}), true), None);
        assert_eq!(
            guard.call("run_code", &run, false),
            None,
            "a wait is a call that went through and ends the circle"
        );
    }

    #[test]
    fn what_the_person_is_told_is_one_question_whatever_stopped_the_turn() {
        let long = "x".repeat(28);
        let mut worst = RepeatGuard::default();
        for attempt in 1..=3 {
            let command = format!(
                "cargo test; git diff; npm run; sed -i s/a/b/ {long}.md {attempt}; yarn add"
            );
            fail(&mut worst, &command, "boom");
        }
        let mut stopped = vec![worst];
        let mut circle = RepeatGuard::default();
        loop_on(&mut circle, "ls", 5);
        stopped.push(circle);
        for guard in stopped {
            let said = guard.sentence().unwrap();
            assert_eq!(said.matches('?').count(), 1, "{said}");
            assert!(said.ends_with('?') && said.chars().count() < 500, "{said}");
        }
    }
}
