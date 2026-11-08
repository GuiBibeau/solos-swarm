# Merge queue: a Ticket that lands a stack of pull requests

> **Status, 2026-11-08.** Design for #108. Nothing here is built. It re-plans PRs 3 and 5 of the
> [GitHub App plan](../github-app/prs.md), which are not built either, so that a queue can use them. The
> [sketch](sketch/) holds the types and signatures, with `todo!()` bodies.

## 1. The problem

The owner is moving a research programme, a private repository of Modal GPU runs and multi-TB data builds, into agent
mode. Its work arrives as pull requests that touch the same build files. Today a repository Ticket can owe an answer, a
commit in its clone, or a pull request the host opens as the bot. Integration work runs the other way. It starts from
pull requests that already exist and ends with them merged.

The ask, in the owner's words (#108): a Ticket names a stack of existing pull requests. For each one, in order, it:

1. brings the PR up to date with `main`;
2. resolves the conflicts it can;
3. runs the gate;
4. waits for CI, through the GitHub App;
5. squash-merges the PR, with `approvers` gating each merge;
6. stops with a question on a conflict it cannot resolve.

The owner also wants a report per pull request. The example is five research PRs that touch the same build files,
merged in order with a golden test kept green.

**Acceptance.** A Ticket that names three PRs merges them in order, with CI green between each. On the first conflict
it cannot resolve, it stops with a question.

## 2. What exists today

All of the GitHub App's forge work is delivery. None of it merges.

- **What a Ticket owes.** `Deliverable` is `answer < change < pull_request` (`crates/agent-core/src/change.rs:63-85`).
  The Router may lower a repository's `owes` for one request but never raise it (`src/agent/broker.rs:1869-1881`). The
  broker fills `ForgeTerms` from config, never from the model (`src/agent/broker.rs:1949-1961`).
- **One Ticket, one pull request.** `Ticket.change` is an `Option<Change>` (`crates/agent-core/src/model.rs:473-476`).
  A `Change` records only the bot's own branch, head, delivery, checks and pushes
  (`crates/agent-core/src/change.rs:364-388`).
- **The bot writes only its own branches.** The bot writes only to branches named `solos/<ticket>/<slug>`.
  `BranchName` parses only that shape, and the Ticket id in the name is what decides who owns a branch
  (`crates/agent-core/src/change.rs:135-217`). The host pushes from its mirror. Its lease is the head it last
  confirmed, and it never overwrites a branch that moved (`src/agent/forge/mirror.rs:587-634`, the rule at
  `:606-612`).
- **Admission.** The host fetches a worker's bundle into its bare mirror and judges what it calls the agent's own
  range: the commits reachable from the head and from nothing on the base (`src/agent/forge/mirror.rs:338-353`). It
  checks that every commit's author and committer is the bot (`:368-410`), that no commit touches a protected path
  (`:411-455`), and that the change is within its size and file limits (`:456-485`). The harvest bundles
  `$BASE..HEAD` (`src/agent/environment.rs:139-141`).
- **Nothing merges.** `ForgeJob` has one variant, `Publish` (`crates/agent-core/src/delivery.rs:21-25`), and
  `Execute` has one too (`src/agent/ports.rs:320-322`). No code calls a merge, review or update-branch endpoint. The
  only write calls are the push, `POST pulls` (`src/agent/forge/github.rs:250-282`) and issue comments
  (`src/agent/github/channel.rs:64`).
- **Config that nothing reads.** The config accepts `merge`, `ci` and `max_repairs` (`src/agent/config.rs:305-323`)
  and records them in the Ticket's terms (`:497-508`), but no code acts on them. `merge` defaults to `ask`
  (`:514-522`). `github.reconcile_secs` defaults to 300 (`:224-226`, `:680-682`), and nothing reads it.
- **CI intake.** A failed workflow run on any pull request, the bot's own included, becomes a Trigger
  (`src/agent/github/source.rs:548-578`). Nothing reads check runs or statuses for a head.
- **The App's grant.** Contents, pull requests and issues are write. Checks, statuses and actions are read. The App
  never holds `workflows` or `administration` (`src/agent/github/app.rs:82-94`). A token is narrowed to one
  repository, and the HTTP method decides its access (`:96-117`).
- **Asks and orders.** `Ask` is `Budget` or `Secret` (`crates/agent-core/src/model.rs:694-699`). `Order` is
  `Approve`, `Stop`, `StopBecause` or `Secret` (`src/agent/ports.rs:556-566`). Approvals take Slack or Discord buttons
  (`docs/agent-mode/how-to/mcp.md:69-70`).
- **Schema.** `agent.db` is at version 9 (`crates/agent-store/src/schema.rs:4`).

### A gap in today's admission

The protected-path check reads `rev-list --no-merges` over the range (`src/agent/forge/mirror.rs:415-420`). It never
asks what a merge commit changes beyond its parents, and only the file count reads the net diff (`:456-485`). A
hostile worker can therefore change a protected path inside a merge commit's own tree: merge two of its own branches
with `--no-commit`, edit a protected file, and commit. Every commit in the range is by the bot, and no non-merge commit
touches the path, so the bundle is admitted. `repository-tickets.md:89` says otherwise.

GitHub's missing `workflows` permission still stops `.github/workflows/**`. It does not stop the other paths that
`protected` exists for: `.github/actions/**` (composite actions that CI runs), `CODEOWNERS`, or an owner's own globs
such as `vendor/**`. The test that admits a worker's merge of a newer base (`tests/agent_github.rs:5152-5183`) passes
whatever the merge commit itself changes.

A merge queue makes merge commits its main path, so this design closes the gap first, for every Ticket (Q0 in §7).

## 3. What must stay true

The security review verified three GitHub properties. This design keeps two as they are and changes one, in one
stated way:

| Property | Here |
|---|---|
| The App lacks `workflows` (and `administration`) | **Kept.** No permission is added (§5.5). |
| Nothing merges today | **Changed by design**, which is the point. Every merge goes through one pure gate, pinned to one head, and needs an approver's yes at that head unless the owner opted into `auto` for the bot's own PRs. |
| Bot branches are `solos/<ticket>/<slug>` | **Kept for every branch the bot creates.** A queue Ticket may also **fast-forward** the head branch of a pull request a person named in that Ticket. Each such push adds commits on top of the head the host observed, never removes one, and is leased on that head. This is the one property that changes (§5.3), and decision 1 can keep it unchanged. |

The rest of the house rules hold too:

- outsiders never drive work or spend;
- a worker may be hostile and holds no forge tool;
- the host runs git only in its own mirror;
- every forge write is an idempotent Effect committed with the decision that owes it.

## 4. Three designs

All three share the parts that are not in question:

- polling, because the box has no inbound port;
- the PR 5 merge gate and its verdicts;
- the sha-pinned squash merge;
- a resolve round in which a hostile worker holds no forge tool.

They differ on one thing: **whose commits land on which branch**.

### A. Update each PR's own branch by merging the base into it (recommended)

When a PR is behind `main`, the host merges `main` into the PR's head in its mirror and pushes the merge commit to the
PR's branch as a fast-forward. If `git merge-tree` finds conflicts, a worker resolves them in a resolve round, inside a
merge commit of its own. Admission judges only the new commits, as usual. The PR then merges through the planned gate.
Because the merge is a squash and the head contains `main`, the tree that lands on `main` is exactly the tree CI
tested.

### B. Rebase each PR onto the base

This is the issue's literal wording. A worker rebases the PR's commits onto `main` and the host force-pushes them over
the PR branch, leased on the old head. A rebase rewrites every commit of the person's PR. Each copy has a new id, the
bot as committer, the person as author, and no signature. Admission would have to admit commits authored by someone
other than the bot. To keep any meaning, it would then have to prove that each rewritten commit is the same change as
an original. `git patch-id` stops matching as soon as a conflict shifts a hunk's context, so in the conflicted case,
the one that matters, that proof fails.

### C. Carry each PR onto a branch of the bot's own

The host never writes to anyone's branch. For each PR, it builds one squashed commit on `main` from the merge of the
PR's head, plus any resolution, and publishes it as the bot's own PR on `solos/<queue ticket>/<n>-<slug>`. That commit
carries `Co-authored-by` trailers. The bot merges its own PR, then closes the original with a link. This keeps all
three properties exactly. The person's work lands under the bot's authorship, though, and the original PR shows
"closed", not "merged". Its `Closes #n` does not fire, its reviews do not carry over, and its reviewers must look at a
different PR.

### Compared

| | A. Update by merge | B. Rebase | C. Carry |
|---|---|---|---|
| A hostile worker in a resolve round can | write any bytes into the **conflicted files** of one merge commit, and add ordinary commits that are judged per commit as today | rewrite **every** commit of the person's PR while it keeps their name as author | write any bytes into the carried squash commit |
| It cannot | touch a protected path, change a file the merge did not conflict on inside the merge commit, drop or rewrite the person's commits (the push is a fast-forward of the observed head), forge an author, or reach another PR or `main` | (with admission relaxed) be stopped from attributing its code to the person; the person's original commits leave the branch | write outside `solos/`; but the person's code lands as the bot's |
| An outsider can | at most comment on a queued PR (that taints it, so `auto` asks) or, with write access, push to it (new head, new CI, new yes) | the same | the same |
| Changes a reviewed property | yes: one, stated, and decision 1 can keep it | yes: the bot force-pushes rewritten commits onto people's branches, and the identity check weakens | no |
| What lands on `main` | the person's PR, merged, authored by them (GitHub's squash), `Closes` fires | the same | a bot PR; the original is closed |
| The owner configures | `queue:` per repository (§5.8), `merge`, a ruleset on `main` | the same, plus rules for foreign authors | the same, plus closing permission semantics |
| Code | a resolve round, a merge check in admission, a push target for a PR branch, the queue record | all of A, plus patch-id equivalence, author mapping and signature loss | a squash builder, carry PRs and closing; less new admission |

**Recommendation: A.** The queue squash-merges, so a branch's own history never reaches `main`. Rebasing therefore
buys nothing on `main` and costs the person's commit ids, signatures and authorship on the branch. A merge keeps their
commits byte for byte and adds exactly one bot commit, which today's identity rule already judges. GitHub's own "Update
branch" button does the same thing. C is the most conservative choice for properties, but it misattributes people's
work and breaks the PR's identity, which is what integration work exists to preserve. When every PR in a queue is the
bot's own, A writes only `solos/` branches anyway.

**Rejected outright:**

- **GitHub's native merge queue.** `docs/agent-mode/github-app/rationale.md:406` already rejected it. It needs branch
  protection, GraphQL and workflows that listen for `merge_group`, which is a `.github/workflows/` change the agent
  cannot make. It resolves no conflict, it has no per-merge approval, and it moves the gate off our chain.
- **GitHub's `PUT pulls/{n}/update-branch`.** It is a variant of A that does the clean merge on GitHub. It answers
  `202` and finishes later, so the host would poll for a head it did not make. It cannot be run against the `file://`
  bare remotes the offline tests use. Its conflict answer is a bare 422, with no list of paths for a worker. The host's
  own `git merge-tree` gives all three, in the mirror where admission already runs.

## 5. The recommended design

### 5.1 How it looks

```text
gui     @solos merge #41, #42 and #43 in GuiBibeau/regimes into main, in that order
solos   Merge queue on GuiBibeau/regimes into main: #41 "Golden test for the HMM fit" → #42 "Switch the build
        to uv" → #43 "Regime labels for the 2024 build". I bring each up to date with main, wait for CI, and ask
        before each merge.                                                                         [Stop]
solos   CI passed on #41 at 3f2a9c1d4e7b (4 checks). Merge #41 into main? Squash, head 3f2a9c1d4e7b.
                                                                                     [Merge] [Don't merge]
gui     [Merge]
solos   Merged #41 as 51d0b7c2a9e1. Next: #42.
solos   #42 was behind main. I merged main (51d0b7c2a9e1) into it as 8b1d7e2f0c3a: no conflicts. CI is running.
solos   CI passed on #42 at 8b1d7e2f0c3a (4 checks). Merge #42 into main? Squash, head 8b1d7e2f0c3a.
                                                                                     [Merge] [Don't merge]
gui     [Merge]
solos   Merged #42 as 7e1d0b2c4f5a. Next: #43.
solos   #43 conflicts with main in build/pipeline.toml and data/golden.json. A worker is resolving them on #43's
        branch (round 1 of 2).
solos   I stopped the queue at #43. The worker asks: "#42 regenerated data/golden.json with uv's lockfile, and #43
        adds two regimes to it. Should #43's goldens be rebuilt on top of #42's (that needs the Modal build), or
        kept as #43 wrote them?" Reply here to answer it, or fix #43 on GitHub and press Continue.
                                                                                       [Continue] [Stop]
```

The Ticket stays open at the question. When it ends, its Result is the per-PR report, built from the Store and never
from a model's words:

```text
Merge queue on GuiBibeau/regimes into main: 3 of 3 merged.
#41  merged as 51d0b7c2a9e1 · head 3f2a9c1d4e7b (as opened) · CI 4 passed · yes: gui (discord)
#42  merged as 7e1d0b2c4f5a · head 8b1d7e2f0c3a (main merged in by the host) · CI 4 passed · yes: gui (github review)
#43  merged as 0a9f3e1b7d2c · head 5c8e2a7b1f4d (conflicts resolved, round 2) · CI 4 passed · yes: gui (discord)
```

### 5.2 The queue's state, and one step function

A queue Ticket has `Plan::Queue { pulls }` and `Ticket.queue: Option<Queue>`. It is a separate record, not a map of
`Change`s: a queue entry is someone's pull request, which the Ticket adopted, not one the Ticket delivered.

```text
Queue { base, entries: [Entry] }                     in the order the person named
Entry { pr, branch (the PR's head ref), title, author, own (the bot's own PR, and whose),
        pull: PullView { head, base_tip, behind, checks, verdicts, merge },
        made (who made `head`: Person | Host{from, base} | Round{n}),
        updates, rounds, state (Pending | Active | Merged | Skipped), stop (a question for a person) }
```

The active entry is derived: it is the first entry that is neither merged nor skipped. Later entries are not touched
until their turn, because each merge moves `main` and any earlier update would be wasted.

`queue_step(ticket)` is pure. The Engine runs it after every fact that could change its answer: an observation, a
verdict, an Effect's result, an Assignment's end, a Continue, a stop. This is the same rule that drives `merge_gate`
(`docs/agent-mode/github-app/rationale.md:259`). For the active entry, in order, the first match decides:

1. The Ticket was stopped by a person: nothing more.
2. The entry has a stop: wait for the person.
3. Not observed yet: wait. The next poll observes it (§5.6).
4. Preflight failed: stop with the reason, before any write. The PR may not be open in this repository, may not target
   `queue.base`, or may be a draft. Its head may be `main` itself. Another open queue may hold it.
5. A person merged it: record `Merged { by: person }` and go to the next entry.
6. A person closed it: stop. Continue skips it.
7. It is behind `main` and an update is not in flight: commit `Effect::Forge(Update)`. Past `max_updates`, stop.
8. A resolve round is live: wait.
9. CI is pending: wait. Past `ci_wait_secs`, stop.
10. CI failed: wait, and tell once per head. A rerun that passes at the same head moves on, with no new approval
    (§5.7). At a head a resolve round made, PR 3's bounded repair applies instead.
11. The merge gate (§5.4): ask, wait or merge.
12. The last entry is merged or skipped: end the Ticket with the report.

Stops that a person must clear (a closed PR, a conflict nobody resolved, a refused push) are stored in `Entry.stop`.
Waits whose cause is an observation are derived and never stored: an observation that removes the cause removes the
wait. "CI failed at this head" is one of those.

### 5.3 Who updates a branch (hard part 1)

**The clean case: the host, with no worker.** Same-repository PR branches are already in the mirror, which fetches
`+refs/heads/*` (`src/agent/forge/mirror.rs:900-911`). `Update { pr, from: H, base: B }` runs
`git merge-tree --write-tree H B` in the bare mirror. If the merge is clean, the host makes the commit with
`git commit-tree -p H -p B`, as the bot and with the bot's identity. The mirror's git clears its environment
(`src/agent/forge/mirror.rs:869-883`), so the identity is passed explicitly. The host keeps the commit under a staging
ref, so a retry pushes the same object. It runs that commit through the same `judge` as a worker's (below), then
pushes it. No worker, no sandbox, no tokens spent. `merge-tree --write-tree` needs git 2.38 or later, which
`agent github check` gains as a line.

**The conflicted case: a resolve round.** The host does not start a worker when:

- a conflicted path is protected (a person must decide);
- the PR comes from a fork (the App cannot push there);
- `queue.resolve` is `stop`;
- the entry has used its `max_rounds`.

Each of these stops with a question that names the paths. Otherwise the Engine launches a session Assignment on the
Ticket's repository, with a fresh clone made by `Forge::prepare_round`:

- `/work` is checked out at H on a local branch, with B as `origin/<base>`;
- `/host/repo.json` has `base_commit: H` and `merge_from: B`;
- `/host/conflicts.json` lists the conflicted paths.

The worker's prompt gains one paragraph. It runs `git merge origin/<base>` and resolves only the listed files in that
merge commit. Any other edit goes in an ordinary commit after the merge. It does not rebase or rewrite commits before
H. It runs the repository's `evidence` command if one is configured. Then it reports `ready`, or `blocked` with one
question when the right resolution is a person's decision. It holds no new tool.

**Admission of a round's commits.** The worker's `ready` goes through `admit_ready`
(`src/agent/broker.rs:1077-1188`) as any Ticket's does. The proposal carries `Range::Round { from: H, base: B,
conflicts }`.

- **Harvest.** The bundle is `HEAD --not H B`, not `$BASE..HEAD` (`src/agent/environment.rs:139-141`). It holds only
  the round's own commits, not `main`'s.
- **The range** is the commits reachable from the head and from neither H nor B. That is today's range rule with
  `base_commit = H` and `refs/heads/<base>` excluded (`src/agent/forge/mirror.rs:338-353`). The person's commits and
  `main`'s are outside it.
- **Unchanged checks, over that range:**
  - H is an ancestor of the head (`NotDescendant`, `:330-337`), so no commit of the person's is rewritten or dropped;
  - every commit's author and committer is the bot (`:368-410`), so the person's name cannot be put on the worker's
    code;
  - no non-merge commit touches a protected path (`:411-455`).
- **New: merge commits are judged on what they themselves change** (Q0, every Ticket). For each merge commit in the
  range, the host computes `git merge-tree --write-tree` of its parents in the mirror and diffs the commit's tree
  against it. That diff is the merge's own edits. `protected` applies to it.
- **New, for a round:**
  - the round's first merge commit has exactly the parents `[H, B]`;
  - its own edits touch only `conflicts`, so a merge commit cannot carry a quiet change to a file that merged cleanly;
  - size and file count are measured from the host's merge result to the head, not from H, so `main`'s changes since
    H do not count against the worker.

A refusal is the `worker_report` 409, as today, with a sentence that says what to do next.

**The push.** One adapter path makes the PR's branch hold a head that contains the base: `Update` with `head: None`
for the host's merge, or `Some(head)` for an admitted round. It runs `ls-remote` first, then pushes with
`--force-with-lease=refs/heads/<branch>:<H>`. The push names H as the old value, so GitHub refuses it if the branch
moved, even between our read and our push. Admission already proved the new head descends from H, so every queue
push is a fast-forward. The lease is the compare-and-swap, not a license to force. A branch that moved is not
`Withheld` for good, as the bot's own `Change` is (`crates/agent-core/src/change.rs:300-313`). For a queue entry, a
person pushing to their own PR is normal. The entry re-observes and starts again from the new head. Only the host's
own updates count toward `max_updates`.

The push target is a new type, `PrBranch`. It names `refs/heads/<ref>` of the queue's repository only. It is built
only from a PR that preflight read, and is refused when the ref is the base, the repository's default branch, or a
ref `git check-ref-format` rejects. No code path can build a push to `main`, and a test pins that.

**The bot's own PR in a queue.** An entry whose branch parses as `solos/<t>/<slug>` and whose body holds Ticket `t`'s
marker (`src/agent/forge/github.rs:54-56`) belongs to Ticket `t`. In the same transaction that accepts the queue, `t`'s
`Change` is held as `Withheld::Queued { by }`. A review comment then cannot resume `t` and race the queue on one
branch. A queue that names a PR another open queue holds is refused at preflight. Only one queue per repository is open
at a time.

### 5.4 Approvals bound to the exact head (hard part 2)

A queue uses PR 5's gate, generalized from `Ticket.change` to a `PullView` that a `Change` and an `Entry` both hold.
There is one gate and it has two callers. PR 5's ten conditions read from the entry
(`docs/agent-mode/github-app/rationale.md:259-270`): not off, not stopped, open, no stop, nothing in flight, no
worker live, CI passed, no approver's no, authority at this head, and not already merging or refused. Two conditions
are added:

- **11. The head contains the base's tip** (`behind == 0` from GitHub's compare at observation). Otherwise the entry
  waits `Behind`, and step 7 updates it.
- **Auto is narrower on a queue.** `merge: auto` grants without asking only for an entry that is the bot's own PR,
  where the Ticket is untainted and the head was made by the host or by that PR's own Ticket. A round's head, or a
  person's PR, always asks. That is decision 5.

What binds a yes to one head:

- **Verdicts are keyed by head.** `Verdicts { head, by }` (`docs/agent-mode/github-app/sketch/01_core.rs:273-292`)
  holds yes and no for one head. A verdict for any other head counts for nothing, by construction, not by a reset that
  could be missed.
- **Buttons carry the head.** `Ask::Merge { pr, head }` puts the full head in the button's id, and
  `Order::Merge { pr, head, allow }` carries it back. The Engine ignores a click whose head is not the entry's head
  now. A click on an old message does nothing after the host pushed an update.
- **GitHub reviews count at their commit.** `observe` reads each approver's latest decisive review whose `commit_id`
  is the head. An approval given at H before the host's update to H' grants nothing at H'.
- **The merge is pinned.** `PUT pulls/{n}/merge` sends `sha: head` and `merge_method: squash`. GitHub answers 409 if
  the head moved. The host then reads the PR back and records `Merged` only when `merged` is true. It also checks that
  the squash commit's parent is the base tip the gate saw. If not, the tree that landed was not the tree CI passed:
  the entry records it and the queue stops before the next PR (§5.7).
- **Every push makes a new head.** The host's update, a round's push and a person's push each make one, so none of
  them can ride on an earlier yes. The cost is one approval per update. Decision 4 asks whether the host's own clean
  update should carry a yes over.
- **No self-approval.** No code posts a review today, and the queue adds none. A test pins that no request body holds
  `"event": "APPROVE"`.

### 5.5 What the App may write (hard part 3)

| Permission | Today (`app.rs:82-94`) | The queue uses it for | Change |
|---|---|---|---|
| `contents: write` | held | the fast-forward push to a PR branch; `PUT pulls/{n}/merge` | none |
| `pull_requests: write` | held | the merge call; comments on the PR | none |
| `checks`, `statuses`, `actions: read` | held | observing CI at a head | none |
| `actions: write` | absent | would allow an automatic rerun of a failed job | **not added** (decision 6) |
| `workflows`, `administration` | absent | | **not added** |

`contents: write` is repository-wide. GitHub has no token scoped to a branch, so the code and the owner's rulesets
together keep `main` human-gated:

1. **The code never writes `main`.** `PrBranch` cannot name the base (§5.3). A merge happens only through the PR merge
   call, pinned to one head, after the gate.
2. **The owner opts in per repository.** A queue needs a `queue:` block on the repository, and `merge` other than
   `off`. `merge` already defaults to `ask` (`src/agent/config.rs:514-522`), which needs an approver's yes at each
   head. `auto` is a second opt-in, and §5.4 narrows it.
3. **Only approvers start queues,** and only from a channel that shows buttons. That is the same rule as a vault
   item's first use (`docs/agent-mode/security.md:64-65`). Decision 9.
4. **GitHub enforces it too.** The recommended ruleset on `main` requires a pull request, the required checks and "up
   to date before merging", with no bypass for the App (decision 8). Then even a compromised host cannot push to
   `main`, and cannot merge a PR whose head is behind or red. Adding required approvals with stale-approval dismissal
   makes a human review on GitHub mandatory at the head, on GitHub's side as well as ours. It also means GitHub refuses
   to merge the owner's own PRs on the owner's own approval, so decide it per repository.
5. **`workflows` stays out.** GitHub refuses a push by an App without `workflows` that creates or updates a workflow
   file. As far as we know, a merge commit that brings the base's workflow edits into an older branch counts as such a
   push, and Q3's live recipe checks this on the sandbox. Either way, the push answer is a decided no. The entry stops:
   "main changed workflow files since #43 was opened. I cannot update its branch without the workflows permission,
   which I do not hold. Press Update branch on #43, then Continue."

### 5.6 Waiting for CI (hard part 4)

The box receives nothing, so the agent polls. Webhooks need an inbound endpoint or a third-party relay, and
`docs/agent-mode/github-app/rationale.md:209-215` rejected both. The webhook delivery log stays a later spike
(`docs/agent-mode/github-app/prs.md:33`). PR 3's machinery already does the polling. The queue adds a watched set and
a faster beat for one PR per queue:

- **The watched set.** After each reconcile pass, the github Source holds `(repo, branch) → ticket` for every open
  `Change` and every queue's active entry. A completed workflow run on a watched branch becomes a `Nudge` and calls
  `observe`, instead of today's Trigger (`src/agent/github/source.rs:548-578`). An Actions result is therefore seen
  within one listing poll, 60 seconds.
- **The active entry's beat.** Each queue's active entry is observed every `github.poll_secs`, not every
  `reconcile_secs`, so CI from apps other than Actions is seen within a minute too. One beat is three conditional
  GETs: the pull, its check runs and statuses, and the compare. With one queue per repository, that is about 180
  requests an hour, most of them 304s, which GitHub does not count (`docs/agent-mode/github-app/rationale.md:211`).
  The installation's limit is 5,000 an hour.
- **Derived, not accumulated.** `observe` reads the latest attempt of each check at the head every time. A rerun in
  progress reads as `Pending` and holds the gate. A stale green can never merge.
- **CI that never reports.** Path-filtered workflows can leave a head with no check at all
  (`docs/agent-mode/github-app/rationale.md:415`). Past `queue.ci_wait_secs` (default one hour) with nothing
  reported, the entry stops with a question, so it never waits for ever.
- **A push starts CI.** A push made with an App installation token starts workflows; only the Actions `GITHUB_TOKEN`
  is exempt. CI therefore runs on every head the queue pushes.

### 5.7 Failure modes (hard part 5)

| What happens | What the queue sees | What it does |
|---|---|---|
| A flaky check fails | `Failed` at head H | Waits, and says once per head which check failed, with its link. A person reruns it on GitHub. The next observation reads `Pending`, then `Passed`, at the same H. Verdicts at H still stand, and the queue goes on. Nothing is rerun automatically, because that needs `actions: write` (decision 6). |
| CI fails at a head a round made | `Failed` at a `Round` head | PR 3's bounded repair: the round's worker resumes with the failure, up to `max_repairs`. Then it stops. |
| `main` moves mid-queue (another PR merged) | the active entry is behind | It updates again from the new tip, which means a new head, new CI and a new yes. After `max_updates` host updates for one entry, it stops: "main keeps moving under #42". |
| `main` moves between the gate and the merge call | GitHub merges onto a newer base, or refuses | With "up to date before merging" on, GitHub refuses with 405, and the entry re-observes and updates. Without it, the read-back finds the squash commit's parent is not the tip the gate saw. The queue records that the merged tree was not the tested one and stops before the next PR. |
| A person pushes to a queued PR | a new head | The new head has no verdicts and pending CI, and is updated if it is behind. If our push was in flight, the lease refuses it and the entry re-observes. A person's commit is never overwritten. |
| A person force-pushes away the host's merge | a head that is behind again | It updates again from the person's head. |
| A person closes a queued PR | `closed` | It stops: "#42 was closed by alice. Continue skips it; later PRs may depend on it." |
| A person merges a queued PR | `merged` | It records `Merged { by: person }` from the read-back and goes on. |
| A person retargets the base, or makes it a draft | preflight fails again | It stops with the reason. |
| A fork PR is behind | the App cannot push to a fork | It stops: "Press Update branch on #44, then Continue". A fork PR that is not behind merges like any other. |
| GitHub refuses the update push (workflow files, a ruleset, signed commits) | `[remote rejected]` | It stops with GitHub's words (clipped). |
| A conflict on a protected path | `merge-tree` names one | It stops with no worker. |
| The worker cannot resolve | `blocked` with a question | It stops with the worker's question. A listed person's reply resumes the worker through the existing path (`crates/agent-core/src/model.rs:520-531`), while rounds remain. Continue re-observes, for a person who fixed it on GitHub. |
| Branch protection refuses the merge | 405 | `MergeRefused`, told once per head, then it stops. It never asks for a bypass. |
| The agent crashes or restarts | an Effect without a fact | Effects rerun and converge: `ls-remote` finds our merge (parents `[H, B]`, by the bot), and the merge read-back finds the PR merged. Observation re-derives the rest. |
| A person stops the queue Ticket | `stopped_by_person` | No more Effects. PRs already merged stay merged, and a PR keeps the merge commit the host pushed. |
| `policy.paused`, `github.writes: false`, a suspended App | | Held, as planned for PR 2 (`docs/agent-mode/repository-tickets.md:120-122`). |

Every stop is an Attention with **Continue** and **Stop** buttons (`Ask::Continue`, `Order::Continue`). Typed text
never continues a queue. Continue clears the stop and re-observes the entry from scratch.

### 5.8 Config shape

```yaml
forge:
  kind: github
  repos:
    GuiBibeau/regimes:
      about: Regime-switching research
      base: main
      owes: pull_request          # a queue writes to the forge
      merge: ask                  # off refuses queue plans; auto: see §5.4
      ci: expected
      queue:                      # absent: no merge-queue Tickets on this repository
        max_pulls: 10             # PRs one queue may name, 1 to 30
        max_updates: 3            # host-made updates of one PR before it stops
        resolve: worker           # worker | stop: who meets a conflict
        max_rounds: 2             # resolve rounds per PR
        ci_wait_secs: 3600        # a head with no check reported this long stops
```

Validation:

- `queue` needs `forge: github`, `owes: pull_request`, `merge` other than `off`, and `github.writes: true`;
- with `ci: none`, a queue merges on a yes alone, and config load warns.

The squash method is the only one, so it is not a field. At accept, `QueueTerms` is recorded in `Mandate.forge`
beside `ForgeTerms`, so a config edit never changes a queue in flight.

### 5.9 Store changes

Each PR that adds a stored field or variant takes the next free schema version with an empty migration, as every
forge PR has (`crates/agent-store/src/schema.rs:125-130`). Today's is 9. If nothing else lands first, the versions
fall as follows:

- **V10, Q1:** `MessageKind::Status`, `Observed::Seen`.
- **V11, Q2:** `Ask::Merge`, `Verdicts`, `MergeState`, `Ticket.outsiders`.
- **V12, Q3:** `Plan::Queue`, `Ticket.queue`, `ForgeJob::Update`, `Ask::Continue`, `Order::Continue`,
  `Withheld::Queued`, the queue events.
- **V13, Q4:** `Made::Round` and the round events.

New events, all on the hash chain: `QueueAccepted`, `EntryObserved` (only when its kind changes), `EntryUpdated
{from, to, by}`, `EntryConflicted {paths}`, `EntryStopped {why}`, `EntryContinued {by}`, `EntrySkipped`,
`QueueFinished`. PR 5's `MergeAsked`, `MergeAnswered`, `MergeStarted`, `Merged` and `MergeRefused` gain the `pr` they
are about.

### 5.10 Broker tools and grants

- **Workers: no new tool.** A resolve round's worker has exactly a repository worker's grants. The `WORKER_TOOLS`
  golden list (`docs/agent-mode/github-app/prs.md:12`) does not change.
- **The Router:** `router_decide` gains `plan: "queue"` and `pulls: [int]`. That touches `extensions/agent-tools.json`
  (`:40-46`), the registry and the fixture for that one tool. `router_decide` refuses a queue plan in these cases, each
  with a sentence:
  - the repository has no `queue:` block;
  - `merge` is `off`;
  - `pulls` is empty, repeats a number or exceeds `max_pulls`;
  - the requester may not approve;
  - the Thread's channel cannot show buttons.

  The PR numbers are the only thing a model contributes. The acknowledgement lists each PR's title from GitHub, so a
  misread number is visible before anything is written, and every merge still needs a yes at its head.
- **Controls:** `Order::Merge` and `Order::Continue` are adapter commands, checked with `policy.may_approve`, like
  Approve. Discord and Slack gain the buttons. A GitHub approving review is an `Order::Merge` read by `observe`, as PR
  5 planned.

### 5.11 Watchdog, dashboard and docs

- **Watchdog:** no new kind. A stop is an Attention, so the existing `Waiting` alert fires
  (`src/agent/watchdog.rs:81-94`). A queue that waits on CI past `ci_wait_secs` becomes a stop. An Update or Merge
  Effect that keeps failing is the existing `Failing` alert.
- **Dashboard and status:**
  - `TicketSnapshot` (`src/agent/publish/snapshot.rs:106-147`) gains `queue`: one row per PR, with its number, title,
    short head, step, CI, verdicts and outcome. That is the per-PR report the issue asks for, and it is live.
  - `deliverable` reads `merge queue`.
  - `agent status` prints one line per entry, with its `Waiting` reason.
- **Docs:**
  - `repository-tickets.md` gains "Merging a stack" and corrects line 89 (Q0);
  - `security.md`'s GitHub section states the changed property;
  - `configuration.md` gains the `queue` table;
  - `how-to/repositories.md` gains "Merge a stack of pull requests";
  - `github-app/prs.md` points its PR 3 and PR 5 rows here.

### 5.12 How it is tested

**Offline**, in the house style: the fake GitHub in `tests/agent_github.rs:244` with a `file://` bare remote, the real
`GithubForge`, mirror and push, the scripted harness and the real broker. No test calls GitHub or a model. The fake
grows these:

- people's PR branches;
- `compare`;
- check runs per sha, with a rerun;
- reviews with `commit_id`;
- `PUT pulls/{n}/merge`, which checks the sha, squashes into the bare repository, and has switches for "up to date",
  "required review" and a 405;
- a person's push, close, merge, retarget and draft;
- a `pre-receive` hook on the bare remote that refuses what GitHub refuses: a workflow file pushed without
  `workflows`.

The tests:

1. **Acceptance:**
   - three PRs (a person's that is current, a person's that is behind, and the bot's own that is behind) merge in
     order on three clicks;
   - each merge call carries the head that was clicked;
   - each merged head contains the previous merge, and each squash's parent is the tip the gate saw;
   - the fake's CI passed at each merged head, and the per-PR report matches the Store.
2. **The first unresolvable conflict stops:**
   - with `resolve: stop`, the second PR conflicts: it stops with the paths and a Continue, the third PR's branch is
     byte-unchanged, and nothing merges after the stop;
   - the same with a conflict on `.github/actions/x.yml` under `resolve: worker`: no worker starts.
3. **Admission of a round:**
   - a merge commit with parents `[H, B]` whose edits are only in conflicted files is admitted;
   - it is refused for any of: an extra file in the merge commit's own edits, a protected path in them, a commit
     authored as the PR's author, a rebased H (`NotDescendant`), or a second parent that is not B;
   - the push is a fast-forward leased on H.
4. **Q0 on every Ticket:** an evil merge that edits a protected path in its own tree is refused. The existing
   conflict-repair test (`tests/agent_github.rs:5152-5183`) passes unedited.
5. **The head binding:**
   - a yes at H, then the host's update to H', merges nothing until a yes at H';
   - a click on the H message after H' does nothing;
   - a GitHub approval with `commit_id` H does nothing at H';
   - a person's push after a yes clears it;
   - the merge call carries H', and the fake's 409 for a moved head records nothing.
6. **Failure modes, one test each:**
   - `main` moves before the click: a second update;
   - `main` moves between the gate and the call, with "up to date" on (a 405, then a re-update) and off (the
     read-back finds an untested tree and the queue stops);
   - a flaky fail, then a rerun passes at the same head: it merges on the yes already given;
   - a person's push loses our lease: nothing is overwritten;
   - a person closes, then Continue: it is skipped;
   - a person merges it: it is recorded and the queue goes on;
   - a fork that is behind stops;
   - `max_updates` stops;
   - `ci_wait_secs` with no check reported stops.
7. **Crash convergence:** drop each Effect's result and restart on the same store. The result is one push and one
   merge per entry.
8. **Fences:**
   - `PrBranch` refuses the base and the default branch;
   - no request ever asks to approve;
   - every write mint names the one repository;
   - the App's permissions equal `manifest.json` (`docs/agent-mode/github-app/manifest.json`), unedited;
   - `WORKER_TOOLS` is unedited.

**Live**, through `/verify-agent-mode` on `GuiBibeau/solos-swarm-sandbox`, with a new recipe `github-queue.md`. A
second account opens three PRs. #A and #B change different files. #C changes the line #A changes, so it conflicts. The
owner asks from Discord and clicks Merge twice. The evidence comes from `gh` on the Mac, and the model cannot invent
it:

- `gh pr view <n> --json state,mergedBy,mergeCommit,headRefOid` reads merged by `app/solos-bot` at the clicked head;
- each merge commit's parent is the previous merge;
- `gh pr checks` is green at each merged head;
- #C is open, its head unchanged under `resolve: stop`, and the Discord Attention names its paths.

The recipe also records what GitHub does with a base that changed a workflow file (§5.5).

## 6. Relation to the GitHub App plan

This design **supersedes PRs 3 and 5** of [`github-app/prs.md`](../github-app/prs.md). It keeps their content, and
the rows Q1 and Q2 below replace their rows. Q1 and Q2 change three signatures so that a queue reuses them instead of
copying them:

- `Forge::observe(repo, branch: BranchName)` becomes `observe(repo, pr)`, because a queued PR's branch is not a
  `BranchName`;
- `merge_gate(ticket)` becomes `merge_gate(terms, &PullView)`, which a `Change` and an `Entry` both hold;
- `ForgeJob::Merge`, `Ask::Merge` and `Order::Merge` gain `pr`, and carry the full head.

PR 3's repair rounds and PR 5's gate rules, tests and live recipes stand as written for the bot's own PRs. The
superseded rows also gain one rule each:

- PR 3's watched set includes queue entries;
- PR 5's read-back checks the squash commit's parent.

**PRs 4 and 6 are untouched.** It also replaces two items of that plan's "Later, not scheduled" list
(`docs/agent-mode/github-app/prs.md:34`):

- "Stacks (`Ticket.change` becomes a map)" becomes `Ticket.queue`, a separate record: a queue adopts PRs, it does not
  deliver them;
- "a host-run gate command inside `admit`" becomes Q5, an Effect of its own, which escapes the timing objection in
  `docs/agent-mode/repository-tickets.md:207-214`.

"Adopting a collaborator's push" stays later for the bot's own `Change`. Queue entries adopt people's pushes by design.

## 7. PR plan

Each PR leaves `main` shippable and ends in an offline behavior test and a live recipe on the sandbox. Builds and tests
run on the remote box.

| # | PR | Adds | The check |
|---|---|---|---|
| Q0 | **Admission judges merge commits** (a fix for every Ticket, landable now) | In `judge`, the merge-tree diff of each merge commit in the range against its parents, checked against `protected`. A `git >= 2.38` line in `agent github check`. The correction to `repository-tickets.md:89`. | Test 4. Every existing `agent_github` and `agent_forge` test passes unedited. |
| Q1 | **Watch** (GitHub App PR 3, re-planned) | As planned: `EventFeed`/`RestFeed`, the reconcile pass, `Observed::Seen`, `MessageKind::Status`, bounded CI repair for the bot's own PRs. Plus `observe(repo, pr)`, and the watched set fed from open Changes (queue entries join in Q3). V10. | PR 3's tests as planned. A completed run on a watched branch is one Nudge and no Trigger. Recipe `github-repair.md` as planned. |
| Q2 | **Merge** (GitHub App PR 5, re-planned) | As planned: `merge_gate`, `Verdicts`, `Ask::Merge`, `Order::Merge`, `Engine::merge_verdict`, `outsiders`, `Forge::execute(Merge)`. Plus `PullView`, `pr` on the merge types, the full head in buttons, and the read-back's parent check. V11. | PR 5's gate table and tests as planned, plus test 5 for the bot's own PR. Recipe `github-ship.md` as planned. |
| Q3 | **Queue, clean path** (usable: "merge #41, #42, #43" lands a stack that needs no resolution) | `queue:` config and `QueueTerms`; `router_decide`'s `plan: queue`; `Plan::Queue`, `Ticket.queue`, `queue_step`; preflight; `ForgeJob::Update` with the host merge; `PrBranch`; the watched set and the active-entry beat; `Withheld::Queued`; Continue and Stop; the per-PR report, `agent status` and the dashboard row; docs. Every conflict stops with a question. V12. | Tests 1, 2 (the `resolve: stop` half), 5, 6, 7 and 8. This PR meets #108's acceptance. Recipe `github-queue.md`. |
| Q4 | **Resolve rounds** (usable: conflicts a worker can settle are settled) | `Forge::prepare_round`, the round's harvest, `Range::Round` in admission (parents `[H, B]`, own edits only in conflicted files, size from the host's merge), the prompt paragraph, pushing an admitted round's head, repair rounds on a round's head. V13. | Test 3 and the protected-path half of test 2. A recipe step where #C's conflict is settled by a worker, then needs a click. |
| Q5 | **Host-run gate** (optional, decision 10) | `queue.gate: {command, timeout_secs}`, run as `Effect::Forge(Gate)` in a fresh offline `run_code` sandbox on a clean checkout of the candidate, with `deps` mounted, before the update is pushed. Its pass is a merge-gate condition. | A candidate whose gate fails is never pushed, and the entry stops with the log's tail. A passing one goes on. |

Order: Q0 any time, first preferably. Q1 → Q2 → Q3 → Q4 → Q5. The planned PR 4 can land anywhere after Q1, and PR 6
anywhere.

## 8. The in-flight fixes

None of the five branches was on `origin` when this was written (`git branch -r`; `gh pr list` lists only #96 and
#97). The design depends on them by name:

- **`fix/outsider-spend`:** the queue relies on outsiders never creating a Ticket. It also requires the requester to
  be an approver, so the queue adds no exposure if that fix is late.
- **`ops/harden-units`:** Q3 adds host git commands (`merge-tree`, `commit-tree`) in the mirror, with the same trust
  as today's admission. Running them as a dedicated user limits what a git bug can reach. Land it before Q4, which
  writes a worker's commits to people's branches.
- **`feat/worker-file-tools`:** resolve rounds edit conflicted files, and better file tools help. There is no
  dependency, and admission does not care how a commit was made.
- **`feat/cargo-deps`:** Q5 on a Rust repository needs it.
- **`fix/memo-trust`:** no interaction.

## 9. Decisions for the owner

1. **Which PRs may a queue update?** (a) Only the bot's own `solos/` PRs, which leaves the reviewed property
   unchanged. (b) Also people's PRs in the same repository, by fast-forward only (§3). **Recommended: (b).** Your
   research PRs may be your own. Choose (a) if they are all the bot's.
2. **Update by merge or by rebase?** **Recommended: merge only.** The squash makes branch history irrelevant to
   `main`, and a rebase would put the bot's rewrite under people's names.
3. **May a worker resolve conflicts on a PR it did not open** (`queue.resolve`)? **Recommended: `worker`.** Each
   round's head needs an approver's yes at that head, and protected paths never reach a worker.
4. **Does a yes survive the host's own clean update?** Carrying it would save one click per update, and the host can
   prove that the update added only the base. **Recommended: no, in v1.** Each merge gets a yes at the exact head that
   merges.
5. **`merge: auto` on a queue.** **Recommended:** auto merges only the bot's own PRs whose head the host or that PR's
   Ticket made. A round's head or a person's PR always asks.
6. **Automatic rerun of a failed check?** That needs `actions: write`, which also lets the App dispatch workflows and
   cancel runs. **Recommended: no.** A person reruns on GitHub, and the queue goes on by itself when the check passes.
7. **Wait for `main`'s own CI after each merge?** The squashed tree equals the tested head, so it adds a CI run per PR
   for little. **Recommended: no.**
8. **A ruleset on `main`.** **Recommended:** require a pull request, the required checks and "up to date before
   merging", with no bypass for the App. Add required approvals with stale dismissal only on a repository where you
   never want `auto`. GitHub will not let you approve your own PRs, and those would then need another reviewer.
9. **Who may start a queue?** **Recommended:** approvers only, from Discord or Slack. Yes votes can also come as
   GitHub reviews.
10. **Is the golden test in CI?** If it is, CI is the gate and Q5 is not needed. **Recommended:** skip Q5 unless the
    golden test cannot run in CI.
11. **Limits.** **Recommended:** `max_pulls: 10`, `max_updates: 3`, `max_rounds: 2`, `ci_wait_secs: 3600`, and one
    open queue per repository.

## 10. Open risks

- **CI secrets reach more branches.** Same-repository branches run `pull_request` workflows with the repository's
  secrets, and a round's worker writes code those workflows run. This is the open question of
  `docs/agent-mode/github-app/rationale.md:413`, now extended to people's PRs.
- **Git LFS.** A conflict in an LFS pointer cannot be resolved offline, so a round would stop. The research
  repository's data must stay out of git or in LFS that is never edited in a stack.
- **Speed.** A queue moves at CI speed plus a click per PR: five PRs with 15-minute CI take about 90 minutes.
- **Workflow files on `main`.** What GitHub does with an update that carries the base's workflow edits is checked
  live, not assumed (§5.5).
- **The Router reads the PR numbers.** A misread is caught by the acknowledgement's titles and by the per-head yes,
  not prevented. Under `auto`, decision 5 limits what a misread could merge to the bot's own PRs.
