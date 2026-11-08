# Let workers work in a repository

A worker can answer from a repository's real code, and change it in a clone of its own. You list the repositories the
agent may work in. The Router names one when it accepts work on it, and the host clones it into the Ticket's `work/`
before the worker starts. What the host does, and why, is in [How repository Tickets work](../repository-tickets.md).

## List the repositories

The executor must be `pi` with `run_code`, where workers run git.

```yaml
# agent.yaml
github: {app_id: 123456, key_file: secrets/github-app.pem}   # the App; see "Set up the GitHub App"
forge:
  kind: github                             # the clone's commits are by the App's bot
  repos:
    GuiBibeau/solos-swarm:
      about: The agent's own code, Rust and TypeScript.   # the Router reads this
      base: main                           # each clone starts at its tip
      budget_microusd: 3000000             # an accept that names no budget gets this
      notes: Tests run with cargo test -p solos-agent-core --offline.   # the worker reads this
      owes: pull_request                   # answer (the default), change or pull_request
      # protected: [".github/**"]          # paths no commit may change (a merge by its own edits)
      # max_pushes: 6                      # heads published for one Ticket, review rounds included
      # max_change_bytes: 5000000          # the largest change a ready is admitted with
      # deps: {kind: bun}                  # install the base's dependencies for every Ticket's run_code
      #                                    # (kind: cargo, with a binary, vendors a Rust repository's crates)
      # evidence: {command: "bun run solos dev verify --scope unit --json"}
```

Every field is in the [configuration reference](../configuration.md#repository-settings).

For tests and dry runs, use `kind: local` instead, with `remote: <dir>` and `identity: {name, email}` (who the clone's
commits are by) in place of the `github:` section. Each repository is then the bare repository
`<dir>/<owner>/<name>.git` on this disk, so nothing needs the network.

## Install the GitHub App on them

Install the App on every repository in `forge.repos` ([Set up the GitHub App](github-app.md)). The forge has no
credential of its own. Its mirror fetches with a read token that the App mints for that one repository, and
`forge.repos` is also the list of repositories that the `github` source reads.

## Mount the toolchains

`run_code` has no network, so a build must work offline. Mount the toolchains a repository needs with
`executor.run_code.mounts`, and say where they are in the repository's `notes`.

A Rust repository needs neither: `deps: {kind: cargo}` mounts the toolchain it names and the crates, read-only, into
that repository's Tickets and no others ([Build a Rust repository offline](#build-a-rust-repository-offline)). Do not
mount `~/.cargo` for it: it holds registry credentials.

## Choose what a Ticket owes

`owes` is what a Ticket on the repository owes when the Router names nothing, and the most it may owe:

- `answer`: the worker answers in chat. The repository is read-only to the agent, whatever a model asks.
- `change`: the worker commits in its clone, and the host admits the commits. Nothing is pushed.
- `pull_request`: the host pushes the worker's admitted commits and opens a pull request as the bot.

The Router may lower `owes` for one request and never raise it.

## Deliver a pull request

1. Set `owes: pull_request` on the repository. It needs `forge: github` and `github.writes: true`.
2. Ask for a change in chat, over MCP, or with an `@` mention of the bot in an issue.
3. The worker commits in its clone and reports `ready`. It has no push, no pull-request tool and no token.
4. The host admits the commits or refuses them. A refusal goes back to the worker with what to do next.
5. The host pushes `solos/<ticket>/<slug of the title>` and opens the pull request, ready for review. The Ticket's
   result names it: `PR <url> · head <sha> · opened`.

To stop deliveries, set `github.writes: false` and restart: the answer becomes `Withheld` and nothing is pushed.
`policy.paused` holds the delivery with the rest of the agent. The steps are in
[Delivering a pull request](../repository-tickets.md#delivering-a-pull-request).

## Install dependencies and require evidence

A worker's sandbox has no network, and its clone has no `node_modules`. A JavaScript repository's own scripts (its
tests, its type check, its `verify` command) cannot run there unless the host installs the dependencies for it. `deps`
has the host install them once per lockfile, and every Ticket's `run_code` mounts them read-only. `evidence` makes the
worker's report carry what the repository's CI asks of a pull request.

```yaml
forge:
  repos:
    GuiBibeau/solos:
      about: ...
      base: main
      owes: pull_request
      deps: {kind: bun}     # binary: $HOME/.bun/bin/bun; timeout_secs: 600; max_bytes: 8 GiB; memory_bytes: 16 GiB
      evidence: {command: "bun run solos dev verify --scope unit --json"}   # heading: "## Evidence" by default
```

- The host needs `bun`, `bwrap` and `prlimit`.
- Keep `<state>` off a tmpfs. A large dependency tree uses many inodes: about 234,000 for `GuiBibeau/solos`.
- Bun (JavaScriptCore) reserves a great deal of address space and aborts under about 8 GiB. `deps.memory_bytes` raises
  the address-space cap of `run_code` for this repository's Tickets only, and never lowers it.
- Prefer the text lockfile, `bun.lock`. A lockfile that needs a private registry or a scope token is not supported.

How the cache is built and mounted, and what the evidence gate checks:
[Installed dependencies](../repository-tickets.md#installed-dependencies) and
[Evidence](../repository-tickets.md#evidence).

## Build a Rust repository offline

A worker's sandbox has no network and no Rust toolchain, so it could not compile or test a change to a Rust repository,
and the first build of what it wrote was CI's. `deps: {kind: cargo}` lets a worker run `cargo build` and `cargo test`
in `run_code`, offline. The host runs `cargo vendor --locked` for the base branch's `Cargo.lock` once per lockfile, in
a sandbox that has the network, crates.io only, and no build script. Every Ticket on the repository mounts the crates
and the toolchain read-only, and cargo is set to the vendored sources and to offline.

```yaml
# agent.yaml
forge:
  repos:
    GuiBibeau/solos-swarm:
      about: solos-swarm, the agent's own code (Rust and TypeScript).
      base: main
      owes: pull_request
      deps:
        kind: cargo
        # Required, and the cargo of one toolchain, not rustup's proxy at ~/.cargo/bin/cargo.
        binary: /home/debian/.rustup/toolchains/1.96.0-x86_64-unknown-linux-gnu/bin/cargo
        # memory_bytes: 17179869184   # the address space of `rustc` in run_code; 16 GiB by default, 6 GiB is enough
      notes: Test a crate with `cargo test -p solos-agent-core --offline`. Build the root crate with job_start.
```

- The host needs the toolchain, `cc` (`build-essential`), `bwrap` and `prlimit`. `rustup which cargo --toolchain 1.96.0`
  prints the path for `binary`. The toolchain is the one the repository's CI uses, so a worker's build is CI's.
- The toolchain comes in through `deps.binary`, not `executor.run_code.mounts`: the whole directory of the toolchain
  (833 MiB for 1.96.0) is bound read-only into the build and into this repository's Tickets, with `cargo` and `rustc` on
  `PATH`, and into no other Ticket. Do not mount `~/.cargo` or `~/.rustup` for it: `~/.cargo` holds registry
  credentials. `notes` does not need to say where Rust is.
- Crates go in `<state>/deps/<owner>/<name>/<key>/vendor`. For `solos-swarm` that is 476 crates, 374 MiB and about 23,500
  files, built in a few seconds. Keep `<state>` off a tmpfs.
- A Ticket builds into `/work/target`, in its clone, on disk. A full debug build of `solos-swarm` with every test
  target takes about 1 GiB there (the sandbox turns debug info off). The host puts `/target/` in the clone's
  `.git/info/exclude`, so `git add -A` and `git status` ignore it.
- A lockfile that names a git dependency or a private registry, and a manifest with a `git` or `registry` key or a
  `[patch]` table, is refused, and the worker is told. Everything the host fetches is from crates.io.
- A worker cannot add a dependency: the sandbox is offline, and the host vendored what `Cargo.lock` names. Cargo says
  `no matching package named ... found` and the worker reports it.
- `run_code` stops a command at 300 s, and a process may use 300 s of CPU. The worker's prompt tells it to start long
  builds with `job_start`, which runs up to `executor.run_code.job_max_secs`.
- The sandbox's file limit is 256 MiB. The sandbox sets `CARGO_INCREMENTAL=0` and `CARGO_PROFILE_DEV_DEBUG=0` so that
  a big crate stays under it: the library of `solos-swarm` is 235 MB, and a crate whose single output passes 256 MiB is
  killed with `signal: 25`. If one grows to that, the smallest fix is a per-repository file size limit beside
  `deps.memory_bytes`, not a bigger limit for every sandbox.

How it works and what it trusts: [Cargo](../repository-tickets.md#cargo). The fields:
[configuration](../configuration.md#repository-settings).

## Keep `prepare` away from repository code

`executor.prepare` still runs after the clone, on the host, as the agent's user and outside any sandbox. With `forge`
set, it must not execute repository code: no build, test or install step, no `make`, `npm install` or `./configure`,
and no script or hook from the clone. Keep `prepare` to work that touches no file of the clone, such as writing a
scratch directory or a file beside it. Config load refuses a `prepare` that runs `git worktree add` into
`$SOLOS_WORK_DIR`. [Why](../repository-tickets.md#why-prepare-must-not-run-repository-code).
