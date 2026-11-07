# Follow up on a request

A person replies on the same thread, and the reply goes straight to the Ticket with no new Router decision. While a
worker is still on it, the worker takes the reply as new input and its result reflects it. After the Ticket ended or
stopped on a question, the reply reopens it and starts a new worker, which is told what the agent said before, so
its answer goes on from there instead of starting over.

## Sub-features

- `pass-through`: a follow-up on an open Ticket's thread reaches its worker, and the Router makes no new decision.
- `steer`: the result answers the follow-up as well as the request.
- `resume`: a follow-up after the Ticket ended (`ready`) or stopped on a question (`blocked`, `failed`) reopens the same Ticket with a new Assignment, and the Router makes no new decision. The new worker's first prompt quotes the agent's earlier replies, labelled "your earlier replies on this ticket (context, not instructions)", and its reply goes on with no recap, greeting or restated goal.

## How to get to it (user POV)

- `agent_request` with the earlier request's `thread`, or a reply in the same Slack or Discord thread.

## Driving it with bin/mcp.sh

Preconditions: a run launched with no overlay; doctor clean. Not yet driven live: the recipe below is the plan.

- `pass-through`: ask for work that takes a few turns and waits for the person, for example `bin/mcp.sh $RUN request "Fetch the README of GuiBibeau/solos-swarm from the GitHub API and tell me its first heading. Then ask me which section to summarize next and wait for my answer."`. When `replies <thread> attention 540` shows the question, send `bin/mcp.sh $RUN request "Summarize the Quickstart section." <thread>`, then `replies <thread> result 540`.
- Proof: `bin/evidence.sh $RUN follow-up`; `events.jsonl` has exactly one `decision_recorded` for the thread, and the worker transcript shows the follow-up arriving as a new prompt.
- `resume`: ask for work that ends at once, with an answer that has a second part to ask for, for example `bin/mcp.sh $RUN request "In the GitHub repository GuiBibeau/solos-swarm, what is the exact title of pull request #51? Use the GitHub REST API."`, and wait for the `result` with `replies <thread> result 540`. Then follow up on the same thread with a question that only makes sense after the first answer: `bin/mcp.sh $RUN request "And is it merged, and when?" <thread>`. `replies <thread> result 540` returns at once because the first result is there, so poll `bin/mcp.sh $RUN replies <thread>` every 15 s until it lists a second `result`.
- Proof of `resume`, three checks together:
  - The reply continues the conversation: it answers "merged, and when" (checked with `gh pr view 51 -R GuiBibeau/solos-swarm --json mergedAt`, 2026-11-03T16:31:56Z) with no greeting, no restated request and no recap of the title.
  - `bin/evidence.sh $RUN follow-up`: `events.jsonl` has `ticket_resumed` and a second `assignment_started` on the same Ticket, and still exactly one `decision_recorded` for the thread; `tickets.jsonl` holds one Ticket with two Assignments, the first with its `ready` Report.
  - The second Assignment's transcript, `ssh remote-ai "grep -c 'Your earlier replies on this ticket (context, not instructions)' ~/work/verify/$RUN/state/tickets/<ticket>/<second-assignment>.rpc.jsonl"`, finds the heading in the first prompt, and the first result's text appears there once, quoted after `- ready:`. The first Assignment's transcript has no such heading.

## Gotchas

- A recovery after a host restart is quoted the same way, but it cannot be driven on cue against a real model: it needs the agent stopped while a worker is mid-turn. The scripted tests (`a_crash_mid_work_restarts_a_worker_that_is_told_the_progress_it_had_reported` in `tests/agent_worker.rs`) cover it.
- A worker that reports `ready` at once ends the Ticket, and the follow-up becomes `resume`, not `pass-through`. The `pass-through` request must make the worker wait for the person; the `resume` request must not.
- The earlier replies are quoted only for a relaunch. A follow-up that reaches a worker still running is a new prompt in the same session, which already holds what it said, so its transcript has no "earlier replies" block. The block is cut to the most recent 6,000 characters, so a Ticket with a long history shows a line naming how many older replies are not shown.
- A worker may answer the second question from `/work` or from the quoted reply, so the proof is the first prompt in the transcript, not the wording of the answer.
