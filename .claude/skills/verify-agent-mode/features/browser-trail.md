# Browse a page with a signed trail

A worker opens a web page in a headless Lightpanda browser on this host and reads it. Every browser step is recorded
on the event chain before it acts and after, signed with this host's trail key, and the page and the exact view the
model got are kept as blobs, so `solos-swarm agent trail` can later prove what the worker saw and did.

## Sub-features

- `browse`: `browser_open` and `browser_read` on a public page return its title, heading and links.
- `trail-signed`: every asked step has a signed answer, and `agent trail` checks them with the public key only.
- `trail-blobs`: every blob a step names exists and re-hashes to its digest.
- `browser-closes`: the Assignment's end closes the browser with a signed `close` step.

## How to get to it (user POV)

- Any request whose work needs a web page, from any chat surface: the Router accepts it and the worker browses.
- `solos-swarm agent trail --state-dir <state> --config <agent.yaml> [--ticket <id>] [--json] [--strict]` to check.

## Driving it with bin/mcp.sh

Preconditions: a run launched with `overlays/browser.yaml` (Lightpanda 1.0.2 at `/home/debian/work/ci/lightpanda/1.0.2/lightpanda`, sha256 `ae7d01d2…37e19e`); `launch.sh` prints the trail key's public half (`ed25519:…`); doctor clean.

- `browse`: `bin/mcp.sh $RUN request "Open https://example.com in the browser and tell me the page's main heading and the text of its link."`, then `replies <thread> result 540`. The result names "Example Domain" and the link "Learn more" (to iana.org); `curl -s https://example.com` shows the same.
- `trail-signed`, `trail-blobs`, `browser-closes`: `bin/evidence.sh $RUN browser-trail`; `trail.txt` ends with `N steps, N signatures ok, M blobs ok, 0 problems` and `exit 0`, and `events.jsonl` has as many `trail_done` as `trail_asked`, the last a `close`. On the box, `ssh remote-ai "cd ~/work/verify/$RUN && bin/solos-swarm agent trail --state-dir state --config agent.yaml --ticket <ticket>"` prints each step.

## Gotchas

- `--block-private-networks` is on by default, so a page on the box's loopback is unreachable from the browser; use a public page, or add `allow_private_networks: true` to the browser overlay for a local page.
- example.com's body text changes over time (in 2026 it shows a multilingual notice); verify the heading and link, not the paragraph.
- A relaunch empties `state/`: run `evidence.sh` (which runs `agent trail`) before relaunching.
