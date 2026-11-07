# Agent mode

Agent mode is solos-swarm's second mode: a long-lived agent that lives in Slack, Discord, GitHub and schedules. It takes
requests, notices work, runs it on isolated workers, and answers in the thread the request came from. Swarm mode
(`solos-swarm run`) stays, and it is also one way agent mode can run a Ticket. Both modes build from one workspace into
one binary, and run on the same broker code, ledger, harness, sandbox and audit log. The decision is recorded in
[ADR 0001](../adr/0001-hexagonal-ports-and-two-modes.md).

```sh
solos-swarm agent serve --config agent.yaml --state-dir DIR
```

The reference design is DeepSeek's *Exploring Super-agent Design: Towards a More Autonomous Agent System* (November
2026), and its MIT-licensed prototype `deepseek-ai/dsh-superagent-prototype`.

## Status

Phases P0 to P11 are built, and so is the first build of P12, the vault and the verifiable browser. Three phases are
being built from their own PR plans:

- P13, pstack as the coding method: PRs 1 and 2 of 7.
- P14, the agent's own GitHub App: PRs 1 and 2 of 6. The agent reads, comments and delivers pull requests as its bot.
  CI repair and merging are not built.
- P15, memory: PRs 1 to 4 of 5.

[status.md](status.md) has each phase with its pull requests, the changes outside the phases, and the steps still to
do.

## Understand it

- [Agent mode concepts](concepts.md): the super-agent idea, the vocabulary, how a request flows, and how budgets apply.
- [How the agent runs work](runtime.md): the Engine, the Router, workers and their loop guard, background jobs,
  swarm Tickets, Claude Code sessions, remote nodes, the browser, the Watchdog, and memory.
- [Sources and channels](channels.md): how the agent talks, and how Slack, Discord, GitHub, schedules and MCP behave.
- [How repository Tickets work](repository-tickets.md): clones, delivering a pull request, installed dependencies and
  evidence.
- [Security model](security.md): what untrusted text can and cannot do, and the open risks.
- [Agent mode architecture](architecture.md): for contributors. The ports, the Store, the agent broker, the tool
  surface and what both modes share.
- [About the benchmark](benchmark.md): the ablation, its designs, and its results.

## Set up and operate

- [Connect Slack](how-to/slack.md)
- [Connect Discord](how-to/discord.md)
- [Set up the GitHub App](how-to/github-app.md)
- [Let workers work in a repository](how-to/repositories.md)
- [Hand the agent work from an MCP host](how-to/mcp.md)
- [Run workers on Claude Code](how-to/claude-code.md)
- [Write a Goal template for swarm Tickets](how-to/goal-templates.md)
- [Run commands on your own machine](how-to/nodes.md)
- [Give workers an API key they never see](how-to/vault.md)
- [Give workers a browser](how-to/browser.md), and check what it did
- [Manage the agent's memory](how-to/memory.md)
- [Watch and control a running agent](how-to/operate.md)
- [Run the benchmark](how-to/benchmark.md)
- [Running agent mode in production](production.md): the box, the systemd units, deploys, the dashboard and alerts.

## Look it up

- [Configuration reference](configuration.md): every section of agent.yaml, node.yaml, and the bench workload spec.
- [status.md](status.md): what each phase built.

## Design records

Each folder holds the design of one feature and its PR plan:

- [github-app/](github-app/rationale.md): the agent's own GitHub App.
- [memory/](memory/design.md): memos, the review and the dream.
- [pstack/](pstack/rationale.md): pstack as the coding method.
- [vault-browser/](vault-browser/rationale.md): the vault and the verifiable browser.

## Where the old sections went

This README used to hold the whole plan. Links to its old sections still land here, and this table says where each one
moved.

| Old section | Now |
|---|---|
| <a id="later-steps"></a>Later steps, <a id="roadmap"></a>Roadmap, <a id="open-decisions"></a>Open decisions | [status.md](status.md) |
| <a id="what-super-agent-means-here"></a>What "super-agent" means here, <a id="concepts"></a>Concepts | [concepts.md](concepts.md) |
| <a id="budgets"></a>Budgets | [concepts.md#budgets](concepts.md#budgets) |
| <a id="non-goals"></a>Non-goals | [concepts.md#non-goals](concepts.md#non-goals) |
| <a id="architecture"></a>Architecture, <a id="dependency-rules"></a>Dependency rules, <a id="ports"></a>Ports, <a id="what-both-modes-share"></a>What both modes share | [architecture.md](architecture.md) |
| <a id="memory"></a>Memory | [runtime.md#memory](runtime.md#memory), and [how-to/memory.md](how-to/memory.md) |
| <a id="the-dream"></a>The dream | [runtime.md#the-dream](runtime.md#the-dream) |
| <a id="slack-app-setup"></a>Slack app setup | [how-to/slack.md](how-to/slack.md) |
| <a id="discord-app-setup"></a>Discord app setup | [how-to/discord.md](how-to/discord.md), and [channels.md#discord](channels.md#discord) |
| <a id="github-app"></a>GitHub App, <a id="create-and-install-the-app-by-hand-once"></a>Create and install the App, <a id="give-the-agent-its-key"></a>Give the agent its key, <a id="configure-it"></a>Configure it | [how-to/github-app.md](how-to/github-app.md) |
| <a id="what-happens"></a>What happens | [channels.md#github](channels.md#github) |
| <a id="moving-from-token-files"></a>Moving from token files | [how-to/github-app.md#move-from-token-files](how-to/github-app.md#move-from-token-files) |
| <a id="claude-code-workers"></a>Claude Code workers | [how-to/claude-code.md](how-to/claude-code.md), and [runtime.md#claude-code-sessions](runtime.md#claude-code-sessions) |
| <a id="goal-templates"></a>Goal templates | [how-to/goal-templates.md](how-to/goal-templates.md) |
| <a id="how-the-agent-talks"></a>How the agent talks | [channels.md#how-the-agent-talks](channels.md#how-the-agent-talks) |
| <a id="watchdog"></a>Watchdog | [how-to/operate.md](how-to/operate.md), and [runtime.md#the-watchdog](runtime.md#the-watchdog) |
| <a id="web-dashboard"></a>Web dashboard | [configuration.md#web](configuration.md#web), and [production.md#dashboard](production.md#dashboard) |
| <a id="mcp-hosts"></a>MCP hosts | [how-to/mcp.md](how-to/mcp.md) |
| <a id="remote-nodes"></a>Remote nodes | [how-to/nodes.md](how-to/nodes.md), and [runtime.md#remote-nodes](runtime.md#remote-nodes) |
| <a id="vault-and-api-keys"></a>Vault and API keys | [how-to/vault.md](how-to/vault.md) |
| <a id="browser"></a>Browser | [how-to/browser.md](how-to/browser.md), and [runtime.md#the-browser](runtime.md#the-browser) |
| <a id="check-a-trail"></a>Check a trail | [how-to/browser.md#check-a-trail](how-to/browser.md#check-a-trail) |
| <a id="repositories"></a>Repositories | [how-to/repositories.md](how-to/repositories.md), and [repository-tickets.md](repository-tickets.md) |
| <a id="delivering-a-pull-request"></a>Delivering a pull request | [repository-tickets.md#delivering-a-pull-request](repository-tickets.md#delivering-a-pull-request) |
| <a id="installed-dependencies-and-evidence"></a>Installed dependencies and evidence | [how-to/repositories.md#install-dependencies-and-require-evidence](how-to/repositories.md#install-dependencies-and-require-evidence), and [repository-tickets.md#installed-dependencies](repository-tickets.md#installed-dependencies) |
| <a id="security-model"></a>Security model | [security.md](security.md) |
