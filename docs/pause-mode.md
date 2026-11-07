# Pause mode and `control.json`

Pause mode lets a swarm run hold its model calls without ending. A run can then span the provider's peak-price
windows, or wait while an operator looks at something, and lose neither work nor deadline. Source:
`effective_control` in `src/gateway/pause.rs` (re-exported by `src/broker.rs`), `unpaused_deadline` in
`src/swarm.rs`, and `extensions/broker.ts`.

## One answer, three inputs

Before it reserves anything, the broker's `/model` handler calls `effective_control(run_dir, now)`. The first
input that applies wins. The answer is the number of seconds the caller waits, or none.

| Input | Set by | Wait it returns |
|---|---|---|
| `<run-dir>/control.json` with `"mode":"pause"` or `"drain"` | `remote.py pause` or `drain`, or the broker itself on a provider outage | 30 s |
| `<run-dir>/PAUSE` file exists | an operator with `touch`, or the tournament's `drive.py pause` | 60 s |
| `SWARM_REFUSE_PEAK=1` in the service environment, and now is inside a peak window | `remote.py start --refuse-peak` | seconds to the end of the window, clamped to 1-60 |
| none of the above | | none: the call goes through |

`control.json` holds every pause that the broker or `remote.py` sets: the outage pause, a drain, and an operator's
`pause` and `unpause`. The `PAUSE` file and the peak window are inputs to the same function, so all three inputs
pause a run the same way.

## Peak windows

With `SWARM_REFUSE_PEAK=1`, model calls are refused Monday to Friday, 01:00-04:00 and 06:00-10:00 UTC (wall clock,
UTC weekday). Weekends have no window. The windows come from third-party trackers of the provider's peak pricing,
not from an official rate card. A call that passed the gate at 00:59:59 and ends later may still be billed at the
peak price. That is at most one call per agent.

## The 503 reply

A refused call gets HTTP 503 with

```json
{"paused": true, "retry_after_s": 60, "reason": "model calls are paused (operator pause or peak window)"}
```

The gate sits before the reservation. A refused call increments no `model_calls` counter, places no hold, writes no
ledger event and sends nothing to the provider. Calls that already passed the gate finish and settle normally.

The Pi extension (`extensions/broker.ts`) treats a `paused` 503 as "wait and ask again". It sleeps `retry_after_s`
and retries. Its SDK timeout is 24 hours, because the SDK default of 10 minutes would abort a paused call. Pi sees
one slow call, not an error. Tool calls that cost nothing (`run_code`, `fetch_url`) stop when the agent's next model
call waits.

## Deadlines count unpaused time

The Goal deadline is a loop in `unpaused_deadline` that ticks every 5 s and subtracts time only while
`effective_control` is `None`. A step that sits through a three-hour peak window loses none of its work time. The
systemd `RuntimeMaxSec` is the wall-clock backstop: `remote.py start --refuse-peak` widens it by 24 hours
(`PEAK_SLACK_SECONDS`), so a stuck `PAUSE` file is bounded by deadline + 24 h and then the normal stop path files
partial findings.

Because the deadline loop reads the same `effective_control` answer, every pause input stops the clock: the peak
window, the `PAUSE` file, and a `control.json` pause or drain (including an outage pause).

## Operator commands

```sh
python3 scripts/remote.py start --refuse-peak --candidate current --run-id R ...   # peak refusal for this run
python3 scripts/remote.py pause   --run-id R --reason "looking at spend"            # control.json mode=pause
python3 scripts/remote.py unpause --run-id R
ssh remote-ai 'touch ~/.local/state/solos-swarm/runs/R/PAUSE'                       # the older switch
ssh remote-ai 'rm -f  ~/.local/state/solos-swarm/runs/R/PAUSE'
```

The tournament driver, `work/tournament-20261028/drive.py`, wraps the `PAUSE` file as `drive.py pause` and
`drive.py resume` (see [running-a-tournament.md](running-a-tournament.md)).

## What is tested

Tested offline: the peak window table including weekends and boundaries, the `PAUSE` file, the 503 wire, the
deadline clock stopping and restarting, and the real pinned Pi waiting out two paused replies without spending a
call or erroring. `solos-swarm smoke` drives pause, unpause and drain with a scripted provider and no spend.

Run live once: every step of the October 2026 tournament started with `--refuse-peak`, and its w1 step sat through
peak and operator pauses of hours (see [running-a-tournament.md](running-a-tournament.md)). Not checked: whether
the provider's real billing follows the peak windows above. Check it against a provider invoice.
