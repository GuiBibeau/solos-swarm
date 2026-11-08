# Agent mode security model

An agent that acts on events reads far more untrusted text than a batch run does: chat messages, issues, web pages and
tool output. This page explains what that text can and cannot do, what each sandbox holds, and which risks are still
open. The open items are marked **Open**.

## Text grants nothing

- Trigger text is data. It cannot change a permission, a budget or the Policy: the core has no path for it.
- An approval is an adapter command, authenticated by the adapter, never text. A Slack button click arrives on the
  socket the agent opened with its app token, and Slack names the user. A Discord button click arrives on the Gateway
  connection the agent opened, and Discord names the user.
- A stop is either an adapter command (`stop_ticket`, open to approvers and allowed people) or a Router
  `Verdict::Stop` on an Eval of explicit strength.
- Only explicit strength stops work. Bots and CI can at most make inferred candidates, and those start nothing outside
  `auto` mode with a matching enabled rule.

## Sessions are keyless and sandboxed

- Workers stay keyless: the broker holds the model key, and tool credentials on a node stay on the node. A swarm's
  agents are keyless too, behind the swarm's own broker. The Goal they run is the operator's template: Trigger text
  reaches it only as escaped values, and its budget is what the Ticket has left.
- The event-driven surface is larger than a batch run's. The Router and workers keep the `bwrap` sandbox, workers get no
  network except through broker tools, and the Router gets no `run_code`.
- A Claude Code worker holds its broker token, as a Pi worker does, and no provider key. Its built-in tools are off,
  only its grants are allowed, project and local settings are not read, and its loopback bridge adds nothing to what
  the broker checks.
- `fetch_url` reaches the public internet only, unless `executor.fetch.allow_private_networks` is on
  ([What fetch_url can reach](runtime.md#what-fetch_url-can-reach)).

## The host's own user

A worker that escapes its sandbox, and anything the host runs on a repository's behalf (`git`, the dependency install),
runs as the agent's user, with whatever that user can do. In production two things limit it. The units take away
privilege escalation (`NoNewPrivileges`), dangerous system calls and socket families, and every write outside
`~/agent/state` ([Hardening the units](production.md#hardening-the-units)). And the agent should run as a user of its own,
with no sudo, no extra groups and no other services, so that there is nothing else to reach
([Run the agent as its own user](production.md#run-the-agent-as-its-own-user)). The units limit what the user's
processes do; only a dedicated user limits what the user owns.

## Operators over MCP

`solos-swarm mcp --socket` adds no authority: the broker checks every call against the token's grants. With
`--state-dir`, the operator name is not authenticated beyond file access. Whoever can run it with access to the state
directory can file requests under any name, just as anyone who can write the state directory can already do anything.
Treat `mcp:` entries in `policy.people` as local operators.

## External jobs

`watchdog.jobs` makes the agent's host run commands, outside every sandbox, as the agent's user, once a look. What runs
is only what the owner wrote in agent.yaml. A unit or session name is validated before it reaches `systemctl` or `tmux`,
and a status `command` is a list run with no shell, so no Trigger text and no model output is ever part of a command
line. Each program gets no input, a short environment without the agent's credentials, a timeout that kills its whole
process group, and 4 KB of output that reaches only the alert and the dashboard, as one clipped line.

The command itself is as trusted as the agent's own binary: it can do anything the agent's user can. Anyone who can
write agent.yaml can run a command as that user, as they could already by changing `executor`.

## Remote nodes

- Each side pins the other's certificate, with no CA, and the agent admits a node only under the name its certificate
  is pinned for. A node opens no port, and takes commands only on the connection it dialed.
- A node's commands run in its own `bwrap` sandbox with no network.
- A node's secrets are read on the node. They reach the sandbox through bwrap's inherited environment (not its argv,
  which other local users can read) or as read-only files, and are redacted from everything the node sends, by exact
  match.
- **Open.** The article's own caveat applies. A compromised agent host can run any command on every connected node,
  inside that node's sandbox, with its mounts and its secrets. The values never leave the node, but a command can use
  them, such as an API call with a token. Per-node capability leases (the `mvp` `DispatchPermit` design) are a later
  step. Until then, connect only machines whose mounts and secrets you would hand to the agent host.

## Vault keys

- A worker names a vault key and never sees its value.
- The broker writes `KeyUsed` (the key, the method and the first URL) to the chain before the request leaves.
- The broker sends the key only to the key's hosts, over https or to loopback, and never through an ambient proxy
  (`HTTP_PROXY`, `HTTPS_PROXY` or `ALL_PROXY`). On a redirect, it sends the key only when that hop's own URL is one of
  the key's hosts.
- A key whose host is on a private network, loopback included, is sent there only under
  `executor.fetch.allow_private_networks`. `fetch_url`'s refusal of private addresses comes first, and the key is never
  put on a request it refuses.
- The response headers, the body and any error text are scrubbed of every vault value, in each encoding the scrubber
  knows, before the model sees them. That catches a value the server echoes in a known encoding, but not one it
  transforms any other way.
- A first use that needs a person waits for a click, never for text, and is refused at once on a Ticket whose channels
  cannot show the buttons.
- The vault file and its key are refused wherever a sandbox can read them.

## The browser

- A worker drives the browser only through its four tools, never over raw CDP. No code outside `browse` can send a CDP
  command, and its client has no `Runtime.evaluate`.
- The browser runs in its own `bwrap` sandbox with the host network, private networks blocked, frames off and a V8 heap
  cap. Its trail key, its sockets and its blobs are refused wherever a sandbox can read them.
- A login is filled only into a password input of the page's own document, on one of the login's sites, in a form that
  posts to one of them. The origin the browser engine reports decides, never the model or the page.
- Everything the browser returns is scrubbed of every vault value in each encoding the scrubber knows, Markdown escapes
  included, and it takes no screenshot.
- Page text is data, and it cannot approve anything, because only a click answers.
- Every browser step is asked on the chain before the browser acts, and answered with the step signed by the machine
  that ran it, checked under the key pinned at start. An answer that does not verify is recorded as lost and never
  shown. `solos-swarm agent trail` checks the same trail later with public keys only, so a copy of `agent.db` and
  `trail/` can be checked on any machine.
- **Open.** Nothing anchors the chain's head outside `agent.db`, so a trail can still be cut at a clean session
  boundary without detection.

The design is in [`vault-browser/`](vault-browser/rationale.md).

## Memory

- A memo is data in every prompt it reaches. Each line is `>`-quoted and labelled with its trust. No memo can grant a
  permission or change a budget, a tool or a rule: the core never reads memo text for a decision.
- A worker writes no memo. It proposes one, with quotes that the broker finds or refuses, and the core derives its
  trust from where the quotes came from. A person's whole message, quoted as the memo, is the only worker proposal that
  is active at once. A part of a message is not, since a person can quote someone else's words.
- Outside text never becomes an active memo without the owner. A stranger's or a bot's words, a page, a file, and any
  tool's answer, a command's output included, make a memo `external`. The Engine refuses to activate an `external`
  memo by any review decision, and the broker never sends one to the review. It waits, out of every prompt, until the
  owner writes it as their own with `agent memo add`, or it expires after 7 days.
- A `model` proposal waits for the review: a separate model call that sees the whole message a quote was cut from, is
  told to reject instruction-shaped memos and what outside text asks for, and whose answer the host checks before the
  Engine applies it. A review that fails changes nothing.
- **Open.** The review is a model, so a person's message that quotes hostile text, or a worker's words that repeat a
  hostile page, can still be kept as a `model` memo. `agent memo retire` and `forget` take it back.
- The dream reads transcripts, which hold outside text. So it is told that text is data, holds no network tool, and
  can change memory only by citing lines that the host finds in the original transcripts by digest. What it stages is
  a copy, so editing the copy gains nothing. A line that is a tool's answer or the prompt is `external`: it retires no
  memo, and a correction that cites it is never reviewed. Its proposals go through the review, and it can never
  supersede or retire a person's memo.
- **Open.** The dream's `memo_retire` with `duplicate_of` cites no line. It retires a `model` or `external` memo in
  favour of another active memo of the same scope, which a hostile transcript could talk the dream into.
- Only the host makes a Trigger of the Source `dream`, and only the host judges one. The Router never sees it.

## GitHub

- The App never holds `workflows`, so GitHub refuses a push that edits `.github/workflows/**`, and never holds
  `administration`, so it cannot touch branch protection.
- Each token is minted for one repository and the access the call needs, so a read path cannot write, and a
  repository outside `forge.repos` gets no token.
- No token reaches a model, a sandbox or a clone's config. The host admits a worker's commits from a bundle in its own
  mirror, never in the worker's tree ([Delivering a pull request](repository-tickets.md#delivering-a-pull-request)).
