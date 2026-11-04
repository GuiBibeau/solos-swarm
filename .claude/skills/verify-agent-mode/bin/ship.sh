#!/usr/bin/env bash
# Usage: ship.sh <checkout> <run>
# Copies the checkout's tracked and untracked-but-not-ignored files to the box at ~/work/verify/<run>/src,
# installs the pinned Pi tree there and builds the release binary into ~/work/verify/<run>/bin.
set -euo pipefail
. "$(dirname "$0")/lib.sh"
src=$1
run=$2
check_run "$run"
rev=$(git -C "$src" rev-parse --short HEAD)$(git -C "$src" diff --quiet HEAD || echo -dirty)
(cd "$src" && git ls-files -co --exclude-standard -z | COPYFILE_DISABLE=1 tar --no-xattrs --null -T - -cf -) |
	box "set -e; d=~/work/verify/$run; rm -rf \$d/src; mkdir -p \$d/src \$d/bin \$d/evidence; chmod 700 \$d; tar -mxf - -C \$d/src"
box "bash -lc $(printf %q "
	set -euo pipefail
	export PATH=\$HOME/.cargo/bin:\$HOME/.local/opt/node-v24.18.0-linux-x64/bin:\$PATH
	export RUSTUP_TOOLCHAIN=1.96.0 CARGO_TARGET_DIR=\$HOME/work/ci/target-verify CARGO_BUILD_JOBS=24
	cd ~/work/verify/$run/src
	npm ci --no-audit --no-fund > /dev/null 2>&1
	test -f node_modules/@earendil-works/pi-coding-agent/dist/cli.js
	nice -n 10 cargo build --locked --release --bin solos-swarm 2>&1 | tail -1
	cp \$CARGO_TARGET_DIR/release/solos-swarm ~/work/verify/$run/bin/solos-swarm.new
	mv -f ~/work/verify/$run/bin/solos-swarm.new ~/work/verify/$run/bin/solos-swarm
	echo $rev > ~/work/verify/$run/bin/REVISION
	echo shipped $rev to ~/work/verify/$run
")"
