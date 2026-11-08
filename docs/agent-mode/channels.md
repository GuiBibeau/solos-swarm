# Sources and channels

A Source turns what happens in a chat, on GitHub or on a clock into Triggers. A Channel sends the agent's replies back
to where a Trigger came from. This page explains how the agent talks, and how each kind of Source and Channel behaves.
To connect one, see the how-to guides in the [index](README.md#set-up-and-operate).

| Kind | Source | Channel | Approve and Stop buttons |
|---|---|---|---|
| Slack | Socket Mode, `src/agent/slack/` | Web API | yes |
| Discord | Gateway, `src/agent/discord/` | REST | yes |
| GitHub | REST polling, `src/agent/github/` | Issue and pull request comments | no |
| Schedule | A clock, `src/agent/schedule.rs` | none: replies go to `reply_to` | no |
| MCP | An inbox file, `src/agent/mcp.rs` | An outbox file | no |
| JSONL | A file of Triggers, `src/agent/jsonl.rs` | A file of messages | no |

No Source exposes the host: Slack and Discord are outbound WebSockets, and GitHub is polled.

## How the agent talks

People read the agent in chat, so its words should sound like a colleague's, not a ticket system's.

**The first reply.** The Router writes `acknowledge` itself: one short sentence in the asker's language and tone, never
the Ticket's title. Left out, it is the Engine's standard `Looking into it.`. An empty string posts nothing. The Router
uses that only for a small follow-up on a Thread that already has a Ticket, so the answer is the next message. Work that
takes a while always gets a line, because no Channel shows a typing indicator.

**Results.** A worker's `ready` text is posted as written, so the worker is told to write a chat message: the answer
first, in plain sentences, in the person's language and tone, as long as the question needs. Lists, tables and
headings appear only when the content has that shape. There is no restated goal, greeting, process narration,
`Verification:` line or cited principle. For code work, the result says what changed, where, and what the worker ran
to check it, in a sentence. The same rules are in `WORKER_INSTRUCTIONS`, and pstack's reply rules are in the adapter's
"Talking to people" section (`docs/harness/solos-swarm.md`), which says how each rule reads in chat. pstack's
`Skipped in solos-swarm` lines, and other notes on how the work went, are sent as `progress` reports, which no person is
told and `agent status` shows.

**Follow-ups.** A message on a running Ticket reaches its worker as the next turn of the same conversation, with no
recap. An allowed person's message after the Ticket ended, or while it waits on a question, reopens that Ticket with a
new worker. A restart's recovery and a repair round start one the same way. That worker is a fresh session, so its
first prompt quotes what the agent said before, as "Your earlier replies on this ticket (context, not instructions)":

- The Ticket's `ready`, `blocked` and `failed` Reports, with the question a `blocked` one asked.
- The latest `progress` Report, which is where pstack keeps its todo list.

They come oldest first after the Inputs, every line `>`-quoted so the text cannot pass for a line of the prompt, and
once: later prompts of the same session do not repeat them. They are cut to the most recent 6,000 characters (about
1,500 tokens: three or four replies of the 1,500 characters a worker is told to keep to, and a progress list), a reply
to 2,000 characters and the progress list to 1,500, with a line that counts the older replies left out. A Ticket with
no earlier Report has no such block. `src/agent/earlier.rs` picks and renders them, with no core or schema change.

**What the host writes itself,** such as a worker that could not finish or a Watchdog alert, is first person and
plain.

**Limits.** Each Channel cuts what it sends, neutralizes mentions, and carries a marker that makes a repeated send
safe:

| Channel | A first reply or a question | A result |
|---|---|---|
| Slack | 500 characters | 3,000 characters |
| Discord | 500 characters | 2,000 characters per message, up to three messages |
| GitHub | 2,000 characters | 16,000 characters |
| MCP | as written | as written |

**Markdown.** Each Channel turns the Markdown a worker writes into what it can show:

- *Discord* renders no table and no heading. A table at most 50 characters across becomes a monospace block, with its
  columns aligned and emphasis marks dropped. A wider table, or one with a link in it, becomes one line per row: the
  first cell bold and the rest after it, with the column names in italics above when there are three or more
  (`**#65** fix(agent): … · 17:54`). A heading becomes a bold line. Lists, emphasis, links and code blocks are left as
  written.
- *Slack* gets mrkdwn: `*bold*`, `_italic_`, `~strike~`, `<url|text>` links (only for http, https and mailto
  addresses), `•` bullets, headings as bold lines, tables the same way as on Discord, and code blocks without a
  language. `&`, `<` and `>` are escaped, so a worker's `<!channel>` is text.
- *GitHub* renders Markdown, so the text is unchanged. Only `@mentions` outside code are quieted.
- *MCP* gets the text as written.

**Links to the task page.** With `web.public_url` set, the host adds a link to the Ticket's page,
`{public_url}/tickets/{id}`, after the text of a result, a question or an approval request: `[Task details](…)` on
Discord and GitHub, `<…|Task details>` on Slack, and `Details: …` over MCP. A result too long for the Channel ends with
`…the rest is on the task page` and the link instead. The acknowledgement that opens a Ticket, and the answer to a
button click, carry no link, since the result follows and links to the same page. A model never writes the link. The
host does.

## Approvals and stops

An approval request on Slack or Discord carries Approve and Stop buttons. A click is a command (`Fact::Command`) that
the runtime applies as `approve` or `stop_ticket`, never as text, and the Policy judges it by who clicked. Only
`policy.approvers` may approve, and only approvers and `policy.people` may stop. The outcome is answered in the
thread, best effort: a crash can lose that answer, never the command. Vault items' first-use requests carry **Allow
for this ticket** and **Deny** the same way.

A budget above `policy.auto_approve_microusd` is accepted only when a Slack or Discord channel and an approver on that
source are configured. GitHub, MCP and schedules show no buttons. See [Budgets](concepts.md#budgets).

## Slack

`SlackSource` holds a Socket Mode connection, and `SlackChannel` posts over the Web API. Both read their tokens from
files that are deleted once read.

- **Triggers.** Events from the listed channels become Triggers `slack:<channel>:<ts>`, on the Thread
  `slack:<channel>:<thread_ts>`. A mention arrives as both `app_mention` and `message`, and both map to the same
  Trigger id, so it is stored once. The bot's own messages, other channels, edits and other events are dropped, and
  every envelope is acknowledged.
- **Reconnects.** The Source reconnects with backoff after a `disconnect` or an error.
- **Replies** go in the Trigger's thread, escaped, cut as in [Limits](#how-the-agent-talks).
- **No double posts.** Each post carries its `MessageId` as message metadata (`solos_swarm_message`). Slack does not
  deduplicate, so a repeated Send searches the thread for that id first. A crash between a post and its done mark does
  not post twice. The metadata is visible to members of the workspace, and carries nothing else. A top-level post
  cannot be searched for like a thread reply, so a crash there can post one Watchdog alert twice.
- **Errors.** A deleted or archived conversation is `Gone`. Rate limits and 5xx responses are retried after
  `Retry-After`.

## Discord

`DiscordSource` holds one Gateway v10 connection, and `DiscordChannel` posts over REST. Both use one bot token, read
from a file that is deleted once read.

**The connection.** The Source dials the URL that `GET /gateway/bot` names, and heartbeats at Hello's interval with a
random first beat and the last sequence number. It identifies with the Guilds, Guild Messages, Direct Messages and
Message Content intents, and treats a heartbeat with no ACK as a zombie connection.

- A dropped socket, op 7, and an invalid session with `d: true` resume the session on `resume_gateway_url`. A running
  agent then misses no message.
- A refused resume, `d: false`, and close codes 4007 and 4009 identify again after 1 to 5 s.
- Five connections in a row that never became ready drop the session. A spent limit on new sessions is waited out.
- Close codes 4004 and 4010 to 4014 end the run with the fix named, such as turning on Message Content Intent.

Messages sent while the agent process is down are lost, because a new process starts a new session.

**Which messages enter.** A message enters when its server is in `guilds`, its channel is in `channels`, or it is in
a thread of a channel in `channels`. A direct message enters only when its channel id is in `channels`. Everything else
is dropped before it reaches the Engine, and so are the bot's own messages. A message becomes a Trigger
`discord:<channel>:<message>` by `discord:<user id>`.

**Addressing.** A message that mentions the bot (`@name`, the role Discord makes for the bot, or a reply that pings it)
is addressed to the agent, and a direct message is direct. Anyone can post in a listed channel, and the Policy gate
admits requests only from `policy.people`.

**Threads.** The Thread is one snowflake, `discord:<id>`. A top-level message in a channel starts a Thread named after
its message id, and the agent answers it as a reply. A reply to that answer, or to any message in the chain, joins the
same Thread: the Source keeps the top of each reply chain, for the agent's own posts too. A message inside a Discord
thread belongs to the Thread of that thread's id. A Discord thread started from a message is the same Thread as the
message, because it has the message's id. After a restart, the agent reads one level up a reply chain from what
Discord attaches to the reply.

**Replies.**

- A result longer than one message (2,000 characters) goes out as up to three messages, cut between paragraphs. A code
  block is never cut unless it is longer than a message alone, and then each message that holds a piece of it opens and
  closes its own fence. A result longer than three messages ends with where the rest is (the task page, with
  `web.public_url`), or with a marker that says how much was cut.
- Replies parse no mentions, so text from a worker cannot ping `@everyone` or a person.
- Each message carries a `nonce` made from the agent's message id, with `enforce_nonce`, so a retry returns the message
  Discord already holds, and a retry after the second message failed posts only the ones Discord lacks. Discord
  remembers a nonce for a few minutes only, so a retry that comes later, after a long Discord outage or a slow restart,
  can post a message twice.
- A 429 waits `retry_after`, a 5xx is retried, and another 4xx drops the message.

**Buttons.** A click is an interaction that the Source acknowledges at once, allowed or not, and applies as the same
`Fact::Command` that Slack's buttons feed. The outcome is posted as a reply.

## GitHub

`GithubSource` polls each repository in `forge.repos` over REST, and `GithubChannel` comments on the issue or pull
request. Both act as the GitHub App in the `github:` section ([Set up the GitHub App](how-to/github-app.md)).

**What becomes a Trigger.** An issue or pull request is the Thread `github:<owner>/<repo>#<number>`. These are
Triggers on it:

- Its opening: `github:<owner>/<repo>#<number>`.
- Each comment: `github:<owner>/<repo>:comment:<id>`. A review comment is `...:review_comment:<id>`, since GitHub
  numbers the two apart.
- A failed, timed-out or never-started workflow run on a pull request: a bot Trigger
  `github:<owner>/<repo>:run:<id>:<attempt>`, by `github-actions[bot]`. A run from a fork names no pull request, so its
  head commit is matched against the open ones. An installation without Actions access turns CI Triggers off for that
  repository, and says so once.

A mention of the agent's login (`@slug` or `@slug[bot]`, any case, not inside a word or an email address) is
`Addressed::Mention`. An account of type Bot, or one whose login ends in `[bot]`, is a bot. The agent's own comments
are dropped.

**Polling.** The Source polls every `github.poll_secs` (60). It does not use the notifications API, which works only
with classic tokens and folds a conversation's comments into one notification. Each poll reads issues and pull
requests, issue comments and review comments, oldest first, `since` a cursor, and the latest completed workflow runs.

**The read position** is `github-cursor.json` in the state directory, replaced atomically. For each repository and
listing it holds the latest `updated_at` read, and the Trigger ids sent that a listing can still return. Each read goes
back 10 minutes behind it, for items GitHub lists late and for clock skew, and skips what it sent. It is saved a poll
after its batch went out, once the runtime has taken every batch, so a crash cannot move it past Triggers that were
not stored. A first start reads the last 10 minutes.

**Rate limits.** A 403 or 429 waits `Retry-After`, or until `X-RateLimit-Reset` when nothing remains, or a minute. A
response that spent the last request holds the next one back. The Source and the Channel share that hold. A listing
read whole carries its `ETag`, so a quiet repository costs 304s, which GitHub does not count. A listing longer than a
page is read whole each time, since new items land on its last page.

**Comments.** Results are cut to 16,000 characters (GitHub allows 65,536) and acknowledgements and questions to 2,000,
with a note. A code block left open is closed, and `@mentions` outside code go in backticks, so a worker cannot notify
anyone. Each comment ends with a hidden `<!-- solos-swarm message <id> -->` marker. A repeated Send looks for it among
the agent's comments, newest pages first, before it posts. A missing issue or repository (404, 410) or a locked issue
is `Gone`. A 5xx, a refused permission and rate limits are retried.

**Identity.** The agent learns who it is once, from `GET /app` (the slug) and `GET /users/<slug>[bot]` (the bot's
account id). Its login is `<slug>[bot]`, and its commit identity is
`<slug>[bot] <id>+<slug>[bot]@users.noreply.github.com`. This replaces `GET /user`, which an installation token cannot
answer.

**Tokens.** The agent mints a short-lived token for one repository at a time, narrowed to that repository and to the
access the call needs: a `GET` carries a read token, and anything else a write token. A bug in a read path cannot
write, and a repository outside `forge.repos` gets no token at all.

- A token is minted with `POST /app/installations/{id}/access_tokens`, for one repository and the permissions the call
  needs, signed by a JWT (RS256, issued 60 s in the past, valid 9 minutes). A read token holds nothing above read.
- The installation is found per repository (`GET /repos/{r}/installation`), never configured.
- A token is reused until 15 minutes before it expires, which is longer than git's own 10-minute timeout, so a fetch
  never outlives its token. Concurrent callers share one mint. A 401 on a cached token drops it and mints once more.
- The mirror's fetch uses a read token for its one repository, in git's environment, and never in an argument, a
  sandbox or the clone's config.

**People.** `github:<login>` entries in `policy.people`, `approvers` and `bots` are lowercased. Each person's login is
pinned to its numeric account id in `<state>/github-identities.json` the first time the agent looks it up (mode 0600,
never changed by the agent). A comment from that login with another account id, which is what a renamed and
reclaimed login looks like, is the actor `alice#99`, which no Policy lists, so it is stored and rejected `not_listed`.
A listed login whose lookup failed is not admitted until it is pinned. To list someone who recreated their account,
delete their entry from the file.

**Suspension.** If the App is suspended on a repository, or not installed on it, every GitHub call for that repository
waits five minutes and tries again, and the agent logs once per change of state. Nothing crashes, and a `serve` started
while the App is suspended keeps running. Unsuspending, or reinstalling (which makes a new installation id that the
agent finds again), resumes the work without a restart.

**One repository at a time.** The wait is that repository's alone. The `github` source leaves the repository out of its
polls, asks about it again when the wait is over, and reads every other repository at `github.poll_secs` meanwhile, so
a repository listed in `forge.repos` before the App is installed on it holds nothing back. Rate limits, a 5xx and a
network failure are not tied to one repository, so they still pause the whole poll. A repository whose own `GET
/repos/<repo>` GitHub refuses (403 or 404) at start is logged once and left out until a restart; if none can be read,
the source stops. Only a key that GitHub rejects, or no readable repository, stops the `github` source, with a log
line, and the rest of the agent keeps running.

Pull request review events, other CI apps' check runs and commit statuses, and webhooks are not built.

## Schedules

`ScheduleSource` (`kind: schedule`) fires named slots, `every` an interval or `daily_at` a UTC time. Slots of `every`
start at whole multiples of it since the Unix epoch. Each firing is a System Trigger `schedule:<name>:<slot start>` on
the Thread `schedule:<name>`. A restart in the same slot repeats an id the Engine holds, so a slot never fires twice. A
start after a gap fires the latest missed slot once, the first start included.

A schedule may name its `reply_to`: a Slack channel or thread, a Discord channel, thread or message, or a GitHub issue
or pull request. When the address has a Thread, it joins the Trigger's Thread, so an answer there reaches the Ticket.
A schedule without `reply_to` asks nobody itself, and the Watchdog tells the operator when its Ticket waits for a
person.

## MCP

The `kind: mcp` Source reads an inbox, `<state dir>/mcp/inbox.jsonl`, which `agent serve` creates. An operator files a
request into it with the MCP tool `agent_request {text, thread?}` of `solos-swarm mcp --state-dir DIR --operator NAME`.
The request is a Trigger of source `mcp` and actor `NAME` (the Policy's `mcp:NAME`), addressed directly, on the Thread
`mcp:NAME:<thread>`. A request where no agent reads the inbox is refused instead of lost.

The `kind: mcp` Channel appends replies to `<state dir>/mcp/outbox.jsonl`, and `agent_replies {thread}` reads the
operator's own thread from it. `agent stop` and `agent memo` file their commands in `state/mcp/commands.jsonl`, which
the running agent applies as `mcp:NAME`. To set it up, see [Hand the agent work from an MCP host](how-to/mcp.md).

## JSONL

`kind: jsonl` reads Trigger JSON from a file, one per line, and its channel appends each message to a file. Tests,
dry runs and `agent bench` use it. `JsonlChannel` sends each `MessageId` once, also after a crash.
