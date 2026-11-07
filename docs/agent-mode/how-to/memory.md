# Manage the agent's memory

Memos are what the agent learned on one Ticket that a later, different Ticket should know. You write them and take
them back. Workers on Pi propose them too, and a review decides each one with no click. How proposals, the review and
the daily dream work is in [Memory](../runtime.md#memory), and the design is [memory/design.md](../memory/design.md).

The commands below that change memory go through the running agent as `mcp:<operator>`, as `agent stop` does. So the
agent needs a `kind: mcp` source, `agent serve` must be running, and `policy.people` or `policy.approvers` must name
`mcp:<operator>`. Each such command exits 0 once the memory is as asked, 1 if the agent refused, and 2 if it did not
answer within `--wait-secs` (20 by default).

## Write a memo

```sh
solos-swarm agent memo add --state-dir DIR --operator NAME "Release notes go in CHANGELOG.md, newest first."
```

A memo is at most 400 characters. `--scope global`, the default, puts it in the Router's system prompt and in every
worker's first prompt. `--scope repo:OWNER/NAME`, for a repository in `forge.repos`, puts it only in the first prompt
of workers on that repository. Your memo is your own word, so it is active at once.

## List the memos

```sh
solos-swarm agent memo list --state-dir DIR [--all] [--json]
```

`list` reads the Store and writes nothing. It shows the proposed and active memos, with who wrote each and how far it is
trusted. `--all` adds the retired and forgotten ones.

## Retire a memo

```sh
solos-swarm agent memo retire --state-dir DIR --operator NAME mm_0123456789abcdef --reason "The release process changed."
```

Retiring takes a memo out of every prompt for good, with why. `list --all` still shows it.

## Forget a memo

```sh
solos-swarm agent memo forget --state-dir DIR --operator NAME mm_0123456789abcdef
```

Forgetting erases the memo's text, quotes and reasons from the Store. A memo's text is in its row of `agent.db`, and
only its digest is on the event chain, so the chain still verifies. The Store writes with SQLite's `secure_delete` and
empties its WAL after a forget, so the text is in neither `agent.db` nor `agent.db-wal` afterwards. Two copies stay
until they are pruned: the `add` order in `state/mcp/commands.jsonl`, and the transcripts of workers that were shown
the memo.

## Do the same from an MCP host

An MCP host started with `solos-swarm mcp --state-dir DIR --operator NAME` has `agent_memos`, `agent_memo_add` and
`agent_memo_retire`, which do what `agent memo list`, `add` and `retire` do. See
[Hand the agent work from an MCP host](mcp.md).

## Change what workers may do

Every field of `memory:` is optional:

```yaml
memory:
  proposals: true            # workers may propose memos and read /memos; false: only the owner writes them
  daily_microusd: 500000     # the review's and the dream's cap per UTC day on the `memory` account; absent: no cap
  review:
    model: deepseek-flash    # the workers' model unless set; provider, if set, must be the workers'
    thinking: max            # DeepSeek's reasoning_effort, with thinking on: low, high or max (the default)
    max_output_tokens: 131072  # default 65536, or 131072 at max: DeepSeek's thinking-mode defaults
  dream:
    enabled: true            # on wherever workers propose memos
    at: "03:30"              # UTC, daily; before the 04:30 prune, so the transcripts are still there
    thinking: max            # the dream's reasoning_effort, with thinking on
    max_output_tokens: 131072  # what one of its calls may answer, reasoning included
```

- To keep memory to your own memos, set `proposals: false`. Workers then have no `memo_propose` and no `/memos`, and
  there is no dream.
- To cap what the review and the dream spend, set `daily_microusd`.
- To stop the daily dream and keep proposals, set `dream.enabled: false`.

The dream's digest goes to the Watchdog's operator, so without a `watchdog:` section there is no digest. Every field is
in the [configuration reference](../configuration.md#memory).
