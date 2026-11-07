# Run workers on Claude Code

A session Ticket can run on Claude Code instead of Pi, under the same governance as a Pi worker. What each session
runs in, and what Claude Code calls on the broker, is in [Claude Code sessions](../runtime.md#claude-code-sessions).

## Install Claude Code and name it in the executor

Install Claude Code on the host: the native build, or the npm package with a node. Then name it in `executor`:

```yaml
executor:
  kind: pi                      # the Pi workers' settings still apply: provider, model, prices, limits, run_code
  provider: deepseek
  model: deepseek-v4-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}
  harness: claude-code          # the harness of a session whose Plan names none; default pi
  claude:
    binary: /home/agent/.local/share/claude/versions/2.1.231   # or the npm package's cli.js, with node:
    # node: /opt/node/bin/node
    # solos_swarm: /usr/local/bin/solos-swarm   # what the sandbox runs for tools and the bridge; default this one
```

Without `harness: claude-code`, a Plan must name `claude-code` for a Ticket to run on it. A Plan that names `pi` runs on
Pi either way.

## Start the agent

Run `agent serve` as for Pi workers, with `--pi-root`, `--node` and `--credential-file`. The Router, swarm Tickets and
a Plan that names `pi` still run on Pi, and the key stays in the serving process.

## Check it on the box

```sh
SOLOS_CLAUDE=$(readlink -f ~/.local/bin/claude) cargo test --test agent_claude_code -- --ignored --nocapture
```

The test runs the installed Claude Code as a worker in `agent serve`, keyless and offline, against a scripted upstream
that has it report `progress` and then `ready`. The Ticket ends with one acknowledgement and one result, and both model
calls settle on its account.
