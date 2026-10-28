# Dashboard and the Mac tunnel

`scripts/dashboard_server.py` serves `dashboard/` and a read-only view of one run's `state.json` and `config.yaml`.
It starts no agents and makes no model calls. It polls every five seconds. `Live` needs both a fresh state file and
an active service; otherwise the page shows `Snapshot`, `Stale` or `Unavailable`.

```sh
python3 scripts/dashboard_server.py --state RUN/state.json --config RUN/config.yaml     # a retained run
python3 scripts/dashboard_server.py --lima swarm --latest                               # the local Lima VM
```

YAML configs need `scripts/requirements-dashboard.txt` (PyYAML) in the reader's Python environment.

## Why it is never public

The server has no authentication and no token option. It binds `127.0.0.1` only, and its Host-header check admits
just `127.0.0.1:<port>` and `localhost:<port>`. Reaching it from your Mac therefore goes through an SSH
tunnel, which also means whoever can SSH to the box is the only one who can see a run. A public bind would hand
run telemetry (agent ids, spend, task state) to anyone. The server's allowlist projection keeps private run fields
out of the page, but that is a second line of defence, not a reason to expose it.

## The tunnel script

On the box, once: install the template unit and never enable it.

```sh
mkdir -p ~/.config/systemd/user && cp scripts/systemd/solos-dashboard@.service ~/.config/systemd/user/
systemctl --user daemon-reload
```

The box has no source checkout, so the unit runs `dashboard_server.py` from a deployed candidate. Put
`DASHBOARD_CANDIDATE=<id>` in `~/.config/solos-swarm/dashboard.env`, using a candidate that contains the script and
that no live run depends on being replaced (the unit only reads it). The port defaults to 4173.

On the Mac:

```sh
scripts/dashboard-tunnel.sh --start RUN_ID      # systemctl --user start solos-dashboard@RUN_ID, then the tunnel, then open the browser
scripts/dashboard-tunnel.sh --dry-run           # print the three commands only
scripts/dashboard-tunnel.sh [--host remote-ai] [--port 4173] [--no-open]
```

The tunnel is `ssh -N -L PORT:127.0.0.1:PORT HOST` with `ExitOnForwardFailure=yes`; local and remote port must be
equal because of the Host check. The script validates host, port (1024-65535) and run id, and exits when you press
Ctrl-C. Stop the unit afterwards with `systemctl --user stop solos-dashboard@RUN_ID`. Without `--start`, the unit
must already be running.
