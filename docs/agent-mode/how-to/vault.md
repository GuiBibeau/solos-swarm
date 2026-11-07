# Give workers an API key they never see

A Pi or Claude Code worker can call an API with a key that its model never sees. The key lives in this host's vault,
and the broker adds it to requests to the key's own hosts. The model gets the response back with every vault value
replaced by `[vault:<name>]`. The executor must be `pi`. The guarantees are in
[Vault keys](../security.md#vault-keys), and the design is in [vault-browser/](../vault-browser/rationale.md).

1. Say where the vault and its key file live, outside every sandbox's view:

   ```yaml
   # agent.yaml
   vault: {path: /var/lib/solos/vault.enc, key_file: /var/lib/solos-keys/vault.key}
   ```

   `solos-swarm vault init` refuses `/usr`, `/etc`, `/lib`, `/lib64`, `run_code`'s mounts and its `uv_root`, and a
   Claude Code worker's binaries. `agent serve` also refuses to start when either file is under the state
   directory's `tickets/`, `--pi-root` or the install directory of `--node`.

2. Make the vault once:

   ```sh
   solos-swarm vault init --config agent.yaml
   ```

3. Add a key, bound to its host and to the header that carries it. The value comes on stdin or from a file that is
   deleted after it is read, never as an argument:

   ```sh
   solos-swarm vault add-key --config agent.yaml --name openai --host api.openai.com \
       --header authorization --scheme bearer --about "OpenAI API for research tickets" \
       --for slack:U024BE7LH --value-file /run/secrets/openai
   solos-swarm vault list --config agent.yaml
   ```

   - `--for` names whose Tickets may use the key, and they are the only people asked to approve it. Without it, any
     requester's Ticket may use it, and the Policy's approvers answer.
   - `--approval first-use`, the default, has a person allow the key once per Ticket. `--approval none` is for
     unattended use, such as a schedule's.
   - A first-use item needs an agent that takes approvals: a Slack or Discord channel, and a `slack:` or `discord:`
     approver in the Policy. Without one, its use is refused.
   - Change a value with `--replace`, and remove an item with `vault remove`. A write while the agent serves counts
     from the next call.

4. Restart `agent serve`. Workers now see `vault_list`, and their `fetch_url` takes `key`:

   ```text
   → vault_list {}
   ← {"keys":[{"key":"openai","hosts":["api.openai.com"],"about":"OpenAI API for research tickets",
      "where":"local","use":["fetch_url"],"approval":"needed"}], "logins":[], "note":"You never see these values. …"}
   → fetch_url {"url":"https://api.openai.com/v1/models","key":"openai"}
   ✗ 409 "A person must allow this ticket to send the API key openai to api.openai.com first; they were asked in
          the thread. End your turn: their answer comes back as new input."
   ```

5. Answer the approval. The Ticket's thread gets one message, "Triage the papers wants to send the API key openai to
   api.openai.com for the first time.", with **Allow for this ticket** and **Deny**. Only a click answers. Typing
   "allow" in the thread reaches the worker as text and grants nothing. The answer comes back to the worker as new
   input, and its next call goes through:

   ```text
   → fetch_url {"url":"https://api.openai.com/v1/models","key":"openai"}
   ← {"final_url":"https://api.openai.com/v1/models","status":200,"headers":{…},"body":"{\"data\":[…]}", …}
   ```

The grant holds for the Ticket's life, across its Assignments, and so does a Deny.

A key for a service on this host's own network, loopback included, needs `executor.fetch.allow_private_networks`. A
keyed call is also a `fetch_url` call, so a request whose host resolves to a private address is refused before the key
is put on it. What the broker does with a keyed call, from the chain entry to the scrubbing, is in
[Vault keys](../security.md#vault-keys).
