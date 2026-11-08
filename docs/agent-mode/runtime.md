# How the agent runs work

This page explains what `solos-swarm agent serve` does with a request after a Source has stored it: how the Router
judges it, how a worker runs it, what stops a worker that goes wrong, and what watches over all of it. The words it
uses (Trigger, Thread, Eval, Ticket, Assignment) are defined in [Concepts](concepts.md#the-vocabulary). How each chat
source and channel behaves is in [Sources and channels](channels.md).

## The serving process

`agent serve` is one process. One task in it owns the Engine on the SQLite Store. That task stores the Sources'
Triggers, starts due Evals, and hands each due Effect to its adapter. It marks the Effect done, or failed with an
exponential backoff that starts at 1 s and doubles up to 5 minutes. It applies what the adapters report as Engine
commands: `decide`, `assignment_started` and `report`. While the Policy is paused, it holds every Effect.

With `--until-idle`, `agent serve` reads the Sources once, finishes what they started, prints a summary and exits. Tests
and dry runs use it.

## The Engine

The Engine (`crates/agent-core`) decides everything, does no I/O, and leaves external work as Effects:

- **Intake** is idempotent: a Trigger is stored once by its Source id, however often it arrives. A Trigger reaches
  every Thread it belongs to.
- **Evals.** A Thread has at most one Eval at a time. Triggers that arrive meanwhile join the next batch. Evals are
  batched by strength: weaker Triggers wait, unless a new Ticket adopts them. Triggers the gate rejected stay out of the
  Router's view.
- **Verdicts.** The Router accepts, ignores or stops; the host may decline. Only a Trigger of explicit strength can stop
  work. A decline means no upstream here may run the repository's workers: the Engine records a declined Decision and,
  on an explicit request, sends the host's sentence to the request's channel once. The Router's tool result says the
  person was told, or that nobody was. The accept's upstream and harness come from the repository's policy, never from
  the Router's words.
- **Budgets.** A budget above the approval threshold waits for an approver, and is checked at every launch against the
  current threshold.
- **Work.** Assignments, Reports, resumes, handbacks and retries are Engine commands. A handed-back Trigger is judged
  once, on its primary Thread, however many Tickets hand it back.
- **Policy changes.** The Policy's version covers who may ask, which bots count, the accept mode, the rules and the
  approval threshold. Pausing, approvers and timeouts do not change it, so they never discard a running Eval.
- **Pause.** A pause stops new Evals, and the effect runner holds Effects. The core still records Triggers, resumes
  and wakes.

A Ticket whose Trigger names no reply channel, such as a schedule without `reply_to`, asks nobody itself when it needs a
person. The Watchdog tells the operator when such a Ticket waits.

## The Router

With `router: {kind: pi}`, the Router is a long-lived Pi session (`src/agent/pi_router.rs`). It runs keyless in bwrap,
and its model calls go through the agent broker against `budgets.router_daily_microusd`.

**Batching.** Each Candidate becomes a compact card. The cards queued between turns go into prompts of at most
`router.max_batch` cards (6 by default), oldest first. A session that dies restarts, and the core's Eval expiry brings
back what it left undecided.

**A reply cut off at the output limit.** If a reply is cut off while it asks for decisions, the session ends at once.
Pi would otherwise ask again with the same context until the Router's budget ran out. The cut-off cards go back to the
front of the queue, in prompts of half as many cards, and the size grows back by one card after each full prompt that
fits. A prompt of one card that is cut off is left to its Eval, which expires as usual.

**A hung call.** A Router model call is dropped after half of `policy.eval_timeout_ms`, and charged at its hold, since
the provider may still bill it. A hung call frees the session while its Eval can still be asked again. Workers' calls
keep the transport's own limit.

**Stale ids.** A decision on an Eval that is no longer running is refused, and the refusal lists the Evals open now,
by Thread. Each prompt says that only its own ids can be decided.

**Tools.** The Router has two tools and no `run_code` or `fetch_url`:

- `router_decide`, one flat object: the Eval, the decision, the reason, and the accept or stop fields. The Engine's
  answer, or its refusal, goes back to the model. An accept names a Plan (a session or a swarm), a budget, and for a
  repository Ticket a `repo` and a `deliverable`. A session accept may name an `environment` and a swarm accept a
  `goal_template`.
- `router_read`: a Trigger, a Thread or a Ticket with its neighbours' ids, or memos (`{kind: memo, id: global}` or a
  memo's id).

The Pi extension picks the set with `SWARM_TOOLSET=router`, and takes the Router's system prompt from the broker.

**Budgets in the prompt.** An accept above `policy.auto_approve_microusd` stands only on a Ticket whose origin can show
an Approve button: a Slack or Discord channel, with an approver on that source. The Ticket then waits for the click. A
Ticket from GitHub, MCP or a schedule has no button to wait on, so its accept may name at most the threshold, even where
Slack or Discord takes approvals for their own requests. The Router's prompt says so, and `router_decide` refuses more
and names the cap. [Budgets](concepts.md#budgets) has the rest of the budget rules.

**Swarms in the prompt.** The Router's prompt says that a session fits most work, a bug fix above all, and that a
swarm fits only work that splits into many independent parts: wide research, many checks, a tournament. It lists the
templates, and says that a swarm budget above the approval threshold waits for a person's approval. `router_decide`
refuses a swarm where none runs, and a template that does not exist.

## Workers

With `executor: {kind: pi}`, `PiExecutor` (`src/agent/pi_executor.rs`) runs each Assignment of a `Plan::Session` Ticket
as one keyless worker session in bwrap. A `Plan::Swarm` Assignment goes through the same dispatch point to the
[swarm executor](#swarm-tickets).

**The Ticket's directory.** A worker has its Ticket's directory, `<state>/tickets/<ticket>/`, whose `work/` is
`run_code`'s `/work`. An optional `executor.prepare` command runs there once per Ticket. At most `max_workers` sessions
live at once, and a launch past it waits for a slot.

**The upstream check.** Before the clone, and whatever the environment, the executor asks the repository's model policy
whether the Ticket's upstream may run its workers now. A Ticket whose upstream is no longer admitted — because the
policy changed, or because it was accepted before this build with no recorded upstream and the repository does not
admit `default` — fails its Assignment with a Report that names the policy, and nothing of the repository is read. The
Plan's harness is the one the accepted upstream named, so a repository on a `claude-code` upstream runs Claude Code
whatever `executor.harness` is.

**The account.** The agent broker admits the worker as a `Worker` principal on the account `ticket:<id>`, which every
Assignment of the Ticket shares. So a retry spends from the same budget, a call past the budget is refused, and the
Router's account is untouched.

**Tools.** A worker's tools depend on what the host has, and the Pi extension selects them with
`SWARM_TOOLSET=worker`:

| Tool | What it does | Present when |
|---|---|---|
| `worker_report` | Reports a status, a text and an optional question. The spend comes from the ledger. | always |
| `worker_handback` | Sends a follow-up that is not this Ticket's to the Router, once. | always |
| `run_code` | Runs a bash command in the Ticket's `/work`, in a sandbox with no network. | `executor.run_code` is set |
| `job_start`, `job_wait`, `job_stop` | Run a command in the background. See [Background jobs](#background-jobs). | `executor.run_code` is set |
| `fetch_url` | Fetches a public URL, optionally with a vault key. See [What fetch_url can reach](#what-fetch_url-can-reach). | always |
| `vault_list` | Lists the vault's keys and logins, never their values. | a vault is configured |
| `browser_open`, `browser_act`, `browser_read` | Drive a headless browser. See [The browser](#the-browser). | a browser is configured |
| `browser_login` | Signs in with a vault login. | a browser and a vault are configured |
| `read` | Reads files: `/pstack`, a repository Ticket's `/work` and `/host`, and `/memos`. | pstack, a repository or memo proposals |
| `write_file` | Creates or replaces one file in the Ticket's `/work`, making the directories above it. See [Files](#files). | `executor.run_code` is set and the Ticket runs on this host |
| `edit_file` | Replaces the one place a text is in a file in `/work`. See [Files](#files). | as `write_file` |
| `memo_propose` | Proposes a memo. See [Memory](#memory). | `memory.proposals` |

**pstack.** With a `pstack:` section, every worker's first prompt enters the `pstack.entry` skill (`poteto-mode` by
default), its system prompt carries the solos-swarm adapter (`docs/harness/solos-swarm.md`), and it may `read` the
vendored tree at `/pstack`. The vendored tree is pinned by its `MANIFEST`. The design is in
[pstack/rationale.md](pstack/rationale.md).

### Files

`write_file {path, content}` and `edit_file {path, old, new}` change files in the Ticket's `/work` without a shell. A
worker that wrote files through `run_code` quoted them into heredocs and `python3` one-liners, and looped on the
quoting: the Ticket #106 describes ran the same heredoc and `python3` edits of one file for about five minutes and
failed with no result. The worker's system prompt tells it to write and change files with these two tools.

- A path is relative to `/work` or absolute under it: `src/main.rs` or `/work/src/main.rs`.
- `write_file` makes the directories above the file, and replaces a regular file there.
- `edit_file` replaces `old` with `new` and answers the line where `new` starts. It refuses an `old` that is not in the
  file, or is in it more than once, and leaves the file as it was.
- A file is at most 1 MiB, the limit of a file in `run_code`'s `files`. A replaced file keeps its mode.

**Links.** The broker writes as the host, outside the sandbox, and the worker can put a link anywhere in its `/work`.
So `src/beneath.rs` never resolves a path by name. It opens `/work` once, then opens each directory on the way relative
to the one before with `O_NOFOLLOW`, and makes a missing one with `mkdirat`. It writes a fresh file with
`O_CREAT | O_EXCL` and moves it over the old one with `renameat`, which does not follow a link either. A link on the
way or at the end, a `..` part, an absolute path outside `/work` and anything that is not a regular file are refused,
and nothing is written outside `/work`. `run_code` writes its `files` through the same code.

### What a worker is told

The first prompt of an Assignment holds the title, the scope, the matched rules, every Input so far, the memos for its
scope, and how to report. A relaunched Assignment also gets the Ticket's earlier replies (see
[Follow-ups](channels.md#how-the-agent-talks)). With `recovery`, the prompt adds a note to check `/work` before
repeating any outside action. After the first prompt, the worker gets each later batch of Inputs between turns. A batch that holds only
context from someone the Policy does not allow (a stranger's or a bot's comment, labelled `context only`) starts no
turn, since a turn can cost up to 250 model calls. It waits and comes in the prompt of the next turn that an allowed
person's message or a Note starts.

**Reports.** `progress` and `waiting` are quiet. `ready` sends the result. A question, `blocked` or `failed` asks a
person. A turn that ends with no Report, or with `progress` only, counts as `waiting`, except in the two cases below. A
session that dies reports `failed` and ends its Assignment, so the next reply starts a new one.

### When a turn ends because a model call failed

A turn can end because its last model call failed, with no Report and no `progress` after it. That turn is not
`waiting`. The broker keeps why the call failed on the worker's seat (`ModelFailure`), and the host reports in the
worker's place:

- A budget with no room for another call reports `blocked`.
- Any other failure (the provider, its call limit, the model limit) reports `failed`.

The Engine sends the text to the Ticket's Channels like any such Report, once. It says in plain words what stopped the
work (`I stopped before finishing: this request's budget ($0.50) cannot cover another model call ...`, or
`the model call failed (...)`) and how to go on. The Report is recorded with the Ticket, which now waits for a person,
so a restart ends the Assignment and starts nothing. A reply on the thread goes to the live session after a failure,
and starts a new Assignment after a restart. A budget is fixed at accept time, so the budget text asks for a new
request that names a bigger one.

### When a worker answers in plain text

A turn can end with no Report (or `progress` only) and no failed call, while the worker's last message was its answer
written as plain text. That turn is not `waiting` either, because the person would hear nothing. The harness reports the
text of Pi's last message when Pi stopped by itself with no tool call (`SessionEvent::Said`, from the `message_end`
frame). It reports the text blocks only, never the reasoning, and never a failed, aborted or cut-off reply. At the
turn's end, the host files the Report in the worker's place (`src/agent/plain_answer.rs`):

- A text that asks the person something (up to 500 characters, ending in `?`) is `blocked` with that question. The
  Ticket waits for the person, and their reply goes to the live session.
- Any other text is `ready`, cut to the 8,000 characters a `worker_report` keeps. The Channels cut it further, as they
  do any result.

The Report is recorded with the Ticket like the failed-call one. The person hears it once, a restart tells nothing
again, and a reply on the thread goes on as it does after any Report.

Nothing is filed, and the turn stays `waiting`, when:

- The turn reported anything but `progress` (the report is the answer).
- A job still runs (the turn reports `progress`, as before).
- Input waits for the next prompt.
- The worker handed an Input back that turn.
- The text is empty.
- The text is only reasoning, or a tool call the model wrote out as text (`{"name": ..., "arguments": ...}`,
  `<tool_call>`).

The host does not nudge the worker to report. A nudge would spend another model call on a budget that may be gone,
add a second prompt path to the turn, and still need this fallback when the nudge ends in text too. Claude Code
sessions do not report their last text yet, so they keep the old behavior.

### The loop guard

The broker stops a worker that loops or makes no progress (`src/agent/broker/repeat.rs`), per seat and per turn. It
works in steps, and each step catches what the one before cannot: a circle, refusals that are ignored, calls that
bring nothing new, a failure that comes back in a call that is never byte-identical, and finally the turn's ceilings.

**Circles.** A circle is one call, or up to ten calls in turn, repeated three times with byte-identical arguments:
`A A A`, `A B A B A B`, or five `sed -n` slices of one file gone round three times. The call that closes the third round
is refused with a reason (HTTP 429, which the model reads as the tool's error) that asks it to change its approach or
report `ready`, `blocked` or `failed`. The refused call never runs, and while the circle goes on, every call of it stays
refused. The refusal quotes what the call answered the first time, on one line: a command's exit code and the first
lines of its stdout and stderr, or else the start of the result. So the model sees it already has the answer.

A call that comes back among others that differ, such as a test run between edits, is no circle, and a new turn starts
the count again. A tool whose registry entry says `repeatable` is never refused, and ends any circle it falls into:
`job_wait`, `browser_act` and `browser_read` (what they answer moves between calls), `worker_report` and
`worker_handback` (which must always get through), and the Router's tools. `repeatable` is not part of the tool
schema, so `extensions/agent-tools.json` does not carry it.

**Refusals that are ignored.** A refusal alone does not stop a model that ignores it. One Ticket sent the same refused
`run_code` call 767 times, a model call each, for about $2.25. So the guard counts the calls it refuses, and stops the
turn after 3 refused calls in a row with no call that went through between them, or after 10 refused calls in one turn
however far apart. `A A A A A` is stopped at the fifth call, `A B A B A B A B` at the eighth, and a five-call circle at
the seventeenth.

**No progress.** Some loops are no circle: one that is eleven calls round or more, or whose calls differ by a counter.
So the guard also stops a turn on what it brings:

- 30 calls in a row of tools that may not repeat, each a call the worker already made this turn, and each answering
  what it had already been answered (a command's exit code and output, or a response without its headers).
- A ceiling of 200 such tool calls in a turn.
- A ceiling of 250 model calls in a turn, compactions included. This is the only bound on a loop made of `repeatable`
  tools.

A call of a `repeatable` tool counts toward neither of the first two, so waiting on a job with `job_wait` is not
penalized. A call that is the same but answers something new, such as reading the log a build is writing, is progress.
The thresholds come from the transcripts of the Tickets that looped and of the ones that did not. A normal turn has at
most 2 or 3 such calls in a row, compactions included, and the longest turn that ended well made 128 tool calls,
while the loops made 767 and 1,107.

**Failures that come back.** One Ticket (#94) sent eight heredoc edits of one file that failed alike, between reads of
it, and made about ninety calls before the steps above caught it: each edit differed in whitespace or in its body, so
it was a call the worker had not made, and a failed command was never counted. So the guard also stops a turn on 3
calls of one shape that failed the same way, with no call of that shape that succeeded and no `repeatable` call (a
wait) between. A shape is the tool's name and the programs and files it names; whitespace, quotes, redirections and a
heredoc's body do not make it new. A command that exits 1 with nothing printed said "no" (`grep` found nothing, `test`
was false), which is an answer, not a failure. Digits are left out of a failure's text, so a line number, a count or a
duration that moves does not make it new. The turn stops on the third such call, whether it ran or a circle refused it
before it ran.

**The stop.** From then on, the broker answers the seat's tool calls with the stop, and refuses its next model call
before any hold or upstream call, so the stop costs nothing. That ends a Pi turn exactly as a failed provider call does.
The seat keeps `ModelFailure::Stuck` with what the worker kept doing, read from the arguments of its latest calls: the
tool, its programs and the file it names. The host reports `failed` in the worker's place, with one question that
would get it going again. A failure that came back says:

```text
I tried 3 times to run `python3` on lib.sh, and it failed the same way each time, so I stopped. Is there another way
you want me to handle lib.sh, or should I stop here?
```

A circle or a ceiling asks its own question, such as `I kept re-reading the same parts of meteora-dlmm-allocate-encode.js
without making progress, so I stopped. What should I be looking for in meteora-dlmm-allocate-encode.js?` or `I made 200
tool calls without finishing, so I stopped. Should I keep going from where I am?` The Report is recorded with the
Ticket, and its text is also the Ticket's question, so the Ticket waits at `Stage::Answer` rather than
`Stage::Failed`: the person hears it once, a restart sends nothing, and a reply on the thread answers it. The reply
starts a new turn, which starts every count again. A Claude Code session has no turn to end on a refused model call:
its session ends on the error, and the Assignment fails with that reason, as for any session that dies.

### A reminder to the person who was asked

A Ticket that waits for a person can wait a long time: #94's Ticket sat 19 hours after a stuck worker asked. With
`remind_after` (4h unless set, `null` tells nobody), the runtime looks at the open Tickets every quarter of that,
between 250 ms and 1 min, and sends one reminder to the Ticket's own thread (`Engine::remind`). A wait is an
approval, a question, or a blocked or failed Report. The reminder repeats one line of what the wait is for, since the
message that began it may be far up the thread:

- an approval: `I'm still waiting for a person to approve this. <what the Ticket is for>`, with the Approve buttons
  offered again;
- an answer: `I'm still waiting for your answer. I asked: <the question>`;
- a failure: `I'm still waiting on a person after a failure. I said: <the Report>. Reply here and I will try again.`;
- a block: `I'm still waiting on a person. I said: <the Report>. Reply here and I will pick the work back up.`

The Ticket records the start of the wait it was reminded of (`Ticket.reminded_ms`, schema 10) and a `Reminded` event
goes on the chain, so each wait is told once, across restarts. A wait that begins after an allowed person writes or
the worker asks again is a new wait, and is told again. A wait older than seven days is not reminded of: the person
has had a week, and a late ping is noise. Nothing is reminded while the agent is paused, since it would act on no
answer; the wait is looked at again once the pause lifts. To set the threshold, see
[`remind_after`](configuration.md#remind_after).

### Sampling

Workers sample at `executor.temperature`, 0.6 unless the config says otherwise. The Router and swarm mode keep the
shared upstream body's 0, which swarm mode pins byte for byte. A model that decodes greedily is the likelier to go
round in circles, and DeepSeek's V4 model card recommends 1.0 (its API default is 1, and an older table says 0.0 for
code). 0.6 is the low end of what agentic runs use: enough randomness to leave a loop, and low enough for a command to
come out as meant. Thinking is off for workers, so the temperature takes effect. No frequency or presence penalty is
sent, because DeepSeek's API documents `frequency_penalty` and `presence_penalty` as no longer supported.

### After a restart

At start, an Assignment that the last run left running, or pending with its Launch performed, ends with "host
restarted", oldest Ticket first. One that was working starts again with `recovery`, through `Engine::restart`: the end
and the new launch are one transaction. One that waited for input, an answer or a person starts again with the Input
that wakes it.

## Background jobs

A worker with `run_code` can run a long command in the background and take new Inputs while it runs
(`src/agent/jobs.rs`).

- `job_start {code, timeout_s?}` starts one bash command in the Ticket's `/work`, under `run_code`'s sandbox and limits
  (`CodeExecutor::start_job_in`, with CPU time still capped at 300 s), and returns `{job, status}` at once.
- `job_wait {job, wait_s?}` waits up to `wait_s` (default 30, at most 120) and returns the job's end, or its status and
  what it printed since the last look. It returns early, once per batch, when new input waits for the next turn.
- `job_stop {job}` kills the job and returns its end with the output not yet read.

Each output stream keeps at most `job_output_bytes` unread, and each byte reaches the worker once.

A `run_code` still running after `background_after_secs` returns `{job, status: "running", note}` when the Ticket has
room for a job, and otherwise stays in the foreground. One that ends sooner returns the same receipt as before. Swarm
runs keep the synchronous `CodeExecutor::run`, unchanged.

**Ownership.** Jobs belong to their Assignment, in a table on its `WorkerSeat`. A Router token is refused (403), and
another Ticket's worker finds no such job (404). When the worker ends (a stop, the Ticket's end, a dead session, a
shutdown or a revoked token), its jobs are killed and none is reported. A kill freezes bwrap, kills the namespace it
cloned, then kills bwrap, since a plain kill in bwrap's first milliseconds leaves the namespace running. Jobs do not
survive a restart. The recovery prompt already says to check `/work`.

**Ends.** A job's end reaches the worker once. One lock covers the turn and the jobs, and whoever takes an end removes
the job. In a turn, only the worker's `job_wait` or `job_stop` takes it. At the turn's end, every end the turn left is
taken, and between turns, each end is taken as it comes. Those last two become a Note on the Ticket (`Fact::Note`,
applied as `Engine::note`), which the Engine delivers as a new Input, so the worker reads it on its next turn. A turn
that ends quietly with a job running reports `progress`, not `waiting`, so the Ticket stays at work for `--until-idle`
and the Watchdog's stall clock starts over.

Jobs hold `run_code` slots (`concurrency`) like any sandbox. The limits are under
[`executor.run_code`](configuration.md#executorrun_code).

## What fetch_url can reach

`fetch_url` (`broker::fetch_guarded`) uses swarm mode's limits, and reaches the public internet only. A page, an issue
or a repository can tell a worker to fetch any URL, and this host may run other services on loopback and private
addresses. So every request and every redirect hop, keyed or not, is refused when its host resolves to:

- A loopback address.
- A private address (RFC 1918).
- A link-local address, including the cloud metadata address `169.254.169.254`.
- A shared address (`100.64.0.0/10`, which Tailscale uses).
- A unique-local IPv6 address (`fc00::/7`).
- An unspecified or multicast address.
- An IPv6 address that wraps one of those.

A name is resolved once per hop, every address it has must be public, and the connection is pinned to those addresses.
So a name that answers differently the second time cannot get past the check.

The worker reads `fetch_url refused: <host> resolves to a private address ...` as a 403 with
`reason: private_address`, and nothing was sent. The agent's log gets a line that names the address, which the model is
not told:

```text
agent: fetch_url of the worker of <assignment> on <ticket> was refused (private_address): <host> at <address>, hop <n>
```

For a host that must reach a service on its own network, `executor.fetch.allow_private_networks: true` turns the check
off for every worker's `fetch_url`. The browser's own switch is `browser.allow_private_networks`.

`fetch_url` goes through no proxy, because a proxy would open the connection itself and the check could not see where
it goes. So `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY` do not apply to it.

## Swarm Tickets

A `Plan::Swarm` Ticket runs one governed swarm per Assignment (`src/agent/swarm_executor.rs`). `PiExecutor`'s dispatch
point sends it to `SwarmExecutor`, which runs it as one `swarm::run_goal_until` on the `PiHost` that serves the
workers: `PiHarness`, the provider and the credential. At most `executor.swarm.max_runs` swarms (default 1) run at once,
apart from `max_workers`, since one swarm is many sessions. A launch past it waits for a slot.

**The Goal.** The Goal comes from the template the Plan names, or the default, rendered with the Ticket's id, title,
scope, agent count and remaining budget (`src/agent/goal_template.rs`, minijinja, every placeholder an escaped YAML
literal). It must pass `Goal::validate`, and its budget and agent count must be the Ticket's. Config load renders every
template for one and two agents, so a bad template stops `agent serve` before it starts. A Goal that still fails at
launch is a `failed` Report, never a crash. To write one, see [Write a Goal template](how-to/goal-templates.md).

**The budget.** The Goal's budget is the Mandate budget minus what the Ticket's account (`ticket:<id>`) committed, so a
Ticket never spends past its Mandate across its Assignments. The swarm's own broker holds and settles each call
against the Goal. When the run ends, its spend, with any hold still open counted at its hold, is charged on the
Ticket's account (`SqliteStore::charge`, once per `swarm:<run dir>`) before the Report. So the next Assignment and
every worker call see it. A stopped run is charged too.

**The run directory.** Each run has its own directory, `<state>/tickets/<ticket>/swarm/<assignment>` (`-2`, `-3` when a
launch repeats), with the swarm's usual files: `config.yaml` (the rendered Goal), `manifest.json`, `events.jsonl`,
`state.json`, `artifacts/final.json`, `cleanup.json`, `runtime-report.json` and the transcripts. It stays as evidence.

**Reports.** Completed is a `ready` Report that names the filed findings, the spend and the artifact's path. Stopped is
`failed` with the reason. Drained is `waiting`. A failed or drained swarm also ends its Assignment, so the next message
from a person starts a new run. While it runs, a swarm sends a quiet `progress` Report with what it has filed and spent,
at half the Watchdog's `stalled` threshold and at least every 5 minutes, so the Watchdog never takes a long swarm for a
stalled Assignment.

**Stop and pause.** A Stop Effect cancels a running swarm through the cancel input of `run_goal_until`
(`swarm::Cancel::on`). That takes the operator-cancel path (partial findings, the artifact of what was filed, every
session stopped, `cleanup.json`) and waits until it is done. `run_goal` keeps SIGTERM and Ctrl-C, so
`solos-swarm run` is unchanged. Under `agent serve`, the agent owns the signals and cancels its swarms as it shuts down.
The agent's pause gate holds running swarms through a `PAUSE` file in their run directories.

**After a restart.** Recovery relaunches an interrupted swarm Assignment with `recovery`, in a new run directory. The old
directory stays as it was, and its spend is charged from its `state.json` before the new Goal's budget is read. There
is no `run --resume`.

A running swarm takes no new Input. A follow-up on its Thread waits for the Ticket's next Assignment.

## Claude Code sessions

With `executor.harness: claude-code`, or a Plan that names `claude-code`, a session Ticket runs on
`ClaudeCodeHarness` (`src/harness/claude_code.rs`). `PiExecutor<H, C>` matches a `HarnessKind` where a session starts,
and each arm runs the same generic turn loop on its harness's own `Session` type, so there is no boxing. To set it up,
see [Run workers on Claude Code](how-to/claude-code.md).

Each Claude Code worker runs in bwrap with:

- **No network.** The sandbox has its own network namespace. There, `solos-swarm bridge` (a hidden command) listens on
  `127.0.0.1:41300`, pipes each connection to the broker socket bound at `/broker.sock`, runs `claude` on its own stdio,
  and exits with it.
- **No provider key.** `ANTHROPIC_BASE_URL=http://127.0.0.1:41300`, `ANTHROPIC_API_KEY` set to the session's bearer
  token, and `ANTHROPIC_DEFAULT_HAIKU_MODEL` set to the session's model, so background calls pass the contract. Also
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`, `DISABLE_AUTOUPDATER`, `DISABLE_TELEMETRY` and `DISABLE_ERROR_REPORTING`.
- **DeepSeek's settings on its own wire.** With `executor.wire: anthropic`, the session's model with `[1m]` is also
  `ANTHROPIC_MODEL`, `ANTHROPIC_DEFAULT_OPUS_MODEL` and `ANTHROPIC_DEFAULT_SONNET_MODEL`; `CLAUDE_CODE_SUBAGENT_MODEL`
  is the plain model; `CLAUDE_CODE_EFFORT_LEVEL=max`; and `CLAUDE_CODE_AUTO_COMPACT_WINDOW` is DeepSeek's 786,432
  tokens, or less where `max_input_bytes` is smaller. The broker forwards the calls to DeepSeek's Messages endpoint
  rather than translating them. See [executor.wire](configuration.md#executorwire).
- **A read-only view.** A tmpfs home and `/tmp`. The Claude Code install, the solos-swarm binary, the MCP config and
  the token file are read-only. The Ticket's `work` directory is read-only as `/work`, and files change through
  `run_code`, `write_file` and `edit_file`.
- **Only the broker's tools.** The command line is
  `claude --print --input-format stream-json --output-format stream-json --verbose --bare --model <model> --tools ""
  --strict-mcp-config --mcp-config /runtime/mcp.json --permission-mode dontAsk --allowedTools mcp__swarm__<each grant>
  --setting-sources user --disable-slash-commands --no-session-persistence --system-prompt <the worker prompt>`. The
  MCP config runs `solos-swarm mcp --socket /broker.sock --token-file /runtime/token`, so the session's tools are
  exactly the worker's broker tools, and the system prompt is the one the broker serves at `/session`.

The `result` frame ends a turn. An error `result` is `Rejected` with its text, and so is a session whose `swarm` MCP
server did not connect. A prompt is one stream-json user message. Each session writes `<assignment>.claude.jsonl` (its
stream-json output) and `<assignment>.stderr` in the Ticket's directory. Its token and MCP config files there are
removed with it.

**What it calls.** In a session, Claude Code 2.1.231 calls `HEAD /api/hello`, a preconnect whose answer it ignores (the
broker answers 200), and `POST /v1/messages?beta=true` with `stream: true`, once per model turn. It called no
`count_tokens`. The [Messages facade](architecture.md#the-anthropic-messages-facade) answers it.

**Limits.**

- A pause answers 503 with `Retry-After` (30 s for `control.json`, 60 s for `PAUSE`), which Claude Code waits out up to
  its retry count (10). A longer pause ends the turn with an error, the worker fails, and the Ticket asks a person. A
  Pi worker waits the pause out.
- Claude Code reports any 403, such as the Ticket's budget being reached, as "Failed to authenticate. API Error: 403"
  followed by the broker's reason. The worker fails with that text.
- On the chat wire, images, documents and server tools are not carried, and `tool_choice` and sampling settings are
  not forwarded. On the Anthropic wire, images, documents, `tool_choice` and sampling settings are carried, but a
  server tool other than the web search the broker adds itself is still refused.
- The Router stays a Pi session, and `router_decide` cannot name a harness yet.

## Remote nodes

A node runs the commands of the Tickets bound to it (`src/remote/`). The agent keeps every session. To connect one,
see [Run commands on your own machine](how-to/nodes.md).

**Where a command runs.** The `ExecutionEnvironment` port (`src/agent/environment.rs`) has `run_code(work, args)` and
`job(work, code, timeout, bounds)`, each of which returns a `Started` that reads, waits and stops the same wherever it
runs. `ready()` says whether the environment can take a command now. `Local` is the host's `CodeExecutor`, and each
configured node is another environment. `jobs.rs` runs on the port, and the broker resolves a worker's environment on
each call. `Plan::Session` has an optional `environment`, which `router_decide` may name on a session accept (refused
for a swarm, and for a name that is neither `local` nor a configured node). `executor.environment` is the default for a
Plan that names none.

**The node's side.** `solos-swarm node serve --config node.yaml` (`src/remote/daemon.rs`) dials out, so a machine
behind NAT works, and dials again with a backoff from 1 s to 60 s. It runs `run`, `job_start`, `job_wait` and
`job_stop` in its own bwrap sandbox, through the same `CodeExecutor` and sandbox builder, with the mounts of its own
config, and answers with `run_code`'s receipts. It holds no session state: a job lives on one connection, and a dropped
connection stops every job on it. Each Ticket's `/work` is a directory under its `work_root`, kept between commands.

**The agent's side.** `agent serve` starts a listener (`src/remote/listener.rs`) when agent.yaml has `nodes`. A
connection is admitted only with a certificate pinned in `nodes.allow`, and only under the name that certificate is
pinned for. The live-node table keeps one connection per node, and a newer one replaces it. A command starts as a job
on the node, and a task on the agent relays its output (polled every 2 s) and its end into the worker's `Started`. A
call waits at most 30 s for its answer. The agent pings every 15 s, and 45 s of silence is a lost node: every call
waiting on it fails at once.

**Offline.** A worker's `run_code` or job call on a Ticket whose node is offline is a 503 tool error that says so. A
Launch for such a Ticket fails with retry, so the effect runner's backoff (1 s doubling to 5 minutes) holds it until
the node is back, and the Watchdog's failing-Effect signal tells the operator after 3 attempts. The first retry after
the node reconnects launches it. The prepare command runs only for Tickets on this host, since a node's `/work` is its
own.

**Both harnesses.** A worker's grants and its `GET /tools` come from `AgentBroker::worker_tools(environment)`, so a
Claude Code worker on a Ticket bound to a node lists and calls `run_code` and the job tools through `solos-swarm mcp`
as a Pi worker does, and they run on that node. Its read-only `/work` is then this host's empty Ticket directory, since
the node's is its own. For the same reason, a worker on a node has no `write_file` or `edit_file`, and changes files
with `run_code` there.

**Bounds.** The connection is TLS 1.3 (ALPN `solos-node/1`) with each side's certificate pinned. Frames are a 4-byte
length and JSON, at most 16 MiB. A larger frame ends the connection before it is read, and a request too large for one
frame is refused before it is sent. Output is bounded on the node as it is locally, and again on the agent.

**Secrets.** `secrets:` in node.yaml are read from files on the node. Variables reach the sandbox through the
environment that bwrap inherits, never its argv. Files are bound read-only at `/secrets/<name>`. Every exact copy of a
value is replaced by `[redacted <name>]` before output leaves the node, also when a value is printed across two polls.
The agent never sees a value.

## The browser

A worker's browser is a headless Lightpanda on this host, one process per Assignment that browses, in its own bwrap
sandbox with private networks blocked, frames off and telemetry off (`src/browse/`). To set it up, see
[Give workers a browser](how-to/browser.md).

`browser.max_sessions` (2) bounds how many browsers run at once, and a call past it is refused rather than queued.
`idle_secs` (1800) closes a browser that has had no call. The steps' blobs go where `trail.path` says, at most
`trail.max_bytes` (8 GiB) in all and `trail.ticket_max_bytes` (256 MiB) per Ticket. Past either cap the next step is
refused, and the refusal is itself a signed step.

**Each step.** The core writes each step to the chain as `TrailAsked` before the browser acts, and as `TrailDone` after,
with the step the browser signed: the action, the page's URL, status and title, which login fields it filled, and the
digests of the page's snapshot and of the exact View the model got. The snapshot, the View and any typed text are kept
by digest in the trail store, so the chain holds no page text. The model gets the View only after its signed step is on
the chain. An answer that does not verify under the key pinned at start is recorded as `lost`, and is not shown.

**Order and end.** The steps of one Assignment run one at a time, even when its tool calls arrive at once. The browser
closes with a signed step when its Assignment ends, after `idle_secs`, or when the agent stops. A crash leaves the step
that was running asked and unanswered, which is what happened, and the restart starts a new browser. Cookies live only
in the browser's memory, so after a retry or a restart the worker signs in again, with no new approval.

**Logins.** The browser fills a login only into a password input of the page's own document, on a page whose origin is
one of the login's sites, in a form that posts to one of them. The model never sees the password. A password field
shows `"secret": true`, and every vault value is scrubbed from what comes back.

### What a trail check proves

`solos-swarm agent trail` checks a trail with public keys only:

- Each signed step must verify under the key trusted for the machine that ran it, and must answer the step the core
  recorded as asked under its number.
- Each blob a step names must be in the trail store, and must still hash to its digest.
- A browser session must count its steps from 0 with no gap, name its build on its first step, and end with a signed
  close once its Assignment ends.
- A broken chain and a gap in an Assignment's step numbers are problems too.

Two findings are only warnings, because an honest agent records them. One is an Assignment's last step, asked and
never answered, which a host that stops in the middle of a step leaves. The other is a step recorded as `lost`, which
has the agent's word and no signature. `--strict` counts both as problems. An open browser whose Assignment still runs
is neither. A step that fails a check is no evidence about its session, so one tampered step shows as one problem.

Nothing anchors the chain's head outside `agent.db`, so a trail cut at a clean session boundary still checks clean.

## The Watchdog

With a `watchdog` section in agent.yaml, `agent serve` runs the Watchdog beside the runtime. `assess(snapshot, now,
thresholds)` in `src/agent/watchdog.rs` is pure. It reads only what the Store holds (open Tickets, recent Decisions,
pending Effects with their attempt counts, the spend ledger) and the state directory's pause gate. It calls no model,
so it needs no account. To set it up, see [Get alerts from the Watchdog](how-to/operate.md#get-alerts-from-the-watchdog).

**Signals,** with their default thresholds:

- A Ticket that has waited 1h for a person: an approval, an answer, or a look at blocked or failed work. A Ticket whose
  request named no reply channel says that nobody else was asked.
- A working Assignment with no Report for 30m (stalled). A pause holds this signal, and the quiet time restarts when
  the pause lifts, since workers cannot call a model while paused.
- An Effect with 3 failed attempts, such as a Send stuck on one conversation.
- 1 `unavailable` Decision in the last hour: the Router left an Eval undecided three times running.
- An account at 80% of its cap: the Router's daily cap (known from agent.yaml) or an open Ticket's budget. An account
  with no cap raises nothing.
- A pause that the state directory's `control.json` or `PAUSE` has held for 1h, timed from the file. A pause set in
  agent.yaml, or the peak window, raises nothing.
- An external job (`watchdog.jobs`) that is down, stalled or low on disk. It is not a Ticket, and a pause does not hold
  it. See [External jobs](#external-jobs).

**Alert ids.** An id is `wd_` and a hash of the kind, the subject and a bucket of `repeat` (6h). The subject holds the
problem's start where the Store has one: the launch or Report a stall counts from, the Report or step a wait counts
from, the call that crossed a cap, the pause file's time. Buckets count from when the threshold was crossed. So a
problem is told when it crosses its threshold, and once every 6h while it lasts, and a problem that comes back is a new
alert. A failing Effect and `unavailable` Decisions have no start in the Store, so their buckets are the clock's.

**The loop.** The Watchdog runs every `every` (1m), the first time one interval after the start, once the runtime has
recovered what the last run left. Each new alert becomes a row in the Store's `watchdog_alerts` table (schema v3),
which only the Watchdog writes. The row is written before the alert is sent, and marked sent after. A known id is never
told again, so a restart tells nothing new, and an unsent row goes out at the next tick. The alert goes to
`watchdog.operator` as an attention message, during a pause too.

**Its one action.** With `retry_stalled`, the Watchdog asks the runtime to retry a stalled Assignment, as
`Fact::Retry`, so the Engine keeps one writer. The runtime first checks that the Assignment is still the Ticket's
working one, with the Reports the Watchdog saw, then calls `Engine::retry`, which spends from the same Ticket budget.
It acts once per Ticket, and a second stall of the same Ticket is only told. It never widens a Mandate or a budget.

**Status.** `solos-swarm agent status` shows the alerts `assess` would raise now, next to what is running and why. The
alerts for external jobs come from what the Watchdog last found, in `state/watchdog-jobs.json`: `agent status` checks
nothing itself.

The Watchdog runs inside `agent serve`, so it cannot tell anyone that `serve` is down. `agent health` does that from
outside ([Check the agent from outside it](how-to/operate.md#check-the-agent-from-outside-it)).

### External jobs

The box runs work that agent mode does not own: a downloader as a systemd user unit, a data build in tmux, a training
run that a script reports on. `watchdog.jobs` in agent.yaml lists them ([configuration.md](configuration.md#watchdog)),
and the Watchdog looks at each one on every look, before it assesses. The code is `src/agent/external_jobs.rs`.

**Status.** A job is `up`, or it is in one kind of trouble:

- `down`: it does not run. A unit is not `active` or `reloading`, or is not loaded at all (a transient unit that ended
  is gone). A tmux session does not exist. A pid file is missing, holds no number, or names a process that is not
  running or is a zombie. A status command exits with anything but `0` or `2`, is killed by a signal, cannot be run or
  does not finish within its `timeout`.
- `stalled`: it runs, but its status command exits `2`, or its `progress.file` has not changed for `stalled_after`, or
  does not exist. The detail names the time the file last changed, so it reads the same for as long as the job stays
  stalled.
- `low_disk`: it runs and makes progress, but the disk of `disk.path` has less than `min_free_gb` free, or its free
  space cannot be read.

Down outranks stalled, which outranks low on disk. A job that is down is not asked for its progress or its disk.

**What runs.** The checks run on the host, as the agent's user, outside any sandbox, and only what agent.yaml names. A
unit and a session are checked by validated names through `systemctl --user show` and `tmux has-session -t =NAME`. A
`command` is a list that the owner wrote: no shell reads it, so no text is ever interpolated into a command line, and
nothing a model wrote reaches one. Each program gets no input, an environment of only `PATH`, `HOME`, `USER`, `LOGNAME`,
`LANG`, `LC_ALL`, `TMUX_TMPDIR`, `DBUS_SESSION_BUS_ADDRESS` and `XDG_RUNTIME_DIR`, and its own process group. Past its
timeout the whole group is killed. Of its output only the first 4 KB are kept, the detail is one line of at most 200
characters, and control characters are dropped. All jobs are checked at once, so a job that hangs delays the look by
its own timeout and no more. A command that leaves a background process holding its output counts as not finishing.

**Incidents.** The Watchdog keeps `state/watchdog-jobs.json`: for each job, its status, its detail and when that
status began, and when it last looked. That start is what makes an incident. A job's alert id hashes its name, its
status and that start, with no `repeat` bucket, so one incident is one alert for as long as it lasts, a restart tells
nothing new, and a job that comes back and fails again, or goes from down to stalled, is a new alert. When a job that
the operator was told about is `up` again, the operator is told once more: `The job "l2book" (systemd unit
l2book-download.service) is back up. It had been down for 12m.` That message is a `job_up` row in `watchdog_alerts`,
and a job that was never told about has none. Flapping makes an alert and a recovery each time, because there is no
hold-down. The state is written before the alerts are recorded, so a crash between the two can lose a recovery message
and cannot repeat an alert.

**The dashboard.** `agent publish` uploads that file as `jobs/index.json`, when its content changes and on every
heartbeat, and the dashboard lists the jobs on its home page above the tickets
([production.md](production.md#dashboard)). Without jobs, the file is not written and nothing is uploaded. A registry
that is emptied leaves a file with no jobs, which clears the page.

Spend per job is not watched here. The design for long-running work (#101, #102) owns it.

## Memory

Memos are what the agent learned on one Ticket that a later, different Ticket should know. The owner writes them with
`solos-swarm agent memo add` or the MCP tool `agent_memo_add`, and they are active at once. Workers on Pi propose them
too: a review decides each proposal that is not outside text, with no click, and one that rests on outside text waits
for the owner. The design is [memory/design.md](memory/design.md). To manage memos, see [Manage the agent's
memory](how-to/memory.md).

**Where memos are shown.** Memos are quoted as data in the Router's system prompt (the global ones, within 1 KB) and in
each worker's first prompt (the global ones and its repository's, within 3 KB). When the index in a prompt leaves memos
out, its last line says where to read them. A `memos_shown` event on the chain names the memos each session was shown.

**Proposing.** `memo_propose {text, scope, evidence}` takes:

- `text`: at most 400 characters.
- `scope`: `global`, or the Ticket's own `repo:<owner>/<name>`.
- `evidence`: 1 to 4 pieces, each `{trigger, quote}` (the Trigger the Ticket came from, or one in its input) or
  `{call, quote}` (the tool whose answer it quotes, such as `run_code`).

The broker keeps what each of the seat's calls answered and finds every quote, word for word with runs of whitespace
taken as one. Otherwise it refuses the proposal with why, before anything reaches the Engine. Vault values in the text
and the quotes become `[vault:<name>]`. An Assignment proposes at most 3 memos.

**Trust.** The core derives a memo's trust from where its quotes came from. The least trusted quote sets it:

- A person's whole message, quoted word for word as the memo, is `person`, and the memo is active at once. Only
  people that `policy.people` names count.
- A part of a person's message, or a paraphrase of it, is `model`, even word for word: the part may be words the
  person quoted from someone else.
- A stranger's or a bot's words, a page, a file, and any tool's answer make it `external`. That includes the output
  of `run_code` and of jobs, since a command can print a file, a page or whatever a worker was told to echo.

**The review.** A `model` proposal goes to a separate model call with `thinking: {type: enabled}` and
`reasoning_effort: max`, DeepSeek's documented highest (see
[thinking mode](https://api-docs.deepseek.com/guides/thinking_mode)). The call gets fixed instructions, the proposal
with its quotes, where each came from and the whole message a quote was cut from, and the active memos of its scope,
all as data. It answers one JSON decision:

- `keep`.
- `rewrite`: its words become a new memo that cites the same evidence, and the proposal is retired naming it.
- `merge`: one new memo replaces the proposal and a `model` memo it repeats, and both are retired.
- `reject`.

Its instructions reject what is instruction-shaped (permissions, tools, budgets, targets) and what outside text asks
for. The host checks the answer and applies it through the Engine. Any other answer, a failed call, a pause or a
refusal leaves the proposal proposed, never active. Reviews run in the background, one at a time, each proposal at most
once per run. A start reviews what the last run left proposed, and a proposal that no review decides is retired as
`expiry` after 7 days. The calls are held and settled on the ledger's `memory` account.

**Outside text.** An `external` proposal is never reviewed, and the Engine refuses to make one active by any review
decision: a keep, a rewrite, or a merge whose sources would make the new memo `external`. It stays proposed, out of
every prompt, until it expires after 7 days. If it is right, the owner adds it in their own words with
`agent memo add` (see [Manage the agent's memory](how-to/memory.md)). This is the one exception to the fully
autonomous review: `model` and `person` memos still become active with no click.

**Reading.** `read` serves `/memos/global.md` and `/memos/repo/<owner>/<name>.md`: every active memo of the scope, with
who kept it, why, and its evidence, every model or outside line quoted. The files are rendered from the Store as they
are read, so a forgotten memo is in no file. The Router reads the same with `router_read {kind: memo, id: global}` or a
memo's id.

**Storage.** Memos are a core aggregate in a table of `agent.db` (schema 8). A memo's text is in its row, and only its
digest is on the event chain, so `forget` can erase the text and the chain still verifies.

### The dream

Once a day, at `memory.dream.at`, `agent serve` asks for the dream: an ordinary Ticket, on the Watchdog, the dashboard
and the chain like any other, that checks the memos against what the day's Tickets did (`src/agent/dream.rs`, schema
9).

**Asking.** At the day's slot, the runtime looks whether a Ticket ended since the last dream began. If one did, it
receives a System Trigger of the Source `dream` (`dream:<slot>`), whose id names the slot. If none did, nothing is
asked. The slot is kept in `<state>/dream-slot`, so a restart the same day asks for nothing new, and a run with
`--until-idle` asks for no dream at all. The host judges the dream's Eval itself, in the Router's place, and the Router
never sees it. The dream is a session on Pi on this host, with no budget of its own, no acknowledgement and no reply
channel.

**Staging.** Before the dream's sandbox starts, the host copies these files into its `/work`, read-only and once:

- `tickets/index.json`: each Ticket that ended since the last dream, at most 40, with each transcript's sha256 and line
  count.
- `tickets/<ticket>.json`: its request, input, replies, outcome and cost.
- `transcripts/<ticket>/<assignment>.rpc.jsonl` or `.claude.jsonl`: copies of the workers' transcripts, up to 16 MiB
  each and 128 MiB in all.
- `memos/index.json`: every live memo, with its trust and sources.

A later Assignment of the same dream finds the record in `<ticket>/dream.json`, and writes nothing where a sandbox
wrote.

**Its session.** The dream gets a short brief instead of the worker instructions. The brief says what memory is for,
and tells the dream to correct, merge and retire memos, learn from two Tickets or more, fill a gap, and cite
everything. Its only tools are `worker_report`, `read`
(its `/work` and `/memos`), `run_code` where this host has it, `memo_propose` and `memo_retire`. It has no network. Its
model calls go with DeepSeek's thinking on at `memory.dream.thinking` (`thinking: {type: enabled}`,
`reasoning_effort`, no temperature), and pass every earlier answer's `reasoning_content` back, as thinking mode with
tools needs. Every other session's and swarm mode's request bodies are unchanged. The calls are held and settled on the
`memory` account, under `memory.daily_microusd`.

**Evidence.** The dream cites a transcript line as `{transcript: <assignment>, sha256, line, quote}`. The broker reads
the original transcript that the host keeps, never the copy in `/work`. It checks that the transcript's whole digest is
the cited `sha256` and that the line says the quote, and it records who wrote the line:

- The worker's own words (what it said, or the command it ran): `model`.
- A tool's answer, a command's output included: `external`.
- The prompt: `external`.

An edited copy, a wrong digest, or a line that does not hold the quote is refused. The dream may not cite Triggers or
its own calls.

**Changes.** `memo_propose` with `supersedes: <memo>` proposes a correction. The review decides it like any proposal,
and keeping it in any form retires the old memo, naming the new one (`replaced_by`). A correction that cites an
`external` line is outside text: no review decides it, and the old memo stands.
`memo_retire {memo, reason, evidence | duplicate_of}` retires a `model` or `external` memo at once, citing the lines in
which a worker's own words show it is wrong or stale, or the active memo that already says it. An `external` line
retires nothing: transcripts hold whatever a page, an issue or a command printed. `duplicate_of` cites no line, and
stays as it was. The dream never supersedes or retires a person's memo. A dream makes at most 10 proposals and 10
retirements.

**The digest.** Once the dream has ended and the review has decided what it proposed, or 30 minutes have passed, the
Watchdog tells its operator once what was kept and in place of what, what was rejected, and what was retired. A
proposal that rests on outside text does not hold the digest back: it is listed as waiting for the owner, who can add
it with `agent memo add`. The digest links each Ticket's page where `web.public_url` is set. Its id names the dream, so
a restart neither repeats it nor loses it. Without a `watchdog:` section there is no digest, and the dream's Ticket and
the memos still say what it did.
