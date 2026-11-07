# Dashboard and the Mac tunnel

`scripts/dashboard_server.py` serves `dashboard/` and a read-only view of one swarm run's `state.json` and
`config.yaml`. It starts no agents and makes no model calls. It polls every five seconds. `Live` needs both a fresh
state file and an active service. Otherwise the page shows `Snapshot`, `Stale` or `Unavailable`.

```sh
python3 scripts/dashboard_server.py --state RUN/state.json --config RUN/config.yaml     # a retained run
python3 scripts/dashboard_server.py --lima swarm --latest                               # the local Lima VM
```

YAML configs need `scripts/requirements-dashboard.txt` (PyYAML) in the reader's Python environment.

## Why it is never public

The server has no authentication and no token option. It binds `127.0.0.1` only, and its Host-header check admits
only `127.0.0.1:<port>` and `localhost:<port>`. To reach it from your Mac, you go through an SSH tunnel, so only a
person who can SSH to the box can see a run. A public bind would show run telemetry (agent ids, spend, task state)
to anyone. The server also leaves private run fields out of the page, but that is a second defence, not a reason to
expose it.

## Reach a run on the box through the tunnel

Install the template unit on the box once, from the Mac. Never enable it.

```sh
ssh remote-ai 'mkdir -p ~/.config/systemd/user ~/.config/solos-swarm'
scp scripts/systemd/solos-dashboard@.service remote-ai:.config/systemd/user/
ssh remote-ai 'systemctl --user daemon-reload'
```

The box has no source checkout, so the unit runs `dashboard_server.py` from a deployed candidate. Put
`DASHBOARD_CANDIDATE=<candidate id>` in `~/.config/solos-swarm/dashboard.env` on the box. Use any candidate that
holds `scripts/dashboard_server.py`. The unit only reads it. The port is 4173 unless that file also sets
`DASHBOARD_PORT`.

On the Mac:

```sh
scripts/dashboard-tunnel.sh --start RUN_ID      # systemctl --user start solos-dashboard@RUN_ID, then the tunnel, then open the browser
scripts/dashboard-tunnel.sh --dry-run           # print the commands only
scripts/dashboard-tunnel.sh [--host remote-ai] [--port 4173] [--no-open]
```

The tunnel is `ssh -N -L PORT:127.0.0.1:PORT HOST` with `ExitOnForwardFailure=yes`. The local and the remote port
must be equal because of the Host check. The script checks the host, the port (1024 to 65535) and the run id, and
exits when you press Ctrl-C. Without `--start`, the unit must already be running. Afterwards, stop the unit with
`ssh remote-ai systemctl --user stop solos-dashboard@RUN_ID`.
