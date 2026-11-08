# Long jobs and external spend

> **Status, 2026-11-08.** A design, not built. Design for #101 (detached jobs that outlive a worker session) and #102
> (external spend, such as Modal, on a Ticket's account). Grounded in `main` at `7ea26d2`. It depends on the egress
> design for #99 (`design/repo-resources`), and it shares a job-watch shape with #107 (`feat/external-jobs-watch`).
> Neither branch had a commit when this was written, so section 4.12 and section 4.6 say what this design needs from
> them, not what they built. The [decisions for the owner](#6-decisions-for-the-owner) are at the end.

The sketch of types and signatures is in [`sketch/`](sketch/). Every body there is `todo!()`.

## 1. The problem

### 1.1 What the owner asked

The regimes research programme is moving into agent mode on `remote-ai`. Its work is long:

- Modal training waves of 10 to 48 runs, which take hours and cost real money on Modal's GPUs.
- 80-minute data builds, and multi-hour downloads from public data hosts.

Today such work runs only from an unsandboxed Claude Code session on the host, with `nohup` or `setsid`, and the owner
meters Modal by hand: read `modal billing summary` before and after each wave, and keep caps per wave, a ceiling and a
programme total in their head.

**#101** asks for a job primitive for Tickets. A worker launches a command, or a Modal app, detached, and gets a
handle. The handle is persisted in the Store. Later worker sessions of the same Ticket poll it, stop it and collect its
result. `agent status` and the dashboard show it. The Watchdog alerts when it dies or stalls.

**#102** asks for an external meter: a command plus a parser, such as Modal's billing, whose deltas settle on the
Ticket's account. A launch takes a worst-case hold first, the way a model call does, and a tripwire stops the
Ticket's jobs before the hold is exceeded.

Production runs with no `budgets:` section by the owner's choice (`docs/agent-mode/production.md:118-120`). External
spend must still be capped per Ticket when the owner asks for it.

### 1.2 Acceptance tests

- **#101.** A Ticket starts a 2-hour job. Its worker session ends. A later session of the same Ticket collects the
  result. `agent status` and the dashboard show the job. Killing the job raises one alert.
- **#102.** A Ticket with a $5 budget that launches Modal work sees its Modal spend on its account, and is stopped
  before passing $5.

### 1.3 What exists today

**Background jobs belong to one Assignment and die with it.**

- `job_start`, `job_wait` and `job_stop` keep a table of jobs on the worker's seat (`src/agent/jobs.rs:98-115`,
  `src/agent/broker.rs:150`). When the worker ends, `stop_all` kills every job and reports none
  (`src/agent/jobs.rs:233-244`, `src/agent/broker.rs:299-303`). A revoked token abandons them
  (`src/agent/broker.rs:1239-1243`).
- A job runs `run_code`'s sandbox: `prlimit --as --fsize --cpu=300 --nofile` around `bwrap --unshare-all
  --die-with-parent` (`src/code.rs:663-670`, `src/code.rs:680-683`). The child is `kill_on_drop`
  (`src/code.rs:1283`). So a job cannot outlive the `agent serve` process, and no process may use more than 300 s of
  CPU.
- A job's wall time is at most `job_max_secs`, 300 to 86,400 s, default 1,800 (`docs/agent-mode/configuration.md:166`,
  `src/agent/jobs.rs:67-71`). A Ticket holds at most `max_jobs` (2) jobs whose end has not been read
  (`src/agent/jobs.rs:441-452`). Jobs hold `run_code` slots, `concurrency` (4) across the box
  (`docs/agent-mode/configuration.md:162`, `src/code.rs:393`, `src/code.rs:1230-1233`).
- "Jobs do not survive a restart" (`docs/agent-mode/runtime.md:266-267`). A deploy drains for 90 s and then kills a
  worker in a long tool call (`docs/agent-mode/production.md:201-202`, `docs/agent-mode/production.md:227-229`).
- The sandbox has no network (`src/code.rs:681`, `docs/agent-mode/configuration.md:169`). Workers reach the network
  only through broker tools (`docs/agent-mode/security.md:23-24`).
- A job's end between turns becomes `Fact::Note` (`src/agent/jobs.rs:346-354`), applied as `Engine::note`
  (`src/agent/runtime.rs:472-475`). A Note adds an Input and delivers it only to a running Assignment
  (`crates/agent-core/src/engine.rs:212-220`, `crates/agent-core/src/engine.rs:1685-1688`). Only an allowed person's
  Trigger wakes an idle or ended Ticket (`crates/agent-core/src/engine.rs:137-156`,
  `crates/agent-core/src/engine.rs:1334-1342`).

**The agent's unit kills its whole cgroup.** `solos-agent.service` sets `KillSignal=SIGTERM` and no `KillMode`, so
systemd's default, `control-group`, kills every process in the unit on stop (`scripts/systemd/solos-agent.service:33`).
The unit's `MemoryMax=64G` and `TasksMax=8192` cover the agent and every sandbox it starts
(`scripts/systemd/solos-agent.service:41-42`).

**The ledger holds and settles integer micro-USD per account.**

- A hold is a `spend` row in state `held`. It closes once, as `settled`, `released` or `unknown`, and a closed row can
  never change or go away (`crates/agent-store/src/schema.rs:62-83`).
- `reserve` checks the cap and inserts the hold in one IMMEDIATE transaction (`crates/agent-store/src/ledger.rs:152-183`).
  An open hold counts at its hold and a closed one at its cost (`crates/agent-store/src/ledger.rs:147-149`). A cost
  above its hold is kept as reported (`crates/agent-store/src/ledger.rs:185-186`).
- `charge` records a cost paid elsewhere, once per request id (`crates/agent-store/src/ledger.rs:240-271`). Swarm
  Tickets already use it to settle a whole swarm on the Ticket's account (`docs/agent-mode/runtime.md:323-327`,
  `src/agent/broker.rs:1267-1287`). That is the precedent for external spend.
- **At every start, `close_open_holds` charges every open hold at its hold** (`crates/agent-store/src/ledger.rs:273-282`,
  called at `src/agent/broker.rs:953`). That is right for a model call that died with the process. It is wrong for a GPU
  job that is still running on Modal across a deploy: its hold would close for good at the worst case.
- A Ticket's account `ticket:<id>` is capped for life by its Mandate budget, or not at all where it has none
  (`src/agent/broker.rs:511-521`). With no `budgets:`, a session Ticket has no budget
  (`docs/agent-mode/concepts.md:95-102`).
- A cost above a hold on a capped account sets a broker-wide overrun flag that stops every capped account's model calls
  (`src/agent/broker.rs:1543-1549`).

**The Watchdog is a pure assessment over the Store.** `assess(snapshot, now, thresholds)` raises `waiting`, `stalled`,
`failing`, `unavailable`, `spend` and `paused` (`src/agent/watchdog.rs:79-94`, `src/agent/watchdog.rs:314-345`). An
alert id is a hash of the kind, the subject and a bucket of `repeat`, counted from when the threshold was crossed, so
each problem is told once and again every 6 h while it lasts (`src/agent/watchdog.rs:136-150`). Each alert is a row in
`watchdog_alerts` before it is sent (`crates/agent-store/src/schema.rs:88-101`). Its stall signal is about a working
Assignment with no Report (`docs/agent-mode/runtime.md:496-497`). An idle Ticket that waits on a job has none.

**`agent status` and the publisher read `agent.db` read-only.** `agent status` shows Tickets, spend and the alerts
`assess` would raise (`src/agent/status.rs:23-38`, `src/agent/status.rs:102-110`). The publisher uploads one snapshot per
Ticket and an index (`src/agent/publish/snapshot.rs:59-147`). Neither knows of a job.

**The Store is at schema 9** (`crates/agent-store/src/schema.rs:4`). Every PR that adds a Ticket field, an Input kind or
an event bumps it (`docs/agent-mode/architecture.md:128-130`).

## 2. The answers in brief

| Hard part | Answer |
|---|---|
| Where a detached job runs | In its own transient systemd user unit, `solos-job-<id>.service`, in a `solos-jobs.slice`, wrapping the same `prlimit` and `bwrap` line as `run_code`. systemd enforces its wall time, memory, CPU and task caps, and its own cgroup outlives `agent serve`. |
| Restart and recovery | The job's row is in the Store. At start, before the broker serves anyone, the supervisor reconciles each live row with `systemctl --user show`, reads the end record a finished unit left, and resumes watching. |
| Orphans | Unit names are the host's. A `solos-job-*` unit with no live row, or whose Ticket ended, is stopped. So are Modal apps that no live job owns. |
| Network | No second path. The job's sandbox gets #99's egress for the repository, plus the Modal token as a sandbox secret. Section 4.6 lists what this needs from #99. |
| Meter | The host runs `modal billing report --resolution h --json`, with its own token, in its own sandbox. Its numbers are per App and per closed hour, so it is the truth for settling, and too slow for a tripwire. |
| Hold | Before launch: the declared burn per hour times the wall cap, plus an allowance for overhead, reserved on the Ticket's account in the same transaction as the caps. |
| Tripwire | The host stops the job's Modal Apps and its unit when metered plus estimated spend, plus the cost of stopping, reaches the hold. The estimate is live: running containers times their price, every minute. The worker session is never the thing stopped. |
| Accounting | The hold is a `spend` row of a new kind, `external`, which a restart does not close. It settles once at the metered total after the last hour is reported. Integer micro-USD throughout, the Decimal from Modal rounded up. |
| Trust | The host runs the meter and the guard. A worker can create Apps and tag them, and nothing else that the meter reads. An App no live job owns is stopped within a minute and is an incident. Modal's own workspace budget is the outer cap. |
| No `budgets:` | `external.modal.ticket_cap_microusd` caps a Ticket's Modal spend by itself. Without it, each job is still stopped at its own declared hold. |

## 3. Three designs

All three keep today's `job_start` for short jobs. They differ in where a detached job lives, who holds the Modal
token, and what meters the spend.

### Design A: transient systemd units, a worker-held Modal token, a host meter and guard

- A detached job is a transient user unit made with `systemd-run --user`, in a slice of its own. Its main process is
  `run_code`'s `prlimit` and `bwrap` line. systemd enforces `RuntimeMaxSec`, `MemoryMax`, `CPUQuota` and `TasksMax` on
  the unit's cgroup.
- The job's row lives in a new Store table, written only by a job supervisor inside `agent serve`.
- A Modal job is a detached job whose sandbox also gets #99's egress to Modal and the Modal token. Its launcher runs
  the repository's own Modal app unchanged, such as `modal run --detach waves/train.py`.
- The host meters and guards the dedicated Modal workspace: the Apps it runs, their containers, and the hourly billing.

### Design B: an agent-owned job host, and a host-only Modal runner

- A second always-on unit, `solos-agent-jobs.service` with `Delegate=yes`, runs `solos-swarm agent job-host`. It
  starts each job as its child in a cgroup it makes, writes the limits itself, keeps the output and end record, and
  serves a Unix socket to `agent serve`. Deploys restart `agent serve` and leave the job host alone.
- No worker holds a Modal token. A worker asks for `modal_run {script, gpu, count, timeout_s}`, and the host runs its
  own fixed Modal app (an owner-written runner) with its own token. The runner uploads `/work`, runs the script remotely
  with the GPU, count and timeout the host passed, and Modal enforces that shape. The hold is exact: count times rate
  times timeout.

### Design C: Ticket-owned jobs inside `agent serve`, and a before-and-after meter

- Move today's job table from the worker's seat to the Ticket, in the runtime. A job survives its session and dies with
  `agent serve`, as today.
- A Modal job is a job that runs `modal run --detach`. The remote App survives the local process, and its App id is
  kept in the Store.
- The meter is what the owner does by hand: `modal billing summary` before and after, and the delta charged to the
  Ticket.

### Comparison

| | A: systemd units | B: job host and runner | C: in-process |
|---|---|---|---|
| Survives the worker session | yes | yes | yes |
| Survives a deploy or a crash of `agent serve` | yes: another cgroup | yes, until the job host itself is updated | no: the 80-minute build dies at the next deploy |
| Survives a reboot | no: transient units are gone. The row says `lost`. Modal Apps keep running and stay guarded | no, the same | no |
| Wall cap without the agent | systemd's `RuntimeMaxSec` | the job host's own timer | none: a job dies with the agent instead |
| Resource caps | cgroup per job and a slice for all, set by systemd | cgroups the job host writes itself (reimplements systemd) | `prlimit` only. The 300 s CPU cap must go, with nothing in its place |
| Hostile worker: escape and reach | the same bwrap as `run_code`, plus `NoNewPrivileges=yes` on the unit, which keeps `sudo` out of reach even before the dedicated-user migration | the same bwrap, inside the job host's cgroup | the same bwrap |
| Hostile worker: Modal token | in its job sandbox, readable. It can use it from anywhere Modal's Apps reach. The guard stops unknown Apps, and the owner rotates the token | never in a sandbox. Code in a Modal container may still reach the workspace while the container runs | in its job sandbox, as A |
| Hostile worker: spend shape | declared, then checked live: containers times price, every minute | fixed by Modal from the host's arguments | not checked |
| Hostile worker: data out | Modal Apps run the worker's code with internet. So `external: [modal]` gives that repository's workers a way out | the same: the script runs on Modal with internet, unless the runner can turn it off | the same |
| Outsider | cannot start a job: only workers of accepted Tickets hold the tools, and `accept_mode: off` takes explicit requests from listed people only. Cannot wake a Ticket: only the host and allowed people do | the same | the same |
| Meter trust | host-run, host token, per App and hour, every App accounted for | host-run, the same meter | the delta of a month-to-date total for the whole workspace. Any other spend in it lands on the Ticket, and a month boundary breaks the delta |
| Owner configures | `executor.detached`, `external.modal`, `forge.repos.<repo>.external`, a dedicated Modal workspace and its budget, two token files, #99's egress | the same, plus the job host's unit and the runner template. Research code must fit the runner's contract | a meter command and a token |
| Code (non-test lines, rough) | about 2,400 | about 3,500: the daemon, its protocol and cgroups, the runner | about 900 |
| Meets #101 and #102 | yes | yes | #101 only between deploys. #102 not safely |

**Why A.** It meets both acceptance tests, it survives deploys, and systemd enforces the caps even when the agent is
down. It runs the repository's Modal apps unchanged, through the egress path #99 builds anyway. B is stronger on the
token, and costs a daemon and a runner contract that the research code would have to fit. C is the cheapest and fails
the deploy case that motivated #101. Decision 3 asks whether the owner wants B's runner for Modal anyway.

## 4. The recommended design

### 4.1 The life of a detached job

```text
job_start {detach}  ─ refuse? (grants, caps, hold) ─ row `starting` + hold (one transaction)
        │
        └─ systemd-run --user solos-job-<id> ─ row `running` ─ supervisor polls the unit (and Modal) every minute
                                                     │
    worker session ends ──────── the job keeps running; a later session sees it in its prompt and job_list
    agent serve restarts ─────── the unit keeps running; the supervisor reconciles at start
    tripwire / Ticket ends ───── the host stops the unit and the job's Modal Apps ─ row `stopping`
                                                     │
                     unit ends ─ ExecStopPost writes the end record ─ row `ended`
                                                     │
          Engine::job_ended: an Input on the Ticket; wakes it if idle, within the wake bound
          meter: once the last hour is reported and `settle_lag` passed, the hold settles
```

### 4.2 Where it runs: the unit

The supervisor starts each job with `systemd-run --user`, as the agent's user. The command never goes through a shell.

```text
systemd-run --user --unit=solos-job-jd_3f0a.. --slice=solos-jobs.slice --collect --no-block
  -p RuntimeMaxSec=7200 -p MemoryMax=32G -p MemorySwapMax=0 -p CPUQuota=800% -p TasksMax=512
  -p NoNewPrivileges=yes -p Nice=5 -p CPUWeight=50 -p IOWeight=50
  -p StandardOutput=append:<state>/tickets/<ticket>/jobs/<job>/stdout
  -p StandardError=append:<state>/tickets/<ticket>/jobs/<job>/stderr
  -p ExecStopPost=<the end-record command>
  -- /usr/bin/prlimit --as=.. --fsize=.. --nofile=512 -- /usr/bin/bwrap <run_code's binds> ... /usr/bin/bash /job/command.sh
```

- **The sandbox is `run_code`'s.** The same builder makes the line (`src/code.rs:649-752`), with the Ticket's
  `Extras` (a repository's installed dependencies). Two things change. `--cpu=300` is dropped, since the wall cap
  times `CPUQuota` now bounds CPU time. `--die-with-parent` is dropped, since the unit's main process is bwrap itself.
- **The command is a file, not an argument.** The host writes the worker's code to `jobs/<job>/command.sh` (mode 0400)
  and binds it read-only at `/job/command.sh`. systemd expands `%` specifiers and `$` variables in `ExecStart`, so
  worker text in an argument could leak paths or change meaning. In a file, it is only bash's to read.
- **The output is files the job cannot rewrite.** systemd opens `stdout` and `stderr` outside the sandbox, and the
  sandbox does not mount `jobs/`. `prlimit --fsize` still caps each file, since the limit applies to the process that
  writes.
- **The end record is the host's.** `ExecStopPost` runs outside the sandbox after the main process ends, however it
  ends, with systemd's `$SERVICE_RESULT`, `$EXIT_CODE` and `$EXIT_STATUS`. It writes `jobs/<job>/end` atomically.
  `--collect` lets systemd unload the unit at once, so failed units do not pile up. Nothing in the sandbox can write the
  end record, so a job cannot report its own success.
- **The slice bounds all jobs together.** `solos-jobs.slice` is a unit file that `scripts/agent-deploy` installs with
  the others (`docs/agent-mode/production.md:619-629`), with its own `MemoryMax`, `CPUQuota` and `TasksMax`. Jobs are
  outside `solos-agent.service`'s cgroup, so its `MemoryMax=64G` no longer covers them, and the slice takes its place.
- **Hardening lives in one place.** A prefix drop-in, `~/.config/systemd/user/solos-job-.service.d/10-hardening.conf`,
  applies to every `solos-job-*` unit, and deploy installs it from git. A transient unit does not inherit
  `solos-agent.service`'s settings, so whatever `ops/harden-units` adds to the agent's unit is repeated there. PR 2
  checks on the box that a prefix drop-in reaches a transient unit, and that bwrap still starts under
  `NoNewPrivileges=yes`. If either fails, the supervisor passes the same properties itself.
- **Limits are configuration** (section 4.8): the most running jobs on the box and per Ticket, the longest wall time,
  memory, CPU, tasks and output. Detached jobs do not take `run_code` slots, so a two-hour job never starves
  `run_code`.
- **Where it does not run.** A Ticket bound to a remote node gets no detached jobs, since a node keeps no session state
  and a dropped connection stops its jobs (`docs/agent-mode/runtime.md:408-412`). That is a later step.

`agent health` already asks `systemctl --user show` (`src/agent/health.rs:65-70`), so the agent's user can reach its
user manager from its units. Lingering is already on (`docs/agent-mode/production.md:22`).

### 4.3 Restart, recovery and orphans

- **A deploy or a crash.** The job's cgroup is under the user manager, not under `solos-agent.service`, so stopping the
  agent's unit leaves it running. The job keeps writing its files.
- **Recovery.** At start, before the broker serves anyone (where `close_open_holds` runs today,
  `src/agent/broker.rs:953`), the supervisor reads every row that is not `ended`:
  - The unit is active: keep watching. Output offsets are in the row, so each byte still reaches a worker once.
  - The unit is gone and `end` exists: the job ended while the agent was down. Mark it ended and tell the Ticket.
  - The unit is gone and there is no `end`: the box rebooted, or the user manager restarted. Mark it `lost`. A Modal
    job's Apps may still run, so the guard keeps watching them by App id until they stop (section 4.7).
- **Orphans.** `systemctl --user list-units 'solos-job-*' --all` lists every job unit. A unit whose row is ended or
  missing, or whose Ticket ended, is stopped and logged. Unit names come from host ids (`jd_` and a hash), so a worker
  cannot name one. The same sweep runs every 10 minutes.
- **The Ticket ends.** The runtime tells the supervisor when it commits a Ticket's end, whatever the outcome, and the
  supervisor stops the Ticket's jobs and their Modal Apps. The minute's poll is the backstop.
- **A worker ends its Ticket while jobs run.** `worker_report ready` or `dropped` on a Ticket with a running detached
  job is refused with "N jobs still run: job_stop them, or report waiting". This goes beside the `ready` admission
  (`src/agent/broker.rs:2075-2085`). It keeps a worker from killing a two-hour job by mistake, and keeps a harvest from
  reading a tree a job is still writing. Decision 7.
- **The disk floor.** `agent health` pauses new work below `health.min_free_gb` (`src/agent/health.rs:458-480`), which
  does not stop a running data build. With decision 8, it also runs `systemctl --user stop 'solos-job-*'`, from
  outside the agent, so a multi-terabyte build cannot fill the disk while the agent is down.

### 4.4 What the worker sees: tools and grants

A short job works as it does today. Detached jobs extend the same tools, and one tool is new.

| Tool | Change |
|---|---|
| `job_start {code, timeout_s?, detach?, quiet_s?, external?}` | `detach: true` makes a detached job. `timeout_s` may then be up to `executor.detached.max_hours`. `quiet_s` is the job's stall threshold. `external: {meter: "modal", burn_microusd_per_hour}` makes a Modal job and needs `detach`. The reply has the job id, the unit, the deadline, and for a Modal job the hold and the room left. |
| `job_list {}` (new) | The Ticket's detached jobs: id, command's first line, state, age, last output, unread bytes, and for a Modal job its Apps, hold and metered spend. |
| `job_wait {job, wait_s?}` | Also takes a detached job. It reads the output files from the offsets in the row, at most `job_output_bytes` per stream per call, and takes the end once. |
| `job_stop {job}` | Also takes a detached job. It stops the unit and the job's Modal Apps, and returns at once with `stopping` if they have not gone within 10 s. |
| `read` | Reaches `/jobs/<job>/stdout` and `stderr`, read-only, for the whole output. |

- **Grants.** `detach` is refused unless `executor.detached` is configured. `external` is refused unless the Ticket's
  Mandate holds terms for that meter, which it gets at accept only from its repository's `external` list (section 4.8).
  The Router's token is refused, and another Ticket's job id is unknown, as today (`docs/agent-mode/runtime.md:263-264`).
- **Ids are the Ticket's**, not the Assignment's: `key("jd", [ticket, n])`. A later session uses the same id.
- **The prompt.** A relaunched Assignment's first prompt lists the Ticket's live jobs and the ends not yet read, one
  line each, before the Inputs.
- **Claude Code workers** get the same tools through `solos-swarm mcp`, as they get the job tools today
  (`docs/agent-mode/runtime.md:427-430`).

### 4.5 Waking the Ticket

A detached job's end must reach its Ticket even when no worker runs. Today a Note waits for the next Assignment
(`crates/agent-core/src/engine.rs:1685-1688`), and only an allowed person wakes an idle Ticket. So the Engine gets one
command:

- `Engine::job_ended(ticket, job, text, max_wakes)` adds an `InputBody::Job { job, text }`, which is data and grants
  nothing.
- If the Ticket is open, has no live Assignment, waits for no person and is not delivering, and `ticket.job_wakes` is
  below `max_wakes`, it adds one to `job_wakes` and starts the next Assignment, through the same `start` that checks
  approval (`crates/agent-core/src/engine.rs:1710-1732`).
- Past the bound, it sets the Ticket's attention ("job X ended; the jobs have woken this Ticket N times; reply to go
  on") and sends it, so the Watchdog's `waiting` signal applies.
- `job_wakes` returns to 0 when an allowed person's Input arrives (`add_input` with `from_allowed_person`).
- An end still reaches a worker once, as today (`src/agent/jobs.rs:7-12`). In a turn, only the worker's own `job_wait`
  or `job_stop` takes it. Otherwise the supervisor takes it at the turn's end, or as it comes, and sends it as
  `Fact::JobEnded`, which the runtime applies as `job_ended`.

A job end is spend that no person caused. The bound makes a worker that chains short jobs stop after `max_wakes`
relaunches and ask a person. An outsider cannot reset it, because only an allowed person's Input does. Decision 5.

### 4.6 Network: what this design needs from #99

A Modal launch needs egress to Modal and a Modal token. A download needs egress to its data host. This design adds no
network path of its own. It needs these from the egress design:

1. **Egress for a job's sandbox**, not only for `run_code`. It is resolved once at `job_start` from the repository's
   policy and fixed for the job's life. A config change applies to the next job.
2. **A mechanism that does not live in `agent serve`.** A detached job outlives the agent. If #99 picks a broker proxy
   inside `agent serve`, every deploy cuts a job's network for the restart, and a launcher that streams logs from Modal
   sees the stream drop. A per-sandbox network namespace with a kernel allowlist, or a proxy in the job's own unit,
   does not. If #99 must use the agent's proxy, this design accepts the cut, and the job tolerates a reconnect.
3. **Modal's hosts**, pinned by a probe: `api.modal.com:443` for the gRPC client, and whatever hosts the client uploads
   mounts and images to. PR 7 records them by running a real `modal run` under #99's logging.
4. **A sandbox secret**: a file bound read-only into the sandbox, or a variable in its environment, never in its argv
   or in a unit property that `systemctl show` prints. Its exact value is scrubbed from the job's output files before a
   worker reads them, as a node redacts its secrets (`docs/agent-mode/runtime.md:436-439`). This is not the vault: a
   vault key is never seen by a worker (`docs/agent-mode/security.md:53`), and a Modal client must read its token.
5. **A trail entry per connection**, with the job id: host, port and bytes.

The meter does not need #99. It runs on the host's behalf, in a sandbox the host builds with the host's network, as the
dependency install does (`docs/agent-mode/repository-tickets.md:147-151`). It should log its requests in the same trail.

### 4.7 External spend

#### The meter

The meter is an adapter with a fixed command and a parser in Rust, run by the host:

```text
modal billing report --start <hour> --end <hour> --resolution h --json --tag-names solos_job
```

What Modal documents ([CLI](https://modal.com/docs/cli/latest/billing),
[reference](https://modal.com/docs/reference/modal.billing)):

- **Granularity.** One row per object (an App) per interval, with its `cost` as a Decimal, its environment and the tags
  asked for. Intervals are daily or hourly.
- **Only closed intervals.** "Data will be reported for full intervals only". The hour in progress is never in it.
- **Latency.** The docs promise no freshness. So an hour's cost is known at the earliest after the hour ends, and maybe
  later. The design waits `settle_lag` (2 h) after an hour ends before it treats that hour as final.
- **`modal billing summary`** is for whole months only. It is a cross-check of the workspace total, never a delta.

So the meter is the truth for settling and too slow for a tripwire. A GPU wave can spend an hour's burn before the
meter sees a cent of it. The tripwire uses a live estimate instead.

The live estimate comes from two more host commands, run every `poll` (1 minute): `modal app list --json` and
`modal container list --json`. The price of a container comes from `modal billing rates --json`, read once a day. The
docs do not say which fields the container listing has. If it does not name the GPU, every running container counts at
`container_ceiling_microusd_per_hour`. Nor do they say that `modal app stop` takes an App id, though its argument is an
App identifier. PR 6 pins each command's JSON with a recorded fixture, and checks the stop by id on the box.

#### The hold

- The worker declares `burn_microusd_per_hour`, the most its job spends per hour while it runs, at most
  `max_burn_microusd_per_hour`. `timeout_s` is the job's wall cap.
- **Hold = burn × timeout / 3600, rounded up, plus `overhead_microusd`** (image builds, start-up, idle containers).
  Example: $4/h for 2 h is $8.00, plus $0.50 overhead, so the hold is 8,500,000.
- The hold is reserved before `systemd-run`, in one IMMEDIATE transaction that checks three caps (section 4.7,
  Accounting). A refusal says how long the job could run instead: "at $4.00/h, this Ticket has room for 1.1 h".
- The job's deadline is `started + timeout`. systemd kills the launcher there, and the host stops the job's Apps at the
  deadline minus `stop_grace`.

#### The tripwire

Every `poll`, for each live Modal job, the supervisor computes:

- **metered**: the cost of the job's Apps in hours the meter has reported.
- **estimated**: for each minute since the last reported hour, the observed burn: the running containers of the job's
  Apps, each at its price.
- **stopping**: the observed burn times `stop_grace` (5 minutes): what the job spends while it stops.

It trips when **metered + estimated + stopping ≥ hold**, at the deadline, when the observed burn exceeds the declared
burn by more than `shape_tolerance_percent` (10), and when the Ticket ends. A trip:

1. Runs `modal app stop <app> --yes` for each of the job's Apps, and `systemctl --user stop solos-job-<id>`. It stops
   the job, not the worker. A worker that is stopped leaves a detached App running.
2. Checks every poll that the Apps report stopped and no container runs. Past `stop_grace`, it is the
   `external_stuck` alert, and the host keeps stopping.
3. Records the trip in the row, sends a `Job` Input to the Ticket with the reason, and raises `external_tripped`.

The tripwire is a pure function of the row and the observations (`sketch/04_external.rs`), tested on a table.

#### The guard: every App has an owner

The meter's rows are per App. The guard decides whose each App is.

- **A dedicated Modal workspace for agent mode** (decision 2). Every App in it is agent mode's.
- **One holder at a time** (decision 9). The first Modal job of a Ticket makes it the meter's holder. Another
  Ticket's Modal `job_start` is refused until the holder has no live Modal job and none of its Apps runs. Handover is
  immediate, since the meter's rows are per App: the old holder's Apps stopped before the new one's began.
- **Registration.** An App that appears while the holder has live Modal jobs is the holder's. It is the job's whose
  launcher runs, or whose id is in the App's `solos_job` tag. The supervisor sets `SOLOS_JOB` in the job's sandbox so
  the repository's app can tag itself. Tags only split cost between one Ticket's own jobs, so a worker that lies in a
  tag moves money within its own account.
- **Unknown Apps.** A running App with no holder, or one that appears after its holder released, is stopped at once.
  The meter is latched: no new Modal job starts until a person clears it with `agent jobs clear modal`. The Watchdog
  raises `external_unattributed`.
- **When a Modal job ends.** Its launcher may exit while its Apps still run, since `--detach` keeps an App alive
  without its client. So a Modal job ends when its unit has ended and none of its Apps runs a container. Then the host
  stops any of its Apps still deployed, so nothing it made outlives it, and the holder may release.
- **Conservation.** For each reported hour, the workspace total equals the sum of the rows registered to Tickets' jobs
  plus the unattributed rows. Unattributed cost is charged to the account `external:modal`, never to a Ticket, and
  alerted.
- **The outer cap.** The owner sets a workspace budget or spend limit in Modal. "When the spend limit is reached, Modal
  stops workloads that would incur additional out-of-pocket charges" ([budgets](https://modal.com/docs/guide/budgets)).
  It is monthly and coarse, and it depends on no code here.

#### Accounting in the ledger

All amounts are integer micro-USD, as the ledger keeps them. Modal's `cost` is parsed from its decimal text, never
through a float, and rounded up to the next micro-USD.

| Moment | Ledger operation |
|---|---|
| `job_start` of a Modal job | `reserve_external`: one hold row of kind `external`, provider `modal`, request id `job:<id>`, on `ticket:<id>`. One IMMEDIATE transaction checks three caps: the Mandate budget against everything the account committed (model calls and external), the meter's `ticket_cap_microusd` against the account's `external` rows for `modal`, and the meter's `daily_microusd` against all `external` rows for `modal` since the UTC day began. |
| While it runs | Nothing. The hold counts at its full value (`crates/agent-store/src/ledger.rs:147-149`). The meter's rows are kept in `meter_rows` (section 4.9), and the dashboard shows held and metered side by side. |
| A restart | `close_open_holds` closes only rows of kind `model`. An external hold stays open. |
| The job ended and its last hour is final | `settle_external`: the hold closes at the metered cost of its Apps, with no tokens. A cost above the hold is kept as reported, as today, and raises `external_overrun`. |
| A row that arrives after its job settled | `charge` on the same account, once per `meter:<hour>:<app>`, so the ledger stays exact. |
| Unattributed rows | `charge` on `external:modal`, once per `meter:<hour>:<app>`. |
| The meter is broken for good | The hold stays open, counted at its worst case. `agent jobs settle <job> --at-hold` closes it as `unknown`, at its hold. Decision 10. |

What follows for the existing code:

- A model call on a capped Ticket sees an open Modal hold in `committed`. So a Ticket's Mandate budget covers its model
  calls and its Modal spend together, which is what "a $5 budget" means.
- The Watchdog's `spend` alert at 80% of a Ticket's budget counts Modal spend without a change
  (`src/agent/watchdog.rs:244-256`).
- An external overrun latches its meter and does not set the broker's model overrun flag
  (`src/agent/broker.rs:1543-1549`), which would stop every capped account's model calls.

**Acceptance #102 in these terms.** The Ticket's Mandate budget is $5 (a repository `budget_microusd`, or a Router
accept), or `ticket_cap_microusd` is $5. A Modal job whose hold does not fit what is left is refused before anything
launches. A job that fits is stopped by the tripwire when its metered and estimated spend, plus the cost of stopping,
reaches its hold. Its committed spend never passes $5. Its metered spend passes $5 only if Modal bills more than the
host can see, which is an `external_overrun` alert and a latched meter.

#### Trust: who runs what

| Piece | Runs as | Can a worker influence it? |
|---|---|---|
| The meter, the guard's listings, the rates, `modal app stop` | The host, in a host-built sandbox with the host network, an empty environment, `modal` read-only and the **meter token** bound read-only. The command is fixed by `agent.yaml`. | No. No worker sandbox mounts the meter token, its sandbox or its output. Its output is parsed into integers by Rust, and no model reads it. |
| The parser | Rust, in `agent serve` | No. A row it cannot parse fails the reading, and the hold stays at its worst case. |
| What Apps exist and their tags | The worker's code on Modal | Yes. The guard stops Apps it cannot place, and tags only split one Ticket's own spend. |
| The shape: GPUs and containers | The worker's code on Modal | Yes. The live estimate counts what runs, the shape check trips over-declared jobs within a poll, and Modal's workspace budget caps the rest. |
| The **worker token** | In the job's sandbox, through #99 | It is readable. A worker can copy it out through a Modal App's internet. Apps it starts from elsewhere are in the same workspace, so the guard stops them and latches the meter. The owner rotates the token. |
| What the job prints about its own spend | The worker | Ignored. Only the meter settles. |

Two Modal tokens, both from the dedicated workspace: one the workers' jobs get, one only the meter holds. A leaked
worker token can be rotated without blinding the meter.

#### When `budgets:` is absent

Production has no `budgets:`, so a Ticket has no Mandate budget. External spend is still bounded three ways:

1. `external.modal.ticket_cap_microusd`, when the owner sets it, caps each Ticket's Modal spend by itself, apart from
   any model budget. It is recorded in the Mandate at accept, like the forge's terms
   (`docs/agent-mode/repository-tickets.md:21-23`), so a config edit never changes a running Ticket's cap.
2. Without it, every job is still stopped at its own hold: the burn the worker declared times the time it asked for.
   An uncapped Ticket is never refused a hold, and it is never allowed past one.
3. `external.modal.daily_microusd` and Modal's workspace budget bound all Tickets together.

### 4.8 Config shape

```yaml
executor:
  kind: pi
  run_code: {}
  detached:                                 # absent: no detached jobs
    max_running: 6                          # on the box, all Tickets together
    max_per_ticket: 2
    max_hours: 12                           # the longest timeout_s, in hours
    memory_bytes: 34359738368               # 32 GiB per job (MemoryMax)
    cpu_quota_percent: 800                  # 8 cores per job (CPUQuota)
    tasks_max: 512
    output_bytes: 268435456                 # each stream file (prlimit --fsize)
    quiet: 30m                              # the stall threshold of a job that names none
    max_wakes: 8                            # job ends that may relaunch an idle Ticket per person's message
external:                                   # absent: no external spend
  modal:
    kind: modal
    modal: /home/debian/.local/bin/modal    # the CLI the host runs, read-only in its sandbox
    meter_token_file: /run/user/1000/solos-agent/modal-meter.toml
    worker_token_file: /run/user/1000/solos-agent/modal-worker.toml
    ticket_cap_microusd: 5000000            # each Ticket's Modal spend; absent: each job only to its own hold
    daily_microusd: 50000000                # all Tickets' Modal spend per UTC day; absent: none
    max_burn_microusd_per_hour: 40000000    # the most one job may declare
    container_ceiling_microusd_per_hour: 40000000   # a container whose GPU the listing does not name
    overhead_microusd: 500000               # added to every hold
    poll: 1m
    stop_grace: 5m
    settle_lag: 2h
    shape_tolerance_percent: 10
forge:
  repos:
    GuiBibeau/solos-markov-regime-switching-solana-perps:
      about: the regimes research programme (Python, Modal).
      base: main
      external: [modal]                     # its Tickets may spend on these meters
      # egress: ...                         # #99: Modal's hosts and the public data hosts
watchdog:
  thresholds:
    job_quiet: 30m                          # the default when a job names no quiet_s
    meter_stale: 3h                         # no new hour from the meter for this long
```

Both token files are runtime copies, read once at start and deleted, as every token file is
(`docs/agent-mode/configuration.md:11`, `docs/agent-mode/production.md:29-45`), and then held in memory. The meter's
token reaches its sandbox through the environment bwrap inherits, never its argv, as a node's secrets do
(`docs/agent-mode/runtime.md:436-439`). The workers' token reaches a job through #99's sandbox secret (section 4.6).
Load refuses either file under a path a sandbox can read. `external` without `executor.detached` is refused, and so is a repository's
`external` entry that names no configured meter.

### 4.9 Store changes

Four schema bumps, one for each PR that changes the file (PRs 1, 3, 4 and 6 in section 5). Each takes the next free
number when it lands: 10 to 13 from `main` today, unless #99 or #107 takes some first. Each makes an older build refuse
the file instead of dropping what it cannot read (`docs/agent-mode/architecture.md:128-130`).

| PR | Change |
|---|---|
| 1 | `ALTER TABLE spend ADD COLUMN kind TEXT NOT NULL DEFAULT 'model' CHECK (kind IN ('model', 'external'))`. Every existing row is a model call. |
| 3 | The `jobs` table. |
| 4 | No table: `Ticket.job_wakes`, `InputBody::Job`, and the events `JobEnded` and `JobWakeRefused`. An empty migration. |
| 6 | The `meter_rows` and `meter_state` tables, and `Mandate.external` (`ExternalTerms {meter, ticket_cap_microusd}`). |

```sql
CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY NOT NULL,          -- jd_...
    owner TEXT NOT NULL,                   -- ticket:<id>; #107 may add operator
    state TEXT NOT NULL CHECK (state IN ('starting', 'running', 'stopping', 'ended')),
    body TEXT NOT NULL                     -- JSON: spec, unit, apps, offsets, end, hold id, trip
) STRICT;
CREATE INDEX IF NOT EXISTS jobs_live ON jobs (owner) WHERE state != 'ended';

CREATE TABLE IF NOT EXISTS meter_rows (
    meter TEXT NOT NULL, interval_ms INTEGER NOT NULL, object TEXT NOT NULL,
    cost_microusd INTEGER NOT NULL, body TEXT NOT NULL,   -- the row as read, for audit
    owner TEXT, charged TEXT,              -- the job or `external:<meter>`, and the ledger row that took it
    read_ms INTEGER NOT NULL,
    PRIMARY KEY (meter, interval_ms, object)
) STRICT;

CREATE TABLE IF NOT EXISTS meter_state (
    meter TEXT PRIMARY KEY NOT NULL,
    body TEXT NOT NULL                     -- JSON: holder, latch, rates, failures, read until
) STRICT;
```

- **One writer.** The job supervisor writes `jobs`, `meter_rows` and `meter_state`, as the Watchdog alone writes
  `watchdog_alerts` (`docs/agent-mode/architecture.md:65-66`). The ledger still writes `spend`, and the Engine the
  Ticket.
- Rows are JSON with index columns, as the other aggregates are (`docs/agent-mode/architecture.md:120`).

### 4.10 Watchdog

`Snapshot` gains the live and recently ended jobs and each meter's state. `assess` stays pure. New signals:

| Kind | Raised when | Subject (so it is told once) |
|---|---|---|
| `job_died` | A job ended in a way nobody asked: a signal the agent did not send, the OOM killer, `lost`, or a Modal App that stopped without the host stopping it. Raised only while the end is younger than `repeat`, so it is told once. | the job and its end time |
| `job_stalled` | A running job printed nothing for its `quiet_s`, or `job_quiet`. Told again every `repeat` while it lasts. New output ends it, and a later silence is a new alert. | the job and its last output time |
| `external_tripped` | The tripwire stopped a job. | the job and the trip time |
| `external_stuck` | A stop was asked and an App still runs after `stop_grace`. | the job and the stop time |
| `external_unattributed` | An unknown App was seen, or an hour's rows have cost no job owns. | the meter and the hour, or the App |
| `external_overrun` | A job settled above its hold. | the job |
| `meter_failing` | The meter failed 3 readings in a row, or no new hour came for `meter_stale`. | the meter and the first failure |
| `spend` (extended) | A Ticket's Modal spend reached `spend_percent` of its `ticket_cap_microusd`. | as today |

A job that exits with a non-zero code is not an alert: its worker gets the end, as with any command. A job its worker
stopped, or that reached its timeout, is not an alert either (decision 6). So **killing the job raises one alert**, as
#101 asks, and a lasting stall is told every 6 h.

The Watchdog runs inside `agent serve` (`docs/agent-mode/runtime.md:524-525`). A job that dies while the agent is down
is raised at the first tick after the start, since its end time is in the Store.

### 4.11 `agent status`, the publisher and the dashboard

- **`agent status`** gets a `jobs` section and a `meters` section:

  ```text
  jobs: 1 running, 1 ended and not read
    jd_3f0a.. tk_9ab1.. running 1h12m, output 40s ago, `modal run --detach waves/train.py`
      modal: 2 Apps, 8 containers, held $8.50, metered $3.10 to 11:00, estimated $1.40 since
  meters: modal held by tk_9ab1.., last hour 11:00 read at 12:07, no unattributed spend
  ```

  `--json` has the same, with `budget_microusd`-style integers.
- **The publisher** adds `jobs: [JobView]` to each `TicketSnapshot` and `jobs_running` to each `IndexRow`
  (`src/agent/publish/snapshot.rs:70-147`). A `JobView` has the id, the command's first line, state, times, last
  output, end, Apps, hold, metered and estimated spend, and a trip's reason. It never has output text, which can hold
  anything a job printed.
- **The dashboard** shows a Jobs panel on a Ticket's page, and a `/jobs` page with every live job. #107's external jobs
  go on the same page (section 4.12).

### 4.12 How Ticket jobs relate to external jobs (#107)

#107 watches long jobs that are not Tickets: a systemd unit, a tmux session, a pid file or a status command. The
Watchdog checks each every minute, the dashboard lists them, and an alert goes out once per incident. A Ticket's
detached job is the same thing with an owner and a lifecycle. So they should share one shape:

```rust
enum Owner { Operator, Ticket(TicketId) }
enum Probe { Unit(String), Tmux(String), PidFile(PathBuf), Command(Argv), Modal { meter: String, apps: Vec<String> } }
struct Watched { id: String, owner: Owner, probes: Vec<Probe>, quiet_ms: Option<Ms> }
struct Observation { alive: bool, last_activity_ms: Option<Ms>, ended: Option<End> }
```

| | #107's external jobs | Ticket jobs |
|---|---|---|
| Who adds one | the operator (`agent jobs register`, or config) | a worker's `job_start {detach}` |
| Probes | unit, tmux, pid file, command | a unit (`solos-job-<id>`), and Modal Apps |
| Who stops it | the operator, if at all | the worker, the tripwire, the Ticket's end, the operator |
| Its end goes to | the operator, as an alert | the Ticket, as an Input, and an alert only when unasked |
| Spend | at most a floor alert on a meter (#107's "spend floor") | a hold, a tripwire and a settlement |

- **One table, one Watchdog check.** The `jobs` table has an `owner` column for this. If #107 lands first with its
  own registry table, this design adds `owner` and its body fields to that table instead of making a second one. Either
  way, `job_died` and `job_stalled` are one function over `Watched` and `Observation`, written once.
- **The meter is shared.** #107's Modal waves outside agent mode run in the owner's own workspace. The same Modal meter
  adapter, configured watch-only on that workspace, gives #107 its spend floor. Only Ticket jobs take holds.
- **What this design asks of #107:** the `Probe::Unit` probe reads `systemctl --user show` as `agent health` does, and
  the incident subject is the watched id and the incident's start, so a restart tells nothing twice.

### 4.13 Docs changes

- `runtime.md`: "Background jobs" gains "Detached jobs" (the unit, restart, wakes) and a new "External spend" (meter,
  hold, tripwire, guard). "The Watchdog" lists the new signals.
- `configuration.md`: `executor.detached`, `external`, `forge.repos.<repo>.external`, the two thresholds.
- `concepts.md#budgets`: external spend counts on the Ticket's account; the three bounds when `budgets:` is absent.
- `security.md`: a "Detached jobs and external spend" section. Its **Open** items: a worker holds a Modal token; a Modal
  App can reach the internet, so `external: [modal]` is egress for that repository's workers; the guard sees between
  polls only what the listing shows.
- `production.md`: the `solos-jobs.slice` unit and the hardening drop-in, the two token files, the dedicated Modal
  workspace and its budget, `agent jobs` commands, and that a reboot loses detached jobs.
- `how-to/operate.md`: list, stop and settle jobs; clear a latched meter.
- `architecture.md`: the Store's new tables and the job supervisor as their one writer.
- `README.md`: this record in the design records list.

### 4.14 How it is tested

Builds and tests run on the remote box, not on the Mac.

- **Pure tests.**
  - The unit's argv from a spec: a golden test, as the sandbox lines are pinned today
    (`docs/agent-mode/architecture.md:239`). It checks that no worker text is in it, and that `--cpu` is absent.
  - Hold arithmetic, decimal parsing and rounding up: table tests, including Modal's `cost` strings with many digits.
  - The tripwire predicate: a table of metered, estimated, observed burn, declared burn, stop grace and deadline.
  - Attribution: one holder, handover inside one hour, tagged and untagged Apps, an unknown App, conservation.
  - `assess` with jobs: each new signal, its subject, told once, and a stall that ends and comes back.
- **Store tests.** `reserve_external` with each of the three caps, and two at once taking the last of a cap.
  `close_open_holds` leaves external holds open. `settle_external` once. A late row charged once. Migrations run twice
  change nothing. An older build refuses the file.
- **Core tests.** `job_ended` delivers to a running Assignment, wakes an idle Ticket, does not wake one that waits for a
  person, stops at `max_wakes` with attention, and an allowed person's Input resets the count and an outsider's does
  not. The differential walk covers the new Input.
- **End to end on macOS and Linux, with fakes.** A `JobRunner` trait has `Systemd` and `Fake`, which runs a plain
  process and writes the end record. A `ModalCli` trait has a scripted fake: Apps appear, containers run, billing rows
  arrive an hour late. With the scripted harness: a job outlives its Assignment and a later one collects it; the agent
  restarts mid-job; a shape violation trips; an unknown App is stopped and latches the meter; settlement waits for the
  last hour.
- **Linux, on the box.** Real `systemd-run --user` with a `sleep` job: it survives a restart of `agent serve`, its end
  record is written for exit, signal and timeout, `systemctl --user kill` raises one `job_died`, and an orphan unit is
  stopped at start.
- **Live acceptance** with the `verify-agent-mode` skill, on the regimes repository, with `ticket_cap_microusd: 5000000`:
  a job declared at $4/h for 2 h is refused with the room left; one at $1/h for 3 h runs; a second container past the
  declared shape trips within two minutes; the Ticket's account settles about two hours after the end; the dashboard
  shows the job throughout.

### 4.15 Dependencies on work in flight

| Branch | What this design needs from it, or gives it |
|---|---|
| `design/repo-resources` (#99) | Egress for a job's sandbox, independent of `agent serve`, Modal's hosts, a sandbox secret and a trail entry per connection (section 4.6). PRs 1 to 6 do not need it; PR 7 does. |
| `feat/external-jobs-watch` (#107) | The shared `Watched` and `Probe` shape and one Watchdog check (section 4.12). Whichever lands first makes the table, and the other extends it. |
| `ops/harden-units` | The hardening that transient job units must repeat, in the `solos-job-.service.d` drop-in. The dedicated user needs lingering and its own user manager, which `systemd-run --user` uses. |
| `fix/outsider-spend` | A job wake is spend no person caused in the moment. Its rule for who may cause spend must admit the host's wake on a Ticket an allowed person started, and nothing an outsider sends may reset `job_wakes`. |
| `fix/memo-trust` | Job output is outside text. A memo that quotes it must get the trust of tool output, not of a person. |
| `feat/worker-file-tools` | `read` of `/jobs/<job>/stdout` may use its file tools. |
| `feat/cargo-deps` | A detached job gets the Ticket's `Extras`, so it sees installed dependencies as `run_code` does. |

### 4.16 Open risks

- **A worker holds a Modal token**, and a Modal App can reach the internet. Giving a repository `external: [modal]`
  gives its workers a way to send its code and data out, and to spend in the workspace from anywhere until the token
  is rotated. The guard, the latch and Modal's workspace budget bound the spend. Nothing here bounds the data.
  Decisions 2 and 3.
- **Between polls**, a hostile job can start many containers. The guard sees them within `poll`, and Modal's own
  concurrency limits and budget bound the burst.
- **Cost Modal bills but the estimate misses** (storage, network egress, image builds) is covered only by
  `overhead_microusd` until the meter settles. A miss shows as `external_overrun`.
- **A reboot loses local jobs.** Their rows say `lost`, and their Modal Apps stay guarded until they stop.
- **Disk per job is not capped**, only each output file. Decision 8 stops jobs at the disk floor. A quota per Ticket
  directory is a later step.

## 5. PR plan

Each PR leaves `main` shippable. Builds and tests run on the remote box.

| # | PR | What it adds | The check |
|---|---|---|---|
| 1 | The ledger's external holds | `spend.kind`, `reserve_external` with three caps, `settle_external`, `close_open_holds` for `model` only. Schema bump. No caller yet. | Store tests in 4.14. Every existing ledger test passes unedited. |
| 2 | Job units | `JobRunner` with `Systemd` and `Fake`. The unit's argv from a spec, the command file, the end record from `ExecStopPost`, `show` and `stop` parsed. `solos-jobs.slice` and the drop-in in `scripts/systemd/`, and in `agent-deploy`. | Golden argv. On the box: start, end record for exit, signal and timeout, stop, prefix drop-in applied, bwrap under `NoNewPrivileges`. |
| 3 | Detached jobs | The `jobs` table (schema bump) and the supervisor: `job_start {detach}`, `job_list`, `job_wait` and `job_stop` on detached jobs, offsets, recovery and the orphan sweep at start, the Ticket's end stops its jobs, `ready` refused while jobs run, the prompt lists them, `read` of `/jobs`. `executor.detached`. | Fake end to end: a job outlives its Assignment and a later one collects it; a restart mid-job. On the box: the same with real units and a real restart. |
| 4 | Job ends wake the Ticket | `InputBody::Job`, `Ticket.job_wakes`, `Engine::job_ended`, the events. Schema bump. | Core tests in 4.14 and the differential walk. |
| 5 | Jobs on the Watchdog, status and dashboard | `job_died`, `job_stalled`, `job_quiet`. `agent status` jobs. `JobView` in the publisher. The Jobs panel and `/jobs` page. Docs for detached jobs. | `assess` table tests. On the box: kill a job, one alert in Discord; status and dashboard show it. **#101's acceptance.** |
| 6 | The Modal meter, read-only | `ExternalMeter` and the Modal adapter: report, apps, containers, rates, stop. Parsers with recorded fixtures. The host sandbox with network and the meter token. `meter_rows` and `meter_state`. `external` config, `forge.repos.<repo>.external`, `Mandate.external` at accept (schema bump). `agent meter read` prints a reading. | Parser fixtures and rounding tests. On the box: `agent meter read modal` against the dedicated workspace prints the last hours, changes nothing. |
| 7 | Modal jobs | `job_start {external}`: the hold, the holder, registration, the guard, the tripwire, stopping Apps, settlement, late rows, unattributed charges, the latch. The Modal token as #99's sandbox secret, and #99's egress for the job. | Scripted-Modal end to end in 4.14. On the box: the live acceptance. **#102's acceptance.** |
| 8 | External spend on the Watchdog, and the operator's commands | `external_*` and `meter_failing` signals, the extended `spend`. `agent jobs list`, `stop`, `settle --at-hold`, `clear` through `mcp/commands.jsonl`, as `agent stop` goes (`docs/agent-mode/production.md:693-699`). Docs for external spend and security. | `assess` tests. On the box: an unknown App started by hand is stopped, one alert, and `agent jobs clear modal` lifts the latch. |

Order: 1 to 5 deliver #101 and need nothing from the parallel branches. 6 can go in beside them. 7 needs #99. 8 closes
#102. If #107 lands first, PR 3 extends its table and PR 5 reuses its check.

## 6. Decisions for the owner

1. **Where detached jobs run.** Transient systemd user units (A), an agent-owned job host (B), or inside `agent serve`
   (C). **Default: A.**
2. **A dedicated Modal workspace for agent mode**, with a workspace budget or spend limit set in Modal, two tokens (the
   workers', the meter's), and none of your own secrets or volumes in it. Without it, the guard cannot tell your own
   runs from a worker's: it would stop yours, or it would have to let any unknown App run. **Default: yes.**
3. **Who holds the Modal token.** The job's sandbox, so your repository's Modal apps run unchanged (A), or only the
   host, through a runner your Modal code must fit (B's runner). **Default: the job's sandbox**, with the risk in 4.16
   accepted for the regimes repository only.
4. **The per-Ticket Modal cap.** `ticket_cap_microusd` for every Ticket that may use Modal, and a `daily_microusd`.
   Without a cap, each job is still stopped at its own declared hold. **Default: set it to your per-wave cap** ($5 in
   the acceptance test), and leave `daily_microusd` to Modal's workspace budget.
5. **Job ends wake an idle Ticket.** On, bounded by `max_wakes` per person's message, or off, so an end only waits for
   your next message. **Default: on, 8.**
6. **Which ends alert.** A kill the agent did not send, the OOM killer, a lost job, an App stopped outside the host.
   Not a non-zero exit, a timeout or a worker's own stop. **Default: as listed.**
7. **`ready` while jobs run.** Refused, or allowed and the jobs stopped. **Default: refused.**
8. **The disk floor stops detached jobs.** `agent health` stops every `solos-job-*` unit below `health.min_free_gb`.
   **Default: yes.**
9. **One Modal holder at a time.** One Ticket spends on Modal at once, and another waits for it. Several holders need
   a Modal environment per Ticket and attribution by environment, a later step. **Default: one.**
10. **A dead meter.** A hold whose meter never reports stays open at its worst case until you close it with
    `agent jobs settle --at-hold`, or closes by itself at its hold after 7 days. **Default: stays open until you close
    it.**
11. **Approval of external caps.** A `ticket_cap_microusd` above `policy.auto_approve_microusd` waits for an Approve
    click at the Ticket's first Modal job, or the config cap is your approval. **Default: the config cap is your
    approval.**

## Sources

- Modal, `modal billing` CLI: https://modal.com/docs/cli/latest/billing
- Modal, `modal.billing` reference: https://modal.com/docs/reference/modal.billing
- Modal, budgets and spend limits: https://modal.com/docs/guide/budgets
- Modal, `modal app` and `modal container` CLI: https://modal.com/docs/cli/latest/app,
  https://modal.com/docs/cli/latest/container
