# Write a Goal template for swarm Tickets

A swarm Ticket runs the Goal that its template renders. A template is a swarm Goal file, the YAML that
`solos-swarm run --config` reads, with minijinja placeholders for the Ticket. How swarm Tickets run is in
[Swarm Tickets](../runtime.md#swarm-tickets).

## Write the template

The placeholders are `ticket`, `title`, `scope`, `agents` and `budget_microusd`, which is what the Ticket has left to
spend. Every placeholder prints a quoted, escaped YAML value, so a title or scope that came from a message cannot add a
key or end a string. Use a placeholder as a whole value, and join text inside the braces with `~`.

1. Write `agent_count: {{ agents }}` and `budget_microusd: {{ budget_microusd }}`, with one task per agent in a
   `{% for n in range(agents) %}` loop. A Goal with another budget or agent count does not launch.
2. Leave out `upstream`, or name the workers' provider. The swarm's calls go through the workers' provider and
   credential, and a Goal without `upstream` gets it, as `solos-swarm run --provider` does. The Goal's `model` must be
   one that provider admits, and its prices at least the admitted rates.
3. Write the rest as in swarm mode: `scope` on the deployment allowlist, `deadline_seconds`, `max_calls_per_agent`,
   the request limits, the `grants` and the prompts. Keep `worker_prompt` and `synthesis_prompt` together within 8,192
   bytes, and put a long scope in the task inputs.

This example is `tests/fixtures/goal-template.yaml`:

```yaml
schema_version: 1
intent: {{ title }}
worker_prompt: {{ "Claim your task, then file one record for your part. Ticket " ~ ticket ~ ": " ~ scope }}
synthesis_prompt: Call read_findings, then submit_artifact with every record of every finding.
scope: synthetic-fixtures-only
model: deepseek-flash
agent_count: {{ agents }}
deadline_seconds: 600
budget_microusd: {{ budget_microusd }}
max_calls_per_agent: 16
max_input_bytes: 262144
max_output_tokens: 2048
input_price_microusd_per_million: 300000
output_price_microusd_per_million: 1200000
grants: [claim_task, board_read, board_post, submit_finding, read_findings, submit_artifact]
tasks:
{% for n in range(agents) %}  - id: {{ "part-" ~ n }}
    input: {part: {{ n }}, of: {{ agents }}, question: {{ scope }}}
{% endfor %}seed: 1
catalog_version: agent-swarm
scorer_version: "1"
```

## Give it its own words

A template that is not research can set its own `nudges` and `record_fields`. Each field it leaves out keeps the
research swarm's. A nudge's placeholders are single braces, such as `{agent_id}`, which minijinja leaves alone, and
one inside a `{{ }}` string comes out the same. Keep `title` and `scope` out of a nudge, because a brace in a message
would make the Goal invalid.

```yaml
nudges:
  explore: {{ "You are {agent_id}. Keep reading the code for " ~ ticket ~ "." }}
  file: "You are {agent_id}. File now, exactly: {call}"
record_fields: {file_from: [path], copy: [part], result_key: changes}
```

Every placeholder and rule of the two fields is in [Swarm Goal words](../architecture.md#swarm-goal-words).

## Limit what its workers fetch

A template whose workers get `fetch_url` can limit what they reach with `fetch`, which the swarm's broker enforces on
every call and every redirect hop:

```yaml
fetch: {hosts: [app.example.com, "*.example.org"], methods: [GET, HEAD], per_agent_rps: 1}
```

A host name is a plain scalar, but `*.suffix` needs quotes in YAML. The fields, the refusals and the audit scopes are
in [Goal `fetch` policy](../../goal-fetch-policy.md). A swarm's `fetch_url` refuses private, loopback and link-local
addresses unless the policy sets `allow_private_networks: true`
([Private addresses](../../goal-fetch-policy.md#private-addresses)). A session worker's
`fetch_url` has no Goal, and `executor.fetch.allow_private_networks` is its switch.

## Name the template in agent.yaml

```yaml
executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}
  swarm:
    templates: {research: goals/research.yaml}   # relative to this file
    default_template: research                   # for a swarm plan that names none
    # max_runs: 1                                # swarms at once
```

`agent serve` renders each template for one and two agents as it loads the config, and refuses one that does not make
a valid Goal.
