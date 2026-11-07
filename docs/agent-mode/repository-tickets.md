# How repository Tickets work

A repository Ticket is a Ticket whose Mandate names one of `forge.repos`. Its worker reads the repository's real code
in a clone of its own, runs git and the code there with `run_code`, and commits there. What the Ticket owes decides
what its `ready` means: an answer, a change committed in the clone, or a pull request the host opens as the bot. This
page explains what the host does at each step and why. To set it up, see
[Let workers work in a repository](how-to/repositories.md).

A question about how or why a repository works counts as work on it, so the Router names the repository for it too.

## Naming a repository

`router_decide` takes `repo`, one of `forge.repos`, and `deliverable`, which says what the finished work owes:
`answer`, `change` or `pull_request`. Without `deliverable`, the Ticket owes the repository's `owes`. `owes` is also a
ceiling. The Router may lower it for one request and never raise it, and `router_decide` refuses a `deliverable` above
it and names the ceiling. So `owes: answer` makes a repository read-only to the agent, whatever a model asks.

`router_decide` also refuses an unknown repository, a repository on a swarm plan, a repository on a remote node, and a
`deliverable` with no repository.

The Ticket records the repository in its Mandate and the deliverable in its Plan. For a change or a pull request, it
also records the repository's terms: the merge mode, CI, and the limits. A later config edit never changes the terms of
work in flight.

Tickets that name a repository need schema V6 of `agent.db`, and Tickets that deliver a change or a pull request need
V7, so an older build refuses the file.

## The mirror and the clone

The host keeps a bare mirror of each repository at `<state>/forge/<owner>/<name>.git`. Before a Ticket's first worker,
it fetches the mirror and clones it into `<state>/tickets/<ticket>/work`. The clone has its history, the forge's other
branches as `origin/*`, a `.git` of its own, and the commit identity in its config: the App's bot for `kind: github`.
Then the host writes `<ticket>/host/repo.json`, which holds the repository, its base, the commit the clone started
from, and the notes.

A later Assignment of the same Ticket finds `repo.json` and keeps the tree as the last worker left it. A clone with no
`repo.json` is made again, since no worker ran in it.

The host runs git only in its mirror, and in the empty `work/` before any sandbox touched it. A sandbox can plant a
hook or a config in the clone, but no host git ever reads them. The token reaches git in its environment, for the
mirror's fetch alone, and never in an argument, a sandbox or the clone's config.

## What the worker can read

On a repository Ticket, `read` also reaches `/work` (the clone) and `/host`, read-only. The worker's system prompt
names the repository, its base and the notes.

The worker writes `/work`, so `read` bounds what it scans there. A file's lines are counted over its first 32 MiB, and
the reply for a longer file says `total_at_least` in place of `total`. A directory is listed from its first 20,000
entries, and `truncated` says there were more.

A repository Ticket gets `read` even with no `pstack:` configured. The Pi sandbox binds `/pstack` only when `pstack:`
is set, and then binds only the tree the host checked.

## Why `prepare` must not run repository code

`executor.prepare` runs after the clone, on the host, as the agent's user and outside any sandbox. A repository is data
until a sandbox runs it. A `prepare` that runs it (a build, a test, an install, a script or a hook from the clone)
gives that repository's authors the host.

With `forge` configured, config load also refuses a `prepare` that runs `git worktree add` into `$SOLOS_WORK_DIR`. A
linked worktree's `.git` points at a path that no sandbox has.

## Delivering a pull request

A pull request is owed, not done. The worker has no push, no pull-request tool and no token. Its whole vote is
`worker_report ready`, which the host admits or refuses, and everything after that is the host's. A worker that ends
its turn in plain text, which the host files as `ready` for it, goes through the same admission.

```text
ready ─ harvest ─ admit ─ one commit: ProposalAdmitted + Effect::Forge(Publish) + the worker's Assignment ends
        (a fresh      (in the mirror)           the Ticket stays open, stage `delivering`
         sandbox)
Publish ─ push the branch (lease) ─ find or open the pull request ─ read it back ─ Delivered, the Result, the Ticket ends
```

**Harvest.** The host runs a fixed script in a sandbox it builds, with `/work` read-only, a fresh writable `/out`, no
network and an empty environment. Git is told to ignore the clone's own config: no file system monitor, no hooks and
no replace refs. The script writes `git bundle <base commit>..HEAD`. Its own output is advisory.

**Admit.** The bundle is untrusted data. The host copies it to `<state>/forge-stage/<ticket>/<head>.bundle`, beside
the mirrors, so a mirror that is made again cannot lose it. It opens the bundle without following a link and within
`max_change_bytes`, and fetches it into its mirror with `fsckObjects` and replace refs off. It judges the agent's own
range there, never in the worker's tree. The range is the commits reachable from the head and from nothing on the
base. These checks apply:

- The head descends from the commit the clone started from.
- Every commit's author and committer is the bot, exactly.
- No commit in the range changes a `protected` path, an intermediate commit included.
- The change is within `max_change_bytes` and 2,000 files.

Commits of a newer base that the worker merged in are the base's, and are not judged. A refusal is the
`worker_report` call's 409. Nothing is committed, the Ticket stays open, and the sentence says what to do next. For a
commit by someone else, it says to run `git rebase --exec 'git commit --amend --no-edit --reset-author' <base>`.

**Publish.** The Effect is committed with the admission, so a crash anywhere leaves it queued, and the runtime runs it
again. Each step is an "ensure":

1. The host names the branch `solos/<ticket>/<slug of the title>` and runs `ls-remote`. A branch that already holds the
   head is done. A branch that holds the last head the host confirmed is pushed with `--force-with-lease` against
   that head. Anything else is someone else's branch, and is never overwritten: the Change is `Withheld`, and one
   question goes to a person. The token is a write token for the one repository, in the mirror git's environment
   only.
2. The host finds the pull request by its head, or opens it ready for review, never as a draft. It carries the
   worker's words, `Closes #n` when the request was an issue of the same repository, who asked, and a hidden marker
   that ties it to the Ticket. A pull request on that branch without the marker is refused, and one that a person
   closed is not reopened.
3. The host reads back what GitHub shows before it records anything.

**The result.** The Ticket ends only then. Its Result is the worker's text and one line that the host builds from what
it read back: `PR <url> · head <sha> · opened` (`updated` for a later head). The Ticket subscribes to the pull
request's conversation, so a listed person's comment on it resumes the Ticket. The first Result reaches the origin: the
issue or the chat thread. From then on, the pull request's conversation takes the place of an origin on GitHub, so a
review round answers once on GitHub, and a chat origin keeps hearing everything. A follow-up that arrives while the
pull request is being made is kept, and takes the Ticket up again once the pull request exists.

**Limits.** A Ticket publishes at most `max_pushes` heads. A `ready` with the same head is an answer, not a second
push.

**Held.** `policy.paused` holds the Effect with the rest of the agent. A suspended or missing installation makes it
wait five minutes and try again, and it converges when the owner lifts the suspension. `github.writes: false` makes the
answer `Withheld`, and nothing is pushed.

**A change.** A Ticket that owes a change ends when the host admits the worker's commits, with the same checks. Its
result says `Committed <sha> (n files) in the ticket's clone; not published.` A `ready` with no commits is refused.

**Older Tickets.** A Ticket accepted before this build, with no terms in its Mandate, ends on `ready` as it did.

## Installed dependencies

`deps` makes the host install a repository's dependencies once per lockfile, in a cache that every Ticket's `run_code`
mounts read-only.

**What the cache is built from.** Only the host's mirror at the commit the Ticket's clone started from (`base_commit`
in `repo.json`): the lockfile (`bun.lock`, and `bun.lockb` if there is one), the root `package.json`, the
`package.json` of each workspace the root names, and the files that `patchedDependencies` names. Never a worker's
branch, and never `/work`, which a worker writes. `bunfig.toml` and `.npmrc` are not read. The install uses no
registry, scope, token or linker setting that the repository chose, and fetches from `https://registry.npmjs.org/`
only. A text lockfile that names any other host (a tarball, a git dependency, a private registry) is refused, and the
worker is told the reason. A binary `bun.lockb` is read as a lockfile like any other, but cannot be checked for the
hosts it names. The install still reads no registry setting, so a registry package comes from npmjs.

The cache key is the SHA-256 of those files and where they are. A change to the lockfile or a manifest at the base is a
new cache, and a commit that changes neither uses the same one. Caches are never changed once written. A bun upgrade
does not change the key.

**The build.** `bun install --frozen-lockfile --ignore-scripts` runs in a bwrap sandbox of its own. Besides the browser,
it is the one place that shares the host's network on the host's behalf. It holds nothing else of the host's: no home,
token, vault or agent state, an empty environment except a few variables, `/usr` and the TLS and DNS files read-only,
the package manager read-only, and a scratch directory. No lifecycle script runs, so no code of the repository runs.
The build is killed at `timeout_secs`, or when its scratch passes `max_bytes`.

The build writes into a `tmp-*` directory and renames it to `<state>/deps/<owner>/<name>/<key>`, so a crash leaves
nothing a Ticket would mount. A directory with its `manifest.json` is a complete cache. Builds of one repository run one
at a time, under a lock of their own, so a slow install does not hold up the mirror. A failed build is not tried again
for the same lockfile for five minutes. `agent prune` removes a cache that no Ticket has mounted for `--keep-days`, and
the scratch of a build that died.

**When.** The build runs lazily, as each Ticket on the repository starts and its clone is made, before the worker. The
first Ticket builds, and the rest find the cache with a read of the mirror. A build that cannot run (no `bun.lock`, a
refused lockfile, a network that is down, a full disk, a missing `bun`) never fails the Ticket. The worker's prompt
says that dependencies are unavailable and why, and tells it to say so when it reports.

**What a Ticket's `run_code` sees.** Bun links each workspace's `node_modules` into one store, `node_modules/.bun`,
with relative links (`apps/web/node_modules/zod -> ../../../node_modules/.bun/zod@4.6.6/node_modules/zod`):

- The directories that hold real files, the store among them, are mounted read-only at their paths under `/work`.
- A directory of nothing but links is made again in each sandbox as a fresh tmpfs, because tools write there. Vite
  makes `node_modules/.vite-temp`, which a read-only mount refuses, and that fails a repository's own tests. What they
  write is gone when the command ends.
- The shared cache is never writable, so no Ticket can plant a file that another will run.
- The package manager is mounted read-only as `bun` and as `node` in `/runtime/bun`, first on `PATH`. Bun runs
  `#!/usr/bin/env node` scripts (ESLint, for one), and the host's own node may be too old for them.
- `HOME` is a throwaway `/tmp/home`. A tool that writes a cache under `$HOME` would otherwise put it in the clone, and a
  dirty tree fails an evidence check.
- The clone gets `node_modules/` and `/.bun/` in its `.git/info/exclude`, written before any sandbox ran, so
  `git add -A` and `git status` ignore them.

The sandbox is still offline. `bun install` and `bun add` cannot run, so a dependency a worker adds is not installed,
and a change to a manifest or the lockfile is not reflected in the cache.

The Pi session's own sandbox holds none of this. Its `/work` is a tmpfs for Pi's settings, not the clone, and nothing
in it runs repository code: every command goes through `run_code`, and `read` is the broker's. The Claude Code harness
is the same, with its tools off.

`kind` is the one variant, `bun`. npm or pnpm would be new variants.

## Evidence

`evidence` tells the worker to run `command` in `/work` once its commits are final and its tree is clean, and to paste
the JSON it prints, unedited, in a ```` ```json ```` fence under `heading` in its ready report. That report is the pull
request's description. The description's mention quieting leaves a fence as written.

The host reads the report before it opens a pull request, and so before it pushes a branch. It refuses a `ready` that
CI would fail at once:

- No heading followed by a `json` fence.
- JSON that is not an object. A long report is cut at 8,000 characters, so its block may be cut.
- An object whose `ok` is not `true`.
- An object whose `dirty` is `true`.

The refusal says what to run and what to do, including to report `blocked` when the command cannot run, for example
when dependencies are unavailable. A `ready` that is only a reply on a head already delivered is not a description and
is not checked. The host does not judge the proof itself. Which commit the object names (`sha`), and every other rule,
stay with the repository's CI.

**Why the worker runs the command and the host does not.** The host could run the command itself after `admit`, on a
fresh checkout of the admitted head, and append the JSON, which a worker could then neither mangle nor make up. That is
the more trustworthy design, and it is not this one, for now. `verify` takes about a minute on `GuiBibeau/solos` and
more on a bigger repository. That is too long to hold a `worker_report` call (the Pi extension's broker call times out
at 11 minutes) or the mirror's lock. The result would also have to cross from `admit` to `Publish` and go into a
description that the host builds from the worker's text. The gate above catches a missing, malformed, failed or dirty
block at once, and CI judges the rest. The sandbox and the cache that `deps` adds are what a host-run `verify` would
use.

**Not covered.** A pull request's description is made once. A later head that the host pushes for a review or a CI
repair keeps the first head's evidence, and a CI that requires evidence for every head asks the worker to run the
command again.
