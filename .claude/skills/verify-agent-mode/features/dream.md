# The daily dream checks memory against the transcripts

Once a day, at `memory.dream.at` (03:30 UTC by default), the agent asks itself for a dream when a Ticket ended since the
last one. The dream is an ordinary Ticket that the host judges in the Router's place. Before its sandbox starts, the
host stages into its `/work`, read-only, a digest of each Ticket that ended, copies of their workers' transcripts, and
every live memo. Its model calls go with DeepSeek's thinking on at `max` effort and are charged to the `memory`
account. It corrects a memo the transcripts contradict (`memo_propose` with `supersedes`), merges duplicates, retires
stale memos (`memo_retire`) and proposes lessons that showed in two Tickets or more, each citing transcript lines that
the host checks against the original files. Only a worker's own words count: a line that is a tool's answer (a
command's output included) or the prompt is outside text, which retires nothing, and a proposal that cites one is
never reviewed and waits for the owner. The review decides its other proposals, and the Watchdog's operator gets one
digest of what changed.

## Sub-features

- `asked`: a day with an ended Ticket gets a `dream:<slot>` trigger and a Ticket whose mandate source and requester are `dream`. A day with none gets no trigger and no Ticket.
- `staged`: the dream's `/work` holds `tickets/index.json`, one `tickets/<ticket>.json` per ended Ticket, `transcripts/<ticket>/<assignment>.rpc.jsonl`, and `memos/index.json`, each file mode `0444`. Each staged transcript's `sha256` in the index matches `sha256sum` of the original under `state/tickets/<ticket>/`.
- `thinks`: the dream's transcript shows `thinking` blocks. The `memory` account has its spend rows. The dream's own `ticket:` account has none, and no other Ticket's calls went with thinking on.
- `same-trap`: two real Tickets that hit the same trap (for example a `cargo test` in a repository whose tests run another way) lead the dream to propose one memo naming the trap. The memo cites a line from each Ticket in which the worker says it in its own words, with sources of kind `transcript` and trust `model`. The review keeps it, and the next Ticket's first prompt shows it.
- `corrects`: an active memo the transcripts contradict ends `retired` with `replaced_by` naming the dream's kept correction, whose `supersedes` names it.
- `output-waits`: a dream proposal that cites only a command's output is `external`, gets no `the review` line in `serve.log`, stays `proposed`, and the digest lists it as `Still waiting for you`. A `memo_retire` that cites such a line is refused with `outside text`.
- `digest`: the Watchdog's operator conversation gets exactly one `Memory, <day>: ...` message per dream, with links to the Tickets it read. Restarting the agent the same day sends no second message and starts no second dream.

## How to get to it (user POV)

- The operator's Discord or Slack conversation that `watchdog.operator` names: the daily digest arrives there.
- An MCP host with `solos-swarm mcp --operator <name>`: `agent_status` lists the dream's Ticket, and `agent_memos` lists the memos it changed.
- On the box: `solos-swarm agent memo list --all --json --state-dir <state>` shows `supersedes`, `replaced_by` and the reason of each retirement.

## Driving it with bin/mcp.sh

Preconditions: a run launched with an overlay that sets `memory.dream.at` a few minutes ahead (UTC, `HH:MM`) and a
`watchdog:` section whose operator is a conversation you can read, and a clean doctor. `serve.log` has `agent: the dream
comes daily at HH:MM, thinking at max`. Not yet driven live: the recipe below is the plan. The box has no `sqlite3`
command: the queries below use the `q` helper in SKILL.md.

- `same-trap`: before the dream's time, make two requests on the sandbox repository that each need its tests, for example `bin/mcp.sh $RUN request "On <repo>, run its tests and tell me whether they pass."` twice in separate threads, and wait for both results (`replies <thread> result 900`). Look in each transcript for the same failed first attempt (`grep -n 'cargo test' state/tickets/<ticket>/*.rpc.jsonl`).
- `asked`: after the time passes, `ssh remote-ai "grep 'the dream of' ~/work/verify/$RUN/serve.log"`. Then `bin/evidence.sh $RUN dream`, and `jq -c '.[1] | fromjson | select(.type == "ticket_created")' events.jsonl` shows the dream's Ticket. `q "select id from triggers where id like 'dream:%'"` shows one trigger.
- `staged`: `ssh remote-ai "ls -l ~/work/verify/$RUN/state/tickets/<dream>/work/transcripts/*/"` shows mode `-r--r--r--`. Compare `jq -r '.tickets[].assignments[] | .assignment + " " + .sha256' .../work/tickets/index.json` with `sha256sum state/tickets/<ticket>/<assignment>.rpc.jsonl` for each line. The index writes each digest as `sha256:<hex>`, and leaves `sha256` out for a transcript too large to copy.
- `thinks`: `grep -c '"thinking"' state/tickets/<dream>/<assignment>.rpc.jsonl` is above zero. `q "select account, count(*) from spend group by account"` shows `memory` rows and no `ticket:<dream>` row.
- `corrects` and `same-trap`: `solos-swarm agent memo list --all --json` shows the dream's proposals (author `<dream>/<assignment>`, with `supersedes` on a correction and `replaced_by` on the memo it replaced), and the review's decision on each. `q "select json_extract(body, '$.sources') from memos where json_extract(body, '$.author.ticket') = '<dream>'"` shows sources of kind `transcript`. `serve.log` has `agent: the review kept` or `kept in its own words` for each one. Then a new request on the repository: its worker's `memos_shown` names the kept memo.
- `digest`: the operator conversation shows one `Memory, <day>: the dream read N tickets ...` message with `Kept`, `Retired` or `Rejected` lines and ticket links. Restart the agent on the same state ("Restart a run and keep its state" in SKILL.md: `launch.sh $RUN --keep-state`, since a verify run has no systemd unit), wait two Watchdog intervals, and confirm there is no second message and no second `dream:` trigger.

## Gotchas

- No Ticket ended since the last dream means no dream: the slot is noted in `state/dream-slot` and nothing is asked until the next day. Set `at` after your requests end.
- A run with `--until-idle` never dreams.
- `same-trap` needs workers that say what they found, not only commands that print it. A worker that ran `cargo test` and moved on without a word leaves the dream only command output to cite, which is `external`: the lesson then waits for the owner. That is the rule working, not a broken dream.
- The dream may decide nothing needs changing. That is a valid outcome. Check its report (`agent status`) and transcript before calling the feature broken.
- At `max` effort each of the dream's calls may take minutes. The Watchdog may report it `stalled` ("has gone quiet") if it sends no report for 30 minutes.
- The digest waits up to 30 minutes for the review of the dream's proposals, then lists any proposal still waiting.
- `memory.daily_microusd` caps the review and the dream together. A cap that is reached stops the dream's calls, and the dream ends failed.
