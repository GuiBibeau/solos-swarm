# Use an API key the model never sees

An operator stores an API key in the vault, bound to its host. A worker asks for it by name (`fetch_url` with `key`).
The broker records `key_used` on the chain, adds the key to requests to that host only, and the model gets the
response without ever seeing the key.

## Sub-features

- `vault-cli`: `vault init`, `vault add-key` (value from a file deleted after reading) and `vault list` (no value shown).
- `keyed-call`: a request authenticated with the key reaches its host with the right token.
- `key-used`: the chain records `key_used` with the key and the URL before the request is sent.
- `never-seen`: the key's value never reaches the model or its reply.

## How to get to it (user POV)

- The operator: `solos-swarm vault add-key --config agent.yaml --name <name> --host <host> --header authorization --scheme bearer --approval none|first-use --value-file <file>`.
- A person asking for work that needs the API. The worker discovers keys with `vault_list`.

## Driving it with bin/mcp.sh

Preconditions: a run launched with `overlays/vault.yaml` (so `launch.sh` ran `vault init`), and a clean doctor. A loopback "account API" that returns a random account name only when the bearer token is right:

```sh
ssh remote-ai 'bash -s' <<EOF
cd ~/work/verify/$RUN
rm -f probe.log
port=18\$((RANDOM % 900 + 100)); echo \$port > secrets/probe.port
python3 -c 'import secrets; print("vtest-" + secrets.token_hex(16), end="")' > secrets/probe.value
python3 -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1]).read().encode()).hexdigest())' secrets/probe.value > secrets/probe.value.sha256
python3 -c 'import secrets; print("acct-" + secrets.token_hex(4), end="")' > secrets/probe.account
setsid -f bash -c 'echo \$\$ > probe.pid; exec timeout 1500 python3 src/.claude/skills/verify-agent-mode/bin/key_probe_server.py '\$port' secrets/probe.value.sha256 secrets/probe.account probe.log 1400' > /dev/null 2>&1 < /dev/null
sleep 1; curl -s -o /dev/null -w "without a key: %{http_code}\n" http://127.0.0.1:\$port/v1/me
bin/solos-swarm vault add-key --config agent.yaml --name probe --host 127.0.0.1:\$port --header authorization --scheme bearer --about "the verification account API" --approval none --value-file secrets/probe.value
bin/solos-swarm vault list --config agent.yaml
EOF
```

- `vault-cli`: the output shows `without a key: 401` and `saved probe`, `secrets/probe.value` is gone, and `vault list` shows a row that starts `KEY  probe  127.0.0.1:<port>  authorization: Bearer …`, with no value.
- `keyed-call`: `bin/mcp.sh $RUN request "Which account does our probe API key belong to? Call our probe API at http://127.0.0.1:<port>/v1/me with fetch_url using the vault key probe, and tell me the account name it returns."`, then `replies <thread> result 540`. The result names the account in `secrets/probe.account`, and `probe.log` has a `200` line with `bearer_token_sent: true`: the probe answers 200 only when the token's sha256 matches.
- `key-used`: `bin/evidence.sh $RUN keyed-fetch`. `events.jsonl` holds `key_used` with the secret `local/probe` and the URL, before the worker's `reported`.
- `never-seen`: the probe never echoes a header, so the account name is the proof of the call, and no test value may appear anywhere the model writes or reads: `ssh remote-ai "grep -rl vtest- ~/work/verify/$RUN/state ~/work/verify/$RUN/evidence"` prints nothing (the value lives only in the vault and as a hash in `secrets/`).

## Gotchas

- Phrase the request as using the key, never as reporting what the server received. On 2026-11-04 the Router twice ignored "tell me exactly what the server reports it received in the authorization header" as a request to disclose a credential, and its session remembered the first refusal. A relaunch gives it a fresh session.
- MCP cannot click an approval, so the probe key is `--approval none`. First-use approval is driven on Discord (discord-chat.md).
- The vault admits plain http only on loopback. Any other host must be https.
- `cleanup.sh` stops the probe by `probe.pid`. A probe started without the pid file exits on its own after 1400 s.
