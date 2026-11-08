# A model policy per repository

> **Status, 2026-11-08.** PR 1 of [the plan](#5-pr-plan) is built: `forge.repos.<repo>.models` and
> `executor.upstreams` load and are checked, and `src/agent/models.rs` holds the policy. The rest is design only, for
> [#103](https://github.com/GuiBibeau/solos-swarm/issues/103). Grounded in `main` at `7ea26d2`. The [sketch](sketch/)
> holds types and signatures, not compiled.

## 0. In one paragraph

Today the broker holds one model key and one provider, and every Router call, worker call, memo review and dream goes
to it. The owner's rule is that a private repository's code reaches DeepSeek only after an explicit approval for that
repository, and that some repositories' code stays on Claude. This design gives each repository in `forge.repos` an
allowlist of providers, per role, and makes the host pick every model from it. A repository's Tickets run on an
upstream its list allows, or are declined with a sentence the person reads. Every other path by which a repository's
text reaches a model (the Router's cards and reads, memos, the memory review, the dream) is filtered by the provider of
the session that would read it. The Router stays on DeepSeek and sees a withheld line where a restricted repository's
text would be. A worker on Claude runs on Claude Code, through the broker's native Anthropic wire, with the Anthropic
key held by the broker. The provider of each Ticket and Assignment is recorded in the Store and shown on the dashboard.

## 1. The problem

The owner is moving a private research programme, `GuiBibeau/solos-markov-regime-switching-solana-perps`, into agent
mode on `remote-ai`. The Router and the workers run DeepSeek Flash for every repository
([production.md:68-77](../production.md)). The owner's rule:

- A private repository's code goes to DeepSeek only after an explicit approval for that repository.
- Some repositories' code work stays on Claude.

The acceptance test, from the issue:

1. A repository restricted to Anthropic runs its worker on a Claude model through the broker.
2. A DeepSeek-only config refuses that repository's Tickets with a clear reason.
3. The dashboard shows the provider of each Ticket.

The repositories in the owner's setup, with their visibility as `gh repo view <repo> --json visibility` reported it on
2026-11-08:

| Repository | Visibility | In `forge.repos` today | Reaches DeepSeek today |
|---|---|---|---|
| `GuiBibeau/solos` | private | yes, `owes: pull_request` ([production.md:88-93](../production.md)) | yes, every Ticket on it |
| `GuiBibeau/solos-swarm` | public | yes, read only ([production.md:94-96](../production.md)) | yes |
| `GuiBibeau/solos-markov-regime-switching-solana-perps` | private | no | no, and it must not until this lands |

`GuiBibeau/solos` is private and already runs on DeepSeek, so its approval must be written down too. That is
[decision 2](#7-decisions-for-the-owner).

## 2. What exists

### 2.1 One provider, one key, one set of worker terms

- `agent serve` reads one model key, `--credential-file`, once into memory ([src/agent/mod.rs:111-112](../../../src/agent/mod.rs),
  [:145](../../../src/agent/mod.rs)). It resolves one upstream from the Router's provider, else the workers'
  (`pi_upstream`, [src/agent/config.rs:1672-1688](../../../src/agent/config.rs)), and builds one transport around the
  key ([src/agent/mod.rs:151-171](../../../src/agent/mod.rs)).
- Config load refuses a Router and workers on different providers, "since one credential serves both"
  ([src/agent/config.rs:1577-1581](../../../src/agent/config.rs)). The memory review must use the workers' provider for
  the same reason ([src/agent/config.rs:1324-1330](../../../src/agent/config.rs)).
- The admitted providers are `deepseek` and `venice`, each with one fixed endpoint
  ([src/gateway/upstream.rs:45-62](../../../src/gateway/upstream.rs), [:152-158](../../../src/gateway/upstream.rs)).
  Both speak OpenAI chat completions with a bearer key ([src/gateway/upstream.rs:77-128](../../../src/gateway/upstream.rs)).
- The broker holds one `provider` and one `worker_terms` ([src/agent/broker.rs:768-772](../../../src/agent/broker.rs)).
  Every worker is admitted on those terms ([src/agent/broker.rs:1015-1019](../../../src/agent/broker.rs)), and every
  held call is dispatched to that one provider ([src/agent/broker.rs:1602](../../../src/agent/broker.rs)). The swarm
  executor is handed the same provider ([src/agent/mod.rs:511-519](../../../src/agent/mod.rs)).
- The executor asks every session for one model name, `parts.model`
  ([src/agent/pi_executor.rs:418-419](../../../src/agent/pi_executor.rs), [:775](../../../src/agent/pi_executor.rs)).
- The ledger already records a provider on every hold ([src/agent/broker.rs:1553-1563](../../../src/agent/broker.rs),
  the `provider` column at [crates/agent-store/src/schema.rs:66](../../../crates/agent-store/src/schema.rs)). It does
  not record the model.

**How workers stay keyless.** The key lives in the transport inside the broker
([src/gateway/upstream.rs:71-76](../../../src/gateway/upstream.rs)). A session gets only a bearer token: `SWARM_TOKEN`
for Pi ([src/harness.rs:180](../../../src/harness.rs)), `ANTHROPIC_API_KEY` set to the token for Claude Code, which
reaches the broker through a loopback bridge ([src/harness/claude_code.rs:133-140](../../../src/harness/claude_code.rs)).
The broker maps the token to a `Grant` ([src/agent/broker.rs:594-600](../../../src/agent/broker.rs)) and revokes it
when the Assignment ends ([src/agent/pi_executor.rs:765-768](../../../src/agent/pi_executor.rs)).

### 2.2 Provider, wire and harness are three things

- **Provider**: who receives the text and bills for it: `deepseek`, `venice`, and, after this design, `anthropic`.
  The owner's rule is about providers, so the policy keys on the provider alone.
- **Wire**: how the broker talks to the provider. `chat` is OpenAI chat completions, today's only wire. Branch
  `feat/deepseek-anthropic-wire`, in flight, adds `anthropic`: Messages forwarded natively, so Claude Code runs on
  DeepSeek's Anthropic endpoint (`https://api.deepseek.com/anthropic`) with the key still in the broker.
- **Harness**: what runs in the sandbox. Pi speaks chat to the broker's `/model`
  ([extensions/broker.ts:71-76](../../../extensions/broker.ts)). Claude Code speaks Messages to `/v1/messages`, which
  today translates to chat ([src/agent/broker/messages.rs:38-57](../../../src/agent/broker/messages.rs)).
  `executor.harness` picks the default, and a Plan may name one
  ([src/agent/pi_executor.rs:384-402](../../../src/agent/pi_executor.rs)).

So "a Claude Code worker" is not "a Claude model". Claude Code on DeepSeek's Anthropic wire is still provider
`deepseek`, and the policy treats it as DeepSeek. A Claude model needs provider `anthropic`, with its own key.

### 2.3 Where a repository's text reaches a model today

`R` is a repository. "Text" is anything that came from R: its code, its issues and pull request comments, and what a
worker on R wrote (reports, questions, memos, transcripts), since those quote the code.

| # | Path | What of R it carries | Which model reads it | Where |
|---|---|---|---|---|
| 1 | Router cards | GitHub issue and PR comments on R, as Triggers on thread `github:R#n`, quoted up to 1,200 characters | the Router's provider | [pi_router.rs:224-250](../../../src/agent/pi_router.rs), [github/mod.rs:57](../../../src/agent/github/mod.rs) |
| 2 | `router_read` of a Trigger | the same text, up to 2,000 characters | the Router | [broker.rs:2445-2483](../../../src/agent/broker.rs) |
| 3 | `router_read` of a Ticket | the worker's latest report (1,000 characters), its waiting question, its end reason | the Router | [broker.rs:2406-2443](../../../src/agent/broker.rs) |
| 4 | `router_read` of memos | every active memo of `repo:R`, or any memo by id with its quotes | the Router | [broker/memory.rs:985-1030](../../../src/agent/broker/memory.rs) |
| 5 | The Router's system prompt | the global memos, which a worker on R may have written; each repository's `about`, which the owner wrote | the Router | [broker.rs:1398-1414](../../../src/agent/broker.rs), [pi_router.rs:156-170](../../../src/agent/pi_router.rs) |
| 6 | The clone | R's code at `/work`, read with `read` and run with `run_code` | the Ticket's worker | [pi_executor.rs:699-721](../../../src/agent/pi_executor.rs) |
| 7 | A worker's first prompt | the Ticket's Inputs (R's issue text), earlier reports, the global and `repo:R` memos | the worker | [pi_executor.rs:748-757](../../../src/agent/pi_executor.rs), [runtime.rs:908-925](../../../src/agent/runtime.rs) |
| 8 | A worker's `read /memos` | the memo page of any repository, from any Ticket | every worker | [broker/memory.rs:948-968](../../../src/agent/broker/memory.rs) |
| 9 | Memo proposals | a worker on R may file a `global` memo | later, every session | [broker/memory.rs:335-336](../../../src/agent/broker/memory.rs) |
| 10 | The memory review | the proposal, its quotes (often R's command output), and the active memos of its scope | the workers' provider | [broker/review.rs:394-452](../../../src/agent/broker/review.rs) |
| 11 | The dream | up to 40 ended Tickets' records and transcripts, and every live memo | the workers' provider | [dream.rs:178-195](../../../src/agent/dream.rs), [dream.rs:445](../../../src/agent/dream.rs) |
| 12 | Swarms | nothing: a swarm cannot name a repository | none | [broker.rs:1896-1898](../../../src/agent/broker.rs) |
| 13 | A handback's reason | goes to the event log only | none | [crates/agent-core/src/engine.rs:383-387](../../../crates/agent-core/src/engine.rs) |
| 14 | The dashboard | every Ticket's request, conversation and reports | no model: a private Vercel Blob store | [publish/snapshot.rs:107-147](../../../src/agent/publish/snapshot.rs) |
| 15 | Shared mounts and the vault | `executor.run_code.mounts` bind into every worker's sandbox; vault items are scoped by requester, not by repository | every worker | [exposure.rs:62-66](../../../src/exposure.rs), [vault.rs:275-284](../../../src/vault.rs) |

Two things already help. The agent's own replies never come back as Triggers: Discord drops the bot's messages and
GitHub the bot's comments ([discord/source.rs:45](../../../src/agent/discord/source.rs),
[github/source.rs:38](../../../src/agent/github/source.rs)). And a repository Ticket runs only on this host
([broker.rs:2000-2005](../../../src/agent/broker.rs)), so R's clone never reaches a node.

## 3. Three designs

### A. One agent, a clearance per session (recommended)

The repository config lists the providers each role may use. The host picks every model: a repository's Tickets get an
upstream its list allows, fixed at accept, and the Router never chooses one. Every session has a **clearance**: the
provider its calls go to. Every read path in the table above asks one pure function whether R's text may reach that
provider in that role. Where it may not, the session sees a withheld line in its place. The Router stays one Pi
session on DeepSeek.

### B. A lane per provider

The agent runs one Router, one memory review and one dream per provider. An Eval whose thread carries R's text goes to
a Router whose provider R allows. A Claude lane would judge R's issues with their text, so its titles and scopes are
better than A's. It still needs A's filters, because the DeepSeek Router can `router_read` any Ticket. And the Router is
Pi only ([runtime.md:393](../runtime.md)), so a Router on Claude needs a broker translation from chat to Messages that
does not exist (section 4.4).

### C. Two agents

The owner runs a second `agent serve` with its own config, state directory and unit, whose `forge.repos` holds only
the restricted repositories and whose models are all Anthropic. Nothing is shared: Store, memory, dream, and the
Watchdog are separate. It needs no policy code. It still needs an Anthropic upstream, and a Router on Claude, since a
DeepSeek Router there would read R's issues. The scripted Router names no repository
([scripted.rs:79](../../../src/agent/scripted.rs)), so it cannot stand in.

### Compared

| | A. Clearance per session | B. Lane per provider | C. Two agents |
|---|---|---|---|
| A hostile worker on a DeepSeek Ticket | Has no clone of R, and every memo, Input and page that carries R is filtered out. Left open: shared mounts and vault items without a first-use approval (section 4.10). | Same as A. | No path at all: another process, Store and state directory, possibly another OS user once `ops/harden-units` lands. Same mounts and vault caveat per config. |
| A hostile worker on R, on Claude | Cannot move R's text to a DeepSeek session: its memos carry R, its reports are withheld from the Router, its PR conversation is on R's thread. It can still post to a public URL with `fetch_url`, as any worker can today. | Same as A. | Same as A. |
| An outsider | Cannot pick a provider: the host does. Cannot comment on a private repository. With `accept_mode: off`, cannot start work. Injection through R's issues no longer reaches the DeepSeek Router at all, since it sees no text. | A Claude Router reads R's issues, so they can try to inject it, as any issue can today. | Same as B for the second agent's Router. |
| What the owner configures | One `models` line per repository and one Anthropic upstream with its key file. | A plus a Router, review and dream section per lane. | A second `agent.yaml`, unit, state directory, Discord channel, MCP state directory and dashboard prefix. Two places to look. |
| Code, with tests | About 3,000 lines in six PRs. | A plus about 2,000: a chat-to-Messages translation for a Pi Router on Claude, Eval routing between Routers, a review and dream per lane. | About 1,500 (the Anthropic upstream, a Router on Claude, a dashboard for two agents), plus the operations work. |

A keeps one agent and one place to look, and it is the only design that keeps the Router on DeepSeek. The price is that
the Router judges a restricted repository's GitHub requests on a withheld card. With `accept_mode: off`, those are
explicit requests from listed people, so the judgment left is small: a title, a budget and a deliverable, which fall
back to the repository's own `budget_microusd` and `owes`. C is the fallback if the owner wants hard isolation more
than one place to look.

## 4. The recommended design

### 4.1 The rule

A repository's code and text reach a model only on a provider that the repository's `models.allow` lists for that role.
The host picks every model. Every read that a model can make is filtered by the provider it runs on.

There are three roles:

- `router`: the Router's cards, its `router_read`, and its system prompt.
- `worker`: the workers of R's Tickets (the clone, the Inputs, the memos), and any other worker shown a memo that
  carries R.
- `memory`: the memory review of a proposal that carries R, and the dream reading R's transcripts and memos.

### 4.2 Config

```yaml
executor:
  kind: pi
  provider: deepseek                 # the default upstream, as today; its name is `default`
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1320000, cache_hit_microusd_per_million: 30000}
  claude:
    binary: /home/agent/.local/share/claude/versions/2.1.231
  upstreams:                         # new: other upstreams a Ticket may run on, by name
    claude:
      provider: anthropic
      model: claude-sonnet-5-5
      wire: anthropic                # the only wire anthropic has
      harness: claude-code           # what its sessions run on; anthropic needs claude-code
      key_file: /run/user/1000/solos-agent/anthropic.key
      prices:
        input_microusd_per_million: 2000000
        output_microusd_per_million: 10000000
        cache_hit_microusd_per_million: 200000
        cache_write_microusd_per_million: 2500000
      max_output_tokens: 16000
forge:
  kind: github
  repos:
    GuiBibeau/solos:
      # about, base, owes, deps, evidence as today
      models: {allow: [deepseek]}
    GuiBibeau/solos-swarm:
      models: {allow: [deepseek, anthropic]}
    GuiBibeau/solos-markov-regime-switching-solana-perps:
      about: Regime-switching research for Solana perps (private).
      base: main
      owes: pull_request
      budget_microusd: 2000000
      models:
        allow: [anthropic]           # every role
        worker: claude               # the upstream its workers run on
```

`allow` is a list, which applies to every role, or a map that names each role:

```yaml
      models:
        allow: {router: [deepseek, anthropic], worker: [anthropic], memory: [anthropic]}
```

The rules config load checks:

- **Every repository names `models`.** A repository without it fails to load, with a sentence that points at this
  section. This is how "only after an explicit approval" holds: no repository reaches any provider by default. It is a
  breaking change for the production config, which must add a line to each repository before the deploy that ships it.
- A role map names all three roles. A provider is one of `deepseek`, `venice` and `anthropic`.
- `worker` names `default` or a key of `executor.upstreams` whose provider the worker role allows. Without it, the
  worker upstream is `default` when its provider is allowed, else the one allowed upstream. Two allowed upstreams and
  no `worker` is an error that asks for one.
- An upstream's `harness` and `wire` must fit: `pi` needs `chat`; `claude-code` takes either; `anthropic` has only the
  `anthropic` wire, so it needs `claude-code`.
- `key_file` is a one-use token file, refused under any path a sandbox can read, as `github.key_file` is
  ([src/agent/mod.rs:794-814](../../../src/agent/mod.rs)). The default upstream keeps `--credential-file`.
- A repository with no allowed upstream here still loads. `agent serve` says so at start, and its Tickets are declined
  (section 4.3). That is acceptance test 2.

**Tickets that name no repository**, such as a Discord or MCP question, run on `default`, as today. Nothing of R reaches
them except what a person wrote: they have no clone, and every memo and Input that carries R is filtered by their
clearance. A question about how R works should name R, and the Router's prompt already says so
([pi_router.rs:156-162](../../../src/agent/pi_router.rs)). If the Router fails to name it, the worker on DeepSeek cannot
read R's code and says so. That fails safe.

### 4.3 Where it is enforced

One pure module, `src/agent/models.rs`, builds a `ModelPolicy` from the config and answers
`admits(repo, role, provider)`. Everything below calls it. agent-core stays pure: the runtime hands the core the set of
repositories a session is cleared for, as data.

**At accept.** `router_decide`'s verdict ([broker.rs:1841-1981](../../../src/agent/broker.rs)) already fills
`ForgeTerms` from config and never from the model's words ([broker.rs:1949-1961](../../../src/agent/broker.rs)). It
does the same for the model. An accept that names R gets `Mandate.upstream`, R's worker upstream, and its Plan's
`harness` from that upstream, where today the Router always leaves it empty
([broker.rs:1884-1888](../../../src/agent/broker.rs)). Two refusals are added:

- **No upstream here is allowed for R.** The verdict becomes a `Decline`, a new Verdict. The Engine records a declined
  Decision and sends the host's sentence to the request's channel, once, on an explicit Eval:
  `I can't work on GuiBibeau/solos-markov-regime-switching-solana-perps here: its code may go only to anthropic, and this
  agent runs no anthropic model. Nothing was started.` The Router's tool reply says the person was told. Today an
  ignore sends nothing ([engine.rs:2026](../../../crates/agent-core/src/engine.rs)), and the Router is told to ignore
  "questions you cannot act on" ([pi_router.rs:53](../../../src/agent/pi_router.rs)), so without a Decline the person
  would hear nothing.
- **An Eval whose Triggers are on R's GitHub thread must name R**, when R's router role does not admit the Router's
  provider. Otherwise a Ticket with no repository, on DeepSeek, would get R's issue text as its Inputs.

**At launch.** Before the clone, the executor checks the Ticket's upstream against the policy as it is now. A budget is
checked the same way at every launch ([runtime.md:28](../runtime.md)). A Ticket whose repository's policy changed, or a
Ticket accepted before this build with no recorded upstream on a repository whose policy no longer admits `default`,
ends its Assignment with a `failed` Report that names the policy. The person hears it once, and nothing is cloned.

**At admission.** `admit_worker` takes the Ticket's upstream and admits the seat on that upstream's terms. It refuses
an upstream whose provider R's worker role does not admit. This check cannot be reached by a correct accept and launch;
it is the backstop.

**At dispatch.** The broker's one `provider` becomes a map from upstream name to transport, and `dispatch` picks by the
grant's upstream. A grant cannot name a transport the map does not hold.

**Every read path,** numbered as in section 2.3:

| # | Path | How the policy is enforced |
|---|---|---|
| 1 | Router cards | A Trigger on `github:R#n`, where R's router role does not admit the Router's provider, shows `[withheld: GuiBibeau/... allows its text only on anthropic]` in place of its text. The card keeps who wrote, where, the strength, the thread key (which names R and the number) and the Tickets. |
| 2 | `router_read` of a Trigger | The same withheld line. |
| 3 | `router_read` of a Ticket on R | Keeps the id, stage, repository, plan, budget, requester and ids. Withholds the report text, the waiting question and the end reason. The scope and title stay, since the Router wrote them. |
| 4 | `router_read` of memos | A memo that carries R is not listed or served. |
| 5 | The Router's system prompt | The global memo index leaves out memos that carry R. Each repository's `about` stays: it is the owner's own text, and the Router needs it to name R. |
| 6 | The clone | Exists only on R's Tickets, whose upstream R's worker role admits. |
| 7 | A worker's first prompt | The upstream is admitted. The memo index leaves out memos that carry a repository whose worker role does not admit this worker's provider. The executor withholds any Input carrying such a repository, as a defense: section 4.3's accept rule already keeps them apart. |
| 8 | A worker's `read /memos` | The listing and pages show only memos this worker's clearance admits. |
| 9 | Memo proposals | The core derives a memo's `carries` from its sources (the Ticket, the calls and the transcript lines it cites, each belonging to a Ticket) and its scope, as it derives trust today. A `global` memo filed on R carries R, so only sessions cleared for R see it. |
| 10 | The memory review | A proposal that carries a repository whose memory role does not admit the review's provider is not sent. It stays proposed, and the Watchdog tells the operator once that it waits for them (decision 5). The related memos the review sees are filtered the same way ([review.rs:403-406](../../../src/agent/broker/review.rs)). |
| 11 | The dream | The staging leaves out Tickets on such repositories and memos that carry them, and the digest says how many were left out. A citation must name a staged transcript. |
| 12 | Swarms | Unchanged: a swarm still cannot name a repository. A swarm accept from R's GitHub thread is refused by the accept rule above. |
| 13 | A handback's reason | Unchanged: it reaches no model. |
| 14 | The dashboard | Not a model. It publishes as today, and shows each Ticket's provider. Decision 6. |
| 15 | Shared mounts and the vault | Not closed by this design. See section 4.10. |

### 4.4 Running a worker on a Claude model

The worker must call Claude through the broker, so the key stays in the broker and every call is held, settled and
paused like any other. Three ways were weighed.

**(i) Pi on a chat wire to Anthropic.** The broker would translate Pi's chat requests into Messages and the answers
back: the reverse of `gateway::anthropic`, which is 872 lines with its tests. The Pi extension registers one model id,
`deepseek-flash`, with DeepSeek's window and prices ([extensions/broker.ts:59-67](../../../extensions/broker.ts)), so it
would have to register the session's model too. Pi's chat stream carries reasoning as plain text, with no place for
the signed thinking blocks Anthropic wants back unchanged, and Anthropic's current models keep thinking on: Claude Sonnet 5.5 refuses `thinking: disabled` and wants `between_tools` instead, and
on accounts created since 2026-08-31 a history whose thinking was edited is refused. About 900 to 1,200 lines, and the
least faithful to the model.

**(ii) Claude Code on the `anthropic` wire, to Anthropic (recommended).** Claude Code already speaks Messages to the
broker. `feat/deepseek-anthropic-wire` makes the broker forward Messages natively to DeepSeek. Anthropic is one more
provider on the same wire, with:

- the endpoint `https://api.anthropic.com/v1/messages`, the key in `x-api-key`, and an `anthropic-version` header;
- models whose id starts with `claude-`;
- its own key, from `key_file`, in its own transport in the broker's map;
- a receipt reader for Anthropic's usage, where `input_tokens` leaves out `cache_read_input_tokens` and
  `cache_creation_input_tokens` (today's reader takes OpenAI's `prompt_tokens`,
  [pricing.rs:138-140](../../../src/gateway/pricing.rs));
- a price table with a cache-write rate. Anthropic bills a cache write at 1.25 times the input rate, and today's hold
  prices all input at the input rate ([pricing.rs:50-64](../../../src/gateway/pricing.rs)). A settlement above its hold
  stops every capped account ([broker.rs:1648-1654](../../../src/agent/broker.rs)), so the hold must price input at the
  higher of the two rates;
- price bounds per provider. Today's bounds stop at $10 per million output tokens
  ([pricing.rs:36-40](../../../src/gateway/pricing.rs)). Claude Sonnet 5.5 is $2 in and $10 out per million, with cache
  reads at $0.20, so it fits. Claude Opus 5.5 is $4 and $20, so it does not;
- no `temperature`. The broker samples workers at `executor.temperature`
  ([broker.rs:566-571](../../../src/agent/broker.rs)). Claude Sonnet 5.5 refuses a temperature other than its
  default, and Anthropic's current API version refuses one on every model;
- thinking blocks passed through unchanged. Today's facade drops them
  ([gateway/anthropic.rs:69-70](../../../src/gateway/anthropic.rs)), which this wire must not do;
- server tools still refused, so a model cannot search or fetch the web outside `fetch_url`'s guard. This matters for
  the DeepSeek Anthropic wire too, since DeepSeek runs web search itself when a request declares it.

The Claude Code harness also needs the session's model in more variables than `ANTHROPIC_DEFAULT_HAIKU_MODEL`
([claude_code.rs:140](../../../src/harness/claude_code.rs)), which the wire branch adds for DeepSeek. About 500 to 700
lines on top of that branch. Two Claude Code gaps carry over: a session does not report its last plain text yet, and a
budget refusal reads as "Failed to authenticate" ([runtime.md:172-173](../runtime.md),
[runtime.md:390-391](../runtime.md)).

**(iii) Claude Code with its own login in the sandbox.** Rejected. The credential would sit where a prompt-injected
worker can read it, the sandbox would need the network, and the spend would bypass the ledger and the pause.

Prices are Anthropic's list prices as of 2026-10-06. Sonnet 5.5 costs about 6.7 times DeepSeek's input rate and 7.6
times its output rate in the production config ([production.md:72](../production.md)), so decision 7 sets a budget.

### 4.5 Store changes

| Change | Where | Why |
|---|---|---|
| `Mandate.upstream: Option<UpstreamChoice>` (`name`, `provider`, `model`, `harness`) | [model.rs:275-297](../../../crates/agent-core/src/model.rs) | What the Ticket was accepted to run on, fixed at accept like `forge` |
| `Assignment.ran_on: Option<RanOn>` (`upstream`, `provider`, `model`, `harness`), set by `assignment_started` | [model.rs:409-417](../../../crates/agent-core/src/model.rs), [engine.rs:318](../../../crates/agent-core/src/engine.rs) | What each Assignment actually ran on, for Tickets accepted before this build too |
| `Verdict::Decline`, `DecisionKind::Declined` and its Send | [model.rs:585-623](../../../crates/agent-core/src/model.rs) | The refusal the person reads |
| `Memo.carries: BTreeSet<RepoName>`, derived by the core | [memo.rs:284-303](../../../crates/agent-core/src/memo.rs) | Filters memos by clearance |
| `MemoAudience` gains the repositories the session is cleared for | [memo.rs:342-351](../../../crates/agent-core/src/memo.rs) | `memo_index` stays pure |

Each PR that adds a field takes the next free schema version, with an empty migration, as every such PR does
([architecture.md](../architecture.md), "The Store"). The latest is 9
([schema.rs:4](../../../crates/agent-store/src/schema.rs)); `fix/memo-trust` may take 10 first. The `spend` table does
not change: its `provider` column already gives each Ticket's spend by provider.

### 4.6 Broker tools and grants

- No new tool. `router_decide` gains no field: the Router never names a model or a harness. Its description gains one
  sentence: the host picks the model from the repository's policy, and an accept on a repository no model here may
  read is declined and the person is told. `extensions/agent-tools.json` is regenerated for that sentence.
- `router_read` answers carry `withheld` where text was held back.
- A worker's grants do not change. A Claude Code worker on R holds the same broker tools as a Pi worker, through
  `solos-swarm mcp`.
- `agent-tools.json` and the Pi extension are otherwise unchanged.

### 4.7 Watchdog, status and dashboard

- **Start.** `agent serve` logs each repository's allowed providers per role and its worker upstream, and each
  repository that has none here.
- **Watchdog.** One new alert: a memo proposal that no reviewer here may read waits for the operator. Told once.
  Spend alerts work unchanged, since each Ticket keeps its own account.
- **`agent status`.** Each open Ticket shows its provider and model. The Router's provider is shown once.
- **Dashboard.** The snapshot format goes from 1 to 2 ([publish/snapshot.rs:24](../../../src/agent/publish/snapshot.rs)).
  `TicketSnapshot` gains `model: {upstream, provider, model, harness}`, from the Mandate or else the latest
  Assignment. `AssignmentView` gains `ran_on`. `IndexRow` gains `provider`. The ticket page shows it beside the plan,
  where it shows the harness today ([web/agent-dashboard/app/tickets/[id]/page.tsx:102](../../../web/agent-dashboard/app/tickets/%5Bid%5D/page.tsx)),
  and the index shows a provider column. That is acceptance test 3.

### 4.8 Docs

- `configuration.md`: `forge.repos.<repo>.models` and `executor.upstreams`.
- `security.md`: a "Model policy" section with the rule, what "withheld" means, and what is left open (section 4.10).
- `runtime.md`: withheld cards and reads, workers on an upstream, the memory filters.
- `how-to/repositories.md` and `how-to/claude-code.md`: run a repository on Claude.
- `production.md`: the `anthropic.key` token, and the `models` lines for the three repositories.
- `architecture.md`: the broker's transports by upstream.

### 4.9 Tests

- **Acceptance 1.** A config with an Anthropic upstream pointed at a fake Messages server, and a local forge repository
  whose `allow` is `[anthropic]`. The installed Claude Code runs the worker, as `tests/agent_claude_code.rs` does
  today. The fake saw `POST /v1/messages` with the broker's `x-api-key`, the model `claude-sonnet-5-5` and no
  temperature. The sandbox's environment held no key. The ledger's rows name `anthropic`. The Mandate records the
  upstream.
- **Acceptance 2.** A DeepSeek-only config with the same repository. `router_decide` naming it is declined. The
  request's channel gets the sentence once. No Ticket launches, and no clone is made.
- **Acceptance 3.** A snapshot test of format 2, and a dashboard test that renders the provider.
- **The canary test.** Plant one string in R's issue, a worker report on R, a memo proposed on R, a dream citation and
  a transcript on R. Run the Router, a worker on a Ticket with no repository, the review and the dream, each against a
  fake upstream that records every body it gets. No body sent to the DeepSeek fake holds the string. One test covers
  every row of section 4.3, and a new read path that forgets the policy fails it.
- **Config.** A repository without `models`, an unknown provider, a role map missing a role, an upstream whose harness
  and wire do not fit, two allowed upstreams and no `worker`, a `key_file` a sandbox can read: each refused with its
  sentence.
- **Live, on `remote-ai`** through `/verify-agent-mode`, with a private throwaway repository and a small Anthropic
  budget: a Ticket on it runs on Claude; `grep` finds the canary planted in its issue in no Router transcript under
  `state/router/` and in no DeepSeek worker's transcript.

### 4.10 What it does not cover

- **Shared mounts.** `executor.run_code.mounts` bind into every worker's sandbox
  ([exposure.rs:62-66](../../../src/exposure.rs)). A checkout or dataset of R mounted there reaches DeepSeek workers.
  Mounts need to be per repository first; the parallel `design/repo-resources` covers that. Until then the docs say
  not to mount R's data globally.
- **The vault.** Items are scoped by requester, not repository ([vault.rs:275-284](../../../src/vault.rs)). A GitHub
  token or login that can read R lets any worker read R through `fetch_url` or the browser. A first-use approval asks a
  person on each Ticket ([vault.rs:171-178](../../../src/vault.rs)), so such an item must never be `approval: none`.
- **Exfiltration to the internet.** A prompt-injected worker on R can send what it read to a public URL with
  `fetch_url`, on any provider, as today. This design decides which model reads R, not where a worker may send data.
- **Channels and the operator's MCP host.** Results go to Discord, GitHub and the MCP outbox as today. An MCP host on
  a model the owner has not approved would read R's replies through `agent_replies`.
- **The dashboard.** The Blob store is private and sign-in gated, but it holds R's text (decision 6).

## 5. PR plan

Each PR leaves `main` shippable and ends with the check named. Builds and tests run on the remote box.

| # | PR | Adds | Check |
|---|---|---|---|
| 1 | **The policy in config** | `forge.repos.<repo>.models` (list or role map, `worker`), required on every repository. `executor.upstreams` parsed, `deepseek` and `venice` only. `src/agent/models.rs` with `ModelPolicy::admits`. The start log. Docs: `configuration.md`, and the `models` lines in `production.md`. | Config tests for every refusal above. The production config, with a `models` line on each repository, loads under `agent health`'s config check. |
| 2 | **The host picks the upstream, or declines** | `Mandate.upstream` and the Plan's harness filled at accept. `Verdict::Decline` with its told sentence. The GitHub-thread accept rule. The launch check. Schema bump. | Acceptance 2, end to end on the runtime with a fake Router call. |
| 3 | **Many upstreams in the broker** | The transport map and per-upstream terms. `admit_worker` takes the Ticket's upstream. The session's model per Ticket. `key_file` per upstream. `Assignment.ran_on`. Schema bump. | Two fake chat upstreams with two keys, two repositories with different `allow`: each Ticket's calls reach its fake with its key, the ledger names each provider, and no sandbox environment holds a key. |
| 4 | **Anthropic** (after `feat/deepseek-anthropic-wire`) | Provider `anthropic` on the `anthropic` wire: endpoint, headers, model prefix, Anthropic receipts, the cache-write rate and the higher hold, bounds per provider, no temperature, thinking passed through, server tools refused, the Claude Code model variables. | Acceptance 1 against the fake Messages server, and one live Ticket on a private sandbox repository on `remote-ai` with a $0.50 budget. |
| 5 | **Withheld and filtered reads** | Withheld cards and `router_read`. The Router's memo index. `Memo.carries` and the cleared audience. `/memos`. The review's skip and filter. The dream's staging filter. The executor's Input filter. The Watchdog's waiting-memo alert. Schema bump. | The canary test, and the live `grep` of the canary in the Router's transcripts. |
| 6 | **Show it** | Snapshot format 2, the dashboard's provider on the ticket page and the index, `agent status`. Docs: `security.md`, `runtime.md`, `how-to/repositories.md`, `how-to/claude-code.md`, `architecture.md`. | Snapshot tests and the dashboard's tests (acceptance 3). |

**The gate.** `GuiBibeau/solos-markov-regime-switching-solana-perps` stays out of `forge.repos` until PRs 1 to 5 are
deployed. Before PR 5, a Claude worker's report on it would reach the DeepSeek Router through `router_read`.

## 6. Work in flight this depends on

No PR is open for these branches on 2026-11-08; each is being built.

- **`feat/deepseek-anthropic-wire`.** PR 4 builds on its native Messages transport and its `wire` setting. It must keep
  refusing server tools.
- **`fix/memo-trust`.** The review found that workers may file `global` memos from a repository Ticket. If that fix
  makes a repository Ticket's proposals repository-scoped, `carries` still matters for the dream's citations and for
  memos already stored. If it lands first, it likely takes schema 10.
- **`feat/worker-file-tools`.** Any new file tool must read only the seat's own Ticket directories, or it becomes a new
  row of section 2.3.
- **`ops/harden-units`.** The `anthropic.key` token joins the tokens its units copy into their runtime directory.
- **`fix/outsider-spend`.** A Decline sends its sentence only on an explicit Eval, so an outsider cannot use it to
  make the agent post.
- **`feat/cargo-deps`.** No model path. Its dependency builds run no model.
- **`design/repo-resources`** (a parallel design). Section 4.10's shared mounts depend on mounts becoming per
  repository.

## 7. Decisions for the owner

1. **Fail closed?** Every repository must name `models`, or config does not load. *Default: yes.* The alternative,
   "absent means every provider", would let a newly added private repository reach DeepSeek without an approval.
2. **What each repository allows.** *Default:* `GuiBibeau/solos` `[deepseek]`, since production already sends it there;
   `GuiBibeau/solos-swarm`, public, `[deepseek, anthropic]`;
   `GuiBibeau/solos-markov-regime-switching-solana-perps` `[anthropic]`. Say whether `solos` may also go to Anthropic.
3. **May the DeepSeek Router read the research repository's issues and reports?** *Default: no* (`allow` as a list, so
   the Router sees withheld cards). A role map with `router: [deepseek, anthropic]` gives better titles and scopes, and
   sends issue text and worker reports to DeepSeek.
4. **The Claude model.** *Default: `claude-sonnet-5-5`* ($2 and $10 per million tokens). Claude Opus 5.5 ($4 and $20)
   also needs the price bounds raised, which PR 4 does per provider.
5. **Memory on a Claude-only repository.** *Default:* a proposal on it waits for you, and the dream skips its Tickets.
   You keep what is right with `agent memo add`. A later PR could run the review on the Ticket's own upstream.
6. **The dashboard.** *Default:* publish the research repository's Tickets in full, as for any Ticket, since the Blob
   store is private, sign-in gated, and is not a model. The alternative is a per-repository switch that publishes only
   titles, stages and spend.
7. **Budget.** *Default:* `budget_microusd: 2000000` on the research repository. Claude costs about seven times
   DeepSeek per token here. A request from GitHub, MCP or a schedule is still capped at `auto_approve_microusd`
   ($0.50), so larger work is asked from Discord, where you can approve it.
8. **The Anthropic key.** *Default:* a key of its own, in an Anthropic workspace with a monthly spend limit, so the
   provider caps spend even if the ledger fails.
9. **Mounts and vault until per-repository mounts exist.** *Default:* the research repository's data is not mounted
   through `executor.run_code.mounts`, and no vault item that can read a private repository is `approval: none`.
