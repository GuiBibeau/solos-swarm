#!/bin/bash
# NOT RUN. Operator step, after explicit approval: stages the tournament dir on the box and enables the two timers.
# Cancel any time with:  ssh remote-ai 'touch ~/research/regimes/tournament-data/CANCELLED; systemctl --user disable --now tournament-gate.timer tournament-build.timer; systemctl --user stop "solos-swarm-tourney-*"'
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
python3 "$HERE/gen.py" w1 >/dev/null
ssh remote-ai "mkdir -p ~/research/regimes/tournament-20261028" && tar -C "$HERE" --exclude evidence --exclude __pycache__ -cf - . | ssh remote-ai "tar -C ~/research/regimes/tournament-20261028 -xf -"
ssh remote-ai 'chmod +x ~/research/regimes/tournament-20261028/*.sh; mkdir -p ~/.config/systemd/user ~/research/regimes/tournament-data
 cp ~/research/regimes/tournament-20261028/systemd/* ~/.config/systemd/user/; rm -f ~/research/regimes/tournament-data/{CANCELLED,GAVE_UP}
 systemctl --user daemon-reload; systemctl --user enable --now tournament-build.timer tournament-gate.timer; systemctl --user list-timers "tournament-*" --no-legend'
