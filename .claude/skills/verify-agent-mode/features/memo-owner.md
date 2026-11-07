# The owner's memory shapes later work

The owner tells the agent something to remember with `agent_memo_add` (or `solos-swarm agent memo add` on the box).
It is the owner's own word, so it is active at once, with no review: the Router's system prompt and the first prompt
of every later worker hold it as a quoted line of data, and the hash chain records which sessions it shaped
(`memos_shown`). Retiring it takes it out of every later prompt; forgetting it erases its text and keeps its digest.

## Sub-features

- `owner-memo`: `agent_memo_add` returns `done`, the memo `active`, trust `person`, author `mcp:verifier`, and `agent_memos` lists it.
- `shapes-work`: an English request after the memo gets its `result` in the language the memo asks for.
- `memos-shown`: `events.jsonl` holds a `memos_shown` naming the memo for the Router (`"to":{"kind":"router"}`) and for the request's worker (`"to":{"kind":"worker",...}`), each with an `index_sha256`, and no event holds the memo's text.
- `retire`: after `agent_memo_retire`, `agent_memos` is empty, the next result is in English, and its worker has no `memos_shown`.
- `forget`: after `agent memo forget` on the box, `agent memo list --all` shows the memo `forgotten` with no text, and `agent trail` (which checks the whole chain) exits 0.
- `refused`: an operator the Policy does not name gets "is not allowed to change the agent's memory", and nothing changes.

## How to get to it (user POV)

- An MCP host (Claude Code, Codex) with `solos-swarm mcp --operator <name>`: `agent_memo_add`, `agent_memos`, `agent_memo_retire`.
- On the box: `solos-swarm agent memo add|list|retire|forget --state-dir <state> --operator <name>`, which go through the running agent as `mcp:<name>`.

## Driving it with bin/mcp.sh

Preconditions: a run launched with no overlay; doctor clean. Not yet driven live: the recipe below is the plan.

- `owner-memo`: `bin/mcp.sh $RUN memo-add "Always answer in French, whatever language the request is in."` prints `{"result": "done", ...}`; keep `memo.id`. `bin/mcp.sh $RUN memos` lists it.
- `shapes-work`: `bin/mcp.sh $RUN request "What is the capital of Canada? Answer in one sentence."`, then `bin/mcp.sh $RUN replies <thread> result 540`. The result is French ("La capitale du Canada est Ottawa.").
- `memos-shown`: `bin/evidence.sh $RUN memo-owner`, then in the snapshot `jq -c 'select(.type == "memos_shown")' events.jsonl`: one line with `"to":{"kind":"router"}` and one with `"to":{"kind":"worker","ticket":"<the ticket>",...}`, both with `"memos":["<id>"]`. `grep -c 'whatever language' events.jsonl` prints 0.
- `retire`: `bin/mcp.sh $RUN memo-retire <id> "verification over"`, `bin/mcp.sh $RUN memos` prints `[]`, then `request "What is the capital of Australia? Answer in one sentence."` gets an English result, and the evidence shows no `memos_shown` for its worker.
- `forget`: `ssh remote-ai "~/work/verify/$RUN/bin/solos-swarm agent memo forget --state-dir ~/work/verify/$RUN/state --operator verifier <id>"`, then `agent memo list --all --state-dir ...` shows `forgotten` with no `>` line, and `agent trail --state-dir ~/work/verify/$RUN/state` exits 0.
- `refused`: `ssh remote-ai "~/work/verify/$RUN/bin/solos-swarm agent memo add --state-dir ~/work/verify/$RUN/state --operator stranger 'x'"` exits 1 with "is not allowed to change the agent's memory"; `memos` is unchanged.

## Gotchas

- A Router session reads the memos when it starts. One that was running when the memo changed ends before its next turn, and `serve.log` says "the global memos changed"; that is expected, not a failure.
- A memo is quoted data the model weighs against the worker's rule to answer "in the language and tone of their message". An English answer to `shapes-work` is a finding about that boundary, not a broken memo: check `memos_shown` first.
- `memo-add` and `memo-retire` wait up to 20 s for the agent's answer. Without an answer they fail with "Is it running?": run `doctor.sh`.
- `verifier` may change the memory because `agent.verify.yaml` lists `mcp:verifier` in `policy.people`.
