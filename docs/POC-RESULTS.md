# Ten-agent POC result

PASS on `remote-ai`, 2026-10-12T20:30:42.025000+07:00. The requested first passing grade is met. The TRD PR #1 was already merged before implementation.

| Measure | Observed |
|---|---|
| Scope | One Linux host, one synthetic reconciliation Goal |
| Agents | 10 pinned Pi 1.1.0 processes, each with a useful Finding and live DeepSeek inference |
| Live model calls | 33 |
| Goal runtime | 13.774 seconds |
| All-ten lifetime overlap | 13.746 seconds |
| Conservative ledger spend | $0.018874 |
| Unsettled reservations | 0 |
| Independent checks | 79, all PASS |
| Worker and credential cleanup | Confirmed |
| Host | Debian 13 x86_64, 128 logical CPUs, 1188086728 KiB RAM |
| Provider/model | DeepSeek / deepseek-flash, non-thinking |
| Isolation | bubblewrap user/PID/mount/network namespaces, no native Pi tools or ambient resources |

The ledger prices all input as a cache miss at conservative peak rates. This is not a provider invoice. The [DeepSeek pricing page](https://api-docs.deepseek.com/quick_start/pricing/) was checked on 2026-10-12. Both retained live builds passed the same frozen seed `20261012`; combined conservative spend was $0.037739. No failed case was rerolled.

## Exact candidate

- Build revision: `3c80fcf13b3c46854022279d93bb99a761ff31af`.
- Candidate ID: `20cdc2f9ad21707d9e2bd072c1800df6f5b2f08e8787e1242362237715a9b48e`.
- Runtime SHA-256: `65bed09a39ee39ca446b9b15788e53b14b1d82423c0d94fc8690e884ad4c608a`.
- Run ID: `poc-20261012-02`.
- The broker reads and deletes a one-use credential file. The agents receive no provider key. An exact-key scan found no credential in source or collected evidence.

```sh
python3 scripts/remote.py start --candidate 20cdc2f9ad21707d9e2bd072c1800df6f5b2f08e8787e1242362237715a9b48e --run-id poc-20261012-02 --seed 20261012
python3 scripts/remote.py status --run-id poc-20261012-02
python3 scripts/remote.py collect --run-id poc-20261012-02 --output work/evidence/poc-20261012-02
```

## Control and negative demonstrations

- Lost start acknowledgement: killed SSH during a deliberately delayed response; repeated start returned the existing job. The controller stayed disconnected for 40 seconds across the 30-second host deadline. All ten probe workers stopped. Zero paid calls.
- Cancellation and repeated cancellation: both returned exit 0 and confirmed shutdown. All ten probe workers stopped. Zero paid calls.
- Wrong artifact: changed one record only in a copy of passing evidence. The exact checker returned exit 1 / FAIL. Original artifact, protected expected data and original passing report stayed unchanged.
- Evidence survived all cleanup. Lifecycle probes were explicitly non-live and correctly failed live qualification.
- Receiver tests observed zero upstream calls for denied identity/model/tool/budget/stop requests, then exactly one permitted control call that settled once.

Local checks passed: Cargo tests, Clippy with warnings denied, 29 Python checks and three Pi integration checks. There is no claim of a protected release pipeline.

## Deferred coverage and known limit

This result proves the requested first single-host milestone. It does not pass the full TRD three-node/500-agent acceptance. Raft/QUIC, HA, a local provider and air-gap bootstrap, full Tier P/Firecracker, signed audit witnessing, policy actors, Need/history/retraction semantics, module lifecycle, three-family rotation and protected release promotion remain deferred.

A forced SIGKILL or host crash before systemd takes ownership can leave the private credential handoff file. The trusted operator must confirm that the matching service is stopped and remove that run's file. No credential handoff file remained in the demonstrated runs.
