# Chat on Discord with approvals

A person mentions the bot in a Discord channel. The agent replies to that message, posts an approval request with
Approve and Stop buttons when the work's budget is above the auto-approve line, acts on the click of a person the
Policy allows, and posts the result as a reply in the same chain.

## Sub-features

- `gateway`: the bot shows online in the server's member list once `agent serve` starts.
- `mention`: `@bot <request>` from a person in `policy.people` becomes a Trigger and an accepted Ticket.
- `approval`: the bot posts "<title> needs approval first: one worker session with a budget of $<n>." with Approve and Stop.
- `approve`: an approver's click posts "Approved by <name>. The work starts now." and the work runs.
- `result`: the result arrives as a reply to the same message.
- `stop`: Stop on a new request ends it (not yet driven).

## How to get to it (user POV)

- A Discord server with the bot installed (Developer Portal: Message Content Intent on, Interactions Endpoint URL empty; invite scope `bot`, permissions 274877975552), and a mention of the bot in a listed server or channel.

## Driving it with a person on Discord

Preconditions: the bot token, written by the operator into a 0600 file on the box (never through chat or a screenshot); an agent.yaml with `kind: discord` source and channel, `policy.people` and `policy.approvers` as `discord:<user id>`, and `budgets.session_microusd` above `policy.auto_approve_microusd` so a request asks for approval. The operator's test server on 2026-11-04 was "zelix" (guild 1567472680515403786, `#general` 1567472681463451711, app 1567474348112216166), with the config in `~/work/zelix/agent.yaml` on the box.

- `gateway`: start `agent serve` with that config; in Chrome (Claude in Chrome), open the channel and show the member list: the bot is online.
- `mention`, `approval`, `approve`, `result`: the person posts `@bot hello` and clicks Approve. Screenshot the channel; the agent's store shows `trigger_received`, `ticket_created`, `decision_recorded` (`accept`), `ticket_approved` by the person's user id, `assignment_started`, `reported` (`ready`), `ticket_ended`, and every `send` effect done on its first attempt.

## Gotchas

- Clicks and posts come from a person: never post or click on the operator's behalf without their go-ahead in chat.
- The Discord check on 2026-11-04 used the scripted Router and executor ("Scripted executor: no work ran."): it proves the chat loop, not real work. For real work on Discord, use the real Router and Pi worker of `agent.verify.yaml` with `kind: discord` added.
- After an answer the buttons stay on the approval message; a second click is refused with a reply.
- Discord remembers a post's nonce for a few minutes only; a retry after a long outage can post twice.
