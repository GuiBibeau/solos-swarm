//! A worker that sends the same tool call again and again is looping, and
//! its model keeps being paid for it: one Ticket spent its whole budget
//! on a hundred byte-identical `run_code` calls. The broker sees every
//! tool call a seat makes, so it is the place to say so: when a seat's
//! calls start to go round in a circle, the call that closes the third
//! round is refused with a reason the model reads, which says what the call
//! answered the first time and asks the model to change its approach or to
//! report.
//!
//! A circle is one call, or up to three calls in turn, repeated three
//! times: `A A A`, or `A B A B A B`. A call that merely comes back among
//! others that differ, as a test run does between edits, is no circle.
//!
//! A refusal alone does not stop a model that has gone degenerate: another
//! Ticket sent the same refused call 767 times, a model call for each. So
//! the guard also counts the calls it refuses, and past a limit it says the
//! seat's turn is [`Stopped`]. The broker then answers that seat's tool
//! calls with the stop and refuses its next model call, which ends a Pi
//! turn without another call to the provider, and the host reports in the
//! worker's place.
//!
//! The guard is per seat, remembers hashes of the last few calls only, and
//! leaves alone a tool whose registry entry says a repeat is normal.

use super::clip;
use serde_json::Value;
use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};

/// A circle goes round this many times before it is refused.
const ROUNDS: usize = 3;
/// The most calls one round of a circle has.
const LONGEST_ROUND: usize = 3;
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
/// How many calls' results the guard keeps, to say what a refused call
/// answered the first time.
const ANSWERS: usize = 2 * RECENT;
/// How much of an earlier result a refusal quotes: this many lines of
/// stdout and of stderr, each this long, and this long in all.
const QUOTED_LINES: usize = 3;
const QUOTED_LINE_CHARS: usize = 120;
const QUOTED_CHARS: usize = 320;

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

/// Why a seat's turn is stopped: it kept sending calls the guard refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stopped {
    /// This many refused calls in a row.
    InARow(usize),
    /// This many refused calls in the turn.
    InTurn(usize),
}

/// One seat's recent calls: a hash of the tool's name and arguments, or
/// `None` for a call of a tool that may repeat, which is like no other.
#[derive(Debug, Default)]
pub(super) struct RepeatGuard {
    recent: VecDeque<Option<u64>>,
    /// What the latest calls answered, by call, oldest first.
    answers: VecDeque<(u64, String)>,
    refused_in_a_row: usize,
    refused_in_turn: usize,
    stopped: Option<Stopped>,
}

/// A hash of a call: the tool's name and its arguments.
fn key(tool: &str, args: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    tool.hash(&mut hasher);
    args.to_string().hash(&mut hasher);
    hasher.finish()
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
    /// refusals in a row; see [`stopped`](Self::stopped).
    pub(super) fn call(&mut self, tool: &str, args: &Value, repeatable: bool) -> Option<Repeated> {
        let repeated = self.circle((!repeatable).then(|| key(tool, args)));
        if repeated.is_none() {
            self.refused_in_a_row = 0;
            return None;
        }
        self.refused_in_a_row += 1;
        self.refused_in_turn += 1;
        if self.stopped.is_none() {
            self.stopped = if self.refused_in_a_row >= STOP_IN_A_ROW {
                Some(Stopped::InARow(self.refused_in_a_row))
            } else if self.refused_in_turn >= STOP_IN_A_TURN {
                Some(Stopped::InTurn(self.refused_in_turn))
            } else {
                None
            };
        }
        repeated
    }

    /// Why the turn is stopped, once the refusals reach a limit. It stays
    /// stopped until the next turn begins.
    pub(super) fn stopped(&self) -> Option<Stopped> {
        self.stopped
    }

    /// Keep what a call of `tool` with `args` answered, so a later refusal
    /// of the same call can say so. A refusal of the call is an answer too:
    /// the same call is refused the same way.
    pub(super) fn answered(&mut self, tool: &str, args: &Value, answer: Result<&Value, &str>) {
        let key = key(tool, args);
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
    use super::{ANSWERS, RECENT, RepeatGuard, Repeated, STOP_IN_A_ROW, STOP_IN_A_TURN, Stopped};
    use serde_json::json;

    fn run(guard: &mut RepeatGuard, code: &str) -> Option<Repeated> {
        guard.call("run_code", &json!({"code": code}), false)
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

        let mut three = RepeatGuard::default();
        for round in 1..=3 {
            for call in ["a", "b", "c"] {
                let refused = run(&mut three, call);
                let closes = round == 3 && call == "c";
                assert_eq!(refused.is_some(), closes, "round {round} {call}");
                if closes {
                    assert_eq!(refused, Some(Repeated::InTurn(3)));
                }
            }
        }

        // Four different calls in a round are too many to be told from work.
        let mut four = RepeatGuard::default();
        for _ in 0..4 {
            for call in ["a", "b", "c", "d"] {
                assert_eq!(run(&mut four, call), None);
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
        run(&mut guard, "ls");
        run(&mut guard, "pwd");
        assert_eq!(
            guard.stopped(),
            Some(Stopped::InARow(STOP_IN_A_ROW)),
            "a stopped turn stays stopped, whatever comes next"
        );
        guard.begin_turn();
        assert_eq!(guard.stopped(), None, "a new turn is a new chance");
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
}
