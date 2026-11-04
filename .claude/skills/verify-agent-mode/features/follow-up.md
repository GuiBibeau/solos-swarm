# Follow up on a running request

While a request's worker is still on it, the person replies on the same thread. The reply goes straight to the
running worker as new input (pass-through), with no new Router decision, and the worker's result reflects it.

## Sub-features

- `pass-through`: a follow-up on an open Ticket's thread reaches its worker, and the Router makes no new decision.
- `steer`: the result answers the follow-up as well as the request.
- `resume`: a follow-up after the Ticket ended is judged by the Router again (not pass-through).

## How to get to it (user POV)

- `agent_request` with the earlier request's `thread`, or a reply in the same Slack or Discord thread.

## Driving it with bin/mcp.sh

Preconditions: a run launched with no overlay; doctor clean. Not yet driven live: the recipe below is the plan.

- `pass-through`: ask for work that takes a few turns and waits for the person, for example `bin/mcp.sh $RUN request "Fetch the README of GuiBibeau/solos-swarm from the GitHub API and tell me its first heading. Then ask me which section to summarize next and wait for my answer."`. When `replies <thread> attention 540` shows the question, send `bin/mcp.sh $RUN request "Summarize the Quickstart section." <thread>`, then `replies <thread> result 540`.
- Proof: `bin/evidence.sh $RUN follow-up`; `events.jsonl` has exactly one `decision_recorded` for the thread, and the worker transcript shows the follow-up arriving as a new prompt.

## Gotchas

- A worker that reports `ready` at once ends the Ticket, and the follow-up becomes `resume`, not `pass-through`. The request must make the worker wait for the person.
