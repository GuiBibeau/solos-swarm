# Contributing to solos-swarm

Thanks for your interest. This page covers the build, the tests, the style and the path from an idea to a merged pull request.

## Before you start

- Use Linux. A run needs `bwrap` (bubblewrap). CI pins Rust 1.96.0 and Node 24.18.0.
- Run heavy builds and full test suites on Linux. The CI jobs run on `ubuntu-24.04`.
- Read [README.md](README.md) for the two modes and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the code map.

## Build

```sh
python3 scripts/bootstrap_tokenizers.py --output work/tokenizers
cargo build --locked --release
```

The binary is `target/release/solos-swarm`.

## Test

Run the format check and clippy, which are the lint job in `.github/workflows/ci.yml`:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
```

Run the Rust tests with one thread, as [README.md](README.md) does:

```sh
cargo test --locked -- --test-threads=1
```

CI builds the same tests with `cargo-nextest` and runs them in shards. A pull request that changes only Markdown
documentation, or only `web/agent-dashboard`, skips the Rust jobs, and one that changes no dashboard file skips the web
job (`scripts/ci-changes.sh` decides). To use the nextest path locally:

```sh
cargo nextest run --locked --workspace
```

Run the Python tests, the dashboard tests and the broker tests:

```sh
python3 -m venv work/dashboard-python
work/dashboard-python/bin/pip install -r scripts/requirements-dashboard.txt
work/dashboard-python/bin/python -m unittest discover -s tests -p 'test_*.py'
node --test tests/dashboard-*.test.mjs
npm ci --ignore-scripts --no-audit --no-fund
npm run test:broker
```

The smoke run checks the broker, ledger, pause, drain and resume with no key, network or sandbox:

```sh
./target/release/solos-swarm smoke --dir work/smoke
```

Run the agent dashboard checks from `web/agent-dashboard`:

```sh
cd web/agent-dashboard
npm ci --no-audit --no-fund
npm run lint
npm run typecheck
npm test
npm run build
```

## Code style

Follow [docs/rust-style.md](docs/rust-style.md). Use default `rustfmt` style, keep clippy clean, document every `unsafe` block with a `// SAFETY:` comment, and leave no `dbg!`, `todo!` or `unimplemented!` in a change.

## Propose a change

1. Open an issue with the bug report or feature request form in `.github/ISSUE_TEMPLATE/`. Describe the problem and the result you want.
2. For a large change, wait for a reply before you write code. A small fix can go straight to a pull request.
3. Fork the repository and create a branch from `main`.
4. Make one focused change and add or update tests.
5. Run the checks above.
6. Fill in `.github/pull_request_template.md` and open the pull request against `main`.
7. Keep the pull request open to review. CI must pass before a merge.

Do not report a vulnerability in a public issue. Read [SECURITY.md](SECURITY.md) and use GitHub's private vulnerability reporting.

## Docs

- [README.md](README.md) explains what the two modes do and how to start each one.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) maps the workspace, the ports and the modules.
- [docs/agent-mode/README.md](docs/agent-mode/README.md) is the index for agent mode.
- [docs/rust-style.md](docs/rust-style.md) lists the Rust lints and the gates CI runs.

If a change alters a command, a flag, a file or a config field, update the doc that names it.
