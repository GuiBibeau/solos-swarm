# Ten-agent preview

PASS. Recorded host SSH alias: `remote-ai` (example name for that run). Time: 2026-10-12T20:30:42.025000+07:00.

| Measure | Observed |
|---|---|
| Scope | One Linux host, one synthetic reconciliation Goal |
| Agents | 10 pinned Pi 1.1.0 processes, each with a Finding and live DeepSeek inference |
| Live model calls | 33 |
| Goal runtime | 13.774 seconds |
| All-ten lifetime overlap | 13.746 seconds |
| Conservative ledger spend | $0.018874 |
| Unsettled reservations | 0 |
| Independent checks | 79, all PASS |
| Worker and credential cleanup | Confirmed |
| Host | Debian 13 x86_64, 128 logical CPUs, 1188086728 KiB RAM |
| Provider / model | DeepSeek / deepseek-flash, non-thinking |
| Isolation | bubblewrap user, PID, mount, and network namespaces |

Ledger prices treat input as a cache miss at peak rates. This is not a provider invoice. [DeepSeek pricing](https://api-docs.deepseek.com/quick_start/pricing/) checked 2026-10-12. Both retained live builds used seed `20261012`. Combined conservative spend: $0.037739.

## Candidate

| Field | Value |
|---|---|
| Build revision | `3c80fcf13b3c46854022279d93bb99a761ff31af` |
| Candidate ID | `20cdc2f9ad21707d9e2bd072c1800df6f5b2f08e8787e1242362237715a9b48e` |
| Runtime SHA-256 | `65bed09a39ee39ca446b9b15788e53b14b1d82423c0d94fc8690e884ad4c608a` |
| Run ID | `poc-20261012-02` |

The broker reads and deletes a one-use credential file. Agents receive no provider key. An exact-key scan found no credential in source or collected evidence.

```sh
python3 scripts/remote.py --host remote-ai start --candidate 20cdc2f9ad21707d9e2bd072c1800df6f5b2f08e8787e1242362237715a9b48e --run-id poc-20261012-02 --seed 20261012
python3 scripts/remote.py --host remote-ai status --run-id poc-20261012-02
python3 scripts/remote.py --host remote-ai collect --run-id poc-20261012-02 --output work/evidence/poc-20261012-02
```

## Controls

| Case | Result |
|---|---|
| Lost start acknowledgement | Repeated start returned the existing job; 10 probe workers stopped; 0 paid calls |
| Controller disconnect | 40 seconds across a 30-second host deadline |
| Cancel, then cancel again | Exit 0; shutdown confirmed; 0 paid calls |
| One altered evidence record | Checker exit 1 / FAIL; original artifact unchanged |
| Lifecycle probes | Marked non-live; failed live qualification |
| Denied identity, model, tool, budget, stop | 0 upstream calls, then 1 permitted control call settled once |

Local checks at the time: Cargo tests, Clippy with warnings denied, 29 Python checks, 3 Pi integration checks.

## Outside this report

| Not claimed | Where it is tracked |
|---|---|
| Three-node / 500-agent acceptance | [TRD.md](TRD.md) §10 |
| Raft/QUIC HA, air-gap bootstrap, Firecracker, witness, three-family promotion | [MVP-TRACEABILITY.md](MVP-TRACEABILITY.md) |
| This binary after a rebuild | New candidate, new evidence |

A SIGKILL or host crash before systemd takes the unit can leave `~/.local/state/solos-swarm/credentials/<run>-*`. Confirm the matching service is stopped, then remove that file. The demonstrated runs left no credential handoff file.
