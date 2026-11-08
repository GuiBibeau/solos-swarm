# Host operations for agent mode

> **Status, 2026-11-08.** Design for [#104](https://github.com/GuiBibeau/solos-swarm/issues/104). Nothing is built.
> The user split it needs is on `main` (`ops/harden-units`, #117), but the box still has to be migrated to it and the
> helper installed before host ops is turned on ([section 10](#10-migration-and-rollout)). Rust types and signatures
> are in [`sketch/`](sketch/).

## 0. In one paragraph

Some requests are host operations, not repository changes: free disk, restart a paused download, check a unit, edit an
ops script. Workers cannot see the host, so today these fall back to unsandboxed Claude Code sessions. This design adds
them without giving a model a shell on the host. A small helper, `solos-hostops`, runs as the user that owns the
resources (`debian`) in a locked-down system unit: no network, `NoNewPrivileges`, no way to reach `debian`'s user
manager, and a home directory that holds only the paths it works on, so no credential file is there to read. It offers
a fixed set of typed actions over a Unix socket that only the agent's user may open. The units it may restart are
system units, restarted through a polkit rule that names each one. The helper, not the worker, finds
the candidates (for example idle cargo targets) and proves each one unused. The worker picks from that list. The helper
turns the pick into a plan with every target pinned by device and inode. The core hashes the plan, renders it, and asks
an approver with Discord buttons that name the plan's id. A click approves exactly that digest, once, before it
expires, and only then does the host run it. The helper runs its own stored copy of the plan, proves each item again,
and deletes with fd-relative calls that never follow a link. Every query, plan, click, action and byte freed goes to the
agent's hash chain and to the helper's own chain, which the agent cannot write. Build the code now, but enable it only
after the box has been moved to its own unprivileged user (`solos-agent` in `production.md`): while the agent still runs
as `debian`, it holds that user's passwordless sudo, and no helper boundary means anything.

## 1. The problem

In the owner's words (#104): some owner requests are host operations, not repository changes:

- freeing disk safely: survey, prove a path is unused, delete with a log;
- restarting a paused download;
- checking systemd units;
- editing an ops script with a backup.

"Sandboxed workers cannot see the host, so these fell back to unsandboxed Claude Code sessions." The ask is a host-ops
profile: an allowlist of commands and paths, a dry run first, an approval step for destructive actions (delete, unit
restart), everything logged to the trail, and no access to credential files.

**Acceptance test.** A "free 100 GB of idle cargo targets" request produces a dry-run list, waits for approval, deletes
only the listed paths, and logs the bytes freed.

The context that makes this concrete comes from the sibling issues. The box runs an l2Book downloader as a transient
user unit (#107), and a job with a 950 GB free-disk floor (#100). So the everyday story is: the disk fills, the
downloader pauses at its floor, the owner asks the agent to free 100 GB of idle cargo targets and then restart the
download.

## 2. What the system does today

| Fact | Where |
|---|---|
| `agent serve` runs as a systemd **user** unit. Production names the user `solos-agent`, gives it no sudo and no extra groups, and keeps its files under `~/agent`. | `docs/agent-mode/production.md:1-10`, `production.md#run-the-agent-as-its-own-user`, `production.md#units` |
| Before the migration the agent runs as `debian`, the box's main user, with passwordless sudo, and owns the other production services. The migration is on `main` but has not been run on the box. | `production.md#run-the-agent-as-its-own-user`; `ops/harden-units` (#117) |
| The agent's user unit now sets `NoNewPrivileges`, a seccomp filter and `ProtectHome=read-only`, and writes only `~/agent/state`. These limit the agent's processes, not what its user owns. | `scripts/systemd/solos-agent.service:46-64`, `production.md#hardening-the-units`, `production.md#what-hardening-does-not-do` |
| Every token rests in `~/agent/tokens`, mode 0600, owned by the agent's own user, and the unit copies them into its runtime directory. | `production.md#store-a-token`, `scripts/systemd/solos-agent.service:8-12`, `:31` |
| A worker's `run_code` sandbox binds `/usr`, all of `/etc`, `/lib` and `/lib64` read-only, the Ticket's directory at `/work`, and only the extra mounts and files the owner configures. | `src/code.rs:679-711`, `src/bwrap.rs:9` |
| Host file writes on a worker-reachable tree used path strings, and a racing job could swap a symlink in (security review, finding s01-1, proven). | `src/code.rs:787-845` |
| `agent prune` plans, then asks the Store again before it removes; it removes by path, links not followed at the leaf. | `src/agent/prune.rs:235-285`, `:423-444`, `:514-537` |
| A budget approval is an Engine command: the actor must be a person the Policy lets approve, and the Ticket must be waiting. | `crates/agent-core/src/engine.rs:222-250`, `crates/agent-core/src/policy.rs:38`, `:138-140` |
| A vault first use is asked once per item and answered only by a click, from `answerable_by` or the approvers. | `crates/agent-core/src/engine.rs:626-721` |
| What a message asks is typed (`Ask::Budget`, `Ask::Secret`), and buttons show only while the ask stands. | `crates/agent-core/src/model.rs:691-699`, `:511-519` |
| A Discord button's `custom_id` is an action, `:`, the Ticket id and an argument, at most 100 characters. The click becomes a `Command` with the Discord user. | `src/agent/discord/mod.rs:72-79`, `src/agent/discord/channel.rs:182-208`, `src/agent/discord/source.rs:326-372`, `crates/agent-core/src/model.rs:53-55` |
| Commands are applied in one place, and a refusal is the reply. | `src/agent/runtime.rs:629-680`, `src/agent/ports.rs:556-566` |
| Only Slack and Discord take approvals, and only with an approver on that source. | `src/agent/config.rs:1690-1708` |
| An attention message (a question or an ask) is cut to 500 characters on Discord. | `src/agent/discord/channel.rs:16`, `:128-139` |
| Outside effects are written to the chain before they happen: a keyed request, a browser step. | `src/agent/broker/secrets.rs:66-78`, `crates/agent-core/src/model.rs:834`, `:857` |
| The event log is hash-chained and verifiable, but nothing anchors its head outside `agent.db`. | `crates/agent-store/src/chain.rs:76`, `docs/agent-mode/security.md:93-94` |
| A seat can be given a narrow toolbox: the dream holds five tools. | `src/agent/broker/toolbox.rs:19-25`, `:121-133`, `:343-348` |
| The Gate admits a System actor (a schedule, the dream) as explicit, and a resume revalidates it without a list. | `crates/agent-core/src/policy.rs:100`, `:121-128` |
| The health check already asks `systemctl --user show` about the agent's own unit. | `src/agent/health.rs:73`, `:698` |
| The Store's schema is at version 9, and a new Ticket field, Input kind or event bumps it. | `crates/agent-store/src/schema.rs:4`, `docs/agent-mode/architecture.md:128` |

## 3. The boundary: what the agent may do alone, and what stays the owner's

Issue #104 draws one line: host work is a request a person answers, not a capability the agent holds. This design makes
that line four gates, each of which can refuse on its own.

| Operation | Who may start it | What still has to happen | What enforces it |
|---|---|---|---|
| Survey idle cargo targets, read a listed unit's state, read a listed ops file | A worker on a host-ops Ticket that a listed requester started | Nothing: the helper answers a read-only query | Typed tools with no path or command argument; the helper's config lists the roots, units and files; `HostQueried` on the chain |
| Delete surveyed targets, restart a listed unit, edit or restore a listed ops file | The worker proposes; the helper plans; only a listed approver's click approves | The helper re-proves each item against its pins and runs its own stored copy of the plan | One digest-bound click, fd-relative apply, the helper's sandbox, polkit per unit, the agent's hash chain and the helper's own |
| Deploy a release, edit `agent.yaml`, change a systemd unit or drop-in, change the polkit rule or the helper's config, install a package, use sudo, read a token, change a user, a mount or the firewall | The owner only | The owner runs it in a shell on the box, or `scripts/agent-deploy` from the Mac | The helper has no such action. The agent cannot write its user units (`ProtectHome=read-only`, `ProtectSystem=strict`), and the helper's binary, units, config and polkit rule are root-owned |
| Anything outside the helper's config | Nobody | An owner edits `/etc/solos-hostops/hostops.yaml` (and, for a unit, the polkit rule), then restarts the helper | The config is root-owned, mode 0640, group `debian`; the agent's user cannot read or write it, and `check-unit` refuses a unit that does not match it |

**What the agent already does on the host, and keeps doing.** `agent serve` writes its own state directory; `agent prune`
removes only finished work there; `agent health` reads its own unit's state; `agent publish` reads `agent.db`; a
repository Ticket's host steps run `git` and the package manager on that Ticket's clone. Host ops widens none of this.
It adds one narrow, typed door to the resources owned by `debian`, and only after a person clicks.

**The deploy boundary.** `scripts/agent-deploy` runs on the Mac as the owner (or as `AGENT_USER`), builds a release,
copies the user units into `~/.config/systemd/user`, and restarts them. The agent cannot run it, edit those units or
replace its release (`production.md#hardening-the-units`). The helper is not installed by a normal deploy: its binary,
system unit, socket unit, config and polkit rule go in root-owned with sudo, in the owner-run step of section 10. An
agent request to change any of them is not in the tool schema, so it is refused before a model can argue for it.

**What a worker can never name.** A path to delete (it names a survey id), a command, a unit outside its status or
restart list, a template instance, an ops file outside the list, a root outside the config, an approver, a second
plan while one waits, or another Ticket's survey id. Section 7.7 lists where each refusal lives.

## 4. Threat model

**Who can push on it.**

- **An outsider**: a GitHub commenter on a public repository, a Discord member who is not in `policy.people`, a web page
  a worker fetches. Production runs `policy.accept_mode: off`, so an outsider cannot start work, but their text can
  reach a worker that someone else started.
- **A hostile worker**: any worker may be prompt-injected. Assume it calls every tool it holds with the worst
  arguments, as often as it is allowed to, and writes misleading text.
- **The Router**: it reads Trigger text, so it may be steered into accepting a request with the wrong profile.
- **A compromised agent process**: a worker that escaped bwrap (s01-1 was a proven host write) runs as the agent's user,
  can write `agent.db`, and can talk to anything that user can reach. After the migration that user is `solos-agent`,
  with no sudo and no other services; before it, that user is `debian`, and this design is not safe to enable.

**What is at stake.** Credentials (`~/agent/tokens`, `~/.ssh`, the DeepSeek key in `~/.pi`, the Modal and GitHub CLI
tokens), sudo (which is root), the other production services and their units, the 6 TB research data, the owner's
ops scripts, and the agent's own record of what it did.

**The rule this design follows.** Each layer stops one attacker, and none relies on the layer above it:

1. The person's click stops a hostile worker. Nothing destructive runs without it.
2. The helper's own allowlist and invariants stop a compromised agent process. Even with forged approvals, it can only do
   what the helper's root-owned config allows.
3. The Unix user split and the helper's mount namespace keep credentials and sudo away from both. This is host
   enforcement, not a prompt.

## 5. Three shapes, and what a hostile worker does with each

### 5.1 (a) A general host shell with a command and path allowlist

The worker gets `host_exec {argv}`. The broker checks `argv[0]` against a command list and every path-like argument
against path prefixes, then runs it on the host as the agent's user, after an approval for "destructive" commands.

What a hostile worker does with it:

- **Arguments bypass the command list.** Allowed commands run other commands: `find ~/work -exec sh -c ...`, GNU
  `sed -i '1e curl ...'` (the `e` command executes), `awk 'BEGIN{system(...)}'`, `tar --to-command`, `tar -C ~ -x`
  over `~/.bashrc`, `rsync -e`, `xargs`, `env`, `timeout`, `nice`, `git -c core.sshCommand=...`. Each needs its own
  rule, and one missed flag is code execution. Any execution as `debian` today reaches `sudo`, so one miss is root.
- **Path tricks beat a prefix list.** `~/work/../.ssh/id_ed25519`, a symlink `~/work/x -> ~/.ssh`, `/proc/self/root/...`,
  a hard link, an argument the checker did not see as a path (`--files0-from=`, `-T`, `@file`), or one that starts
  with `-`.
- **TOCTOU.** The checker resolves a path, then the command resolves it again later. Anything that can write in the tree
  can swap a directory for a symlink between the two.
- **Units run arbitrary commands.** `systemctl --user restart x` runs whatever `x`'s unit file and drop-ins say, and
  `systemctl --user edit`, `set-environment LD_PRELOAD=...` or `link` change that. An allowed edit to the unit file or
  to the script it runs, followed by an allowed restart, is code execution.
- **Credential reads.** Any read command on `~/agent/tokens/model.key` returns the key to the model, which can then
  print it in its report or send it out with `fetch_url`.
- **Approval.** The person approves a command line. Discord shows them 500 characters of it (`channel.rs:16`).

The owner configures commands, path prefixes and per-command argument rules, forever. About 1,500 lines of code, plus a
checker that is a deny list in disguise. **Rejected.**

### 5.2 (b) Purpose-built, typed host tools

The worker gets `disk_survey`, `delete_paths {paths}` (with an approval step), `unit_status` and `unit_restart` for an
allowlist of units, and `edit_ops_file` with an automatic backup. The broker runs them.

What a hostile worker does with it:

- **No commands, so no argument bypass.** The only programs run are fixed argv such as
  `systemctl restart -- <exact allowlisted name>`.
- **Path tricks** remain if `delete_paths` takes paths from the worker. The checker must normalize, refuse `..`,
  absolute paths outside the roots, links at any component, and mount crossings. Every check is a chance to be wrong.
- **TOCTOU between dry run and delete** remains unless the delete pins what was shown and re-checks it with fd-relative
  calls. A path-based delete like `prune.rs:423-444` is exposed to a parent swap.
- **Units still run arbitrary commands.** Restarting an allowlisted unit runs its current definition. If the unit file,
  a drop-in, the user manager's environment or the script it runs changed since the approval, the person approved
  something else. `edit_ops_file` plus `unit_restart` is code execution by design.
- **Credentials.** If the tools run inside `agent serve`, they run as the agent's user, which today owns the tokens and
  holds sudo. The tool code is the only thing between the worker and both.

The owner configures roots, units and ops files. About 2,500 lines. Much better than (a), but where the code runs and
who picks the paths decide whether it holds.

### 5.3 (c) A plan that a person approves item by item, executed by the host

The worker writes a plan of host actions. The person approves each item. The host runs exactly the approved items.

What a hostile worker does with it:

- **If items are free shell**, (c) has every problem of (a), and adds approval fatigue: the person reads shell for each
  item, with obfuscation (`rm -rf ~/work/x/../../.ssh`), long lines cut at 500 characters, and bidi or homoglyph tricks
  in names.
- **If the description is model text**, it can lie ("remove a stale cache" beside a path to `~/.ssh`).
- **Per-item buttons** do not fit: Discord gives five rows of five buttons per message, and 30 approvals for one request
  teach the person to click without reading.
- **TOCTOU** is the same as (b): an approval of a path is not an approval of whatever is at that path later.
- **What it gets right**: a plan is the dry run, the person sees the whole set before anything happens, and the host
  executes, not the worker.

The owner configures approvers. About 1,000 lines with free-text items. Its safety is the reader's attention.

### 5.4 Side by side

| Attack or question | (a) host shell | (b) typed tools | (c) item-by-item plan | Recommended |
|---|---|---|---|---|
| Command allowlist bypassed through arguments | Root, via one missed flag and sudo | No commands run | Root, if items are shell | No commands run; fixed argv only |
| `..`, absolute paths, odd names | Prefix checks, bypassable | Checker on worker paths | Person reads paths | The worker names survey ids, never paths |
| Symlink at any component | Followed | Depends on the code | Followed at run time | `openat2` with `RESOLVE_BENEATH`, `NO_SYMLINKS`, `NO_XDEV` |
| TOCTOU between dry run and delete | Wide open | Open unless pinned | Open | Device and inode pinned, proof repeated, rename to trash, fd-relative delete |
| A unit that runs arbitrary commands | Edit, then restart | Restart runs the current definition | Same | System units only, through a polkit rule per unit; each must run with `NoNewPrivileges` and without the user manager; every file it runs pinned by digest |
| Credential files | Readable as the agent's user (its own tokens, and `debian`'s before the migration) | Same | Same | Different user; the helper's home is an empty tmpfs with only its roots bound in |
| Approval replayed on another list | n/a | Possible if the approval names a Ticket | Possible | The click names a plan id bound to one digest, once, with an expiry |
| Misleading approval text | Model text | Model text | Model text | Host-rendered from typed items, escaped |
| An outsider starts host work | Through any accepted Ticket | Same | Same | Only listed requesters, explicit, from a button channel |
| A compromised agent process | `debian` plus sudo: root | The agent's own user after the migration (no sudo, no other services) | Same | The agent's own user, plus the helper's typed actions only |
| What the owner configures | Commands, prefixes, argument rules | Roots, units, files | Approvers | Roots, units, files (helper side); requesters (agent side) |
| Code | about 1.5k lines | about 2.5k | about 1k | about 3.5k, plus 2.5k of tests |

## 6. Credentials and blast radius

**Does it need the dedicated agent user?** Yes. Before the migration the agent's user owns the tokens
(`production.md#store-a-token`) and has passwordless sudo. A guarantee such as "no access to credential files" cannot be
enforced against a process that runs as the owner of those files, and a helper that runs as the same user as its client
adds nothing: the client can do the action itself. After the migration (`production.md#run-the-agent-as-its-own-user`),
the agent runs as `solos-agent`, with no sudo rule and no write access to `debian`'s home; `agent serve` also refuses to
start with `host_ops` set unless it cannot open that home at all (section 7.7). The split is on `main` (#117); it still
has to be run on the box.

**Does it need a separate privileged helper?** Yes, for a second reason too. After the migration the agent's user can no
longer touch `debian`'s cargo targets, units or ops scripts at all. Something has to cross that line, and it must be a
small program with a typed interface, not a shell.

**Who the helper runs as.** `debian`, the owner of the resources, not root. A root helper could delete any user's files,
and a bug in it would be root. Everything this issue names belongs to `debian`.

**Why `NoNewPrivileges` alone does not keep sudo out.** `NoNewPrivileges=yes` binds one process tree. `debian`'s user
manager (`user@1000.service`, the box's `debian` uid) is another tree, and it runs without that flag. Any process of
`debian` that can reach the user manager's socket (`/run/user/1000/systemd/private` or the user bus) can ask it to start
a transient unit, such as `systemd-run --user sudo ...`, and the manager starts it with full sudo. A helper that
restarted user units would need that socket, so a code-execution bug in the helper would be root, and so would an edited
script in a restarted user unit. So the helper never talks to the user manager. The units it may see and restart are
**system** units with `User=debian`, and the helper restarts them over the system bus, where a polkit rule allows
`debian` to `restart` exactly the listed unit names and nothing else
([`sketch/50-solos-hostops.rules`](sketch/50-solos-hostops.rules)). Each such unit must itself run with
`NoNewPrivileges=yes` and without `/run/user` (`ProtectHome=yes` or `tmpfs`, or `InaccessiblePaths=/run/user`), so what
it runs cannot reach the user manager either. The helper checks both before a restart. A root drop-in that sets
`NoNewPrivileges=yes` on `user@1000.service` would close that path for every `debian` process, but it also stops `sudo`
in the owner's own login session; [decision 13](#13-decisions-for-the-owner) recommends against it.

**What the helper may do. Nothing else.**

1. List candidate directories of an allowed kind under the configured roots, with sizes, times and an "unused" proof.
   It never returns file contents, except (3).
2. Delete a directory it listed, after proving it again, by fd-relative calls.
3. Read and replace a listed ops file, keeping a backup in its own state directory.
4. Run `systemctl show` on the system units its config lists for status, and `systemctl restart` on the ones it lists
   for restart, which the polkit rule allows.
5. Write its own log, plans and backups under `/var/lib/solos-hostops`.

**What it may not do, enforced by systemd and checked again in code:**

- execute anything but `/usr/bin/systemctl` with fixed argv;
- gain privileges: `NoNewPrivileges=yes`, no capabilities, and no access to `/run/user`, so no path to the user
  manager;
- see credentials: the home directory is an allowlist, not a deny list. `ProtectHome=tmpfs` gives the helper an empty
  `/home` and `/run/user`, and `BindPaths=` puts back only its roots and ops-file directories. `~/.ssh`, `~/.pi`,
  `~/.config/gh`, `~/.config/solana`, `~/.kube`, shell history and everything else in the home are not in its namespace;
- write outside its roots, its ops-file directories and its state directory (`ProtectSystem=strict`);
- use the network: `PrivateNetwork=yes`, `RestrictAddressFamilies=AF_UNIX`;
- follow a symlink, cross a mount, or edit a file with more than one hard link;
- talk to anyone but the agent's uid (`SO_PEERCRED` on each connection).

Its binary, units, polkit rule and config are root-owned, so neither the agent's user nor the helper can replace them.

**Blast radius, by attacker.**

| Attacker | Today (no host ops) | With host ops, after the migration |
|---|---|---|
| Outsider | Nothing, with `accept_mode: off` | Nothing: a host-ops Ticket needs a listed person's explicit request from a button channel |
| Hostile worker on another Ticket | Its sandbox | The same: host tools are not in its toolbox, and its sandbox cannot see the helper's socket |
| Hostile worker on a host-ops Ticket | n/a | Reads survey lists, unit status and listed ops files; proposes a plan; the person sees host-rendered text and decides. No shell, no network, no credentials |
| Injected Router | Accepts wrong work | Cannot make a host-ops Ticket for an unlisted person: the core refuses it |
| Compromised agent process | `debian` plus sudo: root | Its own unprivileged user. Through the helper: delete proven-idle cargo targets under the roots, restart listed system units, edit listed ops files (with backups and the helper's own log) |
| Bug in the helper | n/a | `debian` without sudo, in a namespace that holds only its roots, no network, no user manager |

The weak spot left is edit then restart: an edited ops script run by a restarted unit is code execution as `debian`
inside that unit's sandbox. With `NoNewPrivileges` and no user manager it cannot reach sudo, but it can read and send
whatever that unit can. The pins in 7.2 detect a unit that changed; they do not contain what it runs. That is why edits
are off by default ([decision 10](#13-decisions-for-the-owner)).

## 7. The recommended design

Shape (b)'s typed tools, (c)'s plan-then-approve semantics, and a helper boundary. Three changes to (b) and (c) carry
the safety: the helper picks candidates, the click approves one digest, and the helper executes its own stored plan.

### 7.1 The acceptance test, step by step

1. The owner writes in Discord: "free 100 GB of idle cargo targets".
2. The Router accepts it as a session with `profile: host_ops`. The core checks that the Trigger is explicit and its
   actor is in `policy.host_requesters`, or refuses. The Mandate records `host_ops: true`.
3. The worker starts with the host-ops toolbox: `worker_report`, `worker_handback`, `host_survey`, `host_units`,
   `host_read` and `host_plan`. It has no `run_code`, `fetch_url`, browser, vault, `read` or memo tools.
4. It calls `host_survey {kind: cargo_target, min_bytes: 1000000000}`. The helper walks its roots, classifies,
   measures and proves, and returns rows such as
   `{id: sv_5c1e..., root: repos, path: "regimes/target", bytes: 22.0 GB, idle_days: 9, in_use: false}`.
   The call is recorded on the chain as `HostQueried`.
5. It calls `host_plan {delete: [sv_5c1e..., sv_9a07..., ...]}` with idle rows that add up to 100 GB. The helper proves
   each again, pins it, and returns a plan body. The broker hands it to the Engine, which recomputes the digest, stores
   the plan as `Waiting`, records `HostPlanProposed`, and sends one attention message with `Ask::HostPlan`:

   ```text
   Host plan hp_3f2a91c07d4e5b18 (expires 14:32 UTC). You asked: "free 100 GB of idle cargo targets".
   Delete 7 cargo target directories, 104.2 GB in all:
    1  "/home/debian/work/ci/target-slot-3"     31.4 GB  idle 12 d  no lock, no process inside
    2  "/home/debian/work/repos/regimes/target" 22.0 GB  idle 9 d   no lock, no process inside
   ...
   Nothing else is touched. sha256:3f2a91c0...
   [Approve hp_3f2a91c0: 7 deletes, 104.2 GB]  [Deny]
   ```

   The worker's call returns `{plan, status: waiting}`, and it ends its turn. Nothing has been deleted.
6. The owner clicks Approve. Discord names the user on the agent's own Gateway connection. The Engine checks that the
   user may approve host plans, that the plan waits, and that it has not expired. In one commit it marks the plan
   `Approved`, records `HostPlanAnswered`, and queues `Effect::Host`. A second click is refused.
7. The effect runner sends the helper `Apply {digest, approved_by, agent_head}`. The helper loads its own copy of that
   plan, writes `applying` with the agent's chain head to its log, and for each item opens the path again beneath the
   root, checks device, mount and inode, takes cargo's locks,
   checks that nothing was built in it since the plan and no process is inside, renames it into its trash on the same
   mount, checks the moved entry is the same inode, deletes it fd-relative, and writes `done` with the bytes freed.
8. The runner applies `Engine::host_done`: `HostActionDone` per item and `HostPlanDone {freed_bytes, free_before,
   free_after, helper_head}` go to the chain, and the worker gets the host-rendered result as an Input. It reports
   `ready`:
   "Freed 104.2 GB from 7 directories. Free space on /home went from 412 GB to 516 GB."

A target that a build touched between steps 5 and 7 is refused and kept, and the receipt says so.

### 7.2 The helper: `solos-hostops`

**Process.** A separate binary from a new workspace crate, `crates/hostops`, so the program that runs as `debian` is
small enough to review and is installed root-owned at `/usr/local/libexec/solos-hostops`. It depends on `agent-core`
for the plan types and digest, on `serde`, `serde_json`, `serde_yaml` and `rustix` (already in `Cargo.lock` at 1.1.5)
for `openat2`, `renameat2`, `unlinkat`, `statx` and `flock`. No async runtime, no HTTP, no new third-party crate.
`solos-hostops.socket` (root creates and owns `/run/solos-hostops/hostops.sock`, group `<agent group>`, mode 0660)
activates `solos-hostops.service` (`User=debian`, the sandbox in section 6). The protocol is one JSON request and one
JSON answer per line; `hello` names protocol version 1 and the helper's own id.

**Startup checks (it refuses to start otherwise).** Its config is owned by root, group `debian`, mode 0640. Its binary
is not writable by the agent's uid. `/proc/self/status` says `NoNewPrivs: 1`, and `/run/user` is empty. Its uid is
neither 0 nor `agent_uid`. `openat2` works. Each root is absolute, has no symlink component, is owned by the helper's
uid, is neither `/` nor a home directory, is not under a denied path, and neither contains nor sits inside another
root. Each ops file is checked the same way. `solos-hostops check-unit` also runs at start: the config's protocol
version is the one it serves; every root and ops-file directory is a `BindPaths=` entry in its own unit and nothing else
of the home is bound; and the polkit rule names exactly the config's restart units. Then it finishes any trash a crash
left, and logs `started`.

**Survey (`survey`).** It walks each root from a directory fd, opening each step with `openat2(RESOLVE_BENEATH |
RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV)` from its parent's fd, to a configured depth, in at most
60 s, and does not descend into a candidate once found. A directory is a `cargo_target` only if it looks exactly like
one, because a public repository can commit lookalike files: a `CACHEDIR.TAG` with exactly the bytes cargo writes, a
`.rustc_info.json`, at least one profile directory holding `.fingerprint/`, no `.git`, and no top-level entry outside
cargo's layout (`debug`, `release`, `doc`, `package`, `tmp`, target triples, `CACHEDIR.TAG`, `.rustc_info.json`, and
the like). For each candidate it records the root's absolute path, the relative path as components, device, mount id
and inode, allocated bytes (blocks, a file with several links counted once, as `prune.rs:514-537` does), the newest
mtime inside, and the unused proof. Each row gets an id, `sv_` and 16 hex digits of a hash of the Ticket, root,
components, device and inode. Rows are kept for 10 minutes per Ticket. A survey cut by the time limit says `partial`.
The helper's own trash is never listed.

**The unused proof**, for `cargo_target`:

- **Idle**: the newest mtime inside is older than the root's `idle_days`.
- **No cargo build**: cargo holds an `flock` on `.cargo-lock` in each profile directory (`debug/`, `release/`,
  `<triple>/<profile>/`) while it builds. The helper takes every one of them with `LOCK_EX | LOCK_NB`, at survey, plan
  and apply alike. One it cannot take means a build is running. PR 2 checks on the box whether a `build.build-dir` in
  any cargo config moves the lock out of the target, and refuses such targets if it does.
- **No process inside**: while it walks, the helper keeps the set of (device, inode) pairs in the tree. Then it stats
  every `/proc/<pid>/{cwd,root,exe,fd/*}` link and every file in `/proc/<pid>/maps` of its uid's processes, and a hit
  in the set means in use. Matching by inode, not path, works across mount namespaces (a build in bwrap or a
  container). A process of its uid whose links it cannot read (another gid, or not dumpable) counts as in use. Other
  users' processes cannot reach `debian`'s files after the migration.

**Plan (`plan`).** The request names the Ticket, the plan's number on the Ticket, the requester's words and the items:
survey ids, unit names or ops-file edits. The helper refuses anything it cannot resolve, proves each item again, and
builds the plan body (`HostPlanBody` in `sketch/01_core.rs`): version, Ticket, number, the requester's words, the
helper's id, planned and expiry times, and each item with its action and its pins. Pins are what apply checks again:

| Action | Pinned at plan time |
|---|---|
| `delete` | the root's absolute path, components, device, mount id, inode, bytes, newest mtime |
| `restart` | the unit name; sha256 of its fragment and each drop-in; of every file its `Exec*=` lines name (`ExecStartPre`, `ExecStart`, `ExecStartPost`, `ExecCondition`, `ExecReload`, `ExecStop`, `ExecStopPost`), program and script alike; of each `EnvironmentFile=`; its `WorkingDirectory`; the uid, gid and mode of each of those files and each parent directory, so a file the agent's uid could rewrite is refused; and its `NoNewPrivileges`, `ProtectHome` and `InaccessiblePaths` |
| `edit` | the file, device, inode, mode, sha256 of the old and the new content, and the unified diff |
| `restore` | the backup's id, and sha256 of the current and the restored content |

It stores the body at `/var/lib/solos-hostops/plans/<digest>.json` (mode 0600), logs `planned`, and returns the body
and the digest. A plan holds at most 25 items and at most one edit, and its rendering is at most 5,400 characters, so
the whole plan fits Discord's three parts of 2,000 characters (`channel.rs:18-23`) and nothing approved is ever cut. A
plan over any limit is refused, never shortened. At most 200 plan files are kept, the oldest unapplied ones pruned
first.

**Apply (`apply`).** The request names a digest, the approver and the head of the agent's chain. The helper loads its
own stored body for that digest and recomputes the digest from it, since other `debian` processes can write its state
directory. A digest it never planned is refused, a plan past its expiry (plus 10 minutes for the effect runner's
retries) is refused, and a digest it already applied returns the stored receipts, so a retried Effect is safe. It
applies one plan at a time. It logs `applying` with the agent's chain head, then per item `asked`, the action, and
`done`:

- **delete**: open the parent beneath the root, `statx` the entry without following, and check device, mount id and
  inode; take every profile's `.cargo-lock`; check that the newest mtime is not newer than the pin and that no process
  is inside; create `<root>/.solos-hostops-trash/` on the same mount (checked by mount id); `renameat2(...,
  RENAME_NOREPLACE)` the entry into it; `statx` the moved entry and, if its device or inode differs (a swap between the
  check and the rename), rename it back and refuse the item. Then delete the moved tree: each subdirectory is opened
  with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_XDEV)` from its parent's fd, so a mount point,
  a bind mount of the same filesystem included, is refused rather than entered; each entry goes with `unlinkat`; a
  read-only directory is made writable with `fchmod` on its open fd first (prune's `make_writable`, `prune.rs:446-461`,
  does this by path, which the helper must not copy); then release the locks. Bytes freed are the blocks of every
  entry it unlinked whose link count was 1. A symlink in the tree is unlinked, never followed. A hard link to a file
  outside frees nothing and leaves the file. An entry it cannot remove (a file another user owns) makes the item
  `failed` with the bytes freed so far, and what is left stays in the trash, named in the receipt.
- **restart**: compute every pin again and refuse on any difference; run
  `/usr/bin/systemctl --no-ask-password restart -- <unit>` with a 60 s limit, which polkit allows for that name only;
  record the new `ActiveState`, `SubState` and `MainPID`. The pins detect drift up to that call. They do not contain
  what the unit runs; the unit's own `NoNewPrivileges` and lack of `/run/user` do.
- **edit**: open the file beneath its configured directory without following; check device, inode, mode, one link and
  the old digest; write the backup to `/var/lib/solos-hostops/backups/<plan>/<n>-<name>` (0600, fsync); write the new
  content to a fresh temporary file in the same directory (`O_CREAT | O_EXCL | O_NOFOLLOW`), give it the old mode with
  `fchmod`, fsync, `renameat` it over the file, fsync the directory.
- **restore**: the same, with the backup as the new content. The backup is named by its id, looked up in the helper's
  own table, never joined into a path.

It records `statvfs` free space before and after for each filesystem it touched, logs `applied` with its own log's
head, and returns one receipt per item (`done {freed_bytes}`, `refused {why}` or `failed {why}`) and that head. A
refused item does not stop the others, except that an item after a failed `edit` of the same file is refused.

**Units (`units`).** `systemctl show` over the system bus, for system units that match the status list (exact names or
`*` patterns), with a fixed property list: `ActiveState`, `SubState`, `Result`, `NRestarts`, `ExecMainStartTimestamp`,
`ExecMainExitTimestamp`, `ExecMainStatus`, `MainPID`, `MemoryCurrent`, `Description`, `ExecStart`. Reading needs no
privilege. Restart takes exact names only; a template instance is a unit name a worker would choose, so it is refused.
A unit is refused for restart when it is not a system unit with `User=` the helper's user, when it lacks
`NoNewPrivileges=yes`, when `/run/user` is reachable from it, when any pinned file or directory is writable by the
agent's uid, or when it is one of the agent's or the helper's own units. `debian`'s user units are out of reach on
purpose: seeing them needs the user manager's socket, which is also the way to sudo. Journal lines are not offered in
the first build ([decision 11](#13-decisions-for-the-owner)).

**Ops files (`read`).** Exact paths from the config, regular files of at most 256 KiB, UTF-8, one hard link, not under
any `systemd` directory. A file with a secret-shaped string (a private key block, `ghp_`, `github_pat_`, `sk-`, `AKIA`,
`xoxb-` and the like) is refused for reading and editing: ops files hold no secrets. Every answer from the helper, unit
properties included, passes the same scrubber. An edit whose diff is over 60 lines is refused.

**Its log.** `/var/lib/solos-hostops/log.jsonl`, one JSON record per line, each with the sha256 of the line before:
`started`, `surveyed`, `planned`, `applying`, `asked`, `done`, `applied`. The agent's user cannot write it. Each side
records the other's head: `applying` holds the agent's chain head, and `HostPlanDone` holds the helper's log head.
`solos-hostops log verify` checks the chain. `solos-swarm agent host-check --helper-log <path>`, run by the owner,
checks that every applied digest has an approving `HostPlanAnswered` in `agent.db` and matching receipts. This anchors
host actions outside `agent.db`, which `security.md:93-94` lists as open for the trail.

### 7.3 Approval bound to one digest

The approval path is the one budgets and vault items use. Only the object of the click is new.

- **The digest.** `HostPlanBody` is serialized as canonical JSON (struct fields in declaration order, no maps) and
  hashed: `Digest::of` in `crates/agent-core/src/trail.rs`. The plan id is `hp_` and the first 16 hex digits. The body
  holds the Ticket id, the plan's number on it, and the requester's words (clipped to 200 characters), so the same list
  asked twice is two plans and the quoted sentence is inside what the click approves.
- **The message.** `Ask::HostPlan { plan }`. The Engine renders it from the body alone with `host::render`, a pure
  function, so every character the person reads is fixed by the digest: the body carries each root's absolute path and
  the requester's own words, and the Engine enforces the 5,400-character cap on the rendering itself. It holds no model
  prose. The only words a model chose are an edit's new lines, which appear as the diff the person approves. Each path
  is quoted, and anything outside printable ASCII, a backtick, a quote, and every space that is repeated or at an end
  is escaped (`\u{...}`), so no markdown, bidi control, terminal escape or run of spaces can fake a column or change
  what the list shows. The list sits in a code block.
- **The buttons.** `solos_host_ok:<ticket>:<plan>` and `solos_host_no:<ticket>:<plan>`, 53 characters. The Approve
  button's label says what it approves, from the Store: `Approve hp_3f2a91c0: 7 deletes, 104.2 GB`. The click becomes
  `Order::HostPlan { plan, approve }`.
- **The check.** `Engine::answer_host_plan` requires a person and `Policy::may_approve_host(source, actor)`. When
  `policy.host_approvers` is set, that list decides, and each name must also be in `policy.approvers`; when it is not
  set, only a `policy.approvers` name who is also a `policy.host_requesters` name may click. The default is therefore
  never wider than the budget approvers. It also requires a plan in `Waiting` and a click before `expires_ms`.
  Approved, it queues `Effect::Host { ticket, plan, digest }` in the same commit. The effect carries the digest, and the
  helper runs only its own stored body for that digest.
- **No replay, no switch.** A click names one plan id, and a plan is approved once. A Ticket has at most one waiting
  plan, and the worker cannot replace it: `host_plan` is refused while one waits. Only Deny, expiry or the Ticket's end
  closes a waiting plan, so a worker cannot post plan B under the message the person is reading for plan A. A closed
  plan's buttons are refused (`Ticket::awaits` turns false, and the runtime drops the buttons on a resend). A worker's
  report on a host-ops Ticket that contains `Host plan hp_` is refused, so its own text cannot imitate a plan. There is
  no path from typed text, a GitHub comment, an MCP command or a worker's tool call to an approval.
- **Item by item.** One click approves exactly the listed set. To drop items, the person clicks Deny and says what to
  change; the worker proposes a new plan, and the new digest gets new buttons. Per-item buttons were weighed in 5.3 and
  rejected.
- **Where it cannot ask.** A host-ops Ticket whose channels cannot show the buttons (no Discord channel with a host
  approver) cannot propose a plan, and the broker says so at once, as the vault does (`security.md:74-75`).

### 7.4 Audit

Every action and its result goes to the agent's hash chain. New `DomainEvent`s:

| Event | Written when |
|---|---|
| `HostQueried {ticket, assignment, tool, args_sha256, answer_sha256}` | Each `host_survey`, `host_units` and `host_read` answer |
| `HostPlanProposed {ticket, plan, digest, items, bytes, expires_ms}` | The Engine stored a plan |
| `HostPlanAnswered {ticket, plan, digest, by, approved}` | A click; when approved, this is the write-ahead record of the apply |
| `HostPlanClosed {ticket, plan, why}` | Expired, or the Ticket ended while it waited |
| `HostActionDone {ticket, plan, n, outcome}` | Per item, from the helper's receipt |
| `HostPlanDone {ticket, plan, freed_bytes, free_before, free_after, helper_head}` | After the helper's answer, with the head of the helper's log |

The helper's own chain records the same plans and actions from the other side.

### 7.5 Config

`agent.yaml` names who may ask and approve, and where the helper is. It holds no allowlist, because the agent's user can
edit it.

```yaml
policy:
  people: [discord:<your id>]
  approvers: [discord:<your id>]
  host_requesters: [discord:<your id>]     # who may ask for host work; each must also be in people. Default: none
  # host_approvers: [discord:<your id>]    # who may click on a host plan; each must also be in approvers.
                                           # Default: the approvers who are also host_requesters
host_ops:                                  # absent: no host tools, and no Ticket can be host-ops
  socket: /run/solos-hostops/hostops.sock
  plan_ttl_mins: 30                        # at most the helper's own max_plan_ttl_mins
```

`/etc/solos-hostops/hostops.yaml` is owned by root, group `debian`, mode 0640, so neither the agent's user nor a
worker sandbox (which binds all of `/etc`) can read it. It is the allowlist.

```yaml
agent_uid: 1001                  # id -u solos-agent; the only uid that may connect
max_plan_ttl_mins: 30
roots:                           # must not overlap
  repos: {path: /home/debian/work/repos, kinds: [cargo_target], idle_days: 7, max_depth: 6}
  ci:    {path: /home/debian/work/ci, kinds: [cargo_target], idle_days: 2, max_depth: 2}
units:                           # system units with User=debian, NoNewPrivileges=yes and no /run/user
  status: [l2book-download.service, "regimes-*.service", "regimes-*.timer"]
  restart: [l2book-download.service]   # each also named in the polkit rule
ops_files: []                    # exact paths, for example /home/debian/work/regimes/ops/download.sh
deny: []                         # added to the built-in list, never replacing it
backups_keep_days: 90
```

The matching unit's `BindPaths=` lists the same roots and ops-file directories, and the polkit rule lists the same
restart units. `solos-hostops check-unit` compares the three and fails on a difference, so the config cannot ask for
more than the sandbox and polkit give. The unit, the rule and the config are examples in
[`sketch/`](sketch/); the real ones are installed root-owned in section 10.

### 7.6 Core and Store changes

In `crates/agent-core` (pure, as before), a new module `host.rs` and small additions:

- `HostPlanId`, `HostPlanBody`, `HostItem`, `HostAction`, `Pins`, `HostPlan`, `HostPlanState` (`Waiting`, `Approved`,
  `Denied`, `Closed`, `Done`), `HostReceipt`, `HostOutcome`, `render`, and the digest.
- `Mandate.host_ops: bool`, fixed at accept. `Ticket.host_plans: BTreeMap<HostPlanId, HostPlan>`. Both default to
  empty, so older rows read back.
- `Ask::HostPlan`, `InputBody::HostPlan {plan, approved, by}` and `InputBody::HostDone {plan, text}`,
  `Effect::Host(HostJob)`, and the six events.
- `Engine::{propose_host_plan, answer_host_plan, host_done}`. `create_ticket` (`engine.rs:2075`) refuses
  `host_ops` unless the strength is explicit, the requester is a person (the Gate admits System actors as explicit,
  `policy.rs:100`), the Trigger's source is one whose channel shows buttons, and `Policy::may_request_host` admits the
  requester. `revalidate` checks the same before a resume, System actors included (`policy.rs:121-128`). On a host-ops
  Ticket, only Inputs from people in `host_requesters` or the host approvers are delivered.
- `Policy::{may_request_host, may_approve_host}`. `AllowlistPolicy` gains `host_requesters` and `host_approvers`.
  `host_requesters` enters `version()` (`policy.rs:80-89`), since it decides who may originate work.
- `Ticket::stage` reports `Stage::Approval` while a host plan waits, so existing views and the Watchdog see it.

Store: the next free schema version (10, unless another PR takes it first), with an empty migration, as
`architecture.md:128` requires for a new Ticket field, Input kind and event. The differential walk covers the new
commands.

### 7.7 Broker tools, grants and the Router

- **Tools** (`src/agent/broker/registry.rs`, and `extensions/agent-tools.json` regenerated):
  `host_survey {kind, root?, min_bytes?}`, `host_units {units?}`, `host_read {file}`,
  `host_plan {delete?: [survey id], restart?: [unit], edit?: {file, replace: [{old, new}]}, restore?: {file, backup}}`.
  None takes a path string for a delete. `host_plan` returns the plan id, its items and `waiting`. None executes.
- **Grants.** A new `Resource::HostOps`, present when `host_ops` is configured, the helper answered at start, and the
  seat's Ticket is host-ops. A host-ops seat keeps only `HOST_OPS_TOOLS` through `Toolbox::retain`, as the dream does.
  Every other seat is refused the host tools with "this ticket is not a host-operations ticket".
- **Limits.** One waiting plan per Ticket, and `host_plan` is refused while it waits. At most 10 plans per Ticket, and
  one survey at a time per Ticket. The loop guard (`broker/repeat.rs`) applies as usual.
- **Router.** `router_decide` accepts `profile: host_ops` on a session with no `repo` and no remote `environment`, and
  refuses it on a swarm. Its prompt says host work exists, that only the owner asks for it in chat, and that the host,
  not the Router, decides whether a request may be host work. The core is the gate.
- **The client.** `src/agent/host_ops.rs` holds the socket client and the `Effect::Host` runner. At start, with
  `host_ops` set, `agent serve` refuses to run unless:
  - the socket's directory is root-owned and not writable by its own uid, so no other process put a socket there;
  - the helper's `hello` names protocol version 1, a uid of its own that differs from the agent's, and the agent's uid
    as `agent_uid`; an unknown version refuses the start rather than guessing;
  - the agent cannot open the helper user's home directory, which `hello` names;
  - its own uid is in none of the groups that grant sudo (`sudo`, `wheel`, `admin`), and its own `NoNewPrivs` is 1
    (this alone proves little, since a user manager could start anything; the group check and the migration carry the
    weight);
  - no `executor.run_code` mount or file, and no node mount, exposes the helper's socket or `/run`, since any sandbox
    that can see the socket runs as the agent's uid and would pass `SO_PEERCRED`;
  - the Policy has a host requester and a host approver on a source whose channel shows buttons.

  The helper enforces the split from its side too: it will not start when `agent_uid` is its own uid, and it drops a
  connection whose `SO_PEERCRED` uid is not `agent_uid`.

### 7.8 Channels

Discord only in the first build, since production has no Slack. Two actions beside the four in
`discord/mod.rs:72-79`. An attention message with `Ask::HostPlan` is packed into up to three parts and never cut; if it
would be cut, it is posted without buttons with a line saying it cannot be approved here. The core's 5,400-character
cap keeps that from happening. Slack can follow with the same `Ask`.

### 7.9 Watchdog, status, health, dashboard and docs

- **Watchdog.** A waiting host plan is a wait for a person, timed from the plan's asked time, with its own text
  ("waiting for approval of host plan hp_..."), in `watchdog.rs:347` (the `waiting` function). A helper that does not
  answer shows as a failing Effect after three attempts, which the Watchdog already reports.
- **`agent status`.** Shows a waiting plan (id, items, bytes, expiry) and the last applied plan with the bytes freed.
  Lines come from `host::render`, so they carry no control characters (security review s08-2).
- **`agent health`.** A `hostops` check: with `host_ops` set, the helper's socket answers `hello` within 5 s.
- **Dashboard.** The publisher adds each Ticket's plans: rendered lines, state, approver, receipts and bytes freed.
- **Docs.** `security.md` gains "Host operations". `configuration.md` gains `policy.host_requesters`,
  `policy.host_approvers` and `host_ops`. `runtime.md` gains "Host operations". `production.md` gains installing the
  helper, its units and its log. A new `how-to/host-ops.md`. The `verify-agent-mode` skill gains a host-ops scenario.

### 7.10 How it is tested

- **Core** (`crates/agent-core`): the digest is recomputed and must match; a click on an expired, denied, closed or
  done plan is refused; a second click queues no second Effect; a bot, a system actor or a non-approver is refused; an
  unlisted requester or an inferred Trigger cannot make a host-ops Ticket; a resume rechecks the requester; Inputs from
  people not allowed are not delivered; `render` escapes control, bidi and non-ASCII characters; the differential walk.
- **Helper** (`crates/hostops`, Linux): classification by `CACHEDIR.TAG`; idle by mtime; a child holding
  `.cargo-lock` makes a target in use; a process whose cwd is inside makes it in use; a symlink at the root, at a parent
  or at the leaf, at plan or at apply, is refused; a directory swapped for another (new inode) is refused; a target
  touched after the plan is refused and kept; read-only subdirectories are deleted; a hard link to a sentinel file
  outside the root frees nothing and leaves the sentinel; a **race test** runs a thread that swaps a parent for a
  symlink to a sentinel tree 10,000 times while apply runs, and the sentinel tree is never touched; a denied path is
  refused even when configured; the log verifies and a tampered line is found; apply is idempotent by digest, refuses an
  unknown digest and an expired plan. Units use a fake `systemctl` path in tests: a changed fragment, drop-in or
  environment is refused, a template instance is refused, a unit without `NoNewPrivileges` is refused. Edits: an old
  string that is missing or appears twice is refused, a symlinked or hard-linked file is refused, a unit file is
  refused, a secret-shaped file is refused, a file changed since the plan is refused, the mode is kept, the backup
  equals the old bytes, a restore round-trips, and a kill between the temporary write and the rename leaves the
  original. Ignored tests run on the box: a mount crossing inside a `bwrap` user namespace, and a real scratch **system**
  unit, installed and removed by the test, for status and restart.
- **Agent** (`tests/agent_host_ops.rs`, `ScriptedHarness`, the helper in-process over a socket pair, clicks as
  `Fact::Command`): **the acceptance test**. Survey, plan, an attention message with exactly the listed paths, nothing
  deleted before the click, the click, only the listed paths gone (an idle target left out of the plan remains), and
  the chain holds `HostPlanProposed`, `HostPlanAnswered`, `HostActionDone` and `HostPlanDone` with the bytes freed.
  Then the hostile-worker matrix: a normal Ticket holds no host tools; a host-ops worker holds no `run_code` or
  `fetch_url`; a survey id from another Ticket is refused; a path string is refused by the schema; typed "approve" text
  and an MCP command approve nothing; a click on a closed plan is refused; a GitHub Trigger cannot make a host-ops
  Ticket. Startup refuses a helper with the agent's own uid, an unknown protocol version, and a process without
  `NoNewPrivs`. A helper that is down makes the Effect fail and the Watchdog report it, and no path falls back to a
  shell.
- **The box** (`verify-agent-mode`): scratch roots with fake targets, a real Discord click, then
  `solos-hostops log verify` and `agent host-check`. Then `systemd-analyze security solos-hostops.service`,
  `solos-hostops check-unit`, and probes that the helper cannot read `~/.ssh` or run `sudo`, and that the polkit rule
  refuses a unit outside the restart list.

## 8. Failure modes

Every row fails closed: the action does not happen, or it stops at the item whose pins no longer hold, and the person
sees a receipt.

| Failure | What happens | What bounds it |
|---|---|---|
| The helper is down, or its socket is not the root-owned one | `agent serve` refuses to start with `host_ops` set; at run time the `Effect::Host` retries three times and the Watchdog reports a failing Effect | No fallback to a shell; the plan stays `Approved` and can be retried when the helper is back, or expires |
| The helper crashes mid-delete | An item is renamed into `<root>/.solos-hostops-trash/` before it is deleted; on restart the helper finishes that trash before serving. An item that had not reached the rename is untouched; the receipt says which | Each item is independent; the log has `applying` and per-item `asked`/`done`; nothing outside the trash was touched |
| The person never clicks | The plan expires; the worker is told; nothing runs | `expires_ms` in the body and the helper's own expiry check; the buttons stop working |
| The worker is prompt-injected | It can read surveys and propose a plan of real, idle targets; it cannot name a path, a command or an approver, and it cannot make anything destructive happen | Typed tools, the digest-bound click, host-rendered text, and the helper's config |
| The agent process is compromised and forges an approval | It can ask the helper to apply a plan the helper itself built, within the config | The helper trusts only its own stored body; it cannot reach sudo, credentials or the user manager; its log is outside `agent.db`; the residual risk is in section 12 |
| A path is swapped for a symlink, a mount or a hard link, or is a different inode | The `openat2` flags refuse it, or the pin check or the post-rename `statx` refuses it; the item is kept and the receipt says `refused` | `RESOLVE_BENEATH`, `RESOLVE_NO_SYMLINKS`, `RESOLVE_NO_MAGICLINKS`, `RESOLVE_NO_XDEV`; device, mount and inode pins; the race test |
| A build or a process starts in a target after the survey | The cargo lock cannot be taken, the mtime is newer than the pin, or a process is inside; the item is refused and kept | The proof is repeated at plan and apply; the locks are held from the check to the rename |
| The disk fills during the delete | Each item frees its own blocks; a failure to unlink leaves the rest in the trash and the item `failed`; `free_before`/`free_after` show what happened | Per-item receipts; the helper needs no extra space on the target's filesystem |
| A unit changed since the plan | The pins do not match and the restart is refused; the worker proposes a new plan | Pins of the fragment, drop-ins, `Exec*` files, environment and sandbox properties; the polkit rule still limits the name |
| The unit restart fails | `systemctl` exits non-zero or times out at 60 s; the receipt is `failed`; the other items still run | Per-item receipts; polkit refuses an unlisted name; the helper does not retry a restart by itself |
| A unit has no `NoNewPrivileges` or can reach `/run/user` | The helper refuses the restart at plan and at apply | The unit's own hardening, checked from its properties (decision 9) |
| An edit then restart turns into code execution as `debian` | The edit is refused when no ops file is listed, or the diff is too large or secret-shaped; if enabled, the unit's `NoNewPrivileges` and hidden `/run/user` keep it out of sudo | Edits are off by default (decision 10); the helper's backups and log; section 12 states what remains |
| A plan is too long for Discord | The Engine refuses it at 5,400 characters instead of shortening it; a message that would be cut is posted without buttons | `MAX_RENDERED_CHARS`; the channel sends no buttons when it cannot show the whole plan |
| The config, the unit's `BindPaths` and the polkit rule disagree | `solos-hostops check-unit` fails at start, and `agent health` reports the helper down | The three are compared before anything is served |
| The helper or the agent is upgraded alone | The protocol version differs; the side refuses rather than guessing | `hello` carries version 1; the agent refuses an unknown helper version |
| Host ops is enabled before the migration | `agent serve` refuses: its uid has sudo or equals the helper's; the helper refuses `agent_uid` equal to its own uid | The startup checks on both sides (sections 6 and 7.7) |
| The Store schema moved | An older binary refuses the file | `architecture.md:128`; the rollback is to disable `host_ops`, not to downgrade the binary |

## 9. Dependencies and work in flight

State on 2026-11-08, after rebasing on `origin/main`:

| Branch | State | How this design depends on it |
|---|---|---|
| `ops/harden-units` | **Merged**, #117 | **Hard.** The dedicated agent user, `NoNewPrivileges=yes` on `solos-agent.service`, and no read access to `debian`'s home. The code is in `main`; running the migration on the box is section 10. |
| `fix/outsider-spend` | **Merged**, #112 | None hard. Host-ops Tickets need a listed requester's explicit request whatever the accept mode. It narrows the same door. |
| `fix/memo-trust` | On `origin`, no pull request at the rebase | None. A host-ops seat holds no memo tools, and a memo can grant nothing. |
| `feat/worker-file-tools` | Not on `origin` | The helper and that fix both need fd-relative file operations for s01-1. Whichever lands first owns the module (`openat2` beneath a directory fd, no links, no mounts), and the other reuses it. |
| `feat/cargo-deps` | Not on `origin` | None. Cargo caches under the agent's state belong to the agent's user, and `agent prune` frees them. The helper's roots never include the agent's state. |

## 10. Migration and rollout

The switch has five states, and each refuses to move early.

1. **The design lands; host ops is off.** `host_ops` is absent from `agent.yaml`, so no host tool exists and no Ticket can
   be host-ops. PRs 1 to 7 change nothing on a host that does not configure it.
2. **The box runs the agent as its own user.** Run the migration in
   [Run the agent as its own user](../production.md#run-the-agent-as-its-own-user). It creates `solos-agent`, gives it no
   sudo and no groups, moves `agent.yaml`, the tokens, the state and the backups, and redeploys the same revision as the
   new user. Check `id`, `sudo -n true` (refused), the unit's `NoNewPrivileges=yes`, and that `solos-agent` cannot open
   `/home/debian` (`sudo -u solos-agent ls /home/debian` must fail; if it does not, `chmod 0750 /home/debian` on the
   box). From here on the agent cannot write `debian`'s cargo targets, and the startup check refuses `host_ops` if it can
   read the home either.
3. **The helper is installed and still off.** The owner runs the new root step, `scripts/agent-deploy hostops-install`
   (PR 8), which runs the root commands over SSH: the binary to `/usr/local/libexec/solos-hostops`, the config to
   `/etc/solos-hostops/hostops.yaml`
   (root:debian 0640), `solos-hostops.{socket,service}` to `/etc/systemd/system/`, and
   `50-solos-hostops.rules` to `/etc/polkit-1/rules.d/`. Then `solos-hostops check-unit`,
   `systemd-analyze security solos-hostops.service`, the probes (`sudo` fails; `~/.ssh` and `~/.pi` are absent and
   unreadable), and `systemctl enable --now solos-hostops.socket`. With no agent connected, nothing runs; its log says
   `started`.
4. **Turn it on for one scratch root.** Add `host_ops` and `policy.host_requesters` to `agent.yaml`, keep `roots` to a
   scratch directory with a few fake `cargo_target`s, and restart. `agent health` must show the `hostops` check green.
   The owner asks from Discord for the scratch targets, clicks, and `solos-hostops log verify` plus `agent host-check`
   agree.
5. **Turn it on for the real roots.** Add the real roots, units and idle days, restart the helper, and run the
   acceptance test from issue #104: "free 100 GB of idle cargo targets", a dry-run list, the click, only the listed
   paths, and the bytes in the receipt.

**Rollback.** Remove `host_ops` from `agent.yaml` and restart: the host tools are gone, no new host-ops Ticket can be
created, and a waiting plan expires. Stop and disable `solos-hostops.socket` and `.service` if the helper itself is
suspect; the policy rule and config can stay root-owned and inert. In-flight work: an `Approved` plan's Effect retries
until the helper returns, and the Watchdog reports it; a plan past its expiry is refused and must be proposed again. The
helper's state directory keeps its log, plans and backups; deleting it is an owner decision, not part of rollback.

Do not roll back the binary below the schema this adds (10, unless another PR takes it first): the Store is forward-only
(`architecture.md:128`). To return to running as `debian`, turn host ops off first; `agent serve` and the helper both
refuse a uid split in which they are the same user.

## 11. PR plan

Each PR leaves `main` shippable, adds no third-party crate, and is checked on the remote box.

| # | PR | What it adds | The check |
|---|---|---|---|
| 1 | Host plans in the core | `host.rs` types, digest and `render`; `Mandate.host_ops`, `Ticket.host_plans`; `Ask::HostPlan`, the Input kinds, `Effect::Host`, the events; `Engine::{propose_host_plan, answer_host_plan, host_done}`; Policy fields and methods; the schema bump. Nothing creates a host-ops Ticket yet. | Core tests listed in 7.10, the differential walk, schema tests, and old rows reading back |
| 2 | The helper's delete path | `crates/hostops`: the fd-relative module, survey, classification, sizes, the unused proof, plan and apply for `delete`, the log. A library and tests only. | The helper tests for delete, the race test, the log tamper test |
| 3 | The helper's units and ops files | `systemctl` with fixed argv, the pins, `restart`; `read`, `edit`, `restore`, backups, the secret scrubber, the diff. Journal lines stay off. | Unit and edit tests with a fake `systemctl`; ignored box tests with a real scratch system unit |
| 4 | The helper as a service | The `solos-hostops` binary, the socket protocol, `SO_PEERCRED`, the startup checks, `check-unit`, `scripts/systemd/solos-hostops.{socket,service}`, and `scripts/polkit/50-solos-hostops.rules`. | Protocol tests; on the box, `systemd-analyze security`, `check-unit`, and probes that `~/.ssh` is hidden, `sudo` fails, and an unlisted unit cannot be restarted |
| 5 | Host tools in the broker | `host_ops` config, the startup checks in `agent serve`, the four tools, `Resource::HostOps`, `HOST_OPS_TOOLS`, `router_decide`'s `profile`, the Facts, the `Effect::Host` runner, `HostQueried`. | `tests/agent_host_ops.rs`: the acceptance test and the hostile-worker matrix; every existing exact-tool-list test unedited |
| 6 | Approval in Discord | The two actions, `Order::HostPlan`, never-cut rendering with buttons on the last part. | Golden bodies, the `custom_id` length test, a too-long plan posts no buttons, typed text approves nothing |
| 7 | Watchdog, status, health, dashboard, docs | 7.9, and `solos-swarm agent host-check`. | Watchdog, status and publish tests; the docs listed in 7.9 |
| 8 | Turn it on | After the box runs as `solos-agent`: the owner-run `agent-deploy hostops-install` step that installs the helper's binary, config, units and polkit rule root-owned with sudo, and the box scenario in `verify-agent-mode`. | The box run with a real Discord click on scratch roots, `agent host-check`, then the owner's real "free 100 GB" request |

Order: 1, then 2 to 4 (the helper) and 5 to 7 (the agent) can go in parallel. 8 waits for the migration in section 10.

## 12. What stays open

- **The helper trusts the agent's word that a person clicked.** A compromised agent process can approve its own plans.
  The helper bounds what that buys (idle cargo targets, listed units, listed files) and logs it where the agent cannot
  write, but it cannot tell a forged click from a real one. Discord signs interactions only when they arrive at an HTTP
  endpoint (Ed25519), not over the Gateway. Moving the buttons to such an endpoint would let the helper verify the click
  itself. That is a later step.
- **Edit then restart** stays code execution as `debian`, without sudo. Decision 10 keeps it off until wanted.
- **The unused proof** sees only the helper's own uid's processes, and the cargo lock is cargo's. A new kind needs its
  own proof.
- **A restarted unit runs as `debian` in the system manager**, outside the helper's sandbox. Its own hardening is the
  owner's, and the helper checks the pieces it can see.
- **The polkit rule is per user, not per program.** Any process of `debian` may restart the listed units, not only the
  helper. On this box that adds nothing to `debian`'s passwordless sudo, but it is a rule to revisit if `debian` ever
  loses it.
- **`debian` keeps passwordless sudo.** This design keeps the agent and the helper away from it. It does nothing about
  the owner's own unsandboxed sessions on the box.

## 13. Decisions for the owner

Each line is a choice with the option this design recommends. The defaults and migration in section 10 assume these
answers.

1. **May host ops run before the box is migrated to `solos-agent`?** *Recommended: no.* `agent serve` and the helper both
   refuse a state where the agent's uid is `debian` or equals the helper's.
2. **Who runs the helper?** `debian` in a root-owned system unit; root; or a new user with ACLs on your directories.
   *Recommended: `debian`*, so a bug is not root and everything in issue #104 is reachable.
3. **Who may ask for host work?** *Recommended:* `policy.host_requesters: [discord:<your id>]`. GitHub, MCP and
   schedules never start host work; each requester must also be in `people`.
4. **Who may approve a host plan?** *Recommended:* the same person, by default (`approvers ∩ host_requesters`). Add a
   `policy.host_approvers` list only if a second person should approve the owner's plans.
5. **One click or a click per item?** *Recommended: one click per plan*, with the whole plan rendered by the host. To
   drop items, click Deny and ask again.
6. **How long may a plan wait?** *Recommended: 30 minutes.* A plan approved after a longer pause is refused by the
   helper and must be proposed again.
7. **Where are the real roots, and how idle is idle?** *Recommended:* cargo targets only, under the real paths (for
   example `~/work/repos` at 7 days and the CI target slots at 2 days). Please name the paths and the thresholds.
8. **What may be deleted?** *Recommended:* `cargo_target` only in the first build. `node_modules` and data builds can
   follow, each with its own proof.
9. **Which units may be restarted?** *Recommended:* only the l2Book downloader, with `NoNewPrivileges=yes` and no
   `/run/user`; turn it from a transient user unit into a system unit file so it can be restarted after it stops, and
   name it in both the config and the polkit rule.
10. **Ops-file edits at first?** Edit then restart is code execution as `debian` (without sudo). *Recommended: no
    ops files listed at first*; add exact paths after the delete path has run for a week.
11. **Journal lines in unit status?** They can carry secrets and outside text. *Recommended: off.*
12. **Backups and trash?** *Recommended:* backups kept 90 days in `/var/lib/solos-hostops/backups`; delete at once (the
    point is freeing disk, and cargo targets rebuild), with at most 200 plan files.
13. **A `NoNewPrivileges=yes` drop-in on `user@1000.service`?** It would close the user-manager path for every `debian`
    process, but it also stops `sudo` in your own login session. *Recommended: no.* The helper never reaches that
    manager, and every restartable unit is required to be out of reach of it.
