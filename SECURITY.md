# Security policy

## Report a vulnerability

Report vulnerabilities through GitHub's private vulnerability reporting. Go to the **Security** tab of this repository and select **Report a vulnerability**. That opens a private advisory with the maintainers.

Do not report a vulnerability in a public issue, pull request or discussion. Do not include secrets in a report. Use placeholders.

A useful report includes:

- the commit on `main` that you tested,
- the mode and command, such as `solos-swarm run` or `solos-swarm agent serve`,
- the steps to reproduce,
- the impact and the conditions it needs,
- logs or traces with keys, tokens and private data removed.

If you cannot use GitHub's form, email the maintainer through their GitHub profile.

## Versions that get fixes

Security fixes land on `main`. Test a report against the latest commit on `main`, and apply the fix there.

## In scope

The agent mode security model is in [docs/agent-mode/security.md](docs/agent-mode/security.md). This policy covers:

- **Sandbox.** The `bwrap` sandbox and the paths it exposes, built in `src/bwrap.rs` and `src/exposure.rs`. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) maps both.
- **Broker and keys.** The model gateway in `src/gateway/`, the swarm broker in `src/broker.rs`, the agent broker in `src/agent/broker.rs`, and the vault in `src/vault.rs` and `src/vault/`.
- **GitHub App.** The App identity, its keys and its tokens in `src/agent/github/`. [docs/agent-mode/how-to/github-app.md](docs/agent-mode/how-to/github-app.md) explains the permissions and the key file.
- **Dashboard.** The read-only swarm dashboard in `scripts/dashboard_server.py` and `dashboard/`, and the agent dashboard in `web/agent-dashboard/`.
- **Memory store.** The SQLite store in `crates/agent-store/` and the memo review path in `src/agent/broker/memory.rs` and `src/agent/broker/review.rs`. [docs/agent-mode/memory/design.md](docs/agent-mode/memory/design.md) describes the design.

Please keep the report private until a fix is on `main` and the advisory is published.
