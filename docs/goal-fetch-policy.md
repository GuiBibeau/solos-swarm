# Goal `fetch` policy

A Goal that grants `fetch_url` lets its agents reach the network through the broker. The Goal's `scope` is a label
and its prompt is a request, so neither one stops an agent. The optional `fetch` policy is the part the broker
enforces, on every call. Source: `FetchPolicy` in `src/broker_fetch.rs`, `Goal::validate` in `src/broker.rs`.

```json
"fetch": {
	"hosts": ["zelix-front-dev.fly.dev", "zelix-deck.vercel.app"],
	"methods": ["GET", "HEAD", "OPTIONS"],
	"per_agent_rps": 1,
	"follow_redirects_off_list": false
}
```

## Fields

| Field | Meaning |
|---|---|
| `hosts` (required, 1 to 64 entries) | The hosts a request may name. An entry is an exact name, `*.example.com` for any name under that suffix, or an IP address. A wildcard never matches the bare suffix, and its suffix needs two labels or more, so `*.com` is refused. Case and a trailing dot do not matter. A port is not part of the match. |
| `methods` | The methods allowed, from `GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE` and `OPTIONS`. Left out, every one of them is allowed. An empty list is refused. |
| `per_agent_rps` | The calls per second that one agent may make, from 0.01 to 1000. A call that follows redirects counts once. Left out, there is no limit. |
| `follow_redirects_off_list` | Default `false`: a redirect to a host that is not on the list is refused and not followed. `true` follows it, but the URL that the agent asked for must still be on the list. |
| `allow_private_networks` | Default `false`. See [Private addresses](#private-addresses). |

`Goal::validate` reads every field. It refuses an unknown key, a missing `hosts`, and a policy on a Goal without the
`fetch_url` grant, so a typo cannot leave a Goal that only looks restricted. On the host, `scripts/remote.py start`
also refuses a policy with an empty `hosts` list or without the `fetch_url` grant, before it writes the provider key to
a file.

## Refusals

The broker checks the URL that the agent asked for, then every redirect hop before it requests that hop. A refused
call sends nothing. The agent gets a tool result, not an HTTP error, so a swarm worker can read it:

```json
{"refused": true, "reason": "redirect_off_list", "error": "fetch_url refused: ... redirects to https://accounts.google.com/..., whose host ... is not on this Goal's fetch allowlist ...", "url": "https://accounts.google.com/..."}
```

| `reason` | When |
|---|---|
| `host_not_allowed` | The host is not on `hosts`. |
| `method_not_allowed` | The method is not on `methods`. |
| `redirect_off_list` | A redirect names a host that is not on `hosts`, and `follow_redirects_off_list` is `false`. |
| `rate_limited` | The agent is over `per_agent_rps`. The reply carries `retry_after_ms`. |
| `private_address` | The host resolves to a private address. See [Private addresses](#private-addresses). |
| `header_not_allowed` | The agent set its own `Host` header, which could name another site on a listed address. Under a policy the broker sets `Host` from the URL. |

Each refusal is a `fetch_refused` event in the run's `events.jsonl`. The event names the agent, the URL it asked
for, the URL refused, the hop and the reason. For a private address it also names the address. When the Goal has a
policy, `scripts/check_run.py` checks every completed `fetch_url` against it again, as the `fetch-scope` check.

## Private addresses

Every swarm `fetch_url` refuses a private address, with a policy or without one. The refused ranges are loopback,
private (RFC 1918), link-local (`169.254.0.0/16`, which holds the cloud metadata address), carrier-grade NAT and
Tailscale (`100.64.0.0/10`), multicast, reserved and documentation addresses, and an IPv6 address that wraps one of
those. The broker resolves a host name once per hop. All of its addresses must be public, and the connection is then
pinned to them, so a name that answers differently the second time cannot get past the check.

To reach a private host on purpose, such as a staging service on the same network, set `allow_private_networks: true`
in the policy. A Goal without a policy has no way to allow it: name the private host in a policy.

## Goals without a policy

A Goal without a `fetch` policy may reach any public host, with any method and no rate limit. The private-address
refusal still applies.

Agent mode's own `fetch_url` (`src/agent/broker.rs`) goes through the same guard with no policy. Its switch for
private addresses is `executor.fetch.allow_private_networks` in the agent config (default `false`), described in
[docs/agent-mode/README.md](agent-mode/README.md).

## Audit scopes

A Goal's `scope` must be on the deployment allowlist in `Goal::validate`:

- `synthetic-fixtures-only` and `public-web-research`.
- The two scopes in `AUDIT_SCOPES_BEFORE_FETCH_POLICY` (`src/broker.rs`): `authorized-audit:solana-developer-platform`
  and `authorized-audit:solana-platform-redteam`. They were frozen before the policy existed, so a Goal under one of
  them may grant `fetch_url` without a policy.
- The scopes in `AUDIT_SCOPES`: today only `authorized-audit:zelix`. A Goal under one of them that grants `fetch_url`
  must carry a `fetch` policy.

Each `authorized-audit:<target>` scope is a reviewed authorization to probe that target. All scopes share one
envelope: the deadline, spend and call limits that `Goal::validate` checks. To authorize a new target, add one line to
`AUDIT_SCOPES`.

## Goal builders

- `scripts/prepare_platform_case.py` builds the Goal for the live platform red team, with a policy. Its flags are
  `--fetch-host` (repeatable), `--fetch-methods`, `--fetch-rps` and `--allow-rpc`.
- `scripts/prepare_redteam_case.py` builds a source-review Goal. It grants no `fetch_url`, so it has no policy.
- In agent mode, a swarm Ticket's Goal template is YAML for the same `Goal`, so it sets the same `fetch:` key.
