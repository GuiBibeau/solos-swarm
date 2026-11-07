# About the benchmark

DeepSeek's article measured why a super-agent should delegate, and what pass-through saves. `solos-swarm agent bench`
(`src/agent/bench/`) reproduces that ablation on agent mode itself. This page says what the bench simulates, what it
cannot model, and what its runs found. To run it, see [Run the benchmark](how-to/benchmark.md).

## What the article measured

The article ran a stream of about 300 group-chat and CI messages with about 20 work requests, simulated the coding work
with a tool that only takes time, and ran each design five times:

| Design | Completion p50 / p95 (min) | Cache-read tokens per run |
|---|---|---|
| Super-agent | 12.4 / 15.7 | 23.2M |
| Super-agent without pass-through | 12.6 / 15.5 | 31.8M |
| Single session with a subagent tool, neutral prompt | 20.4 / 29.5 | 6.1M |
| Single session with a subagent tool, delegation prompt | 13.8 / 29.0 | 10.8M |

Without pass-through, completion stayed about the same, but the Router made about 30% more decisions and read about 37%
more cache tokens.

## The workload

A spec and a seed give one stream in every build (`src/agent/bench/workload.rs`). The generator is SplitMix64, written
out in the code. Message times follow `per_minute`, as a Poisson stream or evenly.

- **Requests.** Allowed people (`dev-N`) address the agent (`@agent ...`), each request on its own Thread, within the
  first `request_span` of the stream. Each request carries its expected work, drawn uniformly from `work_minutes`, as
  `meta.bench.work_ms`. No two requests ask for the same task, so a spec takes at most 24 requests: a model Router
  reads a repeated task as work already done, and ignores it (#37).
- **Follow-ups and CI.** Follow-ups by a request's author, and CI messages from `ci-bot` (on `policy.bots`) or
  `deploy-bot` (not on it), land on request Threads, 10% to 50% and 20% to 90% of the way through the request's
  expected work.
- **Chatter.** The rest is chatter from allowed people and from people not on the list (`guest-N`). Only the guests
  mention the agent, since an addressed message from an allowed person is a request.

The spec's fields and defaults are in the [configuration reference](configuration.md#bench-workload-spec).

## Simulated work

`executor: {kind: simulated}` (`src/agent/simulated.rs`) stands in for the workers. An Assignment starts when one of
`max_workers` slots (64) is free, reports started, sleeps its request's `work_ms` times `time_factor`, and reports
`ready`. It reads the work from the Ticket's origin Trigger, which the runtime hands every Executor as
`Launch::origin`. A Ticket whose origin names no work takes `default_work_ms` (10 minutes). Inputs are taken and
ignored.

## The designs

Each design is an agent.yaml that the bench writes into the run's directory:

- **`super-agent`**: agent mode as it is, with 64 worker slots.
- **`no-pass-through`**: `pass_through: false` at the top of agent.yaml. An allowed person's follow-up on a subscribed
  Thread still reaches its Ticket, and is also queued for the Router like every other Trigger. A new Ticket that adopts
  queued Triggers leaves them queued too. This design is the one core change the bench needed,
  `Engine::set_pass_through` (on by default), applied by `Runtime::with_pass_through`.
- **`serial`**: one worker slot, and `router_waits_for_work: true`, which holds each Candidate until no simulated work
  runs (`HeldRouter`). The bench sets `policy.eval_timeout_ms` past the deadline, so the held Evals do not expire.

`serial` stands in for the single session with a subagent tool, and it is an approximation. That session decides
whether to delegate, and can keep reading while a subagent runs. Here every request is delegated, and nothing new is
read until the work returns. `serial` is nearer the worst case of the neutral prompt, and the delegation prompt is not
modelled.

With the scripted Router, each design uses `router: {kind: scripted, ignore_follow_ups: true}`. An explicit Candidate
on a Thread whose Ticket is still open is ignored, so a follow-up that reaches the Router costs a decision, not a
second Ticket.

## How a run works

For each design and run, the bench serves a fresh agent inside the bench's own process (`agent::serve_until`: a new
state directory, Store, broker and sessions). It copies `workload.jsonl` into the source's `triggers.jsonl` as each Trigger
falls due on the scaled clock, with its wall time as `at_ms`. A run stops at the first of these:

- Every request has its result, the whole stream is in, and no Thread waits for the Router.
- The deadline: by default the stream, then all its work end to end, then 30 minutes.
- The Router's next call would pass the run's cap.

Then the bench reads the run's Store into the reports.

## The paid guard

`--router pi` refuses to start without `--max-spend-usd`, before it reads the key. That sum is for the whole bench.
Each run gets an even share as `budgets.router_daily_microusd`, so its broker refuses any call past it. The bench also
stops a run once the run's whole Router spend, plus its largest hold so far, would pass the share, so a UTC midnight
cannot reopen the cap. A design whose run reached its cap skips its remaining runs.

Before it starts, the bench prints the caps and an estimate: the Candidates each design sends the Router, two calls
each, an input that grows with the session, 90% cache hits when the prices name a cache-hit rate, and 200 output tokens
a call. The workers are simulated, so the Router is the only spender.

## Results

### The free run

On remote-ai, with the default workload, seeds 1 to 3 and `--time-factor 0.01` (about 10 minutes of wall time for all
nine runs):

| Design | Completion p50 / p95 (min) | Router decisions per run | Evals per run | Unfinished |
|---|---|---|---|---|
| super-agent | 10.2 / 15.7 | 142 | 142 | 0 of 60 |
| no-pass-through | 10.2 / 15.7 | 177 | 177 | 0 of 60 |
| serial | 82.4 / 177.3 | 128 | 128 | 0 of 60 |

`super-agent` lands on the article's shape (12.4 / 15.7) with a Router that takes no time. Without pass-through,
completion is the same to the decimal, since a scripted decision is instant, and the Router decides 25% more often (the
article: about 30%). `serial` is far slower than the article's single session (20.4 / 29.5). Its 20 requests of 6 to
16 minutes arrive within about 48 minutes, so done one after another, the last waits about three hours. That is the
bound of no delegation at all, not a measure of the article's baseline. `serial` also decides less often, because the
Triggers that queue on a Thread while the Router is held are judged in one Eval. Tokens and dollars need a paid run.

### The live run

On 2026-11-03 on remote-ai, with a Router on DeepSeek V4.1 Flash (`deepseek-flash`), the default workload,
`--time-factor 1` and the shipped Router defaults (`max_output_tokens` 8,192, `max_batch` 6), after #37 and #45. The
ledger priced output at DeepSeek's peak rate.

| Design | Runs | Completion p50 / p95 (min) | Router decisions per run | Model calls per run | Cache-read tokens per run | Spend per run | Unfinished |
|---|---|---|---|---|---|---|---|
| super-agent | 2 (seeds 1, 2) | 9.7 / 15.7 | 144 | 287 | 6.7M | $0.243 | 0 of 40 |
| no-pass-through | 1 (seed 1) | 10.2 / 16.0 | 172 | 341 | 9.8M | $0.345 settled | 3 of 20 (budget) |
| serial | 1 (seed 1) | 94.0 / 177.5 | 134 | 62 | 0.93M | $0.054 settled | 0 of 20 |

`report.md` shows $0.423 and $0.105 for the last two. Those figures count the one reservation each run still held when
it ended: $0.078 and $0.051. No call in these runs failed or settled as unknown.

- **Delegation sets completion time, not the model.** On the same stream, `super-agent` finishes requests in 9.7
  minutes at p50 and `serial` in 94.0. `serial` spends less because its Router makes 62 calls: it waits while work
  runs.
- **Pass-through saves Router work at no cost in completion.** Without it, the Router made 19% more decisions and 19%
  more calls, each over a longer context (9.8M against 6.7M cache-read tokens). It settled 42% more spend, and ran into
  its $0.50 cap with three requests left. An earlier run on seed 1 (`max_output_tokens` 2,048, before #37) points the
  same way: 187 against 147 decisions, and $0.37 against $0.24, both 20 of 20.
- **Router spend is replayed context.** Output is 17k to 22k tokens a run, and cache reads are 6.7M to 9.8M, so cost
  tracks calls times context length.
- **The sample is small.** There is one `no-pass-through` run, which its budget stopped, and one `serial` run. The
  direction holds across both live benches. The size is not settled.

Before this run, the live attempts found four defects, each fixed first:

1. The generator repeated 8 of 20 tasks, and a model Router rightly ignored them as done (#37).
2. A Router prompt held every queued candidate, overflowed the output cap, and Pi asked again until the budget ran out.
3. A provider call could hang for 10 minutes, so every Eval expired.
4. A refused decision on an expired Eval did not name the open one, so the Router retried stale ids.

The last three were fixed in #45. The four attempts cost $3.52 in all.

## Not measured yet

The delegation-prompt variant, and whether the Router's decisions were right.
