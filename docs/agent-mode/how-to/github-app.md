# Set up the GitHub App

The agent works on GitHub as the bot of a GitHub App and holds no other GitHub credential: no personal access token and
no machine account. One App serves the `github` source, the `github` channel and the forge. You decide what the App
may do in two places: the repositories you install it on, and the permissions you give it.

The design is [github-app/rationale.md](../github-app/rationale.md), built in the pull requests of
[github-app/prs.md](../github-app/prs.md). Two of its PRs are built:

- PR 1 (#63): the agent reads, clones and comments as its own bot.
- PR 2 (#75): a worker's `ready` delivers a pull request. The host admits the worker's commits from a sandbox-made
  bundle and checks them, pushes `solos/<ticket>/<slug>`, and opens the pull request as `solos-bot[bot]`
  (`src/agent/forge/github.rs`). See [Deliver a pull request](repositories.md#deliver-a-pull-request).

CI repair, merging and the reconcile timer are not built. The config accepts `forge.repos.*.merge`, `ci` and
`max_repairs`, and `github.reconcile_secs`, but nothing acts on them yet. How the App's tokens, identity and pinned
logins work is in [GitHub](../channels.md#github).

## Create and install the App

You do this by hand, once, on github.com: **Settings**, **Developer settings**, **GitHub Apps**, **New GitHub App**.
[`github-app/manifest.json`](../github-app/manifest.json) is the same list as a GitHub App manifest, and a test pins
its permissions to the ones the agent mints.

1. **GitHub App name**: `solos-bot`, or any free name. People will write `@solos-bot`.
2. **Homepage URL**: `https://github.com/GuiBibeau/solos-swarm`, or any URL.
3. **Callback URL**: leave it empty. Turn off **Request user authorization (OAuth) during installation**.
4. **Webhook**: clear **Active**. The agent polls, so it has no URL and no secret.
5. **Repository permissions**:
   - Contents: Read and write
   - Pull requests: Read and write
   - Issues: Read and write
   - Checks: Read-only
   - Commit statuses: Read-only
   - Actions: Read-only
   - Metadata: Read-only (GitHub sets it)
   - Everything else: No access. In particular, set **Workflows** and **Administration** to **No access**.
6. **Organization permissions** and **Account permissions**: none. **Subscribe to events**: none.
7. **Where can this GitHub App be installed?**: **Only on this account**.
8. Select **Create GitHub App**. Note the **App ID** at the top of the next page.
9. Under **Private keys**, select **Generate a private key**. The browser downloads `<name>.<date>.private-key.pem`.
10. Select **Install App**, choose your account, choose **Only select repositories**, and pick the repositories the
    agent may act on. Start with a throwaway one.

The App never holds `workflows`, so GitHub itself refuses a push that edits `.github/workflows/**`. It never holds
`administration`, so it cannot touch branch protection.

## Give the agent its key

From the Mac, in this repository:

```sh
scripts/agent-deploy token github-app.pem < ~/Downloads/solos-bot.2026-11-05.private-key.pem
rm -P ~/Downloads/solos-bot.2026-11-05.private-key.pem
```

The key is stored beside the other one-use token files, as `~/agent/tokens/github-app.pem` (mode 0600). The production
unit copies `tokens/` into its tmpfs runtime directory before every start. `agent serve` reads that copy once, deletes
it, and keeps the parsed key in memory, zeroized when it drops. The key is not a vault item, and no sandbox can see it:
`serve` refuses a `github.key_file` under any path a sandbox can read, before it reads it.

To rotate the key without downtime, generate a second key in the App's settings, store it, restart, then delete the
first key on GitHub. GitHub keeps several keys per App.

## Configure the agent

```yaml
policy:
  people: [github:alice]               # logins who may ask for work; lowercase
  approvers: [github:alice]
github:
  app_id: 123456                       # the App's numeric id
  key_file: secrets/github-app.pem     # a one-use token file; in production /run/user/1000/solos-agent/github-app.pem
  # poll_secs: 60                      # the source's interval; a quiet poll costs 304s, which GitHub does not count
  # writes: true                       # false: no write token is ever minted, so the agent writes nothing on GitHub
forge:
  kind: github                         # no token_file, no identity: the clone's commits are by the App's bot
  repos:                               # the one list of repositories: the source, the channel and the forge use it
    acme/widgets: {about: The widget service, base: main, owes: pull_request}
sources:
  - kind: github                       # no fields: it reads forge.repos
  - kind: schedule
    schedules:
      - name: triage
        every: 6h                      # 00:00, 06:00, 12:00 and 18:00 UTC
        text: Triage the issues opened since the last sweep.
        reply_to: {kind: github, repo: acme/widgets, number: 1}   # a repository of forge.repos
channels:
  - kind: github                       # comments as the bot
# router, executor (pi, with run_code, which the forge needs) and budgets: as in production.md
```

`owes` is the most the agent does in that repository. See [Let workers work in a repository](repositories.md). Every
field of `github:` is in the [configuration reference](../configuration.md#github).

## Check the App

On the box, `serve` has deleted its copy of the key, so name the durable one:

```text
$ ~/agent/current/bin/solos-swarm agent github check --config ~/agent/agent.yaml \
      --key-file ~/agent/tokens/github-app.pem --state-dir ~/agent/state
  app solos-bot (123456) acts as solos-bot[bot]; people write @solos-bot
  acme/widgets  installation 7788  write: contents issues pull_requests
                read: actions checks metadata statuses   workflows: absent (good)  administration: absent (good)
                mint read ok  mint write ok (unused here)  base main exists
  github:alice pinned to account 1234567 (/home/agent/agent/state/github-identities.json)
  ok
```

`check` exits 1 on any miss and says what to do. A miss is a key GitHub refuses, an installation that is missing,
suspended or short of a permission, an installation that holds `workflows` or `administration`, a base branch that
does not exist, or a pinned login that now names another account. It reads the key and deletes nothing. Without
`--state-dir`, it looks the accounts up and shows them, and pins nothing.

## When a repository in `forge.repos` lacks the App

You can list a repository in `forge.repos` before the App is installed on it, and an owner can suspend the App on one
account while it works on another. A repository without the App never holds back the others.

- `serve` still starts. It logs once that the App is not installed on the repository, with the URL to install it from,
  or that it is suspended, and leaves the repository out. The `github` source reads every other repository at
  `github.poll_secs`, as usual.
- It asks GitHub about the left-out repository again every five minutes, and logs nothing more while the answer is the
  same. When you install or unsuspend the App, the next ask works, the log says `the App works on <repo> again`, and
  the source reads the repository with no restart. It reads from the repository's saved position. For a repository the
  agent has not read yet, that is when the agent first listed it, less the 10-minute overlap, so a comment written
  meanwhile arrives then.
- A reply or a pull request for that repository waits and tries again every five minutes, as for any suspended App.
- If GitHub refuses the repository itself (a 403 or a 404 on `GET /repos/<repo>`) although the App is installed on it,
  waiting cannot help. The log names the repository and GitHub's words once, and the source leaves that repository out
  until the next restart. The other repositories are read. If the App can read none of them, the `github` source stops
  and says so, and the rest of the agent keeps running.
- A key GitHub rejects still stops the `github` source, since it is the same for every repository.

`agent github check` lists each repository that is missing or suspended and exits 1.

## Prove it works

Open one issue on the throwaway repository:

```sh
gh issue create -R <owner>/<repo> -t "@solos-bot what is the first line of README.md?"
```

Within a minute a comment answers it, and its author is `solos-bot[bot]`.

## Stop the agent acting on GitHub

Any of these stops it:

- Suspend or uninstall the App on GitHub, under **Settings**, **Applications**, **Installed GitHub Apps**. Every GitHub
  call for a repository it covered then waits five minutes and tries again, while the `github` source reads the other
  repositories ([details](#when-a-repository-in-forgerepos-lacks-the-app)). Unsuspending or reinstalling resumes the
  work without a restart.
- Set `github.writes: false` and restart. No write token is ever minted.
- Set `policy.paused: true`. It holds every Effect with the rest of the agent.
- Remove a repository from `forge.repos`. A repository outside it gets no token.

## Move from token files

The agent used to authenticate with two personal access tokens: one for the `github` source (`sources[].token_file`)
and one for the forge (`forge.token_file`). It named the forge's commit identity in `forge.identity`. All three are
gone, and so are the `github` source's own `repos`, `api_base` and `poll_secs`. A config that still names any of them
fails to load with one sentence that points here.

1. Create and install the App, as in [Create and install the App](#create-and-install-the-app).
2. Store its key, as in [Give the agent its key](#give-the-agent-its-key).
3. Add the `github:` section.
4. Make the source `- kind: github` with no fields.
5. Move its `repos` into `forge.repos`, merged with the forge's.
6. Drop `token_file` and `identity` from the forge.
7. Move `poll_secs` and `api_base` into `github:`.
8. Delete `forge-token` and the source's token file from `~/agent/tokens`.
9. Run `agent github check`.
10. Deploy.
