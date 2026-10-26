---
name: verify-solos-swarm
description: Build, test, and qualify solos-swarm releases and 10-worker runs.
---

# Verify solos-swarm

Reference docs: `../../docs/ARCHITECTURE.md`, `../../docs/RUNBOOK.md`, `../../docs/CAMPAIGNS.md`, and `../../docs/BENCHMARKS.md`.

```bash
npm ci && npm run build
cargo test --locked
npm test
python3 -m unittest discover -s tests -p 'test_*.py'
./target/release/solos-swarm mvp qa --config /etc/solos-swarm/qa-config.json --profile release-small --output /tmp/qa-report.json
```
