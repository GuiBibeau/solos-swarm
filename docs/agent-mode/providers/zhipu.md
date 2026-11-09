# Zhipu AI

Zhipu AI (Z.ai) has two admitted providers: `zhipu`, the pay-as-you-go API, and `zhipu-coding`, the GLM Coding Plan.
A repository may list either in `models.allow`, and an `executor.upstreams` entry may run its workers on it. Both are
admitted upstream; only `anthropic` remains policy-only.

The chat endpoints are `https://api.z.ai/api/paas/v4/chat/completions` (`zhipu`) and
`https://api.z.ai/api/coding/paas/v4/chat/completions` (`zhipu-coding`), called with a Bearer key. They are not
interchangeable: Z.ai refuses a Coding Plan key on the pay-as-you-go endpoint with error `1113 Insufficient balance
or no resource package`, and a pay-as-you-go key buys nothing on the plan's endpoint. Name the provider that matches
the key.

## Models and prices

`zhipu_model()` admits exactly three model ids. All three take a 1,000,000-token context and return at most 128K
output tokens; the API's `max_tokens` ceiling is 131072.

| Model | What it is | Input | Cached input | Output |
|---|---|---|---|---|
| `glm-5.3` | text | $1.40 | $0.26 | $4.40 |
| `glm-5.3-flash` | multimodal | $0.15 | $0.03 | $0.50 |
| `glm-5.3-flashx` | a faster flash | $0.37 | $0.075 | $1.25 |

Prices are per million tokens, verified in Z.ai's docs on 2026-11-09. Z.ai lists cache storage as limited-time
free, so there is no cache-write price.

## Thinking

GLM-5.3 always thinks. It rejects `thinking: {type: disabled}`, so a Zhipu request never carries a `thinking` field.
When a session wanted thinking off, the gateway sends `reasoning_effort: "low"`; otherwise it clamps the effort to
`low`, `high` or `max`. Z.ai's default is `max`.

## Run it on an upstream

An `executor.upstreams` entry named `zhipu-flash` runs a repository's Tickets on GLM-5.3 Flash. `models.worker`
selects it, and `models.allow` must include the provider it names for the worker role. The example uses
`zhipu-coding`, for a GLM Coding Plan key; with a pay-as-you-go key, write `provider: zhipu` and allow `zhipu`.

```yaml
executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 30000}
  upstreams:
    zhipu-flash:
      provider: zhipu-coding
      model: glm-5.3-flash
      wire: chat
      harness: pi
      key_file: /run/user/1000/solos-agent/zhipu.key   # one-use: read once, then deleted
      prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000, cache_hit_microusd_per_million: 30000}
forge:
  kind: github
  repos:
    GuiBibeau/solos-swarm:
      about: solos-swarm, the agent's own code (Rust and TypeScript).
      base: main
      models: {allow: [deepseek, zhipu-coding], worker: zhipu-flash}
```

The input and output rates are above Zhipu's published rates. `PriceTable::validate` refuses input below 300000
microusd per million and output below 1200000, and requires a cache-hit rate between 6000 and the input rate, so an
admitted table cannot use GLM-5.3 Flash's $0.15 input and $0.50 output. The hold therefore overstates each call.
Settlement uses the same table, so the ledger records more than Zhipu bills. The cache-hit rate, 30000, is the
published $0.03.

A `claude-code` harness may run on the upstream too. Keep `wire: chat`: the bridge translates Claude Code's Messages
calls onto chat completions, and no Anthropic Messages wire is shipped for either Zhipu provider (`wire: anthropic`
is refused at config load). `executor.claude` still names the `claude` binary.

## Claude Code against Z.ai

Claude Code can also use Z.ai directly, without solos-swarm, through Z.ai's Anthropic-compatible endpoint:

```sh
export ANTHROPIC_AUTH_TOKEN=<your Z.ai API key>
export ANTHROPIC_BASE_URL=https://api.z.ai/api/anthropic
export ANTHROPIC_DEFAULT_OPUS_MODEL='glm-5.3[1m]'
export ANTHROPIC_DEFAULT_SONNET_MODEL='glm-5.3[1m]'
export ANTHROPIC_DEFAULT_HAIKU_MODEL='glm-5.3-flash[1m]'
export CLAUDE_CODE_AUTO_COMPACT_WINDOW=1000000
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
export API_TIMEOUT_MS=3000000
```

Do not also set `ANTHROPIC_API_KEY`. With both keys set, an auth error looks like a bad key. Z.ai's guide writes the
1,000,000-token context as `glm-5.3[1m]` and `glm-5.3-flash[1m]`, paired with `CLAUDE_CODE_AUTO_COMPACT_WINDOW=1000000`.

## Pi's models.json

Pi names Z.ai under the provider key `zai`, not `zhipu`. The key can be a shell command, so the secret stays in Pi's
auth file. A Coding Plan key needs the plan's base URL, `https://api.z.ai/api/coding/paas/v4`; the direct API,
`https://api.z.ai/api/paas/v4`, returns error 1113 for one. Every model takes `contextWindow: 1000000`, `maxTokens: 131072`, `reasoning: true`, and a
`thinkingLevelMap` that maps `off` to `low`, since GLM-5.3 cannot stop thinking.

```json
{
  "providers": {
    "zai": {
      "baseUrl": "https://api.z.ai/api/coding/paas/v4",
      "api": "openai-completions",
      "apiKey": "!jq -r '.zai.key' /home/you/.pi/agent/auth.json",
      "compat": {
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": true,
        "supportsStore": false,
        "maxTokensField": "max_tokens"
      },
      "models": [
        {"id": "glm-5.3-flash", "contextWindow": 1000000, "maxTokens": 131072,
         "reasoning": true, "thinkingLevelMap": {"off": "low"},
         "cost": {"input": 0.15, "output": 0.50, "cacheRead": 0.03, "cacheWrite": 0}}
      ]
    }
  }
}
```

The costs are dollars per million tokens, and `cacheWrite` is 0 because cache storage is limited-time free. Add
`glm-5.3` and `glm-5.3-flashx` with the same fields and their own `id` and `cost`; replace the auth file path with
your own.
