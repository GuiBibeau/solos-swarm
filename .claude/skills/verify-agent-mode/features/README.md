# Agent mode verification map

The maintained source for verifying agent mode's user-facing behavior. Read the index, then use the matching feature
file as the recipe. The skill's [SKILL.md](../SKILL.md) holds Launch, Doctor, Drive, Evidence and Cleanup.

## Baseline preconditions

- A run shipped and launched with `bin/ship.sh` and `bin/launch.sh`, with the overlays the feature names.
- `bin/doctor.sh <run>` prints only `ok` lines.
- The operator approved the session's spend cap. A real Router and real workers spend money on every drive.
- Never drive an agent this verification did not launch, such as the operator's own Discord agent.

## Driving conventions

- Act as the person `mcp:verifier` through `bin/mcp.sh`. Treat every command as literal.
- Phrase each request as the work a person would ask for. The Router judges intent: it ignores a request that reads
  as moving a credential to an unknown place, even when the place is our own test server.
- One feature per launch when the feature needs an overlay. Snapshot evidence before relaunching.

## Proof and skip reporting

- Pair the user action (request text, thread id) with the observable result (the reply) and an independent check
  the model could not invent (`gh`, a server log, a chain event, `agent trail`'s exit status).
- Record the feature id and run id with every artifact (`bin/evidence.sh <run> <feature-id>`).
- Report an unreachable path with the attempted command and the unmet precondition. A skipped entry point is not
  verified through a different one.

## Feature entry contract

Each feature file starts with an H1 title and one paragraph describing the user-visible behavior, then exactly four
H2 sections in this order: `Sub-features`, `How to get to it (user POV)`, `Driving it with bin/mcp.sh` (or the
harness it uses), and `Gotchas`.

## Features

| Feature | Last driven live | Result |
|---|---|---|
| [Ask and get real work done](./ask-and-answer.md) | 2026-11-04, v20261104-1032, b04f9f2 | passed: open PRs and PR #51 matched `gh` |
| [Code work on a repository, the pstack way](./pstack-repo.md) | 2026-11-04, v20261104-1815, 04ceeec | passed: answer and commit id matched the clone, poteto-mode entered, commit by the forge identity, remote untouched |
| [Browse a page with a signed trail](./browser-trail.md) | 2026-11-04, v20261104-1815, 04ceeec | passed: `agent trail` 2 steps, 2 signatures ok, 2 blobs ok |
| [Use an API key the model never sees](./keyed-fetch.md) | 2026-11-04, v20261104-1815, 04ceeec | passed: probe 401 without, 200 with the vault key, `key_used` on the chain, no test value in state |
| [Chat on Discord with approvals](./discord-chat.md) | 2026-11-04, zelix, b04f9f2, scripted Router | passed: approval buttons, Approve, result |
| [Follow up on a request](./follow-up.md) | not yet | seeded (`resume` now checks that the new worker's first prompt quotes its earlier replies) |
| [Work on GitHub as the App's own bot](./github-app-auth.md) | not yet | seeded (needs the App created by hand) |
| [Ask for a change and get a pull request from the bot](./github-pr.md) | not yet | seeded (needs the App created by hand and `owes: pull_request` on the sandbox) |
| [The owner's memory shapes later work](./memo-owner.md) | not yet | seeded |
| [Workers remember, and the review decides](./memo-remember.md) | not yet | seeded |

Not yet mapped, each a real surface: Swarm Tickets (`executor.swarm`, the Router choosing `Plan::Swarm`), schedules,
remote nodes, Claude Code workers, the Watchdog's alerts, Slack, `agent bench`, and swarm mode itself (`solos-swarm run`, verified by
the repo's `skills/verify-solos-swarm`).
