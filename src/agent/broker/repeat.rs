//! A worker that sends the same tool call again and again is looping, and
//! its model keeps being paid for it: one Ticket spent its whole budget
//! on a hundred byte-identical `run_code` calls. The broker sees every
//! tool call a seat makes, so it is the place to say so: when a seat's
//! calls start to go round in a circle, the call that closes the third
//! round is refused with a reason the model reads, which asks it to change
//! its approach or to report.
//!
//! A circle is one call, or up to three calls in turn, repeated three
//! times: `A A A`, or `A B A B A B`. A call that merely comes back among
//! others that differ, as a test run does between edits, is no circle.
//!
//! The guard is per seat, remembers hashes of the last few calls only, and
//! leaves alone a tool whose registry entry says a repeat is normal.

use serde_json::Value;
use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// A circle goes round this many times before it is refused.
const ROUNDS: usize = 3;
/// The most calls one round of a circle has.
const LONGEST_ROUND: usize = 3;
/// How many of a seat's latest calls the guard remembers.
const RECENT: usize = ROUNDS * LONGEST_ROUND;

/// Why a call is refused as a repeat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Repeated {
    /// This exact call, this many times in a row.
    InARow(usize),
    /// A round of this many different calls, gone round `ROUNDS` times.
    InTurn(usize),
}

impl Repeated {
    /// What the model reads: what it did, what not to do, and what to do
    /// instead.
    pub(super) fn why(self, tool: &str) -> String {
        let did = match self {
            Self::InARow(times) => format!("you made this exact call {times} times in a row"),
            Self::InTurn(calls) => format!(
                "you made these {calls} calls in turn, {ROUNDS} times over, and this is the \
                 last of them"
            ),
        };
        format!(
            "{tool} refused: {did}, so repeating it will not tell you anything new. Do not send \
             it again. Change your approach: use other arguments or another tool, or stop and \
             report with worker_report (ready if the work is done, blocked or failed with the \
             reason if you cannot go on). If you are waiting for something to change, end your \
             turn: a finished job comes back as new input, or use job_wait."
        )
    }
}

/// One seat's recent calls: a hash of the tool's name and arguments, or
/// `None` for a call of a tool that may repeat, which is like no other.
#[derive(Debug, Default)]
pub(super) struct RepeatGuard {
    recent: VecDeque<Option<u64>>,
}

impl RepeatGuard {
    /// A new turn starts from nothing: new input is a reason to look again.
    pub(super) fn begin_turn(&mut self) {
        self.recent.clear();
    }

    /// Note a call of `tool` with `args`, and say whether it closes a
    /// circle. A refused call is noted as well, so one that is sent again
    /// stays refused. A `repeatable` tool is noted but never refused, and
    /// it ends any circle it falls into: its call again is normal because
    /// what it answers moves.
    pub(super) fn call(&mut self, tool: &str, args: &Value, repeatable: bool) -> Option<Repeated> {
        let call = (!repeatable).then(|| {
            let mut hasher = DefaultHasher::new();
            tool.hash(&mut hasher);
            args.to_string().hash(&mut hasher);
            hasher.finish()
        });
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

#[cfg(test)]
mod tests {
    use super::{RECENT, RepeatGuard, Repeated};
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
        let why = Repeated::InARow(3).why("run_code");
        for says in [
            "run_code refused",
            "this exact call 3 times in a row",
            "Do not send it again",
            "Change your approach",
            "worker_report",
            "job_wait",
        ] {
            assert!(why.contains(says), "{says:?} is not in {why}");
        }
        let why = Repeated::InTurn(2).why("read");
        assert!(why.contains("these 2 calls in turn, 3 times over"), "{why}");
    }
}
