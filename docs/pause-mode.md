# Pause mode and `control.json`

Pause mode lets a run hold its model calls without ending. It exists so a run can span the provider's
peak-price windows, or wait while an operator looks at something, and lose neither work nor deadline.
Source: `effective_control` in `src/broker.rs`, `unpaused_deadline` in `src/main.rs`, `extensions/broker.ts`.

## One answer, four inputs

Before it reserves anything, the broker's `/model` handler asks `effective_control(run_dir, now)`. The first
input that applies wins, and the answer is the number of seconds the caller should wait.

| Input | Set by | Wait it returns |
|---|---|---|
| `<run-dir>/control.json` with `"mode":"pause"` or `"drain"` | `remote.py pause` / `drain`, or the broker itself on a provider outage | 30 s |
| `<run-dir>/PAUSE` file exists | an operator (`touch`), for example `drive.py pause` | 60 s |
| `SWARM_REFUSE_PEAK=1` in the service environment, and now is inside a peak window | `remote.py start --refuse-peak` | seconds to the end of the window, clamped to 1-60 |
| none | | the call goes through |

`control.json` is the source of truth for broker-side and `remote.py` pauses (outage pause, drain, operator
`pause`/`unpause`). The `PAUSE` file and the peak window are inputs to the same function, so there is one pause
mechanism with three ways in.

## Peak windows

With `SWARM_REFUSE_PEAK=1`, model calls are refused Monday to Friday, 01:00-04:00 and 06:00-10:00 UTC (wall clock,
UTC weekday). Weekends have no window. The windows are the provider's third-party-tracked peak pricing, not an
official rate card. A call that passed the gate at 00:59:59 and ends later may still be billed at peak: at most one
call per agent.

## The 503 reply

A refused call gets HTTP 503 with

```json
{"paused": true, "retry_after_s": 60, "reason": "model calls are paused (operator pause or peak window)"}
```

The gate sits before the reservation. A refused call increments no `model_calls` counter, places no hold, writes no
ledger event and sends nothing to the provider. Calls that already passed the gate finish and settle normally.

The Pi extension (`extensions/broker.ts`) treats a `paused` 503 as "wait and ask again": it sleeps `retry_after_s`
and retries, with the SDK timeout raised to 24 hours (the SDK default of 10 minutes would abort a paused call).
Pi sees one slow call, not an error. Tool calls that cost nothing (`run_code`, `fetch_url`) simply stop when the
agent's next model call waits.

## Deadlines count unpaused time

The Goal deadline is a loop in `unpaused_deadline` that ticks every 5 s and only subtracts time while
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

The tournament driver wraps the `PAUSE` file as `drive.py pause` / `drive.py resume`
(see [running-a-tournament.md](running-a-tournament.md)).

## Offline evidence, and what is not tested

Tested offline: the peak window table including weekends and boundaries, the `PAUSE` file, the 503 wire, the
deadline clock stopping and restarting, and the real pinned Pi waiting out two paused replies without spending a
call or erroring. Not tested: a multi-hour pause against the live provider, and the live provider's real peak
behaviour. `solos-swarm smoke` exercises pause, unpause and drain with a scripted provider and no spend.
