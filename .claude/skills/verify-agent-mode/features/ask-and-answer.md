# Ask and get real work done

A person asks the agent for a piece of work in their own words. The Router, a real model, accepts it and says so on
the thread; a sandboxed Pi worker does the work with its tools (here `fetch_url` to a public API) and posts the
result on the same thread.

## Sub-features

- `acknowledge`: an accepted request gets an `acknowledgement` reply that names the work ("On it: …").
- `real-work`: the worker calls a real tool, and the result reports what the tool returned.
- `known-answer`: a request whose answer is checkable gets that exact answer.
- `ticket-closes`: a `ready` result ends the Ticket (`ticket_ended` with `completed`).

## How to get to it (user POV)

- An MCP host (Claude Code, Codex) with `solos-swarm mcp --operator <name>`: `agent_request`, then `agent_replies`.
- The same request from a Slack or Discord person mentioning the bot (see discord-chat.md for that entry point).

## Driving it with bin/mcp.sh

Preconditions: a run launched with no overlay; doctor clean.

- `real-work`: `bin/mcp.sh $RUN request "List the open pull requests of the GitHub repository GuiBibeau/solos-swarm, with each one's number and title. Use the GitHub REST API."`, then `bin/mcp.sh $RUN replies <thread> result 540`. The replies hold an `acknowledgement` and a `result`. Check the result against `gh pr list -R GuiBibeau/solos-swarm --state open --json number,title`. An empty list is weak proof on its own: also run `known-answer`.
- `known-answer`: `bin/mcp.sh $RUN request "In the GitHub repository GuiBibeau/solos-swarm, what is the exact title of pull request #51, and is it merged? Use the GitHub REST API."`. The result names "feat(agent): solos-swarm agent trail" and merged at 2026-11-03T16:31:56Z, as `gh pr view 51 -R GuiBibeau/solos-swarm --json title,state,mergedAt` does.
- `ticket-closes` and the tool call: `bin/evidence.sh $RUN ask-and-answer`; `events.jsonl` holds `trigger_received`, `eval_started`, `ticket_created`, `decision_recorded` (`accept`), `assignment_started`, `reported` (`ready`) and `ticket_ended`. The worker's transcript, `state/tickets/<ticket>/<assignment>.rpc.jsonl` on the box, holds the `fetch_url` call with its URL and the response body.

## Gotchas

- A Router that ignores a request sends no reply. `replies` then times out: read `mcp.sh $RUN status` (decisions) for its reason before calling the run failed.
- GitHub's unauthenticated API allows 60 calls an hour from the box's address. A 403 rate-limit result is the API, not the agent.
- On 2026-11-04 each probe cost under $0.002 (Router and worker together).
