# Sourced by the other helpers. Every run lives on the box at ~/work/verify/<run>/.
VERIFY_HOST=${VERIFY_HOST:-remote-ai}
VERIFY_LOCAL=${VERIFY_LOCAL:-$HOME/.cache/solos-swarm-verify}
SKILL_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
SSH_OPTS=(-o BatchMode=yes -o ConnectTimeout=20 -o ServerAliveInterval=30)

box() { ssh "${SSH_OPTS[@]}" "$VERIFY_HOST" "$@"; }

check_run() {
	[[ ${1:-} =~ ^[A-Za-z0-9._-]+$ ]] || { echo "bad run id: '${1:-}' (letters, digits, . _ -)" >&2; exit 2; }
}
