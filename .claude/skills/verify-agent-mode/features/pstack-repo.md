# Code work on a repository, the pstack way

A person asks about, or for a change to, a repository the operator lists under `forge.repos`. The Router names the
repository when it accepts the work. The host clones it into the Ticket's `work/` before the first worker starts. The
worker enters pstack's `poteto-mode`, works in the clone with `run_code` (git included), `read`, `edit_file` and
`write_file`, and answers from the real code. With `forge: local`, as in this recipe, nothing is pushed: a local forge
has no pull requests. Pull requests ship through the GitHub App (#75), only for a repository with `owes: pull_request`
under `forge: github`. [github-pr.md](github-pr.md) drives that.

## Sub-features

- `repo-named`: the accepted Ticket names the repository, and `state/tickets/<ticket>/host/repo.json` holds `repo`,
  `base` and `base_commit`.
- `pstack-entry`: the worker's first prompt carries the `<skill name="poteto-mode" location="/pstack/skills/poteto-mode/SKILL.md">`
  block, and its answer follows poteto-mode (it opens with the goal, or names what it skipped and why).
- `answer-from-code`: an answer checkable against the clone (a constant's value, the commit id) matches it.
- `commit-in-clone`: a requested change is committed in the clone by `forge.identity`, and the remote is untouched.
- `file-tools`: the worker writes the change with `write_file` or `edit_file`, not a heredoc in `run_code`.

## How to get to it (user POV)

- Any chat or MCP request that names a listed repository: "In the GuiBibeau/solos-swarm repository, …".
- The operator lists the repository under `forge.repos`: `forge: github` through the GitHub App, or `forge: local`
  for a dry run. `pstack.entry` names the skill every worker enters (default `poteto-mode`). It is the only key of
  `pstack:`.

## Driving it with bin/mcp.sh

Preconditions: a run launched with `overlays/pstack.yaml overlays/forge.yaml`. `launch.sh` seeds
`forge-remote/GuiBibeau/solos-swarm.git` with one commit of the shipped checkout. Doctor clean. `forge.yaml` sets no
`owes`, so the repository owes `answer`, the default.

- `answer-from-code`, `repo-named`: `bin/mcp.sh $RUN request "In the GuiBibeau/solos-swarm repository: what is the value of the constant MAX_SWARM_AGENTS in crates/agent-core/src/engine.rs, and what is the full commit id of the commit your copy of the code is at? Read it from the code."`, then `replies <thread> result 540`. Check the value with `grep -n 'MAX_SWARM_AGENTS: u16' ~/work/verify/$RUN/src/crates/agent-core/src/engine.rs` and the commit with `git -C ~/work/verify/$RUN/forge-remote/GuiBibeau/solos-swarm.git rev-parse main`. `cat state/tickets/<ticket>/host/repo.json` names the same commit.
- `pstack-entry`: `grep -c '<skill name=\\"poteto-mode\\"' state/tickets/<ticket>/*.rpc.jsonl` is at least 1.
- `commit-in-clone`: `bin/mcp.sh $RUN request "In the GuiBibeau/solos-swarm repository, add a file VERIFY.md at the repository root containing exactly the line 'verified by the agent' and commit it with the message 'docs: add VERIFY.md'. Do not push. Tell me the new commit id."`. Then `git -C state/tickets/<ticket>/work log -1 --format='%H %an <%ae> %s'` shows the reply's commit id by `solos-swarm-verify <verify@example.invalid>`, `cat VERIFY.md` holds the line, and the bare remote's `main` is still the seed commit.
- `file-tools`: on the same Ticket, `grep -oh '"toolName":"[a-z_]*"' state/tickets/<ticket>/*.rpc.jsonl | sort | uniq -c` lists `write_file` (or `edit_file`) among the tools called, and the `run_code` calls hold no `<<` heredoc that writes VERIFY.md.
- `bin/evidence.sh $RUN pstack-repo`.

## Gotchas

- The worker may read files with `run_code` (`cat`, `sed`) rather than `read`. Both are real reads of the clone.
- `write_file` and `edit_file` exist only where `run_code` runs on this host, which `agent.verify.yaml` gives every
  run. Before they existed, workers wrote files with heredocs in `run_code`, so an older transcript shows no
  `write_file`.
- The `local` forge's commit ids are the seed's, not GitHub's. Check them against the bare repository, never with
  `gh`.
- The forge needs `executor.run_code`, which `agent.verify.yaml` carries for every run.
- `owes` changed what `commit-in-clone` proves. The 2026-11-04 pass ran before `owes` existed (#75). Under
  `forge.yaml` as it is, the repository owes `answer`: the worker is not told to commit, and a commit it makes anyway
  stays in the clone unchecked. To have the host check the commits at `ready` (commits by the clone's identity only, no
  protected path, the size limit) and keep them in the clone, use a copy of the overlay with `owes: change` on the repository.
  `owes: pull_request` does not load with `forge: local`.
- On 2026-11-04 both drives together cost under $0.01.
