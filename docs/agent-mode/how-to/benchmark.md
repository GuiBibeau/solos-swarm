# Run the benchmark

`solos-swarm agent bench` (`src/agent/bench/`) reproduces DeepSeek's super-agent ablation on a generated stream of chat
and CI messages. What it measures, the designs it compares, and the results so far are in
[About the benchmark](../benchmark.md).

## Write a workload, if you want your own

The default workload needs no file. To change it, write a YAML spec with the fields you want to change. The fields and
their defaults are in the [configuration reference](../configuration.md#bench-workload-spec). To see the Triggers a
spec makes:

```sh
solos-swarm agent workload [--spec FILE] [--seed N] --out triggers.jsonl
```

## Run it for free

The scripted Router (`--router scripted`, the default) decides at once and costs nothing, so a free run measures
scheduling only:

```sh
solos-swarm agent bench --design super-agent,no-pass-through,serial --runs 3 --time-factor 0.01 \
  --out bench/scripted-$(date +%F)
```

At `--time-factor 0.01` a workload minute lasts 0.6 s. A `super-agent` run takes about 35 s, and a `serial` run, whose
20 requests work one after another, about 2 minutes.

## Run it with a model Router

A paid run spends money, so get the operator's approval of the sum first. `--router pi` takes the `router` section of
`--router-config` (a `kind: pi` Router), and the usual `--pi-root`, `--node` and `--credential-file`:

```sh
solos-swarm agent bench --design super-agent,no-pass-through --runs 5 --router pi \
  --router-config agent.yaml --pi-root ~/solos-swarm --node ~/.local/bin/node \
  --credential-file /run/user/1000/router-key --max-spend-usd 5 --time-factor 1 \
  --out bench/pi-$(date +%F)
```

- `--max-spend-usd` is required. The bench refuses to start without it, before it reads the key. The sum is for the
  whole bench, split evenly over its runs.
- Before it starts, the bench prints each run's cap and an estimate of the spend.
- Use `--time-factor 1`. A model Router's latency is real, so at a smaller factor each second it takes counts as
  `1 / factor` seconds of workload time. A run then lasts about as long as its stream: a little over an hour on the
  defaults.
- `--deadline-minutes M` stops each run after M workload minutes.

How the caps work is in [The paid guard](../benchmark.md#the-paid-guard).

## Read the report

The bench writes `report.json` and `report.md` into `--out`. The first table of `report.md` is the article's, one row
per design. The second has one row per run.

- **Completion p50 / p95.** Workload minutes from a request's arrival (when the agent stored its Trigger) to the
  `ready` Report that ended its Ticket, by nearest rank over every request of every run of the design. A request with
  no result when its run stopped is counted in **Unfinished** and left out of the percentiles, so read the two
  together.
- **Router decisions.** Accepts, ignores and stops on an Eval, each a Router turn's work. The gate's decisions
  (unlisted people and bots) cost no model call and are in the run rows. **Evals** counts Evals started, so an expired
  Eval counts again.
- **Model calls, tokens and spend.** These come from the run's spend ledger: the Router's account and every Ticket's.
  They are 0 with the scripted Router. Cache-read tokens are input tokens that the provider reported as cache hits.

`report.json` has every request (arrival, expected work, completion, Ticket), each run's counts, and its runtime
summary. Each run directory keeps `agent.yaml`, `workload.jsonl`, `triggers.jsonl` as fed, `outbox.jsonl` and
`state/agent.db`, which `solos-swarm agent status --state-dir` reads.
