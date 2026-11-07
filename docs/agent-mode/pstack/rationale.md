# pstack as the way solos-swarm codes and operates

> **Status, 2026-11-07.** Designed in #53. PRs 1 and 2 of [prs.md](prs.md) are built. PR 1 (#54) vendors pstack
> 0.15.36 pinned by its `MANIFEST`, enters every worker into `poteto-mode`, compiles in the adapter
> ([`docs/harness/solos-swarm.md`](../../harness/solos-swarm.md)), and adds the `read` tool over `/pstack`. PR 2 (#57)
> gives a Ticket a repository of `forge.repos`, cloned by the host into the Ticket's `work/`. The GitHub App design
> ([`../github-app/`](../github-app/rationale.md), #62) superseded PRs 3, 4 and 7 and the forge token. The App replaced
> the token (#63), and a worker's `ready` now delivers a pull request with no `pr_publish` tool (#75). PR 4's `check`
> tool and proof gate became the host's admission of the commits plus, per repository, an `evidence` command the worker
> runs (#83). PRs 5 (delegates) and 6 (two model families) are not built. Neither are `/method`, `/children`,
> `pstack.entries`, `pstack.models`, `pstack.roles` and `pstack.delegates`. The config takes only `pstack.entry`
> (default `poteto-mode`). A relaunched worker quotes its latest progress report since memory PR 1 (#82).

This is the design contract for making pstack the way solos-swarm does coding work. It came out of an architect bakeoff: two candidate designs (Opus and DeepSeek V4.1 Flash) written against the same task and grounding, scored by a DeepSeek cross-judge against a six-criterion rubric. The sketch files in `sketch/` and `adapter-draft.md` are the base candidate as written. **Where they differ from the [Amendments](#amendments) below, the amendments win.**

The package:

| File | What it holds | Lives at |
|---|---|---|
| `rationale.md` | This design | `docs/agent-mode/pstack/rationale.md` |
| `01_core.rs` | Repository, method, pull requests, proofs, the `ready` and merge gates | `crates/agent-core/src/change.rs`, plus `model.rs` and `engine.rs` |
| `02_registry.rs` | The eight new worker tools and two Router arguments, with schemas and replies | `src/agent/broker/registry.rs`, `extensions/broker.ts`, `tests/fixtures/agent-tools.json` |
| `03_pstack.rs` | The vendored tree, the entry block, the rule, the system prompt section, the reader | `src/agent/pstack.rs`, `vendor/pstack/`, `scripts/pstack-sync` |
| `04_delegate.rs` | Delegates: pool, workspaces, the job kind, the ledger part | `src/agent/delegate.rs`, `jobs.rs`, `crates/agent-store/src/ledger.rs` |
| `05_forge.rs` | The `Forge` port, the GitHub and local adapters, the CI watcher | `src/agent/ports.rs`, `src/agent/forge/` |
| `06_broker.rs` | The handlers, new facts, a seat's sandbox view | `src/agent/broker.rs`, `src/agent/broker/{pstack,forge}.rs`, `environment.rs` |
| `07_config_executor.rs` | agent.yaml, credentials, the launch, the Router | `config.rs`, `mod.rs`, `pi_executor.rs`, `pi_router.rs` |
| `adapter-draft.md` | The host adapter, pstack name to solos-swarm tool | `docs/harness/solos-swarm.md`, compiled into the binary |
| `prs.md` | The PR sequence | `docs/agent-mode/pstack/prs.md` |

## Problem

Gui wants pstack to be how solos-swarm does coding work and how it operates. A request from Slack, Discord, GitHub, MCP or a schedule should be grounded first, designed before it crosses a boundary, fanned out over diverse models, proven on the real surface, and shipped as a clean reviewed PR. New skills and playbooks must arrive without code changes.

The shape is not obvious because pstack and a solos-swarm worker are opposites. pstack is text that tells a capable parent agent what to do with its host: spawn typed subagents on chosen models, keep a todo list, read sibling files by relative path, work in worktrees, push, open PRs, wake on CI. A worker here is a broker-tool client with no file tools, no subagents, no repository in its own sandbox, one model, and a `ready` that ends the Ticket on the spot (engine.rs:414-418). The grounding's verdict holds: getting pstack into a worker is cheap, and pstack cannot do coding work there until the host adds a delegate, a workspace that can commit, and a publish path.

Constraints the design keeps, from the grounding and the task:

- pstack hides itself from Pi's skill list (`skills.js:279`), and the extension replaces Pi's system prompt (`broker.ts:267-279`). So pstack is entered by name, and the host does the entering.
- A reader is mandatory. poteto-mode routes to playbooks, principle leaves and references by path.
- One live Assignment per Ticket (`engine.rs:710-721`). Worker slots are held for a whole session (`pi_executor.rs:499-503`), so children that draw on `max_workers` deadlock their parents. Tool calls cap at 660 s (`broker.ts:159`).
- Workers stay keyless and offline. The forge token stays on the host. Page and repository text is data.
- The core stays pure: no I/O, no crypto types, Effects and events as data, one tool registry with drift tests, idempotent commands.
- Fan-out stays inside the Mandate's budget. The Router's daily cap is untouched. Above `auto_approve_microusd`, a person approves, as today.
- pstack is MIT and is shipped verbatim, pinned, with an update path. Adaptation happens in a host adapter and host tools, never by editing pstack.
- Two model families exist today: DeepSeek and, through Venice, GLM-5 (`upstream.rs:38-62`). No Opus or Grok is assumed.
- Everything tests offline: the scripted harness, a fake upstream, a fake forge.

## Usage (caller's view)

### (a) A person in chat: "fix the flaky retry test in solos-swarm"

In Discord, on a channel the agent listens to:

```text
gui      @solos fix the flaky retry test in solos-swarm
solos    Fix the flaky retry test needs approval first: a pstack session (poteto-mode) on
         GuiBibeau/solos-swarm with a budget of $3.00.                      [Approve] [Stop]
gui      [Approve]
solos    On it: Fix the flaky retry test.
         … about forty minutes. `agent status` and the MCP `agent_status` show the latest
         progress report, which is the worker's todo list: the Bug fix playbook's six steps,
         two of them done, "3. Plan the fix [doing]" …
solos    Goal: stop `retry_backoff_caps_at_max` failing one run in ten, without slowing retries.
         What was broken: the test advanced a paused clock before the retry task had armed its
         timer, so one run in ten saw the old deadline.
         Root cause: jobs.rs arms the wait after it reads the clock. Fix: read the clock once,
         after arming. Verification: 200 runs before the fix, 19 failures (log quoted); 200
         after, 0. The failing test lands first in history, then the fix.
         Principles: fix-root-causes (fixed where the race starts, no retry around the test),
         prove-it-works (the same 200-run repro, before and after).
         PR https://github.com/GuiBibeau/solos-swarm/pull/61 · head 3f2a9c1 · unit passed ·
         CI passed
```

Everything above the PR line is the worker's text, written per the Bug fix playbook's reply. The PR line is not. The Engine builds it from what the host recorded (01_core.rs `result_text`), so a link, a head or a pass in it is never the model's claim.

The person can stop there and merge by hand. Or, in the same Discord thread or on the PR itself:

```text
gui      ship it
         … a few minutes: the worker starts a verifier delegate on glm-5 against #61's head …
solos    Merge #61 into main? head 3f2a9c1 · unit passed · CI passed · verified PASS by glm-5
                                                                    [Merge] [Don't merge]
gui      [Merge]
solos    Landed #61 on main. Verified by verify-61 on glm-5 at 3f2a9c1, and read back after the
         merge.
         PR https://github.com/GuiBibeau/solos-swarm/pull/61 · merged as 7e1d0b2
```

"ship it" resumes the ended Ticket because Gui owns it and the Ticket subscribed to the PR's Thread when it published (`may_resume`, engine.rs:678-680). The worker runs the Shipping playbook. Its verifier is a delegate on the other family. The merge itself waits for a click, because typed text never grants authority here.

### (b) The worker model

The system prompt is today's worker prompt (pi_executor.rs:82-114), then the pstack section (03_pstack.rs `section`):

```text
pstack. You work the pstack way. Its skills are at /pstack (pstack 0.15.36, read-only): read
them with read, run their bash scripts with run_code. pstack is written in Cursor's names; the
solos-swarm adapter below maps them. Wherever a skill tells you to read the Claude Code
adapter, use this one instead.

# pstack in solos-swarm
… the adapter, verbatim …

Your pstack-models rule:
feature, refactoring: deepseek-flash
bug-fix: deepseek-flash
judgment and prose: glm-5
how explorer: deepseek-flash
how explainer: glm-5
arena runners: deepseek-flash, glm-5
arena cross-judge pool: glm-5, deepseek-flash
…
You run on deepseek-flash (family deepseek). The families here are deepseek and glm.

You are the lead of this ticket.
Repository: GuiBibeau/solos-swarm, base main. /work is a clone at main's tip, with its
history; commit there. Rust is at /rust/bin and its crates at /cargo; build offline.
Checks: unit, ci.
```

Its first prompt starts with poteto-mode, in the exact block Pi's own `/skill:` expansion builds (agent-session.js:1676-1677), then the assignment:

```text
<skill name="poteto-mode" location="/pstack/skills/poteto-mode/SKILL.md">
References are relative to /pstack/skills/poteto-mode.

# Poteto mode
…
</skill>

Ticket tk_5f1e9a0c2b7d4e11: Fix the flaky retry test

Scope:
The retry test in solos-swarm fails now and then. Find out why and fix it.
…
```

Tools: `worker_report`, `worker_handback`, `run_code`, `job_start`, `job_wait`, `job_stop`, `fetch_url`, `read`, `delegate`, `check`, `pr_publish`, `pr_view`, `pr_reply`, `pr_merge`.

An example stretch of turns:

```text
→ read {"path":"/pstack/skills/poteto-mode/playbooks/bug-fix.md"}
← {"path":"…/bug-fix.md","lines":"     1\t### Bug fix\n…","from":1,"to":38,"total":38}
→ worker_report {"status":"progress","text":"Goal: … Todo:\n1. Reproduce it yourself … [doing]\n2. Binary-search the cause …\n…\n6. Run Opening a PR."}
→ job_start {"code":"cd /work && for i in $(seq 200); do PATH=/rust/bin:$PATH CARGO_HOME=/cargo cargo test --offline -q -p solos-swarm retry_backoff 2>&1 | tail -1; done > /method/repro-before.txt"}
← {"job":"jb_81c2…","status":"running"}
→ delegate {"name":"explore-jobs","group":"how-1","role":"how explorer","model":"deepseek-flash","brief":"Goal: … Angle: how jobs.rs arms and waits on timers … Read /work/src/agent/jobs.rs …"}
→ delegate {"name":"explore-clock","group":"how-1","role":"how explorer","model":"deepseek-flash","brief":"… the paused test clock …"}
← {"job":"delegate:explore-jobs","status":"running","model":"deepseek-flash","family":"deepseek","workspace":"read","max_microusd":400000,"files":"/children/explore-jobs"}
   … turn ends. New input: the job's end, then one Note for group how-1 …
→ read {"path":"/children/explore-jobs/report.md"}
→ delegate {"name":"fix-1","agent":"poteto-agent","role":"bug-fix","workspace":"fork","brief":"… Read /pstack/skills/poteto-mode/SKILL.md first … the failing test first, then the fix …"}
   … its end comes back …
→ run_code {"code":"git -C /work fetch /children/fix-1/work HEAD && git -C /work merge --ff-only FETCH_HEAD && git -C /work log --oneline -3"}
→ check {"name":"unit"}
← {"job":"jb_9d0e…","status":"running","check":"unit","commit":"3f2a9c1…"}
→ pr_publish {"branch":"retry-flake","title":"fix(agent): arm the retry timer before reading the clock","body":"## Why\n…"}
← {"pr":61,"url":"https://github.com/GuiBibeau/solos-swarm/pull/61","head":"3f2a9c1…","created":true,"ci":"pending","next":"CI's end comes back to you as new input. …"}
→ worker_report {"status":"ready","text":"Goal: … What was broken: …"}
✗ 409 "ready needs a passing check at the head of each open pull request: #61 (3f2a9c1) has none.
       Run check, or report waiting with what is missing."
   … the turn ends; the Ticket is still open. New input: the check's end (unit passed, recorded),
     then "CI on #61 at 3f2a9c1: 9 passed." …
→ pr_view {"pr":61}
← {"verdict":"READY","proofs":{"checks":{"unit":true,"ci":true}},…}
→ worker_report {"status":"ready","text":"Goal: … What was broken: …"}
← {"recorded":true,"next":"Sent to whoever asked; the ticket has ended. Stop here."}
```

The refused `ready` is the gate at work: the check had not ended, so nothing proved the head yet. A worker that reports `ready` on a repository Ticket without publishing anything is not refused, since an investigation's answer is legitimate. But its result then ends with the host's line "No pull request was published.", so a link the model wrote into its own text cannot pass for a real one.

### (c) The operator

```yaml
executor:
  kind: pi
  provider: deepseek
  model: deepseek-flash                       # also the alias "deepseek-flash" in the rule
  prices: {input_microusd_per_million: 300000, output_microusd_per_million: 1200000}
  max_workers: 2
  run_code:
    mounts:                                   # toolchains the repository needs, read-only
      - {target: /rust, source: /home/agent/.rustup/toolchains/stable-x86_64-unknown-linux-gnu}
      - {target: /cargo, source: /srv/cargo-home}   # a CARGO_HOME with the crates vendored
    concurrency: 6
pstack:
  entries: [poteto-mode, how, why]
  models:
    glm-5: {provider: venice, model: zai-org-glm-5, prices: {input_microusd_per_million: 600000, output_microusd_per_million: 2200000}}
  roles:
    judgment and prose: glm-5
    how explainer: glm-5
    arena runners: [deepseek-flash, glm-5]
    arena cross-judge pool: [glm-5, deepseek-flash]
    interrogate reviewers: [deepseek-flash, glm-5]
  delegates: {max_live: 4, max_microusd: 400000}
forge:
  kind: github
  token_file: secrets/forge-token
  identity: {name: solos-swarm-bot, email: bot@example.com}
  repos:
    GuiBibeau/solos-swarm:
      about: The agent's own code, Rust and TypeScript.
      base: main
      budget_microusd: 3000000
      notes: Rust is at /rust/bin and its crates at /cargo; build offline.
      checks:
        unit: {command: "PATH=/rust/bin:$PATH CARGO_HOME=/cargo CARGO_TARGET_DIR=/checks/cache cargo test --offline --workspace --lib"}
        ci: {forge: true}
      merge: ask
```

```sh
# The forge token: a fine-grained token of the agent's machine account, on the listed
# repositories only: Contents read and write, Pull requests read and write, Checks and
# Commit statuses read. Never Workflows. It is not the github Source's token.
solos-swarm agent serve --config agent.yaml --state-dir /var/lib/solos \
    --pi-root /opt/solos-swarm --node /opt/node/bin/node \
    --credential-file secrets/deepseek-key --credential-file venice=secrets/venice-key
agent: pstack 0.15.36 at /opt/solos-swarm/vendor/pstack, tree sha256:7c1e…, adapter sha256:40aa…
```

Which pstack runs is the solos-swarm commit that runs: `vendor/pstack/` sits in the same checkout `--pi-root` names. To update, a maintainer runs `scripts/pstack-sync <upstream ref>` in a PR (see (d)). Models and roles are agent.yaml settings, so changing them takes a restart of `serve` and no change to pstack or to code. With no `pstack.models`, every role runs on the workers' own model, one family, which pstack handles as its own fallback. Without `pstack` and `forge` sections, agent mode behaves exactly as it does today.

### (d) A maintainer adding a pstack skill or playbook

- **A new playbook upstream, or a changed one.** Run `scripts/pstack-sync v0.16.0`. It copies `pstack/` from cursor/plugins at that ref into `vendor/pstack/`, rewrites `MANIFEST`, and stops with "plugin.json says 0.16.0 and the adapter was checked against 0.15.36". The maintainer reads the text diff of pstack in the PR, adds or changes adapter rows for any Cursor name pstack started to use, bumps `ADAPTER_CHECKED_AGAINST`, and pushes. No other code changes: poteto-mode routes to the playbook, and the worker reads it.
- **A new skill the Router should be able to enter directly**, such as `reflect` for a weekly schedule. Add its name to `pstack.entries`. The Router's prompt lists it with pstack's own description from the vendored frontmatter.
- **A new role or agent in pstack.** Nothing: the host derives roles from the vendored setup-pstack's rule block and agents from `/pstack/agents`. The operator may give the role a model in `pstack.roles`.
- **A pstack step that needs a host capability that does not exist.** Either a row in the adapter's skip table (one line, no code), or a new broker tool: the registry, `broker.ts`, the fixture, one adapter row, and the drift tests that hold those four together.
- **A repository-local skill**, such as a `verify-<app>` that create-verification-skill writes. It lives in the target repository at `.agents/skills/`, arrives through a reviewed pull request, and workers read it from `/work`. The host keeps no skill store of its own.

## Shape

### The six decisions

**1. Who picks the method: both, split by authority.** The Router picks what is authority and must survive a restart. It names the repository (`Mandate.repo`) and the pstack entry skill (`Plan::Session.method`), from operator lists, at accept time. `decide` fills the default entry when the Router names none, so the Plan never depends on config read later, and a recovery launch enters the same skill (per make-operations-idempotent). The worker picks the playbook, inside poteto-mode, because that routing table is pstack's text. Copying it into the Router would fork pstack into a prompt and a cheap triage model, and would need a code list of 23 playbook names that every pstack release could break. The Router changes in one way it must: its instructions now count a question about a repository as work, since today it ignores "questions you cannot act on" (pi_router.rs:46-47).

**2. A pstack subagent is a delegate: a broker-run child session that is a job.** `delegate` starts a separate Pi session for one brief and returns `{job}` at once. It is not a Ticket, not an Assignment and not a Goal. The Engine's invariants and the Router's authority stay untouched, and the existing job machinery gives the rest for free: its end reaches the lead exactly once (jobs.rs:7-12), as a Note between turns or through `job_wait` in one, and `job_stop` stops it. A `group` makes N delegates wake the lead once, which is the shape of every pstack fan-out (how, arena, interrogate, swarm). Four rules make fan-out safe (04_delegate.rs): depth one, so a delegate cannot delegate and pstack's "subagent forbidden to spawn" path applies; its own pool (`delegates.max_live`), so it never takes a worker slot and no one ever waits on a slot it holds; the Ticket's own ledger account with a per-delegate part cap checked in the same transaction, so the Mandate caps lead and delegates together and children's spend counts against the parent with no new accounting; and ownership by the Assignment, so a delegate dies with it. The lead picks the model by alias from its rule; the broker refuses an alias it lacks with the list of valid ones, which is exactly the error shape pstack's fallback rule reads (poteto-mode:100). The broker, not the session, decides which upstream a grant calls (`admit_request`, upstream.rs:163-167), so a delegate cannot run on a model the operator did not list.

**3. The method's state lives where pstack keeps it, with the core holding only proof.** The todo list is the text of the lead's `progress` reports: pstack says to update the list at each phase, a progress report is already stored on the Ticket, `agent status` already shows the latest one, and a recovery Launch now quotes it back. No todo tool, no new state. One rule keeps it true: a progress report the host makes on the lead's behalf (today's quiet-turn report while a job runs, and the new heartbeat while delegates run) repeats the lead's last own progress text and adds one host line, so the latest report always holds the list. The decision trail is `/method/decisions.tsv`, a host directory outside the repository that persists across Assignments, written with show-me-your-work's own `log.sh` through `run_code`. The playbook and its phase stay in the worker's context and its progress text. The core holds only what has authority or must be verified: the method, the repository, the pull requests, the proofs and the merge grants. A core that modelled playbook phases would put pstack's text in Rust, and every new playbook would be a code change.

**4. A change becomes a PR through the host, and the host never runs git where a sandbox writes.** A worker's `/work` is a repository whose config and hooks the worker controls, so host git there would run what the worker planted. The design keeps one rule: the host never runs git in a repository a sandbox can write, and never writes into a directory a sandbox can write. Every git command on a worker's tree runs in a `run_code` sandbox. What crosses to the host is a `git bundle` file, data, written into a fresh directory bound writable for that one sandbox call and opened by the host with `O_NOFOLLOW` as a regular file under a size limit. The host fetches it into its own bare mirror with `transfer.fsckObjects`, checks (descends from the base, no protected path, under the size limit), and pushes with the token in git's environment, never argv. Branches live in the Ticket's namespace `solos/<ticket>/<slug>`; a re-publish force-pushes only over the host's own last push (a lease), so a person's push is never overwritten. The PR is found by its head branch or created, ready, never draft. `PublishAsked` is on the chain before the push, as `KeyUsed` is before a keyed fetch (engine.rs:620-660). On first sight the Ticket subscribes to the PR's Thread, which is the "pull request bound to its Ticket" that P8 listed as later work. CI is watched by host code with no model: the watcher derives what to watch from the Store's open Changes, polls the forge, and records `PrSeen`; the Engine wakes the worker with one Note when CI at the head turns terminal. Merge is approval-gated: `pr_merge` runs `merge_gate` in the core (open, CI passed at the head, a passing check there, a passing review there from another family when one exists, and a person's click unless `merge: auto`), asks once with Merge and Don't merge buttons (the same machinery as a vault item's Allow and Deny), and merges only the head the gate saw, since GitHub's merge call takes the sha.

The workspace comes from the same place. For a Ticket with a repository, `Forge::prepare` clones from the host's mirror into an empty `work/` before any sandbox runs, with history and the agent's identity, so commits work in `run_code`. That replaces the documented `git worktree add` prepare, whose `.git` file points outside the sandbox (the grounding's inferred failure).

**5. Verification is enforced: `ready` on coding work needs host-recorded proof at each open PR's head.** `Engine::report` refuses `ready` while any open pull request of the Ticket has no passing check at its head (01_core.rs `unproven`). The refusal is the tool error the worker already handles. Proof comes only from the host: `check` runs an operator-named command, as a job, on a clean checkout of the commit (a `--shared` clone at that sha, in a cache directory no plain `run_code` mounts), so uncommitted edits or `assume-unchanged` tricks cannot make the committed code pass; or the operator names the forge's CI as a check (`ci: {forge: true}`). The record is committed before the worker sees "passed". Proofs are keyed by commit, so a check a delegate ran in its fork counts once the lead publishes the same commit, and a proof never follows a branch to a commit it did not see. A Ticket that published nothing is not gated, so an investigation's answer is still just an answer. This is pstack's prove-it-works in structure, not prose, per encode-lessons-in-structure: an agent that skips verification gets a refusal, not a warning. Shipping's independent verifier is also structural: a delegate started with `verifies` returns the review the merge gate needs.

**6. pstack is vendored verbatim, pinned by digest, updated by PR.** `vendor/pstack/` holds the upstream `pstack/` directory unchanged, its MIT `LICENSE`, its `plugin.json`, and a `MANIFEST` with one sha256 per file, the upstream ref and a tree digest. `agent serve` refuses to start on any difference. `MethodEntered` records version, tree digest and adapter digest for every session, so an audit can say which pstack shaped which Ticket. The adapter lives in solos-swarm (`docs/harness/solos-swarm.md`), compiled in, and records the pstack version it was checked against; `scripts/pstack-sync` and a test refuse a vendored version the adapter was not checked against. One way to choose a pstack version: the solos-swarm commit that is running.

### Data, and how it flows

The core gains five validated names (`RepoName`, `CommitSha`, `Entry`, `CheckName`, `BranchName`), three maps on the Ticket, and two gates (01_core.rs):

```text
Mandate.repo: Option<RepoName>            authority, fixed at accept
Plan::Session.method: Option<Entry>       the entry skill, filled at decide
Ticket.changes: PrRef -> Change           PRs as the forge last answered (head, state, CI, thread)
Ticket.proofs:  CommitSha -> Proof        checks run on a commit, reviews of a commit
Ticket.merges:  PrRef -> SecretGrant      a person's merge decision, final for the Ticket
unproven(&Ticket) -> Option<why>          the ready gate
merge_gate(&Ticket, pr, terms)            Go { head } | Waiting | refused
```

Trace the dominant access patterns through it. "Can this Ticket report ready?" reads `changes` for open ones and `proofs[head]` for each: two map lookups, no index to add. "Can #61 merge?" reads one Change, one Proof and one grant. "What does the person's result say?" is derived from the same two maps. "What should the watcher poll?" is open Tickets with an open Change whose CI is pending, which the Store already indexes as open Tickets. "Did this delegate verify the head that is merging?" is `proofs[head].reviews`, since the review was bound to the head when the delegate started.

A Ticket's directories:

```text
<ticket>/work          the repository        run_code /work rw (lead), read
<ticket>/method        the lead's notes      run_code /method rw (lead), ro (delegates), read
<ticket>/children/<n>/ work (a fork, rw to that delegate), report.md (host)   /children ro (lead), read
<ticket>/runs/<n>/     a delegate's session files                            no sandbox
<ticket>/host          host-written only     /host ro: checks/, base.bundle, repo.json, transcripts/ (per file)
<ticket>/checks        check cache           /checks rw, mounted only into check jobs
vendor/pstack          the pinned tree       /pstack ro, read
```

`read` and `run_code` use the same virtual paths, so a path one prints works in the other. The kernel enforces the roots (`openat2` with `RESOLVE_BENEATH`), so a symbolic link a sandbox plants or swaps cannot reach a host file.

### Interface depth

Eight tools carry everything (02_registry.rs). Each hides a lot behind a small schema:

- `read` hides the roots, the kernel's containment, bounds and binary detection behind a path.
- `delegate` hides the pool, the workspace copy made in a sandbox, a child seat with its own grants and prompt, the model table, the part cap, the job table's exactly-once end, the group barrier, the heartbeat that keeps the Watchdog from calling a waiting lead stalled, and the review record. The lead passes a name, a brief and optionally a model.
- `pr_publish` hides the sandboxed bundle, the mirror, fsck, ancestry and protected paths, the lease, the token, find-or-create, stacking, the binding to the Ticket, and arming the CI watcher. The lead passes a branch slug, a title and a body.
- `check` hides the clean checkout, the cache, the job, and the record that `ready` depends on.
- `pr_merge` hides the forge's current state, a five-condition gate, a person's click and the sha-pinned merge.

What stays exposed is what pstack's text already talks about: briefs, models, workspaces (read or fork), groups, branches, checks, pull requests. Waiting and stopping reuse `job_wait` and `job_stop`, so there is one way to wait on anything in the background. No tool exposes git, a token, a forge's wire type or a ledger.

### What it deliberately does not do

- No playbook, phase or todo state in the core, and no Router choice of playbook.
- No nested delegates, no delegates on remote nodes, no Claude Code delegates, no browser or vault in a delegate.
- No coding Ticket on a remote node: `decide` refuses a repository with a remote environment.
- No effort ladder: each alias runs at one level, and `upstream_body` keeps thinking off (upstream.rs:183-184).
- No change to swarm mode. pstack's **swarm** skill runs on delegates, not on `Plan::Swarm`.
- No pstack for the Router.
- No model-supplied evidence. A worker cannot attach a SHA, a verdict or a link that the host did not record.

## Synthesis decision

**Base: this design (candidate A).** The cross-judge scored it 28 of 30 against 15 for candidate B, and my own read agreed. A keeps every token out of every model and sandbox by one rule (the host never runs git where a sandbox can write; only a fsck-checked bundle crosses into a host mirror), runs operator-named checks on a clean checkout of the commit, gives delegates caller-keyed names, their own pool and group wakes, and binds a review to the head it reviewed. The judge found no token path and no deadlock in A.

B was rejected as the base on proven defects: its `publish` and `verify` ran host git in the worker-writable tree with the token in the environment (a planted hook or `pushurl` reaches the token), its `verify` ran a model-chosen command on the dirty working tree, its merge asked but never merged, child ids collided for two delegates on one model, and its pstack pin was self-referential.

**Grafted from B:**

1. **A typed obligation.** `Deliverable { Answer, Change, PullRequest }` and `Ticket::owed() = max(planned, reached)`. The Router sets it, an operator can set a repository default (`forge.repos.<repo>.owes`, e.g. `pull_request`), and publishing raises it. A's `ready` gate then reads `owed`: a Ticket that owes a change needs a passing check at the work's head even before it publishes, which closes A's unpublished-coding bypass.
2. **A FAIL at a commit stands, and the reviewer's brief is host-built.** `merge_gate` refuses while any review at the head is a FAIL, so a second reviewer cannot launder the first. `delegate {verifies}` receives a host-assembled brief (the PR's diff, base and recorded checks) under the lead's prose.
3. **Bounded CI repair after `ready`.** The CI watcher follows every Ticket with an open Change, not only open Tickets. A new CI failure on a PR the Ticket published resumes it through one typed exception, at most `forge.max_repairs` times (default 2), under the owner's Mandate.

## Amendments

These amend the sketch files and the adapter draft. Each comes from the cross-judge's defect list or a graft above.

| # | Amendment | Changes |
|---|---|---|
| 1 | `Deliverable` on `Plan::Session.method`, `owed()` on Ticket, repository default `owes`, publish raises it (graft 1). | `01_core.rs`, `02_registry.rs` (`router_decide.deliverable`), `07_config_executor.rs` |
| 2 | `ready` gate reads `owed`: `change` needs a passing check at `/work`'s head; `pull_request` needs a passing check at each open PR's head (and, with a second family configured, a passing review there). | `01_core.rs` `unproven` |
| 3 | `merge_gate` refuses while any review at the head is `Fail` (graft 2). | `01_core.rs` |
| 4 | Reviewer briefs are host-built from `pr_view` data, under the lead's prose (graft 2). | `04_delegate.rs` |
| 5 | CI watcher scope: Tickets with an open Change; bounded `forge.max_repairs` resume after `ready` (graft 3). | `05_forge.rs`, `01_core.rs` |
| 6 | With a `forge` repository configured, config load refuses an operator `prepare` that runs `git worktree add` into the work dir; the forge clone replaces it. | `07_config_executor.rs` |
| 7 | The adapter's drift test checks every worker tool is named, so the adapter must name `fetch_url`, `vault_list`, `worker_handback` and the browser tools. | `adapter-draft.md`, `02_registry.rs` |
| 8 | One-family fallback is stated as this host's adaptation, not pstack's: with one family, a role that needs another family runs fresh-context on the same family, the review is recorded with `same_family: true`, and the merge gate says so in its line. | `adapter-draft.md`, `01_core.rs` |
| 9 | Claude Code leads delegate to Pi children in v1 (stated in the adapter); Claude Code delegates are later work. | `adapter-draft.md` |
| 10 | Stacks: `merge_gate` refuses a PR whose base is another open PR of the Ticket until that base has merged (Shipping's "contiguous run from the bottom"). | `01_core.rs` |

## Scope of the first build

PRs 1 to 7 of [prs.md](prs.md), with the amendments folded into the PR that owns each file. Later, not scheduled: coding in Swarm Tickets, generated verification skills wired to the proof gate (a check that names a repository's `.agents/skills/verify-*` script), formal verification skills, delegates on remote nodes and on Claude Code.

## Tradeoffs accepted

- We accept depth-one delegation (no orchestrate sub-coordinators, no delegate that fans out) in exchange for a fan-out that cannot deadlock by construction and a pool rule that fits in one sentence.
- We accept that the host rebuilds Pi's five-line skill expansion instead of sending `/skill:poteto-mode`, in exchange for the same bytes on Pi and Claude Code, no `--skill` in either argv, and no golden argv test edited.
- We accept vendoring about 7 MB of pstack (563 files, 548 KB of it markdown) into the repository, in exchange for one commit that pins host and method together, updates whose text a reviewer reads in the diff, and nothing extra to ship to the verify box.
- We accept that a repository with no configured check cannot report `ready` once it opens a pull request, in exchange for a gate that no model text can satisfy. Config load refuses a repository with no check, so this shows up at start, not mid-Ticket.
- We accept a clean `--shared` clone and a cold-ish build per checked commit (the target cache persists per Ticket), in exchange for proof about the committed tree, not the working tree.
- We accept that "merge it" typed by the owner still needs a click, in exchange for keeping "text never grants authority" without exceptions. An operator who wants otherwise sets `merge: auto`, and the other four merge conditions still hold.
- We accept a second credential file for a second family, in exchange for real cross-family judging. A deployment on Venice alone already has both families with one key.
- We accept that the adapter is about 13 KB in every worker's system prompt, in exchange for pstack's full mapping always in view. The system prompt is identical across a session's turns, so it is served from the provider's prefix cache after the first.
- We accept that a delegate's progress is invisible to the person until it ends, in exchange for delegates that need no Channel, no Ticket and no approval path.
- We accept that delegates of one Ticket are one trust domain: a fork isolates state so candidates do not trample each other, not as a security boundary between them. The lead fetches a fork's commits in its own sandbox, never on the host, and reviews them before it publishes.
- We accept that a CI failure on a published PR can reach the lead twice, as the watcher's Note and as the github Source's bot Trigger on the PR Thread, in exchange for a watcher that works even where no github Source is configured. The adapter tells the worker they are one event.

## Alternatives considered

- **Child Tickets.** A worker tool that files a child Ticket per subagent, each with its own Assignment, Mandate and worker slot. It looks native, but it lets a worker mint Mandates, so either the Router judges every child (a Router turn per explorer) or a worker grants authority. Children would take `max_workers` slots while their parents hold theirs, which is the deadlock. And pstack's parent aggregates every fan-out, which a child Ticket's Result does not feed back into. It exposes Ticket lifecycle to the caller for no capability the job shape lacks.
- **A swarm Goal with a "selected" artifact mode and a `swarm_run` tool** (the grounding's explorer 3). It keeps swarm governance and run evidence. It hides less: arena's base pick moves into a host contract, Goals cap prompts at 8 KiB, grants are a closed set without `read`, scopes have no coding scope, and agents get private scratch, not the repository. Better later, as the Router-level fan-out for wide research, than as pstack's `Task`.
- **Claude Code native.** Run every pstack worker on Claude Code with `Read`, `Agent` and `TodoWrite` and `--plugin-dir /pstack`, and use pstack's own claude-code.md. Least code. But its subagents share one sandbox, one seat, one budget and one upstream model, so there is no isolation, no per-child cap and no family diversity, Pi workers get nothing, and a "Claude Code" worker is not running Claude anyway (messages.rs:41).
- **A method engine in the core.** Model playbooks as state machines with phases and gates in agent-core, so the host enforces each step. It would encode pstack's text in Rust, every new playbook would be a code change, and pstack's own rule that a step may be skipped with a reason would become a code path. It fails the growth requirement outright.
- **Publish after the Ticket.** On `ready`, the host commits `/work`, pushes and opens one PR. Simple and tokenless, but pstack's flows need the PR during the run: stacks, fix rounds, babysit, CI waits and Shipping. And `ready` would then carry no PR to verify.

## Answers to the grounding's open questions

1. **Who enters pstack?** The host, at every launch, with the entry the Plan stores. The Router names it from `pstack.entries` or gets the default. Later deliveries are not re-entered; the system prompt names the path so the worker can re-read it after compaction.
2. **How does the model read files?** A broker `read` tool over five roots (`/pstack`, `/work`, `/method`, `/children`, `/host`), kernel-contained, served to Pi and over MCP to Claude Code. `/pstack` is also mounted read-only in `run_code`, for pstack's bash scripts. Yes, it exposes the ticket's `work/`.
3. **What shape does fan-out take?** A worker `delegate` that is a job, depth one, with `read` (the lead's tree, read-only) or `fork` (a sandbox-made copy with the lead's uncommitted changes) workspaces. A Goal "selected" mode stays a later, Router-level option.
4. **How do model roles map?** Admit the second family that already exists, GLM-5 through Venice, with a second credential (or Venice alone). The operator's `pstack.roles` becomes the rule in each prompt; families are derived from admitted model ids in code. With one family, pstack's own fallback ("same family, fresh context, say so") applies, and the adapter says so.
5. **Where does publishing happen?** In broker tools the model drives, which are pstack's "built-in PR tool". The forge adapter on the host holds a token of its own; the model and sandboxes never see it. Pushes go to the Ticket's namespace with a lease; protected paths are refused; merges need a click unless the operator opts in. The PR is bound by subscribing the Ticket to its Thread on first sight.
6. **What does `ready` mean for coding work?** No new status and no evidence slot. `ready` is refused unless every open PR's head has a host-recorded passing check, and the Result carries a host-built line per PR.
7. **Where does pstack live?** Vendored verbatim in solos-swarm at `vendor/pstack/` with its MIT license and a MANIFEST, verified at start, updated by `scripts/pstack-sync` in a PR. Per-repository `.agents/skills/` is for that repository's own skills only.
8. **Where do generated verification skills go?** Into the target repository at `.agents/skills/verify-<app>/`, through a pull request. A person merging it is the promotion. Workers read it from `/work`; the host has no skill store to vet.
9. **Does swarm mode stay research-only?** Yes. pstack applies to agent-mode workers and their delegates.
10. **Should reflect and show-me-your-work see transcripts?** Yes, the Ticket's own, read-only, at `/host/transcripts/`, bound per file so the Ticket directory's token files stay out of reach. That makes show-me-your-work's transcript audit and its cross-family reviewer real instead of self-report.
11. **Who writes the adapter?** solos-swarm, at `docs/harness/solos-swarm.md`, compiled in. Offering it upstream next to claude-code.md is a later courtesy; pstack would need one line in each skill's header to point at it, and the host does not depend on that.
12. **Does the Router get pstack too?** No. Its job is triage on a daily cap. Its instructions change so codebase questions are accepted as work with a `repo`.

## Open questions and risks

- **Can DeepSeek V4.1 Flash with thinking off follow poteto-mode?** pstack was written for Opus- and Grok-class models. Should the first live runs compare a Bug fix on deepseek-flash against the same Ticket with `judgment and prose` on glm-5, using pstack's Eval playbook, before pstack becomes every session's default entry?
- **Should `effort` map to thinking?** `upstream_body` forces thinking off for every call. Would an alias option `thinking: on` (a gateway change that also raises holds) be worth it for judgment roles?
- **Is a per-process CPU limit of 300 s enough for checks?** `prlimit --cpu` applies per process, so cargo's many rustc processes each get their own 300 s, but one long rustc or test binary can still die. Should a check name its own CPU limit?
- **Who provides offline toolchains and vendored dependencies?** A check that cannot build offline cannot pass, so `ready` becomes impossible for that repository. Is a `ci: {forge: true}` check acceptable as the only proof for repositories whose builds need the network?
- **Is the context cost acceptable?** A Feature run starts about a dozen delegates. A poteto-agent delegate inlines its definition and poteto-mode (about 23 KB) before its brief. Should delegates of `agent: general` get a shorter adapter, the Tools and Paths tables only?
- **Is one forge token per deployment enough?** Every configured repository shares it. Would per-repository tokens be worth the config, for an operator who mixes public and private repositories?
- **Should the CI watcher also wake the lead on new review threads?** Today they reach it only through the github Source, when one is configured for that repository, or through `pr_view`.
- **Does Pi accept a wire model id it did not register?** Swarm mode already starts agents with `--model <upstream model>` (swarm.rs:218-220), which suggests yes. A delegate on `zai-org-glm-5` depends on it in agent mode too. The first delegate PR's test with the scripted harness cannot show it; the live verify run can.

## Next implementation step

*Done in #54.* The step as planned: vendor pstack 0.15.36 with `scripts/pstack-sync` and write `src/agent/pstack.rs`'s `Tree::open` and `Tree::enter`, with the test that `enter` matches Pi's own expansion of the vendored poteto-mode byte for byte.
