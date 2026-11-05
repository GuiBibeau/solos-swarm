# Work on GitHub as the App's own bot

A person writes `@solos-bot` on an issue of a repository the operator lists under `forge.repos`. The agent, which holds
no personal access token and acts as the GitHub App's bot, reads the issue with a token minted for that one repository,
and answers in a comment whose author is `<slug>[bot]`. Its key is read once and deleted, and no token is ever written
to disk. PR 1 of `docs/agent-mode/github-app/prs.md`.

## Sub-features

- `app-identity`: the comment's author is `solos-bot[bot]`, the bot of the App, not a person.
- `answer-from-code`: the comment answers from the repository's real files, checkable against `gh api .../contents`.
- `check`: `agent github check` on the box prints the installation, its permissions (no `workflows`, no
  `administration`), a read and a write mint, the base branch and each listed login's pinned account.
- `no-token-at-rest`: after the drive, `grep -r ghs_ ~/work/verify/$RUN` finds nothing, and the key file is gone from
  the runtime directory.

## How to get to it (user POV)

- Comment `@solos-bot <question about the repository>` on an issue, or open an issue that says it.
- The operator created and installed the App by hand ([GitHub App](../../../../docs/agent-mode/README.md#github-app)),
  listed the repository under `forge.repos` and `github:<login>` under `policy.people`, and stored the key.

## Driving it with gh

Preconditions: a throwaway repository the operator owns (`GuiBibeau/solos-swarm-sandbox`) with the App installed on it
alone, so every token the run mints is narrowed to it. `overlays/github-app.yaml` adds the `github:` and `forge:`
sections (the key at `secrets/github-app.pem`, relative to the run directory; the sandbox as the only repository).
Put the key there before launching: `ssh remote-ai 'umask 077; cat > ~/work/verify/$RUN/secrets/github-app.pem' <
~/Downloads/<name>.<date>.private-key.pem`. The run's `sources` and `channels` must list `kind: github` as well (see
Gotchas). `bin/doctor.sh $RUN` clean. The operator approved the spend cap.

- `check`: `ssh remote-ai '~/work/verify/$RUN/bin/solos-swarm agent github check --config ~/work/verify/$RUN/agent.yaml
  --key-file ~/work/verify/$RUN/secrets/github-app.pem --state-dir ~/work/verify/$RUN/state'` prints `ok` and the table.
- `app-identity`, `answer-from-code`: `gh issue create -R GuiBibeau/solos-swarm-sandbox -t "@solos-bot what is the first
  line of README.md?" -b ""`, then within a minute or two `gh api repos/GuiBibeau/solos-swarm-sandbox/issues/<n>/comments
  --jq '.[] | [.user.login, .body] | @tsv'`. The author is `solos-bot[bot]`, and the text matches `gh api
  repos/GuiBibeau/solos-swarm-sandbox/contents/README.md --jq .content | base64 -d | head -1`.
- `no-token-at-rest`: `ssh remote-ai 'grep -r ghs_ ~/work/verify/$RUN || echo none'` prints `none`.
- `bin/evidence.sh $RUN github-app-auth`.

## Gotchas

- The verify config's `sources` and `channels` are lists, which an overlay cannot extend (a repeated top-level key is a
  config error). Until `launch.sh` merges lists, drive this feature with a copy of `agent.verify.yaml` whose `sources`
  and `channels` also hold `- kind: github` and whose `policy.people` also holds `github:<your login>`.
- The issue's author must be a login under `policy.people`, and the person is pinned to the account id GitHub gives it
  the first time the agent looks it up.
- Never point this at a repository the App is installed on that is not in `forge.repos`: the agent mints no token for it.
- Not yet driven live: it needs the App created by hand first.
