# Code work on a repository, the pstack way

A person asks about, or for a change to, a repository the operator lists under `forge:`. The Router names the
repository when it accepts the work; the host clones it into the Ticket's `work/` before the first worker starts; the
worker enters pstack's poteto-mode, works in the clone with `run_code` (git included) and `read`, and answers from the
real code. Commits stay in the clone: nothing is pushed until pull requests ship (PR 3 of `docs/agent-mode/pstack/prs.md`).

## Sub-features

- `repo-named`: the accepted Ticket names the repository; `state/tickets/<ticket>/host/repo.json` holds `repo`, `base`
  and `base_commit`.
- `pstack-entry`: the worker's first prompt carries the `<skill name="poteto-mode" location="/pstack/skills/poteto-mode/SKILL.md">`
  block, and its answer follows poteto-mode (it opens with the goal, or names what it skipped and why).
- `answer-from-code`: an answer checkable against the clone (a constant's value, the commit id) matches it.
- `commit-in-clone`: a requested change is committed in the clone by `forge.identity`, and the remote is untouched.

## How to get to it (user POV)

- Any chat or MCP request that names a listed repository: "In the GuiBibeau/solos-swarm repository, …".
- The operator lists the repository under `forge.repos` (kind `github` with a token file, or `local` for a dry run).

## Driving it with bin/mcp.sh

Preconditions: a run launched with `overlays/pstack.yaml overlays/forge.yaml`. `launch.sh` seeds
`forge-remote/GuiBibeau/solos-swarm.git` with one commit of the shipped checkout. Doctor clean.

- `answer-from-code`, `repo-named`: `bin/mcp.sh $RUN request "In the GuiBibeau/solos-swarm repository: what is the value of the constant MAX_SWARM_AGENTS in crates/agent-core/src/engine.rs, and what is the full commit id of the commit your copy of the code is at? Read it from the code."`, then `replies <thread> result 540`. Check the value with `grep -n 'MAX_SWARM_AGENTS: u16' ~/work/verify/$RUN/src/crates/agent-core/src/engine.rs` and the commit with `git -C ~/work/verify/$RUN/forge-remote/GuiBibeau/solos-swarm.git rev-parse main`; `cat state/tickets/<ticket>/host/repo.json` names the same commit.
- `pstack-entry`: `grep -c '<skill name=\\"poteto-mode\\"' state/tickets/<ticket>/*.rpc.jsonl` is at least 1.
- `commit-in-clone`: `bin/mcp.sh $RUN request "In the GuiBibeau/solos-swarm repository, add a file VERIFY.md at the repository root containing exactly the line 'verified by the agent' and commit it with the message 'docs: add VERIFY.md'. Do not push. Tell me the new commit id."`. Then `git -C state/tickets/<ticket>/work log -1 --format='%H %an <%ae> %s'` shows the reply's commit id by `solos-swarm-verify <verify@example.invalid>`, `cat VERIFY.md` holds the line, and the bare remote's `main` is still the seed commit.
- `bin/evidence.sh $RUN pstack-repo`.

## Gotchas

- The worker may read files with `run_code` (`cat`, `sed`) rather than `read`; both are real reads of the clone.
- The `local` forge's commit ids are the seed's, not GitHub's: check them against the bare repository, never `gh`.
- The forge needs `executor.run_code`, which `agent.verify.yaml` now carries for every run.
- On 2026-11-04 both drives together cost under $0.01.
