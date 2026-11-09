# Adopt agent mode

This page takes a new deployment from a clone to its first Ticket, with no maintainer-specific value in it. The
production runbook is [Running agent mode in production](../production.md): the systemd user units,
`scripts/agent-deploy`, deploys and rollbacks, the dashboard, health checks and pruning.
[The configuration reference](../configuration.md) is the field-by-field lookup for `agent.yaml`.

## What you need

- Linux, with `/usr/bin/bwrap` and `/usr/bin/prlimit`. The Pi sessions and the sandboxes need both.
- Rust 1.96.0.
- Node 24.18.0.
- A key for one of the shipped providers. This page uses DeepSeek, which the example's `deepseek-flash` names.
- A GitHub account, and a repository you can install a GitHub App on.

## Build

In a clone of this repository:

```sh
cargo build --locked --release
npm ci --ignore-scripts --no-audit --no-fund
```

The binary is `target/release/solos-swarm`. `npm ci` installs the pinned Pi coding agent into `node_modules`;
`--pi-root` below points at this checkout, which also holds the `extensions` the sessions load.

## Optional: a node

A single-host deployment skips this. To run workers' commands on another machine, make the agent's certificate once
on the agent's host, and note the fingerprint it prints:

```sh
./target/release/solos-swarm agent init-nodes --dir ~/agent/node-tls
```

Each node pins that fingerprint as `agent.fingerprint` in its `node.yaml`, and `agent.yaml` names the certificate and
key under `nodes:`. [Run commands on your own machine](nodes.md) covers the rest.

## Store the model key as a one-use token

`agent serve` reads `--credential-file` once and deletes it. Keep the durable key in a file only your user can read,
and give each start its own copy:

```sh
install -d -m 700 ~/agent/tokens
printf '%s' '<your provider key>' > ~/agent/tokens/model.key
chmod 600 ~/agent/tokens/model.key
```

Do not pass `~/agent/tokens/model.key` itself to `--credential-file`: it is gone after the first start. The production
unit copies it into a tmpfs runtime directory in `ExecStartPre` for the same reason
([Store a token](../production.md#store-a-token)).

The other credentials are one-use too: the Discord bot token, the GitHub App's private key, and the Vercel Blob token
when the dashboard is on. Keep the durable copies in `~/agent/tokens/`, mode 0600, and place a copy at each path
`agent.yaml` names before a start.

## Write agent.yaml

Keep `agent.yaml` outside the checkout you pass to `--pi-root`: `agent serve` refuses a GitHub key under any path a
sandbox can read, and it counts the whole of `--pi-root`'s tree as one. For this page, the config lives in `~/agent/`:

```sh
mkdir -p ~/agent
cp agent.yaml.example ~/agent/agent.yaml
```

Open it and fill every `<placeholder>`: the GitHub login and Discord id in `policy.people` and `policy.approvers`, the
Discord guild id on the source, the App's numeric id and key path under `github`, and the two repositories under
`forge.repos`. Remove the Discord Source and Channel together if you do not use Discord. If you do not turn the
dashboard chat on, also drop `web:you@example.com` from `policy.people` and `policy.approvers`, and remove
`answer_sources: [web]`. For a provider other than DeepSeek, change `router.provider`, `executor.provider`, both
`model` values and each repository's `models.allow`.

The prices are the admitted floor: what the ledger settles a million input and output tokens at, in microUSD. Raise
them to what your provider charges. There is no `budgets:` section, so nothing is capped; to cap a session Ticket and
the Router's day, add
`budgets: {session_microusd: <microUSD>, router_daily_microusd: <microUSD>}`
([Budgets](../concepts.md#budgets)).

## The GitHub App

Create and install the App once, by hand, as [Set up the GitHub App](github-app.md#create-and-install-the-app) says.
Note its App ID, store its private key as `~/agent/tokens/github-app.pem` (mode 0600), and install it on every
repository in `forge.repos`. The agent mints a token for one of them at a time and for none outside that list.

Check the App before the first start:

```sh
./target/release/solos-swarm agent github check --config ~/agent/agent.yaml \
    --key-file ~/agent/tokens/github-app.pem --state-dir ~/agent/state
```

It ends with `ok`, or exits 1 and says what to fix. `--key-file` names the durable copy, because `serve` deletes the
one at `github.key_file`.

## The first `agent serve`

Place the token copies the config names, then start the agent:

```sh
install -d -m 700 ~/agent/secrets ~/agent/runtime
install -m 600 ~/agent/tokens/model.key ~/agent/runtime/model.key
install -m 600 ~/agent/tokens/github-app.pem ~/agent/secrets/github-app.pem
install -m 600 ~/agent/tokens/discord-token ~/agent/secrets/discord-token   # only with the Discord Source
./target/release/solos-swarm agent serve \
    --config ~/agent/agent.yaml \
    --state-dir ~/agent/state \
    --pi-root "$PWD" \
    --node "$(command -v node)" \
    --credential-file ~/agent/runtime/model.key
```

`--pi-root` is the checkout with `node_modules` and `extensions`; `--state-dir` holds the SQLite Store and is created
if missing. The first start also creates `~/agent/state/mcp/`, so an MCP host can file requests ([Hand the agent work
from an MCP host](mcp.md)). Leave it running and watch its log; Ctrl-C stops it.

If the start fails, read the sentence: every config field is checked at load, and an unknown one or a typo names
itself.

## The first tagged issue

On one repository in `forge.repos`, open an issue whose text tags the App's bot. The App's slug is on its settings
page; its login is `<slug>[bot]`, and a mention is `@<slug>` or `@<slug>[bot]`, any case. With the GitHub CLI:

```sh
gh issue create -R <owner>/<name> --title "@<slug> what is the first line of README.md?"
```

The `github` Source polls each repository every `github.poll_secs` (60 by default), so within about a minute the agent
comments on the issue as `<slug>[bot]`. The first repository in the example owes `answer`, so it answers and
changes nothing. The second owes `pull_request`, so a request for a change there ends as a pull request on
`solos/<ticket>/<slug>` ([Let workers work in a repository](repositories.md)).

A request from Discord or from an MCP host works the same way through its Channel. `policy.people` must name the
principal, or the agent refuses it.

## Current limits

These are the edges of this build, not settings to find:

- The shipped providers are `deepseek`, `venice`, `zhipu` and `zhipu-coding`. `anthropic` may be named in a
  repository's `models.allow`, but no upstream runs on it yet: it is policy-only.
- A named upstream may be `provider: openai-compatible` with its own `base_url` and `models` catalog, but the
  broker still holds one transport, the executor's default provider, so a Ticket on such an upstream is refused
  today. Use a shipped provider until the per-upstream transport map lands.
- The forge is GitHub-only. (`forge.kind: local` exists for tests and dry runs against bare repositories on disk, not
  for a deployment.)
- The hosted dashboard is a Vercel app: it reads a private Vercel Blob store and signs people in with Sign in with
  Vercel. The web chat writes through that app, so it needs the dashboard, `web.publish` and `web.chat`; without them
  the agent is reachable on its other Sources only.
- Deployment assumes systemd user units on one Linux host, as [production.md](../production.md) describes. One
  `agent serve` holds the Store.

## Where to go next

[Running agent mode in production](../production.md) is the runbook. [The configuration reference](../configuration.md)
lists every field of `agent.yaml`, and [the agent-mode index](../README.md) links the guide for each Source, the vault,
the browser, workers on Claude Code, memory and the benchmark.
