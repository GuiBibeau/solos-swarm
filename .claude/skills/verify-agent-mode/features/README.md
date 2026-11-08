# Agent mode verification map

This map is the maintained source for verifying agent mode's user-facing behavior. Read the index, then use the
matching feature file as the recipe. The skill's [SKILL.md](../SKILL.md) holds Launch, Doctor, Drive, Evidence and
Cleanup, and the `q` helper that reads the live store.

## Baseline preconditions

- A run shipped and launched with `bin/ship.sh` and `bin/launch.sh`, with the overlays the feature names.
- `bin/doctor.sh <run>` prints only `ok` lines.
- The operator approved the session's spend cap. A real Router and real workers spend money on every drive.
- Never drive an agent this verification did not launch, such as the operator's own Discord agent.

## Driving conventions

- Act as the person `mcp:verifier` through `bin/mcp.sh`. Treat every command as literal.
- Phrase each request as the work a person would ask for. The Router judges intent: it ignores a request that reads
  as moving a credential to an unknown place, even when the place is our own test server.
- One feature per launch when the feature needs an overlay. Snapshot the evidence before you relaunch.
- Filter `events.jsonl` with `jq -c '.[1] | fromjson | select(.type == "...")'`: each row is `[seq, "<event JSON>"]`.
- The box has no `sqlite3` command. Read a store table with the `q` helper in SKILL.md.

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

A result holds for the revision it names. A feature that has never run live says `not yet` in both columns, and its
recipe is a plan.

| Feature | Last driven live | Result |
|---|---|---|
| [Ask and get real work done](./ask-and-answer.md) | 2026-11-04, v20261104-1032, b04f9f2 | passed: open PRs and PR #51 matched `gh` |
| [Code work on a repository, the pstack way](./pstack-repo.md) | 2026-11-04, v20261104-1815, 04ceeec | passed: answer and commit id matched the clone, poteto-mode entered, commit by the forge identity, remote untouched |
| [Browse a page with a signed trail](./browser-trail.md) | 2026-11-04, v20261104-1815, 04ceeec | passed: `agent trail` 2 steps, 2 signatures ok, 2 blobs ok |
| [Use an API key the model never sees](./keyed-fetch.md) | 2026-11-04, v20261104-1815, 04ceeec | passed: probe 401 without, 200 with the vault key, `key_used` on the chain, no test value in state |
| [Chat on Discord with approvals](./discord-chat.md) | 2026-11-04, zelix, b04f9f2, scripted Router | passed: approval buttons, Approve, result |
| [Follow up on a request](./follow-up.md) | not yet | not yet |
| [Stop a looping worker and remind a waiting person](./loop-stop-and-remind.md) | not yet | not yet |
| [Work on GitHub as the App's own bot](./github-app-auth.md) | not yet | not yet: needs the App created by hand |
| [Ask for a change and get a pull request from the bot](./github-pr.md) | not yet | not yet: needs the App created by hand and `owes: pull_request` on the sandbox |
| [The owner's memory shapes later work](./memo-owner.md) | not yet | not yet |
| [Workers remember, and the review decides](./memo-remember.md) | not yet | not yet |
| [The daily dream checks memory against the transcripts](./dream.md) | not yet | not yet |
| [Watch jobs that are not Tickets](./external-jobs.md) | not yet | not yet |

These parts of agent mode have no recipe yet: swarm Tickets (`executor.swarm`, the Router choosing `Plan::Swarm`),
schedules, remote nodes (`node serve`), Claude Code workers, the Watchdog's alerts about Tickets, Slack, `agent bench`,
`agent health`, `agent prune`, `agent stop`, and `agent publish` with the web dashboard. Swarm mode itself
(`solos-swarm run`) is verified by the repository's `skills/verify-solos-swarm`, not here.
