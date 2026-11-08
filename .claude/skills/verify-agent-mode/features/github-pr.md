# Ask for a change and get a pull request from the bot

A person asks, in chat or on GitHub, for a change to a repository the operator lists under `forge.repos` with `owes:
pull_request`. The worker commits in its clone and reports `ready`. The host harvests those commits in a sandbox it
builds, admits them in its own mirror (every commit by the bot, none touching `.github/**`, within the size limit),
pushes them to a branch it names, `solos/<ticket>/<slug>`, and opens a ready pull request as `solos-bot[bot]`. The Ticket
ends only once the pull request exists, and its Result carries a line the host built from what it read back: `PR <url> ·
head <sha12> · opened`. The worker has no push, no token and no pull-request tool. PR 2 of
`docs/agent-mode/github-app/prs.md`, merged as #75.

## Sub-features

- `delivers`: one pull request, ready (not draft), authored by the App's bot, on `solos/<ticket>/<slug>`, whose head is
  the commit the chain's `ProposalAdmitted` names, and whose commits are all by the bot as author and committer. The
  body names no branch but `headRefName`: the worker is told the host names and pushes the branch, so a local branch
  name in the body (`fix/...`) is a failure.
- `origin-closes`: asked from a GitHub issue, the pull request's body says `Closes #<issue>`, and asked from Discord it does
  not.
- `refusal`: a worker that commits as someone else, or into `.github/`, is refused with the remedy, nothing is pushed, and
  its next attempt is admitted.
- `crash`: killing the process the moment the proposal is admitted leaves one pull request and one `Delivered` event
  after the restart.
- `no-token-at-rest`: no installation token is in the state directory, a transcript or an event.

## How to get to it (user POV)

- On Discord: "add a CONTRIBUTING line that says tests run offline" in the thread of the agent's channel; or on GitHub,
  `@solos-bot add a CONTRIBUTING line that says tests run offline` as a comment on an issue.
- The operator created and installed the App ([GitHub App](../../../../docs/agent-mode/how-to/github-app.md)), listed
  the repository under `forge.repos` with `owes: pull_request`, listed the person under `policy.people`, and stored the
  key. `github.writes` is not `false`.

## Driving it with gh

Preconditions: the same as `github-app-auth.md` (the throwaway repository `GuiBibeau/solos-swarm-sandbox`, the App
installed on it alone, the key at `secrets/github-app.pem`, `sources` and `channels` listing `kind: github`), plus
`owes: pull_request` on the sandbox in the overlay's `forge.repos` and a Router and workers that run (the operator
approved the spend cap). The sandbox needs a `CONTRIBUTING.md`, or the worker creates it. `bin/doctor.sh $RUN` clean.

- `delivers`, `origin-closes`: `gh issue create -R GuiBibeau/solos-swarm-sandbox -t "Tests run offline" -b "@solos-bot add a
  CONTRIBUTING line that says tests run offline"`. A worker takes a few minutes. Then
  `gh pr list -R GuiBibeau/solos-swarm-sandbox --json number,headRefName,author --jq '.[]'` finds the pull request, and
  `gh pr view <n> -R GuiBibeau/solos-swarm-sandbox --json author,headRefName,headRefOid,isDraft,body,commits` on the Mac
  shows: `author.login` `app/solos-bot`, `headRefName` `solos/tk_.../...`, `isDraft` false, `body` ending in
  `<!-- solos-swarm ticket tk_... -->` and holding `Closes #<issue>`, and every commit's `authors` and the committer the
  bot (`gh api repos/GuiBibeau/solos-swarm-sandbox/commits/<headRefOid> --jq '[.author.login, .committer.login]'` is
  `["solos-bot[bot]","solos-bot[bot]"]`). The agent's comment on the issue ends `PR <url> · head <sha12> · opened`.
  Compare `<sha12>` with `headRefOid` and with the chain: `q "select body from events where json_extract(body,
  '$.type') = 'proposal_admitted'"` (the `q` helper in SKILL.md) names the same head.
  Asked from Discord instead, the body has no `Closes`.
- `refusal`: ask for a change that must touch a workflow ("add a CI step to .github/workflows/ci.yml") and watch the
  worker's transcript (`state/tickets/<id>/<assignment>.rpc.jsonl`): `worker_report` answers 409 with `which is protected
  (.github/**)` and the `git reset --soft` remedy, no `solos/` branch exists meanwhile
  (`gh api repos/GuiBibeau/solos-swarm-sandbox/branches --jq '.[].name'`), and the retry is accepted or the worker
  reports blocked.
- `crash`: ask for another small change, and poll the chain until `proposal_admitted` appears
  (`q "select count(*) from events where json_extract(body, '$.type') = 'proposal_admitted'"`). Then at once run
  `ssh remote-ai "pkill -9 -f 'solos-swarm agent serve.*verify-$RUN-key'"`, start the agent again on the same state
  ("Restart a run and keep its state" in SKILL.md, not `launch.sh`, which empties `state/`), and wait for idle.
  `gh pr list -R GuiBibeau/solos-swarm-sandbox --head solos/<ticket>/<slug>` shows exactly one pull request, and
  `q "select count(*) from events where json_extract(body, '$.type') = 'delivered'"` counts one for the Ticket.
- `no-token-at-rest`: `ssh remote-ai 'grep -r ghs_ ~/work/verify/$RUN || echo none'` prints `none`.
- `bin/evidence.sh $RUN github-pr`.

## Gotchas

- Not yet driven live: it needs the App created by hand and the sandbox configured with `owes: pull_request`.
- A repository whose `owes` is `answer` (the default) never gets a pull request, whatever the Router asks: `router_decide`
  refuses a deliverable above `owes`. `owes: pull_request` with `github.writes: false` does not load.
- The pull request is the host's, not the worker's. A worker that is asked to "open a pull request" cannot, and says it
  reported ready. The host opens it after the worker's `ready` is admitted.
- A collaborator's push to the bot's branch parks the Ticket (one question, `Withheld`), and the agent never overwrites
  it. A person who closes the pull request is not undone.
- `merge`, CI repair and review rounds are later PRs of the design: this one stops at an open pull request.
