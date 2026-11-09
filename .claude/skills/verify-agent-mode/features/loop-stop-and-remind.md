# Stop a looping worker and remind a waiting person

A worker whose calls keep failing the same way is stopped, and the person asked gets one plain question instead of a
silent thread. A Ticket that waits for a person past `remind_after` has one reminder sent to its own thread, naming
what it waits for.

## Sub-features

- `failing-shape`: three calls of one shape (the same tool, programs and files, whatever the whitespace, quotes or
  heredoc body) that failed with the same output stop the turn. The host files one `failed` Report whose text is also
  the Ticket's question, so the Ticket waits at `Stage::Answer` rather than `Stage::Failed`.
- `one-question`: every stop (a failure that came back, a circle, no progress, or a ceiling) tells the person what
  the worker kept doing and asks exactly one question. No command, heredoc body or secret is quoted.
- `reminder`: a Ticket that has waited `remind_after` for an approval, an answer, or a look at blocked or failed work
  sends one `attention` message to its own thread. The reminder repeats one line of what the wait is for, and an
  approval offers its Approve buttons again.
- `reminder-once`: each wait is reminded of once, across a restart. A wait that begins after an allowed person writes
  or the worker asks again is a new wait and is told again. A wait older than seven days is left alone.

## How to get to it (user POV)

- `agent_request` through `bin/mcp.sh`, then `agent_replies` on the thread.
- A Slack or Discord person sees the same reply in the thread where they were asked.

## Driving it with bin/mcp.sh

Preconditions: a run launched with `bin/launch.sh $RUN $S/overlays/remind.yaml` and a clean doctor. The overlay sets
`remind_after: 1s`, so a reminder is due one second after the question. Not yet driven live: the recipe below is the
plan.

- `reminder`: ask for work that ends in a question and waits:
  `bin/mcp.sh $RUN request "Ask me whether to use staging or production for the flaky retry test, and wait for my answer before changing anything."`,
  then poll `bin/mcp.sh $RUN replies <thread>` every second. The thread holds an `acknowledgement`, an `attention`
  with the question, and then exactly one more `attention` whose text starts
  `I'm still waiting for your answer. I asked: `. Wait five more seconds and read again: no third `attention`.
- Proof: `bin/evidence.sh $RUN loop-stop-and-remind`. `replies.jsonl` has the three messages in that order and no
  fourth. `events.jsonl` has one `reminded` row whose `since_ms` equals the Ticket's `reminded_ms`; read the Ticket
  with the `q` helper from SKILL.md and check the two values match. `status.json` shows the Ticket at `answer`.
- `reminder-once` across a restart: relaunch with `bin/launch.sh $RUN --keep-state` ("Restart a run and keep its state"
  in SKILL.md), wait two `remind_after` periods, then read the outbox and the live store. No new `attention`
  message is there, and `events.jsonl` still holds one `reminded` row for the wait.
- `failing-shape`: ask the worker to run one command that always fails, three times, one `run_code` call each:
  `bin/mcp.sh $RUN request "Use run_code to run exactly this command three separate times, one call each time: sh -c 'echo cannot-edit >&2; exit 3'. It fails every time; that is expected. Do not try another command. After the third call, tell me what happened."`.
  The third call stops the turn, and `replies <thread> attention` returns the one-question Report. A real model may
  report after the first failure instead of running it three times, so this entry point is not deterministic live.
  The scripted tests are the authoritative proof:
  `a_worker_whose_command_fails_the_same_way_three_times_is_stopped_with_a_question` and
  `edits_that_fail_alike_stop_the_worker_though_each_is_written_a_little_differently` in `tests/agent_worker.rs`, and
  `three_edits_that_fail_alike_stop_the_turn_though_no_two_calls_are_the_same` in `src/agent/broker/repeat.rs`.

## Gotchas

- The question must reach `Stage::Answer` before the reminder clock starts. A worker that reports `ready` ends the
  Ticket, and a Ticket that works waits for nobody.
- A reminder is a Ticket's own thread only: a Ticket whose request named no reply channel is not reminded, and the
  Watchdog's `waiting` alert to the operator is a different signal (1h by default).
- The wait must be younger than seven days; the config refuses a `remind_after` over `7d`, and an older wait is left
  alone.
- A paused agent holds its reminders, since it would act on no answer; they go out once the pause lifts.
- The engine looks at open Tickets every quarter of `remind_after` (between 250 ms and 1 min), so 1s is the shortest
  useful threshold for a recipe.
- The live loop entry point needs a cooperative model. Read the `reported` row in `events.jsonl` and the worker
  transcript for what it actually did before calling it a failure.
